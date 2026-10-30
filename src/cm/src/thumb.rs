//! Thumb-2 instructions of ARMv7E-M (the Cortex-M4's: ARMv7-M plus the DSP
//! extension), decoded as in the ARMv7-M ARM (DDI 0403E), chapter A5, and
//! executed on the Cpu. No floating point (FPv4 instructions are
//! undefined here: a UsageFault).

use crate::cpu::{Cpu, Stop};
use crate::scs;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
    Rrx,
}

/// Shift_C: (result, carry out)
fn shift_c(v: u32, t: Shift, n: u32, c: bool) -> (u32, bool) {
    if n == 0 && t != Shift::Rrx {
        return (v, c);
    }
    match t {
        Shift::Lsl => {
            if n >= 33 {
                (0, false)
            } else if n == 32 {
                (0, v & 1 != 0)
            } else {
                (v << n, v >> (32 - n) & 1 != 0)
            }
        }
        Shift::Lsr => {
            if n >= 33 {
                (0, false)
            } else if n == 32 {
                (0, v >> 31 != 0)
            } else {
                (v >> n, v >> (n - 1) & 1 != 0)
            }
        }
        Shift::Asr => {
            if n >= 32 {
                let s = (v as i32 >> 31) as u32;
                (s, s & 1 != 0)
            } else {
                (((v as i32) >> n) as u32, v >> (n - 1) & 1 != 0)
            }
        }
        Shift::Ror => {
            let r = v.rotate_right(n % 32);
            (r, r >> 31 != 0)
        }
        Shift::Rrx => ((c as u32) << 31 | v >> 1, v & 1 != 0),
    }
}

/// DecodeImmShift
fn decode_imm_shift(t: u32, imm5: u32) -> (Shift, u32) {
    match t {
        0 => (Shift::Lsl, imm5),
        1 => (Shift::Lsr, if imm5 == 0 { 32 } else { imm5 }),
        2 => (Shift::Asr, if imm5 == 0 { 32 } else { imm5 }),
        _ => {
            if imm5 == 0 {
                (Shift::Rrx, 1)
            } else {
                (Shift::Ror, imm5)
            }
        }
    }
}

fn reg_shift_type(t: u32) -> Shift {
    match t {
        0 => Shift::Lsl,
        1 => Shift::Lsr,
        2 => Shift::Asr,
        _ => Shift::Ror,
    }
}

/// AddWithCarry: (result, carry, overflow)
#[inline]
fn add_c(x: u32, y: u32, c: bool) -> (u32, bool, bool) {
    let u = x as u64 + y as u64 + c as u64;
    let s = x as i32 as i64 + y as i32 as i64 + c as i64;
    let r = u as u32;
    (r, u >> 32 != 0, r as i32 as i64 != s)
}

/// ThumbExpandImm_C: the modified immediate constant
fn expand_imm_c(imm12: u32, c: bool) -> (u32, bool) {
    if imm12 >> 10 == 0 {
        let b = imm12 & 0xff;
        let v = match imm12 >> 8 & 3 {
            0 => b,
            1 => b << 16 | b,
            2 => b << 24 | b << 8,
            _ => b << 24 | b << 16 | b << 8 | b,
        };
        (v, c)
    } else {
        let unrot = 0x80 | (imm12 & 0x7f);
        let r = unrot.rotate_right(imm12 >> 7);
        (r, r >> 31 != 0)
    }
}

fn sat_signed(v: i64, bits: u32) -> (u32, bool) {
    let max = (1i64 << (bits - 1)) - 1;
    let min = -(1i64 << (bits - 1));
    if v > max {
        (max as u32, true)
    } else if v < min {
        (min as u32, true)
    } else {
        (v as u32, false)
    }
}

fn sat_unsigned(v: i64, bits: u32) -> (u32, bool) {
    let max = (1i64 << bits) - 1;
    if v > max {
        (max as u32, true)
    } else if v < 0 {
        (0, true)
    } else {
        (v as u32, false)
    }
}

#[inline]
fn bits(x: u32, hi: u32, lo: u32) -> u32 { x >> lo & ((1u64 << (hi - lo + 1)) - 1) as u32 }

#[inline]
fn sext(v: u32, nbits: u32) -> u32 { ((v << (32 - nbits)) as i32 >> (32 - nbits)) as u32 }

impl Cpu {
    #[inline]
    fn set_nz(&mut self, r: u32) {
        self.n = r >> 31 != 0;
        self.z = r == 0;
    }

    /// The 16-bit data processing instructions set flags outside IT blocks.
    #[inline]
    fn flags16(&self) -> bool { !self.cur_in_it }

    // ======================= 16-bit =======================

    pub(crate) fn exec16(&mut self, i: u32) {
        let op = i >> 10;
        match op {
            0b000000..=0b001111 => self.dp16_shift_add(i),
            0b010000 => self.dp16_alu(i),
            0b010001 => self.dp16_special(i),
            0b010010 | 0b010011 => {
                // LDR (literal)
                let rt = bits(i, 10, 8);
                let a = (self.pc.wrapping_add(4) & !3).wrapping_add(bits(i, 7, 0) * 4);
                let v = self.read(a, 4);
                self.set_reg(rt, v);
            }
            0b010100..=0b010111 => {
                // Load/store, register offset
                let (rm, rn, rt) = (bits(i, 8, 6), bits(i, 5, 3), bits(i, 2, 0));
                let a = self.reg(rn).wrapping_add(self.reg(rm));
                match bits(i, 11, 9) {
                    0 => {
                        let v = self.reg(rt);
                        self.write(a, 4, v)
                    }
                    1 => {
                        let v = self.reg(rt);
                        self.write(a, 2, v)
                    }
                    2 => {
                        let v = self.reg(rt);
                        self.write(a, 1, v)
                    }
                    3 => {
                        let v = sext(self.read(a, 1), 8);
                        self.set_reg(rt, v)
                    }
                    4 => {
                        let v = self.read(a, 4);
                        self.set_reg(rt, v)
                    }
                    5 => {
                        let v = self.read(a, 2);
                        self.set_reg(rt, v)
                    }
                    6 => {
                        let v = self.read(a, 1);
                        self.set_reg(rt, v)
                    }
                    _ => {
                        let v = sext(self.read(a, 2), 16);
                        self.set_reg(rt, v)
                    }
                }
            }
            0b011000..=0b100011 => {
                // STR/LDR(B) and STRH/LDRH (immediate)
                let (imm5, rn, rt) = (bits(i, 10, 6), bits(i, 5, 3), bits(i, 2, 0));
                let (size, load) = match bits(i, 15, 11) {
                    0b01100 => (4, false),
                    0b01101 => (4, true),
                    0b01110 => (1, false),
                    0b01111 => (1, true),
                    0b10000 => (2, false),
                    _ => (2, true),
                };
                let a = self.reg(rn).wrapping_add(imm5 * size);
                if load {
                    let v = self.read(a, size);
                    self.set_reg(rt, v);
                } else {
                    let v = self.reg(rt);
                    self.write(a, size, v);
                }
            }
            0b100100..=0b100111 => {
                // STR/LDR (SP-relative)
                let rt = bits(i, 10, 8);
                let a = self.sp().wrapping_add(bits(i, 7, 0) * 4);
                if i & 1 << 11 != 0 {
                    let v = self.read(a, 4);
                    self.set_reg(rt, v);
                } else {
                    let v = self.reg(rt);
                    self.write(a, 4, v);
                }
            }
            0b101000 | 0b101001 => {
                // ADR
                let v = (self.pc.wrapping_add(4) & !3).wrapping_add(bits(i, 7, 0) * 4);
                self.set_reg(bits(i, 10, 8), v);
            }
            0b101010 | 0b101011 => {
                // ADD Rd, SP, #imm
                let v = self.sp().wrapping_add(bits(i, 7, 0) * 4);
                self.set_reg(bits(i, 10, 8), v);
            }
            0b101100..=0b101111 => self.misc16(i),
            0b110000 | 0b110001 => {
                // STM (STMIA) Rn!, {list}
                let rn = bits(i, 10, 8);
                let mut a = self.reg(rn);
                for r in 0..8 {
                    if i >> r & 1 != 0 {
                        let v = self.reg(r);
                        self.write(a, 4, v);
                        a = a.wrapping_add(4);
                    }
                }
                self.set_reg(rn, a);
            }
            0b110010 | 0b110011 => {
                // LDM (LDMIA) Rn{!}, {list}
                let rn = bits(i, 10, 8);
                let mut a = self.reg(rn);
                for r in 0..8 {
                    if i >> r & 1 != 0 {
                        let v = self.read(a, 4);
                        self.set_reg(r, v);
                        a = a.wrapping_add(4);
                    }
                }
                if i >> rn & 1 == 0 {
                    self.set_reg(rn, a);
                }
            }
            0b110100..=0b110111 => {
                let cond = bits(i, 11, 8);
                match cond {
                    0xe => self.undefined(i),
                    0xf => self.svc(),
                    _ => {
                        if self.cond(cond) {
                            let off = sext(bits(i, 7, 0) << 1, 9);
                            self.next_pc = self.pc.wrapping_add(4).wrapping_add(off);
                        }
                    }
                }
            }
            0b111000 | 0b111001 => {
                // B (unconditional)
                let off = sext(bits(i, 10, 0) << 1, 12);
                self.next_pc = self.pc.wrapping_add(4).wrapping_add(off);
            }
            _ => self.undefined(i),
        }
    }

    fn dp16_shift_add(&mut self, i: u32) {
        let s = self.flags16();
        match bits(i, 13, 9) {
            0b00000..=0b01011 => {
                // LSL/LSR/ASR (immediate); MOV (register) for LSL #0
                let (rm, rd) = (bits(i, 5, 3), bits(i, 2, 0));
                let (t, n) = decode_imm_shift(bits(i, 12, 11), bits(i, 10, 6));
                let (r, c) = shift_c(self.reg(rm), t, n, self.c);
                self.set_reg(rd, r);
                if s {
                    self.set_nz(r);
                    self.c = c;
                }
            }
            0b01100 | 0b01101 | 0b01110 | 0b01111 => {
                let (rn, rd) = (bits(i, 5, 3), bits(i, 2, 0));
                let y = if i & 1 << 10 != 0 { bits(i, 8, 6) } else { self.reg(bits(i, 8, 6)) };
                let x = self.reg(rn);
                let sub = i & 1 << 9 != 0;
                let (r, c, v) = if sub { add_c(x, !y, true) } else { add_c(x, y, false) };
                self.set_reg(rd, r);
                if s {
                    self.set_nz(r);
                    self.c = c;
                    self.v = v;
                }
            }
            _ => {
                let rdn = bits(i, 10, 8);
                let imm = bits(i, 7, 0);
                match bits(i, 12, 11) {
                    0 => {
                        // MOV imm
                        self.set_reg(rdn, imm);
                        if s {
                            self.set_nz(imm);
                        }
                    }
                    1 => {
                        // CMP imm
                        let (r, c, v) = add_c(self.reg(rdn), !imm, true);
                        self.set_nz(r);
                        self.c = c;
                        self.v = v;
                    }
                    k => {
                        let x = self.reg(rdn);
                        let (r, c, v) = if k == 2 { add_c(x, imm, false) } else { add_c(x, !imm, true) };
                        self.set_reg(rdn, r);
                        if s {
                            self.set_nz(r);
                            self.c = c;
                            self.v = v;
                        }
                    }
                }
            }
        }
    }

    fn dp16_alu(&mut self, i: u32) {
        let s = self.flags16();
        let (rm, rdn) = (bits(i, 5, 3), bits(i, 2, 0));
        let x = self.reg(rdn);
        let y = self.reg(rm);
        let op = bits(i, 9, 6);
        let mut c = self.c;
        let mut v = self.v;
        let (r, write) = match op {
            0x0 => (x & y, true),
            0x1 => (x ^ y, true),
            0x2 | 0x3 | 0x4 | 0x7 => {
                let t = [Shift::Lsl, Shift::Lsl, Shift::Lsl, Shift::Lsr, Shift::Asr, Shift::Lsl, Shift::Lsl, Shift::Ror][op as usize];
                let (r, cc) = shift_c(x, t, y & 0xff, self.c);
                c = cc;
                (r, true)
            }
            0x5 => {
                let (r, cc, vv) = add_c(x, y, self.c);
                c = cc;
                v = vv;
                (r, true)
            }
            0x6 => {
                let (r, cc, vv) = add_c(x, !y, self.c);
                c = cc;
                v = vv;
                (r, true)
            }
            0x8 => (x & y, false),
            0x9 => {
                // RSB #0 (NEG)
                let (r, cc, vv) = add_c(!y, 0, true);
                c = cc;
                v = vv;
                self.set_reg(rdn, r);
                if s {
                    self.set_nz(r);
                    self.c = c;
                    self.v = v;
                }
                return;
            }
            0xa => {
                let (r, cc, vv) = add_c(x, !y, true);
                self.set_nz(r);
                self.c = cc;
                self.v = vv;
                return;
            }
            0xb => {
                let (r, cc, vv) = add_c(x, y, false);
                self.set_nz(r);
                self.c = cc;
                self.v = vv;
                return;
            }
            0xc => (x | y, true),
            0xd => (x.wrapping_mul(y), true),
            0xe => (x & !y, true),
            _ => (!y, true),
        };
        if write {
            self.set_reg(rdn, r);
            if s {
                self.set_nz(r);
                self.c = c;
                self.v = v;
            }
        } else {
            // TST
            self.set_nz(r);
        }
    }

    fn dp16_special(&mut self, i: u32) {
        let rm = bits(i, 6, 3);
        let rdn = bits(i, 2, 0) | (i >> 7 & 1) << 3;
        match bits(i, 9, 8) {
            0 => {
                // ADD (register), high registers; no flags
                let r = self.reg(rdn).wrapping_add(self.reg(rm));
                if rdn == 15 {
                    self.branch_write(r);
                } else {
                    self.set_reg(rdn, r);
                }
            }
            1 => {
                let (r, c, v) = add_c(self.reg(rdn), !self.reg(rm), true);
                self.set_nz(r);
                self.c = c;
                self.v = v;
            }
            2 => {
                let r = self.reg(rm);
                if rdn == 15 {
                    self.branch_write(r);
                } else {
                    self.set_reg(rdn, r);
                }
            }
            _ => {
                let target = self.reg(rm);
                if i & 1 << 7 != 0 {
                    // BLX
                    self.r[14] = self.next_pc | 1;
                }
                self.bx_write(target);
            }
        }
    }

    fn misc16(&mut self, i: u32) {
        let op = bits(i, 11, 5);
        match op {
            0b0000000..=0b0000011 => {
                let v = self.sp().wrapping_add(bits(i, 6, 0) * 4);
                self.set_sp(v);
            }
            0b0000100..=0b0000111 => {
                let v = self.sp().wrapping_sub(bits(i, 6, 0) * 4);
                self.set_sp(v);
            }
            0b0001000..=0b0001111 | 0b0011000..=0b0011111 | 0b1001000..=0b1001111 | 0b1011000..=0b1011111 => {
                // CBZ / CBNZ
                let rn = bits(i, 2, 0);
                let off = (i >> 9 & 1) << 6 | bits(i, 7, 3) << 1;
                let nonzero = i & 1 << 11 != 0;
                if (self.reg(rn) != 0) == nonzero {
                    self.next_pc = self.pc.wrapping_add(4).wrapping_add(off);
                }
            }
            0b0010000..=0b0010111 => {
                let m = self.reg(bits(i, 5, 3));
                let v = match bits(i, 7, 6) {
                    0 => sext(m & 0xffff, 16),
                    1 => sext(m & 0xff, 8),
                    2 => m & 0xffff,
                    _ => m & 0xff,
                };
                self.set_reg(bits(i, 2, 0), v);
            }
            0b0100000..=0b0101111 => {
                // PUSH
                let list = bits(i, 7, 0) | (i >> 8 & 1) << 14;
                self.push(list);
            }
            0b0110011 => {
                // CPSIE / CPSID
                if self.privileged() {
                    let disable = i & 1 << 4 != 0;
                    if i & 2 != 0 {
                        self.primask = disable;
                    }
                    if i & 1 != 0 {
                        self.faultmask = disable;
                    }
                    self.bus.scs.dirty = true;
                }
            }
            0b1010000..=0b1010011 | 0b1010110 | 0b1010111 => {
                let m = self.reg(bits(i, 5, 3));
                let v = match bits(i, 7, 6) {
                    0 => m.swap_bytes(),
                    1 => (m & 0xff00_ff00) >> 8 | (m & 0x00ff_00ff) << 8,
                    _ => sext(((m & 0xff) << 8) | (m >> 8 & 0xff), 16),
                };
                self.set_reg(bits(i, 2, 0), v);
            }
            0b1100000..=0b1101111 => {
                // POP
                let list = bits(i, 7, 0) | (i >> 8 & 1) << 15;
                self.pop(list);
            }
            0b1110000..=0b1110111 => {
                // BKPT
                self.stopped = Some(Stop::Breakpoint);
                self.next_pc = self.pc;
            }
            0b1111000..=0b1111111 => {
                if bits(i, 3, 0) != 0 {
                    // IT
                    self.it = bits(i, 7, 0) as u8;
                } else {
                    match bits(i, 7, 4) {
                        2 => self.wfe(),
                        3 => self.wfi(),
                        4 => self.sev(),
                        _ => {}
                    }
                }
            }
            _ => self.undefined(i),
        }
    }

    pub(crate) fn privileged(&self) -> bool { self.handler_mode() || self.control & 1 == 0 }

    fn wfi(&mut self) {
        self.sleeping = true;
        self.in_wfe = false;
    }

    fn wfe(&mut self) {
        if self.event {
            self.event = false;
        } else {
            self.sleeping = true;
            self.in_wfe = true;
        }
    }

    /// SEV: this core's event, and the others' (the machine passes it on).
    fn sev(&mut self) {
        self.event = true;
        self.sev = true;
    }

    /// PUSH / STMDB SP!: registers in `list`, lowest at the lowest address.
    fn push(&mut self, list: u32) {
        let n = list.count_ones();
        let mut a = self.sp().wrapping_sub(4 * n);
        let base = a;
        for r in 0..16 {
            if list >> r & 1 != 0 {
                let v = self.reg(r);
                self.write(a, 4, v);
                a = a.wrapping_add(4);
            }
        }
        self.set_sp(base);
    }

    /// POP / LDMIA SP!
    fn pop(&mut self, list: u32) {
        let mut a = self.sp();
        let mut pc = None;
        for r in 0..16 {
            if list >> r & 1 != 0 {
                let v = self.read(a, 4);
                a = a.wrapping_add(4);
                if r == 15 {
                    pc = Some(v);
                } else if r != 13 {
                    self.r[r as usize] = v;
                }
            }
        }
        self.set_sp(a);
        if let Some(v) = pc {
            self.bx_write(v);
        }
    }

    // ======================= 32-bit =======================

    pub(crate) fn exec32(&mut self, i: u32) {
        let op1 = bits(i, 28, 27);
        let op2 = bits(i, 26, 20);
        let op = i >> 15 & 1;
        match op1 {
            1 => {
                if op2 & 0b1100100 == 0 {
                    self.ldm_stm32(i);
                } else if op2 & 0b1100100 == 0b0000100 {
                    self.ldrd_strd_excl_tb(i);
                } else if op2 & 0b1100000 == 0b0100000 {
                    self.dp_shifted_reg(i);
                } else {
                    self.coprocessor(i);
                }
            }
            2 => {
                if op == 1 {
                    self.branch_misc(i);
                } else if op2 & 0b0100000 == 0 {
                    self.dp_mod_imm(i);
                } else {
                    self.dp_plain_imm(i);
                }
            }
            _ => {
                if op2 & 0b1110001 == 0 {
                    self.store_single(i);
                } else if op2 & 0b1100111 == 0b0000001 {
                    self.load_single(i, 1);
                } else if op2 & 0b1100111 == 0b0000011 {
                    self.load_single(i, 2);
                } else if op2 & 0b1100111 == 0b0000101 {
                    self.load_single(i, 4);
                } else if op2 & 0b1110000 == 0b0100000 {
                    self.dp_register(i);
                } else if op2 & 0b1111000 == 0b0110000 {
                    self.multiply(i);
                } else if op2 & 0b1111000 == 0b0111000 {
                    self.long_multiply_divide(i);
                } else if op2 & 0b1000000 != 0 {
                    self.coprocessor(i);
                } else {
                    self.undefined(i);
                }
            }
        }
    }

    fn coprocessor(&mut self, i: u32) {
        let cp = bits(i, 11, 8);
        if self.fpu {
            self.fp_instruction(i);
        } else if (cp == 10 || cp == 11) && self.bus.scs.cpacr >> 20 & 0xf == 0 {
            // No FPU: NOCP if the coprocessor's disabled, else undefined
            let at = self.pc;
            self.usage_fault_at(scs::UFSR_NOCP, at);
        } else {
            self.undefined(i);
        }
    }

    fn ldm_stm32(&mut self, i: u32) {
        let op = bits(i, 24, 23);
        let w = i & 1 << 21 != 0;
        let l = i & 1 << 20 != 0;
        let rn = bits(i, 19, 16);
        let list = i & 0xffff;
        let n = list.count_ones();
        match op {
            1 => {
                // STMIA / LDMIA (POP.W when Rn is SP with writeback)
                let mut a = self.reg(rn);
                if l {
                    let mut pc = None;
                    let base_in_list = list >> rn & 1 != 0;
                    for r in 0..16 {
                        if list >> r & 1 != 0 {
                            let v = self.read(a, 4);
                            a = a.wrapping_add(4);
                            if r == 15 {
                                pc = Some(v);
                            } else {
                                self.set_reg(r, v);
                            }
                        }
                    }
                    if w && !base_in_list {
                        self.set_reg(rn, a);
                    }
                    if let Some(v) = pc {
                        self.bx_write(v);
                    }
                } else {
                    for r in 0..16 {
                        if list >> r & 1 != 0 {
                            let v = self.reg(r);
                            self.write(a, 4, v);
                            a = a.wrapping_add(4);
                        }
                    }
                    if w {
                        self.set_reg(rn, a);
                    }
                }
            }
            2 => {
                // STMDB (PUSH.W) / LDMDB
                let start = self.reg(rn).wrapping_sub(4 * n);
                let mut a = start;
                if l {
                    let mut pc = None;
                    let base_in_list = list >> rn & 1 != 0;
                    for r in 0..16 {
                        if list >> r & 1 != 0 {
                            let v = self.read(a, 4);
                            a = a.wrapping_add(4);
                            if r == 15 {
                                pc = Some(v);
                            } else {
                                self.set_reg(r, v);
                            }
                        }
                    }
                    if w && !base_in_list {
                        self.set_reg(rn, start);
                    }
                    if let Some(v) = pc {
                        self.bx_write(v);
                    }
                } else {
                    for r in 0..16 {
                        if list >> r & 1 != 0 {
                            let v = self.reg(r);
                            self.write(a, 4, v);
                            a = a.wrapping_add(4);
                        }
                    }
                    if w {
                        self.set_reg(rn, start);
                    }
                }
            }
            _ => self.undefined(i),
        }
    }

    fn ldrd_strd_excl_tb(&mut self, i: u32) {
        let op1 = bits(i, 24, 23);
        let op2 = bits(i, 21, 20);
        let op3 = bits(i, 7, 4);
        let rn = bits(i, 19, 16);
        let rt = bits(i, 15, 12);
        let rd = bits(i, 11, 8);
        if op1 == 0 && op2 == 0 {
            // STREX
            let a = self.reg(rn).wrapping_add(bits(i, 7, 0) * 4);
            let ok = self.excl == Some(a);
            if ok {
                let v = self.reg(rt);
                self.write(a, 4, v);
            }
            self.excl = None;
            self.set_reg(rd, !ok as u32);
        } else if op1 == 0 && op2 == 1 {
            // LDREX
            let a = self.reg(rn).wrapping_add(bits(i, 7, 0) * 4);
            self.excl = Some(a);
            let v = self.read(a, 4);
            self.set_reg(rt, v);
        } else if op1 & 2 == 0 && op2 & 2 == 2 || op1 & 2 == 2 {
            // LDRD / STRD (immediate)
            let p = i & 1 << 24 != 0;
            let u = i & 1 << 23 != 0;
            let w = i & 1 << 21 != 0;
            let l = i & 1 << 20 != 0;
            let imm = bits(i, 7, 0) * 4;
            let base = if rn == 15 { self.pc.wrapping_add(4) & !3 } else { self.reg(rn) };
            let off = if u { base.wrapping_add(imm) } else { base.wrapping_sub(imm) };
            let a = if p { off } else { base };
            if l {
                let v1 = self.read(a, 4);
                let v2 = self.read(a.wrapping_add(4), 4);
                self.set_reg(rt, v1);
                self.set_reg(rd, v2);
            } else {
                let v1 = self.reg(rt);
                let v2 = self.reg(rd);
                self.write(a, 4, v1);
                self.write(a.wrapping_add(4), 4, v2);
            }
            if w {
                self.set_reg(rn, off);
            }
        } else if op1 == 1 && op3 >= 8 {
            // ARMv8-M load-acquire / store-release: LDA(B/H), STL(B/H), and
            // their exclusive forms LDAEX(B/H), STLEX(B/H) (op3 1x00 byte,
            // 1x01 halfword, 1x10 word; bit 2 exclusive)
            let size = match op3 & 3 {
                0 => 1,
                1 => 2,
                2 => 4,
                _ => return self.undefined(i),
            };
            let exclusive = op3 & 4 != 0;
            let a = self.reg(rn);
            if op2 & 1 != 0 {
                if exclusive {
                    self.excl = Some(a);
                }
                let v = self.read(a, size);
                self.set_reg(rt, v);
            } else if exclusive {
                let rd = bits(i, 3, 0);
                let ok = self.excl == Some(a);
                if ok {
                    let v = self.reg(rt);
                    self.write(a, size, v);
                }
                self.excl = None;
                self.set_reg(rd, !ok as u32);
            } else {
                let v = self.reg(rt);
                self.write(a, size, v);
            }
        } else if op1 == 1 && op2 == 0 {
            // STREXB / STREXH
            let a = self.reg(rn);
            let size = if op3 == 4 { 1 } else if op3 == 5 { 2 } else { return self.undefined(i) };
            let rd = bits(i, 3, 0);
            let ok = self.excl == Some(a);
            if ok {
                let v = self.reg(rt);
                self.write(a, size, v);
            }
            self.excl = None;
            self.set_reg(rd, !ok as u32);
        } else if op1 == 1 && op2 == 1 {
            match op3 {
                0 | 1 => {
                    // TBB / TBH
                    let base = self.reg(rn);
                    let idx = self.reg(bits(i, 3, 0));
                    let half = if op3 == 1 {
                        self.read(base.wrapping_add(idx << 1), 2)
                    } else {
                        self.read(base.wrapping_add(idx), 1)
                    };
                    self.next_pc = self.pc.wrapping_add(4).wrapping_add(half * 2);
                }
                4 | 5 => {
                    let a = self.reg(rn);
                    self.excl = Some(a);
                    let v = self.read(a, if op3 == 4 { 1 } else { 2 });
                    self.set_reg(rt, v);
                }
                _ => self.undefined(i),
            }
        } else {
            self.undefined(i);
        }
    }

    /// Data processing (shifted register).
    fn dp_shifted_reg(&mut self, i: u32) {
        let op = bits(i, 24, 21);
        let s = i & 1 << 20 != 0;
        let rn = bits(i, 19, 16);
        let rd = bits(i, 11, 8);
        let rm = bits(i, 3, 0);
        let imm5 = bits(i, 14, 12) << 2 | bits(i, 7, 6);
        let ty = bits(i, 5, 4);
        if op == 0b0110 {
            // PKHBT / PKHTB
            let (t, n) = decode_imm_shift(ty, imm5);
            let (sh, _) = shift_c(self.reg(rm), t, n, self.c);
            let x = self.reg(rn);
            let r = if i & 1 << 5 != 0 { (x & 0xffff_0000) | (sh & 0xffff) } else { (sh & 0xffff_0000) | (x & 0xffff) };
            self.set_reg(rd, r);
            return;
        }
        let (t, n) = decode_imm_shift(ty, imm5);
        let (y, sc) = shift_c(self.reg(rm), t, n, self.c);
        self.dp_op(op, s, rn, rd, y, sc, i);
    }

    /// The data processing operations shared by the shifted register and
    /// modified immediate forms: op, flags, Rn, Rd, the second operand and
    /// its shifter carry.
    fn dp_op(&mut self, op: u32, s: bool, rn: u32, rd: u32, y: u32, sc: bool, i: u32) {
        let logical = |cpu: &mut Cpu, r: u32, write: bool| {
            if write {
                cpu.set_reg(rd, r);
            }
            if s {
                cpu.set_nz(r);
                cpu.c = sc;
            }
        };
        match op {
            0b0000 => {
                // AND, TST (Rd 15 with S)
                let r = self.reg(rn) & y;
                logical(self, r, rd != 15);
            }
            0b0001 => {
                let r = self.reg(rn) & !y;
                logical(self, r, true);
            }
            0b0010 => {
                // ORR, MOV (Rn 15)
                let r = if rn == 15 { y } else { self.reg(rn) | y };
                logical(self, r, true);
            }
            0b0011 => {
                // ORN, MVN (Rn 15)
                let r = if rn == 15 { !y } else { self.reg(rn) | !y };
                logical(self, r, true);
            }
            0b0100 => {
                // EOR, TEQ
                let r = self.reg(rn) ^ y;
                logical(self, r, rd != 15);
            }
            0b1000 | 0b1010 | 0b1011 | 0b1101 | 0b1110 => {
                let x = self.reg(rn);
                let (r, c, v) = match op {
                    0b1000 => add_c(x, y, false),
                    0b1010 => add_c(x, y, self.c),
                    0b1011 => add_c(x, !y, self.c),
                    0b1101 => add_c(x, !y, true),
                    _ => add_c(!x, y, true),
                };
                // CMN / CMP: Rd 15 with S
                if !(rd == 15 && s && (op == 0b1000 || op == 0b1101)) {
                    self.set_reg(rd, r);
                }
                if s {
                    self.set_nz(r);
                    self.c = c;
                    self.v = v;
                }
            }
            _ => self.undefined(i),
        }
    }

    fn dp_mod_imm(&mut self, i: u32) {
        let imm12 = (i >> 26 & 1) << 11 | bits(i, 14, 12) << 8 | bits(i, 7, 0);
        let (y, sc) = expand_imm_c(imm12, self.c);
        let op = bits(i, 24, 21);
        let s = i & 1 << 20 != 0;
        self.dp_op(op, s, bits(i, 19, 16), bits(i, 11, 8), y, sc, i);
    }

    fn dp_plain_imm(&mut self, i: u32) {
        let op = bits(i, 24, 20);
        let rn = bits(i, 19, 16);
        let rd = bits(i, 11, 8);
        let imm12 = (i >> 26 & 1) << 11 | bits(i, 14, 12) << 8 | bits(i, 7, 0);
        let imm16 = rn << 12 | imm12;
        let imm3_2 = bits(i, 14, 12) << 2 | bits(i, 7, 6);
        match op {
            0b00000 => {
                // ADDW / ADR (add)
                let base = if rn == 15 { self.pc.wrapping_add(4) & !3 } else { self.reg(rn) };
                self.set_reg(rd, base.wrapping_add(imm12));
            }
            0b00100 => self.set_reg(rd, imm16),
            0b01010 => {
                // SUBW / ADR (subtract)
                let base = if rn == 15 { self.pc.wrapping_add(4) & !3 } else { self.reg(rn) };
                self.set_reg(rd, base.wrapping_sub(imm12));
            }
            0b01100 => {
                // MOVT
                let v = (self.reg(rd) & 0xffff) | imm16 << 16;
                self.set_reg(rd, v);
            }
            0b10000 | 0b10010 | 0b11000 | 0b11010 => {
                // SSAT / USAT (with SSAT16 / USAT16 for no shift, op 10010/11010)
                let x = self.reg(rn);
                let sh = i & 1 << 21 != 0;
                let sat = bits(i, 4, 0);
                let unsigned = op & 0b01000 != 0;
                if !sh || imm3_2 != 0 || op & 0b00010 == 0 {
                    let (t, n) = if sh { (Shift::Asr, if imm3_2 == 0 { 32 } else { imm3_2 }) } else { (Shift::Lsl, imm3_2) };
                    let (v, _) = shift_c(x, t, n, self.c);
                    let (r, q) = if unsigned { sat_unsigned(v as i32 as i64, sat) } else { sat_signed(v as i32 as i64, sat + 1) };
                    self.set_reg(rd, r);
                    self.q |= q;
                } else {
                    // SSAT16 / USAT16
                    let lo = x as u16 as i16 as i64;
                    let hi = (x >> 16) as u16 as i16 as i64;
                    let ((l, q1), (h, q2)) = if unsigned {
                        (sat_unsigned(lo, sat), sat_unsigned(hi, sat))
                    } else {
                        (sat_signed(lo, sat + 1), sat_signed(hi, sat + 1))
                    };
                    self.set_reg(rd, (l & 0xffff) | h << 16);
                    self.q |= q1 || q2;
                }
            }
            0b10100 | 0b11100 => {
                // SBFX / UBFX
                let lsb = imm3_2;
                let width = bits(i, 4, 0) + 1;
                let x = self.reg(rn);
                let f = if lsb + width >= 32 { x >> lsb } else { x >> lsb & ((1 << width) - 1) };
                let r = if op == 0b10100 { sext(f, width) } else { f };
                self.set_reg(rd, r);
            }
            0b10110 => {
                // BFI / BFC (Rn 15)
                let lsb = imm3_2;
                let msb = bits(i, 4, 0);
                if msb >= lsb {
                    let width = msb - lsb + 1;
                    let m = if width == 32 { u32::MAX } else { ((1u32 << width) - 1) << lsb };
                    let src = if rn == 15 { 0 } else { self.reg(rn) << lsb };
                    let r = (self.reg(rd) & !m) | (src & m);
                    self.set_reg(rd, r);
                }
            }
            _ => self.undefined(i),
        }
    }

    fn branch_misc(&mut self, i: u32) {
        let op1 = bits(i, 14, 12);
        let op = bits(i, 26, 20);
        let s = i >> 26 & 1;
        let j1 = i >> 13 & 1;
        let j2 = i >> 11 & 1;
        if op1 & 0b101 == 0b000 {
            if op & 0b0111000 != 0b0111000 {
                // B<cond>.W
                let cond = bits(i, 25, 22);
                if self.cond(cond) {
                    let off = s << 20 | j2 << 19 | j1 << 18 | bits(i, 21, 16) << 12 | bits(i, 10, 0) << 1;
                    self.next_pc = self.pc.wrapping_add(4).wrapping_add(sext(off, 21));
                }
                return;
            }
            match op {
                0b0111000 | 0b0111001 => self.msr(i),
                0b0111010 => {
                    // Hints
                    match bits(i, 7, 0) {
                        2 => self.wfe(),
                        3 => self.wfi(),
                        4 => self.sev(),
                        _ => {}
                    }
                }
                0b0111011 => {
                    // CLREX, DSB, DMB, ISB
                    if bits(i, 7, 4) == 2 {
                        self.excl = None;
                    }
                }
                0b0111110 | 0b0111111 => self.mrs(i),
                _ => self.undefined(i),
            }
            return;
        }
        if op1 == 0b010 && op == 0b1111111 {
            // UDF.W
            return self.undefined(i);
        }
        // B.W and BL
        let i1 = !(j1 ^ s) & 1;
        let i2 = !(j2 ^ s) & 1;
        let off = sext(s << 24 | i1 << 23 | i2 << 22 | bits(i, 25, 16) << 12 | bits(i, 10, 0) << 1, 25);
        let target = self.pc.wrapping_add(4).wrapping_add(off);
        if op1 & 0b101 == 0b001 {
            self.next_pc = target;
        } else if op1 & 0b101 == 0b101 {
            self.r[14] = self.next_pc | 1;
            self.next_pc = target;
        } else {
            // BLX (immediate): ARM state, not on M
            self.undefined(i);
        }
    }

    fn mrs(&mut self, i: u32) {
        let rd = bits(i, 11, 8);
        let sysm = bits(i, 7, 0);
        let v = match sysm {
            0..=7 => {
                let mut v = 0;
                if sysm & 1 != 0 {
                    v |= self.ipsr as u32;
                }
                if sysm & 4 == 0 {
                    v |= self.xpsr() & 0xf80f_0000;
                }
                v
            }
            8 => {
                if self.privileged() {
                    self.msp
                } else {
                    0
                }
            }
            9 => {
                if self.privileged() {
                    self.psp
                } else {
                    0
                }
            }
            10 if self.bus.scs.v8m && self.privileged() => self.msplim,
            11 if self.bus.scs.v8m && self.privileged() => self.psplim,
            16 => self.primask as u32,
            17 | 18 => self.basepri as u32,
            19 => self.faultmask as u32,
            20 => self.control,
            _ => 0,
        };
        self.set_reg(rd, v);
    }

    fn msr(&mut self, i: u32) {
        let v = self.reg(bits(i, 19, 16));
        let sysm = bits(i, 7, 0);
        let mask = bits(i, 11, 10);
        self.bus.scs.dirty = true;
        match sysm {
            0..=3 => {
                if mask & 2 != 0 {
                    self.set_apsr_nzcvq(v);
                }
                if mask & 1 != 0 {
                    self.ge = v >> 16 & 0xf;
                }
            }
            _ if !self.privileged() => {}
            8 => self.msp = v & !3,
            9 => self.psp = v & !3,
            10 if self.bus.scs.v8m => self.msplim = v & !7,
            11 if self.bus.scs.v8m => self.psplim = v & !7,
            16 => self.primask = v & 1 != 0,
            17 => self.basepri = v as u8 & 0xe0,
            18 => {
                let b = v as u8 & 0xe0;
                if b != 0 && (self.basepri == 0 || b < self.basepri) {
                    self.basepri = b;
                }
            }
            19 => {
                if self.exec_prio() > -1 {
                    self.faultmask = v & 1 != 0;
                }
            }
            20 => {
                // nPRIV, and SPSEL in thread mode
                self.control = (self.control & !1) | (v & 1);
                if !self.handler_mode() {
                    self.control = (self.control & !2) | (v & 2);
                }
                self.control = (self.control & !4) | (v & 4);
            }
            _ => {}
        }
    }

    fn store_single(&mut self, i: u32) {
        let size = match bits(i, 22, 21) {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return self.undefined(i),
        };
        let rn = bits(i, 19, 16);
        let rt = bits(i, 15, 12);
        let Some((a, wb)) = self.ls_address(i) else { return self.undefined(i) };
        let v = self.reg(rt);
        self.write(a, size, v);
        if let Some(w) = wb {
            self.set_reg(rn, w);
        }
    }

    /// The address of a 32-bit single load/store (imm12, imm8 with P/U/W,
    /// or a shifted register), and the base's writeback value.
    fn ls_address(&mut self, i: u32) -> Option<(u32, Option<u32>)> {
        let rn = bits(i, 19, 16);
        if i & 1 << 23 != 0 {
            // imm12
            return Some((self.reg(rn).wrapping_add(bits(i, 11, 0)), None));
        }
        if i & 1 << 11 != 0 {
            // imm8: P U W in bits 10..8 (including the unprivileged T forms, as
            // ordinary accesses)
            let p = i & 1 << 10 != 0;
            let u = i & 1 << 9 != 0;
            let w = i & 1 << 8 != 0;
            let imm = bits(i, 7, 0);
            let base = self.reg(rn);
            let off = if u { base.wrapping_add(imm) } else { base.wrapping_sub(imm) };
            let a = if p { off } else { base };
            return Some((a, if w { Some(off) } else { None }));
        }
        if bits(i, 11, 6) == 0 {
            // Register, LSL #imm2
            let a = self.reg(rn).wrapping_add(self.reg(bits(i, 3, 0)) << bits(i, 5, 4));
            return Some((a, None));
        }
        None
    }

    /// LDR, LDRB, LDRSB, LDRH, LDRSH (32-bit), and the preload hints.
    fn load_single(&mut self, i: u32, size: u32) {
        let signed = i & 1 << 24 != 0;
        let rn = bits(i, 19, 16);
        let rt = bits(i, 15, 12);
        let a;
        let mut wb = None;
        if rn == 15 {
            // Literal
            let base = self.pc.wrapping_add(4) & !3;
            let imm = bits(i, 11, 0);
            a = if i & 1 << 23 != 0 { base.wrapping_add(imm) } else { base.wrapping_sub(imm) };
        } else {
            match self.ls_address(i) {
                Some((x, w)) => {
                    a = x;
                    wb = w;
                }
                None => return self.undefined(i),
            }
        }
        if rt == 15 && size != 4 {
            // PLD / PLI and other hints
            return;
        }
        let mut v = self.read(a, size);
        if signed && size < 4 {
            v = sext(v, 8 * size);
        }
        if let Some(w) = wb {
            self.set_reg(rn, w);
        }
        if rt == 15 {
            self.bx_write(v);
        } else {
            self.set_reg(rt, v);
        }
    }

    /// Data processing (register): shifts by register, extends, parallel
    /// add/subtract, miscellaneous.
    fn dp_register(&mut self, i: u32) {
        let op1 = bits(i, 23, 20);
        let op2 = bits(i, 7, 4);
        let rn = bits(i, 19, 16);
        let rd = bits(i, 11, 8);
        let rm = bits(i, 3, 0);
        if op2 == 0 && op1 & 0b1000 == 0 {
            // LSL, LSR, ASR, ROR (register)
            let s = op1 & 1 != 0;
            let (r, c) = shift_c(self.reg(rn), reg_shift_type(op1 >> 1), self.reg(rm) & 0xff, self.c);
            self.set_reg(rd, r);
            if s {
                self.set_nz(r);
                self.c = c;
            }
            return;
        }
        if op1 & 0b1000 == 0 && op2 & 0b1000 != 0 {
            // SXTAH SXTH UXTAH UXTH SXTAB16 SXTB16 UXTAB16 UXTB16 SXTAB SXTB UXTAB UXTB
            let rot = bits(i, 5, 4) * 8;
            let m = self.reg(rm).rotate_right(rot);
            let add = if rn == 15 { 0 } else { self.reg(rn) };
            let r = match op1 {
                0b000 => add.wrapping_add(sext(m & 0xffff, 16)),
                0b001 => add.wrapping_add(m & 0xffff),
                0b010 => {
                    let lo = (add as u16).wrapping_add(sext(m & 0xff, 8) as u16);
                    let hi = ((add >> 16) as u16).wrapping_add(sext(m >> 16 & 0xff, 8) as u16);
                    lo as u32 | (hi as u32) << 16
                }
                0b011 => {
                    let lo = (add as u16).wrapping_add((m & 0xff) as u16);
                    let hi = ((add >> 16) as u16).wrapping_add((m >> 16 & 0xff) as u16);
                    lo as u32 | (hi as u32) << 16
                }
                0b100 => add.wrapping_add(sext(m & 0xff, 8)),
                0b101 => add.wrapping_add(m & 0xff),
                _ => return self.undefined(i),
            };
            self.set_reg(rd, r);
            return;
        }
        if op1 & 0b1000 != 0 && op2 & 0b1000 == 0 {
            return self.parallel_addsub(i);
        }
        if op1 & 0b1100 == 0b1000 && op2 & 0b1100 == 0b1000 {
            // QADD, QDADD, QSUB, QDSUB, REV, REV16, RBIT, REVSH, SEL, CLZ
            let x = self.reg(rn);
            let y = self.reg(rm);
            let r = match (op1 & 3, op2 & 3) {
                (0, 0) => self.qadd(y as i32 as i64 + x as i32 as i64),
                (0, 1) => {
                    let d = self.qadd(2 * x as i32 as i64);
                    self.qadd(y as i32 as i64 + d as i32 as i64)
                }
                (0, 2) => self.qadd(y as i32 as i64 - x as i32 as i64),
                (0, 3) => {
                    let d = self.qadd(2 * x as i32 as i64);
                    self.qadd(y as i32 as i64 - d as i32 as i64)
                }
                (1, 0) => x.swap_bytes(),
                (1, 1) => (x & 0xff00_ff00) >> 8 | (x & 0x00ff_00ff) << 8,
                (1, 2) => x.reverse_bits(),
                (1, 3) => sext(((x & 0xff) << 8) | (x >> 8 & 0xff), 16),
                (2, 0) => {
                    // SEL
                    let mut r = 0;
                    for b in 0..4 {
                        let src = if self.ge >> b & 1 != 0 { x } else { y };
                        r |= src & (0xff << (8 * b));
                    }
                    r
                }
                (3, 0) => x.leading_zeros(),
                _ => return self.undefined(i),
            };
            self.set_reg(rd, r);
            return;
        }
        self.undefined(i);
    }

    /// Signed saturating to 32 bits, setting Q.
    fn qadd(&mut self, v: i64) -> u32 {
        let (r, q) = sat_signed(v, 32);
        self.q |= q;
        r
    }

    /// The parallel add/subtract instructions (SADD16 ... UHSAX).
    fn parallel_addsub(&mut self, i: u32) {
        let op1 = bits(i, 22, 20);
        let op2 = bits(i, 5, 4);
        let unsigned = i & 1 << 6 != 0;
        let x = self.reg(bits(i, 19, 16));
        let y = self.reg(bits(i, 3, 0));
        let rd = bits(i, 11, 8);
        // op2: 00 basic (sets GE), 01 Q (saturating), 10 H (halving)
        let mode = op2;
        let lanes8 = op1 == 0b000 || op1 == 0b100;
        let mut ge = 0u32;
        let mut r = 0u32;
        if lanes8 {
            let sub = op1 == 0b100;
            for b in 0..4 {
                let (a, c) = if unsigned {
                    ((x >> (8 * b) & 0xff) as i64, (y >> (8 * b) & 0xff) as i64)
                } else {
                    ((x >> (8 * b)) as u8 as i8 as i64, (y >> (8 * b)) as u8 as i8 as i64)
                };
                let s = if sub { a - c } else { a + c };
                let lane = match mode {
                    0 => {
                        let set = if unsigned { if sub { s >= 0 } else { s >= 0x100 } } else { s >= 0 };
                        if set {
                            ge |= 1 << b;
                        }
                        s as u32 & 0xff
                    }
                    1 => {
                        if unsigned {
                            sat_unsigned(s, 8).0
                        } else {
                            sat_signed(s, 8).0 & 0xff
                        }
                    }
                    _ => (s >> 1) as u32 & 0xff,
                };
                r |= lane << (8 * b);
            }
        } else {
            let (xl, xh, yl, yh) = if unsigned {
                ((x & 0xffff) as i64, (x >> 16) as i64, (y & 0xffff) as i64, (y >> 16) as i64)
            } else {
                (x as u16 as i16 as i64, (x >> 16) as u16 as i16 as i64, y as u16 as i16 as i64, (y >> 16) as u16 as i16 as i64)
            };
            // (lo, hi) results and whether each is an add
            let (lo, hi, lo_add, hi_add) = match op1 {
                0b001 => (xl + yl, xh + yh, true, true),
                0b010 => (xl - yh, xh + yl, false, true),
                0b110 => (xl + yh, xh - yl, true, false),
                0b101 => (xl - yl, xh - yh, false, false),
                _ => return self.undefined(i),
            };
            let one = |s: i64, add: bool, ge: &mut u32, shift: u32| -> u32 {
                match mode {
                    0 => {
                        let set = if unsigned && add { s >= 0x10000 } else { s >= 0 };
                        if set {
                            *ge |= 3 << shift;
                        }
                        s as u32 & 0xffff
                    }
                    1 => {
                        if unsigned {
                            sat_unsigned(s, 16).0
                        } else {
                            sat_signed(s, 16).0 & 0xffff
                        }
                    }
                    _ => (s >> 1) as u32 & 0xffff,
                }
            };
            r = one(lo, lo_add, &mut ge, 0) | one(hi, hi_add, &mut ge, 2) << 16;
        }
        if mode == 0 {
            self.ge = ge;
        }
        self.set_reg(rd, r);
    }

    /// Multiply, multiply-accumulate and absolute difference.
    fn multiply(&mut self, i: u32) {
        let op1 = bits(i, 22, 20);
        let op2 = bits(i, 5, 4);
        let rn = bits(i, 19, 16);
        let ra = bits(i, 15, 12);
        let rd = bits(i, 11, 8);
        let rm = bits(i, 3, 0);
        let x = self.reg(rn);
        let y = self.reg(rm);
        let acc = if ra == 15 { 0 } else { self.reg(ra) };
        let half = |v: u32, top: bool| -> i64 { if top { (v >> 16) as u16 as i16 as i64 } else { v as u16 as i16 as i64 } };
        let r = match op1 {
            0b000 => match op2 {
                0 => y.wrapping_mul(x).wrapping_add(acc),
                1 => acc.wrapping_sub(x.wrapping_mul(y)),
                _ => return self.undefined(i),
            },
            0b001 => {
                // SMLA<x><y> / SMUL<x><y>
                let p = half(x, i & 1 << 5 != 0) * half(y, i & 1 << 4 != 0);
                if ra == 15 {
                    p as u32
                } else {
                    let s = p + acc as i32 as i64;
                    if s != s as i32 as i64 {
                        self.q = true;
                    }
                    s as u32
                }
            }
            0b010 | 0b100 => {
                // SMLAD / SMUAD, SMLSD / SMUSD (X swaps the halves of Rm)
                let ym = if i & 1 << 4 != 0 { y.rotate_right(16) } else { y };
                let p1 = half(x, false) * half(ym, false);
                let p2 = half(x, true) * half(ym, true);
                let p = if op1 == 0b010 { p1 + p2 } else { p1 - p2 };
                let s = p + if ra == 15 { 0 } else { acc as i32 as i64 };
                if s != s as i32 as i64 {
                    self.q = true;
                }
                s as u32
            }
            0b011 => {
                // SMLAW<y> / SMULW<y>
                let p = (x as i32 as i64 * half(y, i & 1 << 4 != 0)) >> 16;
                if ra == 15 {
                    p as u32
                } else {
                    let s = p + acc as i32 as i64;
                    if s != s as i32 as i64 {
                        self.q = true;
                    }
                    s as u32
                }
            }
            0b101 | 0b110 => {
                // SMMLA / SMMUL / SMMLS (R rounds)
                let p = x as i32 as i64 * y as i32 as i64;
                let a = (acc as i64) << 32;
                let mut s = if op1 == 0b101 { a.wrapping_add(p) } else { a.wrapping_sub(p) };
                if ra == 15 && op1 == 0b101 {
                    s = p;
                }
                if i & 1 << 4 != 0 {
                    s = s.wrapping_add(0x8000_0000);
                }
                (s >> 32) as u32
            }
            0b111 => {
                // USAD8 / USADA8
                let mut sum = 0u32;
                for b in 0..4 {
                    let a = (x >> (8 * b) & 0xff) as i32;
                    let c = (y >> (8 * b) & 0xff) as i32;
                    sum += (a - c).unsigned_abs();
                }
                sum.wrapping_add(acc)
            }
            _ => return self.undefined(i),
        };
        self.set_reg(rd, r);
    }

    /// Long multiply, long multiply-accumulate, divide.
    fn long_multiply_divide(&mut self, i: u32) {
        let op1 = bits(i, 22, 20);
        let op2 = bits(i, 7, 4);
        let rn = bits(i, 19, 16);
        let rlo = bits(i, 15, 12);
        let rhi = bits(i, 11, 8);
        let rm = bits(i, 3, 0);
        let x = self.reg(rn);
        let y = self.reg(rm);
        match (op1, op2) {
            (0b001, 0b1111) | (0b011, 0b1111) => {
                // SDIV / UDIV (by zero: 0, unless DIV_0_TRP)
                if y == 0 {
                    if self.bus.scs.ccr & 1 << 4 != 0 {
                        let at = self.pc;
                        return self.usage_fault_at(scs::UFSR_DIVBYZERO, at);
                    }
                    return self.set_reg(rhi, 0);
                }
                let r = if op1 == 0b001 { (x as i32).wrapping_div(y as i32) as u32 } else { x / y };
                self.set_reg(rhi, r);
            }
            _ => {
                let acc = (self.reg(rhi) as u64) << 32 | self.reg(rlo) as u64;
                let half = |v: u32, top: bool| -> i64 { if top { (v >> 16) as u16 as i16 as i64 } else { v as u16 as i16 as i64 } };
                let r: u64 = match (op1, op2) {
                    (0b000, 0) => (x as i32 as i64).wrapping_mul(y as i32 as i64) as u64,
                    (0b010, 0) => x as u64 * y as u64,
                    (0b100, 0) => acc.wrapping_add((x as i32 as i64).wrapping_mul(y as i32 as i64) as u64),
                    (0b110, 0) => acc.wrapping_add(x as u64 * y as u64),
                    (0b110, 0b0110) => {
                        // UMAAL
                        (x as u64 * y as u64).wrapping_add(self.reg(rlo) as u64).wrapping_add(self.reg(rhi) as u64)
                    }
                    (0b100, 0b1000..=0b1011) => {
                        // SMLAL<x><y>
                        let p = half(x, op2 & 2 != 0) * half(y, op2 & 1 != 0);
                        acc.wrapping_add(p as u64)
                    }
                    (0b100, 0b1100 | 0b1101) | (0b101, 0b1100 | 0b1101) => {
                        // SMLALD / SMLSLD
                        let ym = if op2 & 1 != 0 { y.rotate_right(16) } else { y };
                        let p1 = half(x, false) * half(ym, false);
                        let p2 = half(x, true) * half(ym, true);
                        let p = if op1 == 0b100 { p1 + p2 } else { p1 - p2 };
                        acc.wrapping_add(p as u64)
                    }
                    _ => return self.undefined(i),
                };
                self.set_reg(rlo, r as u32);
                self.set_reg(rhi, (r >> 32) as u32);
            }
        }
    }
}
