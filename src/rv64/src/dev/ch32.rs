//! WCH CH32V microcontrollers (machine mode only), flash at 0x0800_0000
//! (also at 0, where they start) and SRAM at 0x2000_0000:
//! - CH32V003: QingKe V2A (RV32EC), 16 KB of flash, 2 KB of SRAM
//! - CH32V307: QingKe V4F (RV32IMAFC), 256 KB of flash, 128 KB of SRAM
//!   (the largest of its flash/SRAM splits)
//!
//! Register layouts follow WCH's reference manuals (CH32V003RM, CH32V30xRM).
//!
//! Modelled: SysTick and the PFIC (interrupt controller: enables, pending
//! bits, priorities; SysTick's software interrupt), which the hart checks
//! for interrupts (Cpu::pending_interrupt) and timer deadlines
//! (Cpu::timer_update); the QingKe hardware prologue/epilogue (HPE, in cpu.rs)
//! and vectored mtvec; USART1, the console, with its interrupts; RCC, which
//! reports oscillators and the PLL ready and sets the clock SysTick counts;
//! the RTC (seconds counter, from the host's time) and backup registers.
//! Other peripheral registers (GPIO, AFIO, flash interface, ...) hold what's
//! written.
//!
//! Not modelled: interrupt nesting and preemption by priority, the RV32E
//! register limit, timers, ADC, I2C, SPI, DMA, USB, Ethernet, flash
//! programming.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::bus::Bus;
use crate::cpu::{Cpu, PRV_M};

pub const FLASH_BASE: u64 = 0x0800_0000;
pub const SRAM_BASE: u64 = 0x2000_0000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chip {
    V003,
    V307,
}

impl Chip {
    pub fn flash_size(self) -> usize {
        match self {
            Chip::V003 => 16 * 1024,
            Chip::V307 => 256 * 1024,
        }
    }

    pub fn sram_size(self) -> usize {
        match self {
            Chip::V003 => 2 * 1024,
            Chip::V307 => 128 * 1024,
        }
    }

    fn usart1_irq(self) -> u32 {
        match self {
            Chip::V003 => 32,
            Chip::V307 => 53,
        }
    }
}
const PFIC_BASE: u64 = 0xe000_e000;
const SYSTICK_BASE: u64 = 0xe000_f000;
/// APB and AHB peripherals
const PERIPH_BASE: u64 = 0x4000_0000;
const PERIPH_SIZE: u64 = 0x2_4000;
const USART1_BASE: u64 = 0x4001_3800;
/// RTC (the V307's; STM32F1-like) and the backup registers
const RTC_BASE: u64 = 0x4000_2800;
const BKP_DR1: u64 = 0x4000_6c04;
/// GPIO ports A to E (the V003 has A, C and D), 0x400 apart
const GPIO_BASE: u64 = 0x4001_0800;
const GPIO_PORTS: u64 = 5;
/// Vendor information: chip configuration, the electronic signature (flash
/// size, unique ID) and the option bytes
const INFO_BASE: u64 = 0x1fff_f000;
const RCC_BASE: u64 = 0x4002_1000;

pub const IRQ_SYSTICK: u32 = 12;
/// SysTick's software interrupt (CTLR.SWIE)
pub const IRQ_SOFTWARE: u32 = 14;
const NUM_IRQS: usize = 128;

// SysTick CTLR bits
const STK_STE: u32 = 1 << 0;
const STK_STIE: u32 = 1 << 1;
const STK_STCLK: u32 = 1 << 2;
const STK_STRE: u32 = 1 << 3;
const STK_INIT: u32 = 1 << 5;
const STK_SWIE: u32 = 1 << 31;
// USART STATR and CTLR1 bits (the CTLR1 interrupt enables are the same bits)
const USART_RXNE: u32 = 1 << 5;
const USART_TC: u32 = 1 << 6;
const USART_TXE: u32 = 1 << 7;

pub struct Ch32 {
    pub chip: Chip,
    /// PFIC: interrupts enabled and pending (set by software), and their
    /// priorities (lower is more urgent)
    ienr: [u32; NUM_IRQS / 32],
    ipsr: [u32; NUM_IRQS / 32],
    iprior: [u8; NUM_IRQS],
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
    /// pending_irq(), until something it depends on changes (the hart
    /// asks every instruction); see invalidate()
    pending_cache: Cell<Option<Option<u32>>>,
    /// RTC counter (seconds): rtc_cnt0 at rtc_t0 (ns)
    rtc_cnt0: u64,
    rtc_t0: u64,
}

impl Ch32 {
    /// A chip whose RTC counter starts at `epoch` (seconds).
    pub fn new(chip: Chip, out: Box<dyn FnMut(u8)>, epoch: u64) -> Ch32 {
        let mut rcc = [0; 16];
        // Reset: HSI on (and for the V003, HCLK = SYSCLK / 3)
        rcc[0] = 1;
        if chip == Chip::V003 {
            rcc[1] = 2 << 4;
        }
        Ch32 {
            chip,
            ienr: [0; NUM_IRQS / 32],
            ipsr: [0; NUM_IRQS / 32],
            iprior: [0; NUM_IRQS],
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
            pending_cache: Cell::new(None),
            rtc_cnt0: epoch,
            rtc_t0: 0,
            // The backup domain as if the RTC had been set up (and kept by a
            // battery): drv_rtc.c's marker, so it keeps the counter
            regs: HashMap::from([(BKP_DR1, 0xa1a1)]),
        }
    }

    /// Does pa belong to a peripheral modelled here?
    pub fn claims(&self, pa: u64) -> bool {
        pa.wrapping_sub(PFIC_BASE) < 0x1000
            || pa.wrapping_sub(SYSTICK_BASE) < 0x100
            || pa.wrapping_sub(PERIPH_BASE) < PERIPH_SIZE
            || pa.wrapping_sub(INFO_BASE) < 0x1000
    }

    /// HCLK in Hz: SYSCLK (HSI, HSE or the PLL) divided by the AHB
    /// prescaler.
    pub fn hclk(&self) -> u64 {
        let cfgr0 = self.rcc[1];
        let hpre = cfgr0 >> 4 & 15;
        match self.chip {
            Chip::V003 => {
                // HSI and HSE (taken as) 24 MHz, the PLL doubles HSI
                let sysclk = if cfgr0 & 3 == 2 { 48_000_000 } else { 24_000_000 };
                let div = match hpre {
                    n @ 0..=7 => n as u64 + 1,
                    n => 2 << (n - 8),
                };
                sysclk / div
            }
            Chip::V307 => {
                // HSI and the board's HSE are 8 MHz; as SystemCoreClockUpdate
                // (CH32V30x_D8C) computes it
                const OSC: u64 = 8_000_000;
                let sysclk = match cfgr0 & 3 {
                    2 => {
                        // PLLMUL: x(n + 2), except 0: x18, 13: x6.5, 14: x15, 15: x16
                        let (mul2, src) = (
                            match cfgr0 >> 18 & 15 {
                                0 => 36,
                                13 => 13,
                                14 => 30,
                                15 => 32,
                                n => 2 * (n as u64 + 2),
                            },
                            if cfgr0 >> 16 & 1 == 0 || cfgr0 >> 17 & 1 != 0 { OSC / 2 } else { OSC },
                        );
                        src * mul2 / 2
                    }
                    _ => OSC,
                };
                const SHIFT: [u32; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 6, 7, 8, 9];
                sysclk >> SHIFT[hpre as usize]
            }
        }
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
            self.invalidate();
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

    /// USART1's status register.
    fn usart_statr(&self) -> u32 {
        let rxne = if self.rx.lock().unwrap().is_empty() { 0 } else { USART_RXNE };
        USART_TXE | USART_TC | rxne
    }

    /// Is interrupt n pending (whether or not it's enabled)?
    fn is_pending(&self, n: u32) -> bool {
        if self.ipsr[(n / 32) as usize] >> (n % 32) & 1 != 0 {
            return true;
        }
        match n {
            IRQ_SYSTICK => self.stk_sr & 1 != 0 && self.stk_ctlr & STK_STIE != 0,
            IRQ_SOFTWARE => self.stk_ctlr & STK_SWIE != 0,
            _ if n == self.chip.usart1_irq() => self.usart_statr() & self.usart_ctlr1 & (USART_RXNE | USART_TC | USART_TXE) != 0,
            _ => false,
        }
    }

    /// Something pending_irq() depends on may have changed outside this
    /// struct (console input).
    #[inline]
    pub fn invalidate(&self) { self.pending_cache.set(None) }

    /// The pending, enabled interrupt the PFIC would take, if any: the most
    /// urgent by priority, then the lowest numbered.
    #[inline]
    pub fn pending_irq(&self) -> Option<u32> {
        if let Some(p) = self.pending_cache.get() {
            return p;
        }
        let p = self.compute_pending_irq();
        self.pending_cache.set(Some(p));
        p
    }

    fn compute_pending_irq(&self) -> Option<u32> {
        let enabled = |n: u32| self.ienr[(n / 32) as usize] >> (n % 32) & 1 != 0;
        let candidates = [IRQ_SYSTICK, IRQ_SOFTWARE, self.chip.usart1_irq()];
        let any_software = self.ipsr.iter().any(|&w| w != 0);
        let software = (0..if any_software { NUM_IRQS as u32 } else { 0 }).filter(|&n| self.ipsr[(n / 32) as usize] >> (n % 32) & 1 != 0);
        candidates
            .into_iter()
            .chain(software)
            .filter(|&n| enabled(n) && self.is_pending(n))
            .min_by_key(|&n| (self.iprior[n as usize], n))
    }

    /// A read of a register modelled here, if pa is one.
    pub fn read(&mut self, pa: u64, size: u32, now: u64) -> Option<u64> {
        if !self.claims(pa) {
            return None;
        }
        // (reading USART1's data register takes a byte of input)
        self.invalidate();
        let shift = (pa & 3) * 8;
        let word = pa & !3;
        let v = self.read32(word, now);
        Some((v >> shift) as u64 & if size >= 4 { 0xffff_ffff } else { (1 << (8 * size)) - 1 })
    }

    /// The vendor information area (read-only).
    fn info(&self, a: u64) -> u32 {
        match a {
            // Chip configuration: bit 14 marks the CH32V307 (RCC_GetClocksFreq)
            0x1fff_f70c => if self.chip == Chip::V307 { 1 << 14 } else { 0 },
            // ESIG: flash size in KB, and a 96-bit unique ID
            0x1fff_f7e0 => (self.chip.flash_size() / 1024) as u32 | 0xffff_0000,
            0x1fff_f7e8 => 0x3638_3676, // "v868"
            0x1fff_f7ec => 0x0001_0203,
            0x1fff_f7f0 => 0x0405_0607,
            // Option bytes: RDPR/nRDPR (read protection off), the rest erased
            0x1fff_f800 => 0xffff_5aa5,
            _ => 0xffff_ffff,
        }
    }

    fn read32(&mut self, a: u64, now: u64) -> u32 {
        if a.wrapping_sub(INFO_BASE) < 0x1000 {
            return self.info(a);
        }
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
            let off = a - PFIC_BASE;
            let word = |base: u64| ((off - base) / 4) as usize;
            return match off {
                // ISR: enabled; IPR: pending
                0x000..=0x00c => self.ienr[word(0)],
                0x020..=0x02c => {
                    let w = word(0x20) as u32;
                    (0..32).filter(|&b| self.is_pending(w * 32 + b)).fold(0, |v, b| v | 1 << b)
                }
                0x100..=0x10c => self.ienr[word(0x100)],
                0x400..=0x47c => {
                    let i = (off - 0x400) as usize;
                    u32::from_le_bytes(self.iprior[i..i + 4].try_into().unwrap())
                }
                _ => 0,
            };
        }
        if a.wrapping_sub(USART1_BASE) < 0x400 {
            let off = (a - USART1_BASE) as usize;
            return match off {
                0x00 => self.usart_statr(),
                0x04 => self.rx.lock().unwrap().pop_front().unwrap_or(0) as u32,
                0x0c => self.usart_ctlr1,
                _ => self.usart_regs.get(off / 4).copied().unwrap_or(0),
            };
        }
        if a.wrapping_sub(RTC_BASE) < 0x400 {
            let cnt = self.rtc_cnt0 + now.saturating_sub(self.rtc_t0) / 1_000_000_000;
            return match a - RTC_BASE {
                // CTLRL: the last write is done (RTOFF), registers synchronized (RSF)
                0x04 => self.regs.get(&a).copied().unwrap_or(0) | 1 << 5 | 1 << 3,
                0x18 => (cnt >> 16) as u32 & 0xffff,
                0x1c => cnt as u32 & 0xffff,
                _ => self.regs.get(&a).copied().unwrap_or(0),
            };
        }
        if a.wrapping_sub(RCC_BASE) < 0x40 {
            let r = self.rcc[((a - RCC_BASE) / 4) as usize];
            return match a - RCC_BASE {
                // CTLR: HSIRDY, HSERDY and PLLRDY follow HSION, HSEON and PLLON
                0x00 => r | (r & 1) << 1 | (r >> 16 & 1) << 17 | (r >> 24 & 1) << 25,
                // BDCTLR: LSERDY follows LSEON; RSTSCKR: LSIRDY follows LSION
                0x20 | 0x24 => r | (r & 1) << 1,
                // CFGR0: SWS follows SW
                0x04 => (r & !0xc) | (r & 3) << 2,
                _ => r,
            };
        }
        if a.wrapping_sub(GPIO_BASE) < GPIO_PORTS * 0x400 && a & 0x3ff == 0x08 {
            // INDR: the pins read back what's driven (nothing is connected)
            return self.regs.get(&(a + 4)).copied().unwrap_or(0);
        }
        self.regs.get(&a).copied().unwrap_or(0)
    }

    /// A GPIO port's output data register (OUTDR), for showing LEDs.
    pub fn gpio_out(&self, port: u32) -> u32 {
        self.regs.get(&(GPIO_BASE + 0x400 * port as u64 + 0x0c)).copied().unwrap_or(0)
    }

    /// A write to a register modelled here; false if pa isn't one.
    pub fn write(&mut self, pa: u64, size: u32, value: u64, now: u64) -> bool {
        if !self.claims(pa) {
            return false;
        }
        self.invalidate();
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
        if a.wrapping_sub(INFO_BASE) < 0x1000 {
            return;
        }
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
            let off = a - PFIC_BASE;
            let word = |base: u64| ((off - base) / 4) as usize;
            match off {
                // IENR/IRER: set/clear enables; IPSR/IPRR: set/clear pending
                0x100..=0x10c => self.ienr[word(0x100)] |= v,
                0x180..=0x18c => self.ienr[word(0x180)] &= !v,
                0x200..=0x20c => self.ipsr[word(0x200)] |= v,
                0x280..=0x28c => self.ipsr[word(0x280)] &= !v,
                0x400..=0x47c => {
                    let i = (off - 0x400) as usize;
                    self.iprior[i..i + 4].copy_from_slice(&v.to_le_bytes());
                }
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
        if a.wrapping_sub(RTC_BASE) < 0x400 {
            // Counter writes (in configuration mode) set a half of it
            let cnt = self.rtc_cnt0 + now.saturating_sub(self.rtc_t0) / 1_000_000_000;
            let v16 = (v & 0xffff) as u64;
            match a - RTC_BASE {
                0x18 => (self.rtc_cnt0, self.rtc_t0) = ((cnt & 0xffff) | v16 << 16, now),
                0x1c => (self.rtc_cnt0, self.rtc_t0) = ((cnt & 0xffff_0000) | v16, now),
                _ => {
                    self.regs.insert(a, v);
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
        if a.wrapping_sub(GPIO_BASE) < GPIO_PORTS * 0x400 && matches!(a & 0x3ff, 0x10 | 0x14) {
            // BSHR sets (low half) and resets (high half) OUTDR bits; BCR resets
            let outdr = a & !0x3ff | 0x0c;
            let mut o = self.regs.get(&outdr).copied().unwrap_or(0);
            if a & 0x3ff == 0x10 {
                o = (o | (v & 0xffff)) & !(v >> 16);
            } else {
                o &= !(v & 0xffff);
            }
            self.regs.insert(outdr, o);
            return;
        }
        self.regs.insert(a, v);
    }
}

/// A CH32V `chip` with `firmware` in its flash, starting at its reset
/// vector, and its RTC at `epoch` (seconds). The USART's output goes to
/// `out`; input is Ch32::rx.
pub fn machine(chip: Chip, firmware: &[u8], out: Box<dyn FnMut(u8)>, epoch: u64) -> Result<Cpu, String> {
    let flash_size = chip.flash_size();
    if firmware.len() > flash_size {
        return Err(format!("firmware is {} bytes; the flash holds {flash_size}", firmware.len()));
    }
    let mut bus = Bus::new(chip.sram_size());
    bus.ram_base = SRAM_BASE;
    let mut flash = firmware.to_vec();
    flash.resize(flash_size, 0xff);
    bus.rom = flash;
    bus.rom_base = FLASH_BASE;
    bus.rom_alias0 = true;
    bus.ch32 = Some(Box::new(Ch32::new(chip, out, epoch)));
    let mut cpu = Cpu::new(bus);
    cpu.xlen32 = true;
    cpu.prv = PRV_M;
    cpu.pc = 0;
    // Virtual time for instructions: about one per cycle at 24 MHz (V003)
    // or 144 MHz (V307); SysTick follows the RCC's clock settings regardless
    cpu.ns_per_insn = if chip == Chip::V003 { 42 } else { 7 };
    Ok(cpu)
}
