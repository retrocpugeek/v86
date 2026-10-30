//! Loading firmware from an ELF file: its PT_LOAD segments' file contents at
//! their physical addresses (as QEMU's armv7m_load_kernel does: initialised
//! data goes to its load address in flash, for the startup code to copy).

use crate::bus::Bus;

pub fn load(bus: &mut Bus, elf: &[u8]) -> Result<(), String> {
    let u16_at = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]) as usize;
    let u32_at = |o: usize| u32::from_le_bytes(elf[o..o + 4].try_into().unwrap());
    if elf.len() < 52 || &elf[..4] != b"\x7fELF" || elf[4] != 1 || elf[5] != 1 {
        return Err("not a 32-bit little-endian ELF file".into());
    }
    if u16_at(18) != 40 {
        return Err("not an ARM ELF file".into());
    }
    let (phoff, phentsize, phnum) = (u32_at(28) as usize, u16_at(42), u16_at(44));
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if ph + 32 > elf.len() {
            return Err("truncated program headers".into());
        }
        let (ptype, offset, paddr, filesz) = (u32_at(ph), u32_at(ph + 4) as usize, u32_at(ph + 12), u32_at(ph + 16) as usize);
        if ptype != 1 || filesz == 0 {
            continue;
        }
        let data = elf.get(offset..offset + filesz).ok_or("segment beyond the end of the file")?;
        bus.load(paddr, data)?;
    }
    Ok(())
}
