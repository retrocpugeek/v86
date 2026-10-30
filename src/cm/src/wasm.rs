//! The browser build's interface (v86-cm.wasm), used by src/cm_cpu.js: one
//! machine (a PebbleOS emulator board), run in slices, its time kept to real
//! time by the host, its display, buttons and UARTs.

use std::ptr::addr_of_mut;

use crate::cpu::{Cpu, Stop};
use crate::dev::pebble::{self, Board, Gpio, Uart, GPIO_BASE, UART_BASE};

static mut CPU: Option<Box<Cpu>> = None;
static mut PIXELS: Vec<u8> = Vec::new();
static mut RGB: Vec<u32> = Vec::new();
static mut OUT: Vec<u8> = Vec::new();

#[allow(static_mut_refs)]
fn cpu() -> &'static mut Cpu { unsafe { CPU.as_deref_mut().expect("cm: no machine") } }

#[no_mangle]
pub extern "C" fn cm_alloc(len: u32) -> u32 {
    let mut v = vec![0u8; len as usize];
    let p = v.as_mut_ptr() as u32;
    std::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn cm_free(ptr: u32, len: u32) {
    unsafe { drop(Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize)) }
}

/// Build the machine: board 0 = flint, 1 = emery, 2 = gabbro; the firmware
/// (an ELF) and the external flash's image; the RTC from `epoch` (UTC
/// seconds). Returns 0, or -1 if the firmware doesn't load.
#[no_mangle]
pub extern "C" fn cm_init(board: u32, elf: u32, elf_len: u32, spi: u32, spi_len: u32, epoch: f64) -> i32 {
    let board = match board {
        1 => Board::Emery,
        2 => Board::Gabbro,
        _ => Board::Flint,
    };
    let elf = unsafe { std::slice::from_raw_parts(elf as *const u8, elf_len as usize) };
    let spi = unsafe { std::slice::from_raw_parts(spi as *const u8, spi_len as usize) };
    match pebble::machine(board, elf, spi, epoch as i64) {
        Ok(mut c) => {
            c.log_unimplemented = false;
            unsafe { *addr_of_mut!(CPU) = Some(Box::new(c)) };
            0
        }
        Err(_) => -1,
    }
}

/// Reset the machine (as its reset button: the flash and external flash
/// keep their contents).
#[no_mangle]
pub extern "C" fn cm_reset() { cpu().reset() }

/// Run up to `budget` instructions: 0 = budget used, 1 = idle (waiting for an
/// interrupt), 2 = halted (a lockup), 3 = reset, 4 = breakpoint.
#[no_mangle]
pub extern "C" fn cm_run(budget: u32) -> u32 {
    let c = cpu();
    let stop = c.run(budget as u64);
    if pebble::watchdog_reset(c) || stop == Stop::Reset {
        c.reset();
        return 3;
    }
    match stop {
        Stop::Budget => 0,
        Stop::Idle => 1,
        Stop::Halted => 2,
        Stop::Reset => 3,
        Stop::Breakpoint => 4,
    }
}

/// The machine's time, in ms.
#[no_mangle]
pub extern "C" fn cm_now_ms() -> f64 { cpu().now_ns() as f64 / 1e6 }

/// When the next device or SysTick event is due (machine ms), or -1.
#[no_mangle]
pub extern "C" fn cm_next_event_ms() -> f64 {
    let n = cpu().bus.next_event_ns();
    if n == u64::MAX {
        -1.0
    } else {
        n as f64 / 1e6
    }
}

/// While idle: move the machine's clock on to `ms` (if it's behind), bring
/// the devices up to it and wake for any interrupt raised.
#[no_mangle]
pub extern "C" fn cm_advance_to_ms(ms: f64) {
    let c = cpu();
    let target = (ms * 1e6) as u64;
    let now = c.now_ns();
    if target > now {
        c.warp_ns += target - now;
    }
    c.bus.update(c.now_ns());
    c.bus.scs.dirty = true;
    c.wake_check();
}

#[no_mangle]
pub extern "C" fn cm_instructions() -> f64 { cpu().icount as f64 }

/// The buttons held: bit 0 back, 1 up, 2 select, 3 down.
#[no_mangle]
pub extern "C" fn cm_set_buttons(state: u32) {
    let c = cpu();
    c.bus.device::<Gpio>(GPIO_BASE).unwrap().set_buttons(state);
    c.bus.sync_all_irqs();
    c.bus.scs.dirty = true;
    c.wake_check();
}

/// Take what UART `n` sent since the last call: returns its length; the
/// bytes are at cm_out_ptr().
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn cm_uart_take(n: u32) -> u32 {
    let out = unsafe { &mut *addr_of_mut!(OUT) };
    out.clear();
    if let Some(u) = UART_BASE.get(n as usize).and_then(|&b| cpu().bus.device::<Uart>(b)) {
        out.append(&mut u.tx);
    }
    out.len() as u32
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn cm_out_ptr() -> u32 { unsafe { (*addr_of_mut!(OUT)).as_ptr() as u32 } }

/// A byte into UART `n`.
#[no_mangle]
pub extern "C" fn cm_uart_input(n: u32, byte: u32) {
    let c = cpu();
    if let Some(u) = UART_BASE.get(n as usize).and_then(|&b| c.bus.device::<Uart>(b)) {
        u.input(&[byte as u8]);
    }
    c.bus.sync_all_irqs();
    c.bus.scs.dirty = true;
    c.wake_check();
}

#[no_mangle]
pub extern "C" fn cm_display_width() -> u32 { pebble::display(cpu()).borrow().width }

#[no_mangle]
pub extern "C" fn cm_display_height() -> u32 { pebble::display(cpu()).borrow().height }

/// Changes when what the display shows may have changed (0: off).
#[no_mangle]
pub extern "C" fn cm_display_frame() -> u32 {
    let d = pebble::display(cpu());
    let d = d.borrow();
    if d.enabled() {
        d.frame | 1
    } else {
        0
    }
}

/// Render the display: returns where its RGBA pixels are (row by row).
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn cm_display_render() -> u32 {
    let d = pebble::display(cpu());
    let d = d.borrow();
    let n = (d.width * d.height) as usize;
    let (rgb, px) = unsafe { (&mut *addr_of_mut!(RGB), &mut *addr_of_mut!(PIXELS)) };
    rgb.resize(n, 0);
    px.resize(n * 4, 0);
    d.render(rgb);
    for (o, &v) in px.chunks_mut(4).zip(rgb.iter()) {
        o.copy_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]);
    }
    px.as_ptr() as u32
}
