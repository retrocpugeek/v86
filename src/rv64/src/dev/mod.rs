//! Devices of QEMU's riscv64 `virt` machine used by the native runner. In
//! the browser build these live on the JS side (v86's virtio stack); the
//! register models follow QEMU's so both stay comparable against it.

pub mod rtc;
pub mod uart;
pub mod virtio;
pub mod virtio_blk;
