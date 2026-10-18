//! Runs riscv-tests ISA tests (rv64ui-p-add etc.): loads each ELF, starts it
//! in M mode at its entry, and waits for a write to `tohost` (1: passed,
//! else (test case << 1) | 1: failed).
//!
//!   rvtest <elf>...

use std::process::exit;

use v86_rv64::bus::{Bus, RAM_BASE};
use v86_rv64::cpu::Cpu;

fn u16le(b: &[u8], o: usize) -> u64 { u16::from_le_bytes([b[o], b[o + 1]]) as u64 }
fn u32le(b: &[u8], o: usize) -> u64 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64 }
fn u64le(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

/// Load an ELF64's PT_LOAD segments; returns (entry, tohost address).
fn load_elf(cpu: &mut Cpu, elf: &[u8]) -> Result<(u64, u64), String> {
    if &elf[..4] != b"\x7fELF" || elf[4] != 2 {
        return Err("not an ELF64".into());
    }
    let entry = u64le(elf, 0x18);
    let (phoff, shoff) = (u64le(elf, 0x20) as usize, u64le(elf, 0x28) as usize);
    let (phnum, shentsize, shnum) = (u16le(elf, 0x38) as usize, u16le(elf, 0x3a) as usize, u16le(elf, 0x3c) as usize);
    for k in 0..phnum {
        let p = phoff + k * 0x38;
        if u32le(elf, p) != 1 {
            continue;
        }
        let (off, paddr, filesz) = (u64le(elf, p + 8) as usize, u64le(elf, p + 0x18), u64le(elf, p + 0x20) as usize);
        cpu.bus.load(paddr, &elf[off..off + filesz]);
    }
    // Find tohost in the symbol table
    for k in 0..shnum {
        let s = shoff + k * shentsize;
        if u32le(elf, s + 4) != 2 {
            continue; // SHT_SYMTAB
        }
        let (off, size, link) = (u64le(elf, s + 0x18) as usize, u64le(elf, s + 0x20) as usize, u32le(elf, s + 0x28) as usize);
        let strtab = u64le(elf, shoff + link * shentsize + 0x18) as usize;
        for e in (off..off + size).step_by(24) {
            let name = u32le(elf, e) as usize + strtab;
            let end = name + elf[name..].iter().position(|&c| c == 0).unwrap_or(0);
            if &elf[name..end] == b"tohost" {
                return Ok((entry, u64le(elf, e + 8)));
            }
        }
    }
    Err("no tohost symbol".into())
}

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: rvtest <elf>...");
        exit(2);
    }
    let (mut passed, mut failed) = (0, 0);
    for f in &files {
        let elf = std::fs::read(f).unwrap_or_else(|e| panic!("{f}: {e}"));
        let mut cpu = Cpu::new(Bus::new(64 << 20));
        cpu.ns_per_insn = 1;
        let (entry, tohost) = match load_elf(&mut cpu, &elf) {
            Ok(v) => v,
            Err(e) => {
                println!("{f}: {e}");
                failed += 1;
                continue;
            }
        };
        cpu.pc = entry;
        let off = (tohost - RAM_BASE) as usize;
        let mut result = None;
        for _ in 0..2000 {
            cpu.run(1000);
            let v = cpu.bus.read_ram(off, 8);
            if v != 0 {
                result = Some(v);
                break;
            }
        }
        let name = std::path::Path::new(f).file_name().unwrap().to_string_lossy();
        match result {
            Some(1) => passed += 1,
            Some(v) => {
                println!("FAIL {name}: test {} (pc {:#x})", v >> 1, cpu.pc);
                failed += 1;
            }
            None => {
                println!("FAIL {name}: no result after 2M steps (pc {:#x}, prv {})", cpu.pc, cpu.prv);
                failed += 1;
            }
        }
    }
    println!("{passed} passed, {failed} failed");
    exit(if failed == 0 { 0 } else { 1 });
}
