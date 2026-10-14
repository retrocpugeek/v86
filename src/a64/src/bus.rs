//! Physical address space: RAM, the GIC, and MMIO devices.

use crate::gic::Gic;

pub const GICD_BASE: u64 = 0x0800_0000;
pub const GICC_BASE: u64 = 0x0801_0000;
pub const GIC_REGION_SIZE: u64 = 0x1_0000;

/// What a device gets to see when it is accessed: guest RAM (for DMA) and the
/// interrupt controller (to raise or lower its lines).
pub struct DevCtx<'a> {
    pub ram: &'a mut [u8],
    pub ram_base: u64,
    pub gic: &'a mut Gic,
    pub now_ns: u64,
}

impl DevCtx<'_> {
    pub fn ram_slice(&mut self, pa: u64, len: usize) -> Option<&mut [u8]> {
        let off = pa.checked_sub(self.ram_base)? as usize;
        self.ram.get_mut(off..off.checked_add(len)?)
    }
}

pub trait Device {
    fn name(&self) -> &str;
    fn read(&mut self, offset: u64, size: u32, ctx: &mut DevCtx) -> u64;
    fn write(&mut self, offset: u64, size: u32, value: u64, ctx: &mut DevCtx);
    /// Called periodically by the run loop (input, completion of async work).
    fn poll(&mut self, _ctx: &mut DevCtx) {}
}

struct Mapping {
    base: u64,
    size: u64,
    dev: Box<dyn Device>,
}

pub struct Bus {
    pub ram: Vec<u8>,
    pub ram_base: u64,
    pub gic: Gic,
    maps: Vec<Mapping>,
    pub now_ns: u64,
    pub log_unmapped: bool,
    /// Most recent device (non-RAM) read: (pa, size, value). For lockstep.
    pub last_mmio_read: Option<(u64, u32, u64)>,
    /// Per RAM page: 1 if it has compiled code (jit.rs); empty without JIT.
    /// A write to such a page clears the flag and queues the page in
    /// `code_written`: its compiled code is dropped before compiled code
    /// runs again, and compiled code doesn't call into it.
    pub code_pages: Vec<u8>,
    pub code_written: Vec<u32>,
}

impl Bus {
    pub fn new(ram_base: u64, ram_size: usize) -> Bus {
        Bus {
            ram: vec![0; ram_size],
            ram_base,
            gic: Gic::new(),
            maps: Vec::new(),
            now_ns: 0,
            log_unmapped: true,
            last_mmio_read: None,
            code_pages: Vec::new(),
            code_written: Vec::new(),
        }
    }

    /// Note a write to RAM offsets off..off + len (see code_pages).
    #[inline(always)]
    pub fn written(&mut self, off: usize, len: usize) {
        if !self.code_pages.is_empty() {
            self.written_slow(off, len);
        }
    }

    #[inline(never)]
    fn written_slow(&mut self, off: usize, len: usize) {
        for p in off >> 12..=(off + len.max(1) - 1) >> 12 {
            if let Some(f) = self.code_pages.get_mut(p) {
                if *f != 0 {
                    *f = 0;
                    self.code_written.push(p as u32);
                }
            }
        }
    }

    pub fn map(&mut self, base: u64, size: u64, dev: Box<dyn Device>) { self.maps.push(Mapping { base, size, dev }); }

    #[inline(always)]
    pub fn ram_offset(&self, pa: u64, size: u64) -> Option<usize> {
        let off = pa.wrapping_sub(self.ram_base);
        if off.wrapping_add(size) <= self.ram.len() as u64 && off < self.ram.len() as u64 {
            Some(off as usize)
        } else {
            None
        }
    }

    pub fn is_ram(&self, pa: u64) -> bool { self.ram_offset(pa, 1).is_some() }

    /// Read at an offset in RAM; `off + size` must be within RAM (callers get
    /// `off` from a RAM page in the TLB and don't cross the page).
    #[inline(always)]
    pub fn read_ram(&self, off: usize, size: u32) -> u64 {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        assert!(off + size as usize <= self.ram.len());
        // Safety: checked above
        unsafe {
            let p = self.ram.as_ptr().add(off);
            match size {
                1 => *p as u64,
                2 => u16::from_le((p as *const u16).read_unaligned()) as u64,
                4 => u32::from_le((p as *const u32).read_unaligned()) as u64,
                _ => u64::from_le((p as *const u64).read_unaligned()),
            }
        }
    }

    #[inline(always)]
    pub fn write_ram(&mut self, off: usize, size: u32, value: u64) {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        assert!(off + size as usize <= self.ram.len());
        self.written(off, size as usize);
        // Safety: checked above
        unsafe {
            let p = self.ram.as_mut_ptr().add(off);
            match size {
                1 => *p = value as u8,
                2 => (p as *mut u16).write_unaligned((value as u16).to_le()),
                4 => (p as *mut u32).write_unaligned((value as u32).to_le()),
                _ => (p as *mut u64).write_unaligned(value.to_le()),
            }
        }
    }

    pub fn read(&mut self, pa: u64, size: u32) -> u64 {
        if let Some(off) = self.ram_offset(pa, size as u64) {
            return read_le(&self.ram[off..], size);
        }
        let v = self.read_mmio(pa, size);
        self.last_mmio_read = Some((pa, size, v));
        v
    }

    pub fn write(&mut self, pa: u64, size: u32, value: u64) {
        if let Some(off) = self.ram_offset(pa, size as u64) {
            self.written(off, size as usize);
            write_le(&mut self.ram[off..], size, value);
            return;
        }
        self.write_mmio(pa, size, value)
    }

    fn read_mmio(&mut self, pa: u64, size: u32) -> u64 {
        if pa.wrapping_sub(GICD_BASE) < GIC_REGION_SIZE {
            return self.gic.dist_read(pa - GICD_BASE, size) as u64;
        }
        if pa.wrapping_sub(GICC_BASE) < GIC_REGION_SIZE {
            return self.gic.cpu_read(pa - GICC_BASE, size) as u64;
        }
        let Bus { ram, ram_base, gic, maps, now_ns, .. } = self;
        for m in maps.iter_mut() {
            if pa.wrapping_sub(m.base) < m.size {
                let mut ctx = DevCtx { ram, ram_base: *ram_base, gic, now_ns: *now_ns };
                return m.dev.read(pa - m.base, size, &mut ctx);
            }
        }
        if self.log_unmapped {
            eprintln!("a64: unmapped read pa={pa:#x} size={size}");
        }
        0
    }

    fn write_mmio(&mut self, pa: u64, size: u32, value: u64) {
        if pa.wrapping_sub(GICD_BASE) < GIC_REGION_SIZE {
            return self.gic.dist_write(pa - GICD_BASE, size, value as u32);
        }
        if pa.wrapping_sub(GICC_BASE) < GIC_REGION_SIZE {
            return self.gic.cpu_write(pa - GICC_BASE, size, value as u32);
        }
        let Bus { ram, ram_base, gic, maps, now_ns, .. } = self;
        for m in maps.iter_mut() {
            if pa.wrapping_sub(m.base) < m.size {
                let mut ctx = DevCtx { ram, ram_base: *ram_base, gic, now_ns: *now_ns };
                return m.dev.write(pa - m.base, size, value, &mut ctx);
            }
        }
        if self.log_unmapped {
            eprintln!("a64: unmapped write pa={pa:#x} size={size} value={value:#x}");
        }
    }

    pub fn poll_devices(&mut self) {
        let Bus { ram, ram_base, gic, maps, now_ns, .. } = self;
        for m in maps.iter_mut() {
            let mut ctx = DevCtx { ram, ram_base: *ram_base, gic, now_ns: *now_ns };
            m.dev.poll(&mut ctx);
        }
    }

    pub fn load(&mut self, pa: u64, data: &[u8]) {
        let off = self.ram_offset(pa, data.len() as u64).expect("load outside RAM");
        self.written(off, data.len());
        self.ram[off..off + data.len()].copy_from_slice(data);
    }
}

#[inline(always)]
pub fn read_le(b: &[u8], size: u32) -> u64 {
    match size {
        1 => b[0] as u64,
        2 => u16::from_le_bytes([b[0], b[1]]) as u64,
        4 => u32::from_le_bytes(b[..4].try_into().unwrap()) as u64,
        8 => u64::from_le_bytes(b[..8].try_into().unwrap()),
        _ => unreachable!(),
    }
}

#[inline(always)]
pub fn write_le(b: &mut [u8], size: u32, v: u64) {
    match size {
        1 => b[0] = v as u8,
        2 => b[..2].copy_from_slice(&(v as u16).to_le_bytes()),
        4 => b[..4].copy_from_slice(&(v as u32).to_le_bytes()),
        8 => b[..8].copy_from_slice(&v.to_le_bytes()),
        _ => unreachable!(),
    }
}
