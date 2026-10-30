//! The System Control Space (0xE000_E000..0xE000_F000) of an ARMv7-M core:
//! the NVIC, the System Control Block, SysTick and the MPU's registers, and
//! the exception state they share with the CPU: which exceptions are
//! enabled, pending and active, and their priorities.
//!
//! Following the ARMv7-M Architecture Reference Manual (DDI 0403E), with the
//! implementation choices of QEMU's armv7m model where the firmware can tell:
//! 3 priority bits (as Pebble's machines configure it), 32 external
//! interrupts, the Cortex-M4's CPUID. The MPU's registers hold what's written
//! but don't restrict accesses (yet).

/// Exception numbers
pub const RESET: usize = 1;
pub const NMI: usize = 2;
pub const HARDFAULT: usize = 3;
pub const MEMMANAGE: usize = 4;
pub const BUSFAULT: usize = 5;
pub const USAGEFAULT: usize = 6;
pub const SVCALL: usize = 11;
pub const DEBUGMON: usize = 12;
pub const PENDSV: usize = 14;
pub const SYSTICK: usize = 15;
/// External interrupts: exception 16 + n
pub const NUM_IRQS: usize = 32;
pub const NUM_EXC: usize = 16 + NUM_IRQS;

pub const SCS_BASE: u32 = 0xe000_e000;
/// Debug and trace components the firmware may touch: DWT (its cycle
/// counter), ITM, FPB; and the debug registers in the SCS (DEMCR)
pub const DWT_BASE: u32 = 0xe000_1000;

/// Priority bits implemented (the top bits of each 8-bit priority field)
const PRIO_BITS: u32 = 3;
const PRIO_MASK: u8 = (0xff << (8 - PRIO_BITS)) as u8;

// CFSR bits (UsageFault in the top half)
pub const UFSR_UNDEFINSTR: u32 = 1 << 16;
pub const UFSR_INVSTATE: u32 = 1 << 17;
pub const UFSR_INVPC: u32 = 1 << 18;
pub const UFSR_NOCP: u32 = 1 << 19;
pub const UFSR_UNALIGNED: u32 = 1 << 24;
pub const UFSR_DIVBYZERO: u32 = 1 << 25;
// HFSR
pub const HFSR_FORCED: u32 = 1 << 30;

pub struct Scs {
    pub pending: [bool; NUM_EXC],
    pub active: [bool; NUM_EXC],
    /// External interrupts' enables (NVIC ISER)
    pub enabled: [bool; NUM_IRQS],
    /// External interrupt lines from devices (level): pending follows a
    /// rising line, and a line still high when its handler returns
    pub lines: [bool; NUM_IRQS],
    /// Priorities of exceptions 4..15 (SHPR) and of the external interrupts
    pub prio: [u8; NUM_EXC],
    pub vtor: u32,
    pub aircr_prigroup: u32,
    pub scr: u32,
    pub ccr: u32,
    /// SHCSR: the fault handlers' enables (MemManage, BusFault, UsageFault)
    pub shcsr: u32,
    pub cfsr: u32,
    pub hfsr: u32,
    pub mmfar: u32,
    pub bfar: u32,
    pub cpacr: u32,
    /// The exception being handled (IPSR), kept here for ICSR
    pub ipsr: usize,
    pub reset_requested: bool,
    // SysTick
    pub stk_ctrl: u32,
    pub stk_load: u32,
    /// The counter was stk_val0 at stk_t0 (ns)
    stk_val0: u32,
    stk_t0: u64,
    /// When the counter next wraps (ns), or u64::MAX
    pub stk_next: u64,
    pub cpu_hz: u64,
    pub ref_hz: u64,
    // MPU
    pub mpu_ctrl: u32,
    pub mpu_rnr: u32,
    pub mpu_rbar: [u32; 8],
    pub mpu_rasr: [u32; 8],
    // DWT and debug
    pub demcr: u32,
    pub dwt_ctrl: u32,
    /// The cycle counter was dwt_cyc0 at cycle count dwt_t0 (in CPU cycles)
    dwt_cyc0: u32,
    dwt_t0: u64,
    pub fpccr: u32,
    pub fpcar: u32,
    pub fpdscr: u32,
    /// Something pending_exception() depends on changed
    pub dirty: bool,
}

impl Scs {
    pub fn new(cpu_hz: u64, ref_hz: u64) -> Scs {
        Scs {
            pending: [false; NUM_EXC],
            active: [false; NUM_EXC],
            enabled: [false; NUM_IRQS],
            lines: [false; NUM_IRQS],
            prio: [0; NUM_EXC],
            vtor: 0,
            aircr_prigroup: 0,
            scr: 0,
            // STKALIGN set (the Cortex-M4's reset value)
            ccr: 1 << 9,
            shcsr: 0,
            cfsr: 0,
            hfsr: 0,
            mmfar: 0,
            bfar: 0,
            cpacr: 0,
            ipsr: 0,
            reset_requested: false,
            stk_ctrl: 0,
            stk_load: 0,
            stk_val0: 0,
            stk_t0: 0,
            stk_next: u64::MAX,
            cpu_hz,
            ref_hz,
            mpu_ctrl: 0,
            mpu_rnr: 0,
            mpu_rbar: [0; 8],
            mpu_rasr: [0; 8],
            demcr: 0,
            dwt_ctrl: 0x4000_0000,
            dwt_cyc0: 0,
            dwt_t0: 0,
            fpccr: 0xc000_0000,
            fpcar: 0,
            fpdscr: 0,
            dirty: true,
        }
    }

    // ---- priorities ----

    /// An exception's priority: fixed for reset, NMI and HardFault.
    pub fn exc_prio(&self, n: usize) -> i32 {
        match n {
            RESET => -3,
            NMI => -2,
            HARDFAULT => -1,
            _ => self.prio[n] as i32,
        }
    }

    /// The group priority (PRIGROUP splits off the subpriority's low bits).
    pub fn group_prio(&self, p: i32) -> i32 {
        if p < 0 {
            return p;
        }
        let mask = !((2u32 << self.aircr_prigroup) - 1) & 0xff;
        (p as u32 & mask) as i32
    }

    /// The pending exception with the highest priority (lowest priority value,
    /// then lowest number) that is enabled, and its group priority.
    pub fn highest_pending(&self) -> Option<(usize, i32)> {
        let mut best: Option<(usize, i32)> = None;
        for n in 1..NUM_EXC {
            if !self.pending[n] {
                continue;
            }
            if n >= 16 && !self.enabled[n - 16] {
                continue;
            }
            let p = self.exc_prio(n);
            if best.map_or(true, |(_, bp)| p < bp) {
                best = Some((n, p));
            }
        }
        best.map(|(n, p)| (n, self.group_prio(p)))
    }

    /// The priority of what's active: the highest-priority active exception's
    /// group priority, or 256 (thread mode, nothing active).
    pub fn active_prio(&self) -> i32 {
        let mut prio = 256;
        for n in 1..NUM_EXC {
            if self.active[n] {
                prio = prio.min(self.group_prio(self.exc_prio(n)));
            }
        }
        prio
    }

    /// Is any exception other than `n` active? (RETTOBASE)
    fn others_active(&self, n: usize) -> bool { (1..NUM_EXC).any(|i| i != n && self.active[i]) }

    // ---- interrupt lines ----

    /// A device's interrupt line: a rising edge (or a high level, when it
    /// isn't active) pends it.
    pub fn set_line(&mut self, irq: usize, level: bool) {
        if irq >= NUM_IRQS {
            return;
        }
        if level && !self.lines[irq] && !self.active[16 + irq] {
            self.pending[16 + irq] = true;
            self.dirty = true;
        }
        self.lines[irq] = level;
    }

    /// An exception handler returned: a level-sensitive interrupt whose line
    /// is still high pends again.
    pub fn deactivate(&mut self, n: usize) {
        self.active[n] = false;
        if n >= 16 && self.lines[n - 16] {
            self.pending[n] = true;
        }
        self.dirty = true;
    }

    // ---- SysTick ----

    fn stk_rate(&self) -> u64 { if self.stk_ctrl & 4 != 0 { self.cpu_hz } else { self.ref_hz } }

    /// The counter's value now (it counts down from LOAD to 0, then reloads).
    fn stk_value(&self, now: u64) -> u32 {
        if self.stk_ctrl & 1 == 0 || self.stk_load == 0 {
            return self.stk_val0;
        }
        let ticks = (now.saturating_sub(self.stk_t0) as u128 * self.stk_rate() as u128 / 1_000_000_000) as u64;
        let period = self.stk_load as u64 + 1;
        if ticks <= self.stk_val0 as u64 {
            self.stk_val0 - ticks as u32
        } else {
            let past = (ticks - self.stk_val0 as u64 - 1) % period;
            (self.stk_load as u64 - past) as u32
        }
    }

    /// When the counter next reaches 0 (from 1): its interrupt. From 0 it
    /// first reloads LOAD on the next tick, so that's LOAD + 1 ticks on.
    fn stk_schedule(&mut self) {
        self.stk_next = if self.stk_ctrl & 1 == 0 || self.stk_load == 0 {
            u64::MAX
        } else {
            let ticks = if self.stk_val0 > 0 { self.stk_val0 as u128 } else { self.stk_load as u128 + 1 };
            self.stk_t0 + (ticks * 1_000_000_000).div_ceil(self.stk_rate().max(1) as u128) as u64
        };
    }

    /// Bring SysTick up to `now`: each time the counter reaches 0 it sets
    /// COUNTFLAG and, with TICKINT, pends its exception.
    pub fn update(&mut self, now: u64) {
        while now >= self.stk_next {
            self.stk_t0 = self.stk_next;
            self.stk_val0 = 0;
            self.stk_ctrl |= 1 << 16;
            if self.stk_ctrl & 2 != 0 {
                self.pending[SYSTICK] = true;
                self.dirty = true;
            }
            self.stk_schedule();
        }
    }

    fn stk_restart(&mut self, now: u64, val: u32) {
        self.stk_val0 = val;
        self.stk_t0 = now;
        self.stk_schedule();
    }

    // ---- registers ----

    /// A word read; `now` in ns and `cycles` for the DWT counter.
    pub fn read(&mut self, a: u32, now: u64, cycles: u64) -> u32 {
        if a & !0xfff == DWT_BASE {
            return match a - DWT_BASE {
                0x000 => self.dwt_ctrl,
                0x004 => self.dwt_cyccnt(cycles),
                _ => 0,
            };
        }
        let off = a.wrapping_sub(SCS_BASE);
        self.update(now);
        match off {
            // ICTR: 32 interrupts
            0x004 => 0,
            0x010 => {
                let v = self.stk_ctrl;
                // COUNTFLAG clears on read
                self.stk_ctrl &= !(1 << 16);
                v
            }
            0x014 => self.stk_load,
            0x018 => self.stk_value(now),
            // CALIB: no reference value
            0x01c => 1 << 31,
            0x100..=0x13c => self.word_of(off - 0x100, |s, i| s.enabled[i]),
            0x180..=0x1bc => self.word_of(off - 0x180, |s, i| s.enabled[i]),
            0x200..=0x23c => self.word_of(off - 0x200, |s, i| s.pending[16 + i]),
            0x280..=0x2bc => self.word_of(off - 0x280, |s, i| s.pending[16 + i]),
            0x300..=0x33c => self.word_of(off - 0x300, |s, i| s.active[16 + i]),
            0x400..=0x5ec => {
                let i = (off - 0x400) as usize;
                (0..4).map(|k| self.prio.get(16 + i + k).copied().unwrap_or(0) as u32).rev().fold(0, |v, p| v << 8 | p)
            }
            // CPUID: Cortex-M4 r0p1
            0xd00 => 0x410f_c241,
            0xd04 => self.icsr(),
            0xd08 => self.vtor,
            0xd0c => 0xfa05_0000 | self.aircr_prigroup << 8,
            0xd10 => self.scr,
            0xd14 => self.ccr,
            0xd18..=0xd20 => {
                let i = (off - 0xd18) as usize + 4;
                (0..4).map(|k| self.prio[i + k] as u32).rev().fold(0, |v, p| v << 8 | p)
            }
            0xd24 => self.shcsr_read(),
            0xd28 => self.cfsr,
            0xd2c => self.hfsr,
            0xd30 => 0,
            0xd34 => self.mmfar,
            0xd38 => self.bfar,
            // ID_PFR0.. ISAR: the Cortex-M4's
            0xd40 => 0x0000_0030,
            0xd44 => 0x0000_0200,
            0xd48 => 0x0010_0000,
            0xd4c => 0,
            0xd50 => 0x0010_0030,
            0xd54 => 0,
            0xd58 => 0x0100_0000,
            0xd5c => 0,
            0xd60 => 0x0110_1110,
            0xd64 => 0x0211_2000,
            0xd68 => 0x2123_2231,
            0xd6c => 0x0111_1131,
            0xd70 => 0x0131_0102,
            0xd88 => self.cpacr,
            // MPU: 8 regions, unified
            0xd90 => 8 << 8,
            0xd94 => self.mpu_ctrl,
            0xd98 => self.mpu_rnr,
            0xd9c | 0xda4 | 0xdac | 0xdb4 => self.mpu_rbar[(self.mpu_rnr & 7) as usize] | (self.mpu_rnr & 7),
            0xda0 | 0xda8 | 0xdb0 | 0xdb8 => self.mpu_rasr[(self.mpu_rnr & 7) as usize],
            0xdfc => self.demcr,
            0xf34 => self.fpccr,
            0xf38 => self.fpcar,
            0xf3c => self.fpdscr,
            // MVFR0..2: single-precision FPv4
            0xf40 => 0x1011_0021,
            0xf44 => 0x1100_0011,
            _ => 0,
        }
    }

    fn word_of(&self, off: u32, f: impl Fn(&Scs, usize) -> bool) -> u32 {
        let base = (off / 4 * 32) as usize;
        (0..32).filter(|&b| base + b < NUM_IRQS && f(self, base + b)).fold(0, |v, b| v | 1 << b)
    }

    fn icsr(&self) -> u32 {
        let pend = self.highest_pending().map_or(0, |(n, _)| n as u32);
        let isr_pending = (16..NUM_EXC).any(|n| self.pending[n]);
        self.ipsr as u32
            | (!self.others_active(self.ipsr) as u32) << 11
            | pend << 12
            | (isr_pending as u32) << 22
            | (self.pending[SYSTICK] as u32) << 26
            | (self.pending[PENDSV] as u32) << 28
            | (self.pending[NMI] as u32) << 31
    }

    fn shcsr_read(&self) -> u32 {
        (self.shcsr & 0x7_0000)
            | self.active[MEMMANAGE] as u32
            | (self.active[BUSFAULT] as u32) << 1
            | (self.active[USAGEFAULT] as u32) << 3
            | (self.active[SVCALL] as u32) << 7
            | (self.active[DEBUGMON] as u32) << 8
            | (self.active[PENDSV] as u32) << 10
            | (self.active[SYSTICK] as u32) << 11
            | (self.pending[USAGEFAULT] as u32) << 12
            | (self.pending[MEMMANAGE] as u32) << 13
            | (self.pending[BUSFAULT] as u32) << 14
            | (self.pending[SVCALL] as u32) << 15
    }

    pub fn dwt_cyccnt(&self, cycles: u64) -> u32 {
        if self.dwt_ctrl & 1 == 0 {
            self.dwt_cyc0
        } else {
            self.dwt_cyc0.wrapping_add(cycles.wrapping_sub(self.dwt_t0) as u32)
        }
    }

    /// A word write. Byte writes to the priority registers merge (see
    /// write_byte).
    pub fn write(&mut self, a: u32, v: u32, now: u64, cycles: u64) {
        self.dirty = true;
        if a & !0xfff == DWT_BASE {
            match a - DWT_BASE {
                0x000 => {
                    let cyc = self.dwt_cyccnt(cycles);
                    self.dwt_ctrl = (self.dwt_ctrl & 0xf000_0000) | (v & 0x0fff_ffff);
                    self.dwt_cyc0 = cyc;
                    self.dwt_t0 = cycles;
                }
                0x004 => {
                    self.dwt_cyc0 = v;
                    self.dwt_t0 = cycles;
                }
                _ => {}
            }
            return;
        }
        let off = a.wrapping_sub(SCS_BASE);
        self.update(now);
        match off {
            0x010 => {
                let val = self.stk_value(now);
                self.stk_ctrl = (self.stk_ctrl & (1 << 16)) | (v & 7);
                self.stk_restart(now, val);
            }
            0x014 => self.stk_load = v & 0xff_ffff,
            // Writing VAL clears it (and COUNTFLAG)
            0x018 => {
                self.stk_ctrl &= !(1 << 16);
                // (counting from 0: the next tick reloads)
                self.stk_restart(now, 0);
            }
            0x100..=0x13c => self.set_bits(off - 0x100, v, |s, i| s.enabled[i] = true),
            0x180..=0x1bc => self.set_bits(off - 0x180, v, |s, i| s.enabled[i] = false),
            0x200..=0x23c => self.set_bits(off - 0x200, v, |s, i| s.pending[16 + i] = true),
            0x280..=0x2bc => self.set_bits(off - 0x280, v, |s, i| s.pending[16 + i] = false),
            0x400..=0x5ec => {
                for k in 0..4 {
                    self.write_byte(a + k, (v >> (8 * k)) as u8);
                }
            }
            0xd04 => {
                if v & 1 << 31 != 0 {
                    self.pending[NMI] = true;
                }
                if v & 1 << 28 != 0 {
                    self.pending[PENDSV] = true;
                }
                if v & 1 << 27 != 0 {
                    self.pending[PENDSV] = false;
                }
                if v & 1 << 26 != 0 {
                    self.pending[SYSTICK] = true;
                }
                if v & 1 << 25 != 0 {
                    self.pending[SYSTICK] = false;
                }
            }
            0xd08 => self.vtor = v & !0x7f,
            0xd0c => {
                if v >> 16 == 0x05fa {
                    self.aircr_prigroup = v >> 8 & 7;
                    if v & 1 << 2 != 0 {
                        self.reset_requested = true;
                    }
                }
            }
            0xd10 => self.scr = v & 0x16,
            0xd14 => self.ccr = v & 0x31b,
            0xd18..=0xd20 => {
                for k in 0..4 {
                    self.write_byte(a + k, (v >> (8 * k)) as u8);
                }
            }
            0xd24 => {
                self.shcsr = v & 0x7_0000;
                self.active[MEMMANAGE] = v & 1 != 0;
                self.active[BUSFAULT] = v & 2 != 0;
                self.active[USAGEFAULT] = v & 8 != 0;
                self.active[SVCALL] = v & 1 << 7 != 0;
                self.active[DEBUGMON] = v & 1 << 8 != 0;
                self.active[PENDSV] = v & 1 << 10 != 0;
                self.active[SYSTICK] = v & 1 << 11 != 0;
                self.pending[USAGEFAULT] = v & 1 << 12 != 0;
                self.pending[MEMMANAGE] = v & 1 << 13 != 0;
                self.pending[BUSFAULT] = v & 1 << 14 != 0;
                self.pending[SVCALL] = v & 1 << 15 != 0;
            }
            // Write 1 to clear
            0xd28 => self.cfsr &= !v,
            0xd2c => self.hfsr &= !v,
            0xd34 => self.mmfar = v,
            0xd38 => self.bfar = v,
            0xd88 => self.cpacr = v & 0x00f0_0000,
            0xd94 => self.mpu_ctrl = v & 7,
            0xd98 => self.mpu_rnr = v & 0xff,
            0xd9c | 0xda4 | 0xdac | 0xdb4 => {
                if v & 0x10 != 0 {
                    self.mpu_rnr = v & 7;
                }
                self.mpu_rbar[(self.mpu_rnr & 7) as usize] = v & !0x1f;
            }
            0xda0 | 0xda8 | 0xdb0 | 0xdb8 => self.mpu_rasr[(self.mpu_rnr & 7) as usize] = v,
            0xdfc => self.demcr = v,
            0xf00 => {
                // STIR: pend an external interrupt
                let n = (v & 0x1ff) as usize;
                if n < NUM_IRQS {
                    self.pending[16 + n] = true;
                }
            }
            0xf34 => self.fpccr = v,
            0xf38 => self.fpcar = v & !7,
            0xf3c => self.fpdscr = v,
            _ => {}
        }
    }

    /// A byte write: the priority registers are byte-accessible.
    pub fn write_byte(&mut self, a: u32, b: u8) {
        self.dirty = true;
        let off = a.wrapping_sub(SCS_BASE);
        match off {
            0x400..=0x5ef => {
                let i = 16 + (off - 0x400) as usize;
                if i < NUM_EXC {
                    self.prio[i] = b & PRIO_MASK;
                }
            }
            0xd18..=0xd23 => {
                let i = (off - 0xd18) as usize + 4;
                self.prio[i] = b & PRIO_MASK;
            }
            _ => {
                // Others: as part of their word
                let w = a & !3;
                let sh = (a & 3) * 8;
                let old = self.read(w, 0, 0);
                self.write(w, (old & !(0xff << sh)) | (b as u32) << sh, 0, 0);
            }
        }
    }

    fn set_bits(&mut self, off: u32, v: u32, mut f: impl FnMut(&mut Scs, usize)) {
        let base = (off / 4 * 32) as usize;
        for b in 0..32 {
            if v >> b & 1 != 0 && base + b < NUM_IRQS {
                f(self, base + b);
            }
        }
    }

    /// When update() next has something to do (ns).
    pub fn next_event_ns(&self) -> u64 { self.stk_next }

    /// The cycle counter's base moves when time is rescaled; nothing to do.
    pub fn is_fault_enabled(&self, n: usize) -> bool {
        match n {
            MEMMANAGE => self.shcsr & 1 << 16 != 0,
            BUSFAULT => self.shcsr & 1 << 17 != 0,
            USAGEFAULT => self.shcsr & 1 << 18 != 0,
            _ => true,
        }
    }
}
