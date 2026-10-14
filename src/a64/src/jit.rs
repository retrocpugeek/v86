//! JIT: compiles hot code to WebAssembly (browser build), a page at a time.
//!
//! The run loop counts entries to block starts (branch targets, and where
//! execution resumes after an exception, an interrupt or an instruction
//! without a handler of its own) in the decoded-instruction cache
//! (DCache::jit_page). When one gets hot, the code of its page is compiled
//! into one function (a region): its blocks are the hot block starts of the
//! page and everything reachable from them within the page, split into basic
//! blocks. The function dispatches on pc in a loop, so branches within the
//! page, including indirect ones (returns), stay in compiled code. A branch
//! to another page calls that page's compiled code, if the target is
//! installed there (up to MAX_HOPS nested calls). A block start that gets hot
//! later recompiles the page.
//!
//! The generated function is handed to the host (JS), which compiles it,
//! puts it into the function table and reports back through jit_done. The
//! run loop then calls it at installed block starts instead of interpreting.
//!
//! Pages with compiled code are flagged in Bus::code_pages. Every write to
//! RAM checks the flag (Bus::written, compiled stores, and A64CPU's JS
//! write functions for DMA); a write to a flagged page drops its compiled
//! code before compiled code runs again (jit_enter). Code that changes while
//! it runs is seen after it returns: changed code needs an ISB, which ends
//! compiled code, to be seen anyway.
//!
//! Compiled code
//! - checks on entry that the exception level and stack pointer selection
//!   are the ones it was compiled for;
//! - works on the Cpu struct in linear memory directly (it gets its address),
//!   with pc and icount in locals;
//! - stops before a block that would take icount past its `end` argument;
//! - does loads and stores of RAM mapped in the TLB inline, and others
//!   through jit_read/jit_write, which set jit_flag to 1 on a fault (it
//!   returns at that instruction, which takes the exception) and to 2 for
//!   anything but plain RAM (it returns after the instruction, so the run
//!   loop sees interrupts raised by devices);
//! - returns one of the STATUS_* values, with pc and icount updated as if
//!   the instructions had been interpreted.

use std::collections::HashMap;
use std::mem::offset_of;

use crate::cpu::Cpu;
use crate::decode::*;
use crate::exc::{ec, Exception};

/// Block entries before a block start is compiled.
pub const DEFAULT_THRESHOLD: u32 = 100;
/// Most compiled regions at a time (function table entries).
const MAX_FUNCS: u32 = 1 << 16;
/// Most calls from compiled code to another page's compiled code before it
/// returns to the run loop (each one nests a wasm call).
const MAX_HOPS: u32 = 64;
const PAGE_SLOTS: usize = 1024;

// JIT state of a block start: an entry counter, or one of these.
const INSTALLED: u32 = 0x8000_0000;
pub const NEVER: u32 = 0xffff_ffff;

#[inline(always)]
pub fn is_installed(j: u32) -> bool { j & 0xc000_0000 == INSTALLED }

#[inline(always)]
pub fn func_index(j: u32) -> u32 { j & 0xf_ffff }

// Return values of compiled code
pub const STATUS_OK: u32 = 0;
pub const STATUS_EXCEPTION: u32 = 1;
/// Compiled for the other exception level or stack pointer: interpret this time.
pub const STATUS_WRONG_MODE: u32 = 3;

/// Hands generated code to the host: (module bytes, function table index,
/// cookie). Returns 1 if the function is installed already, 0 if the host
/// will call Cpu::jit_done(cookie, ..) later, 2 if it failed.
pub type CompileFn = fn(&[u8], u32, u32) -> u32;

/// JIT state of a decoded RAM page.
pub struct JitPage {
    /// Per slot: entry counter, NEVER, or INSTALLED | region function index.
    pub state: [u32; PAGE_SLOTS],
    /// Function index of the installed region.
    region: Option<u32>,
    /// Cookie of the region being compiled.
    pub(crate) pending: Option<u32>,
}

impl JitPage {
    pub fn new() -> Box<JitPage> { Box::new(JitPage { state: [0; PAGE_SLOTS], region: None, pending: None }) }

    pub fn has_region(&self) -> bool { self.region.is_some() }

    /// Uninstall the region (its block starts count again from 0).
    pub fn drop_region(&mut self, free: &mut Vec<u32>) {
        if let Some(index) = self.region.take() {
            free.push(index);
            for j in self.state.iter_mut() {
                if is_installed(*j) {
                    *j = 0;
                }
            }
        }
    }

    /// Install region `index` at block starts `slots`, replacing the old one.
    pub fn install(&mut self, index: u32, slots: &[usize], free: &mut Vec<u32>) {
        self.drop_region(free);
        for &s in slots {
            self.state[s] = INSTALLED | index;
        }
        self.region = Some(index);
    }
}

struct Pending {
    page: usize,
    index: u32,
    slots: Vec<usize>,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct JitStats {
    pub compiled: u64,
    pub entered: u64,
    /// Regions dropped because their page was written
    pub stale: u64,
    pub failed: u64,
    /// Instructions run compiled
    pub insns: u64,
}

pub struct Jit {
    pub enabled: bool,
    pub threshold: u32,
    pub compile: Option<CompileFn>,
    /// First function table index that's ours.
    pub table_base: u32,
    next_func: u32,
    pending: HashMap<u32, Pending>,
    next_cookie: u32,
    pub stats: JitStats,
}

impl Default for Jit {
    fn default() -> Self {
        Jit {
            enabled: false,
            threshold: DEFAULT_THRESHOLD,
            compile: None,
            table_base: 0,
            next_func: 0,
            pending: HashMap::new(),
            next_cookie: 0,
            stats: JitStats::default(),
        }
    }
}

impl Cpu {
    /// Called at a block start: runs compiled code at pc if there is some,
    /// for at most `limit` steps, and returns the steps used. Otherwise
    /// counts the entry (compiling the page once it's hot) and returns None:
    /// the caller interprets.
    #[inline(always)]
    pub fn jit_enter(&mut self, limit: u64) -> Option<u64> {
        if !self.bus.code_written.is_empty() {
            self.jit_drop_written();
        }
        let pc = self.pc;
        if pc & !0xffc != self.tlb.fetch_page || self.il || (self.auto_irq && self.irq_deliverable()) {
            return self.jit_enter_slow(limit);
        }
        let n = ((pc & 0xffc) >> 2) as usize;
        // Safety: djit is the JIT state of fetch_page (set with it in fetch_slow)
        let j = unsafe { *self.djit.add(n) };
        if !is_installed(j) {
            self.jit_count(n);
            return None;
        }
        let icount = self.icount;
        self.jit_hops = MAX_HOPS;
        let status = self.jit_call(self.jit.table_base + func_index(j), icount + limit);
        self.jit_flag = 0;
        let used = self.icount - icount;
        match status {
            STATUS_OK if used != 0 => {
                self.jit.stats.entered += 1;
                self.jit.stats.insns += used;
                self.block_start = true;
                Some(used)
            }
            STATUS_EXCEPTION => {
                self.jit.stats.entered += 1;
                self.jit.stats.insns += self.icount - icount;
                let e = self.jit_exc;
                // Memory aborts don't count the instruction (see step())
                if e.ec != ec::DABT_LOWER && e.ec != ec::DABT_CUR {
                    self.icount += 1;
                }
                let pc = self.pc;
                self.insn_pc = pc;
                self.take_exception(e, pc);
                self.block_start = true;
                Some(self.icount - icount + 1)
            }
            // WRONG_MODE, possibly after chaining to code compiled for another mode
            _ => {
                self.block_start = true;
                (used != 0).then_some(used)
            }
        }
    }

    /// Drop the compiled code of pages that were written (Bus::code_pages).
    #[inline(never)]
    fn jit_drop_written(&mut self) {
        for p in std::mem::take(&mut self.bus.code_written) {
            if let Some(jp) = self.dcache.jit_meta(p as usize) {
                // A region being compiled from the old code is dropped when done
                jp.pending = None;
                if jp.has_region() {
                    self.jit.stats.stale += 1;
                }
            }
            self.dcache.jit_drop(p as usize);
        }
    }

    /// Allocate Bus::code_pages and DCache's table of JIT states; returns
    /// their addresses.
    pub fn jit_tables(&mut self) -> (usize, usize) {
        let pages = self.bus.ram.len() >> 12;
        if self.bus.code_pages.is_empty() {
            self.bus.code_pages = vec![0; pages];
        }
        (self.bus.code_pages.as_ptr() as usize, self.dcache.jit_ptrs(pages))
    }

    /// jit_enter() when the fetch cache doesn't cover pc (a branch to another
    /// page) or an interrupt is pending.
    #[inline(never)]
    fn jit_enter_slow(&mut self, limit: u64) -> Option<u64> {
        if self.il || (self.auto_irq && self.irq_deliverable()) {
            return None;
        }
        let pc = self.pc;
        if self.fetch_slow(pc).is_err() || pc & !0xffc != self.tlb.fetch_page {
            return None;
        }
        self.jit_enter(limit)
    }

    /// Count an entry to the block at slot n of the fetch page; compile the
    /// page once it's hot.
    #[inline(never)]
    fn jit_count(&mut self, n: usize) {
        // Safety: as in jit_enter
        let j = unsafe { &mut *self.djit.add(n) };
        if *j < self.jit.threshold {
            *j += 1;
        } else if *j != NEVER {
            self.jit_compile(n);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn jit_call(&mut self, table_index: u32, end: u64) -> u32 {
        // Safety: a function pointer is a function table index in wasm32, and
        // the host installed a function of this type there (jit_done).
        let f: extern "C" fn(*mut Cpu, u64) -> u32 = unsafe { std::mem::transmute(table_index as usize) };
        f(self as *mut Cpu, end)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn jit_call(&mut self, _: u32, _: u64) -> u32 { unreachable!("compiled code only runs in the browser build") }

    /// Compile the fetch page, whose block start at `slot` got hot.
    fn jit_compile(&mut self, slot: usize) {
        let ram = self.tlb.fetch_ram;
        let page = ram >> 12;
        let threshold = self.jit.threshold;
        let jp = self.dcache.jit_meta(page).expect("fetch page has JIT state");
        if jp.pending.is_some() {
            // Picked up when the page is compiled again
            return;
        }
        let (code_pages, ptrs) = self.jit_tables();
        // From here on, writes to the page are noticed
        self.bus.code_pages[page] = 1;
        let mut words = [0u32; PAGE_SLOTS];
        for (k, w) in words.iter_mut().enumerate() {
            *w = self.bus.read_ram(ram | k << 2, 4) as u32;
        }
        let set = |c: &mut Cpu, v: u32| unsafe { *c.djit.add(slot) = v };
        let Some(compile) = self.jit.compile else { return set(self, NEVER) };
        if decode_op(words[slot]).0.is_none() {
            return set(self, NEVER);
        }
        // Entries: the installed block starts and those that are getting hot
        let jp = self.dcache.jit_meta(page).unwrap();
        let entries: Vec<usize> = (0..PAGE_SLOTS)
            .filter(|&s| {
                let j = jp.state[s];
                s == slot || is_installed(j) || (j != NEVER && j != 0 && j >= threshold / 2)
            })
            .collect();
        let blocks = build_region(&words, &entries);
        let Some(index) = self.dcache.jit_free.pop().or_else(|| {
            (self.jit.next_func < MAX_FUNCS).then(|| {
                self.jit.next_func += 1;
                self.jit.next_func - 1
            })
        }) else {
            return set(self, NEVER);
        };
        let target = Target {
            code_addr: (self.bus.ram.as_ptr() as usize + ram) as u32,
            ram_addr: self.bus.ram.as_ptr() as u32,
            tlb_addr: self.tlb.entries_addr() as u32,
            el: self.el,
            sp_el1: self.el != 0 && self.spsel != 0,
            ptrs_addr: ptrs as u32,
            code_pages_addr: code_pages as u32,
            table_base: self.jit.table_base,
        };
        let code = codegen(&blocks, &target);
        let slots: Vec<usize> = blocks.iter().map(|b| b.slot).collect();
        let cookie = self.jit.next_cookie;
        self.jit.next_cookie = self.jit.next_cookie.wrapping_add(1);
        self.jit.stats.compiled += 1;
        match compile(&code, self.jit.table_base + index, cookie) {
            1 => self.dcache.jit_install(page, index, &slots),
            0 => {
                self.dcache.jit_meta(page).unwrap().pending = Some(cookie);
                self.jit.pending.insert(cookie, Pending { page, index, slots });
            }
            _ => {
                self.jit.stats.failed += 1;
                self.dcache.jit_free.push(index);
                set(self, NEVER);
            }
        }
    }

    /// The host finished compiling (ok) or failed to compile a region.
    pub fn jit_done(&mut self, cookie: u32, ok: bool) {
        let Some(p) = self.jit.pending.remove(&cookie) else { return };
        if !ok {
            self.jit.stats.failed += 1;
        }
        let install = match self.dcache.jit_meta(p.page) {
            Some(jp) if jp.pending == Some(cookie) => {
                jp.pending = None;
                ok
            }
            _ => false,
        };
        if install {
            self.dcache.jit_install(p.page, p.index, &p.slots);
        } else {
            self.dcache.jit_free.push(p.index);
        }
    }

    /// Load for compiled code: see the module comment.
    pub fn jit_read(&mut self, va: u64, size: u32) -> u64 {
        if (va & 0xfff) + size as u64 <= 0x1000 {
            if let Some(off) = self.ram_hit_read(va) {
                return self.bus.read_ram(off, size);
            }
            return match self.translate(va, crate::mmu::Acc::Read, false) {
                Ok(pa) => match self.bus.ram_offset(pa, size as u64) {
                    Some(off) => self.bus.read_ram(off, size),
                    None => {
                        self.jit_flag |= 2;
                        self.bus.read(pa, size)
                    }
                },
                Err(e) => self.jit_fault(e),
            };
        }
        match self.read(va, size, false) {
            Ok(v) => {
                self.jit_flag |= 2;
                v
            }
            Err(e) => self.jit_fault(e),
        }
    }

    /// Store for compiled code: see the module comment.
    pub fn jit_write(&mut self, va: u64, size: u32, v: u64) {
        if (va & 0xfff) + size as u64 <= 0x1000 {
            if let Some(off) = self.ram_hit_write(va) {
                return self.bus.write_ram(off, size, v);
            }
            match self.translate(va, crate::mmu::Acc::Write, false) {
                Ok(pa) => match self.bus.ram_offset(pa, size as u64) {
                    Some(off) => self.bus.write_ram(off, size, v),
                    None => {
                        self.jit_flag |= 2;
                        self.bus.write(pa, size, v)
                    }
                },
                Err(e) => {
                    self.jit_fault(e);
                }
            }
            return;
        }
        match self.write(va, size, v, false) {
            Ok(()) => self.jit_flag |= 2,
            Err(e) => {
                self.jit_fault(e);
            }
        }
    }

    #[cold]
    fn jit_fault(&mut self, e: Exception) -> u64 {
        self.jit_exc = e;
        self.jit_flag = 1;
        0
    }
}

// ---- regions ----

/// A basic block of a region: instructions from `slot` up to a branch (the
/// last instruction), or to where another block starts, an instruction that
/// can't be compiled or the end of the page.
pub struct RBlock {
    pub slot: usize,
    pub ops: Vec<(Form, Op)>,
}

/// Branches, and ISB (compiled code returns after it).
fn ends_block(f: Form) -> bool {
    matches!(
        f,
        Form::B | Form::Bl | Form::Br | Form::Blr | Form::BCond | Form::Cbz { .. } | Form::Tbz { .. } | Form::Barrier { isb: 1 }
    )
}

/// The slot a direct branch at `slot` goes to, if it's in the page.
fn branch_slot(f: Form, op: &Op, slot: usize) -> Option<usize> {
    if !matches!(f, Form::B | Form::Bl | Form::BCond | Form::Cbz { .. } | Form::Tbz { .. }) {
        return None;
    }
    let t = (4 * slot as i64).wrapping_add(op.imm as i64);
    (0..0x1000).contains(&t).then_some(t as usize / 4)
}

/// The basic blocks reachable within the page (`words`) from `entries`.
pub fn build_region(words: &[u32; PAGE_SLOTS], entries: &[usize]) -> Vec<RBlock> {
    let mut leader = [false; PAGE_SLOTS];
    let mut work: Vec<usize> = Vec::new();
    let add = |s: usize, leader: &mut [bool; PAGE_SLOTS], work: &mut Vec<usize>| {
        if s < PAGE_SLOTS && !leader[s] {
            leader[s] = true;
            work.push(s);
        }
    };
    for &s in entries {
        add(s, &mut leader, &mut work);
    }
    while let Some(s) = work.pop() {
        let mut k = s;
        while k < PAGE_SLOTS {
            let (Some(form), op) = decode_op(words[k]) else { break };
            if ends_block(form) {
                if let Some(t) = branch_slot(form, &op, k) {
                    add(t, &mut leader, &mut work);
                }
                // Not-taken path, or where a call returns to
                if !matches!(form, Form::B | Form::Br) {
                    add(k + 1, &mut leader, &mut work);
                }
                break;
            }
            k += 1;
            if k < PAGE_SLOTS && leader[k] {
                break;
            }
        }
    }
    let mut blocks = Vec::new();
    for s in (0..PAGE_SLOTS).filter(|&s| leader[s]) {
        let mut ops = Vec::new();
        let mut k = s;
        while k < PAGE_SLOTS && (k == s || !leader[k]) {
            let (Some(form), op) = decode_op(words[k]) else { break };
            ops.push((form, op));
            k += 1;
            if ends_block(form) {
                break;
            }
        }
        if !ops.is_empty() {
            blocks.push(RBlock { slot: s, ops });
        }
    }
    blocks
}

// ---- code generation ----

// Opcodes
const UNREACHABLE: u8 = 0x00;
const BLOCK: u8 = 0x02;
const LOOP: u8 = 0x03;
const BR: u8 = 0x0c;
const BR_TABLE: u8 = 0x0e;
const BLOCK_EMPTY: u8 = 0x40;
const IF: u8 = 0x04;
const END: u8 = 0x0b;
const RETURN: u8 = 0x0f;
const CALL: u8 = 0x10;
const CALL_INDIRECT: u8 = 0x11;
const SELECT: u8 = 0x1b;
const LOCAL_GET: u8 = 0x20;
const LOCAL_SET: u8 = 0x21;
const LOCAL_TEE: u8 = 0x22;
const ELSE: u8 = 0x05;
const I32_LOAD8_U: u8 = 0x2d;
const I32_LOAD16_U: u8 = 0x2f;
const I64_LOAD8_U: u8 = 0x31;
const I64_LOAD16_U: u8 = 0x33;
const I64_LOAD32_U: u8 = 0x35;
const I64_STORE8: u8 = 0x3c;
const I64_STORE16: u8 = 0x3d;
const I64_STORE32: u8 = 0x3e;
const I32_LE_U: u8 = 0x4d;
const I32_ADD: u8 = 0x6a;
const I32_SUB: u8 = 0x6b;
const I32_MUL: u8 = 0x6c;
const I64_EQ: u8 = 0x51;
const I32_LOAD: u8 = 0x28;
const I64_LOAD: u8 = 0x29;
const I32_STORE: u8 = 0x36;
const I64_STORE: u8 = 0x37;
const I32_CONST: u8 = 0x41;
const I64_CONST: u8 = 0x42;
const I32_EQZ: u8 = 0x45;
const I32_EQ: u8 = 0x46;
const I32_NE: u8 = 0x47;
const I32_LT_U: u8 = 0x49;
const I32_GE_U: u8 = 0x4f;
const I64_EQZ: u8 = 0x50;
const I64_LT_U: u8 = 0x54;
const I64_GT_U: u8 = 0x56;
const I64_GE_U: u8 = 0x5a;
const I32_AND: u8 = 0x71;
const I32_OR: u8 = 0x72;
const I32_XOR: u8 = 0x73;
const I32_SHL: u8 = 0x74;
const I32_SHR_S: u8 = 0x75;
const I32_SHR_U: u8 = 0x76;
const I32_ROTR: u8 = 0x78;
const I64_ADD: u8 = 0x7c;
const I64_SUB: u8 = 0x7d;
const I64_MUL: u8 = 0x7e;
const I64_AND: u8 = 0x83;
const I64_OR: u8 = 0x84;
const I64_XOR: u8 = 0x85;
const I64_SHL: u8 = 0x86;
const I64_SHR_S: u8 = 0x87;
const I32_CLZ: u8 = 0x67;
const I64_CLZ: u8 = 0x79;
const I64_SHR_U: u8 = 0x88;
const I64_ROTR: u8 = 0x8a;
const I32_WRAP_I64: u8 = 0xa7;
const I64_EXTEND_I32_U: u8 = 0xad;
const I64_EXTEND8_S: u8 = 0xc2;
const I64_EXTEND16_S: u8 = 0xc3;
const I64_EXTEND32_S: u8 = 0xc4;

// Imported functions
const FN_READ: u32 = 0;
const FN_WRITE: u32 = 1;

// Locals: the parameters, then i32s, then i64s
const CPU: u32 = 0;
/// icount to stop at
const ICOUNT_END: u32 = 1;
const N32: u32 = 2;
/// TLB entry address
const E32: u32 = 3;
/// jit_flag after the slow path of a load or store (0 if none was taken)
const F32: u32 = 4;
/// Slot of the block to run next (dispatch) or of pc (exit)
const SLOT: u32 = 5;
const NUM_I32: u32 = 4;
/// pc & !0xfff
const PAGE: u32 = 6;
/// icount at the start of the current block
const ICOUNT: u32 = 7;
const A: u32 = 8;
const B: u32 = 9;
const R: u32 = 10;
const T: u32 = 11;
const WB: u32 = 12;
const V: u32 = 13;
/// Address of a load or store
const VA: u32 = 14;
/// Value of a load (slow path)
const MV: u32 = 15;
const NUM_I64: u32 = 10;

const OFF_X: u32 = offset_of!(Cpu, x) as u32;
const OFF_PC: u32 = offset_of!(Cpu, pc) as u32;
const OFF_NZCV: u32 = offset_of!(Cpu, nzcv) as u32;
const OFF_ICOUNT: u32 = offset_of!(Cpu, icount) as u32;
const OFF_FLAG: u32 = offset_of!(Cpu, jit_flag) as u32;
const OFF_HOPS: u32 = offset_of!(Cpu, jit_hops) as u32;
const OFF_EL: u32 = offset_of!(Cpu, el) as u32;
const OFF_SPSEL: u32 = offset_of!(Cpu, spsel) as u32;
const OFF_SP_EL0: u32 = offset_of!(Cpu, sp_el0) as u32;
const OFF_SP_EL1: u32 = offset_of!(Cpu, sp_el1) as u32;
const OFF_SCTLR: u32 = (offset_of!(Cpu, sr) + offset_of!(crate::sysregs::SysRegs, sctlr_el1)) as u32;
const OFF_V: u32 = offset_of!(Cpu, v) as u32;
const OFF_DAIF: u32 = offset_of!(Cpu, daif) as u32;
const OFF_EXCL_ADDR: u32 = offset_of!(Cpu, excl_addr) as u32;
const OFF_EXCL_SIZE: u32 = offset_of!(Cpu, excl_size) as u32;
const fn off_sr(f: usize) -> u32 { (offset_of!(Cpu, sr) + f) as u32 }
const OFF_CPACR: u32 = off_sr(offset_of!(crate::sysregs::SysRegs, cpacr_el1));
const OFF_TPIDR_EL1: u32 = off_sr(offset_of!(crate::sysregs::SysRegs, tpidr_el1));
const OFF_TPIDR_EL0: u32 = off_sr(offset_of!(crate::sysregs::SysRegs, tpidr_el0));
const OFF_TPIDRRO_EL0: u32 = off_sr(offset_of!(crate::sysregs::SysRegs, tpidrro_el0));
const OFF_TLB_ASID: u32 = (offset_of!(Cpu, tlb) + offset_of!(crate::mmu::Tlb, asid)) as u32;

#[derive(Default)]
struct Asm {
    b: Vec<u8>,
}

impl Asm {
    fn op(&mut self, x: u8) { self.b.push(x) }

    fn uleb(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.b.push(byte);
                return;
            }
            self.b.push(byte | 0x80);
        }
    }

    fn sleb(&mut self, mut v: i64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0) {
                self.b.push(byte);
                return;
            }
            self.b.push(byte | 0x80);
        }
    }

    fn i32c(&mut self, v: i32) {
        self.op(I32_CONST);
        self.sleb(v as i64);
    }

    fn i64c(&mut self, v: u64) {
        self.op(I64_CONST);
        self.sleb(v as i64);
    }

    fn get(&mut self, l: u32) {
        self.op(LOCAL_GET);
        self.uleb(l as u64);
    }

    fn set(&mut self, l: u32) {
        self.op(LOCAL_SET);
        self.uleb(l as u64);
    }

    fn mem(&mut self, op: u8, align: u32, offset: u32) {
        self.op(op);
        self.uleb(align as u64);
        self.uleb(offset as u64);
    }

    /// Push a Cpu field.
    fn load64(&mut self, off: u32) {
        self.get(CPU);
        self.mem(I64_LOAD, 3, off);
    }

    fn load32(&mut self, off: u32) {
        self.get(CPU);
        self.mem(I32_LOAD, 2, off);
    }

    /// Cpu field = local.
    fn store64_local(&mut self, off: u32, l: u32) {
        self.get(CPU);
        self.get(l);
        self.mem(I64_STORE, 3, off);
    }

    fn if_(&mut self) {
        self.op(IF);
        self.op(BLOCK_EMPTY);
    }
}

/// What the generated code is built for.
pub struct Target {
    /// Address of the page's code in linear memory.
    pub code_addr: u32,
    /// Address of guest RAM and of the TLB entries in linear memory.
    pub ram_addr: u32,
    pub tlb_addr: u32,
    /// Exception level and stack pointer (SP_EL1 or SP_EL0) the code runs
    /// with; checked on entry.
    pub el: u32,
    pub sp_el1: bool,
    /// Address of DCache's table of JitPage::state addresses per RAM page,
    /// and the function table index of region 0 (to call other regions).
    pub ptrs_addr: u32,
    pub table_base: u32,
    /// Address of Bus::code_pages.
    pub code_pages_addr: u32,
}

struct Gen<'t> {
    a: Asm,
    t: &'t Target,
    sp_off: u32,
    uses_sp: bool,
    uses_el: bool,
    /// Blocks in the region, the current one and its first slot
    nblocks: usize,
    bi: usize,
    slot0: usize,
    /// Control structures open in the current block's code
    nest: u32,
}

impl Gen<'_> {
    fn x(&mut self, r: u8) {
        if r == 31 {
            self.a.i64c(0);
        } else {
            self.a.load64(OFF_X + 8 * r as u32);
        }
    }

    fn xs(&mut self, r: u8) {
        if r == 31 {
            self.uses_sp = true;
            self.a.load64(self.sp_off);
        } else {
            self.a.load64(OFF_X + 8 * r as u32);
        }
    }

    fn set_x(&mut self, r: u8, l: u32) {
        if r != 31 {
            self.a.store64_local(OFF_X + 8 * r as u32, l);
        }
    }

    fn set_xs(&mut self, r: u8, l: u32) {
        if r == 31 {
            self.uses_sp = true;
            self.a.store64_local(self.sp_off, l);
        } else {
            self.a.store64_local(OFF_X + 8 * r as u32, l);
        }
    }

    /// Truncate the value on the stack to 32 bits for W operations.
    fn mask(&mut self, sf: u32) {
        if sf == 0 {
            self.a.i64c(0xffff_ffff);
            self.a.op(I64_AND);
        }
    }

    fn if_(&mut self) {
        self.a.if_();
        self.nest += 1;
    }

    fn end(&mut self) {
        self.a.op(END);
        self.nest -= 1;
    }

    /// Branch to the dispatch loop (SLOT says where to).
    fn br_top(&mut self) {
        self.a.op(BR);
        self.a.uleb((self.nblocks - self.bi) as u64 + self.nest as u64);
    }

    /// Branch to the exit with pc at SLOT.
    fn br_exit(&mut self) {
        self.a.op(BR);
        self.a.uleb((self.nblocks - 1 - self.bi) as u64 + self.nest as u64);
    }

    /// Push the address of instruction k of the block.
    fn pc_k(&mut self, k: usize) {
        self.a.get(PAGE);
        self.a.i64c(4 * (self.slot0 + k) as u64);
        self.a.op(I64_ADD);
    }

    fn add_icount(&mut self, n: usize) {
        self.a.get(ICOUNT);
        self.a.i64c(n as u64);
        self.a.op(I64_ADD);
        self.a.set(ICOUNT);
    }

    /// Return `status` with icount = ICOUNT + n and pc as pushed by `pc`.
    fn ret(&mut self, pc: impl FnOnce(&mut Self), n: usize, status: u32) {
        self.a.get(CPU);
        pc(self);
        self.a.mem(I64_STORE, 3, OFF_PC);
        self.a.get(CPU);
        self.a.get(ICOUNT);
        if n != 0 {
            self.a.i64c(n as u64);
            self.a.op(I64_ADD);
        }
        self.a.mem(I64_STORE, 3, OFF_ICOUNT);
        self.a.i32c(status as i32);
        self.a.op(RETURN);
    }

    /// Go on at instruction k + offset (bytes) after `count` instructions of
    /// the block: in this region if it's in the page, else return.
    fn goto(&mut self, k: usize, offset: u64, count: usize) {
        let target = (4 * (self.slot0 + k) as u64).wrapping_add(offset);
        if target < 0x1000 {
            self.add_icount(count);
            self.a.i32c(target as i32 / 4);
            self.a.set(SLOT);
            self.br_top();
        } else {
            self.a.get(PAGE);
            self.a.i64c(target);
            self.a.op(I64_ADD);
            self.a.set(T);
            self.chain(count);
        }
    }

    /// Return after `count` instructions of the block with pc = T, calling
    /// the compiled code at T (another page's) first if there is some.
    fn chain(&mut self, count: usize) {
        use crate::mmu::{layout as l, P_X0, P_X1};
        self.ret_pc_icount(T, count);
        // Hops left?
        self.a.load32(OFF_HOPS);
        self.a.op(LOCAL_TEE);
        self.a.uleb(N32 as u64);
        self.if_();
        self.a.get(CPU);
        self.a.get(N32);
        self.a.i32c(1);
        self.a.op(I32_SUB);
        self.a.mem(I32_STORE, 2, OFF_HOPS);
        // Aligned, and executable RAM in the TLB
        self.tlb_hit_need(0, 4, if self.t.el == 0 { P_X0 } else { P_X1 });
        self.a.get(T);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(3);
        self.a.op(I32_AND);
        self.a.op(I32_EQZ);
        self.a.op(I32_AND);
        self.if_();
        // N32 = the RAM page: has it compiled code that wasn't written to?
        self.a.get(E32);
        self.a.mem(I32_LOAD, 2, l::RAM);
        self.a.i32c(12);
        self.a.op(I32_SHR_U);
        self.a.op(LOCAL_TEE);
        self.a.uleb(N32 as u64);
        self.a.mem(I32_LOAD8_U, 0, self.t.code_pages_addr);
        self.if_();
        // E32 = its JIT state
        self.a.get(N32);
        self.a.i32c(2);
        self.a.op(I32_SHL);
        self.a.mem(I32_LOAD, 2, self.t.ptrs_addr);
        self.a.op(LOCAL_TEE);
        self.a.uleb(E32 as u64);
        self.if_();
        // N32 = the slot's state: installed?
        self.a.get(E32);
        self.a.get(T);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(0xffc);
        self.a.op(I32_AND);
        self.a.op(I32_ADD);
        self.a.mem(I32_LOAD, 2, 0);
        self.a.op(LOCAL_TEE);
        self.a.uleb(N32 as u64);
        self.a.i32c(0xc000_0000u32 as i32);
        self.a.op(I32_AND);
        self.a.i32c(INSTALLED as i32);
        self.a.op(I32_EQ);
        self.if_();
        self.a.get(CPU);
        self.a.get(ICOUNT_END);
        self.a.get(N32);
        self.a.i32c(0xf_ffff);
        self.a.op(I32_AND);
        self.a.i32c(self.t.table_base as i32);
        self.a.op(I32_ADD);
        self.a.op(CALL_INDIRECT);
        self.a.uleb(0);
        self.a.uleb(0);
        self.a.op(RETURN);
        self.end();
        self.end();
        self.end();
        self.end();
        self.end();
        self.a.i32c(STATUS_OK as i32);
        self.a.op(RETURN);
    }

    /// cpu.pc = local `pc`, cpu.icount = ICOUNT + n.
    fn ret_pc_icount(&mut self, pc: u32, n: usize) {
        self.a.get(CPU);
        self.a.get(pc);
        self.a.mem(I64_STORE, 3, OFF_PC);
        self.a.get(CPU);
        self.a.get(ICOUNT);
        if n != 0 {
            self.a.i64c(n as u64);
            self.a.op(I64_ADD);
        }
        self.a.mem(I64_STORE, 3, OFF_ICOUNT);
    }

    /// Go on at local `pc` after `count` instructions of the block.
    fn goto_local(&mut self, pc: u32, count: usize) {
        self.add_icount(count);
        // Aligned and in this page: dispatch
        self.a.get(pc);
        self.a.i64c(!0xffc);
        self.a.op(I64_AND);
        self.a.get(PAGE);
        self.a.op(I64_EQ);
        self.if_();
        self.a.get(pc);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(0xfff);
        self.a.op(I32_AND);
        self.a.i32c(2);
        self.a.op(I32_SHR_U);
        self.a.set(SLOT);
        self.br_top();
        self.end();
        debug_assert_eq!(pc, T);
        self.chain(0);
    }

    /// Return at instruction k, which the interpreter runs.
    fn punt(&mut self, k: usize) { self.ret(|g| g.pc_k(k), k, STATUS_OK) }

    /// Punt (at instruction k) unless the condition on the stack holds.
    fn punt_unless(&mut self, k: usize) {
        self.a.op(I32_EQZ);
        self.if_();
        self.punt(k);
        self.end();
    }

    /// Punt unless FP/SIMD instructions are enabled (Cpu::check_fp).
    fn fp_check(&mut self, k: usize) {
        self.uses_el = true;
        self.a.load64(OFF_CPACR);
        self.a.i64c(20);
        self.a.op(I64_SHR_U);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(3);
        self.a.op(I32_AND);
        if self.t.el == 0 {
            self.a.i32c(3);
            self.a.op(I32_EQ);
        } else {
            self.a.i32c(1);
            self.a.op(I32_AND);
        }
        self.punt_unless(k);
    }

    /// Punt unless T is aligned to `bytes`.
    fn align_check(&mut self, k: usize, bytes: u32) {
        if bytes > 1 {
            self.a.get(T);
            self.a.op(I32_WRAP_I64);
            self.a.i32c(bytes as i32 - 1);
            self.a.op(I32_AND);
            self.a.op(I32_EQZ);
            self.punt_unless(k);
        }
    }

    /// Byte swap within each `n`-bit unit of R.
    fn swap(&mut self, n: u64, m: u64) {
        let a = &mut self.a;
        a.get(R);
        a.i64c(n);
        a.op(I64_SHR_U);
        a.i64c(m);
        a.op(I64_AND);
        a.get(R);
        a.i64c(m);
        a.op(I64_AND);
        a.i64c(n);
        a.op(I64_SHL);
        a.op(I64_OR);
        a.set(R);
    }

    /// Before a call to jit_read/jit_write at instruction k: icount as the
    /// interpreter has it (devices may read the time).
    fn sync_icount(&mut self, k: usize) {
        self.a.get(CPU);
        self.a.get(ICOUNT);
        self.a.i64c(k as u64 + 1);
        self.a.op(I64_ADD);
        self.a.mem(I64_STORE, 3, OFF_ICOUNT);
    }

    /// After a call to jit_read/jit_write: F32 = jit_flag, and on a fault,
    /// return at instruction k (which takes the exception).
    fn check_fault(&mut self, k: usize) {
        self.a.load32(OFF_FLAG);
        self.a.op(LOCAL_TEE);
        self.a.uleb(F32 as u64);
        self.a.i32c(1);
        self.a.op(I32_EQ);
        self.if_();
        self.ret(|g| g.pc_k(k), k, STATUS_EXCEPTION);
        self.end();
    }

    /// At the end of a load or store: if it wasn't plain RAM, return after it.
    fn check_device(&mut self, k: usize) {
        self.a.get(F32);
        self.if_();
        self.ret(|g| g.pc_k(k + 1), k + 1, STATUS_OK);
        self.end();
    }

    /// Push 1 if condition `cond` holds, else 0.
    fn cond(&mut self, cond: u32) {
        let bit = |g: &mut Gen, n: i32| {
            g.a.get(N32);
            g.a.i32c(n);
            g.a.op(I32_SHR_U);
            g.a.i32c(1);
            g.a.op(I32_AND);
        };
        let not = |g: &mut Gen| {
            g.a.i32c(1);
            g.a.op(I32_XOR);
        };
        self.a.load32(OFF_NZCV);
        self.a.set(N32);
        let (n, z, c, v) = (31, 30, 29, 28);
        match cond >> 1 {
            0 => bit(self, z),
            1 => bit(self, c),
            2 => bit(self, n),
            3 => bit(self, v),
            4 => {
                bit(self, c);
                bit(self, z);
                not(self);
                self.a.op(I32_AND);
            }
            5 => {
                bit(self, n);
                bit(self, v);
                self.a.op(I32_XOR);
                not(self);
            }
            6 => {
                bit(self, n);
                bit(self, v);
                self.a.op(I32_XOR);
                bit(self, z);
                self.a.op(I32_OR);
                not(self);
            }
            _ => self.a.i32c(1),
        }
        if cond & 1 != 0 && cond != 15 {
            not(self);
        }
    }

    /// NZCV for R = A + B (or A - B), R already truncated to the width.
    fn flags_addsub(&mut self, sf: u32, sub: u32) {
        let a = &mut self.a;
        a.get(CPU);
        if sf != 0 {
            // N, Z
            a.get(R);
            a.i64c(63);
            a.op(I64_SHR_U);
            a.op(I32_WRAP_I64);
            a.i32c(31);
            a.op(I32_SHL);
            a.get(R);
            a.op(I64_EQZ);
            a.i32c(30);
            a.op(I32_SHL);
            a.op(I32_OR);
            // C
            if sub != 0 {
                a.get(A);
                a.get(B);
                a.op(I64_GE_U);
            } else {
                a.get(R);
                a.get(A);
                a.op(I64_LT_U);
            }
            a.i32c(29);
            a.op(I32_SHL);
            a.op(I32_OR);
            // V
            a.get(A);
            a.get(if sub != 0 { B } else { R });
            a.op(I64_XOR);
            if sub != 0 {
                a.get(A);
            } else {
                a.get(B);
            }
            a.get(R);
            a.op(I64_XOR);
            a.op(I64_AND);
            a.i64c(63);
            a.op(I64_SHR_U);
            a.op(I32_WRAP_I64);
            a.i32c(28);
            a.op(I32_SHL);
            a.op(I32_OR);
        } else {
            let w = |a: &mut Asm, l: u32| {
                a.get(l);
                a.op(I32_WRAP_I64);
            };
            w(a, R);
            a.i32c(0x8000_0000u32 as i32);
            a.op(I32_AND);
            w(a, R);
            a.op(I32_EQZ);
            a.i32c(30);
            a.op(I32_SHL);
            a.op(I32_OR);
            if sub != 0 {
                w(a, A);
                w(a, B);
                a.op(I32_GE_U);
            } else {
                w(a, R);
                w(a, A);
                a.op(I32_LT_U);
            }
            a.i32c(29);
            a.op(I32_SHL);
            a.op(I32_OR);
            w(a, A);
            w(a, if sub != 0 { B } else { R });
            a.op(I32_XOR);
            w(a, if sub != 0 { A } else { B });
            w(a, R);
            a.op(I32_XOR);
            a.op(I32_AND);
            a.i32c(31);
            a.op(I32_SHR_U);
            a.i32c(28);
            a.op(I32_SHL);
            a.op(I32_OR);
        }
        a.mem(I32_STORE, 2, OFF_NZCV);
    }

    /// NZCV = N and Z of R (C = V = 0).
    fn flags_nz(&mut self, sf: u32) {
        let a = &mut self.a;
        a.get(CPU);
        a.get(R);
        if sf != 0 {
            a.i64c(63);
            a.op(I64_SHR_U);
            a.op(I32_WRAP_I64);
            a.i32c(31);
            a.op(I32_SHL);
        } else {
            a.op(I32_WRAP_I64);
            a.i32c(0x8000_0000u32 as i32);
            a.op(I32_AND);
        }
        a.get(R);
        a.op(I64_EQZ);
        a.i32c(30);
        a.op(I32_SHL);
        a.op(I32_OR);
        a.mem(I32_STORE, 2, OFF_NZCV);
    }

    /// Shift the value on the stack by a constant (ShiftReg()), truncated to the width.
    fn shift_const(&mut self, ty: u32, amount: u32, sf: u32) {
        self.mask(sf);
        if amount == 0 {
            return;
        }
        let a = &mut self.a;
        match (ty, sf != 0) {
            (0, _) => {
                a.i64c(amount as u64);
                a.op(I64_SHL);
                self.mask(sf);
            }
            (1, _) => {
                a.i64c(amount as u64);
                a.op(I64_SHR_U);
            }
            (2, true) => {
                a.i64c(amount as u64);
                a.op(I64_SHR_S);
            }
            (_, true) => {
                a.i64c(amount as u64);
                a.op(I64_ROTR);
            }
            (_, false) => {
                a.op(I32_WRAP_I64);
                a.i32c(amount as i32);
                a.op(if ty == 2 { I32_SHR_S } else { I32_ROTR });
                a.op(I64_EXTEND_I32_U);
            }
        }
    }

    /// ExtendReg() of the value on the stack.
    fn extend(&mut self, option: u32, shift: u32, sf: u32) {
        let a = &mut self.a;
        match (option & 3, option & 4 != 0) {
            (0, false) => {
                a.i64c(0xff);
                a.op(I64_AND);
            }
            (1, false) => {
                a.i64c(0xffff);
                a.op(I64_AND);
            }
            (2, false) => {
                a.i64c(0xffff_ffff);
                a.op(I64_AND);
            }
            (0, true) => a.op(I64_EXTEND8_S),
            (1, true) => a.op(I64_EXTEND16_S),
            (2, true) => a.op(I64_EXTEND32_S),
            _ => {}
        }
        if shift != 0 {
            a.i64c(shift as u64);
            a.op(I64_SHL);
        }
        self.mask(sf);
    }

    /// R = A + B or A - B, truncated.
    fn addsub(&mut self, sf: u32, sub: u32) {
        self.a.get(A);
        self.a.get(B);
        self.a.op(if sub != 0 { I64_SUB } else { I64_ADD });
        self.mask(sf);
        self.a.set(R);
    }

    /// T = access address, WB = writeback value.
    fn address(&mut self, op: &Op, mode: u32) {
        self.xs(op.b);
        self.a.set(A);
        match mode {
            OFF | PRE => {
                self.a.get(A);
                self.a.i64c(op.imm);
                self.a.op(I64_ADD);
                self.a.set(T);
                if mode == PRE {
                    self.a.get(T);
                    self.a.set(WB);
                }
            }
            POST => {
                self.a.get(A);
                self.a.set(T);
                self.a.get(A);
                self.a.i64c(op.imm);
                self.a.op(I64_ADD);
                self.a.set(WB);
            }
            _ => {
                self.a.get(A);
                self.x(op.c);
                self.extend(op.d as u32, op.imm as u32, 1);
                self.a.op(I64_ADD);
                self.a.set(T);
            }
        }
    }

    fn writeback(&mut self, op: &Op, mode: u32) {
        if mode == POST || mode == PRE {
            self.set_xs(op.b, WB);
        }
    }

    /// Start of a load or store instruction.
    fn mem_start(&mut self) {
        self.uses_el = true;
        self.a.i32c(0);
        self.a.set(F32);
    }

    /// VA = T + offset; push 1 if the TLB has a RAM entry for VA that allows
    /// the access and VA..VA+bytes doesn't cross a page (Cpu::ram_hit), with
    /// E32 = the entry's address. For a write, the page also mustn't have
    /// compiled code (the slow path notes the write: Bus::written).
    fn tlb_hit(&mut self, offset: u64, bytes: u32, write: bool) {
        use crate::mmu::{layout as l, P_R0, P_R1, P_W0, P_W1};
        let need = match (write, self.t.el == 0) {
            (false, false) => P_R1,
            (false, true) => P_R0,
            (true, false) => P_W1,
            (true, true) => P_W0,
        };
        self.tlb_hit_need(offset, bytes, need);
        if write {
            // (RAM is -1 for anything but RAM: then this reads the byte
            // before the table, with the result ignored)
            self.a.get(E32);
            self.a.mem(I32_LOAD, 2, l::RAM);
            self.a.i32c(12);
            self.a.op(I32_SHR_S);
            self.a.i32c(self.t.code_pages_addr as i32);
            self.a.op(I32_ADD);
            self.a.mem(I32_LOAD8_U, 0, 0);
            self.a.op(I32_EQZ);
            self.a.op(I32_AND);
        }
    }

    /// tlb_hit() for an access that needs permission `need` (P_*).
    fn tlb_hit_need(&mut self, offset: u64, bytes: u32, need: u8) {
        use crate::mmu::layout as l;
        self.uses_el = true;
        let a = &mut self.a;
        a.get(T);
        if offset != 0 {
            a.i64c(offset);
            a.op(I64_ADD);
        }
        a.set(VA);
        // E32 = entries + index(vpn) * size
        a.get(VA);
        a.i64c(12);
        a.op(I64_SHR_U);
        a.get(VA);
        a.i64c(24);
        a.op(I64_SHR_U);
        a.op(I64_XOR);
        a.op(I32_WRAP_I64);
        a.i32c(l::ENTRIES as i32 - 1);
        a.op(I32_AND);
        a.i32c(l::ENTRY_SIZE as i32);
        a.op(I32_MUL);
        a.i32c(self.t.tlb_addr as i32);
        a.op(I32_ADD);
        a.op(LOCAL_TEE);
        a.uleb(E32 as u64);
        // vpn matches
        a.mem(I64_LOAD, 3, l::VPN);
        a.get(VA);
        a.i64c(12);
        a.op(I64_SHR_U);
        a.op(I64_EQ);
        // MMU on
        a.load64(OFF_SCTLR);
        a.op(I32_WRAP_I64);
        a.op(I32_AND);
        // within the page
        a.get(VA);
        a.op(I32_WRAP_I64);
        a.i32c(0xfff);
        a.op(I32_AND);
        a.i32c(0x1000 - bytes as i32);
        a.op(I32_LE_U);
        a.op(I32_AND);
        // global or the current ASID
        a.get(E32);
        a.mem(I32_LOAD8_U, 0, l::GLOBAL);
        a.get(E32);
        a.mem(I32_LOAD16_U, 1, l::ASID);
        a.get(CPU);
        a.mem(I32_LOAD16_U, 1, OFF_TLB_ASID);
        a.op(I32_EQ);
        a.op(I32_OR);
        a.op(I32_AND);
        // permission
        a.get(E32);
        a.mem(I32_LOAD8_U, 0, l::PERM);
        a.i32c(need as i32);
        a.op(I32_AND);
        a.i32c(0);
        a.op(I32_NE);
        a.op(I32_AND);
        // RAM
        a.get(E32);
        a.mem(I32_LOAD, 2, l::RAM);
        a.i32c(-1);
        a.op(I32_NE);
        a.op(I32_AND);
    }

    /// Push the address in linear memory of VA (base for a memarg offset of
    /// ram_addr), after a TLB hit.
    fn ram_ptr(&mut self) {
        use crate::mmu::layout as l;
        self.a.get(E32);
        self.a.mem(I32_LOAD, 2, l::RAM);
        self.a.get(VA);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(0xfff);
        self.a.op(I32_AND);
        self.a.op(I32_ADD);
    }

    /// Push the value at T + offset (bytes wide, zero-extended); on a fault
    /// return at instruction k.
    fn load(&mut self, k: usize, offset: u64, bytes: u32) {
        self.tlb_hit(offset, bytes, false);
        self.a.op(IF);
        self.a.op(0x7e); // i64 result
        self.nest += 1;
        self.ram_ptr();
        let op = match bytes {
            1 => I64_LOAD8_U,
            2 => I64_LOAD16_U,
            4 => I64_LOAD32_U,
            _ => I64_LOAD,
        };
        self.a.mem(op, 0, self.t.ram_addr);
        self.a.op(ELSE);
        self.sync_icount(k);
        self.a.get(CPU);
        self.a.get(VA);
        self.a.i32c(bytes as i32);
        self.a.op(CALL);
        self.a.uleb(FN_READ as u64);
        self.a.set(MV);
        self.check_fault(k);
        self.a.get(MV);
        self.end();
    }

    /// Store local v (bytes wide) to T + offset; on a fault return
    /// at instruction k.
    fn store(&mut self, k: usize, offset: u64, bytes: u32, v: u32) {
        self.tlb_hit(offset, bytes, true);
        self.if_();
        self.ram_ptr();
        self.a.get(v);
        let op = match bytes {
            1 => I64_STORE8,
            2 => I64_STORE16,
            4 => I64_STORE32,
            _ => I64_STORE,
        };
        self.a.mem(op, 0, self.t.ram_addr);
        self.a.op(ELSE);
        self.sync_icount(k);
        self.a.get(CPU);
        self.a.get(VA);
        self.a.i32c(bytes as i32);
        self.a.get(v);
        self.a.op(CALL);
        self.a.uleb(FN_WRITE as u64);
        self.check_fault(k);
        self.end();
    }

    fn sext(&mut self, size: u32) {
        self.a.op(match size {
            0 => I64_EXTEND8_S,
            1 => I64_EXTEND16_S,
            _ => I64_EXTEND32_S,
        });
    }

    /// Instruction k of the block. Returns false if the block ends with it.
    fn insn(&mut self, k: usize, form: Form, op: &Op) -> bool {
        let w = |sf: u32| if sf != 0 { 64u32 } else { 32 };
        let ones = |sf: u32| if sf != 0 { u64::MAX } else { 0xffff_ffff };
        match form {
            Form::Adr | Form::Adrp => {
                self.pc_k(k);
                if form == Form::Adrp {
                    self.a.i64c(!0xfff);
                    self.a.op(I64_AND);
                }
                self.a.i64c(op.imm);
                self.a.op(I64_ADD);
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::AddImm { sf, sub, s } => {
                self.xs(op.b);
                self.a.set(A);
                self.a.i64c(op.imm);
                self.a.set(B);
                self.addsub(sf, sub);
                if s != 0 {
                    self.flags_addsub(sf, sub);
                    self.set_x(op.a, R);
                } else {
                    self.set_xs(op.a, R);
                }
            }
            Form::LogicImm { sf, opc } => {
                self.x(op.b);
                self.a.i64c(op.imm);
                self.a.op([I64_AND, I64_OR, I64_XOR, I64_AND][opc as usize]);
                self.mask(sf);
                self.a.set(R);
                if opc == 3 {
                    self.flags_nz(sf);
                    self.set_x(op.a, R);
                } else {
                    self.set_xs(op.a, R);
                }
            }
            Form::MovConst => {
                self.a.i64c(op.imm);
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::Movk { sf } => {
                self.x(op.a);
                self.a.i64c(!(0xffffu64 << op.d));
                self.a.op(I64_AND);
                self.a.i64c(op.imm);
                self.a.op(I64_OR);
                self.mask(sf);
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::Bitfield { sf, opc } => {
                let (imms, immr) = (op.c as u32, op.d as u32);
                self.x(op.b);
                self.mask(sf);
                self.a.set(A);
                self.a.get(A);
                if sf != 0 {
                    self.a.i64c(immr as u64);
                    self.a.op(I64_ROTR);
                } else {
                    self.a.op(I32_WRAP_I64);
                    self.a.i32c(immr as i32);
                    self.a.op(I32_ROTR);
                    self.a.op(I64_EXTEND_I32_U);
                }
                self.a.i64c(op.imm);
                self.a.op(I64_AND);
                if opc == 0 {
                    let tmask = crate::bits::ones((imms.wrapping_sub(immr) & (w(sf) - 1)) + 1);
                    let hi = ones(sf) & !tmask;
                    if hi != 0 {
                        // | (sign bit ? hi : 0)
                        self.a.i64c(0);
                        self.a.get(A);
                        self.a.i64c(imms as u64);
                        self.a.op(I64_SHR_U);
                        self.a.i64c(1);
                        self.a.op(I64_AND);
                        self.a.op(I64_SUB);
                        self.a.i64c(hi);
                        self.a.op(I64_AND);
                        self.a.op(I64_OR);
                    }
                }
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::Extr { sf } => {
                let s = op.d as u32;
                self.x(op.c);
                self.mask(sf);
                if s != 0 {
                    self.a.i64c(s as u64);
                    self.a.op(I64_SHR_U);
                    self.x(op.b);
                    self.a.i64c((w(sf) - s) as u64);
                    self.a.op(I64_SHL);
                    self.a.op(I64_OR);
                    self.mask(sf);
                }
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::B => {
                self.goto(k, op.imm, k + 1);
                return false;
            }
            Form::Bl => {
                self.a.get(CPU);
                self.pc_k(k);
                self.a.i64c(4);
                self.a.op(I64_ADD);
                self.a.mem(I64_STORE, 3, OFF_X + 8 * 30);
                self.goto(k, op.imm, k + 1);
                return false;
            }
            Form::BCond => {
                self.cond(op.d as u32);
                self.if_();
                self.goto(k, op.imm, k + 1);
                self.end();
            }
            Form::Cbz { sf, nz } => {
                self.x(op.a);
                self.mask(sf);
                self.a.op(I64_EQZ);
                if nz != 0 {
                    self.a.op(I32_EQZ);
                }
                self.if_();
                self.goto(k, op.imm, k + 1);
                self.end();
            }
            Form::Tbz { nz } => {
                self.x(op.a);
                self.a.i64c(op.d as u64);
                self.a.op(I64_SHR_U);
                self.a.i64c(1);
                self.a.op(I64_AND);
                self.a.op(I64_EQZ);
                if nz != 0 {
                    self.a.op(I32_EQZ);
                }
                self.if_();
                self.goto(k, op.imm, k + 1);
                self.end();
            }
            Form::Br | Form::Blr => {
                self.x(op.b);
                self.a.set(T);
                if form == Form::Blr {
                    self.a.get(CPU);
                    self.pc_k(k);
                    self.a.i64c(4);
                    self.a.op(I64_ADD);
                    self.a.mem(I64_STORE, 3, OFF_X + 8 * 30);
                }
                self.goto_local(T, k + 1);
                return false;
            }
            Form::Nop => {}
            Form::LogicReg { sf, opc, n, shift: _ } => {
                self.x(op.c);
                self.shift_const(op.d as u32, op.imm as u32, sf);
                if n != 0 {
                    self.a.i64c(ones(sf));
                    self.a.op(I64_XOR);
                }
                self.a.set(B);
                self.x(op.b);
                self.a.get(B);
                self.a.op([I64_AND, I64_OR, I64_XOR, I64_AND][opc as usize]);
                self.mask(sf);
                self.a.set(R);
                if opc == 3 {
                    self.flags_nz(sf);
                }
                self.set_x(op.a, R);
            }
            Form::AddReg { sf, sub, s, shift: _ } => {
                self.x(op.c);
                self.shift_const(op.d as u32, op.imm as u32, sf);
                self.a.set(B);
                self.x(op.b);
                self.a.set(A);
                self.addsub(sf, sub);
                if s != 0 {
                    self.flags_addsub(sf, sub);
                }
                self.set_x(op.a, R);
            }
            Form::AddExt { sf, sub, s } => {
                self.x(op.c);
                self.extend(op.d as u32, op.imm as u32, sf);
                self.a.set(B);
                self.xs(op.b);
                self.a.set(A);
                self.addsub(sf, sub);
                if s != 0 {
                    self.flags_addsub(sf, sub);
                    self.set_x(op.a, R);
                } else {
                    self.set_xs(op.a, R);
                }
            }
            Form::Csel { sf, op: o } => {
                self.x(op.b);
                self.x(op.c);
                match o {
                    1 => {
                        self.a.i64c(1);
                        self.a.op(I64_ADD);
                    }
                    2 => {
                        self.a.i64c(u64::MAX);
                        self.a.op(I64_XOR);
                    }
                    3 => {
                        self.a.set(B);
                        self.a.i64c(0);
                        self.a.get(B);
                        self.a.op(I64_SUB);
                    }
                    _ => {}
                }
                self.cond(op.d as u32);
                self.a.op(SELECT);
                self.mask(sf);
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::Madd { sf, sub } => {
                self.x(op.d);
                self.x(op.b);
                self.x(op.c);
                self.a.op(I64_MUL);
                self.a.op(if sub != 0 { I64_SUB } else { I64_ADD });
                self.mask(sf);
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::Shiftv { sf } => {
                let ty = op.d as u32;
                self.x(op.b);
                self.mask(sf);
                self.x(op.c);
                self.a.i64c((w(sf) - 1) as u64);
                self.a.op(I64_AND);
                if sf != 0 {
                    self.a.op([I64_SHL, I64_SHR_U, I64_SHR_S, I64_ROTR][ty as usize]);
                } else if ty < 2 {
                    self.a.op([I64_SHL, I64_SHR_U][ty as usize]);
                    self.mask(sf);
                } else {
                    self.a.op(I32_WRAP_I64);
                    self.a.set(N32);
                    self.a.op(I32_WRAP_I64);
                    self.a.get(N32);
                    self.a.op(if ty == 2 { I32_SHR_S } else { I32_ROTR });
                    self.a.op(I64_EXTEND_I32_U);
                }
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::Ld { size, ext, mode } => {
                self.mem_start();
                self.address(op, mode);
                self.load(k, 0, 1 << size);
                self.a.set(V);
                self.writeback(op, mode);
                self.a.get(V);
                if ext != 0 {
                    self.sext(size);
                    if ext == 2 {
                        self.mask(0);
                    }
                }
                self.a.set(V);
                self.set_x(op.a, V);
                self.check_device(k);
            }
            Form::St { size, mode } => {
                self.mem_start();
                self.address(op, mode);
                self.x(op.a);
                self.a.set(V);
                self.store(k, 0, 1 << size, V);
                self.writeback(op, mode);
                self.check_device(k);
            }
            Form::Ldp { size, signed, mode } => {
                self.mem_start();
                self.address(op, mode);
                self.load(k, 0, 1 << size);
                self.a.set(V);
                self.load(k, 1 << size, 1 << size);
                self.a.set(B);
                if signed != 0 {
                    for l in [V, B] {
                        self.a.get(l);
                        self.a.op(I64_EXTEND32_S);
                        self.a.set(l);
                    }
                }
                self.writeback(op, mode);
                self.set_x(op.a, V);
                self.set_x(op.c, B);
                self.check_device(k);
            }
            Form::Stp { size, mode } => {
                self.mem_start();
                self.address(op, mode);
                self.x(op.a);
                self.a.set(V);
                self.x(op.c);
                self.a.set(B);
                self.store(k, 0, 1 << size, V);
                self.store(k, 1 << size, 1 << size, B);
                self.writeback(op, mode);
                self.check_device(k);
            }
            Form::Barrier { isb } => {
                if isb != 0 {
                    // Changed code is seen after an ISB: return
                    self.ret(|g| g.pc_k(k + 1), k + 1, STATUS_OK);
                    return false;
                }
            }
            Form::Mrs { reg } => {
                self.uses_el = true;
                if self.t.el == 0 && !matches!(reg, MRS_TPIDR_EL0 | MRS_TPIDRRO_EL0) {
                    // EL0 access rules: left to the interpreter
                    self.punt(k);
                    return false;
                }
                match reg {
                    MRS_DAIF => {
                        self.a.load32(OFF_DAIF);
                        self.a.op(I64_EXTEND_I32_U);
                    }
                    MRS_SP_EL0 => self.a.load64(OFF_SP_EL0),
                    MRS_TPIDR_EL1 => self.a.load64(OFF_TPIDR_EL1),
                    MRS_TPIDR_EL0 => self.a.load64(OFF_TPIDR_EL0),
                    _ => self.a.load64(OFF_TPIDRRO_EL0),
                }
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::DcZva => {
                self.uses_el = true;
                if self.t.el == 0 {
                    self.punt(k);
                    return false;
                }
                self.x(op.a);
                self.a.i64c(!63);
                self.a.op(I64_AND);
                self.a.set(T);
                // Anything but RAM in the TLB: left to the interpreter
                self.tlb_hit(0, 64, true);
                self.punt_unless(k);
                for j in 0..8 {
                    self.ram_ptr();
                    self.a.i64c(0);
                    self.a.mem(I64_STORE, 0, self.t.ram_addr + 8 * j);
                }
            }
            Form::MaddL { unsigned, sub } => {
                self.x(op.d);
                for r in [op.b, op.c] {
                    self.x(r);
                    if unsigned != 0 {
                        self.mask(0);
                    } else {
                        self.a.op(I64_EXTEND32_S);
                    }
                }
                self.a.op(I64_MUL);
                self.a.op(if sub != 0 { I64_SUB } else { I64_ADD });
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::Ccmp { sf, sub, imm } => {
                self.cond(op.d as u32);
                self.if_();
                self.x(op.b);
                self.a.set(A);
                if imm != 0 {
                    self.a.i64c(op.c as u64);
                } else {
                    self.x(op.c);
                }
                self.a.set(B);
                self.addsub(sf, sub);
                self.flags_addsub(sf, sub);
                self.a.op(ELSE);
                self.a.get(CPU);
                self.a.i32c(((op.imm as u32) << 28) as i32);
                self.a.mem(I32_STORE, 2, OFF_NZCV);
                self.end();
            }
            Form::Rev { sf, opc } => {
                self.x(op.b);
                self.mask(sf);
                self.a.set(R);
                self.swap(8, 0x00ff_00ff_00ff_00ff);
                if opc >= 2 {
                    self.swap(16, 0x0000_ffff_0000_ffff);
                }
                if opc == 3 {
                    self.a.get(R);
                    self.a.i64c(32);
                    self.a.op(I64_ROTR);
                    self.a.set(R);
                }
                self.set_x(op.a, R);
            }
            Form::Clz { sf } => {
                self.x(op.b);
                if sf != 0 {
                    self.a.op(I64_CLZ);
                } else {
                    self.a.op(I32_WRAP_I64);
                    self.a.op(I32_CLZ);
                    self.a.op(I64_EXTEND_I32_U);
                }
                self.a.set(R);
                self.set_x(op.a, R);
            }
            Form::LdQ { mode, pair } | Form::StQ { mode, pair } => {
                let load = matches!(form, Form::LdQ { .. });
                self.fp_check(k);
                self.address(op, mode);
                // Anything but RAM in the TLB: left to the interpreter
                self.tlb_hit(0, 16 << pair, !load);
                self.punt_unless(k);
                let regs: &[u8] = if pair != 0 { &[op.a, op.c] } else { &[op.a] };
                if load {
                    let tmp = [V, B, MV, R];
                    for (j, &l) in tmp[..2 * regs.len()].iter().enumerate() {
                        self.ram_ptr();
                        self.a.mem(I64_LOAD, 0, self.t.ram_addr + 8 * j as u32);
                        self.a.set(l);
                    }
                    self.writeback(op, mode);
                    for (n, &r) in regs.iter().enumerate() {
                        for h in 0..2 {
                            self.a.store64_local(OFF_V + 16 * r as u32 + 8 * h, tmp[2 * n + h as usize]);
                        }
                    }
                } else {
                    for (n, &r) in regs.iter().enumerate() {
                        for h in 0..2 {
                            self.ram_ptr();
                            self.a.load64(OFF_V + 16 * r as u32 + 8 * h);
                            self.a.mem(I64_STORE, 0, self.t.ram_addr + 16 * n as u32 + 8 * h);
                        }
                    }
                    self.writeback(op, mode);
                }
            }
            Form::LdAcq { size } | Form::LdEx { size } => {
                self.mem_start();
                self.xs(op.b);
                self.a.set(T);
                self.align_check(k, 1 << size);
                self.load(k, 0, 1 << size);
                self.a.set(V);
                self.set_x(op.a, V);
                if matches!(form, Form::LdEx { .. }) {
                    self.a.store64_local(OFF_EXCL_ADDR, T);
                    self.a.get(CPU);
                    self.a.i32c(1 << size);
                    self.a.mem(I32_STORE, 2, OFF_EXCL_SIZE);
                }
                self.check_device(k);
            }
            Form::StRel { size } => {
                self.mem_start();
                self.xs(op.b);
                self.a.set(T);
                self.align_check(k, 1 << size);
                self.x(op.a);
                self.a.set(V);
                self.store(k, 0, 1 << size, V);
                self.check_device(k);
            }
            Form::StEx { size } => {
                self.mem_start();
                self.xs(op.b);
                self.a.set(T);
                self.align_check(k, 1 << size);
                // N32 = the monitor is set for this access; it's cleared either way
                self.a.load32(OFF_EXCL_SIZE);
                self.a.i32c(1 << size);
                self.a.op(I32_EQ);
                self.a.load64(OFF_EXCL_ADDR);
                self.a.get(T);
                self.a.op(I64_EQ);
                self.a.op(I32_AND);
                self.a.set(N32);
                self.a.get(CPU);
                self.a.i32c(0);
                self.a.mem(I32_STORE, 2, OFF_EXCL_SIZE);
                self.a.get(N32);
                self.if_();
                self.x(op.a);
                self.a.set(V);
                self.store(k, 0, 1 << size, V);
                self.end();
                self.a.get(N32);
                self.a.op(I32_EQZ);
                self.a.op(I64_EXTEND_I32_U);
                self.a.set(R);
                self.set_x(op.c, R);
                self.check_device(k);
            }
        }
        true
    }
}

/// A WebAssembly module exporting "f": (cpu: i32, end: i64) -> i32, which
/// runs the region from cpu.pc. It imports "e"."m" (the memory), "e"."r"
/// (jit_read), "e"."w" (jit_write) and "e"."t" (the function table, to call
/// other regions).
///
/// The body is a dispatch loop over the blocks:
///
/// ```text
/// loop                              ;; br_top
///   block                           ;; br_exit
///     block (one per region block)
///       br_table SLOT               ;; to a block's code, else the exit
///     end  code of block 0  (falls into block 1, if it's next)
///     end  code of block 1
///     ...
///   end  return at pc = page + 4 * SLOT
/// end
/// ```
pub fn codegen(blocks: &[RBlock], t: &Target) -> Vec<u8> {
    let nblocks = blocks.len();
    let mut g = Gen {
        a: Asm::default(),
        t,
        sp_off: if t.sp_el1 { OFF_SP_EL1 } else { OFF_SP_EL0 },
        uses_sp: false,
        uses_el: false,
        nblocks,
        bi: 0,
        slot0: 0,
        nest: 0,
    };
    let mut codes = Vec::new();
    for (bi, b) in blocks.iter().enumerate() {
        g.bi = bi;
        g.slot0 = b.slot;
        g.nest = 0;
        let len = b.ops.len();
        // Stop before the block if it would go past ICOUNT_END
        g.a.get(ICOUNT);
        g.a.i64c(len as u64);
        g.a.op(I64_ADD);
        g.a.get(ICOUNT_END);
        g.a.op(I64_GT_U);
        g.if_();
        g.a.i32c(b.slot as i32);
        g.a.set(SLOT);
        g.br_exit();
        g.end();
        let mut open = true;
        for (k, (form, op)) in b.ops.iter().enumerate() {
            if !g.insn(k, *form, op) {
                open = false;
                break;
            }
        }
        if open {
            if blocks.get(bi + 1).is_some_and(|n| n.slot == b.slot + len) {
                // Fall into the next block
                g.add_icount(len);
            } else {
                g.goto(len - 1, 4, len);
            }
        }
        debug_assert_eq!(g.nest, 0);
        codes.push(std::mem::take(&mut g.a.b));
    }

    let mut f = Asm::default();
    // Entry checks
    if g.uses_sp {
        // (el != 0 && spsel != 0) == sp_el1
        f.load32(OFF_EL);
        f.op(I32_EQZ);
        f.load32(OFF_SPSEL);
        f.op(I32_EQZ);
        f.op(I32_OR);
        f.i32c(t.sp_el1 as i32);
        f.op(I32_EQ);
        f.if_();
        f.i32c(STATUS_WRONG_MODE as i32);
        f.op(RETURN);
        f.op(END);
    }
    if g.uses_el {
        // Permission checks are for this EL
        f.load32(OFF_EL);
        f.i32c(t.el as i32);
        f.op(I32_NE);
        f.if_();
        f.i32c(STATUS_WRONG_MODE as i32);
        f.op(RETURN);
        f.op(END);
    }
    f.load64(OFF_PC);
    f.i64c(!0xfff);
    f.op(I64_AND);
    f.set(PAGE);
    f.load64(OFF_ICOUNT);
    f.set(ICOUNT);
    f.load64(OFF_PC);
    f.op(I32_WRAP_I64);
    f.i32c(0xfff);
    f.op(I32_AND);
    f.i32c(2);
    f.op(I32_SHR_U);
    f.set(SLOT);
    f.op(LOOP);
    f.op(BLOCK_EMPTY);
    f.op(BLOCK);
    f.op(BLOCK_EMPTY);
    for _ in 0..nblocks {
        f.op(BLOCK);
        f.op(BLOCK_EMPTY);
    }
    let mut table = vec![nblocks; blocks.last().map_or(0, |b| b.slot + 1)];
    for (bi, b) in blocks.iter().enumerate() {
        table[b.slot] = bi;
    }
    f.get(SLOT);
    f.op(BR_TABLE);
    f.uleb(table.len() as u64);
    for d in table {
        f.uleb(d as u64);
    }
    f.uleb(nblocks as u64);
    for code in codes {
        f.op(END);
        f.b.extend_from_slice(&code);
    }
    f.op(END);
    // Exit: pc = page + 4 * SLOT
    g.bi = nblocks - 1;
    g.nest = 0;
    g.ret(
        |g| {
            g.a.get(PAGE);
            g.a.get(SLOT);
            g.a.op(I64_EXTEND_I32_U);
            g.a.i64c(2);
            g.a.op(I64_SHL);
            g.a.op(I64_ADD);
        },
        0,
        STATUS_OK,
    );
    f.b.extend_from_slice(&g.a.b);
    f.op(END);
    f.op(UNREACHABLE);
    f.op(END);

    // Function body: locals, code
    let mut func = Asm::default();
    func.uleb(2);
    func.uleb(NUM_I32 as u64);
    func.op(0x7f);
    func.uleb(NUM_I64 as u64);
    func.op(0x7e);
    func.b.extend_from_slice(&f.b);

    let mut m = Asm::default();
    m.b.extend_from_slice(b"\0asm\x01\0\0\0");
    let section = |m: &mut Asm, id: u8, content: &[u8]| {
        m.op(id);
        m.uleb(content.len() as u64);
        m.b.extend_from_slice(content);
    };
    // Types: 0 = region (i32, i64) -> i32, 1 = read (i32, i64, i32) -> i64,
    // 2 = write (i32, i64, i32, i64) -> ()
    section(
        &mut m,
        1,
        &[3, 0x60, 2, 0x7f, 0x7e, 1, 0x7f, 0x60, 3, 0x7f, 0x7e, 0x7f, 1, 0x7e, 0x60, 4, 0x7f, 0x7e, 0x7f, 0x7e, 0],
    );
    // Imports: memory, read, write, the function table
    section(
        &mut m,
        2,
        &[4, 1, b'e', 1, b'm', 2, 0, 0, 1, b'e', 1, b'r', 0, 1, 1, b'e', 1, b'w', 0, 2, 1, b'e', 1, b't', 1, 0x70, 0, 0],
    );
    // Functions: one of type 0
    section(&mut m, 3, &[1, 0]);
    // Exports: "f" = function 2
    section(&mut m, 7, &[1, 1, b'f', 0, 2]);
    // Code
    let mut code = Asm::default();
    code.uleb(1);
    code.uleb(func.b.len() as u64);
    code.b.extend_from_slice(&func.b);
    section(&mut m, 10, &code.b);
    m.b
}

/// Self-test for the browser build (run by tools/a64-ref/jit-test.mjs):
/// random code (mostly branching within itself) run compiled and
/// interpreted from the same random state for the same number of steps must
/// end in the same state. `compile` must install synchronously. Returns
/// a description of the first difference, or None.
pub fn selftest(seed: u64, iterations: u32, table_base: u32, compile: CompileFn) -> Option<String> {
    use crate::bus::Bus;
    const RAM_BASE: u64 = 0x4000_0000;
    const RAM_SIZE: usize = 0x10000;
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let (mut skipped, mut not_run) = (0, 0);
    for iter in 0..iterations {
        // Two runs of compilable instructions, mostly not unconditional
        // branches: one in page 0 and one in page 1, branching within
        // themselves and to each other
        let mut br_regs = 0u32;
        let gen = |len: usize, br_regs: &mut u32, next: &mut dyn FnMut() -> u64| {
            let mut words = Vec::new();
            while words.len() < len {
                let mut w = next() as u32;
                if w.is_multiple_of(8) {
                    // Forms random words are (almost) never
                    let r = next() as u32;
                    let (rt, rn, rs) = (r & 31, (r >> 5) % 31, (r >> 10) & 31);
                    let size = (r >> 30) << 30;
                    w = match next() % 13 {
                        0 => [0xd61f_0000, 0xd63f_0000, 0xd65f_0000][(next() % 3) as usize] | rn << 5,
                        1 => [0xd538_d080, 0xd53b_d040, 0xd53b_d060, 0xd538_4100, 0xd53b_4220][(next() % 5) as usize] | rt,
                        2 => 0xd50b_7420 | rt,
                        3 => [0xd503_3bbf, 0xd503_39bf, 0xd503_3f9f, 0xd503_3fdf][(next() % 4) as usize],
                        4 => [0xdac0_0400, 0xdac0_0800, 0xdac0_0c00, 0x5ac0_0400, 0x5ac0_0800, 0xdac0_1000, 0x5ac0_1000]
                            [(next() % 7) as usize]
                            | rn << 5
                            | rt,
                        5 | 6 => [0x08df_fc00, 0x089f_fc00][(next() % 2) as usize] | size | rn << 5 | rt,
                        7 | 8 => 0x085f_7c00 | (next() as u32 & 1) << 15 | size | rn << 5 | rt,
                        9 | 10 => 0x0800_7c00 | (next() as u32 & 1) << 15 | size | rs << 16 | rn << 5 | rt,
                        11 => 0xf980_0000 | (r & 0x3f_ffff),
                        _ => {
                            // Q register loads and stores with small offsets
                            let (base, lsb) = [(0x3d80_0000, 10), (0x3dc0_0000, 10), (0xad00_0000, 15), (0xad40_0000, 15), (0x3c80_0c00, 12), (0x3cc0_0400, 12)]
                                [(next() % 6) as usize];
                            let rt2 = if base >> 24 == 0xad { rs << 10 } else { 0 };
                            base | (r >> 12 & 7) << lsb | rt2 | rn << 5 | rt
                        }
                    };
                }
                if let (Some(form), op) = decode_op(w) {
                    if matches!(form, Form::B | Form::Bl | Form::Br | Form::Blr) && !next().is_multiple_of(8) {
                        continue;
                    }
                    if matches!(form, Form::Br | Form::Blr) {
                        *br_regs |= 1 << op.b;
                    }
                    words.push(w);
                }
            }
            words
        };
        let len = 1 + (next() % 24) as usize;
        let len1 = 1 + (next() % 24) as usize;
        let mut words = gen(len, &mut br_regs, &mut next);
        let mut words1 = gen(len1, &mut br_regs, &mut next);
        let start = (next() % (1024 - len as u64)) as usize * 4;
        let start1 = 0x1000 + (next() % (1024 - len1 as u64)) as usize * 4;
        // Branch targets: the instructions, or just past either run
        let targets: Vec<usize> = (0..=len).map(|k| start + 4 * k).chain((0..=len1).map(|k| start1 + 4 * k)).collect();
        // Most direct branches go to the code
        for (w, at) in words.iter_mut().zip((start..).step_by(4)).chain(words1.iter_mut().zip((start1..).step_by(4))) {
            let (Some(form), _) = decode_op(*w) else { continue };
            let (bits, lsb) = match form {
                Form::B | Form::Bl => (26, 0),
                Form::BCond | Form::Cbz { .. } => (19, 5),
                Form::Tbz { .. } => (14, 5),
                _ => continue,
            };
            if next() % 4 != 0 {
                let off = (targets[(next() % targets.len() as u64) as usize] as i64 - at as i64) / 4;
                let mask = ((1u32 << bits) - 1) << lsb;
                *w = (*w & !mask) | (((off as u32) << lsb) & mask);
            }
        }
        let limit = 1 + next() % 2000;
        let mmu = next() % 2 == 0;
        let state_seed = next();
        let mk = || {
            let mut r = state_seed | 1;
            let mut rn = move || {
                r ^= r >> 12;
                r ^= r << 25;
                r ^= r >> 27;
                r.wrapping_mul(0x2545_f491_4f6c_dd1d)
            };
            let mut c = Box::new(Cpu::new(Bus::new(RAM_BASE, RAM_SIZE)));
            for i in 0..31 {
                c.x[i] = match if br_regs & 1 << i != 0 { 2 } else { rn() % 5 } {
                    0 => rn(),
                    1 => rn() % 64,
                    2 => RAM_BASE + targets[(rn() % targets.len() as u64) as usize] as u64,
                    _ => RAM_BASE + 0x8000 + rn() % 0x4000,
                };
            }
            c.sp_el1 = RAM_BASE + 0xc000 + (rn() % 0x100) * 8;
            c.sp_el0 = RAM_BASE + 0xd000 + (rn() % 0x100) * 8;
            c.spsel = (rn() % 2) as u32;
            c.nzcv = (rn() as u32) & 0xf000_0000;
            c.daif = (rn() as u32) & 0x3c0;
            c.sr.tpidr_el1 = rn();
            c.sr.tpidr_el0 = rn();
            c.sr.tpidrro_el0 = rn();
            c.sr.cpacr_el1 = (rn() % 4) << 20;
            for v in c.v.iter_mut() {
                *v = (rn() as u128) << 64 | rn() as u128;
            }
            if rn() % 2 == 0 {
                // A monitor that may match a following store-exclusive
                c.excl_addr = c.x[(rn() % 31) as usize];
                c.excl_size = 1 << (rn() % 4);
            }
            for b in c.bus.ram[0x2000..].iter_mut() {
                *b = rn() as u8;
            }
            for (k, w) in words.iter().enumerate() {
                c.bus.ram[start + 4 * k..start + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
            }
            for (k, w) in words1.iter().enumerate() {
                c.bus.ram[start1 + 4 * k..start1 + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
            }
            c.pc = RAM_BASE + start as u64;
            c.ns_per_insn = 1;
            if mmu {
                // RAM mapped 1:1 with random permissions (code: everything)
                use crate::mmu::*;
                c.sr.sctlr_el1 |= 1;
                c.el = (rn() % 2) as u32;
                c.tlb.asid = (rn() % 4) as u16;
                for p in 0..(RAM_SIZE >> 12) {
                    let va = RAM_BASE + (p << 12) as u64;
                    let perm = if p < 2 {
                        P_R1 | P_W1 | P_X1 | P_R0 | P_W0 | P_X0
                    } else {
                        [P_R1 | P_W1 | P_R0 | P_W0, P_R1 | P_R0, P_R1 | P_W1, P_R1 | P_W1 | P_R0, 0][(rn() % 5) as usize]
                    };
                    c.tlb.insert(va, va, p < 2 || rn() % 2 == 0, perm, Some(p << 12));
                }
                c.tlb.asid = (rn() % 4) as u16;
            }
            c
        };
        let mut a = mk();
        let mut b = mk();
        a.jit.enabled = true;
        a.jit.threshold = 0;
        a.jit.compile = Some(compile);
        a.jit.table_base = table_base;
        // Compile both
        a.pc = RAM_BASE + start1 as u64;
        a.jit_enter(limit);
        a.pc = RAM_BASE + start as u64;
        a.jit_enter(limit);
        if a.jit.stats.failed != 0 {
            return Some(format!("iteration {iter}: code at {:#x} failed to compile ({:08x?})", a.pc, words));
        }
        // Sometimes change an instruction of the compiled code: it must not run
        let rewrite = next() % 4 == 0;
        if rewrite {
            let at = targets[(next() % targets.len() as u64) as usize];
            let w = loop {
                let w = next() as u32;
                if decode_op(w).0.is_some() {
                    break w;
                }
            };
            a.bus.write_ram(at, 4, w as u64);
            b.bus.write_ram(at, 4, w as u64);
        }
        let a_code = a.bus.ram[..0x2000].to_vec();
        let mut run = a.jit_enter(limit);
        if rewrite && run.is_none() && a.icount == 0 {
            // The entry's page was compiled again
            run = a.jit_enter(limit);
        }
        let Some(used) = run else {
            // Nothing run: the first block doesn't fit in the limit, or its
            // first instruction is left to the interpreter
            not_run += 1;
            if a.icount != 0 || a.pc != b.pc || a.x != b.x {
                return Some(format!("iteration {iter}: state changed without running ({:08x?} {:08x?})", words, words1));
            }
            continue;
        };
        if used > limit + 1 {
            return Some(format!("iteration {iter}: {used} steps for a limit of {limit}"));
        }
        for _ in 0..used {
            b.step();
        }
        // Every write to a page with compiled code must be noticed
        for p in 0..2 {
            let r = p << 12..(p + 1) << 12;
            if a.bus.ram[r.clone()] != a_code[r] && a.bus.code_pages[p] != 0 {
                return Some(format!("iteration {iter}: write to code page {p} not noticed ({:08x?} {:08x?})", words, words1));
            }
        }
        // Code that changes itself is only seen by compiled code after the
        // next ISB (or exception): skip
        if b.bus.ram[..0x2000] != a_code[..] {
            skipped += 1;
            continue;
        }
        let state = |c: &Cpu| {
            (c.x, c.sp_el0, c.sp_el1, c.pc, c.nzcv, c.icount, c.sr.esr_el1, c.sr.far_el1, c.sr.elr_el1, c.excl_size, c.excl_addr)
        };
        if state(&a) != state(&b) || a.v != b.v || a.bus.ram != b.bus.ram {
            let mut d = format!(
                "iteration {iter}: state differs after code {:08x?} at {:#x} and {:08x?} at {:#x}\n",
                words,
                RAM_BASE + start as u64,
                words1,
                RAM_BASE + start1 as u64
            );
            for i in 0..31 {
                if a.x[i] != b.x[i] {
                    d += &format!("  x{i}: jit {:#x} interp {:#x}\n", a.x[i], b.x[i]);
                }
            }
            d += &format!(
                "  sp0 {:#x}/{:#x} sp1 {:#x}/{:#x} pc {:#x}/{:#x} nzcv {:#x}/{:#x} icount {}/{} esr {:#x}/{:#x} far {:#x}/{:#x} ram {}\n",
                a.sp_el0,
                b.sp_el0,
                a.sp_el1,
                b.sp_el1,
                a.pc,
                b.pc,
                a.nzcv,
                b.nzcv,
                a.icount,
                b.icount,
                a.sr.esr_el1,
                b.sr.esr_el1,
                a.sr.far_el1,
                b.sr.far_el1,
                if a.bus.ram == b.bus.ram { "same" } else { "differs" }
            );
            d += &format!("  excl {:#x}/{} {:#x}/{} v {}\n", a.excl_addr, a.excl_size, b.excl_addr, b.excl_size, if a.v == b.v { "same" } else { "differs" });
            return Some(d);
        }
    }
    if skipped + not_run > iterations / 4 {
        return Some(format!("{skipped} of {iterations} iterations skipped, {not_run} not run compiled"));
    }
    None
}
