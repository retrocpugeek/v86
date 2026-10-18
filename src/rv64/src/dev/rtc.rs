//! Goldfish RTC: nanoseconds since the epoch.

use crate::bus::{DevCtx, Device};

pub struct Rtc {
    /// Epoch time (ns) at machine time 0.
    base_ns: u64,
    /// TIME_HIGH, latched by reading TIME_LOW.
    high: u32,
}

impl Rtc {
    pub fn new(epoch_secs: u64) -> Rtc { Rtc { base_ns: epoch_secs * 1_000_000_000, high: 0 } }
}

impl Device for Rtc {
    fn name(&self) -> &str { "goldfish-rtc" }

    fn read(&mut self, off: u64, _size: u32, ctx: &mut DevCtx) -> u64 {
        match off {
            0x00 => {
                let t = self.base_ns.wrapping_add(ctx.now_ns);
                self.high = (t >> 32) as u32;
                t as u32 as u64
            }
            0x04 => self.high as u64,
            _ => 0,
        }
    }

    fn write(&mut self, off: u64, _size: u32, v: u64, ctx: &mut DevCtx) {
        // Setting the time: TIME_LOW written after TIME_HIGH
        match off {
            0x04 => self.high = v as u32,
            0x00 => {
                let t = (self.high as u64) << 32 | (v as u32 as u64);
                self.base_ns = t.wrapping_sub(ctx.now_ns);
            }
            _ => {}
        }
    }
}
