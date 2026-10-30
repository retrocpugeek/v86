//! The browser build's interface (v86-cm.wasm), used by src/cm_cpu.js: one
//! machine (a PebbleOS emulator board, or the card10 badge), run in slices,
//! its time kept to real time by the host, its display, buttons and UARTs.

use std::ptr::addr_of_mut;

use crate::cpu::{Cpu, Stop};
use crate::dev::max32665::{self, Card10};
use crate::dev::pebble::{self, Board, Gpio, Touch, Uart, GPIO_BASE, TOUCH_BASE, UART_BASE};

/// The machine: a Pebble board (one core), or the card10 (two)
enum Machine {
    Pebble(Box<Cpu>),
    Card10 { m: Box<Card10>, epicardium: Vec<u8>, pycardium: Vec<u8> },
}

static mut MACHINE: Option<Machine> = None;
static mut PIXELS: Vec<u8> = Vec::new();
static mut RGB: Vec<u32> = Vec::new();
static mut OUT: Vec<u8> = Vec::new();

#[allow(static_mut_refs)]
fn machine() -> &'static mut Machine { unsafe { (*addr_of_mut!(MACHINE)).as_mut().expect("cm: no machine") } }

/// The Pebble's core (Pebble boards only).
fn cpu() -> &'static mut Cpu {
    match machine() {
        Machine::Pebble(c) => c,
        Machine::Card10 { .. } => panic!("cm: not a Pebble"),
    }
}

fn card10() -> Option<&'static mut Card10> {
    match machine() {
        Machine::Card10 { m, .. } => Some(m),
        Machine::Pebble(_) => None,
    }
}

fn slice(p: u32, len: u32) -> &'static [u8] { unsafe { std::slice::from_raw_parts(p as *const u8, len as usize) } }

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
            unsafe { *addr_of_mut!(MACHINE) = Some(Machine::Pebble(Box::new(c))) };
            0
        }
        Err(_) => -1,
    }
}

/// Build the card10: Epicardium and Pycardium (ELF files) and the external
/// flash's image (its FAT file system); the RTC from `epoch` (UTC seconds).
/// Returns 0, or -1 if the firmware doesn't load.
#[no_mangle]
pub extern "C" fn cm_init_card10(epi: u32, epi_len: u32, py: u32, py_len: u32, flash: u32, flash_len: u32, epoch: f64) -> i32 {
    let (epicardium, pycardium) = (slice(epi, epi_len).to_vec(), slice(py, py_len).to_vec());
    match max32665::machine(&epicardium, &pycardium, slice(flash, flash_len), epoch as i64) {
        Ok(mut m) => {
            for c in m.cores.iter_mut() {
                c.log_unimplemented = false;
            }
            unsafe { *addr_of_mut!(MACHINE) = Some(Machine::Card10 { m: Box::new(m), epicardium, pycardium }) };
            0
        }
        Err(_) => -1,
    }
}

/// The card10's system reset: the chip starts again, its external flash
/// keeps what's on it.
fn card10_reset() {
    if let Machine::Card10 { m, epicardium, pycardium } = machine() {
        let flash = max32665::ext_flash(m).data.clone();
        let epoch = m.bus().device::<max32665::Rtc>(max32665::RTC_BASE).map_or(0, |r| r.seconds(0));
        let now = m.now_ns();
        if let Ok(mut n) = max32665::machine(epicardium, pycardium, &flash, 0) {
            if let Some(r) = n.bus().device::<max32665::Rtc>(max32665::RTC_BASE) {
                r.set_seconds(epoch, now);
            }
            for c in n.cores.iter_mut() {
                c.log_unimplemented = false;
                c.warp_ns = now;
            }
            **m = n;
        }
    }
}

/// Reset the machine (as its reset button: the flash and external flash
/// keep their contents).
#[no_mangle]
pub extern "C" fn cm_reset() {
    match machine() {
        Machine::Pebble(c) => c.reset(),
        Machine::Card10 { .. } => card10_reset(),
    }
}

/// Run up to `budget` instructions: 0 = budget used, 1 = idle (waiting for an
/// interrupt), 2 = halted (a lockup), 3 = reset, 4 = breakpoint.
#[no_mangle]
pub extern "C" fn cm_run(budget: u32) -> u32 {
    if let Some(m) = card10() {
        return match m.run(budget as u64) {
            Stop::Budget => 0,
            Stop::Idle => 1,
            Stop::Halted => 2,
            Stop::Breakpoint => 4,
            Stop::Reset => {
                card10_reset();
                3
            }
        };
    }
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
pub extern "C" fn cm_now_ms() -> f64 {
    match card10() {
        Some(m) => m.now_ns() as f64 / 1e6,
        None => cpu().now_ns() as f64 / 1e6,
    }
}

/// When the next device or SysTick event is due (machine ms), or -1.
#[no_mangle]
pub extern "C" fn cm_next_event_ms() -> f64 {
    let n = match card10() {
        Some(m) => m.next_event_ns(),
        None => cpu().bus.next_event_ns(),
    };
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
    let target = (ms * 1e6) as u64;
    if let Some(m) = card10() {
        m.advance_to(target);
        return;
    }
    let c = cpu();
    let now = c.now_ns();
    if target > now {
        c.warp_ns += target - now;
    }
    c.bus.update(c.now_ns());
    c.bus.scs.dirty = true;
    c.wake_check();
}

/// Run the CPU (both cores, on the card10) as if its clock were `scale`
/// times slower: each instruction takes that much more of the machine's
/// time, which keeps on at the same rate. For hosts that can't keep up.
#[no_mangle]
pub extern "C" fn cm_set_clock_scale(scale: f64) {
    let set = |c: &mut Cpu, hz: u64| c.set_ns_per_insn((1e12 / hz as f64 * scale.max(1.0)) as u64);
    match machine() {
        Machine::Pebble(c) => {
            let hz = c.bus.scs.cpu_hz;
            set(c, hz)
        }
        Machine::Card10 { m, .. } => {
            for c in m.cores.iter_mut() {
                set(c, max32665::CPU_HZ);
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn cm_instructions() -> f64 {
    match card10() {
        Some(m) => m.cores.iter().map(|c| c.icount as f64).sum(),
        None => cpu().icount as f64,
    }
}

/// The buttons held: bit 0 back, 1 up, 2 select, 3 down.
#[no_mangle]
pub extern "C" fn cm_set_buttons(state: u32) {
    if let Some(m) = card10() {
        m.set_buttons(state as u8);
        return;
    }
    let c = cpu();
    c.bus.device::<Gpio>(GPIO_BASE).unwrap().set_buttons(state);
    c.bus.sync_all_irqs();
    c.bus.scs.dirty = true;
    c.wake_check();
}

/// The finger on the touch screen (boards with one): down or up, at (x, y)
/// in display pixels.
#[no_mangle]
pub extern "C" fn cm_touch(down: u32, x: u32, y: u32) {
    if card10().is_some() {
        return;
    }
    let c = cpu();
    if let Some(t) = c.bus.device::<Touch>(TOUCH_BASE) {
        t.input(down != 0, x, y);
    }
    c.bus.sync_all_irqs();
    c.bus.scs.dirty = true;
    c.wake_check();
}

/// Does the board have a touch screen?
#[no_mangle]
pub extern "C" fn cm_has_touch() -> u32 {
    match card10() {
        Some(_) => 0,
        None => cpu().bus.device::<Touch>(TOUCH_BASE).is_some() as u32,
    }
}

/// Take what UART `n` sent since the last call: returns its length; the
/// bytes are at cm_out_ptr().
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn cm_uart_take(n: u32) -> u32 {
    let out = unsafe { &mut *addr_of_mut!(OUT) };
    out.clear();
    if let Some(m) = card10() {
        if let Some(u) = max32665::UART_BASE.get(n as usize).and_then(|&b| m.bus().device::<max32665::Uart>(b)) {
            out.append(&mut u.tx);
        }
    } else if let Some(u) = UART_BASE.get(n as usize).and_then(|&b| cpu().bus.device::<Uart>(b)) {
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
    if let Some(m) = card10() {
        if let Some(u) = max32665::UART_BASE.get(n as usize).and_then(|&b| m.bus().device::<max32665::Uart>(b)) {
            u.input(&[byte as u8]);
        }
        m.input_changed();
        return;
    }
    let c = cpu();
    if let Some(u) = UART_BASE.get(n as usize).and_then(|&b| c.bus.device::<Uart>(b)) {
        u.input(&[byte as u8]);
    }
    c.bus.sync_all_irqs();
    c.bus.scs.dirty = true;
    c.wake_check();
}

#[no_mangle]
pub extern "C" fn cm_display_width() -> u32 {
    match card10() {
        Some(_) => crate::dev::st7735::VIEW_WIDTH as u32,
        None => pebble::display(cpu()).borrow().width,
    }
}

#[no_mangle]
pub extern "C" fn cm_display_height() -> u32 {
    match card10() {
        Some(_) => crate::dev::st7735::VIEW_HEIGHT as u32,
        None => pebble::display(cpu()).borrow().height,
    }
}

/// Changes when what the display shows may have changed (0: off).
#[no_mangle]
pub extern "C" fn cm_display_frame() -> u32 {
    if let Some(m) = card10() {
        // (the card10's display is lit: its backlight isn't modelled)
        return max32665::display(m).frame | 1;
    }
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
    let (rgb, px) = unsafe { (&mut *addr_of_mut!(RGB), &mut *addr_of_mut!(PIXELS)) };
    if let Some(m) = card10() {
        use crate::dev::st7735::{VIEW_HEIGHT, VIEW_WIDTH};
        let n = VIEW_WIDTH * VIEW_HEIGHT;
        rgb.resize(n, 0);
        px.resize(n * 4, 0);
        max32665::display(m).render(rgb);
    } else {
        let d = pebble::display(cpu());
        let d = d.borrow();
        let n = (d.width * d.height) as usize;
        rgb.resize(n, 0);
        px.resize(n * 4, 0);
        d.render(rgb);
    }
    for (o, &v) in px.chunks_mut(4).zip(rgb.iter()) {
        o.copy_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]);
    }
    px.as_ptr() as u32
}
