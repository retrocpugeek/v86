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

/// Create a WCH CH32V003 (dev/ch32.rs) with `firmware` (`len` bytes at
/// `ptr`) in its flash, instead of the virt machine; it starts at once.
/// USART1 is the console (rv64_serial_out, rv64_serial_in). Returns 0, or -1
/// if the firmware doesn't fit.
#[no_mangle]
pub extern "C" fn rv64_init_ch32(ptr: u32, len: u32) -> i32 {
    unsafe {
        *addr_of_mut!(CPU) = None;
        *addr_of_mut!(IRQ_LINES) = [0; NUM_IRQ_LINES + 1];
    }
    let fw = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = Box::new(|b: u8| unsafe { rv64_serial_out(b as u32) });
    let Ok(mut cpu) = crate::dev::ch32::machine(fw, out) else { return -1 };
    cpu.bus.log_unmapped = false;
    cpu.realtime = true;
    unsafe { *addr_of_mut!(UART_RX) = Some(cpu.bus.ch32.as_ref().unwrap().rx.clone()) };
    unsafe { *addr_of_mut!(CPU) = Some(Box::new(cpu)) };
    0
}

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
/// interrupt), 2 = powered off, 3 = reset requested.
#[no_mangle]
pub extern "C" fn rv64_run(budget: u32) -> u32 {
    let cpu = cpu();
    apply_irq_lines(&mut cpu.bus.plic);
    cpu.update_mip();
    let r = cpu.run(budget as u64);
    // Input that arrived while running, and devices' pending work
    cpu.bus.now_ns = cpu.now_ns();
    cpu.bus.poll_devices();
    cpu.update_mip();
    match r {
        RunExit::Budget => 0,
        RunExit::Idle => 1,
        RunExit::PowerOff => 2,
        RunExit::Reset => 3,
    }
}

/// Pull virtual time up to the host clock (milliseconds, e.g. performance.now()).
#[no_mangle]
pub extern "C" fn rv64_sync_time(host_ms: f64) { cpu().sync_host_ns((host_ms * 1e6) as u64) }

/// Milliseconds until the next timer interrupt, or -1 if none is armed.
#[no_mangle]
pub extern "C" fn rv64_ms_until_timer() -> f64 { cpu().ns_until_timer().map_or(-1.0, |ns| ns as f64 / 1e6) }

#[no_mangle]
pub extern "C" fn rv64_icount() -> f64 { cpu().icount as f64 }

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
