//! JIT: compiles hot code to WebAssembly (browser build), a page at a time.
//! The design follows the aarch64 core's (src/a64/src/jit.rs).
//!
//! The run loop counts entries to block starts (branch targets, and where
//! execution resumes after a trap or an instruction that isn't compiled) per
//! physical RAM page (JitPage). When one gets hot, the code of its page is
//! compiled into one function (a region): its blocks are the hot block starts
//! of the page and everything reachable from them within the page, split into
//! basic blocks. The function dispatches on pc in a loop, so branches within
//! the page, including indirect ones (returns), stay in compiled code. A
//! branch to another page calls that page's compiled code, if the target is
//! installed there (up to MAX_HOPS nested calls). A block start that gets hot
//! later recompiles the page.
//!
//! Instructions are 2 bytes aligned (C extension): a page has 2048 slots.
//! Compressed instructions are compiled as their 32-bit expansions; one that
//! crosses into the next page isn't compiled.
//!
//! The generated function is handed to the host (JS), which compiles it,
//! puts it into the function table and reports back through jit_done. The
//! run loop then calls it at installed block starts instead of interpreting.
//!
//! Pages with compiled code are flagged in Bus::code_pages. Every write to
//! RAM checks the flag (Bus::write_ram, compiled stores, and RV64CPU's JS
//! write functions for DMA); a write to a flagged page drops its compiled
//! code before compiled code runs again (jit_enter). Code that changes while
//! it runs is seen after it returns (FENCE.I isn't compiled).
//!
//! Only S and U mode code is compiled, each region for one of them. Compiled
//! code
//! - checks on entry that it runs in the privilege mode it was compiled for,
//!   and reads mstatus.SUM for its permission checks (only CSR instructions,
//!   which aren't compiled, change it);
//! - works on the Cpu struct in linear memory directly (it gets its address),
//!   with pc and icount in locals;
//! - stops before a block that would take icount past its `end` argument;
//! - does loads and stores of RAM pages in the TLB inline, and others
//!   through jit_read/jit_write, which set jit_flag to 1 on a fault (it
//!   returns at that instruction, which takes the trap) and to 2 for
//!   anything but plain RAM (it returns after the instruction, so the run
//!   loop sees interrupts raised by devices);
//! - returns one of the STATUS_* values, with pc and icount updated as if
//!   the instructions had been interpreted.

use std::collections::HashMap;
use std::mem::offset_of;

use crate::cpu::*;
use crate::mmu::{layout as l, Acc, P_R_S, P_R_SUM, P_R_U, P_W_S, P_W_SUM, P_W_U, P_X_S, P_X_U, TLB_ENTRIES};
use crate::rvc;

/// Block entries before a block start is compiled.
pub const DEFAULT_THRESHOLD: u32 = 100;
/// Most compiled regions at a time (function table entries).
const MAX_FUNCS: u32 = 1 << 16;
/// Most calls from compiled code to another page's compiled code before it
/// returns to the run loop (each one nests a wasm call).
const MAX_HOPS: u32 = 64;
pub const PAGE_SLOTS: usize = 2048;

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
/// Compiled for the other privilege mode: interpret this time.
pub const STATUS_WRONG_MODE: u32 = 3;

/// Hands generated code to the host: (module bytes, function table index,
/// cookie). Returns 1 if the function is installed already, 0 if the host
/// will call Cpu::jit_done(cookie, ..) later, 2 if it failed.
pub type CompileFn = fn(&[u8], u32, u32) -> u32;

/// JIT state of a RAM page.
pub struct JitPage {
    /// Per slot: entry counter, NEVER, or INSTALLED | region function index.
    pub state: [u32; PAGE_SLOTS],
    /// Function index of the installed region.
    region: Option<u32>,
    /// Cookie of the region being compiled.
    pending: Option<u32>,
}

impl JitPage {
    fn new() -> Box<JitPage> { Box::new(JitPage { state: [0; PAGE_SLOTS], region: None, pending: None }) }

    /// Uninstall the region (its block starts count again from 0).
    fn drop_region(&mut self, free: &mut Vec<u32>) {
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
    fn install(&mut self, index: u32, slots: &[usize], free: &mut Vec<u32>) {
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
    /// Loads and stores of compiled code that took the slow path
    pub slow: u64,
}

pub struct Jit {
    pub enabled: bool,
    pub threshold: u32,
    pub compile: Option<CompileFn>,
    /// First function table index that's ours.
    pub table_base: u32,
    next_func: u32,
    free: Vec<u32>,
    pending: HashMap<u32, Pending>,
    next_cookie: u32,
    /// Per RAM page, allocated when its first block start is counted.
    pages: Vec<Option<Box<JitPage>>>,
    /// Per RAM page: address of its JitPage::state, or 0. Compiled code
    /// looks branch targets up in it.
    ptrs: Box<[usize]>,
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
            free: Vec::new(),
            pending: HashMap::new(),
            next_cookie: 0,
            pages: Vec::new(),
            ptrs: Box::new([]),
            stats: JitStats::default(),
        }
    }
}

impl Jit {
    /// JIT state of RAM page `page`, allocated if needed.
    fn page(&mut self, page: usize) -> &mut JitPage {
        if self.pages.len() <= page {
            self.pages.resize_with(page + 1, || None);
        }
        let jp = self.pages[page].get_or_insert_with(JitPage::new);
        if let Some(p) = self.ptrs.get_mut(page) {
            *p = jp.state.as_ptr() as usize;
        }
        jp
    }

    fn meta(&mut self, page: usize) -> Option<&mut JitPage> { self.pages.get_mut(page)?.as_deref_mut() }

    /// Address of the table of JitPage::state addresses, for `ram_pages` RAM pages.
    fn ptrs(&mut self, ram_pages: usize) -> usize {
        if self.ptrs.is_empty() {
            self.ptrs = vec![0; ram_pages].into_boxed_slice();
            for (p, jp) in self.pages.iter().enumerate() {
                if let (Some(jp), Some(e)) = (jp, self.ptrs.get_mut(p)) {
                    *e = jp.state.as_ptr() as usize;
                }
            }
        }
        self.ptrs.as_ptr() as usize
    }

    fn install(&mut self, page: usize, index: u32, slots: &[usize]) {
        match self.pages.get_mut(page).and_then(|j| j.as_mut()) {
            Some(jp) => jp.install(index, slots, &mut self.free),
            None => self.free.push(index),
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
        if self.prv == PRV_M || (self.auto_irq && self.pending_interrupt().is_some()) {
            return None;
        }
        let pc = self.pc;
        let page = if pc >> 12 == self.tlb.fetch_vpn && self.prv == self.tlb.fetch_prv {
            self.tlb.fetch_page as usize
        } else {
            self.jit_fetch_page(pc)?
        };
        let slot = ((pc & 0xfff) >> 1) as usize;
        let j = match self.jit.pages.get(page) {
            Some(Some(jp)) => jp.state[slot],
            _ => 0,
        };
        if !is_installed(j) {
            self.jit_count(page, slot, j);
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
                self.jit.stats.insns += used;
                // Like step(): the instruction counts, then traps
                self.icount += 1;
                let e = self.jit_exc;
                let pc = self.pc;
                self.insn_pc = pc;
                self.trap(e.cause, e.tval, false, pc);
                self.block_start = true;
                Some(used + 1)
            }
            // WRONG_MODE, possibly after chaining to code compiled for another mode
            _ => {
                self.block_start = true;
                (used != 0).then_some(used)
            }
        }
    }

    /// The RAM page of pc for instruction fetch (fills the fetch cache).
    #[inline(never)]
    fn jit_fetch_page(&mut self, pc: u64) -> Option<usize> {
        let pa = self.translate(pc, Acc::Exec).ok()?;
        let off = self.bus.ram_offset(pa, 2)?;
        self.tlb.fetch_vpn = pc >> 12;
        self.tlb.fetch_prv = self.prv;
        self.tlb.fetch_page = (off >> 12) as u32;
        Some(off >> 12)
    }

    /// Drop the compiled code of pages that were written (Bus::code_pages).
    #[inline(never)]
    fn jit_drop_written(&mut self) {
        for p in std::mem::take(&mut self.bus.code_written) {
            let jit = &mut self.jit;
            if let Some(Some(jp)) = jit.pages.get_mut(p as usize) {
                // A region being compiled from the old code is dropped when done
                jp.pending = None;
                if jp.region.is_some() {
                    jit.stats.stale += 1;
                }
                jp.drop_region(&mut jit.free);
            }
        }
    }

    /// Allocate Bus::code_pages and the table of JIT states; returns their
    /// addresses.
    pub fn jit_tables(&mut self) -> (usize, usize) {
        let pages = self.bus.ram.len() >> 12;
        if self.bus.code_pages.is_empty() {
            self.bus.code_pages = vec![0; pages];
        }
        (self.bus.code_pages.as_ptr() as usize, self.jit.ptrs(pages))
    }

    /// Count an entry to the block at `slot` of RAM page `page` (state `j`);
    /// compile the page once it's hot.
    #[inline(never)]
    fn jit_count(&mut self, page: usize, slot: usize, j: u32) {
        if j < self.jit.threshold {
            self.jit.page(page).state[slot] = j + 1;
        } else if j != NEVER {
            self.jit_compile(page, slot);
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

    /// Compile RAM page `page`, whose block start at `slot` got hot.
    fn jit_compile(&mut self, page: usize, slot: usize) {
        let threshold = self.jit.threshold;
        if self.jit.page(page).pending.is_some() {
            // Picked up when the page is compiled again
            return;
        }
        let set = |c: &mut Cpu, v: u32| c.jit.page(page).state[slot] = v;
        let Some(compile) = self.jit.compile else { return set(self, NEVER) };
        let mut code = [0u8; 0x1000];
        code.copy_from_slice(&self.bus.ram[page << 12..(page + 1) << 12]);
        if slot_insn(&code, slot).is_none() {
            return set(self, NEVER);
        }
        let (code_pages, ptrs) = self.jit_tables();
        // From here on, writes to the page are noticed
        self.bus.code_pages[page] = 1;
        // Entries: the installed block starts and those that are getting hot
        let jp = self.jit.page(page);
        let entries: Vec<usize> = (0..PAGE_SLOTS)
            .filter(|&s| {
                let j = jp.state[s];
                s == slot || is_installed(j) || (j != NEVER && j != 0 && j >= threshold / 2)
            })
            .collect();
        let blocks = build_region(&code, &entries);
        let Some(index) = self.jit.free.pop().or_else(|| {
            (self.jit.next_func < MAX_FUNCS).then(|| {
                self.jit.next_func += 1;
                self.jit.next_func - 1
            })
        }) else {
            return set(self, NEVER);
        };
        let target = Target {
            ram_addr: self.bus.ram.as_ptr() as u32,
            tlb_addr: self.tlb.entries_addr() as u32,
            prv: self.prv,
            ptrs_addr: ptrs as u32,
            code_pages_addr: code_pages as u32,
            table_base: self.jit.table_base,
        };
        let wasm = codegen(&blocks, &target);
        let slots: Vec<usize> = blocks.iter().map(|b| b.ops[0].slot).collect();
        let cookie = self.jit.next_cookie;
        self.jit.next_cookie = self.jit.next_cookie.wrapping_add(1);
        self.jit.stats.compiled += 1;
        match compile(&wasm, self.jit.table_base + index, cookie) {
            1 => self.jit.install(page, index, &slots),
            0 => {
                self.jit.page(page).pending = Some(cookie);
                self.jit.pending.insert(cookie, Pending { page, index, slots });
            }
            _ => {
                self.jit.stats.failed += 1;
                self.jit.free.push(index);
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
        let install = match self.jit.meta(p.page) {
            Some(jp) if jp.pending == Some(cookie) => {
                jp.pending = None;
                ok
            }
            _ => false,
        };
        if install {
            self.jit.install(p.page, p.index, &p.slots);
        } else {
            self.jit.free.push(p.index);
        }
    }

    /// Load for compiled code: see the module comment.
    pub fn jit_read(&mut self, va: u64, size: u32) -> u64 {
        self.jit.stats.slow += 1;
        if (va & 0xfff) + size as u64 <= 0x1000 {
            return match self.translate(va, Acc::Read) {
                Ok(pa) => match self.bus.ram_offset(pa, size as u64) {
                    Some(off) => self.bus.read_ram(off, size),
                    None => {
                        self.jit_flag |= 2;
                        match self.phys_read(pa, size, va, Acc::Read) {
                            Ok(v) => v,
                            Err(e) => self.jit_fault(e),
                        }
                    }
                },
                Err(e) => self.jit_fault(e),
            };
        }
        match self.load(va, size) {
            Ok(v) => {
                self.jit_flag |= 2;
                v
            }
            Err(e) => self.jit_fault(e),
        }
    }

    /// Store for compiled code: see the module comment.
    pub fn jit_write(&mut self, va: u64, size: u32, v: u64) {
        self.jit.stats.slow += 1;
        if (va & 0xfff) + size as u64 <= 0x1000 {
            match self.translate(va, Acc::Write) {
                Ok(pa) => match self.bus.ram_offset(pa, size as u64) {
                    Some(off) => self.bus.write_ram(off, size, v),
                    None => {
                        self.jit_flag |= 2;
                        if let Err(e) = self.phys_write(pa, size, v, va) {
                            self.jit_fault(e);
                        }
                    }
                },
                Err(e) => {
                    self.jit_fault(e);
                }
            }
            return;
        }
        match self.store(va, size, v) {
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

// ---- instructions ----

/// A compilable instruction at a slot: its 32-bit form (compressed ones
/// expanded) and length in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Insn {
    pub slot: usize,
    pub i: u32,
    pub len: u8,
}

/// The instruction at `slot` of a page, if it can be compiled.
pub fn slot_insn(code: &[u8; 0x1000], slot: usize) -> Option<Insn> {
    let b = 2 * slot;
    let lo = u16::from_le_bytes([code[b], code[b + 1]]) as u32;
    let (i, len) = if lo & 3 != 3 {
        (rvc::expand(lo)?, 2)
    } else if b + 4 <= 0x1000 {
        (lo | (u16::from_le_bytes([code[b + 2], code[b + 3]]) as u32) << 16, 4)
    } else {
        return None;
    };
    compilable(i).then_some(Insn { slot, i, len })
}

/// Can the (32-bit form of the) instruction be compiled? The same encodings
/// as Cpu::exec accepts, for the instructions compiled.
pub fn compilable(i: u32) -> bool {
    let f3 = i >> 12 & 7;
    let f7 = i >> 25;
    match i & 0x7f {
        0x37 | 0x17 | 0x6f => true,
        0x67 => f3 == 0,
        0x63 => f3 != 2 && f3 != 3,
        0x03 => f3 != 7,
        0x23 => f3 <= 3,
        0x13 => match f3 {
            1 => i >> 26 == 0,
            5 => i >> 26 == 0 || i >> 26 == 0x10,
            _ => true,
        },
        0x1b => matches!((f3, f7), (0, _) | (1, 0) | (5, 0) | (5, 0x20)),
        0x33 => matches!((f7, f3), (0, _) | (0x20, 0) | (0x20, 5) | (1, _)),
        0x3b => matches!((f7, f3), (0, 0) | (0x20, 0) | (0, 1) | (0, 5) | (0x20, 5) | (1, 0) | (1, 4..=7)),
        // FENCE (not FENCE.I)
        0x0f => f3 == 0,
        _ => false,
    }
}

fn is_branch(i: u32) -> bool { i & 0x7f == 0x63 }
fn is_jal(i: u32) -> bool { i & 0x7f == 0x6f }
fn is_jalr(i: u32) -> bool { i & 0x7f == 0x67 }
fn ends_block(i: u32) -> bool { is_branch(i) || is_jal(i) || is_jalr(i) }
fn rd(i: u32) -> u8 { (i >> 7 & 31) as u8 }
fn rs1(i: u32) -> u8 { (i >> 15 & 31) as u8 }
fn rs2(i: u32) -> u8 { (i >> 20 & 31) as u8 }
fn imm_i(i: u32) -> u64 { (i as i32 >> 20) as i64 as u64 }
fn imm_s(i: u32) -> u64 { (((i as i32 >> 25) << 5) as u32 | (i >> 7 & 31)) as i32 as i64 as u64 }
fn imm_u(i: u32) -> u64 { (i & 0xffff_f000) as i32 as i64 as u64 }

fn imm_b(i: u32) -> u64 {
    (((i as i32 >> 31) << 12) as u32 | (i >> 25 & 0x3f) << 5 | (i >> 8 & 0xf) << 1 | (i >> 7 & 1) << 11) as i32 as i64 as u64
}

fn imm_j(i: u32) -> u64 {
    (((i as i32 >> 31) << 20) as u32 | (i >> 21 & 0x3ff) << 1 | (i >> 20 & 1) << 11 | (i >> 12 & 0xff) << 12) as i32 as i64
        as u64
}

/// The slot a direct branch or jump goes to, if it's in the page.
fn branch_slot(ins: &Insn) -> Option<usize> {
    let off = if is_branch(ins.i) {
        imm_b(ins.i)
    } else if is_jal(ins.i) {
        imm_j(ins.i)
    } else {
        return None;
    };
    let t = (2 * ins.slot as u64).wrapping_add(off);
    (t < 0x1000).then_some(t as usize / 2)
}

// ---- regions ----

/// A basic block of a region: instructions from its first slot up to a
/// branch (the last instruction), or to where another block starts, an
/// instruction that can't be compiled or the end of the page.
pub struct RBlock {
    pub ops: Vec<Insn>,
}

/// The basic blocks reachable within the page (`code`) from `entries`.
pub fn build_region(code: &[u8; 0x1000], entries: &[usize]) -> Vec<RBlock> {
    let mut leader = vec![false; PAGE_SLOTS];
    let mut work: Vec<usize> = Vec::new();
    let add = |s: usize, leader: &mut [bool], work: &mut Vec<usize>| {
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
            let Some(ins) = slot_insn(code, k) else { break };
            let next = k + ins.len as usize / 2;
            if ends_block(ins.i) {
                if let Some(t) = branch_slot(&ins) {
                    add(t, &mut leader, &mut work);
                }
                // Not-taken path, or where a call returns to
                if is_branch(ins.i) || rd(ins.i) != 0 {
                    add(next, &mut leader, &mut work);
                }
                break;
            }
            k = next;
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
            let Some(ins) = slot_insn(code, k) else { break };
            ops.push(ins);
            k += ins.len as usize / 2;
            if ends_block(ins.i) {
                break;
            }
        }
        if !ops.is_empty() {
            blocks.push(RBlock { ops });
        }
    }
    blocks
}

// ---- code generation ----

// Opcodes
const UNREACHABLE: u8 = 0x00;
const BLOCK: u8 = 0x02;
const LOOP: u8 = 0x03;
const IF: u8 = 0x04;
const ELSE: u8 = 0x05;
const END: u8 = 0x0b;
const BR: u8 = 0x0c;
const BR_TABLE: u8 = 0x0e;
const RETURN: u8 = 0x0f;
const CALL: u8 = 0x10;
const CALL_INDIRECT: u8 = 0x11;
const SELECT: u8 = 0x1b;
const LOCAL_GET: u8 = 0x20;
const LOCAL_SET: u8 = 0x21;
const LOCAL_TEE: u8 = 0x22;
const BLOCK_EMPTY: u8 = 0x40;
const I32_LOAD: u8 = 0x28;
const I64_LOAD: u8 = 0x29;
const I32_LOAD8_U: u8 = 0x2d;
const I64_LOAD8_U: u8 = 0x31;
const I64_LOAD16_U: u8 = 0x33;
const I64_LOAD32_U: u8 = 0x35;
const I32_STORE: u8 = 0x36;
const I64_STORE: u8 = 0x37;
const I64_STORE8: u8 = 0x3c;
const I64_STORE16: u8 = 0x3d;
const I64_STORE32: u8 = 0x3e;
const I32_CONST: u8 = 0x41;
const I64_CONST: u8 = 0x42;
const I32_EQZ: u8 = 0x45;
const I32_EQ: u8 = 0x46;
const I32_NE: u8 = 0x47;
const I32_LE_U: u8 = 0x4d;
const I64_EQZ: u8 = 0x50;
const I64_EQ: u8 = 0x51;
const I64_NE: u8 = 0x52;
const I64_LT_S: u8 = 0x53;
const I64_LT_U: u8 = 0x54;
const I64_GT_U: u8 = 0x56;
const I64_GE_S: u8 = 0x59;
const I64_GE_U: u8 = 0x5a;
const I32_ADD: u8 = 0x6a;
const I32_SUB: u8 = 0x6b;
const I32_MUL: u8 = 0x6c;
const I32_DIV_S: u8 = 0x6d;
const I32_DIV_U: u8 = 0x6e;
const I32_REM_S: u8 = 0x6f;
const I32_REM_U: u8 = 0x70;
const I32_AND: u8 = 0x71;
const I32_SHL: u8 = 0x74;
const I32_SHR_S: u8 = 0x75;
const I32_SHR_U: u8 = 0x76;
const I64_ADD: u8 = 0x7c;
const I64_SUB: u8 = 0x7d;
const I64_MUL: u8 = 0x7e;
const I64_DIV_S: u8 = 0x7f;
const I64_DIV_U: u8 = 0x80;
const I64_REM_S: u8 = 0x81;
const I64_REM_U: u8 = 0x82;
const I64_AND: u8 = 0x83;
const I64_OR: u8 = 0x84;
const I64_XOR: u8 = 0x85;
const I64_SHL: u8 = 0x86;
const I64_SHR_S: u8 = 0x87;
const I64_SHR_U: u8 = 0x88;
const I32_WRAP_I64: u8 = 0xa7;
const I64_EXTEND_I32_S: u8 = 0xac;
const I64_EXTEND_I32_U: u8 = 0xad;
const I64_EXTEND8_S: u8 = 0xc2;
const I64_EXTEND16_S: u8 = 0xc3;
const I64_EXTEND32_S: u8 = 0xc4;
const TY_I32: u8 = 0x7f;
const TY_I64: u8 = 0x7e;

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
/// TLB permission bits loads and stores need (see the entry checks)
const NEED_R: u32 = 6;
const NEED_W: u32 = 7;
const NUM_I32: u32 = 6;
/// pc & !0xfff
const PAGE: u32 = 8;
/// icount at the start of the current block
const ICOUNT: u32 = 9;
const A: u32 = 10;
const B: u32 = 11;
const R: u32 = 12;
const T: u32 = 13;
/// Address of a load or store
const VA: u32 = 14;
/// Value of a load (slow path)
const MV: u32 = 15;
const NUM_I64: u32 = 8;

const OFF_X: u32 = offset_of!(Cpu, x) as u32;
const OFF_PC: u32 = offset_of!(Cpu, pc) as u32;
const OFF_PRV: u32 = offset_of!(Cpu, prv) as u32;
const OFF_ICOUNT: u32 = offset_of!(Cpu, icount) as u32;
const OFF_FLAG: u32 = offset_of!(Cpu, jit_flag) as u32;
const OFF_HOPS: u32 = offset_of!(Cpu, jit_hops) as u32;
const OFF_MSTATUS: u32 = (offset_of!(Cpu, csr) + offset_of!(crate::csr::Csrs, mstatus)) as u32;

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

    fn tee(&mut self, l: u32) {
        self.op(LOCAL_TEE);
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
    /// Address of guest RAM and of the TLB entries in linear memory.
    pub ram_addr: u32,
    pub tlb_addr: u32,
    /// Privilege mode (S or U) the code runs in; checked on entry.
    pub prv: u64,
    /// Address of Jit::ptrs, and the function table index of region 0 (to
    /// call other regions).
    pub ptrs_addr: u32,
    pub table_base: u32,
    /// Address of Bus::code_pages.
    pub code_pages_addr: u32,
}

struct Gen<'t> {
    a: Asm,
    t: &'t Target,
    /// Blocks in the region, and the current one
    nblocks: usize,
    bi: usize,
    /// The current block's instructions
    ops: Vec<Insn>,
    /// Control structures open in the current block's code
    nest: u32,
}

impl Gen<'_> {
    fn x(&mut self, r: u8) {
        if r == 0 {
            self.a.i64c(0);
        } else {
            self.a.load64(OFF_X + 8 * r as u32);
        }
    }

    fn set_x(&mut self, r: u8, l: u32) {
        if r != 0 {
            self.a.store64_local(OFF_X + 8 * r as u32, l);
        }
    }

    fn if_(&mut self) {
        self.a.if_();
        self.nest += 1;
    }

    /// `if` with a result of type `ty`.
    fn if_ty(&mut self, ty: u8) {
        self.a.op(IF);
        self.a.op(ty);
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

    /// Page offset of instruction k of the block (k = len: just past it).
    fn off_k(&self, k: usize) -> u64 {
        match self.ops.get(k) {
            Some(ins) => 2 * ins.slot as u64,
            None => {
                let last = self.ops.last().unwrap();
                2 * last.slot as u64 + last.len as u64
            }
        }
    }

    /// Push the address of instruction k of the block.
    fn pc_k(&mut self, k: usize) {
        let off = self.off_k(k);
        self.a.get(PAGE);
        self.a.i64c(off);
        self.a.op(I64_ADD);
    }

    fn add_icount(&mut self, n: usize) {
        self.a.get(ICOUNT);
        self.a.i64c(n as u64);
        self.a.op(I64_ADD);
        self.a.set(ICOUNT);
    }

    /// Return `status` with icount = ICOUNT + n and pc = address of
    /// instruction k.
    fn ret_k(&mut self, k: usize, n: usize, status: u32) {
        self.a.get(CPU);
        self.pc_k(k);
        self.a.mem(I64_STORE, 3, OFF_PC);
        self.store_icount(n);
        self.a.i32c(status as i32);
        self.a.op(RETURN);
    }

    /// cpu.icount = ICOUNT + n.
    fn store_icount(&mut self, n: usize) {
        self.a.get(CPU);
        self.a.get(ICOUNT);
        if n != 0 {
            self.a.i64c(n as u64);
            self.a.op(I64_ADD);
        }
        self.a.mem(I64_STORE, 3, OFF_ICOUNT);
    }

    /// Go on at page offset `target` (wrapping) after `count` instructions
    /// of the block: in this region if it's in the page, else return.
    fn goto(&mut self, target: u64, count: usize) {
        if target < 0x1000 {
            self.add_icount(count);
            self.a.i32c(target as i32 / 2);
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
        self.a.get(CPU);
        self.a.get(T);
        self.a.mem(I64_STORE, 3, OFF_PC);
        self.store_icount(count);
        // Hops left?
        self.a.load32(OFF_HOPS);
        self.a.tee(N32);
        self.if_();
        self.a.get(CPU);
        self.a.get(N32);
        self.a.i32c(1);
        self.a.op(I32_SUB);
        self.a.mem(I32_STORE, 2, OFF_HOPS);
        // Executable RAM in the TLB
        let need = if self.t.prv == PRV_U { P_X_U } else { P_X_S };
        self.tlb_hit(0, 2, need as i32, false);
        self.if_();
        // N32 = the RAM page: has it compiled code that wasn't written to?
        self.a.get(E32);
        self.a.mem(I32_LOAD, 2, l::RAM);
        self.a.i32c(12);
        self.a.op(I32_SHR_U);
        self.a.tee(N32);
        self.a.mem(I32_LOAD8_U, 0, self.t.code_pages_addr);
        self.if_();
        // E32 = its JIT state
        self.a.get(N32);
        self.a.i32c(2);
        self.a.op(I32_SHL);
        self.a.mem(I32_LOAD, 2, self.t.ptrs_addr);
        self.a.tee(E32);
        self.if_();
        // N32 = the slot's state: installed?
        self.a.get(E32);
        self.a.get(T);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(0xffe);
        self.a.op(I32_AND);
        self.a.i32c(1);
        self.a.op(I32_SHL);
        self.a.op(I32_ADD);
        self.a.mem(I32_LOAD, 2, 0);
        self.a.tee(N32);
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

    /// Go on at local T (a jump target) after `count` instructions of the block.
    fn goto_t(&mut self, count: usize) {
        self.add_icount(count);
        // In this page: dispatch
        self.a.get(T);
        self.a.i64c(!0xfff);
        self.a.op(I64_AND);
        self.a.get(PAGE);
        self.a.op(I64_EQ);
        self.if_();
        self.a.get(T);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(0xfff);
        self.a.op(I32_AND);
        self.a.i32c(1);
        self.a.op(I32_SHR_U);
        self.a.set(SLOT);
        self.br_top();
        self.end();
        self.chain(0);
    }

    /// Before a call to jit_read/jit_write at instruction k: icount as the
    /// interpreter has it (devices may read the time).
    fn sync_icount(&mut self, k: usize) { self.store_icount(k + 1) }

    /// After a call to jit_read/jit_write: F32 = jit_flag, and on a fault,
    /// return at instruction k (which takes the trap).
    fn check_fault(&mut self, k: usize) {
        self.a.load32(OFF_FLAG);
        self.a.tee(F32);
        self.a.i32c(1);
        self.a.op(I32_EQ);
        self.if_();
        self.ret_k(k, k, STATUS_EXCEPTION);
        self.end();
    }

    /// At the end of a load or store: if it wasn't plain RAM, return after it.
    fn check_device(&mut self, k: usize) {
        self.a.get(F32);
        self.if_();
        self.ret_k(k + 1, k + 1, STATUS_OK);
        self.end();
    }

    /// VA = T + offset; push 1 if the TLB has a RAM entry for VA with one of
    /// the permission bits `need` (a constant, or with need < 0, local
    /// -need) and VA..VA+bytes doesn't cross a page, with E32 = the entry's
    /// address. For a write, the page also mustn't have compiled code (the
    /// slow path notes the write: Bus::written).
    fn tlb_hit(&mut self, offset: u64, bytes: u32, need: i32, write: bool) {
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
        a.op(I32_WRAP_I64);
        a.i32c(TLB_ENTRIES as i32 - 1);
        a.op(I32_AND);
        a.i32c(l::ENTRY_SIZE as i32);
        a.op(I32_MUL);
        a.i32c(self.t.tlb_addr as i32);
        a.op(I32_ADD);
        a.tee(E32);
        // vpn matches
        a.mem(I64_LOAD, 3, l::VPN);
        a.get(VA);
        a.i64c(12);
        a.op(I64_SHR_U);
        a.op(I64_EQ);
        // within the page
        a.get(VA);
        a.op(I32_WRAP_I64);
        a.i32c(0xfff);
        a.op(I32_AND);
        a.i32c(0x1000 - bytes as i32);
        a.op(I32_LE_U);
        a.op(I32_AND);
        // permission
        a.get(E32);
        a.mem(I32_LOAD8_U, 0, l::PERM);
        if need < 0 {
            a.get((-need) as u32);
        } else {
            a.i32c(need);
        }
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
        if write {
            // (RAM is -1 for anything but RAM: then this reads the byte
            // before the table, with the result ignored)
            a.get(E32);
            a.mem(I32_LOAD, 2, l::RAM);
            a.i32c(12);
            a.op(I32_SHR_S);
            a.i32c(self.t.code_pages_addr as i32);
            a.op(I32_ADD);
            a.mem(I32_LOAD8_U, 0, 0);
            a.op(I32_EQZ);
            a.op(I32_AND);
        }
    }

    /// Push the address in linear memory of VA (base for a memarg offset of
    /// ram_addr), after a TLB hit.
    fn ram_ptr(&mut self) {
        self.a.get(E32);
        self.a.mem(I32_LOAD, 2, l::RAM);
        self.a.get(VA);
        self.a.op(I32_WRAP_I64);
        self.a.i32c(0xfff);
        self.a.op(I32_AND);
        self.a.op(I32_ADD);
    }

    /// Push the value at T (bytes wide, zero-extended); on a fault return at
    /// instruction k.
    fn load(&mut self, k: usize, bytes: u32) {
        self.tlb_hit(0, bytes, -(NEED_R as i32), false);
        self.if_ty(TY_I64);
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

    /// Store local v (bytes wide) to T; on a fault return at instruction k.
    fn store(&mut self, k: usize, bytes: u32, v: u32) {
        self.tlb_hit(0, bytes, -(NEED_W as i32), true);
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

    /// x[rd] = the value on the stack.
    fn set_rd(&mut self, rd: u8) {
        self.a.set(R);
        self.set_x(rd, R);
    }

    /// Push A and B as i32s.
    fn ab32(&mut self) {
        self.a.get(A);
        self.a.op(I32_WRAP_I64);
        self.a.get(B);
        self.a.op(I32_WRAP_I64);
    }

    /// The high 64 bits of A * B (signs: whether A and B are signed).
    fn mul_high(&mut self, a_signed: bool, b_signed: bool) {
        // From 32-bit halves, unsigned
        let half = |a: &mut Asm, l: u32, hi: bool| {
            a.get(l);
            if hi {
                a.i64c(32);
                a.op(I64_SHR_U);
            } else {
                a.i64c(0xffff_ffff);
                a.op(I64_AND);
            }
        };
        let a = &mut self.a;
        // T = lo(A) * lo(B)
        half(a, A, false);
        half(a, B, false);
        a.op(I64_MUL);
        a.set(T);
        // VA = hi(A) * lo(B) + (T >> 32)
        half(a, A, true);
        half(a, B, false);
        a.op(I64_MUL);
        half(a, T, true);
        a.op(I64_ADD);
        a.set(VA);
        // MV = lo(VA) + lo(A) * hi(B)
        half(a, VA, false);
        half(a, A, false);
        half(a, B, true);
        a.op(I64_MUL);
        a.op(I64_ADD);
        a.set(MV);
        // hi(A) * hi(B) + (VA >> 32) + (MV >> 32)
        half(a, A, true);
        half(a, B, true);
        a.op(I64_MUL);
        half(a, VA, true);
        a.op(I64_ADD);
        half(a, MV, true);
        a.op(I64_ADD);
        // Signed operands: minus the other one if negative
        for (x, y, signed) in [(A, B, a_signed), (B, A, b_signed)] {
            if signed {
                a.get(x);
                a.i64c(63);
                a.op(I64_SHR_S);
                a.get(y);
                a.op(I64_AND);
                a.op(I64_SUB);
            }
        }
    }

    /// Division and remainder (RISC-V results for a zero divisor and for
    /// overflow, where wasm traps); `w`: the 32-bit forms. Pushes the
    /// 64-bit result.
    fn div(&mut self, f3: u32, w: bool) {
        let signed = f3 & 1 == 0;
        let rem = f3 >= 6;
        let a = &mut self.a;
        let (ty, eqz, eq, op) = if w {
            (TY_I32, I32_EQZ, I32_EQ, [I32_DIV_S, I32_DIV_U, I32_REM_S, I32_REM_U])
        } else {
            (TY_I64, I64_EQZ, I64_EQ, [I64_DIV_S, I64_DIV_U, I64_REM_S, I64_REM_U])
        };
        let get = |a: &mut Asm, l: u32| {
            a.get(l);
            if w {
                a.op(I32_WRAP_I64);
            }
        };
        let minus1 = |a: &mut Asm| {
            if w {
                a.i32c(-1)
            } else {
                a.i64c(u64::MAX)
            }
        };
        // Zero divisor: all ones, or the dividend for a remainder
        get(a, B);
        a.op(eqz);
        a.op(IF);
        a.op(ty);
        if rem {
            get(a, A);
        } else {
            minus1(a);
        }
        a.op(ELSE);
        if signed && !rem {
            // MIN / -1 = MIN (A * -1, wrapping); wasm's rem_s doesn't trap
            get(a, B);
            minus1(a);
            a.op(eq);
            a.op(IF);
            a.op(ty);
            if w {
                a.i32c(0);
                get(a, A);
                a.op(I32_SUB);
            } else {
                a.i64c(0);
                get(a, A);
                a.op(I64_SUB);
            }
            a.op(ELSE);
            get(a, A);
            get(a, B);
            a.op(op[0]);
            a.op(END);
        } else {
            get(a, A);
            get(a, B);
            a.op(op[(!signed) as usize + 2 * rem as usize]);
        }
        a.op(END);
        if w {
            a.op(I64_EXTEND_I32_S);
        }
    }

    /// Instruction k of the block. Returns false if the block ends with it.
    fn insn(&mut self, k: usize) -> bool {
        let ins = self.ops[k];
        let i = ins.i;
        let (rd, rs1, rs2) = (rd(i), rs1(i), rs2(i));
        let f3 = i >> 12 & 7;
        let f7 = i >> 25;
        match i & 0x7f {
            0x37 => {
                // LUI
                self.a.i64c(imm_u(i));
                self.set_rd(rd);
            }
            0x17 => {
                // AUIPC
                self.pc_k(k);
                self.a.i64c(imm_u(i));
                self.a.op(I64_ADD);
                self.set_rd(rd);
            }
            0x6f => {
                // JAL
                if rd != 0 {
                    self.pc_k(k + 1);
                    self.set_rd(rd);
                }
                let target = self.off_k(k).wrapping_add(imm_j(i));
                self.goto(target, k + 1);
                return false;
            }
            0x67 => {
                // JALR (the target before rd is written: rd may be rs1)
                self.x(rs1);
                self.a.i64c(imm_i(i));
                self.a.op(I64_ADD);
                self.a.i64c(!1);
                self.a.op(I64_AND);
                self.a.set(T);
                if rd != 0 {
                    self.pc_k(k + 1);
                    self.set_rd(rd);
                }
                self.goto_t(k + 1);
                return false;
            }
            0x63 => {
                self.x(rs1);
                self.x(rs2);
                self.a.op([I64_EQ, I64_NE, 0, 0, I64_LT_S, I64_GE_S, I64_LT_U, I64_GE_U][f3 as usize]);
                self.if_();
                let target = self.off_k(k).wrapping_add(imm_b(i));
                self.goto(target, k + 1);
                self.end();
                // Not taken: the block ends here, open
            }
            0x03 => {
                self.x(rs1);
                self.a.i64c(imm_i(i));
                self.a.op(I64_ADD);
                self.a.set(T);
                self.a.i32c(0);
                self.a.set(F32);
                self.load(k, 1 << (f3 & 3));
                match f3 {
                    0 => self.a.op(I64_EXTEND8_S),
                    1 => self.a.op(I64_EXTEND16_S),
                    2 => self.a.op(I64_EXTEND32_S),
                    _ => {}
                }
                self.set_rd(rd);
                self.check_device(k);
            }
            0x23 => {
                self.x(rs1);
                self.a.i64c(imm_s(i));
                self.a.op(I64_ADD);
                self.a.set(T);
                self.x(rs2);
                self.a.set(B);
                self.a.i32c(0);
                self.a.set(F32);
                self.store(k, 1 << f3, B);
                self.check_device(k);
            }
            0x13 => {
                let imm = imm_i(i);
                self.x(rs1);
                match f3 {
                    0 => {
                        self.a.i64c(imm);
                        self.a.op(I64_ADD);
                    }
                    1 => {
                        self.a.i64c(imm & 63);
                        self.a.op(I64_SHL);
                    }
                    2 | 3 => {
                        self.a.i64c(imm);
                        self.a.op(if f3 == 2 { I64_LT_S } else { I64_LT_U });
                        self.a.op(I64_EXTEND_I32_U);
                    }
                    4 => {
                        self.a.i64c(imm);
                        self.a.op(I64_XOR);
                    }
                    5 => {
                        self.a.i64c(imm & 63);
                        self.a.op(if i >> 30 & 1 != 0 { I64_SHR_S } else { I64_SHR_U });
                    }
                    6 => {
                        self.a.i64c(imm);
                        self.a.op(I64_OR);
                    }
                    _ => {
                        self.a.i64c(imm);
                        self.a.op(I64_AND);
                    }
                }
                self.set_rd(rd);
            }
            0x1b => {
                self.x(rs1);
                self.a.op(I32_WRAP_I64);
                match f3 {
                    0 => {
                        self.a.i32c(imm_i(i) as i32);
                        self.a.op(I32_ADD);
                    }
                    1 => {
                        self.a.i32c(rs2 as i32);
                        self.a.op(I32_SHL);
                    }
                    _ => {
                        self.a.i32c(rs2 as i32);
                        self.a.op(if f7 == 0x20 { I32_SHR_S } else { I32_SHR_U });
                    }
                }
                self.a.op(I64_EXTEND_I32_S);
                self.set_rd(rd);
            }
            0x33 => {
                self.x(rs1);
                self.a.set(A);
                self.x(rs2);
                self.a.set(B);
                if f7 == 1 {
                    match f3 {
                        0 => {
                            self.a.get(A);
                            self.a.get(B);
                            self.a.op(I64_MUL);
                        }
                        1 => self.mul_high(true, true),
                        2 => self.mul_high(true, false),
                        3 => self.mul_high(false, false),
                        _ => self.div(f3, false),
                    }
                } else {
                    self.a.get(A);
                    self.a.get(B);
                    match (f7, f3) {
                        (0, 0) => self.a.op(I64_ADD),
                        (0x20, 0) => self.a.op(I64_SUB),
                        (0, 1) => self.a.op(I64_SHL),
                        (0, 2) => {
                            self.a.op(I64_LT_S);
                            self.a.op(I64_EXTEND_I32_U);
                        }
                        (0, 3) => {
                            self.a.op(I64_LT_U);
                            self.a.op(I64_EXTEND_I32_U);
                        }
                        (0, 4) => self.a.op(I64_XOR),
                        (0, 5) => self.a.op(I64_SHR_U),
                        (0x20, _) => self.a.op(I64_SHR_S),
                        (0, 6) => self.a.op(I64_OR),
                        _ => self.a.op(I64_AND),
                    }
                }
                self.set_rd(rd);
            }
            0x3b => {
                self.x(rs1);
                self.a.set(A);
                self.x(rs2);
                self.a.set(B);
                if f7 == 1 {
                    if f3 == 0 {
                        self.ab32();
                        self.a.op(I32_MUL);
                        self.a.op(I64_EXTEND_I32_S);
                    } else {
                        self.div(f3, true);
                    }
                    self.set_rd(rd);
                } else {
                    self.ab32();
                    self.a.op(match (f7, f3) {
                        (0, 0) => I32_ADD,
                        (0x20, 0) => I32_SUB,
                        (0, 1) => I32_SHL,
                        (0, _) => I32_SHR_U,
                        _ => I32_SHR_S,
                    });
                    self.a.op(I64_EXTEND_I32_S);
                    self.set_rd(rd);
                }
            }
            // FENCE: nothing to order with one hart
            _ => {}
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
///   end  return at pc = page + 2 * SLOT
/// end
/// ```
pub fn codegen(blocks: &[RBlock], t: &Target) -> Vec<u8> {
    let nblocks = blocks.len();
    let mut g = Gen { a: Asm::default(), t, nblocks, bi: 0, ops: Vec::new(), nest: 0 };
    let mut codes = Vec::new();
    for (bi, b) in blocks.iter().enumerate() {
        g.bi = bi;
        g.ops = b.ops.clone();
        g.nest = 0;
        let len = b.ops.len();
        let slot = b.ops[0].slot;
        // Stop before the block if it would go past ICOUNT_END
        g.a.get(ICOUNT);
        g.a.i64c(len as u64);
        g.a.op(I64_ADD);
        g.a.get(ICOUNT_END);
        g.a.op(I64_GT_U);
        g.if_();
        g.a.i32c(slot as i32);
        g.a.set(SLOT);
        g.br_exit();
        g.end();
        let mut open = true;
        for k in 0..len {
            if !g.insn(k) {
                open = false;
                break;
            }
        }
        if open {
            let end = g.off_k(len);
            if blocks.get(bi + 1).is_some_and(|n| 2 * n.ops[0].slot as u64 == end) {
                // Fall into the next block
                g.add_icount(len);
            } else {
                g.goto(end, len);
            }
        }
        debug_assert_eq!(g.nest, 0);
        codes.push(std::mem::take(&mut g.a.b));
    }

    let mut f = Asm::default();
    // Entry checks: the privilege mode, then the permission bits for loads
    // and stores (S mode: as mstatus.SUM says)
    f.load64(OFF_PRV);
    f.i64c(t.prv);
    f.op(I64_NE);
    f.if_();
    f.i32c(STATUS_WRONG_MODE as i32);
    f.op(RETURN);
    f.op(END);
    if t.prv == PRV_U {
        f.i32c(P_R_U as i32);
        f.set(NEED_R);
        f.i32c(P_W_U as i32);
        f.set(NEED_W);
    } else {
        for (need, sum, plain) in [(NEED_R, P_R_SUM, P_R_S), (NEED_W, P_W_SUM, P_W_S)] {
            f.i32c(sum as i32);
            f.i32c(plain as i32);
            f.load64(OFF_MSTATUS);
            f.i64c(MSTATUS_SUM);
            f.op(I64_AND);
            f.op(I64_EQZ);
            f.op(I32_EQZ);
            f.op(SELECT);
            f.set(need);
        }
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
    f.i32c(1);
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
    let mut table = vec![nblocks; blocks.last().map_or(0, |b| b.ops[0].slot + 1)];
    for (bi, b) in blocks.iter().enumerate() {
        table[b.ops[0].slot] = bi;
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
    // Exit: pc = page + 2 * SLOT
    f.get(CPU);
    f.get(PAGE);
    f.get(SLOT);
    f.op(I64_EXTEND_I32_U);
    f.i64c(1);
    f.op(I64_SHL);
    f.op(I64_ADD);
    f.mem(I64_STORE, 3, OFF_PC);
    f.get(CPU);
    f.get(ICOUNT);
    f.mem(I64_STORE, 3, OFF_ICOUNT);
    f.i32c(STATUS_OK as i32);
    f.op(RETURN);
    f.op(END);
    f.op(UNREACHABLE);
    f.op(END);

    // Function body: locals, code
    let mut func = Asm::default();
    func.uleb(2);
    func.uleb(NUM_I32 as u64);
    func.op(TY_I32);
    func.uleb(NUM_I64 as u64);
    func.op(TY_I64);
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

/// Self-test for the browser build (run by tools/rv64-ref/jit-test.mjs):
/// random code (mostly branching within itself) run compiled and
/// interpreted from the same random state for the same number of steps must
/// end in the same state. `compile` must install synchronously. Returns
/// a description of the first difference, or None.
pub fn selftest(seed: u64, iterations: u32, table_base: u32, compile: CompileFn) -> Option<String> {
    use crate::bus::{Bus, RAM_BASE};
    const RAM_SIZE: usize = 0x10000;
    const PTE_V: u64 = 1;
    const PTE_R: u64 = 2;
    const PTE_W: u64 = 4;
    const PTE_X: u64 = 8;
    const PTE_U: u64 = 16;
    const PTE_A: u64 = 64;
    const PTE_D: u64 = 128;
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let (mut skipped, mut not_run) = (0, 0);
    for iter in 0..iterations {
        // Two runs of compilable instructions: one in page 0 and one in page
        // 1, branching within themselves and to each other
        let mut br_regs = 0u32;
        // Registers division uses (often edge cases)
        let mut edge_regs = 0u32;
        let gen = |len: usize, br_regs: &mut u32, edge_regs: &mut u32, next: &mut dyn FnMut() -> u64| {
            // (encoding, length)
            let mut insns: Vec<(u32, u32)> = Vec::new();
            while insns.len() < len {
                let r = next();
                let (w, n) = if r % 4 == 0 {
                    let w = (next() as u32 & 0xffff) | 0;
                    if w & 3 == 3 {
                        continue;
                    }
                    (w, 2)
                } else {
                    const OPS: [u32; 12] = [0x13, 0x13, 0x1b, 0x33, 0x33, 0x3b, 0x37, 0x17, 0x63, 0x03, 0x23, 0x0f];
                    let mut w = next() as u32 & !0x7f;
                    let op = if next() % 16 == 0 { [0x6f, 0x67][(next() % 2) as usize] } else { OPS[(next() % 12) as usize] };
                    w |= op;
                    let f3 = w >> 12 & 7;
                    match op {
                        0x33 | 0x3b => w = (w & 0x01ff_ffff) | [0, 0x20, 1][(next() % 3) as usize] << 25,
                        0x13 if f3 == 1 || f3 == 5 => w = (w & 0x03ff_ffff) | [0, 0x10][(next() % 2) as usize] << 26,
                        0x1b if f3 != 0 => w = (w & 0x01ff_ffff) | [0, 0x20][(next() % 2) as usize] << 25,
                        _ => {}
                    }
                    if op == 0x67 {
                        // JALR to a register holding a target
                        w &= 0x000f_ffff;
                    }
                    if (op == 0x33 || op == 0x3b) && w >> 25 == 1 {
                        *edge_regs |= 1 << rs1(w) | 1 << rs2(w);
                    }
                    (w, 4)
                };
                let i = if n == 2 { rvc::expand(w) } else { Some(w) };
                let Some(i) = i.filter(|&i| compilable(i)) else { continue };
                if (is_jal(i) || is_jalr(i)) && !next().is_multiple_of(4) {
                    continue;
                }
                if is_jalr(i) {
                    *br_regs |= 1 << rs1(i);
                }
                insns.push((w, n));
            }
            insns
        };
        let len = 1 + (next() % 24) as usize;
        let len1 = 1 + (next() % 24) as usize;
        let mut insns = gen(len, &mut br_regs, &mut edge_regs, &mut next);
        let mut insns1 = gen(len1, &mut br_regs, &mut edge_regs, &mut next);
        let size = |v: &[(u32, u32)]| v.iter().map(|&(_, n)| n as usize).sum::<usize>();
        let (size0, size1) = (size(&insns), size(&insns1));
        let start = (next() % ((0x1000 - size0) as u64 / 2)) as usize * 2;
        let start1 = 0x1000 + (next() % ((0x1000 - size1) as u64 / 2)) as usize * 2;
        let offsets = |start: usize, v: &[(u32, u32)]| -> Vec<usize> {
            let mut at = start;
            let mut o = Vec::new();
            for &(_, n) in v {
                o.push(at);
                at += n as usize;
            }
            o.push(at);
            o
        };
        let at0 = offsets(start, &insns);
        let at1 = offsets(start1, &insns1);
        // Branch targets: the instructions, or just past either run
        let targets: Vec<usize> = at0.iter().chain(at1.iter()).copied().collect();
        // Most direct branches and jumps (32-bit ones) go to the code
        for (v, at) in [(&mut insns, &at0), (&mut insns1, &at1)] {
            for (k, (w, n)) in v.iter_mut().enumerate() {
                if *n != 4 || next() % 4 == 0 {
                    continue;
                }
                let off = targets[(next() % targets.len() as u64) as usize] as i64 - at[k] as i64;
                let o = off as u32;
                if is_branch(*w) && (-4096..4096).contains(&off) {
                    *w = (*w & 0x01ff_f07f)
                        | (o >> 12 & 1) << 31
                        | (o >> 5 & 0x3f) << 25
                        | (o >> 1 & 0xf) << 8
                        | (o >> 11 & 1) << 7;
                } else if is_jal(*w) {
                    *w = (*w & 0xfff)
                        | (o >> 20 & 1) << 31
                        | (o >> 1 & 0x3ff) << 21
                        | (o >> 11 & 1) << 20
                        | (o >> 12 & 0xff) << 12;
                }
            }
        }
        let limit = 1 + next() % 2000;
        let mmu = next() % 3 != 0;
        let state_seed = next();
        let mk = || {
            let mut r = state_seed | 1;
            let mut rn = move || {
                r ^= r >> 12;
                r ^= r << 25;
                r ^= r >> 27;
                r.wrapping_mul(0x2545_f491_4f6c_dd1d)
            };
            let mut c = Box::new(Cpu::new(Bus::new(RAM_SIZE)));
            for i in 1..32 {
                let kind = if br_regs & 1 << i != 0 {
                    2
                } else if edge_regs & 1 << i != 0 && rn() % 2 == 0 {
                    3
                } else {
                    rn() % 6
                };
                c.x[i] = match kind {
                    0 => rn(),
                    1 => rn() % 64,
                    2 => RAM_BASE + targets[(rn() % targets.len() as u64) as usize] as u64,
                    // Edge cases (division)
                    3 => [0, u64::MAX, 1 << 63, 0xffff_ffff, 0x8000_0000, 0xffff_ffff_8000_0000, 1][(rn() % 7) as usize],
                    _ => RAM_BASE + 0x2800 + rn() % 0xa000,
                };
            }
            for b in c.bus.ram[0x2000..0xe000].iter_mut() {
                *b = rn() as u8;
            }
            for (v, at) in [(&insns, &at0), (&insns1, &at1)] {
                for (k, &(w, n)) in v.iter().enumerate() {
                    c.bus.ram[at[k]..at[k] + n as usize].copy_from_slice(&w.to_le_bytes()[..n as usize]);
                }
            }
            c.pc = RAM_BASE + start as u64;
            c.ns_per_insn = 1;
            c.csr.mtvec = RAM_BASE + 0xe000;
            c.prv = PRV_S;
            if rn() % 2 == 0 {
                c.csr.mstatus |= MSTATUS_SUM;
            }
            if mmu {
                // satp: Sv39 with an empty root table (TLB misses fault);
                // RAM mapped 1:1 in the TLB with random permissions
                c.csr.satp = 8 << 60 | (RAM_BASE + 0xf000) >> 12;
                if rn() % 4 == 0 {
                    c.prv = PRV_U;
                }
                let u = if c.prv == PRV_U { PTE_U } else { 0 };
                for p in 0..(RAM_SIZE >> 12) - 1 {
                    let va = RAM_BASE + (p << 12) as u64;
                    let pte = if p < 2 {
                        PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D | u
                    } else {
                        let perm = [PTE_R | PTE_W | PTE_D, PTE_R, PTE_R | PTE_W, PTE_X, PTE_W | PTE_R | PTE_D, 0]
                            [(rn() % 6) as usize];
                        let u = if rn() % 4 == 0 { PTE_U ^ u } else { u };
                        if perm == 0 {
                            continue;
                        }
                        PTE_V | PTE_A | perm | u
                    };
                    c.tlb.insert(va, va, pte, Some(p << 12));
                }
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
            return Some(format!("iteration {iter}: code at {:#x} failed to compile ({:08x?})", a.pc, insns));
        }
        // Sometimes change an instruction of the compiled code: it must not run
        let rewrite = next() % 4 == 0;
        if rewrite {
            let at = targets[(next() % targets.len() as u64) as usize] & !1;
            let w = loop {
                let w = next() as u32 | 3;
                if compilable(w) {
                    break w;
                }
            };
            if at + 4 <= 0x2000 {
                a.bus.write_ram(at, 4, w as u64);
                b.bus.write_ram(at, 4, w as u64);
            }
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
                return Some(format!("iteration {iter}: state changed without running ({:08x?} {:08x?})", insns, insns1));
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
                return Some(format!("iteration {iter}: write to code page {p} not noticed ({:08x?} {:08x?})", insns, insns1));
            }
        }
        // Code that changes itself is only seen by compiled code after it
        // returns: skip
        if b.bus.ram[..0x2000] != a_code[..] {
            skipped += 1;
            continue;
        }
        let state = |c: &Cpu| {
            let s = &c.csr;
            (c.x, c.pc, c.prv, c.icount, s.mstatus, s.mepc, s.mcause, s.mtval, s.sepc, s.scause, s.stval)
        };
        if state(&a) != state(&b) || a.bus.ram != b.bus.ram {
            let mut d = format!(
                "iteration {iter}: state differs after code {:08x?} at {:#x} and {:08x?} at {:#x} (mmu {mmu}, limit {limit}, used {used})\n",
                insns,
                RAM_BASE + start as u64,
                insns1,
                RAM_BASE + start1 as u64
            );
            for i in 0..32 {
                if a.x[i] != b.x[i] {
                    d += &format!("  x{i}: jit {:#x} interp {:#x}\n", a.x[i], b.x[i]);
                }
            }
            d += &format!(
                "  pc {:#x}/{:#x} prv {}/{} icount {}/{} mcause {:#x}/{:#x} mtval {:#x}/{:#x} mepc {:#x}/{:#x} ram {}\n",
                a.pc,
                b.pc,
                a.prv,
                b.prv,
                a.icount,
                b.icount,
                a.csr.mcause,
                b.csr.mcause,
                a.csr.mtval,
                b.csr.mtval,
                a.csr.mepc,
                b.csr.mepc,
                if a.bus.ram == b.bus.ram { "same" } else { "differs" }
            );
            return Some(d);
        }
    }
    if skipped + not_run > iterations / 4 {
        return Some(format!("{skipped} of {iterations} iterations skipped, {not_run} not run compiled"));
    }
    None
}
