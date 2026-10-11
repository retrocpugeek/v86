//! Devices used by the native runner. In the browser build these are expected
//! to live on the JS side (reusing v86's virtio stack); the register models
//! follow QEMU's so both stay comparable against the reference.

pub mod pl011;
pub mod pl031;
pub mod virtio;
pub mod virtio_blk;

/// PrimeCell peripheral/cell ID registers at 0xfe0..0xffc.
pub(crate) fn primecell_id(off: u64, periph: [u8; 4]) -> Option<u64> {
    const CELL: [u8; 4] = [0x0d, 0xf0, 0x05, 0xb1];
    match off {
        0xfe0..=0xfec => Some(periph[((off - 0xfe0) / 4) as usize] as u64),
        0xff0..=0xffc => Some(CELL[((off - 0xff0) / 4) as usize] as u64),
        _ => None,
    }
}
