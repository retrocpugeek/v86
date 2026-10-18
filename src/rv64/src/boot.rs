//! Boot like QEMU's riscv64 virt machine with `-bios fw_dynamic.bin -kernel
//! Image`: the reset vector in the boot ROM at 0x1000 jumps to the firmware
//! (OpenSBI) at the start of RAM with a0 = hart id, a1 = DTB and
//! a2 = fw_dynamic_info, which tells OpenSBI to start the kernel in S mode.

use crate::bus::{RAM_BASE, ROM_BASE};
use crate::cpu::{Cpu, PRV_M};

const MIB: u64 = 1 << 20;

pub struct BootInfo {
    pub firmware: u64,
    pub kernel: u64,
    pub dtb: u64,
}

/// QEMU's riscv_compute_fdt_addr: 2 MB-aligned below the end of RAM (or the
/// 3 GB mark).
pub fn dtb_addr(ram_size: u64, dtb_len: u64) -> u64 {
    let end = (RAM_BASE + ram_size).min(3072 * MIB);
    (end - dtb_len) & !(2 * MIB - 1)
}

/// Load firmware, kernel and DTB, and the boot ROM; start at the reset vector.
pub fn load(cpu: &mut Cpu, firmware: &[u8], kernel: &[u8], dtb: &[u8]) -> Result<BootInfo, String> {
    let ram_size = cpu.bus.ram.len() as u64;
    let fw = RAM_BASE;
    // riscv_calc_kernel_start_addr: the firmware's end, aligned to 2 MB
    let kernel_addr = (fw + firmware.len() as u64).next_multiple_of(2 * MIB);
    let dtb_addr = dtb_addr(ram_size, dtb.len() as u64);
    if kernel_addr + kernel.len() as u64 > dtb_addr {
        return Err("kernel overlaps the DTB".into());
    }
    cpu.bus.load(fw, firmware);
    cpu.bus.load(kernel_addr, kernel);
    cpu.bus.load(dtb_addr, dtb);

    // QEMU's riscv_setup_rom_reset_vec, then struct fw_dynamic_info
    let reset_vec: [u32; 10] = [
        0x0000_0297, // 1: auipc t0, %pcrel_hi(fw_dyn)
        0x0282_8613, //    addi  a2, t0, %pcrel_lo(1b)
        0xf140_2573, //    csrr  a0, mhartid
        0x0202_b583, //    ld    a1, 32(t0)
        0x0182_b283, //    ld    t0, 24(t0)
        0x0002_8067, //    jr    t0
        fw as u32,
        (fw >> 32) as u32,
        dtb_addr as u32,
        (dtb_addr >> 32) as u32,
    ];
    let mut rom = Vec::new();
    for w in reset_vec {
        rom.extend_from_slice(&w.to_le_bytes());
    }
    // magic "OSBI", version 2, next_addr, next_mode (S), options, boot_hart
    for v in [0x4942_534f, 2, kernel_addr, 1, 0, 0u64] {
        rom.extend_from_slice(&v.to_le_bytes());
    }
    cpu.bus.rom = rom;
    cpu.pc = ROM_BASE;
    cpu.prv = PRV_M;
    Ok(BootInfo { firmware: fw, kernel: kernel_addr, dtb: dtb_addr })
}
