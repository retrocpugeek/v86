//! Browser build: exports used by src/a64_cpu.js.
//!
//! RAM and the GIC live here; everything else in device space (PL011, PL031,
//! virtio-mmio) is implemented in JS and reached through the `a64_mmio_*`
//! imports. JS devices signal interrupts by writing IRQ_LINES (see
//! a64_irq_lines_ptr) instead of calling back in, since they may run inside
//! an MMIO access while a64_run is on the stack.

use std::ptr::{addr_of, addr_of_mut};

use crate::boot;
use crate::bus::{Bus, DevCtx, Device};
use crate::cpu::{Cpu, RunExit};

#[link(wasm_import_module = "env")]
extern "C" {
    fn a64_mmio_read(addr: u32, size: u32) -> u32;
    fn a64_mmio_write(addr: u32, size: u32, value: u32);
    /// Compile a JIT module (see jit::CompileFn).
    fn a64_jit_compile(ptr: u32, len: u32, table_index: u32, cookie: u32) -> u32;
}

/// Device space handed to JS: everything from the PL011 up to the end of the
/// virtio-mmio slots.
const EXT_BASE: u64 = 0x0900_0000;
const EXT_SIZE: u64 = 0x0200_0000;

const NUM_SPI_LINES: usize = 64;
/// SPI levels written by JS, plus a dirty flag at the end.
static mut IRQ_LINES: [u8; NUM_SPI_LINES + 1] = [0; NUM_SPI_LINES + 1];
static mut CPU: Option<Box<Cpu>> = None;

fn apply_irq_lines(gic: &mut crate::gic::Gic) {
    // Safety: single-threaded; JS only writes these bytes between our reads.
    unsafe {
        let lines = &mut *addr_of_mut!(IRQ_LINES);
        if lines[NUM_SPI_LINES] == 0 {
            return;
        }
        lines[NUM_SPI_LINES] = 0;
        for (n, &l) in lines[..NUM_SPI_LINES].iter().enumerate() {
            gic.set_spi(n as u32, l != 0);
        }
    }
}

struct JsMmio;

impl Device for JsMmio {
    fn name(&self) -> &str { "js" }

    fn read(&mut self, off: u64, size: u32, ctx: &mut DevCtx) -> u64 {
        let addr = (EXT_BASE + off) as u32;
        let v = unsafe {
            if size == 8 {
                a64_mmio_read(addr, 4) as u64 | (a64_mmio_read(addr + 4, 4) as u64) << 32
            } else {
                a64_mmio_read(addr, size) as u64
            }
        };
        apply_irq_lines(ctx.gic);
        v
    }

    fn write(&mut self, off: u64, size: u32, value: u64, ctx: &mut DevCtx) {
        let addr = (EXT_BASE + off) as u32;
        unsafe {
            if size == 8 {
                a64_mmio_write(addr, 4, value as u32);
                a64_mmio_write(addr + 4, 4, (value >> 32) as u32);
            } else {
                a64_mmio_write(addr, size, value as u32);
            }
        }
        apply_irq_lines(ctx.gic);
    }
}

#[allow(static_mut_refs)]
fn cpu() -> &'static mut Cpu { unsafe { CPU.as_deref_mut().expect("a64_init not called") } }

/// Create a machine with `ram_mb` MB of RAM at 0x4000_0000. Returns the
/// address of RAM in linear memory.
#[no_mangle]
pub extern "C" fn a64_init(ram_mb: u32) -> u32 {
    unsafe {
        *addr_of_mut!(CPU) = None;
        *addr_of_mut!(IRQ_LINES) = [0; NUM_SPI_LINES + 1];
    }
    let mut bus = Bus::new(boot::RAM_BASE, (ram_mb as usize) << 20);
    bus.log_unmapped = false;
    bus.map(EXT_BASE, EXT_SIZE, Box::new(JsMmio));
    let mut cpu = Box::new(Cpu::new(bus));
    cpu.realtime = true;
    cpu.log_undef = false;
    let ptr = cpu.bus.ram.as_ptr() as u32;
    unsafe { *addr_of_mut!(CPU) = Some(cpu) };
    ptr
}

#[no_mangle]
pub extern "C" fn a64_irq_lines_ptr() -> u32 { addr_of!(IRQ_LINES) as u32 }

/// Allocate a scratch buffer for passing blobs in; free with a64_free.
#[no_mangle]
pub extern "C" fn a64_alloc(len: u32) -> u32 {
    let mut v = vec![0u8; len as usize];
    let p = v.as_mut_ptr() as u32;
    std::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn a64_free(ptr: u32, len: u32) {
    unsafe { drop(Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize)) }
}

/// Load an arm64 Image and DTB the way QEMU's -kernel does. Returns 0 on success.
#[no_mangle]
pub extern "C" fn a64_boot(image: u32, image_len: u32, dtb: u32, dtb_len: u32) -> i32 {
    let (image, dtb) = unsafe {
        (
            std::slice::from_raw_parts(image as *const u8, image_len as usize),
            std::slice::from_raw_parts(dtb as *const u8, dtb_len as usize),
        )
    };
    match boot::load(cpu(), image, dtb) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Guest physical address of the DTB placed by a64_boot (for a given RAM size).
#[no_mangle]
pub extern "C" fn a64_dtb_addr() -> f64 { boot::dtb_addr(cpu().bus.ram.len() as u64) as f64 }

/// Run up to `budget` steps. Returns 0 = budget used, 1 = idle (waiting for an
/// interrupt), 2 = powered off, 3 = reset requested.
#[no_mangle]
pub extern "C" fn a64_run(budget: u32) -> u32 {
    let cpu = cpu();
    apply_irq_lines(&mut cpu.bus.gic);
    match cpu.run(budget as u64) {
        RunExit::Budget => 0,
        RunExit::Idle => 1,
        RunExit::PowerOff => 2,
        RunExit::Reset => 3,
    }
}

/// Pull virtual time up to the host clock (milliseconds, e.g. performance.now()).
#[no_mangle]
pub extern "C" fn a64_sync_time(host_ms: f64) { cpu().sync_host_ns((host_ms * 1e6) as u64) }

/// Milliseconds until the next timer interrupt, or -1 if none is armed.
#[no_mangle]
pub extern "C" fn a64_ms_until_timer() -> f64 { cpu().ns_until_timer().map_or(-1.0, |ns| ns as f64 / 1e6) }

#[no_mangle]
pub extern "C" fn a64_icount() -> f64 { cpu().icount as f64 }

#[no_mangle]
pub extern "C" fn a64_pc() -> f64 { cpu().pc as f64 }

// ---- JIT (src/jit.rs) ----

fn host_compile(code: &[u8], table_index: u32, cookie: u32) -> u32 {
    unsafe { a64_jit_compile(code.as_ptr() as u32, code.len() as u32, table_index, cookie) }
}

/// Turn the JIT on. Compiled blocks go into the function table from
/// `table_base` up; `threshold` (0 = default) is the block entries before a
/// block is compiled.
#[no_mangle]
pub extern "C" fn a64_jit_enable(table_base: u32, threshold: u32) {
    let c = cpu();
    c.jit.enabled = true;
    c.jit.table_base = table_base;
    c.jit.compile = Some(host_compile);
    c.tlb.invalidate_fetch();
    if threshold != 0 {
        c.jit.threshold = threshold;
    }
}

/// The host finished compiling the module for `cookie` (ok = 1) or failed.
/// Address of Bus::code_pages (one byte per RAM page; see jit.rs), for the
/// JS write functions.
#[no_mangle]
pub extern "C" fn a64_jit_code_pages() -> u32 { cpu().jit_tables().0 as u32 }

/// JS wrote to RAM offsets off..off + len (of a page flagged in code_pages).
#[no_mangle]
pub extern "C" fn a64_jit_written(off: u32, len: u32) { cpu().bus.written(off as usize, len as usize) }

#[no_mangle]
pub extern "C" fn a64_jit_done(cookie: u32, ok: u32) { cpu().jit_done(cookie, ok != 0) }

/// Load and store helpers called by compiled code ("e"."r" and "e"."w").
#[no_mangle]
pub extern "C" fn a64_jit_rd(cpu: *mut Cpu, va: u64, size: u32) -> u64 { unsafe { (*cpu).jit_read(va, size) } }

#[no_mangle]
pub extern "C" fn a64_jit_wr(cpu: *mut Cpu, va: u64, size: u32, v: u64) { unsafe { (*cpu).jit_write(va, size, v) } }

/// JIT counters: 0 = regions compiled, 1 = calls to compiled code, 2 =
/// dropped because the code changed, 3 = failed to compile, 4 = instructions
/// run compiled.
#[no_mangle]
pub extern "C" fn a64_jit_stat(n: u32) -> f64 {
    let s = cpu().jit.stats;
    [s.compiled, s.entered, s.stale, s.failed, s.insns].get(n as usize).map_or(0.0, |&v| v as f64)
}

static mut SELFTEST_MSG: String = String::new();

/// Run jit::selftest (a64_jit_compile must install synchronously). Returns
/// 0 if it passed, else the length of a message at a64_jit_selftest_msg().
#[no_mangle]
pub extern "C" fn a64_jit_selftest(seed: u32, iterations: u32, table_base: u32) -> u32 {
    let r = crate::jit::selftest(seed as u64 | (seed as u64) << 32, iterations, table_base, host_compile);
    unsafe {
        let msg = &mut *addr_of_mut!(SELFTEST_MSG);
        *msg = r.unwrap_or_default();
        msg.len() as u32
    }
}

#[no_mangle]
pub extern "C" fn a64_jit_selftest_msg() -> u32 { unsafe { (*addr_of!(SELFTEST_MSG)).as_ptr() as u32 } }
