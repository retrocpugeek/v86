//! NS16550A UART (QEMU's "serial"), the virt machine's console.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::bus::{DevCtx, Device};

const IER_RDI: u8 = 1;
const IER_THRI: u8 = 2;
const IER_RLSI: u8 = 4;
const IER_MSI: u8 = 8;
const LSR_DR: u8 = 1;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;
const MSR_DCD: u8 = 0x80;
const MSR_DSR: u8 = 0x20;
const MSR_CTS: u8 = 0x10;
const MCR_LOOP: u8 = 0x10;

pub struct Uart {
    irq: u32,
    /// Input from the host (the run loop moves it into the receive FIFO).
    pub rx: Arc<Mutex<VecDeque<u8>>>,
    fifo: VecDeque<u8>,
    out: Box<dyn FnMut(u8)>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    fifo_enabled: bool,
    /// A transmitter-empty interrupt is pending (cleared by reading IIR).
    thr_ipending: bool,
    /// RV64_UART_TRACE: log receive-side activity to stderr.
    trace: bool,
}

impl Uart {
    pub fn new(irq: u32, out: Box<dyn FnMut(u8)>) -> Uart {
        Uart {
            irq,
            rx: Arc::new(Mutex::new(VecDeque::new())),
            fifo: VecDeque::new(),
            out,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0x0c,
            dlm: 0,
            fifo_enabled: false,
            thr_ipending: false,
            trace: std::env::var_os("RV64_UART_TRACE").is_some(),
        }
    }

    fn lsr(&self) -> u8 { LSR_THRE | LSR_TEMT | if self.fifo.is_empty() { 0 } else { LSR_DR } }

    /// Interrupt identification (low 4 bits), 1 if none.
    fn iid(&self) -> u8 {
        if self.ier & IER_RDI != 0 && !self.fifo.is_empty() {
            4
        } else if self.ier & IER_THRI != 0 && self.thr_ipending {
            2
        } else {
            1
        }
    }

    fn update(&self, ctx: &mut DevCtx) { ctx.plic.set_irq(self.irq, self.iid() != 1); }
}

impl Device for Uart {
    fn name(&self) -> &str { "ns16550a" }

    fn read(&mut self, off: u64, _size: u32, ctx: &mut DevCtx) -> u64 {
        let dlab = self.lcr & 0x80 != 0;
        let v = match off {
            0 if dlab => self.dll,
            0 => {
                let b = self.fifo.pop_front().unwrap_or(0);
                if self.trace {
                    eprintln!("[uart] read {:?} ({} left)", b as char, self.fifo.len());
                }
                self.update(ctx);
                b
            }
            1 if dlab => self.dlm,
            1 => self.ier,
            2 => {
                let iid = self.iid();
                if iid == 2 {
                    self.thr_ipending = false;
                    self.update(ctx);
                }
                iid | if self.fifo_enabled { 0xc0 } else { 0 }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => self.lsr(),
            6 => {
                if self.mcr & MCR_LOOP != 0 {
                    // Loopback: DTR -> DSR, RTS -> CTS, OUT1 -> RI, OUT2 -> DCD
                    (self.mcr & 0x0c) << 4 | (self.mcr & 1) << 5 | (self.mcr & 2) << 3
                } else {
                    MSR_DCD | MSR_DSR | MSR_CTS
                }
            }
            7 => self.scr,
            _ => 0,
        };
        v as u64
    }

    fn write(&mut self, off: u64, _size: u32, v: u64, ctx: &mut DevCtx) {
        let v = v as u8;
        let dlab = self.lcr & 0x80 != 0;
        match off {
            0 if dlab => self.dll = v,
            0 => {
                if self.mcr & MCR_LOOP != 0 {
                    self.fifo.push_back(v);
                } else {
                    (self.out)(v);
                }
                // Sent at once: the transmitter is empty again
                self.thr_ipending = true;
            }
            1 if dlab => self.dlm = v,
            1 => {
                let was = self.ier;
                if self.trace && v & IER_RDI != was & IER_RDI {
                    eprintln!("[uart] IER rx {}", v & IER_RDI != 0);
                }
                self.ier = v & (IER_RDI | IER_THRI | IER_RLSI | IER_MSI);
                if self.ier & IER_THRI != 0 && was & IER_THRI == 0 {
                    self.thr_ipending = true;
                }
            }
            2 => {
                self.fifo_enabled = v & 1 != 0;
                if self.trace && v & 2 != 0 && !self.fifo.is_empty() {
                    eprintln!("[uart] FCR clears {} bytes", self.fifo.len());
                }
                if v & 2 != 0 {
                    self.fifo.clear();
                }
            }
            3 => self.lcr = v,
            4 => self.mcr = v & 0x1f,
            7 => self.scr = v,
            _ => {}
        }
        self.update(ctx);
    }

    fn poll(&mut self, ctx: &mut DevCtx) {
        let mut rx = self.rx.lock().unwrap();
        if rx.is_empty() {
            return;
        }
        while self.fifo.len() < 16 {
            match rx.pop_front() {
                Some(b) => {
                    if self.trace {
                        eprintln!("[uart] in {:?}", b as char);
                    }
                    self.fifo.push_back(b)
                }
                None => break,
            }
        }
        drop(rx);
        self.update(ctx);
    }
}
