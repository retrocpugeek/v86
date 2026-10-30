//! The memory map: flash (code, read-only to the CPU) at FLASH_BASE, SRAM at
//! SRAM_BASE with its bit-band alias, the peripherals (devices mapped by the
//! machine, with their interrupt lines into the NVIC), the peripheral
//! bit-band alias and the System Control Space.
//!
//! As in Pebble's QEMU machine (ignore_memory_transaction_failures),
//! accesses to nothing read 0 and are otherwise ignored; `log_unmapped`
//! reports them.

use crate::scs::{Scs, DWT_BASE, SCS_BASE};

pub const FLASH_BASE: u32 = 0x0000_0000;
pub const SRAM_BASE: u32 = 0x2000_0000;
const SRAM_BB_ALIAS: u32 = 0x2200_0000;
const PERIPH_BASE: u32 = 0x4000_0000;
const PERIPH_BB_ALIAS: u32 = 0x4200_0000;

/// A memory-mapped device.
pub trait Device {
    /// A read of `size` bytes (1, 2 or 4) at offset `off`, at time `now` (ns).
    fn read(&mut self, off: u32, size: u32, now: u64) -> u32;
    fn write(&mut self, off: u32, size: u32, value: u32, now: u64);
    /// Bring time-driven state up to `now`.
    fn update(&mut self, _now: u64) {}
    /// When update() next has something to do (ns), or u64::MAX.
    fn next_event_ns(&self) -> u64 { u64::MAX }
    /// The interrupt line's level.
    fn irq(&self) -> bool { false }
    /// Ranges of the XIP memory (Bus::xip) to erase (to 0xFF), at once.
    fn xip_erases(&mut self) -> Vec<(usize, usize)> { Vec::new() }
    /// For downcasting (the host reaches into devices: display, buttons, ...).
    fn as_any(&mut self) -> &mut dyn std::any::Any;
}

pub struct Mapping {
    pub base: u32,
    pub size: u32,
    /// The external interrupt it drives, if any
    pub irq: Option<usize>,
    pub dev: Box<dyn Device>,
}

pub struct Bus {
    pub flash: Vec<u8>,
    pub sram: Vec<u8>,
    /// Memory-mapped external flash (read and write), at xip_base
    pub xip: Vec<u8>,
    pub xip_base: u32,
    pub scs: Scs,
    pub maps: Vec<Mapping>,
    /// The machine's time (ns) and cycle count, kept up to date by the CPU
    /// before device accesses
    pub now_ns: u64,
    pub cycles: u64,
    pub log_unmapped: bool,
    /// Index into maps of the last device hit (most accesses repeat)
    last_map: usize,
}

impl Bus {
    pub fn new(flash_size: usize, sram_size: usize, cpu_hz: u64, ref_hz: u64) -> Bus {
        Bus {
            flash: vec![0xff; flash_size],
            sram: vec![0; sram_size],
            xip: Vec::new(),
            xip_base: 0,
            scs: Scs::new(cpu_hz, ref_hz),
            maps: Vec::new(),
            now_ns: 0,
            cycles: 0,
            log_unmapped: false,
            last_map: 0,
        }
    }

    pub fn map(&mut self, base: u32, size: u32, irq: Option<usize>, dev: Box<dyn Device>) {
        self.maps.push(Mapping { base, size, irq, dev });
    }

    /// The device of type T mapped at `base`.
    pub fn device<T: 'static>(&mut self, base: u32) -> Option<&mut T> {
        self.maps.iter_mut().find(|m| m.base == base)?.dev.as_any().downcast_mut::<T>()
    }

    #[inline(always)]
    fn sram_off(&self, a: u32, size: u32) -> Option<usize> {
        let off = a.wrapping_sub(SRAM_BASE) as usize;
        (off + size as usize <= self.sram.len()).then_some(off)
    }

    #[inline(always)]
    fn xip_off(&self, a: u32, size: u32) -> Option<usize> {
        let off = a.wrapping_sub(self.xip_base) as usize;
        (off + size as usize <= self.xip.len()).then_some(off)
    }

    #[inline(always)]
    fn flash_off(&self, a: u32, size: u32) -> Option<usize> {
        let off = a.wrapping_sub(FLASH_BASE) as usize;
        (off + size as usize <= self.flash.len()).then_some(off)
    }

    /// A read of 1, 2 or 4 bytes (little-endian, any alignment).
    #[inline]
    pub fn read(&mut self, a: u32, size: u32) -> u32 {
        if let Some(off) = self.sram_off(a, size) {
            return le(&self.sram[off..], size);
        }
        if let Some(off) = self.flash_off(a, size) {
            return le(&self.flash[off..], size);
        }
        self.read_slow(a, size)
    }

    #[inline]
    pub fn write(&mut self, a: u32, size: u32, v: u32) {
        if let Some(off) = self.sram_off(a, size) {
            set_le(&mut self.sram[off..], size, v);
            return;
        }
        self.write_slow(a, size, v)
    }

    /// An instruction halfword.
    #[inline(always)]
    pub fn fetch16(&mut self, a: u32) -> u16 {
        if let Some(off) = self.flash_off(a, 2) {
            return u16::from_le_bytes([self.flash[off], self.flash[off + 1]]);
        }
        self.read(a, 2) as u16
    }

    fn read_slow(&mut self, a: u32, size: u32) -> u32 {
        if let Some(off) = self.xip_off(a, size) {
            return le(&self.xip[off..], size);
        }
        // Bit-band aliases: a word per bit
        if let Some((w, bit)) = bitband(a) {
            return self.read(w, 1) >> bit & 1;
        }
        if a & !0xfff == SCS_BASE || a & !0xfff == DWT_BASE {
            let w = self.scs.read(a & !3, self.now_ns, self.cycles);
            return w >> ((a & 3) * 8) & mask(size);
        }
        if let Some(i) = self.find(a) {
            let now = self.now_ns;
            let m = &mut self.maps[i];
            let v = m.dev.read(a - m.base, size, now);
            self.sync_irq(i);
            return v;
        }
        if self.log_unmapped {
            eprintln!("cm: unmapped read {a:#010x} ({size})");
        }
        0
    }

    fn write_slow(&mut self, a: u32, size: u32, v: u32) {
        if let Some(off) = self.xip_off(a, size) {
            set_le(&mut self.xip[off..], size, v);
            return;
        }
        if let Some((w, bit)) = bitband(a) {
            let b = self.read(w, 1);
            let b = if v & 1 != 0 { b | 1 << bit } else { b & !(1 << bit) };
            self.write(w, 1, b);
            return;
        }
        if a & !0xfff == SCS_BASE || a & !0xfff == DWT_BASE {
            if size == 1 {
                self.scs.write_byte(a, v as u8);
            } else if size == 2 {
                self.scs.write_byte(a, v as u8);
                self.scs.write_byte(a + 1, (v >> 8) as u8);
            } else {
                self.scs.write(a, v, self.now_ns, self.cycles);
            }
            return;
        }
        if self.flash_off(a, size).is_some() {
            // Flash is read-only (the firmware writes it through no
            // controller on these machines)
            return;
        }
        if let Some(i) = self.find(a) {
            let now = self.now_ns;
            let m = &mut self.maps[i];
            m.dev.write(a - m.base, size, v, now);
            for (o, n) in m.dev.xip_erases() {
                if let Some(b) = self.xip.get_mut(o..o + n) {
                    b.fill(0xff);
                }
            }
            self.sync_irq(i);
            return;
        }
        if self.log_unmapped {
            eprintln!("cm: unmapped write {a:#010x} ({size}) = {v:#x}");
        }
    }

    fn find(&mut self, a: u32) -> Option<usize> {
        let hit = |m: &Mapping| a.wrapping_sub(m.base) < m.size;
        if self.maps.get(self.last_map).is_some_and(hit) {
            return Some(self.last_map);
        }
        let i = self.maps.iter().position(hit)?;
        self.last_map = i;
        Some(i)
    }

    /// Copy a device's interrupt line into the NVIC.
    fn sync_irq(&mut self, i: usize) {
        let m = &self.maps[i];
        if let Some(irq) = m.irq {
            let level = m.dev.irq();
            if level != self.scs.lines[irq] {
                self.scs.set_line(irq, level);
            }
        }
    }

    /// Bring the devices and SysTick up to `now`.
    pub fn update(&mut self, now: u64) {
        self.now_ns = now;
        self.scs.update(now);
        for i in 0..self.maps.len() {
            if self.maps[i].dev.next_event_ns() <= now {
                self.maps[i].dev.update(now);
                self.sync_irq(i);
            }
        }
    }

    /// When update() next has something to do (ns).
    pub fn next_event_ns(&self) -> u64 {
        self.maps.iter().map(|m| m.dev.next_event_ns()).fold(self.scs.next_event_ns(), u64::min)
    }

    /// Copy a device's line after the host changed it (a button, input).
    pub fn sync_all_irqs(&mut self) {
        for i in 0..self.maps.len() {
            self.sync_irq(i);
        }
    }

    /// Load `data` at `a` (flash or SRAM), for the ELF loader.
    pub fn load(&mut self, a: u32, data: &[u8]) -> Result<(), String> {
        if let Some(off) = self.flash_off(a, data.len() as u32) {
            self.flash[off..off + data.len()].copy_from_slice(data);
        } else if let Some(off) = self.sram_off(a, data.len() as u32) {
            self.sram[off..off + data.len()].copy_from_slice(data);
        } else {
            return Err(format!("{} bytes at {a:#x}: not in flash or SRAM", data.len()));
        }
        Ok(())
    }
}

/// The word and bit a bit-band alias address stands for.
fn bitband(a: u32) -> Option<(u32, u32)> {
    let (alias, base) = if a.wrapping_sub(SRAM_BB_ALIAS) < 0x0200_0000 {
        (SRAM_BB_ALIAS, SRAM_BASE)
    } else if a.wrapping_sub(PERIPH_BB_ALIAS) < 0x0200_0000 {
        (PERIPH_BB_ALIAS, PERIPH_BASE)
    } else {
        return None;
    };
    let off = a - alias;
    Some((base + off / 32, off / 4 % 8))
}

#[inline(always)]
pub fn mask(size: u32) -> u32 { if size >= 4 { u32::MAX } else { (1 << (8 * size)) - 1 } }

#[inline(always)]
pub fn le(b: &[u8], size: u32) -> u32 {
    match size {
        1 => b[0] as u32,
        2 => u16::from_le_bytes([b[0], b[1]]) as u32,
        _ => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
    }
}

#[inline(always)]
pub fn set_le(b: &mut [u8], size: u32, v: u32) {
    match size {
        1 => b[0] = v as u8,
        2 => b[..2].copy_from_slice(&(v as u16).to_le_bytes()),
        _ => b[..4].copy_from_slice(&v.to_le_bytes()),
    }
}
