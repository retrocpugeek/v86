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
