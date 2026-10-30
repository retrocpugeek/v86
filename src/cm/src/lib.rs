//! ARM Cortex-M (ARMv7-M / ARMv7E-M, Thumb-2) system emulator core, for
//! microcontroller firmware; first for PebbleOS's emulator boards (the
//! pebble-generic machine of Pebble's QEMU fork: dev/pebble.rs).

pub mod bus;
pub mod cpu;
pub mod dev;
pub mod elf;
pub mod fpu;
pub mod scs;
pub mod thumb;
#[cfg(target_arch = "wasm32")]
pub mod wasm;
