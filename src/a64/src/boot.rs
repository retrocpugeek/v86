//! Direct kernel boot, laid out exactly like QEMU's `-kernel` on the virt
//! machine so execution can be compared against it from the first instruction.

use crate::cpu::Cpu;

pub const RAM_BASE: u64 = 0x4000_0000;
/// QEMU loads an arm64 Image 2 MB into RAM (plus text_offset).
pub const KERNEL_OFFSET: u64 = 0x20_0000;

pub struct BootInfo {
    pub entry: u64,
    pub dtb: u64,
}

/// Parse the arm64 Image header; returns text_offset.
pub fn image_text_offset(image: &[u8]) -> Result<u64, String> {
    if image.len() < 64 || &image[0x38..0x3c] != b"ARM\x64" {
        return Err("not an arm64 Image (missing ARM\\x64 magic)".into());
    }
    Ok(u64::from_le_bytes(image[8..16].try_into().unwrap()))
}

/// Load kernel and DTB and install QEMU's boot stub at the start of RAM:
///   ldr x0, dtb; mov x1..x3, xzr; ldr x4, entry; br x4
pub fn load(cpu: &mut Cpu, image: &[u8], dtb: &[u8]) -> Result<BootInfo, String> {
    let text_offset = image_text_offset(image)?;
    let ram_size = cpu.bus.ram.len() as u64;
    let entry = RAM_BASE + KERNEL_OFFSET + text_offset;
    // QEMU puts the DTB at the 64 MB mark when RAM allows (above the kernel).
    let dtb_addr = if ram_size >= 128 << 20 { RAM_BASE + (64 << 20) } else { (RAM_BASE + ram_size - 0x20_0000) & !0xfff };
    if entry + image.len() as u64 > dtb_addr {
        return Err("kernel image overlaps the DTB".into());
    }
    cpu.bus.load(entry, image);
    cpu.bus.load(dtb_addr, dtb);
    let stub: [u32; 6] = [0x5800_00c0, 0xaa1f_03e1, 0xaa1f_03e2, 0xaa1f_03e3, 0x5800_0084, 0xd61f_0080];
    let mut bytes = Vec::new();
    for w in stub {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    bytes.extend_from_slice(&dtb_addr.to_le_bytes());
    bytes.extend_from_slice(&entry.to_le_bytes());
    cpu.bus.load(RAM_BASE, &bytes);
    cpu.pc = RAM_BASE;
    Ok(BootInfo { entry, dtb: dtb_addr })
}
