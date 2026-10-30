//! The card10 badge (CCC camp 2019): a Maxim MAX32666 (the MAX32665
//! family: two Cortex-M4F cores at 96 MHz, 1 MB of flash at 0x1000_0000,
//! 560 KB of SRAM) running its firmware: Epicardium (FreeRTOS) on core 0,
//! Pycardium (MicroPython) on core 1.
//!
//! Following the MAX32665 SDK's register definitions (in the firmware's
//! lib/sdk, MAX32665/Include/*_regs.h) and drivers, where they say how the
//! hardware behaves.
//!
//! The two cores share the bus (memory and peripherals); each has its own
//! NVIC and SysTick (its System Control Space), its registers and its clock.
//! Card10 interleaves them in short slices, giving the bus (with that core's
//! SCS in it) to the core that runs. Between the cores:
//! - SEMA's hardware semaphores (a read takes one, a write of 0 frees it)
//! - SEV: one core's event wakes the other from WFE (GCR EVTEN's TX events)
//! - interrupts: every peripheral's line goes to both NVICs (Epicardium
//!   raises TMR5's for core 1)
//! - GCR PERCKCN1.CPU1 starts and stops core 1, from the vector table GCR
//!   GP0 points at

use std::any::Any;
use std::collections::HashMap;

use crate::bus::{Bus, Device};
use crate::cpu::{Cpu, Stop};

pub const FLASH_BASE: u32 = 0x1000_0000;
pub const FLASH_SIZE: usize = 0x10_0000;
pub const SRAM_SIZE: usize = 0x8_c000;
pub const CPU_HZ: u64 = 96_000_000;
/// The peripheral clock: half the system clock
pub const PCLK_HZ: u64 = CPU_HZ / 2;

pub const GCR_BASE: u32 = 0x4000_0000;
pub const SPIXFC_BASE: u32 = 0x4002_7000;
pub const ADC_BASE: u32 = 0x4003_4000;
pub const GPIO_BASE: [u32; 2] = [0x4000_8000, 0x4000_9000];
pub const GPIO_IRQ: [usize; 2] = [24, 25];
pub const SPI_BASE: [u32; 3] = [0x400b_e000, 0x4004_6000, 0x4004_7000];
pub const SPI_IRQ: [usize; 3] = [56, 16, 17];
pub const EXT_FLASH_SIZE: usize = 8 << 20;
pub const RTC_BASE: u32 = 0x4000_6000;
pub const WUT_BASE: u32 = 0x4000_6400;
pub const TMR_BASE: [u32; 6] = [0x4001_0000, 0x4001_1000, 0x4001_2000, 0x4001_3000, 0x4001_4000, 0x4001_5000];
pub const TMR_IRQ: [usize; 6] = [5, 6, 7, 8, 9, 10];
pub const SEMA_BASE: u32 = 0x4003_e000;
pub const UART_BASE: [u32; 3] = [0x4004_2000, 0x4004_3000, 0x4004_4000];
pub const UART_IRQ: [usize; 3] = [14, 15, 34];
pub const I2C_BASE: [u32; 3] = [0x4001_d000, 0x4001_e000, 0x4001_f000];
pub const I2C_IRQ: [usize; 3] = [13, 36, 62];
const RTC_IRQ: usize = 3;

// ---------------------------------------------------------------- registers

/// Registers that hold what's written, for the peripherals nothing models
/// (yet): reads OR in `set` bits (ready flags) for those that have them.
/// With `log`, each register's first access is reported.
pub struct Regs {
    pub name: &'static str,
    words: HashMap<u32, u32>,
    set: Vec<(u32, u32)>,
    pub log: bool,
    seen: std::collections::HashSet<u32>,
}

impl Regs {
    pub fn new(name: &'static str, set: &[(u32, u32)]) -> Regs {
        Regs { name, words: HashMap::new(), set: set.to_vec(), log: false, seen: Default::default() }
    }
    pub fn with(mut self, off: u32, v: u32) -> Regs {
        self.words.insert(off, v);
        self
    }
    fn note(&mut self, what: &str, off: u32, v: u32) {
        if self.log && self.seen.insert(off) {
            eprintln!("max32665: {} {} {:#x} = {:#x}", self.name, what, off, v);
        }
    }
}

impl Device for Regs {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let w = off & !3;
        let mut v = *self.words.get(&w).unwrap_or(&0);
        for &(o, bits) in &self.set {
            if o == w {
                v |= bits;
            }
        }
        self.note("read", w, v);
        v >> ((off & 3) * 8) & if size == 4 { !0 } else { (1 << (size * 8)) - 1 }
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        let w = off & !3;
        let old = *self.words.get(&w).unwrap_or(&0);
        let sh = (off & 3) * 8;
        let m = if size == 4 { !0 } else { ((1u32 << (size * 8)) - 1) << sh };
        let v = (old & !m) | (value << sh & m);
        self.note("write", w, v);
        self.words.insert(w, v);
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

/// Sub-word access to a register file of words.
fn word_write(old: u32, off: u32, size: u32, value: u32) -> u32 {
    let sh = (off & 3) * 8;
    let m = if size == 4 { !0 } else { ((1u32 << (size * 8)) - 1) << sh };
    (old & !m) | (value << sh & m)
}

fn word_read(v: u32, off: u32, size: u32) -> u32 {
    v >> ((off & 3) * 8) & if size == 4 { !0 } else { (1 << (size * 8)) - 1 }
}

// ---------------------------------------------------------------- GCR

/// Global Control Registers: clocks (always ready), resets, core 1.
pub struct Gcr {
    regs: [u32; 0x40],
    /// RSTR0.SYSTEM written: the whole chip resets
    pub system_reset: bool,
}

const GCR_RSTR0: usize = 0x04 / 4;
const GCR_CLKCN: usize = 0x08 / 4;
const GCR_PERCKCN1: usize = 0x48 / 4;
const GCR_EVTEN: usize = 0x4c / 4;
const GCR_REVISION: usize = 0x50 / 4;
const GCR_GP0: usize = 0x80 / 4;

impl Gcr {
    fn new() -> Gcr {
        let mut regs = [0; 0x40];
        // HIRC (60 MHz) enabled, divided by 1: as the boot ROM leaves it
        regs[GCR_CLKCN] = 1 << 18;
        // core 1's clock off
        regs[GCR_PERCKCN1] = 1 << 31;
        regs[GCR_REVISION] = 0xa4;
        Gcr { regs, system_reset: false }
    }
    /// Core 1's clock is on (PERCKCN1.CPU1 clear), from the vector table at
    /// GP0.
    pub fn core1(&self) -> Option<u32> { (self.regs[GCR_PERCKCN1] & 1 << 31 == 0).then_some(self.regs[GCR_GP0]) }
    /// SEV on one core wakes the other (EVTEN CPU0TXEVENT, CPU1TXEVENT).
    pub fn tx_event(&self, core: usize) -> bool { self.regs[GCR_EVTEN] & if core == 0 { 1 << 2 } else { 1 << 5 } != 0 }
}

impl Device for Gcr {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let i = (off / 4) as usize & 0x3f;
        let mut v = self.regs[i];
        if i == GCR_CLKCN {
            // every oscillator ready, and the clock switch done (CKRDY)
            v |= 0x7f << 24 | 1 << 13;
        }
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        let i = (off / 4) as usize & 0x3f;
        let v = word_write(self.regs[i], off, size, value);
        match i {
            GCR_RSTR0 => {
                // resets complete at once (reading back 0)
                if v & 1 << 31 != 0 {
                    self.system_reset = true;
                }
            }
            _ => self.regs[i] = v,
        }
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- SEMA

/// Hardware semaphores: reading one returns it and takes it.
pub struct Sema {
    sema: [u32; 8],
}

impl Device for Sema {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let v = match off & !3 {
            o @ 0..=0x1c => {
                let i = o as usize / 4;
                let v = self.sema[i];
                self.sema[i] = 1;
                v
            }
            0x100 => self.sema.iter().enumerate().fold(0, |v, (i, s)| v | s << i),
            _ => 0,
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, _size: u32, value: u32, _now: u64) {
        if let o @ 0..=0x1c = off & !3 {
            self.sema[o as usize / 4] = value & 1;
        }
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- UART

/// A UART: what's sent is collected (tx); input waits in rx. Sending takes
/// no time.
pub struct Uart {
    ctrl: u32,
    thresh: u32,
    int_en: u32,
    int_fl: u32,
    regs: [u32; 4],
    pub tx: Vec<u8>,
    pub rx: std::collections::VecDeque<u8>,
}

const UART_INT_RX_THRESH: u32 = 1 << 4;
const UART_INT_TX_ALMOST_EMPTY: u32 = 1 << 5;
const UART_INT_TX_THRESH: u32 = 1 << 6;

impl Uart {
    fn new() -> Uart {
        Uart { ctrl: 0, thresh: 0, int_en: 0, int_fl: 0, regs: [0; 4], tx: Vec::new(), rx: Default::default() }
    }
    pub fn input(&mut self, data: &[u8]) {
        self.rx.extend(data);
        self.flags();
    }
    fn flags(&mut self) {
        // the TX FIFO is always empty (sent at once)
        self.int_fl |= UART_INT_TX_ALMOST_EMPTY | UART_INT_TX_THRESH;
        let rx_thresh = (self.thresh & 0x3f).max(1) as usize;
        if self.rx.len() >= rx_thresh {
            self.int_fl |= UART_INT_RX_THRESH;
        }
    }
}

impl Device for Uart {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let v = match off & !3 {
            0x00 => self.ctrl,
            0x04 => self.thresh,
            0x08 => {
                // TX empty; RX empty or its count
                let n = self.rx.len().min(32) as u32;
                1 << 6 | if n == 0 { 1 << 4 } else { n << 8 }
            }
            0x0c => self.int_en,
            0x10 => self.int_fl,
            0x1c => {
                let b = self.rx.pop_front().unwrap_or(0) as u32;
                b
            }
            o @ 0x14..=0x24 => self.regs[((o - 0x14) / 4) as usize % 4],
            _ => 0,
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        match off & !3 {
            0x00 => {
                self.ctrl = word_write(self.ctrl, off, size, value);
                if self.ctrl & 1 << 6 != 0 {
                    self.rx.clear();
                }
                self.ctrl &= !(3 << 5);
            }
            0x04 => self.thresh = word_write(self.thresh, off, size, value),
            0x0c => self.int_en = word_write(self.int_en, off, size, value),
            // write 1 to clear
            0x10 => self.int_fl &= !word_write(0, off, size, value),
            0x1c => self.tx.push(value as u8),
            o @ 0x14..=0x24 => {
                let i = ((o - 0x14) / 4) as usize % 4;
                self.regs[i] = word_write(self.regs[i], off, size, value);
            }
            _ => {}
        }
        self.flags();
    }
    fn irq(&self) -> bool { self.int_en & self.int_fl != 0 }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- TMR

/// A 32-bit timer (TMR0-5, and the wake-up timer, the same on a slower
/// clock), counting its clock divided by its prescaler
/// up to its compare value: one-shot, continuous and compare modes (the
/// ones the firmware's timeouts and delays use); others just count.
pub struct Tmr {
    cnt: u32,
    cmp: u32,
    pwm: u32,
    intr: bool,
    cn: u32,
    /// When cnt was last brought up to date (ns), and the remainder in ns
    at: u64,
    /// The clock it counts (before its prescaler)
    clk_hz: u64,
}

impl Tmr {
    fn new(clk_hz: u64) -> Tmr { Tmr { cnt: 0, cmp: 0xffff_ffff, pwm: 0, intr: false, cn: 0, at: 0, clk_hz } }
    fn enabled(&self) -> bool { self.cn & 1 << 7 != 0 }
    fn mode(&self) -> u32 { self.cn & 7 }
    /// ns per count
    fn tick_ns(&self) -> f64 {
        let pres = (self.cn >> 3 & 7) | (self.cn >> 8 & 1) << 3;
        (1u64 << pres) as f64 * 1e9 / self.clk_hz as f64
    }
    /// Bring the count up to `now`, wrapping or stopping at the compare
    /// value as the mode does.
    fn advance(&mut self, now: u64) {
        if !self.enabled() || now <= self.at {
            self.at = self.at.max(now);
            return;
        }
        let ticks = ((now - self.at) as f64 / self.tick_ns()) as u64;
        if ticks == 0 {
            return;
        }
        self.at += (ticks as f64 * self.tick_ns()) as u64;
        let cmp = self.cmp.max(1) as u64;
        let mut cnt = self.cnt as u64 + ticks;
        match self.mode() {
            // one-shot: at the compare value, interrupt, stop, back to 1
            0 => {
                if self.cnt as u64 <= cmp && cnt >= cmp {
                    self.intr = true;
                    self.cn &= !(1 << 7);
                    cnt = 1;
                }
            }
            // continuous: at the compare value, interrupt and restart at 1
            1 => {
                if cnt >= cmp {
                    self.intr = true;
                    cnt = 1 + (cnt - cmp) % cmp;
                }
            }
            // compare: interrupt at the compare value, counting on (wrapping)
            5 => {
                if (self.cnt as u64) < cmp && cnt >= cmp {
                    self.intr = true;
                }
                cnt &= 0xffff_ffff;
            }
            _ => cnt &= 0xffff_ffff,
        }
        self.cnt = cnt as u32;
    }
}

impl Device for Tmr {
    fn read(&mut self, off: u32, size: u32, now: u64) -> u32 {
        self.advance(now);
        let v = match off & !3 {
            0x00 => self.cnt,
            0x04 => self.cmp,
            0x08 => self.pwm,
            0x0c => self.intr as u32,
            0x10 => self.cn,
            _ => 0,
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, now: u64) {
        self.advance(now);
        match off & !3 {
            0x00 => self.cnt = word_write(self.cnt, off, size, value),
            0x04 => self.cmp = word_write(self.cmp, off, size, value),
            0x08 => self.pwm = word_write(self.pwm, off, size, value),
            0x0c => {
                if value & 1 != 0 {
                    self.intr = false;
                }
            }
            0x10 => {
                let was = self.enabled();
                self.cn = word_write(self.cn, off, size, value);
                if self.enabled() && !was {
                    self.at = now;
                }
            }
            _ => {}
        }
    }
    fn update(&mut self, now: u64) { self.advance(now); }
    fn next_event_ns(&self) -> u64 {
        if !self.enabled() || !matches!(self.mode(), 0 | 1 | 5) || self.intr {
            return u64::MAX;
        }
        let cmp = self.cmp.max(1) as u64;
        if (self.cnt as u64) >= cmp && self.mode() != 1 {
            return u64::MAX;
        }
        let left = cmp.saturating_sub(self.cnt as u64).max(1);
        self.at + (left as f64 * self.tick_ns()).ceil() as u64
    }
    fn irq(&self) -> bool { self.intr }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- RTC

/// The real-time clock: seconds and 1/4096 s, from the host's time at
/// start; never busy.
pub struct Rtc {
    /// The RTC's value (in 1/4096 s) at machine time 0
    base: i64,
    ctrl: u32,
    toda: u32,
    sseca: u32,
    regs: [u32; 2],
}

impl Rtc {
    fn ticks(&self, now: u64) -> i64 { self.base + (now as u128 * 4096 / 1_000_000_000) as i64 }
    /// The RTC's seconds at machine time `now`.
    pub fn seconds(&self, now: u64) -> i64 { self.ticks(now) >> 12 }
    /// Set it to `sec` at machine time `now` (across a reset).
    pub fn set_seconds(&mut self, sec: i64, now: u64) { self.base = (sec << 12) - (self.ticks(now) - self.base); }
}

impl Device for Rtc {
    fn read(&mut self, off: u32, size: u32, now: u64) -> u32 {
        let t = self.ticks(now);
        let v = match off & !3 {
            0x00 => (t >> 12) as u32,
            0x04 => (t & 0xfff) as u32,
            0x08 => self.toda,
            0x0c => self.sseca,
            // ready, not busy
            0x10 => (self.ctrl & !(1 << 3)) | 1 << 4,
            o @ 0x14..=0x18 => self.regs[((o - 0x14) / 4) as usize],
            _ => 0,
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, now: u64) {
        let t = self.ticks(now);
        match off & !3 {
            0x00 => self.base += ((value as i64) << 12 | (t & 0xfff)) - t,
            0x04 => self.base += ((t & !0xfff) | (value as i64 & 0xfff)) - t,
            0x08 => self.toda = value,
            0x0c => self.sseca = value,
            0x10 => self.ctrl = word_write(self.ctrl, off, size, value),
            o @ 0x14..=0x18 => self.regs[((o - 0x14) / 4) as usize] = value,
            _ => {}
        }
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- ADC

/// The ADC (10 bits, 1.22 V full scale): conversions finish at once.
/// Channel 0 is the PMIC's analog multiplexer, channel 7 the light sensor.
pub struct Adc {
    ctrl: u32,
    data: u32,
    intr: u32,
    amux: std::rc::Rc<std::cell::Cell<u8>>,
    /// The light sensor's reading (0-1023)
    pub light: u32,
}

impl Adc {
    fn convert(&self, ch: u32) -> u32 {
        let volts = |v: f32| ((v / 1.22 * 1023.0) as u32).min(1023);
        match ch {
            0 => match self.amux.get() {
                // battery 3.9 V, system 4.0 V (as the PMIC scales them)
                3 => volts(3.9 * 0.272),
                0xa => volts(4.0 * 0.26),
                // the thermistor, at room temperature
                7 => volts(0.5),
                8 => volts(1.25),
                _ => 0,
            },
            7 => self.light,
            _ => 0,
        }
    }
}

impl Device for Adc {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let v = match off & !3 {
            0x00 => self.ctrl,
            0x04 => 0,
            0x08 => self.data,
            0x0c => self.intr,
            _ => 0,
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        match off & !3 {
            0x00 => {
                self.ctrl = word_write(self.ctrl, off, size, value);
                if self.ctrl & 1 != 0 {
                    self.ctrl &= !1;
                    let v = self.convert(self.ctrl >> 12 & 0x1f);
                    self.data = if self.ctrl & 1 << 20 != 0 { v << 6 } else { v };
                    // DONE, and the reference ready
                    self.intr |= 1 << 16 | 1 << 17;
                }
            }
            // flags: write 1 to clear; enables as written
            0x0c => {
                let v = word_write(self.intr, off, size, value);
                self.intr = (v & 0xffff) | (self.intr & !(v & 0xffff_0000) & 0xffff_0000);
            }
            _ => {}
        }
    }
    fn irq(&self) -> bool { self.intr >> 16 & self.intr & 0x1f != 0 }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- TRNG

/// The random number generator: always ready, a xorshift sequence (seeded
/// by the host).
pub struct Trng {
    cn: u32,
    pub state: u64,
}

impl Device for Trng {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let v = match off & !3 {
            0x00 => self.cn,
            0x04 => 1,
            0x08 => {
                let mut x = self.state;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.state = x;
                (x >> 16) as u32
            }
            _ => 0,
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        if off & !3 == 0 {
            self.cn = word_write(self.cn, off, size, value);
        }
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- external flash

/// The MX25 SPI NOR flash (8 MB) on the SPIXFC: the commands the SDK's
/// MX25 driver uses. Programming ANDs into the page, erasing sets to 0xFF;
/// both take no time.
pub struct Mx25 {
    pub data: Vec<u8>,
    /// Written since the host last looked (for saving it)
    pub dirty: bool,
    cmd: Option<u8>,
    addr: u32,
    /// Address bytes still to come
    addr_left: u8,
    wel: bool,
    status_byte: u8,
}

impl Mx25 {
    pub fn new(image: &[u8], size: usize) -> Mx25 {
        let mut data = vec![0xff; size];
        let n = image.len().min(size);
        data[..n].copy_from_slice(&image[..n]);
        Mx25 { data, dirty: false, cmd: None, addr: 0, addr_left: 0, wel: false, status_byte: 0 }
    }
    fn select(&mut self) {
        self.cmd = None;
        self.addr_left = 0;
    }
    fn deselect(&mut self) {
        if let Some(c) = self.cmd {
            if self.addr_left == 0 {
                let n = self.data.len() as u32;
                let erase = |d: &mut Vec<u8>, a: u32, len: u32| {
                    let a = (a & !(len - 1)) % n;
                    d[a as usize..(a + len).min(n) as usize].fill(0xff);
                };
                match c {
                    0x20 if self.wel => erase(&mut self.data, self.addr, 0x1000),
                    0x52 if self.wel => erase(&mut self.data, self.addr, 0x8000),
                    0xd8 if self.wel => erase(&mut self.data, self.addr, 0x1_0000),
                    _ => {}
                }
                if matches!(c, 0x20 | 0x52 | 0xd8 | 0xc7 | 0x60 | 0x02 | 0x32 | 0x38 | 0x01) {
                    if matches!(c, 0x20 | 0x52 | 0xd8) && self.wel {
                        self.dirty = true;
                    }
                    self.wel = false;
                }
            }
        }
        self.cmd = None;
    }
    fn write(&mut self, b: u8) {
        let Some(c) = self.cmd else {
            self.cmd = Some(b);
            match b {
                0x06 => self.wel = true,
                0x04 => self.wel = false,
                0xc7 | 0x60 if self.wel => {
                    self.data.fill(0xff);
                    self.dirty = true;
                }
                0x0b | 0xbb | 0xeb | 0x03 | 0x02 | 0x32 | 0x38 | 0x20 | 0x52 | 0xd8 => {
                    self.addr = 0;
                    self.addr_left = 3;
                }
                _ => {}
            }
            return;
        };
        if self.addr_left > 0 {
            self.addr = self.addr << 8 | b as u32;
            self.addr_left -= 1;
            return;
        }
        match c {
            0x02 | 0x32 | 0x38 if self.wel => {
                let n = self.data.len() as u32;
                let a = (self.addr % n) as usize;
                self.data[a] &= b;
                self.dirty = true;
                // (within the 256-byte page)
                self.addr = (self.addr & !0xff) | (self.addr + 1) & 0xff;
            }
            0x01 => self.status_byte = b,
            _ => {}
        }
    }
    fn read(&mut self) -> u8 {
        match self.cmd {
            Some(0x9f) => {
                let v = [0xc2, 0x25, 0x37][self.addr as usize % 3];
                self.addr += 1;
                v
            }
            // status: never busy; WEL; quad enabled
            Some(0x05) => 0x40 | (self.wel as u8) << 1,
            Some(0x0b | 0xbb | 0xeb | 0x03) if self.addr_left == 0 => {
                let n = self.data.len() as u32;
                let v = self.data[(self.addr % n) as usize];
                self.addr = self.addr.wrapping_add(1);
                v
            }
            _ => 0xff,
        }
    }
}

/// The SPIXFC (SPI execute-in-place flash controller), driven through its
/// FIFOs as the SDK does it: 16-bit words, a header (direction, units, size,
/// and whether slave select deasserts at its end) then the data to send;
/// bytes received go to the RX FIFO. Shared by its registers and FIFO
/// mappings.
pub struct SpixfcState {
    pub flash: Mx25,
    regs: [u32; 8],
    /// Bytes still to send for the current header, and whether to
    /// deassert after them
    tx_left: u32,
    deass: bool,
    selected: bool,
    rx: std::collections::VecDeque<u8>,
}

impl SpixfcState {
    fn tx_word(&mut self, w: u16) {
        if self.tx_left > 0 {
            let mut bytes = vec![w as u8];
            if self.tx_left > 1 {
                bytes.push((w >> 8) as u8);
            }
            for b in bytes {
                self.flash.write(b);
                self.tx_left -= 1;
            }
            if self.tx_left == 0 && self.deass {
                self.end();
            }
            return;
        }
        if w & 0xf000 == 0xf000 {
            // filler after a clocks-only header
            return;
        }
        let dir = w & 3;
        let units = w >> 2 & 3;
        let size = match (w >> 4 & 0x1f) as u32 {
            0 => 32,
            n => n,
        };
        let deass = w >> 13 & 1 != 0;
        let bytes = match units {
            0 => size.div_ceil(8),
            1 => size,
            _ => size * 32,
        };
        if !self.selected {
            self.flash.select();
            self.selected = true;
        }
        if dir & 2 != 0 {
            for _ in 0..bytes {
                let b = self.flash.read();
                self.rx.push_back(b);
            }
        }
        if dir & 1 != 0 && bytes > 0 {
            self.tx_left = bytes;
            self.deass = deass;
            return;
        }
        if deass {
            self.end();
        }
    }
    fn end(&mut self) {
        self.flash.deselect();
        self.selected = false;
    }
}

pub struct SpixfcRegs(pub std::rc::Rc<std::cell::RefCell<SpixfcState>>);
pub struct SpixfcFifo(pub std::rc::Rc<std::cell::RefCell<SpixfcState>>);

impl Device for SpixfcRegs {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let s = self.0.borrow();
        let i = (off as usize / 4) & 7;
        let v = match i {
            // FIFO_CTRL: TX empty; the RX count
            3 => (s.regs[3] & 0x00ff_00ff) | (s.rx.len().min(16) as u32) << 24,
            // INTFL: TX ready, RX done
            5 => s.regs[5] | 1 << 2 | 1 << 3,
            _ => s.regs[i],
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        let mut s = self.0.borrow_mut();
        let i = (off as usize / 4) & 7;
        if i == 5 {
            s.regs[5] &= !word_write(0, off, size, value);
        } else {
            s.regs[i] = word_write(s.regs[i], off, size, value);
        }
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

impl Device for SpixfcFifo {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let mut s = self.0.borrow_mut();
        if off & !3 != 4 {
            return 0;
        }
        let mut v = 0;
        for k in 0..size {
            v |= (s.rx.pop_front().unwrap_or(0xff) as u32) << (8 * k);
        }
        v
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        let mut s = self.0.borrow_mut();
        if off & !3 != 0 {
            return;
        }
        match size {
            2 => s.tx_word(value as u16),
            4 => {
                s.tx_word(value as u16);
                s.tx_word((value >> 16) as u16);
            }
            _ => s.tx_word(0xf000 | value as u16 & 0xff),
        }
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- I2C

/// A device on an I2C bus.
pub trait I2cDevice {
    /// A START (or repeated START) addressing it, to read or to write.
    fn start(&mut self, read: bool);
    fn write(&mut self, b: u8);
    fn read(&mut self) -> u8;
    fn stop(&mut self) {}
    fn as_any(&mut self) -> &mut dyn Any;
}

/// The usual register-based I2C device: a write's first byte selects the
/// register, the rest write from there; reads go on from the register
/// selected. `on_write` sees each register write (for side effects).
pub struct I2cRegs {
    pub name: &'static str,
    pub regs: [u8; 256],
    pub ptr: u8,
    first: bool,
    pub on_write: Option<fn(&mut [u8; 256], u8, u8)>,
    pub on_read: Option<fn(&mut [u8; 256], u8) -> u8>,
}

impl I2cRegs {
    pub fn new(name: &'static str, init: &[(u8, u8)]) -> I2cRegs {
        let mut regs = [0; 256];
        for &(r, v) in init {
            regs[r as usize] = v;
        }
        I2cRegs { name, regs, ptr: 0, first: false, on_write: None, on_read: None }
    }
}

impl I2cDevice for I2cRegs {
    fn start(&mut self, read: bool) { self.first = !read; }
    fn write(&mut self, b: u8) {
        if self.first {
            self.ptr = b;
            self.first = false;
        } else {
            self.regs[self.ptr as usize] = b;
            if let Some(f) = self.on_write {
                f(&mut self.regs, self.ptr, b);
            }
            self.ptr = self.ptr.wrapping_add(1);
        }
    }
    fn read(&mut self) -> u8 {
        let v = match self.on_read {
            Some(f) => f(&mut self.regs, self.ptr),
            None => self.regs[self.ptr as usize],
        };
        self.ptr = self.ptr.wrapping_add(1);
        v
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

/// An I2C controller, as the SDK's master driver uses it: transfers happen
/// at once (bytes written to the FIFO reach the device as they're written,
/// a read's bytes are in the FIFO when its address is sent). Devices are
/// keyed by their 7-bit address; one that isn't there NACKs.
pub struct I2c {
    pub name: &'static str,
    ctrl: u32,
    int_fl0: u32,
    int_en0: u32,
    int_fl1: u32,
    int_en1: u32,
    rx_ctrl1: u32,
    master_ctrl: u32,
    regs: HashMap<u32, u32>,
    /// A START has been given; the next FIFO byte is the address
    want_addr: bool,
    /// The device addressed, and whether it's a read
    cur: Option<(u8, bool)>,
    rx: std::collections::VecDeque<u8>,
    txq: std::collections::VecDeque<u8>,
    pub devices: Vec<(u8, Box<dyn I2cDevice>)>,
    pub log: bool,
}

const I2C_DONE: u32 = 1 << 0;
const I2C_STOP: u32 = 1 << 6;
const I2C_ADDR_NACK: u32 = 1 << 10;

impl I2c {
    pub fn new(name: &'static str) -> I2c {
        I2c {
            name,
            ctrl: 0,
            int_fl0: 0,
            int_en0: 0,
            int_fl1: 0,
            int_en1: 0,
            rx_ctrl1: 0,
            master_ctrl: 0,
            regs: HashMap::new(),
            want_addr: false,
            cur: None,
            rx: Default::default(),
            txq: Default::default(),
            devices: Vec::new(),
            log: false,
        }
    }
    pub fn attach(&mut self, addr: u8, dev: Box<dyn I2cDevice>) { self.devices.push((addr, dev)); }
    pub fn device<T: 'static>(&mut self, addr: u8) -> Option<&mut T> {
        self.devices.iter_mut().find(|(a, _)| *a == addr)?.1.as_any().downcast_mut::<T>()
    }
    fn dev(&mut self, addr: u8) -> Option<&mut Box<dyn I2cDevice>> {
        self.devices.iter_mut().find(|(a, _)| *a == addr).map(|(_, d)| d)
    }
    fn address(&mut self, byte: u8) {
        let (addr, read) = (byte >> 1, byte & 1 != 0);
        let n = if self.rx_ctrl1 & 0xff == 0 { 256 } else { self.rx_ctrl1 & 0xff };
        let log = self.log;
        let name = self.name;
        match self.dev(addr) {
            Some(d) => {
                d.start(read);
                if read {
                    let bytes: Vec<u8> = (0..n).map(|_| d.read()).collect();
                    if log {
                        eprintln!("{name}: read {addr:#04x}: {bytes:02x?}");
                    }
                    self.rx.extend(bytes);
                }
                self.cur = Some((addr, read));
            }
            None => {
                if log {
                    eprintln!("{name}: no device at {addr:#04x}");
                }
                self.cur = None;
                self.int_fl0 |= I2C_ADDR_NACK | I2C_DONE | I2C_STOP;
            }
        }
    }
    fn send(&mut self, b: u8) {
        let Some((a, false)) = self.cur else { return };
        let (name, log) = (self.name, self.log);
        if let Some(d) = self.dev(a) {
            d.write(b);
            if log {
                eprintln!("{name}: write {a:#04x}: {b:02x}");
            }
        }
    }
    fn end(&mut self, stop: bool) {
        if let Some((a, _)) = self.cur.take() {
            if stop {
                if let Some(d) = self.dev(a) {
                    d.stop();
                }
            }
        }
        self.int_fl0 |= I2C_DONE | if stop { I2C_STOP } else { 0 };
    }
}

impl Device for I2c {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let v = match off & !3 {
            0x00 => self.ctrl,
            // TX empty (never full); RX empty or not
            0x04 => 1 << 3 | if self.rx.is_empty() { 1 << 1 } else { 0 },
            0x08 => self.int_fl0,
            0x0c => self.int_en0,
            0x10 => self.int_fl1,
            0x14 => self.int_en1,
            0x18 => 8 << 8 | 8,
            0x20 => self.rx_ctrl1,
            0x2c => self.rx.pop_front().unwrap_or(0xff) as u32,
            0x30 => self.master_ctrl,
            o => *self.regs.get(&o).unwrap_or(&0),
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        match off & !3 {
            0x00 => self.ctrl = word_write(self.ctrl, off, size, value),
            0x08 => self.int_fl0 &= !word_write(0, off, size, value),
            0x0c => self.int_en0 = word_write(self.int_en0, off, size, value),
            0x10 => self.int_fl1 &= !word_write(0, off, size, value),
            0x14 => self.int_en1 = word_write(self.int_en1, off, size, value),
            0x20 => self.rx_ctrl1 = word_write(self.rx_ctrl1, off, size, value),
            0x2c => {
                let b = value as u8;
                if self.want_addr {
                    self.want_addr = false;
                    self.address(b);
                } else if let Some((_, false)) = self.cur {
                    self.send(b);
                } else {
                    // before START: the address, then data
                    self.txq.push_back(b);
                }
            }
            0x30 => {
                let v = word_write(self.master_ctrl, off, size, value);
                if v & 1 != 0 && self.master_ctrl & 1 == 0 {
                    // START (or a repeated START after RESTART): the first
                    // byte queued is the address, the rest data
                    if self.cur.is_some() {
                        self.end(false);
                    }
                    match self.txq.pop_front() {
                        Some(a) => {
                            self.address(a);
                            while let Some(b) = self.txq.pop_front() {
                                self.send(b);
                            }
                        }
                        None => self.want_addr = true,
                    }
                }
                if v & 2 != 0 {
                    // RESTART: this transfer's done; the next START begins
                    // the next
                    self.end(false);
                    self.master_ctrl = 0;
                    return;
                }
                if v & 4 != 0 {
                    self.end(true);
                    self.want_addr = false;
                    self.txq.clear();
                    self.master_ctrl = 0;
                    return;
                }
                self.master_ctrl = v;
            }
            o => {
                let old = *self.regs.get(&o).unwrap_or(&0);
                self.regs.insert(o, word_write(old, off, size, value));
            }
        }
    }
    fn irq(&self) -> bool { self.int_fl0 & self.int_en0 != 0 || self.int_fl1 & self.int_en1 != 0 }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- GPIO

/// A GPIO port: outputs (shared, for devices wired to them: the display's
/// data/command line), inputs the host sets, and edge or level interrupts.
pub struct Gpio {
    regs: [u32; 0x34],
    pub out: std::rc::Rc<std::cell::Cell<u32>>,
    /// The pins' input levels
    pub input: u32,
    int_stat: u32,
}

const GPIO_OUT: usize = 0x18 / 4;
const GPIO_INT_MOD: usize = 0x28 / 4;
const GPIO_INT_POL: usize = 0x2c / 4;
const GPIO_INT_EN: usize = 0x34 / 4;
const GPIO_DUAL_EDGE: usize = 0x5c / 4;

impl Gpio {
    fn new(out: std::rc::Rc<std::cell::Cell<u32>>, input: u32) -> Gpio {
        Gpio { regs: [0; 0x34], out, input, int_stat: 0 }
    }
    /// The host changes input levels: edges raise interrupts as configured.
    pub fn set_input(&mut self, levels: u32) {
        let (old, new) = (self.input, levels);
        self.input = levels;
        let rise = !old & new;
        let fall = old & !new;
        let edge = self.regs[GPIO_INT_MOD];
        let pol = self.regs[GPIO_INT_POL];
        let dual = self.regs[GPIO_DUAL_EDGE];
        let hit = edge & ((dual & (rise | fall)) | (!dual & ((pol & rise) | (!pol & fall))));
        self.int_stat |= hit;
    }
    /// INT_STAT: edge-triggered pins' latched edges, level-triggered pins'
    /// levels as they are.
    fn stat(&self) -> u32 {
        let edge = self.regs[GPIO_INT_MOD];
        let pol = self.regs[GPIO_INT_POL];
        let active = (self.input & pol) | (!self.input & !pol);
        (self.int_stat & edge) | (active & !edge)
    }
}

impl Device for Gpio {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let i = (off as usize / 4).min(0x33);
        let v = match i {
            GPIO_OUT => self.out.get(),
            // IN: outputs read back as driven, inputs as the host set them
            9 => (self.out.get() & self.regs[3]) | (self.input & !self.regs[3]),
            // INT_STAT, only of enabled pins
            0x10 => self.stat() & self.regs[GPIO_INT_EN],
            _ => self.regs[i],
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        let i = (off as usize / 4).min(0x33);
        let v = word_write(self.regs[i], off, size, value);
        // the _SET and _CLR registers after EN, OUT_EN, OUT, INT_EN, WAKE_EN,
        // EN1, EN2
        match i {
            1 | 4 | 0xe | 0x14 | 0x1b | 0x1e => self.regs[i - 1] |= value,
            2 | 5 | 0xf | 0x15 | 0x1c | 0x1f => self.regs[i - 2] &= !value,
            GPIO_OUT => self.out.set(v),
            7 => self.out.set(self.out.get() | value),
            8 => self.out.set(self.out.get() & !value),
            // INT_CLR
            0x12 => self.int_stat &= !value,
            _ => self.regs[i] = v,
        }
    }
    fn irq(&self) -> bool { self.stat() & self.regs[GPIO_INT_EN] != 0 }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- SPI

/// A device on an SPI controller's slave select: bytes in and out, with
/// the select's state.
pub trait SpiDevice {
    fn transfer(&mut self, b: u8) -> u8;
    fn deselect(&mut self) {}
    fn as_any(&mut self) -> &mut dyn Any;
}

/// The ST7735 display on SPI2, its data/command line on GPIO P1.6.
pub struct Display {
    pub lcd: crate::dev::st7735::St7735,
    dc: std::rc::Rc<std::cell::Cell<u32>>,
}

impl SpiDevice for Display {
    fn transfer(&mut self, b: u8) -> u8 {
        self.lcd.byte(b, self.dc.get() & 1 << 6 != 0);
        0xff
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

/// An SPI17Y controller as a master: bytes written to its FIFO are clocked
/// out at once, what comes back (if its RX FIFO's on) goes to the RX FIFO;
/// done (M_DONE) when the transaction's characters have gone.
pub struct Spi {
    ctrl0: u32,
    ctrl1: u32,
    dma: u32,
    int_fl: u32,
    int_en: u32,
    regs: [u32; 16],
    /// Characters sent since the transaction was set up (the FIFO is filled
    /// before START), and whether it's started
    sent: u32,
    started: bool,
    rx: std::collections::VecDeque<u8>,
    pub dev: Option<Box<dyn SpiDevice>>,
}

const SPI_M_DONE: u32 = 1 << 11;

impl Spi {
    fn new(dev: Option<Box<dyn SpiDevice>>) -> Spi {
        Spi { ctrl0: 0, ctrl1: 0, dma: 0, int_fl: 0, int_en: 0, regs: [0; 16], sent: 0, started: false, rx: Default::default(), dev }
    }
    fn send(&mut self, b: u8) {
        let r = self.dev.as_mut().map_or(0xff, |d| d.transfer(b));
        if self.dma & 1 << 22 != 0 {
            self.rx.push_back(r);
        }
        self.sent += 1;
        self.check_done();
    }
    fn check_done(&mut self) {
        let tx = self.ctrl1 & 0xffff;
        if self.started && tx > 0 && self.sent >= tx {
            self.done();
        }
    }
    fn done(&mut self) {
        self.int_fl |= SPI_M_DONE;
        self.started = false;
        self.sent = 0;
        self.ctrl0 &= !(1 << 5);
        // slave select deasserts unless held (SS_CTRL)
        if self.ctrl0 & 1 << 8 == 0 {
            if let Some(d) = self.dev.as_mut() {
                d.deselect();
            }
        }
    }
}

impl Device for Spi {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let v = match off & !3 {
            0x00 => {
                let mut v = 0;
                for k in 0..size {
                    v |= (self.rx.pop_front().unwrap_or(0xff) as u32) << (8 * k);
                }
                return v;
            }
            0x04 => self.ctrl0,
            0x08 => self.ctrl1,
            // the TX FIFO empty, the RX FIFO's count
            0x1c => (self.dma & !(0x3f << 8 | 0x3f << 24)) | (self.rx.len().min(32) as u32) << 24,
            // TX empty and below threshold; RX threshold if there's data
            0x20 => self.int_fl | 3 | if self.rx.is_empty() { 0 } else { 1 << 2 },
            0x24 => self.int_en,
            0x30 => 0,
            o => self.regs[(o / 4) as usize & 15],
        };
        word_read(v, off, size)
    }
    fn write(&mut self, off: u32, size: u32, value: u32, _now: u64) {
        match off & !3 {
            0x00 => {
                for k in 0..size {
                    self.send((value >> (8 * k)) as u8);
                }
            }
            0x04 => {
                let v = word_write(self.ctrl0, off, size, value);
                let start = v & 1 << 5 != 0 && self.ctrl0 & 1 << 5 == 0;
                self.ctrl0 = v;
                if start {
                    let tx = self.ctrl1 & 0xffff;
                    let rx = self.ctrl1 >> 16 & 0xffff;
                    self.started = true;
                    if tx == 0 {
                        // receive only: clock the characters in at once
                        for _ in 0..rx {
                            let r = self.dev.as_mut().map_or(0xff, |d| d.transfer(0xff));
                            self.rx.push_back(r);
                        }
                        self.done();
                    } else {
                        self.check_done();
                    }
                }
            }
            0x08 => {
                self.ctrl1 = word_write(self.ctrl1, off, size, value);
                if !self.started {
                    self.sent = 0;
                }
            }
            0x1c => {
                let v = word_write(self.dma, off, size, value);
                if v & 1 << 23 != 0 {
                    self.rx.clear();
                }
                self.dma = v & !(1 << 7 | 1 << 23);
            }
            0x20 => self.int_fl &= !word_write(0, off, size, value),
            0x24 => self.int_en = word_write(self.int_en, off, size, value),
            o => {
                let i = (o / 4) as usize & 15;
                self.regs[i] = word_write(self.regs[i], off, size, value);
            }
        }
    }
    fn irq(&self) -> bool { (self.int_fl | 3) & self.int_en != 0 }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

// ---------------------------------------------------------------- devices on I2C

/// Interrupt lines from devices on I2C to GPIO pins (low when asserted):
/// devices set them, the machine passes changes on to the GPIO ports.
#[derive(Default)]
pub struct Lines {
    /// The port expander's INT (P1.7)
    pub pe: std::cell::Cell<bool>,
    /// The PMIC's nIRQ (P0.12)
    pub pmic: std::cell::Cell<bool>,
    pub dirty: std::cell::Cell<bool>,
}

impl Lines {
    fn set(&self, line: &std::cell::Cell<bool>, v: bool) {
        if line.get() != v {
            line.set(v);
            self.dirty.set(true);
        }
    }
}

/// The PCAL6408A port expander: 8 pins, the buttons (but the top left
/// one) on inputs 3, 5 and 6, active low. An input's change sets its
/// interrupt status (unless masked) and pulls INT low, until the input
/// port is read.
pub struct Pcal6408a {
    regs: [u8; 256],
    ptr: u8,
    first: bool,
    /// The pins' levels, as the outside world drives them
    pub pins: u8,
    lines: std::rc::Rc<Lines>,
}

impl Pcal6408a {
    /// Buttons held (BUTTON_* bits: 1 left bottom, 2 right bottom, 4 right
    /// top).
    pub fn set_buttons(&mut self, held: u8) {
        let mut pins = 0xff;
        for (bit, pin) in [(1, 5), (2, 3), (4, 6)] {
            if held & bit != 0 {
                pins &= !(1 << pin);
            }
        }
        let changed = (pins ^ self.pins) & self.regs[0x03];
        self.pins = pins;
        if self.regs[0x42] & changed != 0 || changed != 0 {
            self.regs[0x00] = (self.regs[0x00] & !changed) | (pins & changed);
        }
        self.regs[0x46] |= changed & !self.regs[0x45];
        self.irq();
    }
    fn irq(&self) { self.lines.set(&self.lines.pe, self.regs[0x46] != 0); }
}

impl I2cDevice for Pcal6408a {
    fn start(&mut self, read: bool) { self.first = !read; }
    fn write(&mut self, b: u8) {
        if self.first {
            self.ptr = b;
            self.first = false;
            return;
        }
        let r = self.ptr as usize;
        match r {
            0x00 | 0x46 => {}
            // masking an interrupt clears it
            0x45 => {
                self.regs[r] = b;
                self.regs[0x46] &= !b;
            }
            _ => self.regs[r] = b,
        }
        self.irq();
        self.ptr = self.ptr.wrapping_add(1);
    }
    fn read(&mut self) -> u8 {
        let r = self.ptr as usize;
        let v = match r {
            // inputs as driven, outputs as set; reading clears the interrupt
            0x00 => {
                let v = (self.pins & self.regs[0x03]) | (self.regs[0x01] & !self.regs[0x03]);
                self.regs[0x00] = v;
                self.regs[0x46] = 0;
                self.irq();
                v
            }
            _ => self.regs[r],
        };
        self.ptr = self.ptr.wrapping_add(1);
        v
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

/// The MAX77650 PMIC's 7-bit address
pub const PMIC_ADDR: u8 = 0x48;
/// The PCAL6408A port expander (the buttons but one; the display's reset)
pub const PE_ADDR: u8 = 0x21;

/// The MAX77650 PMIC: its registers (from its datasheet's reset values),
/// and its analog multiplexer's selection (CNFG_CHG_I's MUX_SEL), which
/// is what the MAX32665's ADC channel 0 measures.
pub struct Pmic {
    pub regs: I2cRegs,
    pub amux: std::rc::Rc<std::cell::Cell<u8>>,
    lines: std::rc::Rc<Lines>,
}

impl Pmic {
    /// The power button (nEN, the top left one) held or not: STAT_GLBL's
    /// debounced status, and INT_GLBL's falling (pressed) or rising
    /// (released) edge.
    pub fn set_button(&mut self, held: bool) {
        let r = &mut self.regs.regs;
        if (r[0x05] & 4 != 0) == held {
            return;
        }
        r[0x05] = (r[0x05] & !4) | (held as u8) << 2;
        r[0x00] |= if held { 1 << 2 } else { 1 << 3 };
        self.irq();
    }
    /// nIRQ: low while an unmasked interrupt is flagged.
    fn irq(&self) { self.lines.set(&self.lines.pmic, self.regs.regs[0x00] & !self.regs.regs[0x06] != 0); }
}

impl I2cDevice for Pmic {
    fn start(&mut self, read: bool) { self.regs.start(read) }
    fn write(&mut self, b: u8) {
        self.regs.write(b);
        self.amux.set(self.regs.regs[0x20] & 0xf);
        self.irq();
    }
    fn read(&mut self) -> u8 {
        let ptr = self.regs.ptr;
        let v = self.regs.read();
        // INT_GLBL clears when read
        if ptr == 0x00 {
            self.regs.regs[0x00] = 0;
            self.irq();
        }
        v
    }
    fn as_any(&mut self) -> &mut dyn Any { self }
}

/// The PMIC's registers: a charged battery, nothing on USB, the power
/// button released.
fn pmic_regs() -> I2cRegs {
    I2cRegs::new(
        "MAX77650",
        &[
            // STAT_GLBL: nEN (the button) high; DIDM 0 (MAX77650)
            (0x05, 0x00),
            (0x06, 0xff),
            (0x07, 0xff),
            // CID
            (0x11, 0x08),
            (0x12, 0x01),
            (0x18, 0x0f),
            (0x1a, 0xf8),
            (0x1b, 0x10),
            (0x1c, 0x05),
            (0x1d, 0x04),
            (0x20, 0xf0),
        ],
    )
}

// ---------------------------------------------------------------- the machine

/// The card10: its two cores and what's shared between them.
pub struct Card10 {
    /// Core 0 and core 1. The one that ran last holds the bus; the other's
    /// `bus` is a stand-in holding only its own SCS.
    pub cores: [Cpu; 2],
    /// Which core holds the bus
    holder: usize,
    /// Core 1 is running (its clock's on)
    pub core1_on: bool,
    /// Instructions per slice before the other core runs
    pub slice: u64,
    /// Interrupt lines from I2C devices to GPIO pins
    pub lines: std::rc::Rc<Lines>,
}

impl Card10 {
    /// The bus (whichever core holds it).
    pub fn bus(&mut self) -> &mut Bus { &mut self.cores[self.holder].bus }

    /// Give the bus to core `c`, with its own SCS in it.
    fn hand_bus(&mut self, c: usize) {
        if c == self.holder {
            return;
        }
        let (a, b) = self.cores.split_at_mut(1);
        let (from, to) = if c == 1 { (&mut a[0], &mut b[0]) } else { (&mut b[0], &mut a[0]) };
        std::mem::swap(&mut from.bus.scs, &mut to.bus.scs);
        std::mem::swap(&mut from.bus, &mut to.bus);
        self.holder = c;
        let bus = &mut self.cores[c].bus;
        bus.sync_all_irqs();
        bus.scs.dirty = true;
    }

    fn gcr(&mut self) -> &mut Gcr { self.bus().device::<Gcr>(GCR_BASE).expect("GCR") }

    /// The machine's time (ns): the core furthest on.
    pub fn now_ns(&self) -> u64 { self.cores[0].now_ns().max(if self.core1_on { self.cores[1].now_ns() } else { 0 }) }

    /// Start core 1 from the vector table at `ivt`.
    fn start_core1(&mut self, ivt: u32) {
        self.hand_bus(1);
        let now = self.cores[0].now_ns();
        let c = &mut self.cores[1];
        c.reset_at(ivt);
        c.warp_ns += now.saturating_sub(c.now_ns());
        self.core1_on = true;
    }

    /// Run up to `budget` instructions (on the two cores together).
    pub fn run(&mut self, budget: u64) -> Stop {
        let mut left = budget;
        while left > 0 {
            // core 1's clock, as core 0 last set it
            match (self.gcr().core1(), self.core1_on) {
                (Some(ivt), false) => self.start_core1(ivt),
                (None, true) => self.core1_on = false,
                _ => {}
            }
            if std::mem::take(&mut self.gcr().system_reset) {
                return Stop::Reset;
            }
            if self.lines.dirty.take() {
                self.sync_lines();
            }
            let mut idle = [false, !self.core1_on];
            for c in 0..2 {
                if idle[c] {
                    continue;
                }
                self.hand_bus(c);
                // a core that slept catches up with the other
                let other = self.cores[1 - c].now_ns();
                let core = &mut self.cores[c];
                if core.sleeping && other > core.now_ns() && (c == 0 || self.core1_on) {
                    core.warp_ns += other - core.now_ns();
                }
                let before = core.icount;
                let stop = core.run(self.slice.min(left));
                left = left.saturating_sub(core.icount - before);
                // SEV wakes the other core's WFE
                if std::mem::take(&mut self.cores[c].sev) && self.gcr().tx_event(c) {
                    let o = &mut self.cores[1 - c];
                    o.event = true;
                }
                match stop {
                    Stop::Idle => idle[c] = true,
                    Stop::Budget => {}
                    s => {
                        if c == 1 && s == Stop::Reset {
                            // core 1's SYSRESETREQ: just core 1
                            let ivt = self.cores[1].bus.scs.vtor;
                            self.cores[1].reset_at(ivt);
                            continue;
                        }
                        self.hand_bus(0);
                        return s;
                    }
                }
            }
            if idle[0] && idle[1] {
                if !self.skip_idle() {
                    return Stop::Idle;
                }
            }
        }
        Stop::Budget
    }

    /// When the next event is due (ns): either core's SysTick, or a device.
    pub fn next_event_ns(&mut self) -> u64 {
        let mut next = self.bus().next_event_ns();
        let parked = 1 - self.holder;
        if parked == 0 || self.core1_on {
            next = next.min(self.cores[parked].bus.scs.next_event_ns());
        }
        next
    }

    /// While both cores wait: move their clocks on to `target` (ns), bring
    /// the devices up to it and wake for what's due.
    pub fn advance_to(&mut self, target: u64) {
        for c in 0..2 {
            if c == 1 && !self.core1_on {
                continue;
            }
            let core = &mut self.cores[c];
            if target > core.now_ns() {
                core.warp_ns += target - core.now_ns();
            }
        }
        for c in [1 - self.holder, self.holder] {
            if c == 1 && !self.core1_on {
                continue;
            }
            self.hand_bus(c);
            let core = &mut self.cores[c];
            let now = core.now_ns();
            core.bus.update(now);
            core.bus.scs.dirty = true;
            core.wake_check();
        }
    }

    /// The host changed a device's input (a UART's): wake for its interrupt.
    pub fn input_changed(&mut self) {
        let bus = self.bus();
        bus.sync_all_irqs();
        bus.scs.dirty = true;
        for c in self.cores.iter_mut() {
            c.wake_check();
        }
    }

    /// Pass the I2C devices' interrupt lines on to their GPIO pins.
    fn sync_lines(&mut self) {
        let (pe, pmic) = (self.lines.pe.get(), self.lines.pmic.get());
        let bus = self.bus();
        for (port, pin, low) in [(1, 7, pe), (0, 12, pmic)] {
            let g = bus.device::<Gpio>(GPIO_BASE[port]).expect("GPIO");
            let v = if low { g.input & !(1 << pin) } else { g.input | 1 << pin };
            g.set_input(v);
        }
        bus.sync_all_irqs();
        bus.scs.dirty = true;
    }

    /// The buttons held: BUTTON_* bits (1 left bottom, 2 right bottom,
    /// 4 right top, 8 left top: the PMIC's power button).
    pub fn set_buttons(&mut self, held: u8) {
        let i2c = self.bus().device::<I2c>(I2C_BASE[1]).expect("I2C1");
        i2c.device::<Pcal6408a>(PE_ADDR).expect("port expander").set_buttons(held);
        i2c.device::<Pmic>(PMIC_ADDR).expect("PMIC").set_button(held & 8 != 0);
        if self.lines.dirty.take() {
            self.sync_lines();
        }
        for c in self.cores.iter_mut() {
            c.wake_check();
        }
    }

    /// Both cores waiting: move time on to the next event (either core's
    /// SysTick or a device), for both. False if nothing is scheduled.
    pub fn skip_idle(&mut self) -> bool {
        let next = self.next_event_ns();
        if next == u64::MAX {
            return false;
        }
        self.advance_to(next);
        true
    }
}

/// The display.
pub fn display(m: &mut Card10) -> &mut crate::dev::st7735::St7735 {
    let spi = m.bus().device::<Spi>(SPI_BASE[2]).expect("SPI2");
    &mut spi.dev.as_mut().unwrap().as_any().downcast_mut::<Display>().unwrap().lcd
}

/// The external flash (its FAT file system).
pub fn ext_flash(m: &mut Card10) -> std::cell::RefMut<'_, Mx25> {
    let r = m.bus().device::<SpixfcRegs>(SPIXFC_BASE).expect("SPIXFC");
    std::cell::RefMut::map(r.0.borrow_mut(), |s| &mut s.flash)
}

/// Build the card10: Epicardium (core 0's firmware, at 0x1001_0000) and
/// Pycardium (core 1's, at 0x1008_0000) from their ELF files, the external
/// flash's image (its FAT file system); the RTC from
/// `epoch` (UTC seconds). Core 0 starts at Epicardium's vector table, as the
/// bootloader leaves it.
pub fn machine(epicardium: &[u8], pycardium: &[u8], ext_flash: &[u8], epoch: i64) -> Result<Card10, String> {
    let mut bus = Bus::new(FLASH_SIZE, SRAM_SIZE, CPU_HZ, CPU_HZ, false);
    bus.flash_base = FLASH_BASE;
    bus.scs.ictr = 3;
    crate::elf::load(&mut bus, epicardium)?;
    if !pycardium.is_empty() {
        crate::elf::load(&mut bus, pycardium)?;
    }
    bus.map(GCR_BASE, 0x400, None, Box::new(Gcr::new()));
    bus.map(SEMA_BASE, 0x1000, None, Box::new(Sema { sema: [0; 8] }));
    for (i, &b) in UART_BASE.iter().enumerate() {
        bus.map(b, 0x1000, Some(UART_IRQ[i]), Box::new(Uart::new()));
    }
    for (i, &b) in TMR_BASE.iter().enumerate() {
        bus.map(b, 0x1000, Some(TMR_IRQ[i]), Box::new(Tmr::new(PCLK_HZ)));
    }
    {
        // The wake-up timer: a timer on the 32.768 kHz crystal
        bus.map(WUT_BASE, 0x400, Some(53), Box::new(Tmr::new(32_768)));
    }
    // GPIO: port 1's outputs drive the display's data/command line (P1.6);
    // inputs idle high (the pull-ups SystemInit turns on, and the open-drain
    // interrupt lines: the port expander's on P1.7, the PMIC's)
    let gpio_out = [std::rc::Rc::new(std::cell::Cell::new(0)), std::rc::Rc::new(std::cell::Cell::new(0))];
    for i in 0..2 {
        bus.map(GPIO_BASE[i], 0x1000, Some(GPIO_IRQ[i]), Box::new(Gpio::new(gpio_out[i].clone(), !0)));
    }
    for (i, &b) in SPI_BASE.iter().enumerate() {
        let dev: Option<Box<dyn SpiDevice>> = if i == 2 {
            let mut lcd = crate::dev::st7735::St7735::default();
            lcd.flip = true;
            Some(Box::new(Display { lcd, dc: gpio_out[1].clone() }))
        } else {
            None
        };
        bus.map(b, 0x1000, Some(SPI_IRQ[i]), Box::new(Spi::new(dev)));
    }
    let amux = std::rc::Rc::new(std::cell::Cell::new(0));
    let lines = std::rc::Rc::new(Lines::default());
    bus.map(ADC_BASE, 0x1000, Some(20), Box::new(Adc { ctrl: 0, data: 0, intr: 0, amux: amux.clone(), light: 200 }));
    for (i, &b) in I2C_BASE.iter().enumerate() {
        let mut i2c = I2c::new(["I2C0", "I2C1", "I2C2"][i]);
        if i == 1 {
            // The badge's own bus: the PMIC and the port expander
            i2c.attach(PMIC_ADDR, Box::new(Pmic { regs: pmic_regs(), amux: amux.clone(), lines: lines.clone() }));
            let mut regs = [0; 256];
            // inputs, outputs high, interrupts masked (its reset state)
            regs[0x00] = 0xff;
            regs[0x01] = 0xff;
            regs[0x03] = 0xff;
            regs[0x45] = 0xff;
            i2c.attach(PE_ADDR, Box::new(Pcal6408a { regs, ptr: 0, first: false, pins: 0xff, lines: lines.clone() }));
        }
        bus.map(b, 0x1000, Some(I2C_IRQ[i]), Box::new(i2c));
    }
    bus.map(0x4004_d000, 0x1000, Some(4), Box::new(Trng { cn: 0, state: 0x9e37_79b9_7f4a_7c15 ^ epoch as u64 }));
    let spixfc = std::rc::Rc::new(std::cell::RefCell::new(SpixfcState {
        flash: Mx25::new(ext_flash, EXT_FLASH_SIZE),
        regs: [0; 8],
        tx_left: 0,
        deass: false,
        selected: false,
        rx: Default::default(),
    }));
    bus.map(SPIXFC_BASE, 0x1000, Some(38), Box::new(SpixfcRegs(spixfc.clone())));
    bus.map(0x400b_c000, 0x1000, None, Box::new(SpixfcFifo(spixfc)));
    bus.map(RTC_BASE, 0x400, Some(RTC_IRQ), Box::new(Rtc { base: epoch << 12, ctrl: 0, toda: 0, sseca: 0, regs: [0; 2] }));
    // Instruction caches: ready
    bus.map(0x4002_a000, 0x400, None, Box::new(Regs::new("ICC0", &[(0x100, 1 << 16)])));
    bus.map(0x4002_a800, 0x400, None, Box::new(Regs::new("ICC1", &[(0x100, 1 << 16)])));
    // The SIMO regulator: its outputs up (VREGO_B high enough for 96 MHz)
    bus.map(0x4000_4400, 0x400, None, Box::new(Regs::new("SIMO", &[(0x40, 0xf)]).with(0x8, 0x60)));
    // Everything else: registers that hold what's written
    bus.map(0x4000_0000, 0x0020_0000, None, Box::new(Regs::new("periph", &[])));

    let mut c0 = Cpu::new(bus);
    c0.fpu = true;
    c0.reset_at(FLASH_BASE + 0x1_0000);
    let mut c1 = Cpu::new(Bus::new(0, 0, CPU_HZ, CPU_HZ, false));
    c1.fpu = true;
    c1.bus.scs.ictr = 3;
    Ok(Card10 { cores: [c0, c1], holder: 0, core1_on: false, slice: 1000, lines })
}

/// The catch-all register file, to log what the firmware touches.
pub fn log_registers(m: &mut Card10, on: bool) {
    for map in m.bus().maps.iter_mut() {
        if let Some(r) = map.dev.as_any().downcast_mut::<Regs>() {
            r.log = on;
        }
    }
}

/// Log the I2C buses' transfers.
pub fn log_i2c(m: &mut Card10, on: bool) {
    for b in I2C_BASE {
        if let Some(i) = m.bus().device::<I2c>(b) {
            i.log = on;
        }
    }
}
