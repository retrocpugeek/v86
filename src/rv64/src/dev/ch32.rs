//! WCH CH32V003: a QingKe V2A hart (RV32EC, machine mode only) with 16 KB
//! of flash at 0x0800_0000 (also at 0, where it starts), 2 KB of SRAM at
//! 0x2000_0000, and peripherals. Register layouts follow WCH's reference
//! manual (CH32V003RM).
//!
//! Modelled: SysTick and the PFIC (interrupt controller), which the hart
//! checks for interrupts (Cpu::pending_interrupt) and timer deadlines
//! (Cpu::timer_update); USART1, the console; RCC, which reports oscillators
//! and the PLL ready and sets the clock SysTick counts. Other peripheral
//! registers (GPIO, AFIO, flash interface, ...) hold what's written.
//!
//! Not modelled: the hardware prologue/epilogue (HPE: firmware clears
//! INTSYSCR, CSR 0x804, and saves registers itself), vectored mtvec, the
//! RV32E register limit, timers, ADC, I2C, SPI, DMA, flash programming.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::bus::Bus;
use crate::cpu::{Cpu, PRV_M};

pub const FLASH_BASE: u64 = 0x0800_0000;
pub const FLASH_SIZE: usize = 16 * 1024;
pub const SRAM_BASE: u64 = 0x2000_0000;
pub const SRAM_SIZE: usize = 2 * 1024;
const PFIC_BASE: u64 = 0xe000_e000;
const SYSTICK_BASE: u64 = 0xe000_f000;
/// APB and AHB peripherals
const PERIPH_BASE: u64 = 0x4000_0000;
const PERIPH_SIZE: u64 = 0x2_4000;
const USART1_BASE: u64 = 0x4001_3800;
const RCC_BASE: u64 = 0x4002_1000;

pub const IRQ_SYSTICK: u32 = 12;
pub const IRQ_USART1: u32 = 32;

const HSI_HZ: u64 = 24_000_000;

// SysTick CTLR bits
const STK_STE: u32 = 1 << 0;
const STK_STIE: u32 = 1 << 1;
const STK_STCLK: u32 = 1 << 2;
const STK_STRE: u32 = 1 << 3;
const STK_INIT: u32 = 1 << 5;
// USART STATR and CTLR1 bits
const USART_RXNE: u32 = 1 << 5;
const USART_TC: u32 = 1 << 6;
const USART_TXE: u32 = 1 << 7;
const USART_RXNEIE: u32 = 1 << 5;

pub struct Ch32 {
    /// PFIC: interrupts 0..63 enabled
    ienr: [u32; 2],
    stk_ctlr: u32,
    stk_sr: u32,
    stk_cmp: u64,
    /// The SysTick counter was stk_cnt0 at stk_t0 (ns)
    stk_cnt0: u64,
    stk_t0: u64,
    /// When the counter next reaches stk_cmp (ns), or u64::MAX
    stk_next: u64,
    usart_ctlr1: u32,
    usart_regs: [u32; 8],
    /// Console input, and output
    pub rx: Arc<Mutex<VecDeque<u8>>>,
    out: Box<dyn FnMut(u8)>,
    /// RCC registers by offset / 4
    rcc: [u32; 16],
    /// Other peripheral registers, by address
    regs: HashMap<u64, u32>,
}

impl Ch32 {
    pub fn new(out: Box<dyn FnMut(u8)>) -> Ch32 {
        let mut rcc = [0; 16];
        // Reset: HSI on, HCLK = SYSCLK / 3
        rcc[0] = 1;
        rcc[1] = 2 << 4;
        Ch32 {
            ienr: [0; 2],
            stk_ctlr: 0,
            stk_sr: 0,
            stk_cmp: 0,
            stk_cnt0: 0,
            stk_t0: 0,
            stk_next: u64::MAX,
            usart_ctlr1: 0,
            usart_regs: [0; 8],
            rx: Arc::new(Mutex::new(VecDeque::new())),
            out,
            rcc,
            regs: HashMap::new(),
        }
    }

    /// Does pa belong to a peripheral modelled here?
    pub fn claims(&self, pa: u64) -> bool {
        pa.wrapping_sub(PFIC_BASE) < 0x1000 || pa.wrapping_sub(SYSTICK_BASE) < 0x100 || pa.wrapping_sub(PERIPH_BASE) < PERIPH_SIZE
    }

    /// HCLK in Hz: SYSCLK (HSI, HSE (taken as 24 MHz) or the PLL, 2 x
    /// 24 MHz) divided by the AHB prescaler.
    pub fn hclk(&self) -> u64 {
        let cfgr0 = self.rcc[1];
        let sysclk = if cfgr0 & 3 == 2 { 2 * HSI_HZ } else { HSI_HZ };
        let div = match cfgr0 >> 4 & 15 {
            n @ 0..=7 => n as u64 + 1,
            n => 2 << (n - 8),
        };
        sysclk / div
    }

    fn stk_rate(&self) -> u64 { if self.stk_ctlr & STK_STCLK != 0 { self.hclk() } else { self.hclk() / 8 } }

    fn stk_count(&self, now: u64) -> u64 {
        if self.stk_ctlr & STK_STE == 0 {
            return self.stk_cnt0;
        }
        let ticks = (now.saturating_sub(self.stk_t0) as u128 * self.stk_rate() as u128 / 1_000_000_000) as u64;
        self.stk_cnt0.wrapping_add(ticks)
    }

    /// Recompute when the counter reaches the compare value.
    fn stk_schedule(&mut self) {
        self.stk_next = if self.stk_ctlr & STK_STE == 0 || self.stk_cnt0 > self.stk_cmp {
            u64::MAX
        } else {
            let ticks = (self.stk_cmp - self.stk_cnt0).max(1);
            let rate = self.stk_rate().max(1);
            self.stk_t0 + (ticks as u128 * 1_000_000_000).div_ceil(rate as u128) as u64
        };
    }

    /// Bring SysTick up to `now`: set CNTIF at compare matches (restarting
    /// the count from 0 with STRE).
    pub fn update(&mut self, now: u64) {
        while now >= self.stk_next {
            self.stk_sr |= 1;
            if self.stk_ctlr & STK_STRE != 0 {
                self.stk_t0 = self.stk_next;
                self.stk_cnt0 = 0;
                self.stk_schedule();
            } else {
                // Counting on past the compare value
                self.stk_next = u64::MAX;
            }
        }
    }

    /// When update() next has something to do (ns), or u64::MAX.
    pub fn next_event_ns(&self) -> u64 { self.stk_next }

    /// The pending interrupt the PFIC would take (if enabled), if any.
    pub fn pending_irq(&self) -> Option<u32> {
        let enabled = |n: u32| self.ienr[(n / 32) as usize] >> (n % 32) & 1 != 0;
        if self.stk_sr & 1 != 0 && self.stk_ctlr & STK_STIE != 0 && enabled(IRQ_SYSTICK) {
            return Some(IRQ_SYSTICK);
        }
        if self.usart_ctlr1 & USART_RXNEIE != 0 && enabled(IRQ_USART1) && !self.rx.lock().unwrap().is_empty() {
            return Some(IRQ_USART1);
        }
        None
    }

    /// A read of a register modelled here, if pa is one.
    pub fn read(&mut self, pa: u64, size: u32, now: u64) -> Option<u64> {
        if !self.claims(pa) {
            return None;
        }
        let shift = (pa & 3) * 8;
        let word = pa & !3;
        let v = self.read32(word, now);
        Some((v >> shift) as u64 & if size >= 4 { 0xffff_ffff } else { (1 << (8 * size)) - 1 })
    }

    fn read32(&mut self, a: u64, now: u64) -> u32 {
        if a.wrapping_sub(SYSTICK_BASE) < 0x100 {
            self.update(now);
            let cnt = self.stk_count(now);
            return match a - SYSTICK_BASE {
                0x00 => self.stk_ctlr,
                0x04 => self.stk_sr,
                0x08 => cnt as u32,
                0x0c => (cnt >> 32) as u32,
                0x10 => self.stk_cmp as u32,
                0x14 => (self.stk_cmp >> 32) as u32,
                _ => 0,
            };
        }
        if a.wrapping_sub(PFIC_BASE) < 0x1000 {
            return match a - PFIC_BASE {
                // ISR: enabled; IPR: pending
                0x000 | 0x004 => self.ienr[((a - PFIC_BASE) / 4) as usize],
                0x020 => (self.pending_irq() == Some(IRQ_SYSTICK)) as u32 * (1 << IRQ_SYSTICK),
                0x100 | 0x104 => self.ienr[((a - PFIC_BASE - 0x100) / 4) as usize],
                _ => 0,
            };
        }
        if a.wrapping_sub(USART1_BASE) < 0x400 {
            let off = (a - USART1_BASE) as usize;
            return match off {
                0x00 => {
                    let rxne = if self.rx.lock().unwrap().is_empty() { 0 } else { USART_RXNE };
                    USART_TXE | USART_TC | rxne
                }
                0x04 => self.rx.lock().unwrap().pop_front().unwrap_or(0) as u32,
                0x0c => self.usart_ctlr1,
                _ => self.usart_regs.get(off / 4).copied().unwrap_or(0),
            };
        }
        if a.wrapping_sub(RCC_BASE) < 0x40 {
            let r = self.rcc[((a - RCC_BASE) / 4) as usize];
            return match a - RCC_BASE {
                // CTLR: HSIRDY, HSERDY and PLLRDY follow HSION, HSEON and PLLON
                0x00 => r | (r & 1) << 1 | (r >> 16 & 1) << 17 | (r >> 24 & 1) << 25,
                // CFGR0: SWS follows SW
                0x04 => (r & !0xc) | (r & 3) << 2,
                _ => r,
            };
        }
        self.regs.get(&a).copied().unwrap_or(0)
    }

    /// A write to a register modelled here; false if pa isn't one.
    pub fn write(&mut self, pa: u64, size: u32, value: u64, now: u64) -> bool {
        if !self.claims(pa) {
            return false;
        }
        // Narrow writes merge into the word
        let a = pa & !3;
        let v = if size >= 4 {
            value as u32
        } else {
            let shift = (pa & 3) * 8;
            let m = ((1u64 << (8 * size)) - 1) << shift;
            let old = if a.wrapping_sub(USART1_BASE) == 4 { 0 } else { self.read32(a, now) as u64 };
            ((old & !m) | (value << shift & m)) as u32
        };
        self.write32(a, v, now);
        true
    }

    fn write32(&mut self, a: u64, v: u32, now: u64) {
        if a.wrapping_sub(SYSTICK_BASE) < 0x100 {
            self.update(now);
            // Restart the count from its current value under the new settings
            self.stk_cnt0 = self.stk_count(now);
            self.stk_t0 = now;
            match a - SYSTICK_BASE {
                0x00 => {
                    self.stk_ctlr = v & !STK_INIT;
                    if v & STK_INIT != 0 {
                        self.stk_cnt0 = 0;
                    }
                }
                // Writing 0 clears CNTIF
                0x04 => self.stk_sr &= v,
                0x08 => self.stk_cnt0 = (self.stk_cnt0 & !0xffff_ffff) | v as u64,
                0x0c => self.stk_cnt0 = (self.stk_cnt0 & 0xffff_ffff) | (v as u64) << 32,
                0x10 => self.stk_cmp = (self.stk_cmp & !0xffff_ffff) | v as u64,
                0x14 => self.stk_cmp = (self.stk_cmp & 0xffff_ffff) | (v as u64) << 32,
                _ => {}
            }
            self.stk_schedule();
            self.update(now);
            return;
        }
        if a.wrapping_sub(PFIC_BASE) < 0x1000 {
            match a - PFIC_BASE {
                // IENR: set enables; IRER: clear them
                0x100 | 0x104 => self.ienr[((a - PFIC_BASE - 0x100) / 4) as usize] |= v,
                0x180 | 0x184 => self.ienr[((a - PFIC_BASE - 0x180) / 4) as usize] &= !v,
                _ => {}
            }
            return;
        }
        if a.wrapping_sub(USART1_BASE) < 0x400 {
            let off = (a - USART1_BASE) as usize;
            match off {
                0x04 => (self.out)(v as u8),
                0x0c => self.usart_ctlr1 = v,
                _ => {
                    if let Some(r) = self.usart_regs.get_mut(off / 4) {
                        *r = v;
                    }
                }
            }
            return;
        }
        if a.wrapping_sub(RCC_BASE) < 0x40 {
            self.update(now);
            self.stk_cnt0 = self.stk_count(now);
            self.stk_t0 = now;
            self.rcc[((a - RCC_BASE) / 4) as usize] = v;
            // SysTick's clock may have changed
            self.stk_schedule();
            return;
        }
        self.regs.insert(a, v);
    }
}

/// A CH32V003 with `firmware` in its flash, starting at its reset vector.
/// The USART's output goes to `out`; input is Ch32::rx.
pub fn machine(firmware: &[u8], out: Box<dyn FnMut(u8)>) -> Result<Cpu, String> {
    if firmware.len() > FLASH_SIZE {
        return Err(format!("firmware is {} bytes; the flash holds {FLASH_SIZE}", firmware.len()));
    }
    let mut bus = Bus::new(SRAM_SIZE);
    bus.ram_base = SRAM_BASE;
    let mut flash = firmware.to_vec();
    flash.resize(FLASH_SIZE, 0xff);
    bus.rom = flash;
    bus.rom_base = FLASH_BASE;
    bus.rom_alias0 = true;
    bus.ch32 = Some(Box::new(Ch32::new(out)));
    let mut cpu = Cpu::new(bus);
    cpu.xlen32 = true;
    cpu.prv = PRV_M;
    cpu.pc = 0;
    // Virtual time for instructions: about one per cycle at 24 MHz (SysTick
    // follows the RCC's clock settings regardless)
    cpu.ns_per_insn = 42;
    Ok(cpu)
}
