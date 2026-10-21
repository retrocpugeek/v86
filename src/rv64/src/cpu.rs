//! The hart: architectural state, traps and interrupts, and the run loop.

use crate::bus::Bus;
use crate::csr::Csrs;
use crate::mmu::Tlb;
use crate::jit::{self, Jit};
use crate::rvc;

pub const PRV_U: u64 = 0;
pub const PRV_S: u64 = 1;
pub const PRV_M: u64 = 3;

// mstatus bits
pub const MSTATUS_SIE: u64 = 1 << 1;
pub const MSTATUS_MIE: u64 = 1 << 3;
pub const MSTATUS_SPIE: u64 = 1 << 5;
pub const MSTATUS_MPIE: u64 = 1 << 7;
pub const MSTATUS_SPP: u64 = 1 << 8;
pub const MSTATUS_MPP_SHIFT: u32 = 11;
pub const MSTATUS_MPP: u64 = 3 << MSTATUS_MPP_SHIFT;
pub const MSTATUS_FS: u64 = 3 << 13;
pub const MSTATUS_MPRV: u64 = 1 << 17;
pub const MSTATUS_SUM: u64 = 1 << 18;
pub const MSTATUS_MXR: u64 = 1 << 19;
pub const MSTATUS_TVM: u64 = 1 << 20;
pub const MSTATUS_TW: u64 = 1 << 21;
pub const MSTATUS_TSR: u64 = 1 << 22;
pub const MSTATUS_SD: u64 = 1 << 63;

// Interrupt numbers (mip/mie bits, and mcause with the top bit set)
pub const IRQ_SSI: u64 = 1;
pub const IRQ_MSI: u64 = 3;
pub const IRQ_STI: u64 = 5;
pub const IRQ_MTI: u64 = 7;
pub const IRQ_SEI: u64 = 9;
pub const IRQ_MEI: u64 = 11;
pub const MIP_SSIP: u64 = 1 << IRQ_SSI;
pub const MIP_MSIP: u64 = 1 << IRQ_MSI;
pub const MIP_STIP: u64 = 1 << IRQ_STI;
pub const MIP_MTIP: u64 = 1 << IRQ_MTI;
pub const MIP_SEIP: u64 = 1 << IRQ_SEI;
pub const MIP_MEIP: u64 = 1 << IRQ_MEI;

/// Exception causes
pub mod cause {
    pub const INSN_MISALIGNED: u64 = 0;
    pub const INSN_ACCESS: u64 = 1;
    pub const ILLEGAL_INSN: u64 = 2;
    pub const BREAKPOINT: u64 = 3;
    pub const LOAD_MISALIGNED: u64 = 4;
    pub const LOAD_ACCESS: u64 = 5;
    pub const STORE_MISALIGNED: u64 = 6;
    pub const STORE_ACCESS: u64 = 7;
    pub const ECALL_U: u64 = 8;
    pub const ECALL_S: u64 = 9;
    pub const ECALL_M: u64 = 11;
    pub const INSN_PAGE_FAULT: u64 = 12;
    pub const LOAD_PAGE_FAULT: u64 = 13;
    pub const STORE_PAGE_FAULT: u64 = 15;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exception {
    pub cause: u64,
    pub tval: u64,
}

pub type Res<T> = Result<T, Exception>;

pub fn illegal(insn: u32) -> Exception { Exception { cause: cause::ILLEGAL_INSN, tval: insn as u64 } }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    On,
    Off,
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunExit {
    Budget,
    /// Waiting for an interrupt with no timer armed.
    Idle,
    PowerOff,
    Reset,
}

/// Host-time jumps larger than this (the host didn't run us) aren't passed on.
pub const MAX_WARP_NS: u64 = 1_000_000_000;

pub struct Cpu {
    pub x: [u64; 32],
    /// FP registers; single precision values are NaN-boxed.
    pub f: [u64; 32],
    pub pc: u64,
    pub prv: u64,
    pub csr: Csrs,
    pub bus: Bus,
    pub tlb: Tlb,
    /// LR reservation (physical address).
    pub reservation: Option<u64>,
    /// pc of the instruction being executed.
    pub insn_pc: u64,
    /// Retired instructions (and taken traps), drives virtual time.
    pub icount: u64,
    pub ns_per_insn: u64,
    /// Virtual time beyond icount * ns_per_insn: idle time skipped or waited.
    pub warp_ns: u64,
    /// Follow the host clock (browser) instead of skipping idle time.
    pub realtime: bool,
    pub host_base_ns: Option<u64>,
    /// In WFI.
    pub waiting: bool,
    pub power: Power,
    /// Virtual time when the timer next fires (mtimecmp), or u64::MAX.
    pub next_timer_ns: u64,
    /// Set by anything that changes the interrupt or timer state: ends the
    /// current batch of steps.
    pub stop_batch: bool,
    pub log_illegal: bool,
    /// Traps taken, by cause: [interrupts, exceptions] (for diagnostics).
    pub trap_counts: [[u64; 16]; 2],
    /// Take pending interrupts in step(); lockstep turns this off and takes
    /// them where QEMU did.
    pub auto_irq: bool,
    /// Decoded instructions by virtual pc; see ICacheEntry.
    pub icache: Vec<ICacheEntry>,
    icache_gen: u32,
    pub jit: Jit,
    /// Set by jit_read/jit_write for compiled code (see jit.rs).
    pub jit_flag: u32,
    pub jit_exc: Exception,
    /// Calls compiled code may still chain to another page's code (see jit.rs).
    pub jit_hops: u32,
    /// pc is a block start: a branch went there, or the instruction before
    /// it trapped or isn't compiled (see jit.rs).
    pub block_start: bool,
    /// RV32 (XLEN 32): registers hold sign-extended 32-bit values, pc and
    /// addresses are 32 bits, Sv32 paging (see exec32).
    pub xlen32: bool,
    /// CH32V (QingKe) hardware prologue/epilogue: per trap taken, the
    /// caller-saved registers HPE pushed (interrupts with INTSYSCR.HWSTKEN),
    /// which its mret restores.
    hpe_stack: Vec<Option<[u64; 16]>>,
}

/// The registers QingKe's HPE saves: ra, t0-t2, a0-a7, t3-t6.
const HPE_REGS: [usize; 16] = [1, 5, 6, 7, 10, 11, 12, 13, 14, 15, 16, 17, 28, 29, 30, 31];
const INTSYSCR_HWSTKEN: u64 = 1 << 0;
/// Interrupts off, and the next mret doesn't restore the HPE frame (RTOS
/// context switches set it)
const INTSYSCR_GIHWSTKNEN: u64 = 1 << 5;

const ICACHE_ENTRIES: usize = 1 << 14;

/// A fetched and expanded instruction. RISC-V only guarantees that fetches
/// see stores after FENCE.I, and new translations after SFENCE.VMA or a satp
/// write, so the cache is flushed on those (by bumping the generation).
#[derive(Clone, Copy)]
pub struct ICacheEntry {
    pc: u64,
    gen: u32,
    insn: u32,
    raw: u32,
    len: u8,
    prv: u8,
    /// The JIT compiles the instruction (jit::compilable).
    jit: bool,
}

impl Cpu {
    pub fn new(bus: Bus) -> Cpu {
        Cpu {
            x: [0; 32],
            f: [0; 32],
            pc: 0x1000,
            prv: PRV_M,
            csr: Csrs::default(),
            bus,
            tlb: Tlb::default(),
            reservation: None,
            insn_pc: 0,
            icount: 0,
            ns_per_insn: 0,
            warp_ns: 0,
            realtime: false,
            host_base_ns: None,
            waiting: false,
            power: Power::On,
            next_timer_ns: u64::MAX,
            stop_batch: false,
            log_illegal: false,
            trap_counts: [[0; 16]; 2],
            auto_irq: true,
            icache: vec![ICacheEntry { pc: 1, gen: 0, insn: 0, raw: 0, len: 0, prv: 0, jit: false }; ICACHE_ENTRIES],
            icache_gen: 1,
            jit: Jit::default(),
            jit_flag: 0,
            jit_exc: Exception { cause: 0, tval: 0 },
            jit_hops: 0,
            block_start: true,
            xlen32: false,
            hpe_stack: Vec::new(),
        }
    }

    // ---- time ----

    #[inline]
    pub fn now_ns(&self) -> u64 { self.icount.wrapping_mul(self.ns_per_insn).wrapping_add(self.warp_ns) }

    /// Bring mtime-derived state up to date: the bus's time and MTIP.
    pub fn timer_update(&mut self) {
        let now = self.now_ns();
        self.bus.now_ns = now;
        if let Some(c) = self.bus.ch32.as_mut() {
            // CH32V003: SysTick instead of the CLINT
            c.update(now);
            self.next_timer_ns = c.next_event_ns();
            return;
        }
        let cmp = self.bus.clint.mtimecmp;
        let period = 1_000_000_000 / crate::bus::TIMEBASE_HZ;
        if self.bus.mtime() >= cmp {
            self.csr.mip |= MIP_MTIP;
            self.next_timer_ns = u64::MAX;
        } else {
            self.csr.mip &= !MIP_MTIP;
            self.next_timer_ns = cmp.saturating_mul(period);
        }
    }

    /// Time instructions as one per cycle at `hz` (a CH32V's HCLK), keeping
    /// virtual time continuous.
    pub fn set_clock_hz(&mut self, hz: u64) {
        let ns = (1_000_000_000 + hz / 2).checked_div(hz).unwrap_or(1).max(1);
        if ns != self.ns_per_insn {
            let now = self.now_ns();
            self.ns_per_insn = ns;
            self.warp_ns = now.wrapping_sub(self.icount.wrapping_mul(ns));
        }
    }

    /// Advance virtual time to at least the host clock (realtime mode); see
    /// v86's aarch64 core for the reasoning behind MAX_WARP_NS.
    pub fn sync_host_ns(&mut self, host_ns: u64) {
        let now = self.now_ns();
        let base = self.host_base_ns.get_or_insert(host_ns.saturating_sub(now));
        let target = host_ns.saturating_sub(*base);
        if target > now {
            let gap = target - now;
            if gap > MAX_WARP_NS {
                *base += gap - MAX_WARP_NS;
            }
            self.warp_ns += gap.min(MAX_WARP_NS);
        }
    }

    /// How far virtual time is ahead of the host clock (realtime mode), in ns.
    pub fn ns_ahead(&self, host_ns: u64) -> u64 {
        self.host_base_ns.map_or(0, |base| self.now_ns().saturating_sub(host_ns.saturating_sub(base)))
    }

    pub fn ns_until_timer(&self) -> Option<u64> {
        (self.next_timer_ns != u64::MAX).then(|| self.next_timer_ns.saturating_sub(self.now_ns()))
    }

    // ---- interrupts ----

    /// Copy the CLINT's and PLIC's lines into mip (after device accesses).
    pub fn update_mip(&mut self) {
        if let Some(c) = self.bus.ch32.as_mut() {
            // Console input may have arrived
            c.invalidate();
            if c.clock_changed {
                c.clock_changed = false;
                let hz = c.hclk();
                self.set_clock_hz(hz);
            }
        }
        let mut mip = self.csr.mip & !(MIP_MSIP | MIP_MEIP);
        if self.bus.clint.msip {
            mip |= MIP_MSIP;
        }
        if self.bus.plic.out[0] {
            mip |= MIP_MEIP;
        }
        // SEIP: the PLIC's line or'ed with the bit M mode writes
        mip = (mip & !MIP_SEIP) | if self.bus.plic.out[1] || self.csr.seip_sw { MIP_SEIP } else { 0 };
        self.csr.mip = mip;
        self.timer_update();
        self.stop_batch = true;
    }

    /// Is an interrupt pending that ends WFI (whether or not it's enabled)?
    #[inline]
    pub fn wake_pending(&self) -> bool {
        match &self.bus.ch32 {
            Some(c) => c.pending_irq().is_some(),
            None => self.csr.mip & self.csr.mie != 0,
        }
    }

    /// The interrupt to take now, if any.
    #[inline]
    pub fn pending_interrupt(&self) -> Option<u64> {
        if let Some(c) = &self.bus.ch32 {
            // CH32V: the PFIC's interrupts, in machine mode
            let on = self.csr.mstatus & MSTATUS_MIE != 0 && self.csr.intsyscr & INTSYSCR_GIHWSTKNEN == 0;
            return if on { c.pending_irq().map(|n| n as u64) } else { None };
        }
        let pending = self.csr.mip & self.csr.mie;
        if pending == 0 {
            return None;
        }
        let mstatus = self.csr.mstatus;
        let m_on = self.prv < PRV_M || mstatus & MSTATUS_MIE != 0;
        let s_on = self.prv < PRV_S || (self.prv == PRV_S && mstatus & MSTATUS_SIE != 0);
        let m = if m_on { pending & !self.csr.mideleg } else { 0 };
        let s = if s_on { pending & self.csr.mideleg } else { 0 };
        let take = if m != 0 { m } else { s };
        if take == 0 {
            return None;
        }
        [IRQ_MEI, IRQ_MSI, IRQ_MTI, IRQ_SEI, IRQ_SSI, IRQ_STI].into_iter().find(|&i| take & 1 << i != 0)
    }

    // ---- traps ----

    /// Take an exception (interrupt = false) or interrupt at pc.
    pub fn trap(&mut self, cause: u64, tval: u64, interrupt: bool, pc: u64) {
        self.trap_counts[!interrupt as usize][(cause & 15) as usize] += 1;
        let deleg = if interrupt { self.csr.mideleg } else { self.csr.medeleg };
        let mcause = cause | (interrupt as u64) << if self.xlen32 { 31 } else { 63 };
        self.reservation = None;
        if self.prv <= PRV_S && deleg >> cause & 1 != 0 {
            self.csr.scause = mcause;
            self.csr.sepc = pc;
            self.csr.stval = tval;
            let mut s = self.csr.mstatus;
            s = (s & !MSTATUS_SPP) | if self.prv == PRV_S { MSTATUS_SPP } else { 0 };
            s = (s & !MSTATUS_SPIE) | if s & MSTATUS_SIE != 0 { MSTATUS_SPIE } else { 0 };
            s &= !MSTATUS_SIE;
            self.csr.mstatus = s;
            self.prv = PRV_S;
            let base = self.csr.stvec & !3;
            self.pc = if interrupt && self.csr.stvec & 1 != 0 { base + 4 * cause } else { base };
        } else {
            self.csr.mcause = mcause;
            self.csr.mepc = pc;
            self.csr.mtval = tval;
            let mut s = self.csr.mstatus;
            s = (s & !MSTATUS_MPP) | self.prv << MSTATUS_MPP_SHIFT;
            s = (s & !MSTATUS_MPIE) | if s & MSTATUS_MIE != 0 { MSTATUS_MPIE } else { 0 };
            s &= !MSTATUS_MIE;
            self.csr.mstatus = s;
            self.prv = PRV_M;
            let base = self.csr.mtvec & !3;
            self.pc = if interrupt && self.csr.mtvec & 1 != 0 { base + 4 * cause } else { base };
            if self.bus.ch32.is_some() {
                self.ch32_trap(cause, interrupt);
            }
        }
        self.stop_batch = true;
    }

    /// CH32V (QingKe) traps: HPE saves the caller-saved registers of
    /// interrupts, and in vector table mode (mtvec bit 0) exceptions have
    /// table entries too (ecall from M: 5, from U: 8, breakpoint: 9, the rest
    /// HardFault: 3); with mtvec bit 1 the table holds handler addresses,
    /// else instructions to jump to.
    fn ch32_trap(&mut self, cause: u64, interrupt: bool) {
        let frame = (interrupt && self.csr.intsyscr & INTSYSCR_HWSTKEN != 0).then(|| HPE_REGS.map(|r| self.x[r]));
        self.hpe_stack.push(frame);
        let mtvec = self.csr.mtvec;
        if mtvec & 1 == 0 {
            return;
        }
        let entry = if interrupt {
            cause
        } else {
            match cause {
                cause::ECALL_M => 5,
                cause::ECALL_U => 8,
                cause::BREAKPOINT => 9,
                _ => 3,
            }
        };
        let addr = (mtvec & !3) + 4 * entry;
        self.pc = if mtvec & 2 != 0 { self.bus.read(addr, 4) & !1 } else { addr };
    }

    pub fn mret(&mut self) {
        if self.bus.ch32.is_some() {
            // HPE: restore what the trap saved, unless GIHWSTKNEN says not to
            let frame = self.hpe_stack.pop().flatten();
            if self.csr.intsyscr & INTSYSCR_GIHWSTKNEN != 0 {
                self.csr.intsyscr &= !INTSYSCR_GIHWSTKNEN;
            } else if let Some(regs) = frame {
                for (k, &r) in HPE_REGS.iter().enumerate() {
                    self.x[r] = regs[k];
                }
            }
        }
        let s = self.csr.mstatus;
        let mpp = (s & MSTATUS_MPP) >> MSTATUS_MPP_SHIFT;
        let mut s = (s & !MSTATUS_MIE) | if s & MSTATUS_MPIE != 0 { MSTATUS_MIE } else { 0 };
        s |= MSTATUS_MPIE;
        s &= !MSTATUS_MPP; // U
        if mpp != PRV_M {
            s &= !MSTATUS_MPRV;
        }
        self.csr.mstatus = s;
        self.prv = mpp;
        self.pc = self.csr.mepc;
        self.reservation = None;
        self.stop_batch = true;
    }

    pub fn sret(&mut self) {
        let s = self.csr.mstatus;
        let spp = if s & MSTATUS_SPP != 0 { PRV_S } else { PRV_U };
        let mut s = (s & !MSTATUS_SIE) | if s & MSTATUS_SPIE != 0 { MSTATUS_SIE } else { 0 };
        s |= MSTATUS_SPIE;
        s &= !MSTATUS_SPP;
        s &= !MSTATUS_MPRV;
        self.csr.mstatus = s;
        self.prv = spp;
        self.pc = self.csr.sepc;
        self.reservation = None;
        self.stop_batch = true;
    }

    // ---- execution ----

    pub fn icache_flush(&mut self) {
        self.icache_gen = self.icache_gen.wrapping_add(1);
        if self.icache_gen == 0 {
            for e in self.icache.iter_mut() {
                e.gen = 0;
            }
            self.icache_gen = 1;
        }
    }

    /// Execute one instruction, or take an interrupt.
    #[inline]
    pub fn step(&mut self) { self.step_inner::<false>() }

    /// step(); with JIT, also tracks block starts.
    #[inline(always)]
    fn step_inner<const JIT: bool>(&mut self) {
        if let Some(irq) = self.pending_interrupt().filter(|_| self.auto_irq) {
            self.waiting = false;
            let pc = self.pc;
            self.trap(irq, 0, true, pc);
            if JIT {
                self.block_start = true;
            }
            return;
        }
        let pc = self.pc;
        self.insn_pc = pc;
        // Like QEMU's icount, the instruction is counted before it executes.
        self.icount += 1;
        let slot = (pc >> 1) as usize & (ICACHE_ENTRIES - 1);
        let e = self.icache[slot];
        let (r, len, compiled) = if e.pc == pc && e.gen == self.icache_gen && e.prv == self.prv as u8 {
            self.pc = pc.wrapping_add(e.len as u64);
            (self.exec(e.insn, e.raw), e.len, e.jit)
        } else {
            match self.fetch(pc) {
                Ok((raw, len)) => match if len == 2 { rvc::expand_xlen(raw, self.xlen32) } else { Some(raw) } {
                    Some(insn) => {
                        let jit = jit::jit_form(insn, self.xlen32).is_some();
                        self.icache[slot] =
                            ICacheEntry { pc, gen: self.icache_gen, insn, raw, len: len as u8, prv: self.prv as u8, jit };
                        self.pc = pc.wrapping_add(len);
                        (self.exec(insn, raw), len as u8, jit)
                    }
                    None => (Err(illegal(raw)), 0, false),
                },
                Err(e) => (Err(e), 0, false),
            }
        };
        if JIT {
            self.block_start = r.is_err() || !compiled || self.pc != pc.wrapping_add(len as u64);
        }
        if let Err(e) = r {
            if e.cause == cause::ILLEGAL_INSN && self.log_illegal {
                eprintln!("rv64: illegal insn {:08x} at {pc:#x} (prv {})", e.tval, self.prv);
            }
            self.trap(e.cause, e.tval, false, pc);
        }
    }

    /// Timers and WFI handling between instructions. Returns Some(exit) if
    /// the hart can't make progress.
    pub fn housekeeping(&mut self) -> Option<RunExit> {
        match self.power {
            Power::On => {}
            Power::Off => return Some(RunExit::PowerOff),
            Power::Reset => return Some(RunExit::Reset),
        }
        if self.now_ns() >= self.next_timer_ns || self.csr.mip & MIP_MTIP != 0 {
            self.timer_update();
        }
        while self.waiting {
            if self.wake_pending() {
                self.waiting = false;
            } else if self.next_timer_ns != u64::MAX && !self.realtime {
                // Skip idle time straight to the next timer deadline.
                self.warp_ns += self.next_timer_ns.saturating_sub(self.now_ns());
                self.timer_update();
            } else {
                return Some(RunExit::Idle);
            }
        }
        None
    }

    /// Run up to `budget` steps (instructions or traps).
    pub fn run(&mut self, budget: u64) -> RunExit {
        let mut n = 0;
        while n < budget {
            if let Some(exit) = self.housekeeping() {
                return exit;
            }
            // Steps until the next timer deadline (see the aarch64 core).
            let mut limit = budget - n;
            if self.next_timer_ns != u64::MAX && self.ns_per_insn != 0 {
                let left = self.next_timer_ns.saturating_sub(self.now_ns());
                limit = limit.min(left.div_ceil(self.ns_per_insn).max(1));
            }
            let end = n + limit;
            self.stop_batch = false;
            if self.jit.enabled {
                while n < end && !self.stop_batch && !self.waiting {
                    if self.block_start {
                        if let Some(used) = self.jit_enter(end - n) {
                            n += used;
                            continue;
                        }
                    }
                    self.step_inner::<true>();
                    n += 1;
                }
            } else {
                while n < end && !self.stop_batch && !self.waiting {
                    self.step_inner::<false>();
                    n += 1;
                }
            }
        }
        RunExit::Budget
    }
}
