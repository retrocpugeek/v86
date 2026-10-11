//! ARM PL011 UART (ttyAMA0), SPI 1.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::bus::{DevCtx, Device};

const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;
const INT_RT: u32 = 1 << 6;

const FR_RXFE: u64 = 1 << 4;
const FR_TXFE: u64 = 1 << 7;

pub struct Pl011 {
    spi: u32,
    /// Bytes waiting to be received by the guest (fed by the host).
    pub rx: Arc<Mutex<VecDeque<u8>>>,
    out: Box<dyn FnMut(u8)>,
    int_level: u32,
    imsc: u32,
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    cr: u32,
    ifls: u32,
    ilpr: u32,
    dmacr: u32,
}

impl Pl011 {
    pub fn new(spi: u32, out: Box<dyn FnMut(u8)>) -> Pl011 {
        Pl011 {
            spi,
            rx: Arc::new(Mutex::new(VecDeque::new())),
            out,
            int_level: 0,
            imsc: 0,
            ibrd: 0,
            fbrd: 0,
            lcr_h: 0,
            cr: 0x300,
            ifls: 0x12,
            ilpr: 0,
            dmacr: 0,
        }
    }

    fn update(&mut self, ctx: &mut DevCtx) {
        if !self.rx.lock().unwrap().is_empty() {
            self.int_level |= INT_RX | INT_RT;
        }
        ctx.gic.set_spi(self.spi, self.int_level & self.imsc != 0);
    }
}

impl Device for Pl011 {
    fn name(&self) -> &str { "pl011" }

    fn read(&mut self, off: u64, _size: u32, ctx: &mut DevCtx) -> u64 {
        let v = match off {
            0x000 => {
                let mut rx = self.rx.lock().unwrap();
                let c = rx.pop_front().unwrap_or(0);
                if rx.is_empty() {
                    self.int_level &= !(INT_RX | INT_RT);
                }
                drop(rx);
                self.update(ctx);
                c as u64
            }
            0x004 => 0,
            0x018 => {
                let empty = self.rx.lock().unwrap().is_empty();
                FR_TXFE | if empty { FR_RXFE } else { 0 }
            }
            0x020 => self.ilpr as u64,
            0x024 => self.ibrd as u64,
            0x028 => self.fbrd as u64,
            0x02c => self.lcr_h as u64,
            0x030 => self.cr as u64,
            0x034 => self.ifls as u64,
            0x038 => self.imsc as u64,
            0x03c => self.int_level as u64,
            0x040 => (self.int_level & self.imsc) as u64,
            0x048 => self.dmacr as u64,
            _ => super::primecell_id(off, [0x11, 0x10, 0x14, 0x00]).unwrap_or(0),
        };
        v
    }

    fn write(&mut self, off: u64, _size: u32, v: u64, ctx: &mut DevCtx) {
        let v = v as u32;
        match off {
            0x000 => {
                (self.out)(v as u8);
                self.int_level |= INT_TX;
            }
            0x004 => {}
            0x020 => self.ilpr = v,
            0x024 => self.ibrd = v,
            0x028 => self.fbrd = v,
            0x02c => self.lcr_h = v,
            0x030 => self.cr = v,
            0x034 => self.ifls = v,
            0x038 => self.imsc = v,
            0x044 => self.int_level &= !v,
            0x048 => self.dmacr = v,
            _ => {}
        }
        self.update(ctx);
    }

    fn poll(&mut self, ctx: &mut DevCtx) { self.update(ctx); }
}
