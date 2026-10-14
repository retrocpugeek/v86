//! Decoded-instruction cache.
//!
//! Each RAM page instructions are fetched from gets an array of decoded
//! instructions (Op): a handler specialised for the instruction's form, with
//! the operands already extracted. An Op keeps the instruction word it was
//! decoded from and is only used while the word in RAM still matches, so
//! stores and DMA to code need no invalidation.
//!
//! Only the most common integer forms have handlers of their own; everything
//! else (and every encoding a handler would have to reject) runs through
//! Cpu::exec. The handlers must behave exactly like the paths in exec_*.rs
//! they shortcut.

use crate::bits::*;
use crate::cpu::Cpu;
use crate::exc::Res;
use crate::exec_int::{extend_reg, shift_reg};
use crate::jit::JitPage;

pub type Handler = fn(&mut Cpu, &Op) -> Res<()>;

#[derive(Clone, Copy)]
pub struct Op {
    pub insn: u32,
    /// Usually Rd/Rt, Rn and Rm (Rt2 for pairs).
    pub a: u8,
    pub b: u8,
    pub c: u8,
    /// Small operand: condition, shift type, extend option, bit number, ...
    pub d: u8,
    pub f: Handler,
    pub imm: u64,
}

const PAGE_OPS: usize = 1024;
/// Decoded pages kept before the cache is emptied and refilled.
const MAX_PAGES: usize = 2048;

/// Decoded pages, indexed by RAM page number.
#[derive(Default)]
pub struct DCache {
    pages: Vec<Option<Box<[Op]>>>,
    count: usize,
    /// JIT state of the pages (see jit.rs), allocated for a page when its
    /// first block start is counted.
    jit: Vec<Option<Box<JitPage>>>,
    /// Function table slots of compiled regions that were dropped.
    pub jit_free: Vec<u32>,
    /// Per RAM page: address of its JitPage::state, or 0. Compiled code
    /// looks branch targets up in it; allocated once (jit_init).
    jit_ptrs: Box<[usize]>,
}

impl DCache {
    /// The decoded page for RAM page `n`. The pointer stays valid until the
    /// next call that finds the cache full (all pages are freed then), and
    /// callers only keep it until the next call.
    pub fn page(&mut self, n: usize) -> *mut Op {
        if self.pages.len() <= n {
            self.pages.resize_with(n + 1, || None);
        }
        if self.pages[n].is_none() {
            if self.count == MAX_PAGES {
                self.pages.iter_mut().for_each(|p| *p = None);
                for p in self.jit.iter_mut() {
                    if let Some(jp) = p {
                        jp.drop_region(&mut self.jit_free);
                    }
                    *p = None;
                }
                self.jit_ptrs.fill(0);
                self.count = 0;
            }
            self.pages[n] = Some(vec![decode(0); PAGE_OPS].into_boxed_slice());
            self.count += 1;
        }
        self.pages[n].as_mut().unwrap().as_mut_ptr()
    }

    pub fn pages(&self) -> usize { self.count }

    /// Slot `slot` of RAM page `page`, if that page is decoded.
    pub fn slot(&mut self, page: usize, slot: usize) -> Option<&mut Op> {
        self.pages.get_mut(page)?.as_mut()?.get_mut(slot)
    }

    /// JIT state for the slots of decoded RAM page `page`. Valid as long as
    /// the pointer from page() for it.
    pub fn jit_page(&mut self, page: usize) -> *mut u32 {
        if self.jit.len() <= page {
            self.jit.resize_with(page + 1, || None);
        }
        let ptr = self.jit[page].get_or_insert_with(JitPage::new).state.as_mut_ptr();
        if let Some(p) = self.jit_ptrs.get_mut(page) {
            *p = ptr as usize;
        }
        ptr
    }

    /// Address of the table of JitPage::state addresses, for `ram_pages` RAM pages.
    pub fn jit_ptrs(&mut self, ram_pages: usize) -> usize {
        if self.jit_ptrs.is_empty() {
            self.jit_ptrs = vec![0; ram_pages].into_boxed_slice();
            for (p, jp) in self.jit.iter_mut().enumerate() {
                if let (Some(jp), Some(e)) = (jp, self.jit_ptrs.get_mut(p)) {
                    *e = jp.state.as_mut_ptr() as usize;
                }
            }
        }
        self.jit_ptrs.as_ptr() as usize
    }

    /// JIT state of RAM page `page`, if it has any.
    pub fn jit_meta(&mut self, page: usize) -> Option<&mut JitPage> { self.jit.get_mut(page)?.as_deref_mut() }

    /// Install compiled region `index` for RAM page `page` at block starts `slots`.
    pub fn jit_install(&mut self, page: usize, index: u32, slots: &[usize]) {
        match self.jit.get_mut(page).and_then(|j| j.as_mut()) {
            Some(jp) => jp.install(index, slots, &mut self.jit_free),
            None => self.jit_free.push(index),
        }
    }

    /// Drop the compiled region of RAM page `page`.
    pub fn jit_drop(&mut self, page: usize) {
        if let Some(jp) = self.jit.get_mut(page).and_then(|j| j.as_mut()) {
            jp.drop_region(&mut self.jit_free);
        }
    }

    /// Replace slot `n` of RAM page `page` (`slot`), whose instruction word changed.
    #[cold]
    pub fn redecode(&mut self, page: usize, n: usize, slot: &mut Op, insn: u32) {
        if let Some(jp) = self.jit.get_mut(page).and_then(|p| p.as_mut()) {
            if crate::jit::is_installed(jp.state[n]) {
                jp.drop_region(&mut self.jit_free);
            }
            jp.state[n] = 0;
        }
        *slot = decode(insn);
    }
}

// ---- decoding ----

/// Expands to `f::<..>` with a const parameter for each runtime value, each
/// taken from the listed values: `pick!(f; sf: 0, 1; opc: 0, 1, 2, 3)`.
macro_rules! pick {
    ($f:ident; $v:expr => [$($n:literal),*] $(; $rv:expr => [$($rn:literal),*])*) => {
        pick!(@level $f [] $v => [$($n),*] ($($rv => [$($rn),*]);*))
    };
    (@level $f:ident $done:tt $v:expr => [$($n:literal),*] $rest:tt) => {
        match $v {
            $( $n => pick!(@push $f $done $n $rest), )*
            _ => unreachable!(),
        }
    };
    (@push $f:ident [$($p:literal),*] $n:literal ()) => { $f::<$({ $p },)* { $n }> as Handler };
    (@push $f:ident [$($p:literal),*] $n:literal ($v:expr => [$($m:literal),*] $(; $rv:expr => [$($rn:literal),*])*)) => {
        pick!(@level $f [$($p,)* $n] $v => [$($m),*] ($($rv => [$($rn),*]);*))
    };
}

/// An instruction form with a handler of its own (and code generation in
/// jit.rs). The fields are the handler's const parameters; the operands are
/// in the Op.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    Adr,
    Adrp,
    AddImm { sf: u32, sub: u32, s: u32 },
    LogicImm { sf: u32, opc: u32 },
    MovConst,
    Movk { sf: u32 },
    /// SBFM (opc 0) and UBFM (opc 2).
    Bitfield { sf: u32, opc: u32 },
    Extr { sf: u32 },
    B,
    Bl,
    BCond,
    Cbz { sf: u32, nz: u32 },
    Tbz { nz: u32 },
    /// BR and RET.
    Br,
    Blr,
    Nop,
    LogicReg { sf: u32, opc: u32, n: u32, shift: u32 },
    AddReg { sf: u32, sub: u32, s: u32, shift: u32 },
    AddExt { sf: u32, sub: u32, s: u32 },
    /// op: bit 1 = invert, bit 0 = increment.
    Csel { sf: u32, op: u32 },
    Madd { sf: u32, sub: u32 },
    Shiftv { sf: u32 },
    Ldp { size: u32, signed: u32, mode: u32 },
    Stp { size: u32, mode: u32 },
    /// ext: 0 zero-extend, 1 sign-extend to 64 bits, 2 sign-extend to 32 bits.
    Ld { size: u32, ext: u32, mode: u32 },
    St { size: u32, mode: u32 },
    /// DMB, DSB, ISB (no-ops here, but ISB ends compiled code).
    Barrier { isb: u32 },

    // Forms only the JIT handles (the interpreter runs them through exec)
    /// MRS of a register in a field of Cpu (see jit.rs).
    Mrs { reg: u32 },
    DcZva,
    /// SMADDL, SMSUBL, UMADDL, UMSUBL
    MaddL { unsigned: u32, sub: u32 },
    /// ADC, ADCS, SBC, SBCS
    Adc { sf: u32, sub: u32, s: u32 },
    /// UMULH, SMULH
    Mulh { unsigned: u32 },
    /// UDIV, SDIV
    Div { sf: u32, signed: u32 },
    /// CCMN, CCMP (sub = 1); op.d = condition, op.imm = NZCV.
    Ccmp { sf: u32, sub: u32, imm: u32 },
    /// REV16 (opc 1), REV32 / 32-bit REV (2), REV (3)
    Rev { sf: u32, opc: u32 },
    Clz { sf: u32 },
    /// LDR / STR of a Q register, and LDP / STP (pair = 1).
    LdQ { mode: u32, pair: u32 },
    StQ { mode: u32, pair: u32 },
    /// LDAR, STLR
    LdAcq { size: u32 },
    StRel { size: u32 },
    /// LDXR / LDAXR, STXR / STLXR (op.c = status register)
    LdEx { size: u32 },
    StEx { size: u32 },
}

// Mrs registers
pub const MRS_TPIDR_EL1: u32 = 0;
pub const MRS_TPIDR_EL0: u32 = 1;
pub const MRS_TPIDRRO_EL0: u32 = 2;
pub const MRS_SP_EL0: u32 = 3;
pub const MRS_DAIF: u32 = 4;

pub fn decode(i: u32) -> Op {
    let (form, mut op) = decode_op(i);
    if let Some(form) = form {
        op.f = handler(form);
    }
    op
}

/// The instruction's form, if it has a handler of its own or the JIT compiles
/// it (see Form), and its operands.
pub fn decode_op(i: u32) -> (Option<Form>, Op) {
    let mut op =
        Op { insn: i, a: (i & 31) as u8, b: bits(i, 9, 5) as u8, c: bits(i, 20, 16) as u8, d: 0, f: generic, imm: 0 };
    let form = decode_form(i, &mut op);
    (form, op)
}

fn handler(form: Form) -> Handler {
    use Form::*;
    match form {
        Adr => adr,
        Adrp => adrp,
        AddImm { sf, sub, s } => pick!(add_imm; sf => [0, 1]; sub => [0, 1]; s => [0, 1]),
        LogicImm { sf, opc } => pick!(logic_imm; sf => [0, 1]; opc => [0, 1, 2, 3]),
        MovConst => mov_const,
        Movk { sf } => pick!(movk; sf => [0, 1]),
        Bitfield { sf, opc } => pick!(bitfield; sf => [0, 1]; opc => [0, 2]),
        Extr { sf } => pick!(extr; sf => [0, 1]),
        B => b,
        Bl => bl,
        BCond => b_cond,
        Cbz { sf, nz } => pick!(cbz; sf => [0, 1]; nz => [0, 1]),
        Tbz { nz } => pick!(tbz; nz => [0, 1]),
        Br => br,
        Blr => blr,
        Nop => nop,
        LogicReg { sf, opc, n, shift } => pick!(logic_reg; sf => [0, 1]; opc => [0, 1, 2, 3]; n => [0, 1]; shift => [0, 1]),
        AddReg { sf, sub, s, shift } => pick!(add_reg; sf => [0, 1]; sub => [0, 1]; s => [0, 1]; shift => [0, 1]),
        AddExt { sf, sub, s } => pick!(add_ext; sf => [0, 1]; sub => [0, 1]; s => [0, 1]),
        Csel { sf, op } => pick!(csel; sf => [0, 1]; op => [0, 1, 2, 3]),
        Madd { sf, sub } => pick!(madd; sf => [0, 1]; sub => [0, 1]),
        Shiftv { sf } => pick!(shiftv; sf => [0, 1]),
        Ldp { size, signed, mode } => pick!(ldp; size => [2, 3]; signed => [0, 1]; mode => [0, 1, 2]),
        Stp { size, mode } => pick!(stp; size => [2, 3]; mode => [0, 1, 2]),
        Ld { size, ext, mode } => pick!(ld; size => [0, 1, 2, 3]; ext => [0, 1, 2]; mode => [0, 1, 2, 3]),
        St { size, mode } => pick!(st; size => [0, 1, 2, 3]; mode => [0, 1, 2, 3]),
        Barrier { .. } => nop,
        Adc { .. } | Mulh { .. } | Div { .. } => generic,
        Mrs { .. } | DcZva | MaddL { .. } | Ccmp { .. } | Rev { .. } | Clz { .. } | LdQ { .. } | StQ { .. } => generic,
        LdAcq { .. } | StRel { .. } | LdEx { .. } | StEx { .. } => generic,
    }
}

fn decode_form(i: u32, op: &mut Op) -> Option<Form> {
    use Form::*;
    let sf = bits(i, 31, 31);
    let w = if sf != 0 { 64 } else { 32 };
    // ---- data processing (immediate) ----
    if i & 0x1f00_0000 == 0x1000_0000 {
        // ADR / ADRP
        let imm = sext(((bits(i, 23, 5) << 2) | bits(i, 30, 29)) as u64, 21) as u64;
        if sf != 0 {
            op.imm = imm << 12;
            return Some(Adrp);
        }
        op.imm = imm;
        return Some(Adr);
    }
    if i & 0x1f80_0000 == 0x1100_0000 {
        // ADD/ADDS/SUB/SUBS (immediate)
        op.imm = (bits(i, 21, 10) as u64) << if bit(i, 22) { 12 } else { 0 };
        return Some(AddImm { sf, sub: bits(i, 30, 30), s: bits(i, 29, 29) });
    }
    if i & 0x1f80_0000 == 0x1200_0000 {
        // Logical (immediate)
        let n = bits(i, 22, 22);
        if sf == 0 && n != 0 {
            return None;
        }
        let (imm, _) = decode_bit_masks(n, bits(i, 15, 10), bits(i, 21, 16), true, w)?;
        op.imm = imm;
        return Some(LogicImm { sf, opc: bits(i, 30, 29) });
    }
    if i & 0x1f80_0000 == 0x1280_0000 {
        // MOVN / MOVZ / MOVK
        let opc = bits(i, 30, 29);
        let hw = bits(i, 22, 21);
        if opc == 1 || (sf == 0 && hw > 1) {
            return None;
        }
        let pos = hw * 16;
        let imm = (bits(i, 20, 5) as u64) << pos;
        if opc == 3 {
            op.imm = imm;
            op.d = pos as u8;
            return Some(Movk { sf });
        }
        let v = if opc == 0 { !imm } else { imm };
        op.imm = if sf != 0 { v } else { v as u32 as u64 };
        return Some(MovConst);
    }
    if i & 0x1f80_0000 == 0x1300_0000 {
        // SBFM / UBFM (BFM is left to exec)
        let opc = bits(i, 30, 29);
        let n = bits(i, 22, 22);
        let immr = bits(i, 21, 16);
        let imms = bits(i, 15, 10);
        if opc == 1 || opc == 3 || n != sf || (sf == 0 && (immr | imms) & 0x20 != 0) {
            return None;
        }
        let (wmask, tmask) = decode_bit_masks(n, imms, immr, false, w)?;
        op.imm = wmask & tmask;
        op.c = imms as u8;
        op.d = immr as u8;
        return Some(Bitfield { sf, opc });
    }
    if i & 0x1f80_0000 == 0x1380_0000 {
        // EXTR
        let imms = bits(i, 15, 10);
        if bits(i, 30, 29) != 0 || bit(i, 21) || bits(i, 22, 22) != sf || (sf == 0 && imms >= 32) {
            return None;
        }
        op.d = imms as u8;
        return Some(Extr { sf });
    }
    // ---- branches ----
    if i & 0x7c00_0000 == 0x1400_0000 {
        // B / BL
        op.imm = (sext(bits(i, 25, 0) as u64, 26) << 2) as u64;
        return Some(if sf != 0 { Bl } else { B });
    }
    if i & 0xff00_0010 == 0x5400_0000 {
        // B.cond
        op.imm = (sext(bits(i, 23, 5) as u64, 19) << 2) as u64;
        op.d = (i & 0xf) as u8;
        return Some(BCond);
    }
    if i & 0x7e00_0000 == 0x3400_0000 {
        // CBZ / CBNZ
        op.imm = (sext(bits(i, 23, 5) as u64, 19) << 2) as u64;
        return Some(Cbz { sf, nz: bits(i, 24, 24) });
    }
    if i & 0x7e00_0000 == 0x3600_0000 {
        // TBZ / TBNZ
        op.imm = (sext(bits(i, 18, 5) as u64, 14) << 2) as u64;
        op.d = ((sf << 5) | bits(i, 23, 19)) as u8;
        return Some(Tbz { nz: bits(i, 24, 24) });
    }
    match i & 0xffff_fc1f {
        0xd61f_0000 | 0xd65f_0000 => return Some(Br), // BR, RET
        0xd63f_0000 => return Some(Blr),
        _ => {}
    }
    if i & 0xffff_f01f == 0xd503_201f && !matches!(bits(i, 11, 5), 2 | 3) {
        // Hints other than WFE/WFI: NOP, YIELD, SEV, PAuth/BTI, ...
        return Some(Nop);
    }
    if i & 0xffff_f01f == 0xd503_301f && bits(i, 7, 5) != 2 {
        // DSB, DMB, ISB (CLREX is op2 = 2)
        return Some(Barrier { isb: (bits(i, 7, 5) == 6) as u32 });
    }
    if i & 0xffff_ffe0 == 0xd50b_7420 {
        return Some(DcZva);
    }
    if i & 0xfff0_0000 == 0xd530_0000 {
        // MRS
        let reg = match (i >> 5) & 0x7fff {
            0x4684 => MRS_TPIDR_EL1, // 3, 0, 13, 0, 4
            0x5e82 => MRS_TPIDR_EL0, // 3, 3, 13, 0, 2
            0x5e83 => MRS_TPIDRRO_EL0,
            0x4208 => MRS_SP_EL0, // 3, 0, 4, 1, 0
            0x5a11 => MRS_DAIF,   // 3, 3, 4, 2, 1
            _ => return None,
        };
        return Some(Mrs { reg });
    }
    if i & 0xffc0_0000 == 0xf980_0000 || i & 0xffe0_0c00 == 0xf880_0000 || i & 0xff00_0000 == 0xd800_0000 {
        // PRFM (immediate, literal), PRFUM
        return Some(Nop);
    }
    // ---- data processing (register) ----
    if i & 0x1f00_0000 == 0x0a00_0000 {
        // Logical (shifted register)
        let amount = bits(i, 15, 10);
        if sf == 0 && amount >= 32 {
            return None;
        }
        op.d = bits(i, 23, 22) as u8;
        op.imm = amount as u64;
        return Some(LogicReg { sf, opc: bits(i, 30, 29), n: bits(i, 21, 21), shift: (amount != 0) as u32 });
    }
    if i & 0x1f20_0000 == 0x0b00_0000 {
        // ADD/SUB (shifted register)
        let ty = bits(i, 23, 22);
        let amount = bits(i, 15, 10);
        if ty == 3 || (sf == 0 && amount >= 32) {
            return None;
        }
        op.d = ty as u8;
        op.imm = amount as u64;
        return Some(AddReg { sf, sub: bits(i, 30, 30), s: bits(i, 29, 29), shift: (amount != 0) as u32 });
    }
    if i & 0x1f20_0000 == 0x0b20_0000 {
        // ADD/SUB (extended register)
        let shift = bits(i, 12, 10);
        if bits(i, 23, 22) != 0 || shift > 4 {
            return None;
        }
        op.d = bits(i, 15, 13) as u8;
        op.imm = shift as u64;
        return Some(AddExt { sf, sub: bits(i, 30, 30), s: bits(i, 29, 29) });
    }
    if i & 0x3fe0_0800 == 0x1a80_0000 {
        // CSEL / CSINC / CSINV / CSNEG
        op.d = bits(i, 15, 12) as u8;
        return Some(Csel { sf, op: (bits(i, 30, 30) << 1) | bits(i, 10, 10) });
    }
    if i & 0x7fe0_0000 == 0x1b00_0000 {
        // MADD / MSUB
        op.d = bits(i, 14, 10) as u8;
        return Some(Madd { sf, sub: bits(i, 15, 15) });
    }
    if i & 0x7fe0_f000 == 0x1ac0_2000 {
        // LSLV / LSRV / ASRV / RORV
        op.d = bits(i, 11, 10) as u8;
        return Some(Shiftv { sf });
    }
    if i & 0x1fe0_fc00 == 0x1a00_0000 {
        // ADC / ADCS / SBC / SBCS
        return Some(Adc { sf, sub: bits(i, 30, 30), s: bits(i, 29, 29) });
    }
    if i & 0xff60_8000 == 0x9b40_0000 {
        // SMULH / UMULH
        return Some(Mulh { unsigned: bits(i, 23, 23) });
    }
    if i & 0x7fe0_f800 == 0x1ac0_0800 {
        // UDIV / SDIV
        return Some(Div { sf, signed: bits(i, 10, 10) });
    }
    if i & 0xff60_0000 == 0x9b20_0000 {
        // SMADDL / SMSUBL / UMADDL / UMSUBL
        op.d = bits(i, 14, 10) as u8;
        return Some(MaddL { unsigned: bits(i, 23, 23), sub: bits(i, 15, 15) });
    }
    if i & 0x3fe0_0410 == 0x3a40_0000 {
        // CCMN / CCMP
        op.d = bits(i, 15, 12) as u8;
        op.imm = (i & 0xf) as u64;
        return Some(Ccmp { sf, sub: bits(i, 30, 30), imm: bits(i, 11, 11) });
    }
    if i & 0x7fff_f000 == 0x5ac0_0000 {
        let opc = bits(i, 11, 10);
        if opc == 0 || (opc == 3 && sf == 0) {
            return None; // RBIT
        }
        return Some(Rev { sf, opc });
    }
    if i & 0x7fff_fc00 == 0x5ac0_1000 {
        return Some(Clz { sf });
    }
    // ---- Q register loads and stores ----
    if i & 0xff80_0000 == 0x3d80_0000 {
        // LDR / STR (unsigned offset)
        op.imm = (bits(i, 21, 10) as u64) << 4;
        return Some(if bit(i, 22) { LdQ { mode: OFF, pair: 0 } } else { StQ { mode: OFF, pair: 0 } });
    }
    if i & 0xffa0_0000 == 0x3c80_0000 && bits(i, 11, 10) != 2 {
        // LDUR / STUR, pre- and post-indexed
        op.imm = sext(bits(i, 20, 12) as u64, 9) as u64;
        let mode = [OFF, POST, OFF, PRE][bits(i, 11, 10) as usize];
        return Some(if bit(i, 22) { LdQ { mode, pair: 0 } } else { StQ { mode, pair: 0 } });
    }
    if i & 0xfe00_0000 == 0xac00_0000 {
        // LDP / STP / LDNP / STNP
        op.c = bits(i, 14, 10) as u8;
        op.imm = (sext(bits(i, 21, 15) as u64, 7) << 4) as u64;
        let mode = match bits(i, 24, 23) {
            0 | 2 => OFF,
            1 => POST,
            _ => PRE,
        };
        return Some(if bit(i, 22) { LdQ { mode, pair: 1 } } else { StQ { mode, pair: 1 } });
    }
    // ---- load-acquire, store-release, exclusives ----
    if i & 0x3f20_8000 == 0x0800_8000 || i & 0x3fa0_0000 == 0x0800_0000 {
        let size = bits(i, 31, 30);
        return Some(match (bit(i, 23), bit(i, 22)) {
            (true, true) => LdAcq { size },
            (true, false) => StRel { size },
            (false, true) => LdEx { size },
            (false, false) => StEx { size },
        });
    }
    // ---- loads and stores (general-purpose registers) ----
    if i & 0x3e00_0000 == 0x2800_0000 {
        // LDP / STP / LDPSW
        let load = bits(i, 22, 22);
        let (size, signed) = match (bits(i, 31, 30), load) {
            (0, _) => (2, 0),
            (1, 1) => (2, 1),
            (2, _) => (3, 0),
            _ => return None,
        };
        op.c = bits(i, 14, 10) as u8;
        op.imm = (sext(bits(i, 21, 15) as u64, 7) << size) as u64;
        let mode = match bits(i, 24, 23) {
            0 | 2 => OFF,
            1 => POST,
            _ => PRE,
        };
        return Some(if load != 0 { Ldp { size, signed, mode } } else { Stp { size, mode } });
    }
    if i & 0x3e00_0000 != 0x3800_0000 {
        return None;
    }
    // Single register: bits 29:27 = 111, V = 0, bit 25 = 0
    let size = bits(i, 31, 30);
    let mode = if bit(i, 24) {
        op.imm = (bits(i, 21, 10) as u64) << size;
        OFF
    } else if !bit(i, 21) {
        op.imm = sext(bits(i, 20, 12) as u64, 9) as u64;
        match bits(i, 11, 10) {
            0 => OFF,
            1 => POST,
            3 => PRE,
            _ => return None, // LDTR / STTR
        }
    } else if bits(i, 11, 10) == 2 && bit(i, 14) {
        // Register offset (option bit 1 set)
        op.d = bits(i, 15, 13) as u8;
        op.imm = if bit(i, 12) { size as u64 } else { 0 };
        REG
    } else {
        return None;
    };
    let ext = match (bits(i, 23, 22), size) {
        (0, _) => return Some(St { size, mode }),
        (1, _) => 0,
        (2, 3) => return None, // PRFM
        (2, _) => 1,
        (_, 0 | 1) => 2,
        _ => return None,
    };
    Some(Ld { size, ext, mode })
}

// ---- handlers ----

#[inline(always)]
fn width(sf: u32) -> u32 { if sf != 0 { 64 } else { 32 } }

#[inline(always)]
fn trunc(v: u64, sf: u32) -> u64 { if sf != 0 { v } else { v as u32 as u64 } }

pub(crate) fn generic(c: &mut Cpu, op: &Op) -> Res<()> { c.exec(op.insn) }

fn nop(_: &mut Cpu, _: &Op) -> Res<()> { Ok(()) }

fn adr(c: &mut Cpu, op: &Op) -> Res<()> {
    c.set_xr(op.a as u32, c.insn_pc.wrapping_add(op.imm));
    Ok(())
}

fn adrp(c: &mut Cpu, op: &Op) -> Res<()> {
    c.set_xr(op.a as u32, (c.insn_pc & !0xfff).wrapping_add(op.imm));
    Ok(())
}

fn add_imm<const SF: u32, const SUB: u32, const S: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let a = c.xs(op.b as u32);
    if S != 0 {
        let (r, nzcv) = if SUB != 0 { add_with_carry(a, !op.imm, true, SF != 0) } else { add_with_carry(a, op.imm, false, SF != 0) };
        c.nzcv = nzcv;
        c.set_xr(op.a as u32, r & ones(width(SF)));
    } else {
        let r = if SUB != 0 { a.wrapping_sub(op.imm) } else { a.wrapping_add(op.imm) };
        c.set_xs(op.a as u32, trunc(r, SF));
    }
    Ok(())
}

fn logic_imm<const SF: u32, const OPC: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let a = c.xr(op.b as u32);
    let r = match OPC {
        0 | 3 => a & op.imm,
        1 => a | op.imm,
        _ => a ^ op.imm,
    } & ones(width(SF));
    if OPC == 3 {
        c.nzcv = nz_flags(r, SF != 0);
        c.set_xr(op.a as u32, r);
    } else {
        c.set_xs(op.a as u32, r);
    }
    Ok(())
}

fn mov_const(c: &mut Cpu, op: &Op) -> Res<()> {
    c.set_xr(op.a as u32, op.imm);
    Ok(())
}

fn movk<const SF: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let r = (c.xr(op.a as u32) & !(0xffffu64 << op.d)) | op.imm;
    c.set_xr(op.a as u32, trunc(r, SF));
    Ok(())
}

/// SBFM (OPC 0) and UBFM (OPC 2); imm = wmask & tmask, c = imms, d = immr.
fn bitfield<const SF: u32, const OPC: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let w = width(SF);
    let src = c.xr(op.b as u32) & ones(w);
    let mut r = ror(src, op.d as u32, w) & op.imm;
    if OPC == 0 && src >> op.c & 1 != 0 {
        let tmask = ones(((op.c as u32).wrapping_sub(op.d as u32) & (w - 1)) + 1);
        r |= ones(w) & !tmask;
    }
    c.set_xr(op.a as u32, trunc(r, SF));
    Ok(())
}

fn extr<const SF: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let w = width(SF);
    let lo = c.xr(op.c as u32) & ones(w);
    let hi = c.xr(op.b as u32) & ones(w);
    let s = op.d as u32;
    let r = if s == 0 { lo } else { (lo >> s) | (hi << (w - s)) };
    c.set_xr(op.a as u32, r & ones(w));
    Ok(())
}

fn b(c: &mut Cpu, op: &Op) -> Res<()> {
    c.pc = c.insn_pc.wrapping_add(op.imm);
    Ok(())
}

fn bl(c: &mut Cpu, op: &Op) -> Res<()> {
    c.x[30] = c.insn_pc.wrapping_add(4);
    c.pc = c.insn_pc.wrapping_add(op.imm);
    Ok(())
}

fn b_cond(c: &mut Cpu, op: &Op) -> Res<()> {
    if cond_holds(op.d as u32, c.nzcv) {
        c.pc = c.insn_pc.wrapping_add(op.imm);
    }
    Ok(())
}

fn cbz<const SF: u32, const NZ: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    if (trunc(c.xr(op.a as u32), SF) == 0) != (NZ != 0) {
        c.pc = c.insn_pc.wrapping_add(op.imm);
    }
    Ok(())
}

fn tbz<const NZ: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    if (c.xr(op.a as u32) >> op.d & 1 != 0) == (NZ != 0) {
        c.pc = c.insn_pc.wrapping_add(op.imm);
    }
    Ok(())
}

fn br(c: &mut Cpu, op: &Op) -> Res<()> {
    c.pc = c.xr(op.b as u32);
    Ok(())
}

fn blr(c: &mut Cpu, op: &Op) -> Res<()> {
    let t = c.xr(op.b as u32);
    c.x[30] = c.insn_pc.wrapping_add(4);
    c.pc = t;
    Ok(())
}

/// d = shift type, imm = amount (SHIFT is 0 when it's 0).
fn logic_reg<const SF: u32, const OPC: u32, const N: u32, const SHIFT: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let w = width(SF);
    let m = c.xr(op.c as u32);
    let mut b = if SHIFT != 0 { shift_reg(m, op.d as u32, op.imm as u32, SF != 0) } else { m & ones(w) };
    if N != 0 {
        b = !b & ones(w);
    }
    let a = c.xr(op.b as u32);
    let r = match OPC {
        0 | 3 => a & b,
        1 => a | b,
        _ => a ^ b,
    } & ones(w);
    if OPC == 3 {
        c.nzcv = nz_flags(r, SF != 0);
    }
    c.set_xr(op.a as u32, r);
    Ok(())
}

fn add_reg<const SF: u32, const SUB: u32, const S: u32, const SHIFT: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let m = c.xr(op.c as u32);
    let b = if SHIFT != 0 { shift_reg(m, op.d as u32, op.imm as u32, SF != 0) } else { m & ones(width(SF)) };
    let a = c.xr(op.b as u32);
    if S != 0 {
        let (r, nzcv) = if SUB != 0 { add_with_carry(a, !b, true, SF != 0) } else { add_with_carry(a, b, false, SF != 0) };
        c.nzcv = nzcv;
        c.set_xr(op.a as u32, r & ones(width(SF)));
    } else {
        let r = if SUB != 0 { a.wrapping_sub(b) } else { a.wrapping_add(b) };
        c.set_xr(op.a as u32, trunc(r, SF));
    }
    Ok(())
}

/// d = extend option, imm = shift.
fn add_ext<const SF: u32, const SUB: u32, const S: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let b = extend_reg(c.xr(op.c as u32), op.d as u32, op.imm as u32, SF != 0);
    let a = c.xs(op.b as u32);
    let (r, nzcv) = if SUB != 0 { add_with_carry(a, !b, true, SF != 0) } else { add_with_carry(a, b, false, SF != 0) };
    let r = r & ones(width(SF));
    if S != 0 {
        c.nzcv = nzcv;
        c.set_xr(op.a as u32, r);
    } else {
        c.set_xs(op.a as u32, r);
    }
    Ok(())
}

/// OP: bit 1 = op (invert), bit 0 = o2 (increment); d = condition.
fn csel<const SF: u32, const OP: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let r = if cond_holds(op.d as u32, c.nzcv) {
        c.xr(op.b as u32)
    } else {
        let b = c.xr(op.c as u32);
        match OP {
            0 => b,
            1 => b.wrapping_add(1),
            2 => !b,
            _ => b.wrapping_neg(),
        }
    };
    c.set_xr(op.a as u32, trunc(r, SF));
    Ok(())
}

/// d = Ra.
fn madd<const SF: u32, const SUB: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let p = c.xr(op.b as u32).wrapping_mul(c.xr(op.c as u32));
    let a = c.xr(op.d as u32);
    let r = if SUB != 0 { a.wrapping_sub(p) } else { a.wrapping_add(p) };
    c.set_xr(op.a as u32, trunc(r, SF));
    Ok(())
}

/// d = shift type.
fn shiftv<const SF: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let a = c.xr(op.b as u32);
    let n = (c.xr(op.c as u32) % width(SF) as u64) as u32;
    c.set_xr(op.a as u32, trunc(shift_reg(a, op.d as u32, n, SF != 0), SF));
    Ok(())
}

// Addressing modes for loads and stores
pub const OFF: u32 = 0;
pub const POST: u32 = 1;
pub const PRE: u32 = 2;
/// Register offset: Rm (c) extended by option (d) and shifted by imm.
pub const REG: u32 = 3;

/// Access address and base register writeback value.
#[inline(always)]
fn address<const MODE: u32>(c: &Cpu, op: &Op) -> (u64, u64) {
    let base = c.xs(op.b as u32);
    match MODE {
        OFF => (base.wrapping_add(op.imm), 0),
        POST => (base, base.wrapping_add(op.imm)),
        PRE => {
            let va = base.wrapping_add(op.imm);
            (va, va)
        }
        _ => (base.wrapping_add(extend_reg(c.xr(op.c as u32), op.d as u32, op.imm as u32, true)), 0),
    }
}

#[inline(always)]
fn writeback<const MODE: u32>(c: &mut Cpu, op: &Op, wb: u64) {
    if MODE == POST || MODE == PRE {
        c.set_xs(op.b as u32, wb);
    }
}

/// EXT: 0 zero-extend, 1 sign-extend to 64 bits, 2 sign-extend to 32 bits.
fn ld<const SIZE: u32, const EXT: u32, const MODE: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let (va, wb) = address::<MODE>(c, op);
    let v = c.read(va, 1 << SIZE, false)?;
    writeback::<MODE>(c, op, wb);
    let v = match EXT {
        0 => v,
        1 => sext(v, 8 << SIZE) as u64,
        _ => sext(v, 8 << SIZE) as u32 as u64,
    };
    c.set_xr(op.a as u32, v);
    Ok(())
}

fn st<const SIZE: u32, const MODE: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let (va, wb) = address::<MODE>(c, op);
    let v = c.xr(op.a as u32);
    c.write(va, 1 << SIZE, v, false)?;
    writeback::<MODE>(c, op, wb);
    Ok(())
}

/// c = Rt2.
fn ldp<const SIZE: u32, const SIGNED: u32, const MODE: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let (va, wb) = address::<MODE>(c, op);
    let a1 = c.read(va, 1 << SIZE, false)?;
    let a2 = c.read(va.wrapping_add(1 << SIZE), 1 << SIZE, false)?;
    let (a1, a2) = if SIGNED != 0 { (sext(a1, 32) as u64, sext(a2, 32) as u64) } else { (a1, a2) };
    writeback::<MODE>(c, op, wb);
    c.set_xr(op.a as u32, a1);
    c.set_xr(op.c as u32, a2);
    Ok(())
}

fn stp<const SIZE: u32, const MODE: u32>(c: &mut Cpu, op: &Op) -> Res<()> {
    let (va, wb) = address::<MODE>(c, op);
    let (a1, a2) = (c.xr(op.a as u32), c.xr(op.c as u32));
    c.write(va, 1 << SIZE, a1, false)?;
    c.write(va.wrapping_add(1 << SIZE), 1 << SIZE, a2, false)?;
    writeback::<MODE>(c, op, wb);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;

    const RAM_BASE: u64 = 0x4000_0000;
    const RAM_SIZE: usize = 0x2000;

    /// xorshift64*
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
    }

    fn cpu(seed: u64) -> Cpu {
        let mut r = Rng(seed);
        let mut c = Cpu::new(Bus::new(RAM_BASE, RAM_SIZE));
        for i in 0..31 {
            // Mostly addresses in RAM (so loads and stores work), some odd values
            c.x[i] = match r.next() % 4 {
                0 => r.next(),
                1 => r.next() % 64,
                _ => RAM_BASE + 0x800 + r.next() % 0x800,
            };
        }
        c.sp_el1 = RAM_BASE + 0x1000 + (r.next() % 0x40) * 8;
        c.nzcv = (r.next() as u32) & 0xf000_0000;
        for b in c.bus.ram.iter_mut() {
            *b = r.next() as u8;
        }
        c.insn_pc = RAM_BASE + (r.next() % 0x400) * 4;
        c.pc = c.insn_pc + 4;
        c
    }

    fn state(c: &Cpu) -> (Vec<u64>, u64, u64, u32, Vec<u8>) {
        (c.x.to_vec(), c.sp_el1, c.pc, c.nzcv, c.bus.ram.clone())
    }

    /// Every specialised handler must do exactly what Cpu::exec does.
    #[test]
    fn handlers_match_exec() {
        let mut r = Rng(0x1234_5678_9abc_def1);
        let mut tested = 0;
        let mut forms = std::collections::HashSet::new();
        while tested < 300_000 {
            let insn = r.next() as u32;
            let op = decode(insn);
            if op.f as usize == generic as usize {
                continue;
            }
            forms.insert(op.f as usize);
            tested += 1;
            let seed = r.next();
            let mut a = cpu(seed);
            let mut b = cpu(seed);
            let ea = a.exec(insn).map_err(|e| (e.ec, e.iss, e.far));
            let eb = (op.f)(&mut b, &op).map_err(|e| (e.ec, e.iss, e.far));
            assert_eq!(ea, eb, "insn {insn:08x}: result");
            assert!(state(&a) == state(&b), "insn {insn:08x}: state differs");
        }
        assert!(forms.len() > 150, "only {} handlers reached", forms.len());
    }
}
