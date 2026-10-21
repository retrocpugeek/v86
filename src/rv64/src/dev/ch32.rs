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
//! the RTC (seconds counter, from the host's time) and backup registers;
//! I2C1 as a bus master transmitter, at the bus's speed, with an SSD1306 OLED
//! display on it (dev/ssd1306.rs), fed by the CPU or by DMA1 channel 6
//! (with its transfer complete interrupt on the V003); ADC1's software-started conversions, of
//! values the host sets (e.g. for buttons on a resistor ladder); GPIO inputs
//! the host drives, and edges on output pins it watches (e.g. a buzzer).
//! Other peripheral registers (AFIO, flash interface, ...) hold what's
//! written.
//!
//! Not modelled: interrupt nesting and preemption by priority, the RV32E
//! register limit, timers, I2C reception and interrupts, SPI, other DMA
//! channels, USB, Ethernet, flash programming.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::bus::Bus;
use crate::cpu::{Cpu, PRV_M};
use crate::dev::ssd1306::{self, Ssd1306};

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

    /// DMA1 channel 6's interrupt (modelled for the V003 only)
    fn dma1_ch6_irq(self) -> u32 {
        match self {
            Chip::V003 => 27,
            Chip::V307 => u32::MAX,
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
const I2C1_BASE: u64 = 0x4000_5400;
const ADC1_BASE: u64 = 0x4001_2400;
const DMA1_BASE: u64 = 0x4002_0000;
/// DMA1 channel 6's registers (CFGR, CNTR, PADDR, MADDR), I2C1's transmit
/// channel
const DMA_CH6: u64 = 0x6c;
const DMA_EN: u32 = 1 << 0;
const DMA_TCIE: u32 = 1 << 1;
const DMA_DIR: u32 = 1 << 4;
/// INTFR's channel 6 flags: global, transfer complete
const DMA_GIF6: u32 = 1 << 20;
const DMA_TCIF6: u32 = 1 << 21;
const I2C_DMAEN: u32 = 1 << 11;

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
// I2C CTLR1, STAR1 and STAR2 bits
const I2C_PE: u32 = 1 << 0;
const I2C_START: u32 = 1 << 8;
const I2C_STOP: u32 = 1 << 9;
const I2C_SWRST: u32 = 1 << 15;
const I2C_SB: u32 = 1 << 0;
const I2C_ADDR: u32 = 1 << 1;
const I2C_BTF: u32 = 1 << 2;
const I2C_TXE: u32 = 1 << 7;
const I2C_AF: u32 = 1 << 10;
const I2C_MSL: u32 = 1 << 0;
const I2C_BUSY: u32 = 1 << 1;
const I2C_TRA: u32 = 1 << 2;
// ADC CTLR2 and STATR bits
const ADC_ADON: u32 = 1 << 0;
const ADC_CAL: u32 = 1 << 2;
const ADC_RSTCAL: u32 = 1 << 3;
const ADC_SWSTART: u32 = 1 << 22;
const ADC_EOC: u32 = 1 << 1;
/// Edges kept for the host (Ch32::take_edges) before the oldest are dropped
const MAX_EDGES: usize = 1 << 16;

/// I2C1 as a bus master transmitter. Bytes take the bus's time (9 SCL
/// periods); the state advances when its registers are accessed.
#[derive(Default)]
struct I2c {
    ctlr1: u32,
    ctlr2: u32,
    ckcfgr: u32,
    oaddr: [u32; 2],
    sb: bool,
    addr: bool,
    af: bool,
    /// Master mode (MSL, BUSY) and transmitting (TRA)
    master: bool,
    tra: bool,
    /// The address phase is over: data bytes go to the device
    addressed: bool,
    /// A START was requested; SB at this time (ns)
    start_at: Option<u64>,
    stop_pending: bool,
    /// The byte in the shift register, done at shift_until (ns), and
    /// whether it's the address
    shifting: Option<(u8, bool)>,
    shift_until: u64,
    /// DATAR, written while the shift register was busy
    dr: Option<u8>,
    /// STAR1 was read (SB is cleared by a STAR1 read then a DATAR write,
    /// ADDR by a STAR1 read then a STAR2 read)
    star1_read: bool,
    /// The device addressed acknowledged (the display)
    to_display: bool,
    /// Bytes DMA1 channel 6 has yet to write to DATAR
    dma_queue: VecDeque<u8>,
}

/// ADC1, converting on software start (SWSTART).
#[derive(Default)]
struct Adc {
    ctlr1: u32,
    ctlr2: u32,
    rsqr3: u32,
    eoc: bool,
    rdatar: u32,
    regs: [u32; 20],
}

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
    i2c: I2c,
    /// The SSD1306 display on I2C1
    pub display: Ssd1306,
    adc: Adc,
    /// ADC1's inputs (channels 0 to 9; 8 is the internal 1.2 V reference)
    pub adc_in: [u16; 10],
    /// GPIO pins the host drives, by port: mask and levels
    pub gpio_in: [(u32, u32); GPIO_PORTS as usize],
    /// Output pins whose changes are recorded, by port; the changes
    /// (time in ns, port, OUTDR), oldest first
    pub gpio_watch: [u32; GPIO_PORTS as usize],
    edges: VecDeque<(u64, u8, u16)>,
    /// HCLK may have changed (RCC written): the hart retimes its instructions
    pub clock_changed: bool,
    /// DMA1's registers by offset / 4 (INTFR is dma_intfr)
    dma: [u32; 40],
    dma_intfr: u32,
    /// A transfer DMA1 channel 6 is to start: its memory address and length;
    /// the bus reads the bytes and hands them over (dma_supply)
    dma_request: Option<(u32, u32)>,
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
            i2c: I2c::default(),
            display: Ssd1306::default(),
            adc: Adc::default(),
            // VREFINT: 1.2 V of a 3.3 V supply, 10 bits
            adc_in: [0, 0, 0, 0, 0, 0, 0, 0, 372, 0],
            gpio_in: [(0, 0); GPIO_PORTS as usize],
            gpio_watch: [0; GPIO_PORTS as usize],
            edges: VecDeque::new(),
            clock_changed: false,
            dma: [0; 40],
            dma_intfr: 0,
            dma_request: None,
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
        self.i2c_update(now);
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

    /// When update() next has something to do (ns), or u64::MAX: SysTick,
    /// and while DMA feeds I2C1, its bytes.
    pub fn next_event_ns(&self) -> u64 {
        let i2c = &self.i2c;
        let dma = if i2c.dma_queue.is_empty() && i2c.dr.is_none() {
            u64::MAX
        } else if let Some(t) = i2c.start_at {
            t
        } else if i2c.shifting.is_some() {
            i2c.shift_until
        } else {
            u64::MAX
        };
        self.stk_next.min(dma)
    }

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
            _ if n == self.chip.dma1_ch6_irq() => {
                self.dma_intfr & DMA_TCIF6 != 0 && self.dma[(DMA_CH6 / 4) as usize] & DMA_TCIE != 0
            }
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
        let enabled = |n: u32| (n as usize) < NUM_IRQS && self.ienr[(n / 32) as usize] >> (n % 32) & 1 != 0;
        let candidates = [IRQ_SYSTICK, IRQ_SOFTWARE, self.chip.usart1_irq(), self.chip.dma1_ch6_irq()];
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
            // INDR: what the host drives, otherwise what's driven or pulled
            // (OUTDR)
            let (mask, levels) = self.gpio_in[((a - GPIO_BASE) / 0x400) as usize];
            let out = self.regs.get(&(a + 4)).copied().unwrap_or(0);
            return out & !mask | levels & mask;
        }
        if a.wrapping_sub(I2C1_BASE) < 0x400 {
            return self.i2c_read(a - I2C1_BASE, now);
        }
        if a.wrapping_sub(DMA1_BASE) < 0xa0 {
            self.i2c_update(now);
            return match a - DMA1_BASE {
                0x00 => self.dma_intfr,
                0x04 => 0,
                // Channel 6's count: what's left of the transfer
                off if off == DMA_CH6 + 4 && !self.i2c.dma_queue.is_empty() => self.i2c.dma_queue.len() as u32,
                off => self.dma[(off / 4) as usize],
            };
        }
        if a.wrapping_sub(ADC1_BASE) < 0x400 {
            let adc = &mut self.adc;
            return match a - ADC1_BASE {
                0x00 => (adc.eoc as u32) << 1,
                0x04 => adc.ctlr1,
                0x08 => adc.ctlr2,
                0x34 => adc.rsqr3,
                0x4c => {
                    adc.eoc = false;
                    adc.rdatar
                }
                off => adc.regs.get(off as usize / 4).copied().unwrap_or(0),
            };
        }
        self.regs.get(&a).copied().unwrap_or(0)
    }

    // ---- I2C1 ----

    /// One SCL period (ns): CKCFGR's CCR in HCLK cycles (the V003's APB1
    /// clock is HCLK), times 3 in fast mode (25 with DUTY), 2 in standard.
    fn i2c_bit_ns(&self) -> u64 {
        let c = self.i2c.ckcfgr;
        let ccr = (c & 0xfff).max(1) as u64;
        let cycles = if c & 1 << 15 == 0 { 2 * ccr } else if c & 1 << 14 != 0 { 25 * ccr } else { 3 * ccr };
        (cycles * 1_000_000_000 / self.hclk().max(1)).max(1)
    }

    /// Bring I2C1 up to `now`: START conditions, bytes shifted out (to the
    /// display), STOP conditions.
    fn i2c_update(&mut self, now: u64) {
        loop {
            if let Some(t) = self.i2c.start_at {
                if now < t {
                    return;
                }
                let i2c = &mut self.i2c;
                i2c.start_at = None;
                i2c.ctlr1 &= !I2C_START;
                i2c.sb = true;
                i2c.master = true;
                i2c.addressed = false;
                i2c.to_display = false;
                i2c.af = false;
                continue;
            }
            if let Some((b, is_addr)) = self.i2c.shifting {
                if now < self.i2c.shift_until {
                    return;
                }
                let done = self.i2c.shift_until;
                self.i2c.shifting = None;
                if is_addr {
                    // The display acknowledges its address (writes only)
                    if b >> 1 == ssd1306::ADDR && b & 1 == 0 {
                        self.i2c.addr = true;
                        self.i2c.tra = true;
                        self.i2c.addressed = true;
                        self.i2c.to_display = true;
                        self.display.start();
                    } else {
                        self.i2c.af = true;
                    }
                } else if self.i2c.to_display {
                    self.display.byte(b);
                }
                self.dma_feed(done);
                if let Some(next) = self.i2c.dr.take() {
                    self.i2c.shifting = Some((next, false));
                    self.i2c.shift_until = done + 9 * self.i2c_bit_ns();
                    self.dma_feed(done);
                }
                continue;
            }
            if self.i2c.stop_pending && self.i2c.dr.is_none() {
                let i2c = &mut self.i2c;
                i2c.stop_pending = false;
                i2c.ctlr1 &= !I2C_STOP;
                i2c.master = false;
                i2c.tra = false;
                i2c.addressed = false;
                i2c.to_display = false;
            }
            return;
        }
    }

    fn i2c_read(&mut self, off: u64, now: u64) -> u32 {
        self.i2c_update(now);
        let i2c = &mut self.i2c;
        let idle = i2c.shifting.is_none() && i2c.dr.is_none();
        match off {
            0x00 => i2c.ctlr1,
            0x04 => i2c.ctlr2,
            0x08 => i2c.oaddr[0],
            0x0c => i2c.oaddr[1],
            0x14 => {
                i2c.star1_read = true;
                let txe = i2c.addressed && i2c.dr.is_none();
                let btf = i2c.addressed && idle && !i2c.addr;
                i2c.sb as u32 * I2C_SB
                    | i2c.addr as u32 * I2C_ADDR
                    | btf as u32 * I2C_BTF
                    | txe as u32 * I2C_TXE
                    | i2c.af as u32 * I2C_AF
            }
            0x18 => {
                if i2c.star1_read && i2c.addr {
                    i2c.addr = false;
                }
                i2c.star1_read = false;
                let busy = i2c.master || i2c.start_at.is_some();
                i2c.master as u32 * I2C_MSL | busy as u32 * I2C_BUSY | i2c.tra as u32 * I2C_TRA
            }
            0x1c => i2c.ckcfgr,
            _ => 0,
        }
    }

    fn i2c_write(&mut self, off: u64, v: u32, now: u64) {
        self.i2c_update(now);
        let bit = self.i2c_bit_ns();
        let i2c = &mut self.i2c;
        match off {
            0x00 => {
                if v & I2C_SWRST != 0 {
                    self.i2c = I2c { ctlr1: v, ..I2c::default() };
                    return;
                }
                i2c.ctlr1 = v;
                if v & I2C_PE == 0 {
                    return;
                }
                if v & I2C_START != 0 && i2c.start_at.is_none() {
                    // After the byte being sent, if any (a repeated start)
                    let from = if i2c.shifting.is_some() { i2c.shift_until } else { now };
                    i2c.start_at = Some(from + bit);
                }
                if v & I2C_STOP != 0 && i2c.master {
                    i2c.stop_pending = true;
                }
            }
            0x04 => {
                i2c.ctlr2 = v;
                self.dma_check();
            }
            0x08 => i2c.oaddr[0] = v,
            0x0c => i2c.oaddr[1] = v,
            0x10 => {
                let b = v as u8;
                if i2c.sb {
                    // The address
                    i2c.sb = false;
                    i2c.shifting = Some((b, true));
                    i2c.shift_until = now + 9 * bit;
                } else if i2c.addressed {
                    if i2c.shifting.is_none() {
                        i2c.shifting = Some((b, false));
                        i2c.shift_until = now + 9 * bit;
                    } else {
                        i2c.dr = Some(b);
                    }
                }
            }
            0x1c => i2c.ckcfgr = v,
            _ => {}
        }
        self.i2c_update(now);
    }

    // ---- DMA1 channel 6 (I2C1 transmit) ----

    fn dma_ch6_on(&self) -> bool {
        let cfgr = self.dma[(DMA_CH6 / 4) as usize];
        cfgr & DMA_EN != 0 && cfgr & DMA_DIR != 0 && self.i2c.ctlr2 & I2C_DMAEN != 0
    }

    /// Channel 6 and I2C1's DMA requests were enabled: ask the bus for the
    /// transfer's bytes.
    fn dma_check(&mut self) {
        let count = self.dma[(DMA_CH6 / 4 + 1) as usize] & 0xffff;
        if self.dma_ch6_on() && count != 0 && self.i2c.dma_queue.is_empty() && self.dma_request.is_none() {
            self.dma_request = Some((self.dma[(DMA_CH6 / 4 + 3) as usize], count));
        }
    }

    /// The transfer the bus is to read for DMA, if one is starting.
    pub fn take_dma_request(&mut self) -> Option<(u32, u32)> { self.dma_request.take() }

    /// The bytes of the transfer requested (read at its start: the CPU
    /// doesn't usually change them while they're being sent).
    pub fn dma_supply(&mut self, bytes: Vec<u8>, now: u64) {
        self.i2c.dma_queue = bytes.into();
        self.dma_feed(now);
        if self.i2c.shifting.is_none() {
            if let Some(b) = self.i2c.dr.take() {
                self.i2c.shifting = Some((b, false));
                self.i2c.shift_until = now + 9 * self.i2c_bit_ns();
                self.dma_feed(now);
            }
        }
    }

    /// DMA writes DATAR when it's empty; the last byte completes the
    /// transfer.
    fn dma_feed(&mut self, _t: u64) {
        if self.i2c.dr.is_some() || !self.i2c.addressed || !self.dma_ch6_on() {
            return;
        }
        if let Some(b) = self.i2c.dma_queue.pop_front() {
            self.i2c.dr = Some(b);
            if self.i2c.dma_queue.is_empty() {
                self.dma[(DMA_CH6 / 4 + 1) as usize] = 0;
                self.dma_intfr |= DMA_GIF6 | DMA_TCIF6;
                self.invalidate();
            }
        }
    }

    // ---- ADC1 ----

    fn adc_write(&mut self, off: u64, v: u32) {
        let adc = &mut self.adc;
        match off {
            0x00 => adc.eoc &= v & ADC_EOC != 0,
            0x04 => adc.ctlr1 = v,
            0x08 => {
                // Calibration finishes at once; a software start converts
                // the first regular channel at once
                adc.ctlr2 = v & !(ADC_CAL | ADC_RSTCAL | ADC_SWSTART);
                if v & ADC_SWSTART != 0 && v & ADC_ADON != 0 {
                    let ch = (adc.rsqr3 & 0x1f) as usize;
                    adc.rdatar = self.adc_in.get(ch).copied().unwrap_or(0).min(1023) as u32;
                    adc.eoc = true;
                }
            }
            0x34 => adc.rsqr3 = v,
            off => {
                if let Some(r) = adc.regs.get_mut(off as usize / 4) {
                    *r = v;
                }
            }
        }
    }

    // ---- GPIO edges ----

    /// Record port's OUTDR changing from old to new, if it's watched there.
    fn gpio_changed(&mut self, port: usize, old: u32, new: u32, now: u64) {
        if (old ^ new) & self.gpio_watch[port] != 0 {
            if self.edges.len() >= MAX_EDGES {
                self.edges.pop_front();
            }
            self.edges.push_back((now, port as u8, new as u16));
        }
    }

    /// Hand over the recorded edges: time (ns), port, OUTDR.
    pub fn take_edges(&mut self) -> Vec<(u64, u8, u16)> { self.edges.drain(..).collect() }

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
            // SysTick's clock, and the hart's, may have changed
            self.stk_schedule();
            self.clock_changed = true;
            return;
        }
        if a.wrapping_sub(GPIO_BASE) < GPIO_PORTS * 0x400 && matches!(a & 0x3ff, 0x0c | 0x10 | 0x14) {
            // OUTDR; BSHR sets (low half) and resets (high half) its bits;
            // BCR resets them
            let outdr = a & !0x3ff | 0x0c;
            let old = self.regs.get(&outdr).copied().unwrap_or(0);
            let o = match a & 0x3ff {
                0x0c => v,
                0x10 => (old | (v & 0xffff)) & !(v >> 16),
                _ => old & !(v & 0xffff),
            };
            self.regs.insert(outdr, o);
            self.gpio_changed(((a - GPIO_BASE) / 0x400) as usize, old, o, now);
            return;
        }
        if a.wrapping_sub(I2C1_BASE) < 0x400 {
            self.i2c_write(a - I2C1_BASE, v, now);
            return;
        }
        if a.wrapping_sub(ADC1_BASE) < 0x400 {
            self.adc_write(a - ADC1_BASE, v);
            return;
        }
        if a.wrapping_sub(DMA1_BASE) < 0xa0 {
            self.i2c_update(now);
            match a - DMA1_BASE {
                0x00 => {}
                // INTFCR: clearing a channel's GIF clears all its flags
                0x04 => {
                    for ch in 0..7 {
                        let f = 0xf << (4 * ch);
                        if v & 1 << (4 * ch) != 0 {
                            self.dma_intfr &= !f;
                        }
                        self.dma_intfr &= !(v & f);
                    }
                }
                off => {
                    self.dma[(off / 4) as usize] = v;
                    if off == DMA_CH6 && v & DMA_EN == 0 {
                        // Disabling the channel ends its transfer
                        self.i2c.dma_queue.clear();
                    }
                    self.dma_check();
                }
            }
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
    // Virtual time for instructions: one per cycle of HCLK, as the RCC sets
    // it (Cpu::update_mip follows its changes)
    let hz = cpu.bus.ch32.as_ref().unwrap().hclk();
    cpu.set_clock_hz(hz);
    Ok(cpu)
}
