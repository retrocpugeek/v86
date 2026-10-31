//! Browser build: exports used by src/rv64_cpu.js.
//!
//! RAM, the CLINT, PLIC, UART and RTC live here; the virtio-mmio slots are
//! implemented in JS (v86's virtio devices) and reached through the
//! `rv64_mmio_*` imports. JS devices signal interrupts by writing IRQ_LINES
//! (see rv64_irq_lines_ptr) rather than calling back in, since they may run
//! inside an MMIO access while rv64_run is on the stack.

use std::ptr::{addr_of, addr_of_mut};

use crate::boot;
use crate::bus::{Bus, DevCtx, Device, Plic, RAM_BASE};
use crate::cpu::{Cpu, RunExit};
use crate::dev::rtc::Rtc;
use crate::dev::uart::Uart;
use crate::gdb::Gdb;

#[link(wasm_import_module = "env")]
extern "C" {
    fn rv64_mmio_read(addr: u32, size: u32) -> u32;
    fn rv64_mmio_write(addr: u32, size: u32, value: u32);
    /// A byte from the UART (serial console).
    fn rv64_serial_out(byte: u32);
    /// Compile a JIT module (see jit::CompileFn).
    fn rv64_jit_compile(ptr: u32, len: u32, table_index: u32, cookie: u32) -> u32;
}

/// The virtio-mmio slots, handed to JS.
pub const VIRTIO_BASE: u64 = 0x1000_1000;
pub const VIRTIO_SLOTS: u64 = 8;
pub const VIRTIO_SLOT_SIZE: u64 = 0x1000;

const NUM_IRQ_LINES: usize = 64;
/// PLIC source levels written by JS, plus a dirty flag at the end.
static mut IRQ_LINES: [u8; NUM_IRQ_LINES + 1] = [0; NUM_IRQ_LINES + 1];
static mut CPU: Option<Box<Cpu>> = None;
/// The debugger attached to a microcontroller (rv64_gdb_*), whose bytes the
/// page relays from gdb in another machine.
static mut GDB: Option<Gdb> = None;
static mut GDB_IN: [u8; 4096] = [0; 4096];
static mut UART_RX: Option<std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<u8>>>> = None;

fn apply_irq_lines(plic: &mut Plic) {
    // Safety: single-threaded; JS only writes these bytes between our reads.
    unsafe {
        let lines = &mut *addr_of_mut!(IRQ_LINES);
        if lines[NUM_IRQ_LINES] == 0 {
            return;
        }
        lines[NUM_IRQ_LINES] = 0;
        for (n, &l) in lines[..NUM_IRQ_LINES].iter().enumerate().skip(1) {
            if plic.level[n] != (l != 0) {
                plic.set_irq(n as u32, l != 0);
            }
        }
    }
}

struct JsMmio;

impl Device for JsMmio {
    fn name(&self) -> &str { "js" }

    fn read(&mut self, off: u64, size: u32, ctx: &mut DevCtx) -> u64 {
        let addr = (VIRTIO_BASE + off) as u32;
        let v = unsafe {
            if size == 8 {
                rv64_mmio_read(addr, 4) as u64 | (rv64_mmio_read(addr + 4, 4) as u64) << 32
            } else {
                rv64_mmio_read(addr, size) as u64
            }
        };
        apply_irq_lines(ctx.plic);
        v
    }

    fn write(&mut self, off: u64, size: u32, value: u64, ctx: &mut DevCtx) {
        let addr = (VIRTIO_BASE + off) as u32;
        unsafe {
            if size == 8 {
                rv64_mmio_write(addr, 4, value as u32);
                rv64_mmio_write(addr + 4, 4, (value >> 32) as u32);
            } else {
                rv64_mmio_write(addr, size, value as u32);
            }
        }
        apply_irq_lines(ctx.plic);
    }
}

#[allow(static_mut_refs)]
fn cpu() -> &'static mut Cpu { unsafe { CPU.as_deref_mut().expect("rv64_init not called") } }

/// Create a machine with `ram_mb` MB of RAM at 0x8000_0000 and the RTC set to
/// `epoch_secs`. Returns the address of RAM in linear memory.
#[no_mangle]
pub extern "C" fn rv64_init(ram_mb: u32, epoch_secs: f64) -> u32 {
    unsafe {
        *addr_of_mut!(CPU) = None;
        *addr_of_mut!(IRQ_LINES) = [0; NUM_IRQ_LINES + 1];
    }
    let mut bus = Bus::new((ram_mb as usize) << 20);
    bus.log_unmapped = false;
    let uart = Uart::new(10, Box::new(|b| unsafe { rv64_serial_out(b as u32) }));
    unsafe { *addr_of_mut!(UART_RX) = Some(uart.rx.clone()) };
    bus.map(0x1000_0000, 0x100, Box::new(uart));
    bus.map(0x10_1000, 0x1000, Box::new(Rtc::new(epoch_secs as u64)));
    bus.map(VIRTIO_BASE, VIRTIO_SLOTS * VIRTIO_SLOT_SIZE, Box::new(JsMmio));
    let mut cpu = Box::new(Cpu::new(bus));
    cpu.realtime = true;
    cpu.ns_per_insn = 1;
    let ptr = cpu.bus.ram.as_ptr() as u32;
    unsafe { *addr_of_mut!(CPU) = Some(cpu) };
    ptr
}

/// RV32 (riscv32 machine) instead of RV64; before rv64_boot.
#[no_mangle]
pub extern "C" fn rv64_set_xlen32(on: u32) { cpu().xlen32 = on != 0 }

/// Create a WCH CH32V microcontroller (dev/ch32.rs; `chip` 0: CH32V003,
/// 1: CH32V307) with `firmware` (`len` bytes at `ptr`) in its flash, instead
/// of the virt machine, and its RTC at `epoch_secs`; it starts at once.
/// USART1 is the console (rv64_serial_out, rv64_serial_in). Returns 0, or -1
/// if the firmware doesn't fit.
#[no_mangle]
pub extern "C" fn rv64_init_ch32(chip: u32, ptr: u32, len: u32, epoch_secs: f64) -> i32 {
    // A reset with a debugger attached keeps it, and its breakpoints
    #[allow(static_mut_refs)]
    let breakpoints = unsafe { CPU.as_deref_mut() }.map(|c| std::mem::take(&mut c.breakpoints)).unwrap_or_default();
    unsafe {
        *addr_of_mut!(CPU) = None;
        *addr_of_mut!(IRQ_LINES) = [0; NUM_IRQ_LINES + 1];
    }
    // (and, as a reset does, the flash's contents: what the firmware saved
    // there, e.g. high scores, stays)
    #[allow(static_mut_refs)]
    let old_flash = unsafe { CPU.as_deref_mut() }.filter(|c| c.bus.ch32.is_some()).map(|c| std::mem::take(&mut c.bus.rom));
    let fw = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = Box::new(|b: u8| unsafe { rv64_serial_out(b as u32) });
    let chip = if chip == 1 { crate::dev::ch32::Chip::V307 } else { crate::dev::ch32::Chip::V003 };
    let Ok(mut cpu) = crate::dev::ch32::machine(chip, fw, out, epoch_secs as u64) else { return -1 };
    cpu.bus.log_unmapped = false;
    cpu.realtime = true;
    unsafe { *addr_of_mut!(UART_RX) = Some(cpu.bus.ch32.as_ref().unwrap().rx.clone()) };
    if let Some(flash) = old_flash.filter(|f| f.len() == cpu.bus.rom.len()) {
        cpu.bus.rom = flash;
    }
    #[allow(static_mut_refs)]
    if let Some([cs, dc, rst]) = unsafe { LCD_PINS } {
        cpu.bus.ch32.as_mut().unwrap().attach_lcd(cs, dc, rst);
    }
    #[allow(static_mut_refs)]
    if let Some([scl, sda]) = unsafe { I2C_PINS } {
        cpu.bus.ch32.as_mut().unwrap().attach_bitbang_i2c(scl, sda);
    }
    #[allow(static_mut_refs)]
    {
        let c = cpu.bus.ch32.as_mut().unwrap();
        c.adc_wave = unsafe { ADC_WAVE };
        c.adc_stream = unsafe { ADC_STREAM }.map(|(ch, rate)| crate::dev::ch32::Stream::new(ch, rate));
    }
    if let Some(g) = gdb() {
        g.reattach(&mut cpu, breakpoints);
    }
    unsafe { *addr_of_mut!(CPU) = Some(Box::new(cpu)) };
    0
}

// ---- the GDB stub (src/gdb.rs), for a microcontroller ----

#[allow(static_mut_refs)]
fn gdb() -> Option<&'static mut Gdb> { unsafe { GDB.as_mut() } }

/// A debugger connects: the hart stops. Returns 0, or -1 if this isn't a
/// microcontroller.
#[no_mangle]
pub extern "C" fn rv64_gdb_attach() -> i32 {
    let Some(chip) = ch32().map(|c| c.chip) else { return -1 };
    let v003 = chip == crate::dev::ch32::Chip::V003;
    let g = Gdb::attach(cpu(), v003, !v003);
    unsafe { *addr_of_mut!(GDB) = Some(g) };
    0
}

/// The debugger went away: the hart runs on undisturbed.
#[no_mangle]
pub extern "C" fn rv64_gdb_detach() {
    #[allow(static_mut_refs)]
    if let Some(mut g) = unsafe { GDB.take() } {
        g.detach(cpu());
    }
}

/// The debugger has the hart stopped: rv64_run would do nothing, and time
/// stands still.
#[no_mangle]
pub extern "C" fn rv64_gdb_halted() -> u32 { gdb().is_some_and(|g| g.mode == crate::gdb::Mode::Halted && g.reset.is_none()) as u32 }

/// Where JS puts gdb's bytes for rv64_gdb_input (4096 of them).
#[no_mangle]
pub extern "C" fn rv64_gdb_in_ptr() -> u32 { addr_of!(GDB_IN) as u32 }

/// `len` bytes from gdb, in rv64_gdb_in_ptr's buffer.
#[no_mangle]
pub extern "C" fn rv64_gdb_input(len: u32) {
    let data = unsafe { &(&*addr_of!(GDB_IN))[..(len as usize).min(4096)] };
    if let Some(g) = gdb() {
        g.input(cpu(), data);
    }
}

/// The bytes for gdb (rv64_gdb_out_len of them), until rv64_gdb_out_clear.
#[no_mangle]
pub extern "C" fn rv64_gdb_out_ptr() -> u32 { gdb().map_or(0, |g| g.out.as_ptr() as u32) }

#[no_mangle]
pub extern "C" fn rv64_gdb_out_len() -> u32 { gdb().map_or(0, |g| g.out.len() as u32) }

#[no_mangle]
pub extern "C" fn rv64_gdb_out_clear() {
    if let Some(g) = gdb() {
        g.out.clear();
    }
}

/// A CH32V GPIO port's output register (0: port A, 1: B, ...), or 0.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_gpio_out(port: u32) -> u32 {
    // (also before the machine exists: the page may ask early)
    unsafe { CPU.as_deref() }.and_then(|c| c.bus.ch32.as_ref()).map_or(0, |c| c.gpio_out(port))
}

fn ch32() -> Option<&'static mut crate::dev::ch32::Ch32> {
    #[allow(static_mut_refs)]
    unsafe { CPU.as_deref_mut() }.and_then(|c| c.bus.ch32.as_deref_mut())
}

static mut OLED_PIXELS: [u8; crate::dev::ssd1306::WIDTH * crate::dev::ssd1306::HEIGHT] =
    [0; crate::dev::ssd1306::WIDTH * crate::dev::ssd1306::HEIGHT];
static mut EDGES: Vec<f64> = Vec::new();
static mut TONES: Vec<f64> = Vec::new();
/// The ST7735 LCD's pins (port, pin), kept for machines made later (resets)
static mut LCD_PINS: Option<[(usize, u32); 3]> = None;
/// Likewise the bit-banged I2C bus's pins (SCL, SDA), an ADC input's
/// waveform, and the channel and rate of samples streamed into one
static mut I2C_PINS: Option<[(usize, u32); 2]> = None;
static mut ADC_WAVE: Option<crate::dev::ch32::Wave> = None;
static mut ADC_STREAM: Option<(usize, u32)> = None;
/// Samples for rv64_ch32_adc_stream_push
static mut STREAM_BUF: Vec<u16> = Vec::new();
static mut LCD_RGB: [u32; crate::dev::st7735::VIEW_WIDTH * crate::dev::st7735::VIEW_HEIGHT] =
    [0; crate::dev::st7735::VIEW_WIDTH * crate::dev::st7735::VIEW_HEIGHT];
static mut LCD_PIXELS: [u8; crate::dev::st7735::VIEW_WIDTH * crate::dev::st7735::VIEW_HEIGHT * 4] =
    [0; crate::dev::st7735::VIEW_WIDTH * crate::dev::st7735::VIEW_HEIGHT * 4];

/// CH32V: the OLED display's frame counter (changes when what it shows may
/// have), or 0 if there's no machine.
#[no_mangle]
pub extern "C" fn rv64_ch32_oled_frame() -> u32 { ch32().map_or(0, |c| c.display.frame | 1) }

/// CH32V: render the OLED display; returns where its 128 x 64 pixels are
/// (a byte each, 0 or 1, row by row).
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_oled_render() -> u32 {
    let px = unsafe { &mut *addr_of_mut!(OLED_PIXELS) };
    if let Some(c) = ch32() {
        c.display.render(px);
    }
    px.as_ptr() as u32
}

/// CH32V: the OLED display's contrast setting (0 to 255).
#[no_mangle]
pub extern "C" fn rv64_ch32_oled_contrast() -> u32 { ch32().map_or(0, |c| c.display.contrast as u32) }

/// CH32V: put an ST7735 LCD on SPI1, with its chip select, data/command and
/// reset on these pins (port * 16 + pin; port 0 is GPIOA).
#[no_mangle]
pub extern "C" fn rv64_ch32_attach_lcd(cs: u32, dc: u32, rst: u32) {
    let pin = |p: u32| ((p / 16) as usize, p % 16);
    let pins = [pin(cs), pin(dc), pin(rst)];
    unsafe { *addr_of_mut!(LCD_PINS) = Some(pins) };
    if let Some(c) = ch32() {
        c.attach_lcd(pins[0], pins[1], pins[2]);
    }
}

/// CH32V: put the SSD1306 OLED display on an I2C bus the firmware
/// bit-bangs on these pins (port * 16 + pin; port 0 is GPIOA).
#[no_mangle]
pub extern "C" fn rv64_ch32_attach_i2c_gpio(scl: u32, sda: u32) {
    let pin = |p: u32| ((p / 16) as usize, p % 16);
    unsafe { *addr_of_mut!(I2C_PINS) = Some([pin(scl), pin(sda)]) };
    if let Some(c) = ch32() {
        c.attach_bitbang_i2c(pin(scl), pin(sda));
    }
}

/// CH32V: make ADC channel `ch`'s input a waveform: `shape` 1 sine, 2
/// square, 3 triangle, 4 sawtooth (0: none, the level set instead), at `hz`,
/// around `mid` with amplitude `amp` (ADC codes, 0 to 1023).
#[no_mangle]
pub extern "C" fn rv64_ch32_set_adc_wave(ch: u32, shape: u32, hz: f64, mid: f64, amp: f64) {
    let wave = (shape != 0).then_some(crate::dev::ch32::Wave {
        ch: ch as usize,
        shape,
        mhz: (hz.max(0.0) * 1000.0) as u64,
        mid,
        amp,
    });
    unsafe { *addr_of_mut!(ADC_WAVE) = wave };
    if let Some(c) = ch32() {
        c.adc_wave = wave;
        c.adc_input_changed();
    }
}

/// CH32V: stream samples (rv64_ch32_adc_stream_push) into ADC channel
/// `ch` at `rate` Hz, e.g. a microphone's; rate 0 stops.
#[no_mangle]
pub extern "C" fn rv64_ch32_adc_stream(ch: u32, rate: u32) {
    let s = (rate != 0).then_some((ch as usize, rate));
    unsafe { *addr_of_mut!(ADC_STREAM) = s };
    if let Some(c) = ch32() {
        c.adc_stream = s.map(|(ch, rate)| crate::dev::ch32::Stream::new(ch, rate));
        c.adc_input_changed();
    }
}

/// CH32V: room for `n` samples (u16 ADC codes) to stream; returns where.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_adc_stream_buf(n: u32) -> u32 {
    let buf = unsafe { &mut *addr_of_mut!(STREAM_BUF) };
    buf.resize(n as usize, 0);
    buf.as_ptr() as u32
}

/// CH32V: stream the first `n` samples written at rv64_ch32_adc_stream_buf.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_adc_stream_push(n: u32) {
    let buf = unsafe { &*addr_of!(STREAM_BUF) };
    if let Some(c) = ch32() {
        if let Some(s) = c.adc_stream.as_mut() {
            s.push(&buf[..(n as usize).min(buf.len())]);
            c.adc_input_changed();
        }
    }
}

/// CH32V: the LCD's frame counter (changes when what it shows may have), or
/// 0 if there's none.
#[no_mangle]
pub extern "C" fn rv64_ch32_lcd_frame() -> u32 { ch32().and_then(|c| c.lcd.as_ref()).map_or(0, |l| l.frame | 1) }

/// CH32V: render the LCD; returns where its pixels are (RGBA, row by row,
/// st7735::VIEW_WIDTH x VIEW_HEIGHT).
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_lcd_render() -> u32 {
    let px = unsafe { &mut *addr_of_mut!(LCD_PIXELS) };
    let rgb = unsafe { &mut *addr_of_mut!(LCD_RGB) };
    if let Some(lcd) = ch32().and_then(|c| c.lcd.as_ref()) {
        lcd.render(rgb);
        for (o, &v) in px.chunks_mut(4).zip(rgb.iter()) {
            o.copy_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]);
        }
    }
    px.as_ptr() as u32
}

/// CH32V: hand over TIM2's tone changes; returns how many, and
/// rv64_ch32_tones_ptr() where they are: (virtual time in ms, Hz) as f64s.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_take_tones() -> u32 {
    let tones = unsafe { &mut *addr_of_mut!(TONES) };
    tones.clear();
    if let Some(c) = ch32() {
        for (t, hz) in c.take_tones() {
            tones.extend([t as f64 / 1e6, hz as f64]);
        }
    }
    (tones.len() / 2) as u32
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_tones_ptr() -> u32 { unsafe { (*addr_of!(TONES)).as_ptr() as u32 } }

/// CH32V: where the flash's contents are (for the host to keep what the
/// firmware saves there), or 0; rv64_ch32_flash_len() bytes.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_flash_ptr() -> u32 {
    unsafe { CPU.as_deref() }.filter(|c| c.bus.ch32.is_some()).map_or(0, |c| c.bus.rom.as_ptr() as u32)
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_flash_len() -> u32 {
    unsafe { CPU.as_deref() }.filter(|c| c.bus.ch32.is_some()).map_or(0, |c| c.bus.rom.len() as u32)
}

/// CH32V: drive the pins of GPIO `port` in `mask` to `levels`.
#[no_mangle]
pub extern "C" fn rv64_ch32_set_gpio_in(port: u32, mask: u32, levels: u32) {
    if let Some(p) = ch32().and_then(|c| c.gpio_in.get_mut(port as usize)) {
        *p = (mask, levels);
    }
}

/// CH32V: set ADC channel `ch`'s input (0 to 1023).
#[no_mangle]
pub extern "C" fn rv64_ch32_set_adc(ch: u32, value: u32) {
    if let Some(v) = ch32().and_then(|c| c.adc_in.get_mut(ch as usize)) {
        *v = value as u16;
    }
}

/// CH32V: record changes of the output pins of GPIO `port` in `mask`.
#[no_mangle]
pub extern "C" fn rv64_ch32_watch_gpio(port: u32, mask: u32) {
    if let Some(w) = ch32().and_then(|c| c.gpio_watch.get_mut(port as usize)) {
        *w = mask;
    }
}

/// CH32V: hand over the recorded pin changes; returns how many, and
/// rv64_ch32_edges_ptr() where they are: (virtual time in ms, port, OUTDR)
/// as f64s.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_take_edges() -> u32 {
    let edges = unsafe { &mut *addr_of_mut!(EDGES) };
    edges.clear();
    if let Some(c) = ch32() {
        for (t, port, v) in c.take_edges() {
            edges.extend([t as f64 / 1e6, port as f64, v as f64]);
        }
    }
    (edges.len() / 3) as u32
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_ch32_edges_ptr() -> u32 { unsafe { (*addr_of!(EDGES)).as_ptr() as u32 } }

/// Virtual time now, in ms.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_now_ms() -> f64 { unsafe { CPU.as_deref() }.map_or(0.0, |c| c.now_ns() as f64 / 1e6) }

/// How far virtual time is ahead of the host clock (ms, as for
/// rv64_sync_time): a machine timed like real hardware (CH32V) waits that out.
#[no_mangle]
pub extern "C" fn rv64_ms_ahead(host_ms: f64) -> f64 { cpu().ns_ahead((host_ms * 1e6) as u64) as f64 / 1e6 }

/// Where the machine's RAM is in linear memory.
#[no_mangle]
pub extern "C" fn rv64_ram_ptr() -> u32 { cpu().bus.ram.as_ptr() as u32 }

#[no_mangle]
pub extern "C" fn rv64_irq_lines_ptr() -> u32 { addr_of!(IRQ_LINES) as u32 }

/// Scratch memory for passing images in.
#[no_mangle]
pub extern "C" fn rv64_alloc(len: u32) -> u32 {
    let mut v = vec![0u8; len as usize];
    let p = v.as_mut_ptr() as u32;
    std::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn rv64_free(ptr: u32, len: u32) {
    // Safety: allocated by rv64_alloc with this length
    unsafe { drop(Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize)) }
}

/// Where boot puts a DTB of `dtb_len` bytes (for the device tree's memory map).
#[no_mangle]
pub extern "C" fn rv64_dtb_addr(dtb_len: u32) -> f64 { boot::dtb_addr(cpu().bus.ram.len() as u64, dtb_len as u64) as f64 }

/// Load the firmware (OpenSBI fw_dynamic), the kernel and the DTB; see boot::load.
#[no_mangle]
pub extern "C" fn rv64_boot(fw: u32, fw_len: u32, kernel: u32, kernel_len: u32, dtb: u32, dtb_len: u32) -> i32 {
    let s = |p: u32, l: u32| unsafe { std::slice::from_raw_parts(p as *const u8, l as usize) };
    match boot::load(cpu(), s(fw, fw_len), s(kernel, kernel_len), s(dtb, dtb_len)) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// A byte typed on the serial console.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_serial_in(byte: u32) {
    if let Some(rx) = unsafe { UART_RX.as_ref() } {
        rx.lock().unwrap().push_back(byte as u8);
    }
    let cpu = cpu();
    cpu.bus.now_ns = cpu.now_ns();
    cpu.bus.poll_devices();
    cpu.update_mip();
}

/// Run up to `budget` steps. Returns 0 = budget used, 1 = idle (waiting for an
/// interrupt), 2 = powered off, 3 = reset requested, 4 = stopped for the
/// debugger (rv64_gdb_*; it has bytes for gdb).
#[no_mangle]
pub extern "C" fn rv64_run(budget: u32) -> u32 {
    let cpu = cpu();
    apply_irq_lines(&mut cpu.bus.plic);
    cpu.update_mip();
    let r = match gdb() {
        // `monitor reset`: JS rebuilds the machine (rv64_init_ch32)
        Some(g) if g.reset.is_some() => return 3,
        // (a stop is RunExit::Debug)
        Some(g) => match g.poll(cpu, budget as u64) {
            None => return 4,
            Some(r) => r,
        },
        None => cpu.run(budget as u64),
    };
    // Input that arrived while running, and devices' pending work
    cpu.bus.now_ns = cpu.now_ns();
    cpu.bus.poll_devices();
    cpu.update_mip();
    match r {
        RunExit::Budget => 0,
        RunExit::Idle => 1,
        RunExit::PowerOff => 2,
        RunExit::Reset => 3,
        RunExit::Debug => 4,
    }
}

/// Pull virtual time up to the host clock (milliseconds, e.g. performance.now()).
#[no_mangle]
pub extern "C" fn rv64_sync_time(host_ms: f64) { cpu().sync_host_ns((host_ms * 1e6) as u64) }

/// Milliseconds until the next timer interrupt, or -1 if none is armed.
#[no_mangle]
pub extern "C" fn rv64_ms_until_timer() -> f64 { cpu().ns_until_timer().map_or(-1.0, |ns| ns as f64 / 1e6) }

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn rv64_icount() -> f64 { unsafe { CPU.as_deref() }.map_or(0.0, |c| c.icount as f64) }

#[no_mangle]
pub extern "C" fn rv64_pc() -> f64 { cpu().pc as f64 }

/// RAM base, for JS's guest-physical accessors.
#[no_mangle]
pub extern "C" fn rv64_ram_base() -> f64 { RAM_BASE as f64 }

// ---- JIT (src/jit.rs) ----

fn host_compile(code: &[u8], table_index: u32, cookie: u32) -> u32 {
    unsafe { rv64_jit_compile(code.as_ptr() as u32, code.len() as u32, table_index, cookie) }
}

/// Turn the JIT on. Compiled regions go into the function table from
/// `table_base` on; threshold 0 keeps the default.
#[no_mangle]
pub extern "C" fn rv64_jit_enable(table_base: u32, threshold: u32) {
    let c = cpu();
    c.jit.enabled = true;
    c.jit.table_base = table_base;
    c.jit.compile = Some(host_compile);
    if threshold != 0 {
        c.jit.threshold = threshold;
    }
}

/// Address of Bus::code_pages (one byte per RAM page; see jit.rs), for the
/// JS write functions.
#[no_mangle]
pub extern "C" fn rv64_jit_code_pages() -> u32 { cpu().jit_tables().0 as u32 }

/// JS wrote RAM offsets off..off + len, which include a page with compiled code.
#[no_mangle]
pub extern "C" fn rv64_jit_written(off: u32, len: u32) { cpu().bus.written(off as usize, len as usize) }

#[no_mangle]
pub extern "C" fn rv64_jit_done(cookie: u32, ok: u32) { cpu().jit_done(cookie, ok != 0) }

/// Called by compiled code.
#[no_mangle]
pub extern "C" fn rv64_jit_rd(cpu: *mut Cpu, va: u64, size: u32) -> u64 { unsafe { (*cpu).jit_read(va, size) } }

#[no_mangle]
pub extern "C" fn rv64_jit_wr(cpu: *mut Cpu, va: u64, size: u32, v: u64) { unsafe { (*cpu).jit_write(va, size, v) } }

/// JIT counters: 0 = regions compiled, 1 = calls to compiled code, 2 =
/// regions dropped because their page was written, 3 = failed compilations,
/// 4 = instructions run compiled, 5 = loads and stores of compiled code that
/// took the slow path.
#[no_mangle]
pub extern "C" fn rv64_jit_stat(n: u32) -> f64 {
    let s = cpu().jit.stats;
    [s.compiled, s.entered, s.stale, s.failed, s.insns, s.slow].get(n as usize).copied().unwrap_or(0) as f64
}

static mut SELFTEST_MSG: String = String::new();

/// Run jit::selftest (rv64_jit_compile must install synchronously). Returns
/// 0 if it passed, else the length of a message at rv64_jit_selftest_msg().
#[no_mangle]
pub extern "C" fn rv64_jit_selftest(seed: u32, iterations: u32, table_base: u32) -> u32 {
    let r = crate::jit::selftest(seed as u64 | (seed as u64) << 32, iterations, table_base, host_compile);
    let msg = r.unwrap_or_default();
    let len = msg.len() as u32;
    unsafe { *addr_of_mut!(SELFTEST_MSG) = msg };
    len
}

#[no_mangle]
pub extern "C" fn rv64_jit_selftest_msg() -> u32 { unsafe { (*addr_of!(SELFTEST_MSG)).as_ptr() as u32 } }
