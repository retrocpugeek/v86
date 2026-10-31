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
//! (with its transfer complete interrupt on the V003); SPI1 as a master
//! transmitter, at its clock's speed, to an ST7735 colour LCD (dev/st7735.rs)
//! on the pins the host names (chip select, data/command, reset), fed by the
//! CPU or by DMA1 channel 3 (with its interrupt, on the V003); ADC1's
//! software-started conversions, of values the host sets (e.g. for buttons
//! on a resistor ladder), and its continuous ones, timed by its clock, of
//! those, a waveform the host sets or samples it streams (e.g. a
//! microphone's), with the analog watchdog and its interrupt, and DMA1
//! channel 1 moving each result to memory (circular or not, with its half
//! and complete interrupts, on the V003); an I2C bus the firmware bit-bangs
//! on GPIO pins the host names, with the SSD1306 on it (BitI2c); GPIO inputs
//! the host drives, and edges on output
//! pins it watches (e.g. a buzzer); TIM2's PWM output as the tone it plays
//! (e.g. on a buzzer), for the host; the flash interface's fast (64-byte)
//! page erase and programming, and 1 KB page erase (for saving data). Other
//! peripheral registers (AFIO, PWR, ...) hold what's written.
//!
//! Not modelled: interrupt nesting and preemption by priority, the RV32E
//! register limit, timer counters and interrupts, I2C reception and
//! interrupts, SPI reception, other DMA channels, USB, Ethernet, flash
//! locking and standard (16-bit) programming.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::bus::Bus;
use crate::cpu::{Cpu, PRV_M};
use crate::dev::ssd1306::{self, Ssd1306};
use crate::dev::st7735::St7735;

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

    /// DMA1 channel 3's interrupt (modelled for the V003 only)
    fn dma1_ch3_irq(self) -> u32 {
        match self {
            Chip::V003 => 24,
            Chip::V307 => u32::MAX,
        }
    }

    /// DMA1 channel 1's interrupt (modelled for the V003 only)
    fn dma1_ch1_irq(self) -> u32 {
        match self {
            Chip::V003 => 22,
            Chip::V307 => u32::MAX,
        }
    }

    /// ADC1's interrupt (modelled for the V003 only)
    fn adc_irq(self) -> u32 {
        match self {
            Chip::V003 => 29,
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
const SPI1_BASE: u64 = 0x4001_3000;
const TIM2_BASE: u64 = 0x4000_0000;
const FLASH_R_BASE: u64 = 0x4002_2000;
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
/// DMA1 channel 3's registers, SPI1's transmit channel, and its INTFR flags
const DMA_CH3: u64 = 0x30;
const DMA_GIF3: u32 = 1 << 8;
const DMA_TCIF3: u32 = 1 << 9;
/// DMA1 channel 1's registers, ADC1's channel, its INTFR flags (global,
/// transfer complete, half transfer) and more CFGR bits
const DMA_CH1: u64 = 0x08;
const DMA_GIF1: u32 = 1 << 0;
const DMA_TCIF1: u32 = 1 << 1;
const DMA_HTIF1: u32 = 1 << 2;
const DMA_HTIE: u32 = 1 << 2;
const DMA_CIRC: u32 = 1 << 5;
const DMA_MINC: u32 = 1 << 7;
// SPI CTLR1, CTLR2 and STATR bits
const SPI_SPE: u32 = 1 << 6;
const SPI_DFF: u32 = 1 << 11;
const SPI_TXDMAEN: u32 = 1 << 1;
const SPI_TXE: u32 = 1 << 1;
const SPI_BSY: u32 = 1 << 7;
// Flash interface CTLR bits, and its fast programming page
const FLASH_PER: u32 = 1 << 1;
const FLASH_STRT: u32 = 1 << 6;
const FLASH_PAGE_PG: u32 = 1 << 16;
const FLASH_PAGE_ER: u32 = 1 << 17;
const FLASH_BUF_LOAD: u32 = 1 << 18;
const FLASH_BUF_RST: u32 = 1 << 19;
const FLASH_PAGE: usize = 64;
/// Tone changes kept for the host (Ch32::take_tones) before the oldest are
/// dropped
const MAX_TONES: usize = 4096;

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
const ADC_CONT: u32 = 1 << 1;
const ADC_DMA: u32 = 1 << 8;
const ADC_AWD: u32 = 1 << 0;
// ADC CTLR1 bits: the analog watchdog's interrupt enable, single channel and
// enable (regular channels)
const ADC_AWDIE: u32 = 1 << 6;
const ADC_AWDSGL: u32 = 1 << 9;
const ADC_AWDEN: u32 = 1 << 23;
/// How many conversions ahead the analog watchdog's next trip is looked for
const AWD_HORIZON: u64 = 4096;
/// Most of a streamed input kept ahead of the conversions (s): more is
/// dropped, catching up
const STREAM_BACKLOG_S: f64 = 0.25;
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
    /// The last STAR1 read showed ADDR: ADDR is cleared by reading STAR1
    /// (with ADDR set) then STAR2. (Only a read that saw it counts: else an
    /// address finishing between a driver's STAR1 and STAR2 reads would be
    /// cleared unseen, and the driver would wait for it forever.)
    star1_saw_addr: bool,
    /// The device addressed acknowledged (the display)
    to_display: bool,
    /// Bytes DMA1 channel 6 has yet to write to DATAR
    dma_queue: VecDeque<u8>,
}

/// SPI1 as a master transmitter, to the LCD.
#[derive(Default)]
struct Spi {
    ctlr1: u32,
    ctlr2: u32,
    /// The words written are sent by then (ns)
    busy_until: u64,
    /// A DMA transfer (channel 3) completes then (ns)
    dma_done_at: Option<u64>,
}

/// A change to what the flash holds, for the bus to make: the bytes at an
/// offset into it
pub enum FlashOp {
    Erase(usize, usize),
    Program(usize, [u8; FLASH_PAGE]),
}

/// ADC1, converting on software start (SWSTART), once or continuously
/// (CONT): a conversion takes its channel's sample time and 11 cycles of
/// ADCCLK (HCLK through the RCC's prescaler).
#[derive(Default)]
struct Adc {
    ctlr1: u32,
    ctlr2: u32,
    rsqr3: u32,
    eoc: bool,
    rdatar: u32,
    regs: [u32; 20],
    /// STATR's analog watchdog flag
    awd: bool,
    /// Converting continuously: conversion k (counting from 1) ends at
    /// t0 + k * period_num / period_den (ns); `done` of them have
    running: bool,
    t0: u64,
    done: u64,
    period_num: u64,
    period_den: u64,
    /// When conversion done + 1 ends (ns)
    next_t: u64,
    /// The first conversion from done + 1 that trips the analog watchdog
    /// (u64::MAX for none), of those before awd_horizon; awd_valid is false
    /// when it must be looked for again (the window, input or timing changed)
    awd_next: u64,
    awd_horizon: u64,
    awd_valid: bool,
    /// DMA1 channel 1's transfer: its length (CNTR when enabled), and how
    /// many items of it are done
    dma_len: u32,
    dma_idx: u32,
}

/// A waveform the host sets as an ADC channel's input: its shape (1 sine,
/// 2 square, 3 triangle, 4 sawtooth), frequency (mHz), and centre and
/// amplitude (ADC codes).
#[derive(Clone, Copy)]
pub struct Wave {
    pub ch: usize,
    pub shape: u32,
    pub mhz: u64,
    pub mid: f64,
    pub amp: f64,
}

impl Wave {
    fn at(&self, t: u64) -> u32 {
        // The phase from integers: t * f is too large for an f64's precision
        let p = (t as u128 * self.mhz as u128 % 1_000_000_000_000) as f64 / 1e12;
        let x = match self.shape {
            1 => (2.0 * std::f64::consts::PI * p).sin(),
            2 => if p < 0.5 { 1.0 } else { -1.0 },
            3 => 1.0 - 4.0 * (p - 0.5).abs(),
            4 => 2.0 * p - 1.0,
            _ => 0.0,
        };
        (self.mid + self.amp * x).round().clamp(0.0, 1023.0) as u32
    }
}

/// Samples (ADC codes) the host streams as an ADC channel's input, e.g. a
/// microphone's, period_ns apart: the first is at front_t (ns), set when
/// they first meet a conversion. Between samples the input is interpolated;
/// past the last it holds, and the next samples start afresh.
pub struct Stream {
    pub ch: usize,
    period_ns: f64,
    front_t: Option<f64>,
    samples: VecDeque<u16>,
    dry: bool,
}

impl Stream {
    pub fn new(ch: usize, rate: u32) -> Stream {
        Stream { ch, period_ns: 1e9 / rate.max(1) as f64, front_t: None, samples: VecDeque::new(), dry: false }
    }

    pub fn push(&mut self, samples: &[u16]) {
        if self.dry {
            self.samples.clear();
            self.front_t = None;
            self.dry = false;
        }
        self.samples.extend(samples.iter().map(|&v| v.min(1023)));
    }

    fn at(&self, t: u64) -> Option<u32> {
        let ft = self.front_t?;
        let last = *self.samples.back()?;
        let pos = (t as f64 - ft) / self.period_ns;
        if pos <= 0.0 {
            return Some(self.samples[0] as u32);
        }
        let i = pos as usize;
        if i + 1 >= self.samples.len() {
            return Some(last as u32);
        }
        let (a, b) = (self.samples[i] as f64, self.samples[i + 1] as f64);
        Some((a + (b - a) * (pos - i as f64)).round() as u32)
    }

    /// The conversions have reached `t`: start the samples there if they
    /// haven't started, drop those passed and those too far ahead.
    fn advance(&mut self, t: u64) {
        let Some(ft) = self.front_t else {
            if !self.samples.is_empty() {
                self.front_t = Some(t as f64);
            }
            return;
        };
        let n = ((t as f64 - ft) / self.period_ns).max(0.0) as usize;
        if n >= self.samples.len() {
            self.dry = true;
        }
        let drop = n.min(self.samples.len().saturating_sub(1));
        self.samples.drain(..drop);
        self.front_t = Some(ft + drop as f64 * self.period_ns);
        let max = (STREAM_BACKLOG_S * 1e9 / self.period_ns) as usize;
        if self.samples.len() > max {
            // Behind: skip ahead to the newest
            let extra = self.samples.len() - max;
            self.samples.drain(..extra);
        }
    }
}

/// An I2C bus the firmware bit-bangs on two GPIO pins (open drain), with
/// the SSD1306 display on it: the pins' levels are decoded into STARTs,
/// STOPs and bytes, and the display acknowledges its address and the bytes
/// after it by pulling SDA low for the ninth clock.
struct BitI2c {
    scl: (usize, u32),
    sda: (usize, u32),
    scl_hi: bool,
    sda_hi: bool,
    /// The byte being clocked in, and how many of its bits (9: the
    /// acknowledge clock)
    byte: u8,
    nbits: u8,
    /// The next byte is the address; the display was addressed
    first: bool,
    to_display: bool,
    /// The byte clocked in is acknowledged; SDA is being pulled low for it
    ack: bool,
    ack_drive: bool,
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
    /// ADC1's inputs (channels 0 to 9; 8 is the internal 1.2 V reference),
    /// and a waveform or samples the host sets for one instead
    pub adc_in: [u16; 10],
    pub adc_wave: Option<Wave>,
    pub adc_stream: Option<Stream>,
    /// DMA1 channel 1's writes for the bus to make: address, value, size
    dma_writes: Vec<(u32, u32, u32)>,
    /// The I2C bus bit-banged on GPIO pins, if there's one
    bitbang: Option<BitI2c>,
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
    /// A transfer DMA1 channel 6 or 3 is to start: the channel, its memory
    /// address and length in bytes; the bus reads the bytes and hands them
    /// over (dma_supply)
    dma_request: Option<(u8, u32, u32)>,
    dma_supply_ch: u8,
    spi: Spi,
    /// The LCD on SPI1, and its pins (port, pin): chip select (low to
    /// select), data/command (high for data) and reset (low to reset)
    pub lcd: Option<St7735>,
    pub lcd_pins: [(usize, u32); 3],
    /// TIM2's tone (Hz, 0 for none), and its changes (time in ns, Hz)
    tone: u32,
    tones: VecDeque<(u64, u32)>,
    /// The flash interface's page buffer, and a change for the bus to make
    flash_buf: [u8; FLASH_PAGE],
    flash_op: Option<FlashOp>,
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
            // (WDHTR's reset value: the watchdog's window is everything)
            adc: Adc { regs: { let mut r = [0; 20]; r[9] = 0x3ff; r }, ..Adc::default() },
            // VREFINT: 1.2 V of a 3.3 V supply, 10 bits
            adc_in: [0, 0, 0, 0, 0, 0, 0, 0, 372, 0],
            adc_wave: None,
            adc_stream: None,
            dma_writes: Vec::new(),
            bitbang: None,
            gpio_in: [(0, 0); GPIO_PORTS as usize],
            gpio_watch: [0; GPIO_PORTS as usize],
            edges: VecDeque::new(),
            clock_changed: false,
            dma: [0; 40],
            dma_intfr: 0,
            dma_request: None,
            dma_supply_ch: 0,
            spi: Spi::default(),
            lcd: None,
            lcd_pins: [(0, 0); 3],
            tone: 0,
            tones: VecDeque::new(),
            flash_buf: [0xff; FLASH_PAGE],
            flash_op: None,
        }
    }

    /// Put an ST7735 LCD on SPI1, its chip select, data/command and reset
    /// on these pins (port, pin; port 0 is GPIOA).
    pub fn attach_lcd(&mut self, cs: (usize, u32), dc: (usize, u32), rst: (usize, u32)) {
        self.lcd = Some(St7735::default());
        self.lcd_pins = [cs, dc, rst];
    }

    /// Put an I2C bus, bit-banged by the firmware, with the SSD1306 display
    /// on it, on these pins (port, pin; port 0 is GPIOA).
    pub fn attach_bitbang_i2c(&mut self, scl: (usize, u32), sda: (usize, u32)) {
        self.bitbang = Some(BitI2c {
            scl,
            sda,
            scl_hi: true,
            sda_hi: true,
            byte: 0,
            nbits: 0,
            first: false,
            to_display: false,
            ack: false,
            ack_drive: false,
        });
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
        self.spi_update(now);
        self.adc_run(now);
        if self.adc_armed() && (!self.adc.awd_valid || self.adc.done + 1 >= self.adc.awd_horizon) {
            self.adc_awd_scan();
        }
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
    /// while DMA feeds I2C1, its bytes, the end of a DMA transfer to SPI1,
    /// and the next ADC conversion that interrupts.
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
        self.stk_next.min(dma).min(self.spi.dma_done_at.unwrap_or(u64::MAX)).min(self.adc_next_event_ns())
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
            _ if n == self.chip.dma1_ch3_irq() => {
                self.dma_intfr & DMA_TCIF3 != 0 && self.dma[(DMA_CH3 / 4) as usize] & DMA_TCIE != 0
            }
            _ if n == self.chip.dma1_ch1_irq() => {
                let cfgr = self.dma[(DMA_CH1 / 4) as usize];
                self.dma_intfr & DMA_TCIF1 != 0 && cfgr & DMA_TCIE != 0
                    || self.dma_intfr & DMA_HTIF1 != 0 && cfgr & DMA_HTIE != 0
            }
            _ if n == self.chip.adc_irq() => self.adc.awd && self.adc.ctlr1 & ADC_AWDIE != 0,
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
        let candidates = [
            IRQ_SYSTICK,
            IRQ_SOFTWARE,
            self.chip.usart1_irq(),
            self.chip.dma1_ch6_irq(),
            self.chip.dma1_ch3_irq(),
            self.chip.dma1_ch1_irq(),
            self.chip.adc_irq(),
        ];
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
            let port = ((a - GPIO_BASE) / 0x400) as usize;
            let (mask, levels) = self.gpio_in[port];
            let out = self.regs.get(&(a + 4)).copied().unwrap_or(0);
            // (the display pulling SDA low to acknowledge)
            let ack = match &self.bitbang {
                Some(b) if b.ack_drive && b.sda.0 == port => 1 << b.sda.1,
                _ => 0,
            };
            return (out & !mask | levels & mask) & !ack;
        }
        if a.wrapping_sub(I2C1_BASE) < 0x400 {
            return self.i2c_read(a - I2C1_BASE, now);
        }
        if a.wrapping_sub(SPI1_BASE) < 0x400 {
            self.spi_update(now);
            let word_ns = self.spi_word_ns();
            let spi = &self.spi;
            return match a - SPI1_BASE {
                0x00 => spi.ctlr1,
                0x04 => spi.ctlr2,
                // STATR: room for a word once the one before the last is
                // sent; busy until the last is
                0x08 => {
                    let txe = now + word_ns >= spi.busy_until;
                    txe as u32 * SPI_TXE | (now < spi.busy_until) as u32 * SPI_BSY
                }
                _ => 0,
            };
        }
        if a.wrapping_sub(DMA1_BASE) < 0xa0 {
            self.i2c_update(now);
            self.spi_update(now);
            self.adc_run(now);
            return match a - DMA1_BASE {
                0x00 => self.dma_intfr,
                0x04 => 0,
                // Channel 6's count: what's left of the transfer
                off if off == DMA_CH6 + 4 && !self.i2c.dma_queue.is_empty() => self.i2c.dma_queue.len() as u32,
                off => self.dma[(off / 4) as usize],
            };
        }
        if a.wrapping_sub(ADC1_BASE) < 0x400 {
            self.adc_run(now);
            let adc = &mut self.adc;
            return match a - ADC1_BASE {
                0x00 => (adc.eoc as u32) << 1 | adc.awd as u32,
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
                i2c.star1_saw_addr = i2c.addr;
                let txe = i2c.addressed && i2c.dr.is_none();
                let btf = i2c.addressed && idle && !i2c.addr;
                i2c.sb as u32 * I2C_SB
                    | i2c.addr as u32 * I2C_ADDR
                    | btf as u32 * I2C_BTF
                    | txe as u32 * I2C_TXE
                    | i2c.af as u32 * I2C_AF
            }
            0x18 => {
                if i2c.star1_saw_addr {
                    i2c.addr = false;
                }
                i2c.star1_saw_addr = false;
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
            self.dma_request = Some((6, self.dma[(DMA_CH6 / 4 + 3) as usize], count));
        }
    }

    /// The transfer the bus is to read for DMA, if one is starting: its
    /// memory address and length in bytes.
    pub fn take_dma_request(&mut self) -> Option<(u32, u32)> {
        let (ch, addr, len) = self.dma_request.take()?;
        self.dma_supply_ch = ch;
        Some((addr, len))
    }

    /// The bytes of the transfer requested (read at its start: the CPU
    /// doesn't usually change them while they're being sent).
    pub fn dma_supply(&mut self, bytes: Vec<u8>, now: u64) {
        if self.dma_supply_ch == 3 {
            self.spi_dma_supply(bytes, now);
            return;
        }
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

    fn adc_write(&mut self, off: u64, v: u32, now: u64) {
        self.adc_run(now);
        // The watchdog's next trip may have changed
        self.adc.awd_valid = false;
        match off {
            // STATR: writing 0 clears a flag
            0x00 => {
                self.adc.eoc &= v & ADC_EOC != 0;
                self.adc.awd &= v & ADC_AWD != 0;
            }
            0x04 => self.adc.ctlr1 = v,
            0x08 => {
                // Calibration finishes at once; a software start converts
                // the first regular channel at once, or starts converting
                // it continuously
                self.adc.ctlr2 = v & !(ADC_CAL | ADC_RSTCAL | ADC_SWSTART);
                if v & ADC_ADON == 0 {
                    self.adc.running = false;
                } else if v & ADC_SWSTART != 0 {
                    if v & ADC_CONT != 0 {
                        self.adc.running = true;
                        self.adc.t0 = now;
                        self.adc.done = 0;
                        self.adc_retime();
                    } else {
                        self.adc.rdatar = self.adc_value(now);
                        self.adc.eoc = true;
                    }
                }
            }
            0x34 => {
                self.adc.rsqr3 = v;
                self.adc_retime();
            }
            off => {
                if let Some(r) = self.adc.regs.get_mut(off as usize / 4) {
                    *r = v;
                }
                // SAMPTR1 and 2: the sample times
                if off == 0x0c || off == 0x10 {
                    self.adc_retime();
                }
            }
        }
    }

    /// ADCCLK's divisor of HCLK (the RCC's ADCPRE).
    fn adc_div(&self) -> u64 {
        let cfgr0 = self.rcc[1];
        match self.chip {
            Chip::V003 => {
                // 0b0xx00 to 0b11x00: 2, 4, 6, 8; with bit 2 set, (b4:3 + 1)
                // * 4, doubled by b1:0
                let f = (cfgr0 >> 11 & 0x1f) as u64;
                if f & 4 == 0 { [2, 4, 6, 8][(f >> 3 & 3) as usize] } else { (((f >> 3 & 3) + 1) * 4) << (f & 3) }
            }
            Chip::V307 => [2, 4, 6, 8][(cfgr0 >> 14 & 3) as usize],
        }
    }

    /// Time the conversions from here with the clock and sample time now
    /// set, continuing from the last one's end.
    fn adc_retime(&mut self) {
        if self.adc.running {
            self.adc.t0 = self.adc_time(self.adc.done);
            self.adc.done = 0;
        }
        let ch = self.adc.rsqr3 & 0x1f;
        let smp = if ch < 10 { self.adc.regs[4] >> (3 * ch) } else { self.adc.regs[3] >> (3 * (ch - 10)) } & 7;
        let cycles = [3, 9, 15, 30, 43, 57, 73, 241][smp as usize] + 11;
        self.adc.period_num = cycles * self.adc_div() * 1_000_000_000;
        self.adc.period_den = self.hclk().max(1);
        self.adc.next_t = self.adc_time(self.adc.done + 1);
        self.adc.awd_valid = false;
    }

    /// When continuous conversion k ends (ns; rounded up, as adc_run counts
    /// those ended by a time).
    fn adc_time(&self, k: u64) -> u64 {
        let a = &self.adc;
        a.t0 + (k as u128 * a.period_num as u128).div_ceil(a.period_den.max(1) as u128) as u64
    }

    /// The input of the channel converted, at `t` (ns).
    fn adc_value(&self, t: u64) -> u32 { self.adc_input((self.adc.rsqr3 & 0x1f) as usize, t) }

    /// ADC channel ch's input at `t` (ns): the samples streamed for it,
    /// else the waveform set for it, else the level set for it.
    pub fn adc_input(&self, ch: usize, t: u64) -> u32 {
        if let Some(v) = self.adc_stream.as_ref().filter(|s| s.ch == ch).and_then(|s| s.at(t)) {
            return v;
        }
        if let Some(w) = self.adc_wave.filter(|w| w.ch == ch) {
            return w.at(t);
        }
        self.adc_in.get(ch).copied().unwrap_or(0).min(1023) as u32
    }

    /// The streamed input or waveform changed: the watchdog's next trip may
    /// have.
    pub fn adc_input_changed(&mut self) { self.adc.awd_valid = false }

    /// The analog watchdog is on and hasn't tripped.
    fn adc_armed(&self) -> bool { self.adc.running && self.adc.ctlr1 & ADC_AWDEN != 0 && !self.adc.awd }

    /// Does the watchdog trip on value v (of the channel converted)?
    fn adc_trips(&self, v: u32) -> bool {
        let a = &self.adc;
        let watched = a.ctlr1 & ADC_AWDSGL == 0 || a.ctlr1 & 0x1f == a.rsqr3 & 0x1f;
        watched && (v > a.regs[9] & 0x3ff || v < a.regs[10] & 0x3ff)
    }

    /// Look for the watchdog's next trip, up to AWD_HORIZON conversions ahead.
    fn adc_awd_scan(&mut self) {
        let from = self.adc.done + 1;
        let to = from + AWD_HORIZON;
        self.adc.awd_next = (from..to).find(|&k| self.adc_trips(self.adc_value(self.adc_time(k)))).unwrap_or(u64::MAX);
        self.adc.awd_horizon = to;
        self.adc.awd_valid = true;
    }

    /// DMA1 channel 1 and ADC1's DMA requests are on, with items left.
    fn adc_dma_on(&self) -> bool {
        let ch = (DMA_CH1 / 4) as usize;
        self.dma[ch] & DMA_EN != 0 && self.adc.ctlr2 & ADC_DMA != 0 && self.dma[ch + 1] & 0xffff != 0
    }

    /// DMA1 channel 1 moves a conversion's result to memory.
    fn adc_dma_transfer(&mut self, v: u32) {
        let ch = (DMA_CH1 / 4) as usize;
        let cfgr = self.dma[ch];
        let size = 1 << (cfgr >> 10 & 3);
        let step = if cfgr & DMA_MINC != 0 { size } else { 0 };
        self.dma_writes.push((self.dma[ch + 3].wrapping_add(self.adc.dma_idx * step), v, size));
        self.adc.dma_idx += 1;
        let left = (self.dma[ch + 1] & 0xffff) - 1;
        self.dma[ch + 1] = left;
        if self.adc.dma_idx == self.adc.dma_len / 2 {
            self.dma_intfr |= DMA_GIF1 | DMA_HTIF1;
        }
        if left == 0 {
            self.dma_intfr |= DMA_GIF1 | DMA_TCIF1;
            if cfgr & DMA_CIRC != 0 {
                self.dma[ch + 1] = self.adc.dma_len;
                self.adc.dma_idx = 0;
            }
        }
    }

    /// Bring the continuous conversions up to `now`: with DMA, each one's
    /// result goes to memory and the watchdog looks at it; without, the
    /// watchdog's next trip is all that matters.
    fn adc_run(&mut self, now: u64) {
        if !self.adc.running || now < self.adc.next_t || self.adc.period_num == 0 {
            return;
        }
        let a = &self.adc;
        let target = if now < a.t0 { 0 } else { ((now - a.t0) as u128 * a.period_den as u128 / a.period_num as u128) as u64 };
        if target <= a.done {
            return;
        }
        self.invalidate();
        while self.adc.done < target {
            if self.adc_dma_on() {
                let k = self.adc.done + 1;
                let v = self.adc_value(self.adc_time(k));
                if self.adc_armed() && self.adc_trips(v) {
                    self.adc.awd = true;
                }
                self.adc_dma_transfer(v);
                self.adc.done = k;
                continue;
            }
            if self.adc_armed() {
                if !self.adc.awd_valid || self.adc.done + 1 >= self.adc.awd_horizon {
                    self.adc_awd_scan();
                }
                if self.adc.awd_next <= target {
                    self.adc.awd = true;
                    self.adc.done = self.adc.awd_next;
                } else {
                    self.adc.done = target.min(self.adc.awd_horizon - 1);
                }
                continue;
            }
            self.adc.done = target;
        }
        let t = self.adc_time(self.adc.done);
        self.adc.next_t = self.adc_time(self.adc.done + 1);
        self.adc.rdatar = self.adc_value(t);
        self.adc.eoc = true;
        if let Some(s) = self.adc_stream.as_mut() {
            s.advance(t);
        }
    }

    /// When the next conversion that interrupts ends (ns), or u64::MAX: the
    /// watchdog's next trip, DMA's half and complete points.
    fn adc_next_event_ns(&self) -> u64 {
        let a = &self.adc;
        if !a.running {
            return u64::MAX;
        }
        let mut k = u64::MAX;
        if self.adc_armed() && a.ctlr1 & ADC_AWDIE != 0 {
            k = if a.awd_valid { a.awd_next.min(a.awd_horizon) } else { a.done + 1 };
        }
        if self.adc_dma_on() {
            let ch = (DMA_CH1 / 4) as usize;
            let (cfgr, left) = (self.dma[ch], (self.dma[ch + 1] & 0xffff) as u64);
            let half = (a.dma_len / 2) as u64;
            if cfgr & DMA_HTIE != 0 && (a.dma_idx as u64) < half {
                k = k.min(a.done + half - a.dma_idx as u64);
            }
            if cfgr & DMA_TCIE != 0 {
                k = k.min(a.done + left);
            }
        }
        if k == u64::MAX { u64::MAX } else { self.adc_time(k) }
    }

    /// Hand over DMA1 channel 1's writes for the bus to make: address,
    /// value, size.
    pub fn take_dma_writes(&mut self) -> Option<Vec<(u32, u32, u32)>> {
        (!self.dma_writes.is_empty()).then(|| std::mem::take(&mut self.dma_writes))
    }

    // ---- SPI1 and DMA1 channel 3 (SPI1 transmit), to the LCD ----

    /// A word's time on the bus (ns): 8 or 16 SCK periods of HCLK (the
    /// V003's PCLK2) / 2^(BR + 1).
    fn spi_word_ns(&self) -> u64 {
        let bits = if self.spi.ctlr1 & SPI_DFF != 0 { 16 } else { 8 };
        let div = 2u64 << (self.spi.ctlr1 >> 3 & 7);
        (bits * div * 1_000_000_000 / self.hclk().max(1)).max(1)
    }

    fn gpio_level(&self, (port, pin): (usize, u32)) -> bool {
        self.regs.get(&(GPIO_BASE + 0x400 * port as u64 + 0x0c)).copied().unwrap_or(0) >> pin & 1 != 0
    }

    /// A word to the LCD (most significant byte first), if it's selected.
    fn spi_send(&mut self, w: u32) {
        let [cs, dc, _] = self.lcd_pins;
        if self.lcd.is_none() || self.gpio_level(cs) {
            return;
        }
        let data = self.gpio_level(dc);
        let lcd = self.lcd.as_mut().unwrap();
        if self.spi.ctlr1 & SPI_DFF != 0 {
            lcd.byte((w >> 8) as u8, data);
        }
        lcd.byte(w as u8, data);
    }

    fn spi_write(&mut self, off: u64, v: u32, now: u64) {
        self.spi_update(now);
        match off {
            0x00 => self.spi.ctlr1 = v,
            0x04 => {
                self.spi.ctlr2 = v;
                self.spi_dma_check(now);
            }
            0x0c => {
                if self.spi.ctlr1 & SPI_SPE != 0 {
                    // Taken at once (the LCD only listens); the bus is busy
                    // while it's shifted out
                    self.spi_send(if self.spi.ctlr1 & SPI_DFF != 0 { v & 0xffff } else { v & 0xff });
                    self.spi.busy_until = self.spi.busy_until.max(now) + self.spi_word_ns();
                }
            }
            _ => {}
        }
    }

    /// A DMA transfer to SPI1 has ended: CNTR 0, and its flags.
    fn spi_update(&mut self, now: u64) {
        if self.spi.dma_done_at.is_some_and(|t| now >= t) {
            self.spi.dma_done_at = None;
            self.dma[(DMA_CH3 / 4 + 1) as usize] = 0;
            self.dma_intfr |= DMA_GIF3 | DMA_TCIF3;
            self.invalidate();
        }
    }

    /// Channel 3, from memory to SPI1's DATAR, and SPI1's DMA requests, were
    /// enabled: ask the bus for the transfer's bytes.
    fn spi_dma_check(&mut self, _now: u64) {
        let ch = (DMA_CH3 / 4) as usize;
        let (cfgr, count, paddr) = (self.dma[ch], self.dma[ch + 1] & 0xffff, self.dma[ch + 2]);
        let on = cfgr & DMA_EN != 0 && cfgr & DMA_DIR != 0 && self.spi.ctlr2 & SPI_TXDMAEN != 0;
        if on && count != 0 && paddr as u64 == SPI1_BASE + 0x0c && self.spi.dma_done_at.is_none() && self.dma_request.is_none() {
            let msize = 1 << (cfgr >> 10 & 3);
            self.dma_request = Some((3, self.dma[ch + 3], count * msize));
        }
    }

    /// The transfer's bytes go to the LCD now; it ends when SPI1 would have
    /// sent them.
    fn spi_dma_supply(&mut self, bytes: Vec<u8>, now: u64) {
        let cfgr = self.dma[(DMA_CH3 / 4) as usize];
        let msize = 1 << (cfgr >> 10 & 3);
        let words = bytes.len() / msize;
        for w in bytes.chunks(msize) {
            let v = w.iter().rev().fold(0u32, |v, &b| v << 8 | b as u32);
            self.spi_send(if self.spi.ctlr1 & SPI_DFF != 0 { v & 0xffff } else { v & 0xff });
        }
        self.spi.busy_until = self.spi.busy_until.max(now) + words as u64 * self.spi_word_ns();
        self.spi.dma_done_at = Some(self.spi.busy_until);
    }

    // ---- TIM2's tone ----

    /// The tone TIM2 plays: HCLK / (PSC + 1) / (ATRLR + 1) while it counts
    /// and a channel's output (CCxE) is on with a duty cycle; recorded when
    /// it changes.
    fn tim2_tone(&mut self, now: u64) {
        let r = |off: u64| self.regs.get(&(TIM2_BASE + off)).copied().unwrap_or(0);
        let (ctlr1, ccer, psc, arr) = (r(0x00), r(0x20), r(0x28) & 0xffff, r(0x2c) & 0xffff);
        let duty = (0..4).any(|c| ccer >> (4 * c) & 1 != 0 && r(0x34 + 4 * c as u64) != 0);
        let hz = if ctlr1 & 1 != 0 && duty && arr != 0 {
            (self.hclk() / (psc as u64 + 1) / (arr as u64 + 1)) as u32
        } else {
            0
        };
        if hz != self.tone {
            self.tone = hz;
            if self.tones.len() >= MAX_TONES {
                self.tones.pop_front();
            }
            self.tones.push_back((now, hz));
        }
    }

    /// Hand over TIM2's tone changes: time (ns), Hz (0: silence).
    pub fn take_tones(&mut self) -> Vec<(u64, u32)> { self.tones.drain(..).collect() }

    // ---- The flash interface ----

    fn flash_write_reg(&mut self, a: u64, mut v: u32) {
        if a - FLASH_R_BASE == 0x10 {
            if v & FLASH_BUF_RST != 0 {
                self.flash_buf = [0xff; FLASH_PAGE];
            }
            if v & FLASH_STRT != 0 {
                let addr = self.regs.get(&(FLASH_R_BASE + 0x14)).copied().unwrap_or(0) as usize & 0xff_ffff;
                if v & FLASH_PAGE_ER != 0 {
                    self.flash_op = Some(FlashOp::Erase(addr & !(FLASH_PAGE - 1), FLASH_PAGE));
                } else if v & FLASH_PAGE_PG != 0 {
                    self.flash_op = Some(FlashOp::Program(addr & !(FLASH_PAGE - 1), self.flash_buf));
                } else if v & FLASH_PER != 0 {
                    self.flash_op = Some(FlashOp::Erase(addr & !1023, 1024));
                }
            }
            // (each finishes at once: STATR never shows BSY)
            v &= !(FLASH_STRT | FLASH_BUF_LOAD | FLASH_BUF_RST);
        }
        self.regs.insert(a, v);
    }

    /// A write to the flash (offset into it): in fast programming mode, it
    /// goes into the page buffer (and BUF_LOAD keeps it there).
    pub fn flash_store(&mut self, off: usize, size: u32, value: u64) {
        let ctlr = self.regs.get(&(FLASH_R_BASE + 0x10)).copied().unwrap_or(0);
        if ctlr & FLASH_PAGE_PG != 0 {
            for i in 0..size as usize {
                self.flash_buf[(off + i) % FLASH_PAGE] = (value >> (8 * i)) as u8;
            }
        }
    }

    /// The erase or programming the bus is to do to the flash, if one started.
    pub fn take_flash_op(&mut self) -> Option<FlashOp> { self.flash_op.take() }

    // ---- GPIO edges ----

    /// Record port's OUTDR changing from old to new, if it's watched there.
    fn gpio_changed(&mut self, port: usize, old: u32, new: u32, now: u64) {
        if self.bitbang.as_ref().is_some_and(|b| b.scl.0 == port || b.sda.0 == port) {
            self.bitbang_update();
        }
        let (rst_port, rst_pin) = self.lcd_pins[2];
        if port == rst_port && old >> rst_pin & 1 != 0 && new >> rst_pin & 1 == 0 {
            if let Some(lcd) = self.lcd.as_mut() {
                lcd.reset();
            }
        }
        if (old ^ new) & self.gpio_watch[port] != 0 {
            if self.edges.len() >= MAX_EDGES {
                self.edges.pop_front();
            }
            self.edges.push_back((now, port as u8, new as u16));
        }
    }

    /// The bit-banged I2C bus's pins may have changed: decode them.
    fn bitbang_update(&mut self) {
        let out = |s: &Ch32, (port, pin): (usize, u32)| s.gpio_out(port as u32) >> pin & 1 != 0;
        let Some(b) = self.bitbang.as_ref() else { return };
        let (scl, sda_out) = (out(self, b.scl), out(self, b.sda));
        let b = self.bitbang.as_mut().unwrap();
        let sda = sda_out && !b.ack_drive;
        if sda != b.sda_hi {
            b.sda_hi = sda;
            if b.scl_hi {
                if !sda {
                    // START (or a repeated one)
                    b.nbits = 0;
                    b.first = true;
                    b.ack = false;
                } else {
                    // STOP
                    b.to_display = false;
                    b.first = false;
                    b.nbits = 0;
                }
            }
        }
        if scl != b.scl_hi {
            b.scl_hi = scl;
            if scl {
                if b.nbits < 8 {
                    b.byte = b.byte << 1 | sda as u8;
                    b.nbits += 1;
                    if b.nbits == 8 {
                        if b.first {
                            b.first = false;
                            b.to_display = b.byte >> 1 == ssd1306::ADDR && b.byte & 1 == 0;
                            if b.to_display {
                                self.display.start();
                            }
                            b.ack = b.to_display;
                        } else {
                            if b.to_display {
                                self.display.byte(b.byte);
                            }
                            b.ack = b.to_display;
                        }
                    }
                } else if b.nbits == 8 {
                    b.nbits = 9;
                }
            } else if b.nbits == 8 && b.ack {
                b.ack_drive = true;
            } else if b.nbits == 9 {
                b.ack_drive = false;
                b.ack = false;
                b.nbits = 0;
            }
            // (SDA as the display now leaves it, SCL being low)
            b.sda_hi = sda_out && !b.ack_drive;
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
            // SysTick's clock, the hart's and ADCCLK may have changed
            self.stk_schedule();
            self.adc_retime();
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
            self.adc_write(a - ADC1_BASE, v, now);
            return;
        }
        if a.wrapping_sub(SPI1_BASE) < 0x400 {
            self.spi_write(a - SPI1_BASE, v, now);
            return;
        }
        if a.wrapping_sub(TIM2_BASE) < 0x400 {
            self.regs.insert(a, v);
            self.tim2_tone(now);
            return;
        }
        if a.wrapping_sub(FLASH_R_BASE) < 0x400 {
            self.flash_write_reg(a, v);
            return;
        }
        if a.wrapping_sub(DMA1_BASE) < 0xa0 {
            self.i2c_update(now);
            self.spi_update(now);
            self.adc_run(now);
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
                    let old = self.dma[(off / 4) as usize];
                    self.dma[(off / 4) as usize] = v;
                    if off == DMA_CH1 && v & DMA_EN != 0 && old & DMA_EN == 0 {
                        // Enabling channel 1 starts its transfer
                        self.adc.dma_len = self.dma[(DMA_CH1 / 4 + 1) as usize] & 0xffff;
                        self.adc.dma_idx = 0;
                    }
                    if off == DMA_CH6 && v & DMA_EN == 0 {
                        // Disabling the channel ends its transfer
                        self.i2c.dma_queue.clear();
                    }
                    if off == DMA_CH3 && v & DMA_EN == 0 {
                        self.spi.dma_done_at = None;
                    }
                    self.dma_check();
                    self.spi_dma_check(now);
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

#[cfg(test)]
mod tests {
    use super::*;

    const STAR1: u64 = I2C1_BASE + 0x14;
    const STAR2: u64 = I2C1_BASE + 0x18;

    /// An I2C1 at 400 kHz (24 MHz HCLK, fast mode) that has sent a START
    /// and the display's address at time 0; returns when the address is
    /// done (ns).
    fn addressing() -> (Ch32, u64) {
        let mut c = Ch32::new(Chip::V003, Box::new(|_| {}), 0);
        c.write(RCC_BASE + 4, 4, 0, 0);
        c.write(I2C1_BASE + 0x1c, 4, 20 | 1 << 15, 0);
        c.write(I2C1_BASE, 4, (I2C_PE | I2C_START) as u64, 0);
        let mut t = 0;
        while c.read(STAR1, 4, t).unwrap() as u32 & I2C_SB == 0 {
            t += 100;
        }
        c.write(I2C1_BASE + 0x10, 4, (ssd1306::ADDR as u64) << 1, t);
        let done = t + 9 * c.i2c_bit_ns();
        (c, done)
    }

    #[test]
    fn addr_finishing_between_star1_and_star2_is_not_lost() {
        let (mut c, done) = addressing();
        // The driver's check reads STAR1 (before the address is done), then
        // STAR2 (after): ADDR must still be there for its next STAR1 read
        assert_eq!(c.read(STAR1, 2, done - 1).unwrap() as u32 & I2C_ADDR, 0);
        c.read(STAR2, 2, done + 1);
        assert_ne!(c.read(STAR1, 2, done + 2).unwrap() as u32 & I2C_ADDR, 0);
        // ... and that STAR1 read, then STAR2, clears it
        c.read(STAR2, 2, done + 3);
        assert_eq!(c.read(STAR1, 2, done + 4).unwrap() as u32 & I2C_ADDR, 0);
    }

    #[test]
    fn adc_prescaler() {
        // The osillyscope's divisors and the CFGR0 values it writes for them
        let mut c = Ch32::new(Chip::V003, Box::new(|_| {}), 0);
        for (div, bits) in [
            (2, 0),
            (4, 0x4000),
            (6, 0x8000),
            (8, 0xc000),
            (12, 0xa000),
            (16, 0xe000),
            (24, 0xa800),
            (32, 0xe800),
            (64, 0xf000),
            (96, 0xb800),
            (128, 0xf800),
        ] {
            c.write(RCC_BASE + 4, 4, bits, 0);
            assert_eq!(c.adc_div(), div, "CFGR0 {bits:#x}");
        }
    }

    /// 48 MHz, ADC1 converting channel 5 continuously (9-cycle sample: 20
    /// cycles of 24 MHz, 833 ns) into an 8-item circular DMA buffer.
    fn converting(input: u16) -> Ch32 {
        let mut c = Ch32::new(Chip::V003, Box::new(|_| {}), 0);
        c.write(RCC_BASE + 4, 4, 2, 0);
        c.adc_in[5] = input;
        c.write(ADC1_BASE + 0x34, 4, 5, 0);
        c.write(ADC1_BASE + 0x10, 4, 1 << 15, 0);
        c.write(DMA1_BASE + DMA_CH1 + 4, 4, 8, 0);
        c.write(DMA1_BASE + DMA_CH1 + 0xc, 4, SRAM_BASE, 0);
        let cfgr = DMA_MINC | 1 << 8 | 1 << 10 | DMA_CIRC | DMA_HTIE | DMA_TCIE | DMA_EN;
        c.write(DMA1_BASE + DMA_CH1, 4, cfgr as u64, 0);
        c.write(ADC1_BASE + 8, 4, (ADC_ADON | ADC_CONT | ADC_DMA) as u64, 0);
        c.write(ADC1_BASE + 8, 4, (ADC_ADON | ADC_CONT | ADC_DMA | ADC_SWSTART) as u64, 0);
        c
    }

    #[test]
    fn continuous_conversions_go_to_memory() {
        let mut c = converting(300);
        // Half the buffer by the 4th conversion's end, all of it by the 8th's
        assert_eq!(c.next_event_ns(), 3334);
        c.update(3333);
        assert_eq!(c.dma_intfr & DMA_HTIF1, 0);
        c.update(3334);
        assert_ne!(c.dma_intfr & DMA_HTIF1, 0);
        assert_eq!(c.next_event_ns(), 6667);
        c.update(6667);
        assert_ne!(c.dma_intfr & DMA_TCIF1, 0);
        let writes = c.take_dma_writes().unwrap();
        assert_eq!(writes.len(), 8);
        assert_eq!(writes[7], (SRAM_BASE as u32 + 14, 300, 2));
        // Circular: on again from the start
        assert_eq!(c.read(DMA1_BASE + DMA_CH1 + 4, 4, 6667), Some(8));
        assert_eq!(c.pending_irq(), None);
        c.write(PFIC_BASE + 0x100, 4, 1 << 22, 6667);
        assert_eq!(c.pending_irq(), Some(22));
    }

    #[test]
    fn analog_watchdog_trips_at_the_crossing() {
        let mut c = converting(300);
        c.write(DMA1_BASE + DMA_CH1, 4, 0, 0);
        // A square wave around 300, crossing 400 a quarter of the way into
        // each 10 us; a window of 0 to 400
        c.adc_wave = Some(Wave { ch: 5, shape: 2, mhz: 100_000_000, mid: 300.0, amp: 200.0 });
        c.adc_input_changed();
        c.write(ADC1_BASE + 0x24, 4, 400, 0);
        c.write(ADC1_BASE + 0x28, 4, 0, 0);
        c.write(ADC1_BASE + 4, 4, (ADC_AWDSGL | ADC_AWDEN | ADC_AWDIE | 5) as u64, 0);
        c.update(0);
        // The first conversion ends at 833 ns, in the wave's high half
        assert_eq!(c.next_event_ns(), 834);
        c.update(834);
        assert!(c.adc.awd);
        // Cleared while still high: trips again at the next conversion
        c.write(ADC1_BASE, 4, !ADC_AWD as u64, 900);
        c.update(900);
        assert_eq!(c.next_event_ns(), 1667);
        // A window it stays inside: no trip ahead, only a look further on
        c.write(ADC1_BASE + 0x24, 4, 1023, 1000);
        c.update(1000);
        assert_eq!(c.adc.awd_next, u64::MAX);
        assert_eq!(c.next_event_ns(), c.adc_time(c.adc.done + 1 + AWD_HORIZON));
    }

    #[test]
    fn bit_banged_i2c_reaches_the_display() {
        let mut c = Ch32::new(Chip::V003, Box::new(|_| {}), 0);
        c.attach_bitbang_i2c((0, 1), (0, 2));
        let outdr = GPIO_BASE + 0x0c;
        let indr = GPIO_BASE + 0x08;
        let mut lines = 0b110u64;
        let mut set = |c: &mut Ch32, scl: bool, sda: bool| {
            lines = (scl as u64) << 1 | (sda as u64) << 2;
            c.write(outdr, 4, lines, 0);
        };
        // START, then bytes, each acknowledged (SDA read low on the 9th clock)
        set(&mut c, true, true);
        set(&mut c, true, false);
        set(&mut c, false, false);
        for byte in [(ssd1306::ADDR << 1), 0x40, 0xa5] {
            for i in (0..8).rev() {
                let bit = byte >> i & 1 != 0;
                set(&mut c, false, bit);
                set(&mut c, true, bit);
                set(&mut c, false, bit);
            }
            set(&mut c, false, true);
            set(&mut c, true, true);
            assert_eq!(c.read(indr, 4, 0).unwrap() >> 2 & 1, 0, "byte {byte:#x} acknowledged");
            set(&mut c, false, true);
        }
        // STOP
        set(&mut c, false, false);
        set(&mut c, true, false);
        set(&mut c, true, true);
        assert_eq!(c.display.ram[0], 0xa5);
    }
}
