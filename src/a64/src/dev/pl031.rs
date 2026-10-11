//! ARM PL031 real-time clock, SPI 2. Counts seconds of guest virtual time
//! from a host-supplied epoch.

use crate::bus::{DevCtx, Device};

pub struct Pl031 {
    spi: u32,
    /// Unix time at virtual time zero.
    epoch: u64,
    /// Offset applied by guest writes to the load register.
    offset: i64,
    mr: u32,
    imsc: u32,
    ris: u32,
}

impl Pl031 {
    pub fn new(spi: u32, epoch: u64) -> Pl031 { Pl031 { spi, epoch, offset: 0, mr: 0, imsc: 0, ris: 0 } }

    fn now(&self, ctx: &DevCtx) -> u32 {
        (self.epoch as i64 + (ctx.now_ns / 1_000_000_000) as i64 + self.offset) as u32
    }
}

impl Device for Pl031 {
    fn name(&self) -> &str { "pl031" }

    fn read(&mut self, off: u64, _size: u32, ctx: &mut DevCtx) -> u64 {
        match off {
            0x00 => self.now(ctx) as u64,
            0x04 => self.mr as u64,
            0x08 => self.now(ctx) as u64,
            0x0c => 1,
            0x10 => self.imsc as u64,
            0x14 => self.ris as u64,
            0x18 => (self.ris & self.imsc) as u64,
            _ => super::primecell_id(off, [0x31, 0x10, 0x14, 0x00]).unwrap_or(0),
        }
    }

    fn write(&mut self, off: u64, _size: u32, v: u64, ctx: &mut DevCtx) {
        let v = v as u32;
        match off {
            0x04 => self.mr = v,
            0x08 => self.offset += v as i64 - self.now(ctx) as i64,
            0x10 => self.imsc = v & 1,
            0x1c => self.ris &= !v,
            _ => {}
        }
        ctx.gic.set_spi(self.spi, self.ris & self.imsc != 0);
    }
}
