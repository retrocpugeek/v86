//! PebbleOS's emulator machine ("pebble-generic", the qemu_flint,
//! qemu_emery and qemu_gabbro boards): not a model of a watch's chip but a
//! small set of devices made for emulation, each with a simple driver in
//! PebbleOS (drivers/*/qemu*). Register layouts and behaviour follow Pebble's
//! QEMU fork (github.com/coredevices/qemu, hw/arm/pebble_generic.c and the
//! hw/*/pebble_*.c devices it maps), which is the reference to compare with.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::bus::{le, set_le, Bus, Device};
use crate::cpu::Cpu;

pub const UART_BASE: [u32; 4] = [0x4000_0000, 0x4000_1000, 0x4000_2000, 0x4001_4000];
pub const UART_IRQ: [usize; 4] = [0, 1, 2, 12];
pub const TIMER_BASE: [u32; 2] = [0x4000_3000, 0x4000_4000];
pub const TIMER_IRQ: [usize; 2] = [3, 4];
pub const RTC_BASE: u32 = 0x4000_5000;
pub const RTC_IRQ: usize = 5;
pub const GPIO_BASE: u32 = 0x4000_6000;
pub const GPIO_IRQ: usize = 6;
pub const SYSCTRL_BASE: u32 = 0x4000_7000;
pub const DISPLAY_BASE: u32 = 0x4000_8000;
pub const DISPLAY_FB_BASE: u32 = 0x5000_0000;
pub const DISPLAY_IRQ: usize = 7;
pub const EXTFLASH_CTRL_BASE: u32 = 0x4001_0000;
pub const EXTFLASH_BASE: u32 = 0x1000_0000;
pub const EXTFLASH_SIZE: usize = 32 << 20;
pub const TOUCH_BASE: u32 = 0x4001_1000;
pub const TOUCH_IRQ: usize = 9;
pub const AUDIO_BASE: u32 = 0x4001_2000;
pub const AUDIO_IRQ: usize = 10;
pub const WDOG_BASE: u32 = 0x4001_3000;

pub const SYSCLK_HZ: u64 = 64_000_000;
pub const REFCLK_HZ: u64 = 1_000_000;

/// The watch platforms (SDK names), and their emulator boards' hardware
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Board {
    /// Pebble 2 Duo: Cortex-M4, 144x168 1-bit, audio
    Flint,
    /// Pebble Time 2: Cortex-M33, 200x228 8-bit, touch, audio
    Emery,
    /// Pebble Round 2: Cortex-M33, 260x260 round 8-bit, touch
    Gabbro,
}

pub struct BoardConfig {
    pub board_id: u32,
    pub ram_size: usize,
    pub flash_size: usize,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub round: bool,
    pub touch: bool,
    pub audio: bool,
}

impl Board {
    pub fn config(self) -> BoardConfig {
        match self {
            Board::Flint => BoardConfig { board_id: 1, ram_size: 256 << 10, flash_size: 4 << 20, width: 144, height: 168, bpp: 1, round: false, touch: false, audio: true },
            Board::Emery => BoardConfig { board_id: 2, ram_size: 512 << 10, flash_size: 4 << 20, width: 200, height: 228, bpp: 8, round: false, touch: true, audio: true },
            Board::Gabbro => BoardConfig { board_id: 3, ram_size: 512 << 10, flash_size: 4 << 20, width: 260, height: 260, bpp: 8, round: true, touch: true, audio: false },
        }
    }
}

// ---------------------------------------------------------------- UART

/// pebble-simple-uart: DATA, STATE (TX ready, RX available), CTRL (TX/RX
/// interrupt enables), INTSTAT (TX complete, RX ready).
pub struct Uart {
    pub rx: VecDeque<u8>,
    pub tx: Vec<u8>,
    ctrl: u32,
    intstat: u32,
}

impl Default for Uart {
    fn default() -> Uart { Uart { rx: VecDeque::new(), tx: Vec::new(), ctrl: 0, intstat: 0 } }
}

impl Uart {
    /// Bytes from the host (up to the 256-byte FIFO).
    pub fn input(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.rx.len() < 256 {
                self.rx.push_back(b);
            }
        }
    }
}

impl Device for Uart {
    fn read(&mut self, off: u32, _size: u32, _now: u64) -> u32 {
        match off {
            0x00 => self.rx.pop_front().unwrap_or(0) as u32,
            0x04 => 1 | ((!self.rx.is_empty()) as u32) << 1,
            0x08 => self.ctrl,
            0x0c => self.intstat | ((!self.rx.is_empty()) as u32) << 1,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, _now: u64) {
        match off {
            0x00 => {
                self.tx.push(v as u8);
                self.intstat |= 1;
            }
            0x08 => self.ctrl = v & 3,
            0x0c => self.intstat &= !v,
            _ => {}
        }
    }

    fn irq(&self) -> bool {
        let rx = !self.rx.is_empty();
        (self.ctrl & 1 != 0 && self.intstat & 1 != 0) || (self.ctrl & 2 != 0 && (self.intstat & 2 != 0 || rx))
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- timer

/// pebble-timer: counts LOAD down at SYSCLK / (PRESCALER + 1); fires at 0
/// (INTSTAT), reloading unless one-shot.
#[derive(Default)]
pub struct Timer {
    load: u32,
    ctrl: u32,
    intstat: u32,
    prescaler: u32,
    /// The current period started then (ns), and it fires at `due`
    start: u64,
    due: Option<u64>,
}

impl Timer {
    fn period_ns(&self) -> u64 {
        if self.load == 0 {
            return 0;
        }
        (self.load as u128 * (self.prescaler as u128 + 1) * 1_000_000_000 / SYSCLK_HZ as u128) as u64
    }

    fn start(&mut self, now: u64) {
        let p = self.period_ns();
        self.start = now;
        self.due = if p > 0 { Some(now + p) } else { None };
    }
}

impl Device for Timer {
    fn read(&mut self, off: u32, _size: u32, now: u64) -> u32 {
        match off {
            0x00 => self.load,
            0x04 => {
                let p = self.period_ns();
                if self.ctrl & 1 == 0 || p == 0 {
                    return 0;
                }
                let elapsed = (now.saturating_sub(self.start) as u128 * self.load as u128 / p as u128) as u64;
                self.load.saturating_sub(elapsed.min(self.load as u64) as u32)
            }
            0x08 => self.ctrl,
            0x0c => self.intstat,
            0x10 => self.prescaler,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, now: u64) {
        match off {
            0x00 => {
                self.load = v;
                if self.ctrl & 1 != 0 {
                    self.start(now);
                }
            }
            0x08 => {
                let old = self.ctrl;
                self.ctrl = v & 7;
                if self.ctrl & 1 != 0 && old & 1 == 0 {
                    self.start(now);
                } else if self.ctrl & 1 == 0 && old & 1 != 0 {
                    self.due = None;
                }
            }
            0x0c => self.intstat &= !v,
            0x10 => {
                self.prescaler = v;
                if self.ctrl & 1 != 0 {
                    self.start(now);
                }
            }
            _ => {}
        }
    }

    fn update(&mut self, now: u64) {
        while let Some(t) = self.due {
            if now < t {
                break;
            }
            self.intstat |= 1;
            if self.ctrl & 4 == 0 && self.ctrl & 1 != 0 {
                let p = self.period_ns();
                self.start = t;
                self.due = if p > 0 { Some(t + p) } else { None };
            } else {
                self.ctrl &= !1;
                self.due = None;
            }
        }
    }

    fn next_event_ns(&self) -> u64 { self.due.unwrap_or(u64::MAX) }

    fn irq(&self) -> bool { self.ctrl & 2 != 0 && self.intstat & 1 != 0 }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- RTC

/// pebble-rtc: the host's time (UTC seconds) plus the firmware's offset, an
/// alarm, a 1 kHz tick counter and 16 backup registers. Backup register 0
/// holds the emulator's settings for the firmware.
pub struct Rtc {
    /// The host's time at virtual time 0 (seconds), so time moves with the
    /// machine
    pub epoch: i64,
    offset: i64,
    alarm: u32,
    ctrl: u32,
    pub backup: [u32; 16],
}

impl Rtc {
    pub fn new(epoch: i64) -> Rtc {
        let mut backup = [0; 16];
        // QEMU_SETTING_START_CONNECTED, as Pebble's QEMU sets by default
        backup[0] = 2;
        Rtc { epoch, offset: 0, alarm: 0, ctrl: 0, backup }
    }

    fn time(&self, now: u64) -> i64 { self.epoch + (now / 1_000_000_000) as i64 + self.offset }

    fn alarm_due(&self, now: u64) -> bool { self.ctrl & 1 != 0 && self.time(now) as u32 >= self.alarm }
}

impl Device for Rtc {
    fn read(&mut self, off: u32, _size: u32, now: u64) -> u32 {
        match off {
            0x00 => self.time(now) as u32,
            0x04 => (self.time(now) as u64 >> 32) as u32,
            0x08 => self.alarm,
            0x0c => self.ctrl,
            0x10 => (now / 1_000_000) as u32,
            0x40..=0x7c => self.backup[((off - 0x40) / 4) as usize],
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, now: u64) {
        match off {
            0x00 => {
                let t = (self.time(now) as u64 & !0xffff_ffff) | v as u64;
                self.offset = t as i64 - self.epoch - (now / 1_000_000_000) as i64;
            }
            0x04 => {
                let t = (self.time(now) as u64 & 0xffff_ffff) | (v as u64) << 32;
                self.offset = t as i64 - self.epoch - (now / 1_000_000_000) as i64;
            }
            0x08 => self.alarm = v,
            0x0c => {
                if v & 2 != 0 {
                    self.ctrl &= !2;
                }
                self.ctrl = (self.ctrl & 2) | (v & 1);
            }
            0x40..=0x7c => self.backup[((off - 0x40) / 4) as usize] = v,
            _ => {}
        }
    }

    fn update(&mut self, now: u64) {
        if self.alarm_due(now) {
            self.ctrl |= 2;
        }
    }

    fn next_event_ns(&self) -> u64 {
        if self.ctrl & 1 == 0 || self.ctrl & 2 != 0 {
            return u64::MAX;
        }
        // The second the alarm is due
        let secs = self.alarm as i64 - self.epoch - self.offset;
        if secs <= 0 {
            0
        } else {
            secs as u64 * 1_000_000_000
        }
    }

    fn irq(&self) -> bool { self.ctrl & 1 != 0 && self.ctrl & 2 != 0 }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- buttons

pub const BTN_BACK: u32 = 1;
pub const BTN_UP: u32 = 2;
pub const BTN_SELECT: u32 = 4;
pub const BTN_DOWN: u32 = 8;

/// pebble-gpio: four buttons, their state and edge flags, an edge interrupt.
#[derive(Default)]
pub struct Gpio {
    pub state: u32,
    edge: u32,
    intctrl: u32,
}

impl Gpio {
    /// The host presses or releases buttons.
    pub fn set_buttons(&mut self, state: u32) {
        let changed = (state ^ self.state) & 0xf;
        self.edge |= changed;
        self.state = state & 0xf;
    }
}

impl Device for Gpio {
    fn read(&mut self, off: u32, _size: u32, _now: u64) -> u32 {
        match off {
            0x00 => self.state,
            0x04 | 0x0c => self.edge,
            0x08 => self.intctrl,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, _now: u64) {
        match off {
            0x04 | 0x0c => self.edge &= !(v & 0xf),
            0x08 => self.intctrl = v & 1,
            _ => {}
        }
    }

    fn irq(&self) -> bool { self.intctrl & 1 != 0 && self.edge != 0 }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- sysctrl

/// pebble-sysctrl: board identification and the reset reason.
pub struct SysCtrl {
    cfg: [u32; 5],
    pub reset_reason: u32,
}

impl Device for SysCtrl {
    fn read(&mut self, off: u32, _size: u32, _now: u64) -> u32 {
        match off {
            0x00..=0x10 => self.cfg[(off / 4) as usize],
            0x14 => self.reset_reason,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, _now: u64) {
        if off == 0x14 {
            self.reset_reason &= !v;
        }
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- display

/// pebble-display: a framebuffer (1-bit, rows padded to 32 bits, LSB first;
/// or 8-bit ARGB2222) shown when the firmware requests an update, a
/// backlight (brightness and an RGB colour) and an update-done interrupt.
pub struct DisplayState {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub round: bool,
    pub fb: Vec<u8>,
    ctrl: u32,
    pub brightness: u32,
    intstat: u32,
    intctrl: u32,
    pub backlight: [u8; 3],
    /// Changes when an update is requested (or the backlight changes)
    pub frame: u32,
}

/// The rows of one quadrant of the 260x260 round display: pixels masked from
/// the edge inwards (as PebbleOS's g_gbitmap_data_row_infos).
const ROUND_MASK: [u8; 130] = [
    113, 107, 102, 98, 94, 90, 87, 85, 82, 80, 77, 75, 73, 71, 69, 67, 65, 64, 62, 60, 59, 57, 56, 54, 53, 52,
    50, 49, 48, 46, 45, 44, 43, 42, 41, 40, 39, 38, 37, 36, 35, 34, 33, 32, 31, 30, 29, 29, 28, 27, 26, 26,
    25, 24, 23, 23, 22, 21, 21, 20, 19, 19, 18, 18, 17, 16, 16, 15, 15, 14, 14, 13, 13, 12, 12, 11, 11, 10,
    10, 10, 9, 9, 8, 8, 8, 7, 7, 6, 6, 6, 5, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3, 3, 2, 2,
    2, 2, 2, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// How much ambient light a transflective panel reflects (0..255)
const AMBIENT_FLOOR: u32 = 100;

impl DisplayState {
    fn new(width: u32, height: u32, bpp: u32, round: bool) -> DisplayState {
        let size = if bpp == 1 { width.div_ceil(32) * 4 * height } else { width * height };
        DisplayState {
            width,
            height,
            bpp,
            round,
            fb: vec![0; size as usize],
            ctrl: 0,
            brightness: 0xff,
            intstat: 0,
            intctrl: 0,
            backlight: [0xff; 3],
            frame: 0,
        }
    }

    pub fn enabled(&self) -> bool { self.ctrl & 1 != 0 }

    /// The screen as 0xRRGGBB pixels, as Pebble's QEMU draws it (its
    /// non-sunlight palette, with the backlight over the ambient light).
    pub fn render(&self, out: &mut [u32]) {
        let (w, h) = (self.width as usize, self.height as usize);
        let bl_max = self.backlight.iter().copied().max().unwrap_or(0) as u32;
        let amb = AMBIENT_FLOOR * (255 - bl_max) / 255;
        let eff = self.backlight.map(|c| (amb + c as u32).min(255));
        for y in 0..h {
            for x in 0..w {
                let (mut r, mut g, mut b);
                if self.round && {
                    let qx = if x < w / 2 { x } else { w - 1 - x };
                    let qy = if y < h / 2 { y } else { h - 1 - y };
                    qx < ROUND_MASK.get(qy).copied().unwrap_or(0) as usize
                } {
                    out[y * w + x] = 0;
                    continue;
                }
                if self.bpp == 1 {
                    let row = w.div_ceil(32) * 4;
                    let on = self.fb[y * row + x / 8] >> (x & 7) & 1 != 0;
                    let level = if on { if self.brightness != 0 { self.brightness } else { 0xff } } else { 0 };
                    r = level;
                    g = level;
                    b = level;
                } else {
                    let p = self.fb[y * w + x] as u32;
                    let ex = |c: u32| c << 6 | c << 4 | c << 2 | c;
                    r = ex(p >> 4 & 3);
                    g = ex(p >> 2 & 3);
                    b = ex(p & 3);
                    if self.brightness > 0 && self.brightness < 255 {
                        r = r * self.brightness / 255;
                        g = g * self.brightness / 255;
                        b = b * self.brightness / 255;
                    }
                }
                r = r * eff[0] / 255;
                g = g * eff[1] / 255;
                b = b * eff[2] / 255;
                out[y * w + x] = r << 16 | g << 8 | b;
            }
        }
    }
}

pub struct DisplayRegs(pub Rc<RefCell<DisplayState>>);
pub struct DisplayFb(pub Rc<RefCell<DisplayState>>);

impl Device for DisplayRegs {
    fn read(&mut self, off: u32, _size: u32, _now: u64) -> u32 {
        let s = self.0.borrow();
        match off {
            0x00 => s.ctrl,
            0x04 => 0,
            0x08 => s.width,
            0x0c => s.height,
            0x10 => s.bpp,
            0x14 => s.round as u32,
            0x18 => s.brightness,
            0x1c => s.intstat,
            0x20 => s.intctrl,
            0x24 => s.backlight[0] as u32,
            0x28 => s.backlight[1] as u32,
            0x2c => s.backlight[2] as u32,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, _now: u64) {
        let mut s = self.0.borrow_mut();
        match off {
            0x00 => {
                s.ctrl = v & 1;
                if v & 2 != 0 {
                    s.frame = s.frame.wrapping_add(1);
                    s.intstat |= 1;
                }
            }
            0x18 => {
                s.brightness = v & 0xff;
                s.frame = s.frame.wrapping_add(1);
            }
            0x1c => s.intstat &= !v,
            0x20 => s.intctrl = v & 1,
            0x24..=0x2c => {
                s.backlight[((off - 0x24) / 4) as usize] = v as u8;
                s.frame = s.frame.wrapping_add(1);
            }
            _ => {}
        }
    }

    fn irq(&self) -> bool {
        let s = self.0.borrow();
        s.intctrl & 1 != 0 && s.intstat & 1 != 0
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

impl Device for DisplayFb {
    fn read(&mut self, off: u32, size: u32, _now: u64) -> u32 {
        let s = self.0.borrow();
        let o = off as usize;
        if o + size as usize <= s.fb.len() {
            le(&s.fb[o..], size)
        } else {
            0
        }
    }

    fn write(&mut self, off: u32, size: u32, v: u32, _now: u64) {
        let mut s = self.0.borrow_mut();
        let o = off as usize;
        if o + size as usize <= s.fb.len() {
            set_le(&mut s.fb[o..], size, v);
        }
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- external flash

/// pebble-extflash's control registers: erases (4 KB subsectors, 64 KB
/// sectors) of the XIP memory (Bus::xip), and SYNC (persistence: here the
/// host keeps the ranges written, to save them if it likes).
pub struct ExtFlashCtrl {
    addr: u32,
    sync_len: u32,
    /// Erases for the bus to do (offset, length), and synced ranges
    pub erases: Vec<(usize, usize)>,
    pub synced: Vec<(usize, usize)>,
}

impl Device for ExtFlashCtrl {
    fn read(&mut self, off: u32, _size: u32, _now: u64) -> u32 {
        match off {
            0x04 => self.addr,
            0x14 => EXTFLASH_SIZE as u32,
            0x18 => self.sync_len,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, _now: u64) {
        match off {
            0x00 => {
                let size = match v {
                    1 => 0x1000,
                    2 => 0x1_0000,
                    _ => return,
                };
                if self.addr >= EXTFLASH_BASE {
                    let o = (self.addr - EXTFLASH_BASE) as usize & !(size - 1);
                    if o + size <= EXTFLASH_SIZE {
                        self.erases.push((o, size));
                    }
                }
            }
            0x04 => self.addr = v,
            0x18 => self.sync_len = v,
            0x1c => {
                if v >= EXTFLASH_BASE {
                    self.synced.push(((v - EXTFLASH_BASE) as usize, self.sync_len as usize));
                }
                self.sync_len = 0;
            }
            _ => {}
        }
    }

    fn xip_erases(&mut self) -> Vec<(usize, usize)> { std::mem::take(&mut self.erases) }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- audio

/// pebble-audio: 16-bit PCM samples pushed into a ring buffer, drained at
/// the sample rate; a buffer-available interrupt. The samples are kept for
/// the host to play.
pub struct Audio {
    ctrl: u32,
    rate: u32,
    intctrl: u32,
    intstat: u32,
    volume: u32,
    /// Samples buffered, as of `drained_at` (ns)
    level: u32,
    drained_at: u64,
    pub samples: Vec<i16>,
}

const AUDIO_RING: u32 = 4096;

impl Audio {
    fn drain(&mut self, now: u64) {
        if self.rate == 0 {
            self.drained_at = now;
            return;
        }
        let n = (now.saturating_sub(self.drained_at) as u128 * self.rate as u128 / 1_000_000_000) as u64;
        if n > 0 {
            self.level = self.level.saturating_sub(n.min(u32::MAX as u64) as u32);
            self.drained_at += (n as u128 * 1_000_000_000 / self.rate as u128) as u64;
        }
        if self.level <= AUDIO_RING / 2 {
            self.intstat |= 1;
        }
    }
}

impl Device for Audio {
    fn read(&mut self, off: u32, _size: u32, now: u64) -> u32 {
        self.drain(now);
        match off {
            0x00 => self.ctrl,
            0x04 => 1,
            0x08 => self.rate,
            0x10 => self.intctrl,
            0x14 => self.intstat,
            0x18 => AUDIO_RING - self.level,
            0x1c => self.volume,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, now: u64) {
        self.drain(now);
        match off {
            0x00 => self.ctrl = v & 1,
            0x08 => self.rate = v,
            0x0c => {
                if self.level < AUDIO_RING {
                    self.level += 1;
                    if self.samples.len() < 1 << 20 {
                        self.samples.push(v as u16 as i16);
                    }
                }
            }
            0x10 => self.intctrl = v & 1,
            0x14 => self.intstat &= !v,
            0x1c => self.volume = v,
            _ => {}
        }
    }

    fn update(&mut self, now: u64) { self.drain(now) }

    fn next_event_ns(&self) -> u64 {
        if self.intctrl & 1 == 0 || self.intstat & 1 != 0 || self.rate == 0 || self.level <= AUDIO_RING / 2 {
            return u64::MAX;
        }
        let n = (self.level - AUDIO_RING / 2) as u64;
        self.drained_at + n * 1_000_000_000 / self.rate as u64
    }

    fn irq(&self) -> bool { self.intctrl & 1 != 0 && self.intstat & 1 != 0 }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- touch

/// pebble-touch: one finger: down or not (STATE), its position (X, Y), and
/// an interrupt for each change (a press or release, or a move while down).
#[derive(Default)]
pub struct Touch {
    pub down: bool,
    pub x: u32,
    pub y: u32,
    intctrl: u32,
    intstat: u32,
}

impl Touch {
    /// The host's finger: down at (x, y) in display pixels, moved, or up.
    pub fn input(&mut self, down: bool, x: u32, y: u32) {
        let moved = x != self.x || y != self.y;
        self.x = x;
        self.y = y;
        if down != self.down || (down && moved) {
            self.intstat |= 1;
        }
        self.down = down;
    }
}

impl Device for Touch {
    fn read(&mut self, off: u32, _size: u32, _now: u64) -> u32 {
        match off {
            0x00 => self.down as u32,
            0x04 => self.x,
            0x08 => self.y,
            0x0c => self.intctrl,
            0x10 => self.intstat,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, _now: u64) {
        match off {
            0x0c => self.intctrl = v & 1,
            0x10 => self.intstat &= !v,
            _ => {}
        }
    }

    fn irq(&self) -> bool { self.intctrl & 1 != 0 && self.intstat & 1 != 0 }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- watchdog

/// ARM's CMSDK APB watchdog (as QEMU's cmsdk-apb-watchdog), clocked at
/// REFCLK: counts LOAD down; at 0 with INTEN it raises its interrupt (to the
/// system controller, not the NVIC) and reloads, and a second expiry with
/// RESEN resets the machine.
pub struct Watchdog {
    load: u32,
    control: u32,
    intstat: bool,
    locked: bool,
    /// The count was `value0` at `t0` (ns)
    value0: u32,
    t0: u64,
    pub reset: bool,
}

impl Watchdog {
    fn value(&self, now: u64) -> u32 {
        if self.control & 1 == 0 {
            return self.value0;
        }
        let ticks = (now.saturating_sub(self.t0) as u128 * REFCLK_HZ as u128 / 1_000_000_000) as u64;
        self.value0.saturating_sub(ticks.min(u32::MAX as u64) as u32)
    }
}

impl Device for Watchdog {
    fn read(&mut self, off: u32, _size: u32, now: u64) -> u32 {
        match off {
            0x000 => self.load,
            0x004 => self.value(now),
            0x008 => self.control,
            0x010 => self.intstat as u32,
            0x014 => (self.intstat && self.control & 1 != 0) as u32,
            0xc00 => self.locked as u32,
            // Peripheral and component IDs: CMSDK watchdog
            0xfe0 => 0x24,
            0xfe4 => 0xb8,
            0xfe8 => 0x1b,
            0xfec => 0x00,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, _size: u32, v: u32, now: u64) {
        if off == 0xc00 {
            self.locked = v != 0x1acc_e551;
            return;
        }
        if self.locked {
            return;
        }
        match off {
            0x000 => {
                self.load = v;
                self.value0 = v;
                self.t0 = now;
            }
            0x008 => {
                self.value0 = self.value(now);
                self.t0 = now;
                self.control = v & 3;
            }
            0x00c => {
                self.intstat = false;
                self.value0 = self.load;
                self.t0 = now;
            }
            _ => {}
        }
    }

    fn update(&mut self, now: u64) {
        if self.control & 1 == 0 {
            return;
        }
        while self.value(now) == 0 {
            // (the instant it reached 0)
            let at = self.t0 + (self.value0 as u128 * 1_000_000_000 / REFCLK_HZ as u128) as u64;
            if self.intstat && self.control & 2 != 0 {
                self.reset = true;
                self.value0 = self.load;
                self.t0 = at;
                return;
            }
            self.intstat = true;
            self.value0 = self.load.max(1);
            self.t0 = at;
        }
    }

    fn next_event_ns(&self) -> u64 {
        if self.control & 1 == 0 {
            return u64::MAX;
        }
        self.t0 + (self.value0 as u128 * 1_000_000_000 / REFCLK_HZ as u128) as u64
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------- the machine

/// The machine for `board` with `firmware` (an ELF) loaded and the external
/// flash holding `spi_image` (resources, the file system), its RTC starting
/// at `epoch` (UTC seconds).
pub fn machine(board: Board, firmware: &[u8], spi_image: &[u8], epoch: i64) -> Result<Cpu, String> {
    let cfg = board.config();
    // Flint's Cortex-M4; Emery's and Gabbro's Cortex-M33 (ARMv8-M)
    let mut bus = Bus::new(cfg.flash_size, cfg.ram_size, SYSCLK_HZ, REFCLK_HZ, board != Board::Flint);
    bus.xip = vec![0xff; EXTFLASH_SIZE];
    bus.xip_base = EXTFLASH_BASE;
    let n = spi_image.len().min(EXTFLASH_SIZE);
    bus.xip[..n].copy_from_slice(&spi_image[..n]);
    crate::elf::load(&mut bus, firmware)?;
    for i in 0..4 {
        bus.map(UART_BASE[i], 0x1000, Some(UART_IRQ[i]), Box::new(Uart::default()));
    }
    for i in 0..2 {
        bus.map(TIMER_BASE[i], 0x1000, Some(TIMER_IRQ[i]), Box::new(Timer::default()));
    }
    bus.map(RTC_BASE, 0x1000, Some(RTC_IRQ), Box::new(Rtc::new(epoch)));
    bus.map(GPIO_BASE, 0x1000, Some(GPIO_IRQ), Box::new(Gpio::default()));
    let features = cfg.touch as u32 | (cfg.audio as u32) << 1 | (cfg.round as u32) << 2;
    bus.map(
        SYSCTRL_BASE,
        0x1000,
        None,
        Box::new(SysCtrl { cfg: [cfg.board_id, features, cfg.width, cfg.height, cfg.bpp], reset_reason: 1 }),
    );
    bus.map(
        WDOG_BASE,
        0x1000,
        None,
        Box::new(Watchdog { load: 0xffff_ffff, control: 0, intstat: false, locked: false, value0: 0xffff_ffff, t0: 0, reset: false }),
    );
    let display = Rc::new(RefCell::new(DisplayState::new(cfg.width, cfg.height, cfg.bpp, cfg.round)));
    bus.map(DISPLAY_BASE, 0x1000, Some(DISPLAY_IRQ), Box::new(DisplayRegs(display.clone())));
    bus.map(DISPLAY_FB_BASE, 128 << 10, None, Box::new(DisplayFb(display)));
    bus.map(
        EXTFLASH_CTRL_BASE,
        0x1000,
        None,
        Box::new(ExtFlashCtrl { addr: 0, sync_len: 0, erases: Vec::new(), synced: Vec::new() }),
    );
    if cfg.touch {
        bus.map(TOUCH_BASE, 0x1000, Some(TOUCH_IRQ), Box::new(Touch::default()));
    }
    if cfg.audio {
        bus.map(
            AUDIO_BASE,
            0x1000,
            Some(AUDIO_IRQ),
            Box::new(Audio { ctrl: 0, rate: 0, intctrl: 0, intstat: 0, volume: 0, level: 0, drained_at: 0, samples: Vec::new() }),
        );
    }
    Ok(Cpu::new(bus))
}

/// Has the watchdog reset the machine (since the last call)?
pub fn watchdog_reset(cpu: &mut Cpu) -> bool { cpu.bus.device::<Watchdog>(WDOG_BASE).is_some_and(|w| std::mem::take(&mut w.reset)) }

/// The display state.
pub fn display(cpu: &mut Cpu) -> Rc<RefCell<DisplayState>> { cpu.bus.device::<DisplayRegs>(DISPLAY_BASE).unwrap().0.clone() }
