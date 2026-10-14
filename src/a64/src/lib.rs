//! AArch64 (ARMv8.0-A, EL0/EL1 only) system emulator core for v86.
//!
//! Modelled on QEMU's `virt` machine with `-cpu cortex-a53`, restricted to the
//! devices listed in tools/a64-ref/v86-virt.dts.

pub mod bits;
pub mod boot;
pub mod bus;
pub mod cpu;
pub mod dev;
pub mod exc;
pub mod exec_crypto;
pub mod exec_int;
pub mod exec_ldst;
pub mod exec_simd;
pub mod exec_sys;
pub mod gic;
pub mod mmu;
pub mod sysregs;
#[cfg(target_arch = "wasm32")]
pub mod wasm;
