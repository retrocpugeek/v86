//! CPU state, the step loop, exception entry/return and the generic timers.

use crate::bus::Bus;
use crate::decode::{generic, DCache, Op};
use crate::jit::Jit;
use crate::exc::{undef, ec, Exception, Res};
use crate::gic::{PPI_PTIMER, PPI_VTIMER};
use crate::mmu::Tlb;
use crate::sysregs::SysRegs;

pub const PSTATE_N: u32 = 1 << 31;
pub const PSTATE_Z: u32 = 1 << 30;
pub const PSTATE_C: u32 = 1 << 29;
pub const PSTATE_V: u32 = 1 << 28;
pub const DAIF_D: u32 = 1 << 9;
pub const DAIF_A: u32 = 1 << 8;
pub const DAIF_I: u32 = 1 << 7;
pub const DAIF_F: u32 = 1 << 6;
pub const PSTATE_IL: u64 = 1 << 20;

/// Generic timer frequency; QEMU's virt machine uses 62.5 MHz.
pub const CNTFRQ: u64 = 62_500_000;
/// Largest step sync_host_ns() moves virtual time forward by.
pub const MAX_WARP_NS: u64 = 1_000_000_000;
/// Nanoseconds per counter tick (1e9 / CNTFRQ).
pub const NS_PER_TICK: u64 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Power {
    On,
    Off,
    Reset,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunExit {
    /// Executed the requested number of instructions.
    Budget,
    /// Waiting for an interrupt with no timer armed; the host should wait for input.
    Idle,
    PowerOff,
    Reset,
}

pub struct Cpu {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub sp_el1: u64,
    pub pc: u64,
    /// NZCV in bits 31..28.
    pub nzcv: u32,
    /// DAIF in bits 9..6.
    pub daif: u32,
    pub el: u32,
    pub spsel: u32,
    pub il: bool,
    pub v: [u128; 32],
    pub fpcr: u32,
    pub fpsr: u32,
    pub sr: SysRegs,
    pub bus: Bus,
    pub tlb: Tlb,
    /// Exclusive monitor: virtual address and size in bytes (0: open).
    pub excl_addr: u64,
    pub excl_size: u32,
    /// Completed instructions. With `ns_per_insn` this drives virtual time.
    pub icount: u64,
    /// Virtual nanoseconds skipped while idle (WFI).
    pub warp_ns: u64,
    pub ns_per_insn: u64,
    pub waiting: bool,
    pub power: Power,
    pub next_timer_ns: u64,
    /// Address of the instruction being executed.
    pub insn_pc: u64,
    pub log_undef: bool,
    /// Take deliverable IRQs in step(). Lockstep turns this off and injects
    /// interrupts where the reference did.
    pub auto_irq: bool,
    /// icount at the first return to EL0 (diagnostics).
    pub first_el0: Option<u64>,
    /// Follow host time (browser): idle time is waited out by the host rather
    /// than skipped, and sync_host_ns() pulls virtual time up to the host clock.
    pub realtime: bool,
    host_base_ns: Option<u64>,
    /// Ends run()'s current batch of steps: set when the timer deadline,
    /// WFI state or power state may have changed.
    pub stop_batch: bool,
    pub dcache: DCache,
    /// Decoded page for the instruction fetch cache's page (Tlb::fetch_page);
    /// set together with it in fetch_slow.
    pub(crate) dcur: *mut Op,
    pub jit: Jit,
    /// JIT state of the fetch page's slots (DCache::jit_page), like dcur.
    pub(crate) djit: *mut u32,
    /// Set by jit_read/jit_write for compiled code (see jit.rs).
    pub jit_flag: u32,
    pub jit_exc: Exception,
    /// Calls compiled code may still chain to another page's code (see jit.rs).
    pub jit_hops: u32,
    /// The next instruction starts a block: the last one branched, raised an
    /// exception or had no handler of its own (see jit.rs).
    pub block_start: bool,
}

impl Cpu {
    pub fn new(bus: Bus) -> Cpu {
        Cpu {
            x: [0; 31],
            sp_el0: 0,
            sp_el1: 0,
            pc: 0,
            // QEMU resets to EL1h with DAIF masked and Z set.
            nzcv: PSTATE_Z,
            daif: DAIF_D | DAIF_A | DAIF_I | DAIF_F,
            el: 1,
            spsel: 1,
            il: false,
            v: [0; 32],
            fpcr: 0,
            fpsr: 0,
            sr: SysRegs::new(),
            bus,
            tlb: Tlb::new(),
            excl_addr: 0,
            excl_size: 0,
            icount: 0,
            warp_ns: 0,
            ns_per_insn: 1,
            waiting: false,
            power: Power::On,
            next_timer_ns: u64::MAX,
            insn_pc: 0,
            log_undef: true,
            auto_irq: true,
            first_el0: None,
            realtime: false,
            host_base_ns: None,
            stop_batch: false,
            dcache: DCache::default(),
            dcur: std::ptr::null_mut(),
            jit: Jit::default(),
            djit: std::ptr::null_mut(),
            jit_flag: 0,
            jit_exc: Exception::undef(),
            jit_hops: 0,
            block_start: true,
        }
    }

    // ---- registers ----

    #[inline(always)]
    pub fn xr(&self, n: u32) -> u64 { if n == 31 { 0 } else { self.x[n as usize] } }

    #[inline(always)]
    pub fn set_xr(&mut self, n: u32, v: u64) {
        if n != 31 {
            self.x[n as usize] = v
        }
    }

    /// Write a 32- or 64-bit result (W writes zero-extend).
    #[inline(always)]
    pub fn set_xr_sf(&mut self, n: u32, v: u64, sf: bool) { self.set_xr(n, if sf { v } else { v as u32 as u64 }) }

    #[inline(always)]
    pub fn sp(&self) -> u64 { if self.el != 0 && self.spsel != 0 { self.sp_el1 } else { self.sp_el0 } }

    #[inline(always)]
    pub fn set_sp(&mut self, v: u64) {
        if self.el != 0 && self.spsel != 0 {
            self.sp_el1 = v
        } else {
            self.sp_el0 = v
        }
    }

    /// Register or SP for n == 31.
    #[inline(always)]
    pub fn xs(&self, n: u32) -> u64 { if n == 31 { self.sp() } else { self.x[n as usize] } }

    #[inline(always)]
    pub fn set_xs(&mut self, n: u32, v: u64) {
        if n == 31 {
            self.set_sp(v)
        } else {
            self.x[n as usize] = v
        }
    }

    pub fn pstate(&self) -> u64 {
        self.nzcv as u64
            | self.daif as u64
            | ((self.el << 2) | self.spsel) as u64
            | if self.il { PSTATE_IL } else { 0 }
    }

    // ---- time ----

    #[inline(always)]
    pub fn now_ns(&self) -> u64 { self.icount * self.ns_per_insn + self.warp_ns }

    pub fn counter(&self) -> u64 { self.now_ns() / NS_PER_TICK }

    /// Recompute timer outputs and the next deadline. Call after any timer
    /// register write and whenever `now_ns()` passes `next_timer_ns`.
    pub fn timer_update(&mut self) {
        let cnt = self.counter();
        let mut next = u64::MAX;
        for (ctl, cval, ppi) in [
            (self.sr.cntv_ctl, self.sr.cntv_cval, PPI_VTIMER),
            (self.sr.cntp_ctl, self.sr.cntp_cval, PPI_PTIMER),
        ] {
            let enabled = ctl & 1 != 0;
            let fired = enabled && cnt >= cval;
            self.bus.gic.set_irq(ppi, fired && ctl & 2 == 0);
            if enabled && !fired {
                next = next.min(cval.saturating_mul(NS_PER_TICK));
            }
        }
        self.next_timer_ns = next;
        self.stop_batch = true;
    }

    pub fn timer_istatus(&self, ctl: u64, cval: u64) -> u64 {
        if ctl & 1 != 0 && self.counter() >= cval { 4 } else { 0 }
    }

    // ---- exceptions ----

    fn enter_el1(&mut self, ret: u64, vector: u64) {
        let offset = if self.el == 0 {
            0x400
        } else if self.spsel != 0 {
            0x200
        } else {
            0
        };
        self.sr.spsr_el1 = self.pstate();
        self.sr.elr_el1 = ret;
        self.block_start = true;
        self.el = 1;
        self.tlb.invalidate_fetch();
        self.spsel = 1;
        self.il = false;
        // QEMU writes PSTATE = DAIF | mode on entry, clearing NZCV (they're in SPSR).
        self.nzcv = 0;
        self.daif = DAIF_D | DAIF_A | DAIF_I | DAIF_F;
        self.pc = self.sr.vbar_el1.wrapping_add(offset + vector);
        self.excl_size = 0;
    }

    pub fn take_exception(&mut self, e: Exception, pc: u64) {
        let ret = if e.ret_next { pc.wrapping_add(4) } else { pc };
        // IL is set for every A64 instruction-sourced exception except "unknown" from a 16-bit insn.
        self.sr.esr_el1 = ((e.ec as u64) << 26) | (1 << 25) | (e.iss as u64 & 0x1ff_ffff);
        if let Some(far) = e.far {
            self.sr.far_el1 = far;
        }
        self.enter_el1(ret, 0);
    }

    pub fn take_irq(&mut self) {
        self.waiting = false;
        let pc = self.pc;
        self.enter_el1(pc, 0x80);
    }

    pub fn eret(&mut self) -> Res<()> {
        if self.el == 0 {
            return undef();
        }
        let spsr = self.sr.spsr_el1;
        let m = (spsr & 0x1f) as u32;
        self.pc = self.sr.elr_el1;
        self.nzcv = (spsr as u32) & 0xf000_0000;
        self.daif = (spsr as u32) & 0x3c0;
        self.excl_size = 0;
        if m & 0x10 != 0 || (m >> 2) > 1 || m & 2 != 0 || (m >> 2 == 0 && m & 1 != 0) {
            // AArch32 or a non-existent EL: illegal exception return.
            self.il = true;
            return Ok(());
        }
        self.il = spsr & PSTATE_IL != 0;
        self.el = m >> 2;
        self.spsel = m & 1;
        self.tlb.invalidate_fetch();
        if self.el == 0 && self.first_el0.is_none() {
            self.first_el0 = Some(self.icount);
        }
        Ok(())
    }

    // ---- execution ----

    #[inline]
    pub fn irq_deliverable(&self) -> bool { self.bus.gic.irq_out && self.daif & DAIF_I == 0 }

    /// Execute one instruction (or take a pending interrupt).
    #[inline]
    pub fn step(&mut self) { self.step_inner::<false>() }

    /// step(); with JIT, also tracks block starts.
    #[inline(always)]
    fn step_inner<const JIT: bool>(&mut self) {
        if self.auto_irq && self.irq_deliverable() {
            self.take_irq();
            return;
        }
        let pc = self.pc;
        self.insn_pc = pc;
        if self.il {
            self.take_exception(Exception::new(ec::ILLEGAL_STATE, 0), pc);
            return;
        }
        let op = match self.fetch_op(pc) {
            Ok(op) => op,
            Err(e) => {
                self.take_exception(e, pc);
                return;
            }
        };
        self.pc = pc.wrapping_add(4);
        // Like QEMU's icount, the instruction is counted before it executes (a
        // counter read sees itself). QEMU refunds it only for memory aborts
        // (state restored from the TB); UNDEF, traps, SVC and BRK still count.
        self.icount += 1;
        if let Err(e) = (op.f)(self, &op) {
            if e.ec == ec::UNKNOWN && self.log_undef {
                let insn = op.insn;
                eprintln!("a64: undefined insn {insn:08x} at {pc:#x} (EL{})", self.el);
            }
            if e.ec == ec::DABT_LOWER || e.ec == ec::DABT_CUR {
                self.icount -= 1;
            }
            self.take_exception(e, pc);
            return;
        }
        if JIT {
            self.block_start = self.pc != pc.wrapping_add(4) || op.f as usize == generic as *const () as usize;
        }
    }

    /// Timers and WFI handling between instructions. Returns Some(exit) if
    /// the CPU can't make progress (powered off, or idle with no timer armed).
    pub fn housekeeping(&mut self) -> Option<RunExit> {
        match self.power {
            Power::On => {}
            Power::Off => return Some(RunExit::PowerOff),
            Power::Reset => return Some(RunExit::Reset),
        }
        if self.now_ns() >= self.next_timer_ns {
            self.timer_update();
        }
        while self.waiting {
            if self.bus.gic.irq_out {
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

    /// Advance virtual time to at least the host clock (realtime mode). Virtual
    /// time also advances by ns_per_insn per instruction between calls.
    ///
    /// A jump of more than MAX_WARP_NS means the host didn't run us (browsers
    /// freeze or throttle background tabs): virtual time then only moves by
    /// MAX_WARP_NS and the rest is dropped, as if the machine was paused.
    /// Otherwise the guest sees the whole gap pass in one timer tick and the
    /// kernel reports RCU stalls and soft lockups.
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

    /// Nanoseconds until the next armed timer fires, if any.
    pub fn ns_until_timer(&self) -> Option<u64> {
        (self.next_timer_ns != u64::MAX).then(|| self.next_timer_ns.saturating_sub(self.now_ns()))
    }

    /// Run up to `budget` steps (instructions, interrupts or faults).
    pub fn run(&mut self, budget: u64) -> RunExit {
        let mut n = 0;
        while n < budget {
            if let Some(exit) = self.housekeeping() {
                return exit;
            }
            // Steps until the next timer deadline: icount (and with it
            // virtual time) advances by at most one instruction per step, so
            // this never runs past it. Anything that may move the deadline,
            // or stop the CPU, sets stop_batch.
            let mut limit = budget - n;
            if self.next_timer_ns != u64::MAX && self.ns_per_insn != 0 {
                let left = self.next_timer_ns.saturating_sub(self.now_ns());
                limit = limit.min(left.div_ceil(self.ns_per_insn).max(1));
            }
            let end = n + limit;
            self.stop_batch = false;
            if self.jit.enabled {
                while n < end && !self.stop_batch {
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
                while n < end && !self.stop_batch {
                    self.step_inner::<false>();
                    n += 1;
                }
            }
        }
        RunExit::Budget
    }

    pub fn exec(&mut self, insn: u32) -> Res<()> {
        match (insn >> 25) & 0xf {
            0b1000 | 0b1001 => self.exec_dp_imm(insn),
            0b1010 | 0b1011 => self.exec_branch_sys(insn),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.exec_ldst(insn),
            0b0101 | 0b1101 => self.exec_dp_reg(insn),
            0b0111 | 0b1111 => self.exec_simd(insn),
            _ => undef(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_time_pause_is_not_passed_on() {
        let mut c = Cpu::new(crate::bus::Bus::new(0x4000_0000, 0x1000));
        c.realtime = true;
        c.sync_host_ns(5_000_000_000);
        let t0 = c.now_ns();
        // Normal progress is followed
        c.sync_host_ns(5_300_000_000);
        assert_eq!(c.now_ns() - t0, 300_000_000);
        // A 90 s pause (frozen tab) moves virtual time by at most MAX_WARP_NS
        c.sync_host_ns(95_300_000_000);
        assert_eq!(c.now_ns() - t0, 300_000_000 + MAX_WARP_NS);
        // ... and following progress is relative to the new base
        c.sync_host_ns(95_500_000_000);
        assert_eq!(c.now_ns() - t0, 500_000_000 + MAX_WARP_NS);
    }
}
