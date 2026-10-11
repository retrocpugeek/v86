//! CPU state, the step loop, exception entry/return and the generic timers.

use crate::bus::Bus;
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
    /// Exclusive monitor: (virtual address, size in bytes).
    pub excl: Option<(u64, u32)>,
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
            excl: None,
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
        self.el = 1;
        self.spsel = 1;
        self.il = false;
        // QEMU writes PSTATE = DAIF | mode on entry, clearing NZCV (they're in SPSR).
        self.nzcv = 0;
        self.daif = DAIF_D | DAIF_A | DAIF_I | DAIF_F;
        self.pc = self.sr.vbar_el1.wrapping_add(offset + vector);
        self.excl = None;
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
        self.excl = None;
        if m & 0x10 != 0 || (m >> 2) > 1 || m & 2 != 0 || (m >> 2 == 0 && m & 1 != 0) {
            // AArch32 or a non-existent EL: illegal exception return.
            self.il = true;
            return Ok(());
        }
        self.il = spsr & PSTATE_IL != 0;
        self.el = m >> 2;
        self.spsel = m & 1;
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
    pub fn step(&mut self) {
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
        let insn = match self.fetch(pc) {
            Ok(i) => i,
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
        if let Err(e) = self.exec(insn) {
            if e.ec == ec::UNKNOWN && self.log_undef {
                eprintln!("a64: undefined insn {insn:08x} at {pc:#x} (EL{})", self.el);
            }
            if e.ec == ec::DABT_LOWER || e.ec == ec::DABT_CUR {
                self.icount -= 1;
            }
            self.take_exception(e, pc);
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
            } else if self.next_timer_ns != u64::MAX {
                // Skip idle time straight to the next timer deadline.
                self.warp_ns += self.next_timer_ns.saturating_sub(self.now_ns());
                self.timer_update();
            } else {
                return Some(RunExit::Idle);
            }
        }
        None
    }

    /// Run up to `budget` steps (instructions, interrupts or faults).
    pub fn run(&mut self, budget: u64) -> RunExit {
        let mut n = 0;
        while n < budget {
            if let Some(exit) = self.housekeeping() {
                return exit;
            }
            // next_timer_ns is re-read every step since a timer register write may move it.
            while n < budget && self.now_ns() < self.next_timer_ns && !self.waiting && self.power == Power::On {
                self.step();
                n += 1;
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
