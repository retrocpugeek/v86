//! The CPU: ARMv7-M state (registers, the banked stack pointers, the
//! special registers, IT state), the run loop, and exception entry and
//! return (ARMv7-M ARM, B1.5). Instructions are in thumb.rs.

use crate::bus::Bus;
use crate::scs::{self, HARDFAULT, SVCALL, USAGEFAULT};

/// EXC_RETURN values start with this (in the top 8 bits, 0xFF)
const EXC_RETURN_PREFIX: u32 = 0xff00_0000;

/// Why run() stopped
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The budget is used up
    Budget,
    /// WFI with nothing pending: waiting for an interrupt (or an event)
    Idle,
    /// A breakpoint (BKPT, or a debugger's)
    Breakpoint,
    /// A lockup (a fault in a HardFault handler), or something unmodelled
    Halted,
    /// The firmware asked for a reset (AIRCR.SYSRESETREQ)
    Reset,
}

pub struct Cpu {
    /// r0..r12, and r14 (LR); r13 is msp/psp, r15 is pc
    pub r: [u32; 16],
    pub msp: u32,
    pub psp: u32,
    /// The address of the instruction being executed (or next to execute)
    pub pc: u32,
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
    pub q: bool,
    /// APSR.GE (SIMD comparisons)
    pub ge: u32,
    /// The current exception (IPSR); 0 in thread mode
    pub ipsr: usize,
    /// ITSTATE (EPSR bits [15:10] and [26:25], as IT[7:0])
    pub it: u8,
    pub primask: bool,
    pub faultmask: bool,
    pub basepri: u8,
    /// CONTROL: nPRIV (bit 0), SPSEL (bit 1), FPCA (bit 2)
    pub control: u32,
    /// LDREX/STREX's exclusive monitor
    pub excl: Option<u32>,
    pub bus: Bus,
    /// Instructions retired, and the time each takes (ns), as
    /// icount * ns_per_insn (one per clock cycle)
    pub icount: u64,
    pub ns_per_insn_x1000: u64,
    /// Time added while idle (WFI with nothing to do) (ns)
    pub warp_ns: u64,
    /// WFI / WFE: waiting
    pub sleeping: bool,
    pub event: bool,
    /// Something stopped the run (BKPT, lockup) and why
    pub stopped: Option<Stop>,
    /// The next instruction's address (set by the instruction executing)
    pub(crate) next_pc: u32,
    /// The instruction executing is in an IT block (16-bit data processing
    /// instructions don't set flags there)
    pub(crate) cur_in_it: bool,
    /// Breakpoints (a debugger's), and whether to stop on them
    pub breakpoints: Vec<u32>,
    /// When the bus next has something to do (ns)
    next_event: u64,
    /// Report instructions the core doesn't implement (once each)
    pub log_unimplemented: bool,
    pub unimplemented: Vec<u32>,
}

impl Cpu {
    pub fn new(bus: Bus) -> Cpu {
        let hz = bus.scs.cpu_hz.max(1);
        let mut cpu = Cpu {
            r: [0; 16],
            msp: 0,
            psp: 0,
            pc: 0,
            n: false,
            z: false,
            c: false,
            v: false,
            q: false,
            ge: 0,
            ipsr: 0,
            it: 0,
            primask: false,
            faultmask: false,
            basepri: 0,
            control: 0,
            excl: None,
            bus,
            icount: 0,
            ns_per_insn_x1000: 1_000_000_000_000 / hz,
            warp_ns: 0,
            sleeping: false,
            event: false,
            stopped: None,
            next_pc: 0,
            cur_in_it: false,
            breakpoints: Vec::new(),
            next_event: 0,
            log_unimplemented: true,
            unimplemented: Vec::new(),
        };
        cpu.reset();
        cpu
    }

    /// Reset: SP and PC from the vector table at VTOR (0 at reset).
    pub fn reset(&mut self) {
        self.bus.scs.vtor = 0;
        self.r = [0; 16];
        self.r[14] = 0xffff_ffff;
        self.msp = self.bus.read(0, 4) & !3;
        self.psp = 0;
        self.pc = self.bus.read(4, 4) & !1;
        self.control = 0;
        self.primask = false;
        self.faultmask = false;
        self.basepri = 0;
        self.ipsr = 0;
        self.it = 0;
        self.excl = None;
        self.sleeping = false;
    }

    #[inline]
    pub fn now_ns(&self) -> u64 { (self.icount as u128 * self.ns_per_insn_x1000 as u128 / 1000) as u64 + self.warp_ns }

    // ---- registers ----

    #[inline]
    pub fn handler_mode(&self) -> bool { self.ipsr != 0 }

    /// Is the process stack in use (thread mode with SPSEL)?
    #[inline]
    fn psp_selected(&self) -> bool { !self.handler_mode() && self.control & 2 != 0 }

    #[inline]
    pub fn sp(&self) -> u32 { if self.psp_selected() { self.psp } else { self.msp } }

    #[inline]
    pub fn set_sp(&mut self, v: u32) {
        if self.psp_selected() {
            self.psp = v & !3;
        } else {
            self.msp = v & !3;
        }
    }

    /// Register n as an instruction reads it (PC reads as this instruction + 4).
    #[inline]
    pub fn reg(&self, n: u32) -> u32 {
        match n {
            13 => self.sp(),
            15 => self.pc.wrapping_add(4),
            _ => self.r[n as usize],
        }
    }

    /// Write register n (not the PC: see branch_write / load_write_pc).
    #[inline]
    pub fn set_reg(&mut self, n: u32, v: u32) {
        match n {
            13 => self.set_sp(v),
            15 => self.branch_write(v),
            _ => self.r[n as usize] = v,
        }
    }

    /// A branch within Thumb code (B, ADD/MOV to PC): bit 0 ignored.
    #[inline]
    pub fn branch_write(&mut self, v: u32) { self.next_pc = v & !1; }

    /// BX, BLX, POP/LDM/LDR to PC: interworking, and exception return in
    /// handler mode.
    pub fn bx_write(&mut self, v: u32) {
        if self.handler_mode() && v & EXC_RETURN_PREFIX == EXC_RETURN_PREFIX {
            self.exception_return(v);
            return;
        }
        if v & 1 == 0 {
            // ARM state: a UsageFault (INVSTATE) on the next instruction
            self.next_pc = v;
            self.usage_fault_at(scs::UFSR_INVSTATE, v);
            return;
        }
        self.next_pc = v & !1;
    }

    pub fn xpsr(&self) -> u32 {
        (self.n as u32) << 31
            | (self.z as u32) << 30
            | (self.c as u32) << 29
            | (self.v as u32) << 28
            | (self.q as u32) << 27
            | ((self.it as u32) & 3) << 25
            | 1 << 24
            | self.ge << 16
            | ((self.it as u32) >> 2) << 10
            | self.ipsr as u32
    }

    pub fn set_apsr_nzcvq(&mut self, v: u32) {
        self.n = v >> 31 & 1 != 0;
        self.z = v >> 30 & 1 != 0;
        self.c = v >> 29 & 1 != 0;
        self.v = v >> 28 & 1 != 0;
        self.q = v >> 27 & 1 != 0;
    }

    pub fn set_xpsr(&mut self, v: u32) {
        self.set_apsr_nzcvq(v);
        self.ge = v >> 16 & 0xf;
        self.it = ((v >> 25 & 3) | (v >> 10 & 0x3f) << 2) as u8;
        self.ipsr = (v & 0x1ff) as usize;
    }

    // ---- condition codes and IT blocks ----

    pub fn cond(&self, c: u32) -> bool {
        let r = match c >> 1 {
            0 => self.z,
            1 => self.c,
            2 => self.n,
            3 => self.v,
            4 => self.c && !self.z,
            5 => self.n == self.v,
            6 => self.n == self.v && !self.z,
            _ => true,
        };
        if c & 1 != 0 && c != 0xf { !r } else { r }
    }

    #[inline]
    pub fn in_it(&self) -> bool { self.it & 0xf != 0 }

    /// The current IT condition, inside an IT block.
    #[inline]
    pub fn it_cond(&self) -> u32 { (self.it >> 4) as u32 }

    /// Advance ITSTATE past an instruction.
    #[inline]
    pub fn it_advance(&mut self) {
        if self.it & 7 == 0 {
            self.it = 0;
        } else {
            self.it = (self.it & 0xe0) | ((self.it << 1) & 0x1f);
        }
    }

    // ---- memory ----

    #[inline]
    pub fn read(&mut self, a: u32, size: u32) -> u32 { self.bus.read(a, size) }

    #[inline]
    pub fn write(&mut self, a: u32, size: u32, v: u32) {
        if let Some(e) = self.excl {
            if a & !7 == e & !7 {
                self.excl = None;
            }
        }
        self.bus.write(a, size, v);
    }

    // ---- exceptions ----

    /// The current execution priority: active exceptions, BASEPRI, PRIMASK,
    /// FAULTMASK.
    pub fn exec_prio(&self) -> i32 {
        let scs = &self.bus.scs;
        let mut p = scs.active_prio();
        if self.basepri != 0 {
            p = p.min(scs.group_prio(self.basepri as i32));
        }
        if self.primask {
            p = p.min(0);
        }
        if self.faultmask {
            p = p.min(-1);
        }
        p
    }

    /// Take the highest-priority pending exception if it preempts what's
    /// running. Returns whether one was taken.
    pub fn check_interrupts(&mut self) -> bool {
        let Some((n, p)) = self.bus.scs.highest_pending() else { return false };
        if p >= self.exec_prio() {
            return false;
        }
        self.sleeping = false;
        // (the instruction at pc hasn't executed: it's where we return)
        let ret = self.pc;
        self.exception_entry(n, ret);
        true
    }

    /// Enter exception n, returning (later) to `ret` (ARMv7-M B1.5.6).
    pub fn exception_entry(&mut self, n: usize, ret: u32) {
        let scs_ccr_stkalign = self.bus.scs.ccr & 1 << 9 != 0;
        let sp = self.sp();
        let force_align = scs_ccr_stkalign && sp & 4 != 0;
        let frame = (sp.wrapping_sub(0x20)) & if force_align { !7 } else { !3 };
        let xpsr = (self.xpsr() & !(1 << 9)) | (force_align as u32) << 9;
        let words = [self.r[0], self.r[1], self.r[2], self.r[3], self.r[12], self.r[14], ret, xpsr];
        for (i, w) in words.iter().enumerate() {
            self.bus.write(frame + 4 * i as u32, 4, *w);
        }
        self.set_sp(frame);
        self.r[14] = if self.handler_mode() {
            0xffff_fff1
        } else if self.control & 2 != 0 {
            0xffff_fffd
        } else {
            0xffff_fff9
        };
        let vtor = self.bus.scs.vtor;
        let handler = self.bus.read(vtor + 4 * n as u32, 4);
        self.ipsr = n;
        self.it = 0;
        self.control &= !2;
        self.excl = None;
        self.bus.scs.pending[n] = false;
        self.bus.scs.active[n] = true;
        self.bus.scs.ipsr = n;
        self.bus.scs.dirty = true;
        self.pc = handler & !1;
        self.next_pc = self.pc;
    }

    /// Return from an exception: EXC_RETURN in `exc_return`
    /// (ARMv7-M B1.5.8).
    pub fn exception_return(&mut self, exc_return: u32) {
        let n = self.ipsr;
        let mode = exc_return & 0xf;
        if !self.bus.scs.active[n] || !matches!(mode, 0x1 | 0x9 | 0xd) {
            self.usage_fault_at(scs::UFSR_INVPC, self.pc);
            return;
        }
        self.bus.scs.deactivate(n);
        // FAULTMASK clears on return from anything but NMI
        if n != scs::NMI {
            self.faultmask = false;
        }
        let to_thread = mode != 0x1;
        // Pop from the stack the return selects
        let frame = if mode == 0xd { self.psp } else { self.msp };
        let mut w = [0u32; 8];
        for (i, x) in w.iter_mut().enumerate() {
            *x = self.bus.read(frame + 4 * i as u32, 4);
        }
        self.r[0] = w[0];
        self.r[1] = w[1];
        self.r[2] = w[2];
        self.r[3] = w[3];
        self.r[12] = w[4];
        self.r[14] = w[5];
        let ret = w[6];
        let xpsr = w[7];
        let new_sp = frame + 0x20 + if xpsr & 1 << 9 != 0 { 4 } else { 0 };
        if mode == 0xd {
            self.psp = new_sp;
            self.control |= 2;
        } else {
            self.msp = new_sp;
            if to_thread {
                self.control &= !2;
            }
        }
        self.set_xpsr(xpsr);
        // (the frame's IPSR: 0 for thread mode; else the exception returned to)
        if to_thread {
            self.ipsr = 0;
        } else if self.ipsr == 0 {
            // Returning to handler mode with IPSR 0 in the frame: INVPC
            self.ipsr = n;
        }
        self.bus.scs.ipsr = self.ipsr;
        self.excl = None;
        self.pc = ret & !1;
        self.next_pc = self.pc;
    }

    /// A fault (MemManage, BusFault, UsageFault), escalated to HardFault if
    /// its handler is disabled or can't preempt; at `ret` (the faulting
    /// instruction).
    pub fn fault(&mut self, n: usize, cfsr: u32, ret: u32) {
        self.bus.scs.cfsr |= cfsr;
        let mut target = n;
        if n != HARDFAULT {
            let prio = self.bus.scs.group_prio(self.bus.scs.exc_prio(n));
            if !self.bus.scs.is_fault_enabled(n) || prio >= self.exec_prio() {
                target = HARDFAULT;
                self.bus.scs.hfsr |= scs::HFSR_FORCED;
            }
        }
        if target == HARDFAULT && self.exec_prio() <= -1 {
            // A fault at HardFault priority or above: lockup
            eprintln!("cm: lockup: fault {n} (CFSR {:#x}) at {ret:#010x} while in exception {}", self.bus.scs.cfsr, self.ipsr);
            self.stopped = Some(Stop::Halted);
            return;
        }
        self.exception_entry(target, ret);
    }

    pub fn usage_fault_at(&mut self, cfsr: u32, at: u32) { self.fault(USAGEFAULT, cfsr, at) }

    /// An undefined instruction (or one not implemented here).
    pub fn undefined(&mut self, insn: u32) {
        if self.log_unimplemented && !self.unimplemented.contains(&insn) {
            self.unimplemented.push(insn);
            eprintln!("cm: undefined instruction {insn:#010x} at {:#010x}", self.pc);
        }
        let at = self.pc;
        self.usage_fault_at(scs::UFSR_UNDEFINSTR, at);
    }

    /// SVC: SVCall, taken at once (it's synchronous): returning to the next
    /// instruction.
    pub fn svc(&mut self) {
        let ret = self.next_pc;
        let prio = self.bus.scs.group_prio(self.bus.scs.exc_prio(SVCALL));
        if prio >= self.exec_prio() {
            self.fault(HARDFAULT, 0, self.pc);
            self.bus.scs.hfsr |= scs::HFSR_FORCED;
            return;
        }
        self.exception_entry(SVCALL, ret);
    }

    // ---- running ----

    /// Run up to `budget` instructions.
    pub fn run(&mut self, budget: u64) -> Stop {
        let end = self.icount + budget;
        self.stopped = None;
        self.next_event = self.bus.next_event_ns();
        while self.icount < end {
            self.service();
            if self.bus.scs.reset_requested {
                self.bus.scs.reset_requested = false;
                return Stop::Reset;
            }
            if self.sleeping {
                if !self.wake_check() {
                    // Waiting for an interrupt: the host decides how long to
                    // wait (skip_idle, or real time)
                    return Stop::Idle;
                }
                continue;
            }
            if !self.breakpoints.is_empty() && self.breakpoints.contains(&self.pc) {
                return Stop::Breakpoint;
            }
            self.bus.now_ns = self.now_ns();
            self.bus.cycles = self.icount;
            self.step();
            self.icount += 1;
            if let Some(s) = self.stopped.take() {
                return s;
            }
        }
        Stop::Budget
    }

    /// Bring devices and SysTick up to now and take a pending interrupt, if
    /// anything changed (before each instruction).
    #[inline]
    pub fn service(&mut self) {
        let now = self.now_ns();
        if now >= self.next_event || self.bus.scs.dirty {
            self.bus.update(now);
            self.next_event = self.bus.next_event_ns();
            self.bus.scs.dirty = false;
            self.check_interrupts();
        }
    }

    /// Sleeping (WFI): move the clock on to the next device event, and take
    /// the interrupt it raises. Returns false if nothing is scheduled.
    pub fn skip_idle(&mut self) -> bool {
        let next = self.bus.next_event_ns();
        if next == u64::MAX {
            return false;
        }
        let now = self.now_ns();
        if next > now {
            self.warp_ns += next - now;
        }
        self.bus.update(self.now_ns());
        self.bus.scs.dirty = true;
        self.wake_check();
        true
    }

    /// WFI wakes for a pending exception that would preempt were PRIMASK
    /// clear (B1.5.19): a masked one wakes the core without being taken.
    pub fn wake_check(&mut self) -> bool {
        if self.check_interrupts() {
            return true;
        }
        let Some((_, p)) = self.bus.scs.highest_pending() else { return false };
        let saved = self.primask;
        self.primask = false;
        let wake = p < self.exec_prio();
        self.primask = saved;
        if wake {
            self.sleeping = false;
        }
        wake
    }

    /// Execute one instruction (or take a fault instead).
    pub fn step(&mut self) {
        let pc = self.pc;
        let hw1 = self.bus.fetch16(pc) as u32;
        let wide = hw1 >> 11 >= 0b11101;
        let insn = if wide { hw1 << 16 | self.bus.fetch16(pc + 2) as u32 } else { hw1 };
        self.next_pc = pc + if wide { 4 } else { 2 };
        self.cur_in_it = self.in_it();
        if self.cur_in_it {
            let c = self.it_cond();
            // (IT itself never executes inside a block)
            self.it_advance();
            if !self.cond(c) {
                self.pc = self.next_pc;
                return;
            }
        }
        if wide {
            self.exec32(insn);
        } else {
            self.exec16(insn);
        }
        self.pc = self.next_pc;
    }
}
