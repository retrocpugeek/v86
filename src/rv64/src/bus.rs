//! Physical address space of QEMU's riscv64 `virt` machine: RAM, the CLINT
//! and PLIC (here, because the CPU reads their state for interrupts), and
//! other MMIO devices.

pub const RAM_BASE: u64 = 0x8000_0000;
pub const CLINT_BASE: u64 = 0x0200_0000;
pub const CLINT_SIZE: u64 = 0x1_0000;
pub const PLIC_BASE: u64 = 0x0c00_0000;
pub const PLIC_SIZE: u64 = 0x60_0000;
/// Boot ROM with the reset vector (QEMU's "mrom").
pub const ROM_BASE: u64 = 0x1000;
pub const ROM_SIZE: u64 = 0xf000;
/// sifive_test: writes power off (0x5555) or reset (0x7777).
pub const TEST_BASE: u64 = 0x10_0000;
pub const TEST_SIZE: u64 = 0x1000;
/// The CLINT's mtime runs at 10 MHz (timebase-frequency).
pub const TIMEBASE_HZ: u64 = 10_000_000;

/// What a device sees when it's accessed: guest RAM (for DMA), the PLIC (to
/// raise or lower its interrupt line) and the time.
pub struct DevCtx<'a> {
    pub ram: &'a mut [u8],
    pub plic: &'a mut Plic,
    pub now_ns: u64,
}

impl DevCtx<'_> {
    pub fn ram_slice(&mut self, pa: u64, len: usize) -> Option<&mut [u8]> {
        let off = pa.checked_sub(RAM_BASE)? as usize;
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

/// Core-local interruptor: machine software interrupt and timer.
#[derive(Default)]
pub struct Clint {
    pub msip: bool,
    pub mtimecmp: u64,
}

/// Platform-level interrupt controller, one hart: context 0 is M mode,
/// context 1 is S mode.
pub struct Plic {
    pub priority: [u32; PLIC_SOURCES],
    /// Level of each source's line.
    pub level: [bool; PLIC_SOURCES],
    pub pending: [bool; PLIC_SOURCES],
    /// Claimed and not yet completed.
    pub claimed: [bool; PLIC_SOURCES],
    pub enable: [[bool; PLIC_SOURCES]; 2],
    pub threshold: [u32; 2],
    /// External interrupt lines to the hart: [MEIP, SEIP].
    pub out: [bool; 2],
}

pub const PLIC_SOURCES: usize = 96;

impl Default for Plic {
    fn default() -> Self {
        Plic {
            priority: [0; PLIC_SOURCES],
            level: [false; PLIC_SOURCES],
            pending: [false; PLIC_SOURCES],
            claimed: [false; PLIC_SOURCES],
            enable: [[false; PLIC_SOURCES]; 2],
            threshold: [0; 2],
            out: [false; 2],
        }
    }
}

impl Plic {
    /// A device's (level-triggered) interrupt line.
    pub fn set_irq(&mut self, irq: u32, level: bool) {
        let i = irq as usize;
        if i == 0 || i >= PLIC_SOURCES {
            return;
        }
        self.level[i] = level;
        if level && !self.claimed[i] {
            self.pending[i] = true;
        }
        if !level {
            self.pending[i] = false;
        }
        self.update();
    }

    /// The highest-priority pending, enabled source above the threshold of
    /// context `ctx` (0 if none).
    fn best(&self, ctx: usize) -> usize {
        let mut best = 0;
        let mut best_prio = self.threshold[ctx];
        for i in 1..PLIC_SOURCES {
            if self.pending[i] && self.enable[ctx][i] && self.priority[i] > best_prio {
                best = i;
                best_prio = self.priority[i];
            }
        }
        best
    }

    pub fn update(&mut self) {
        for ctx in 0..2 {
            self.out[ctx] = self.best(ctx) != 0;
        }
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let off = off as usize;
        match off {
            0..=0xfff => self.priority.get(off / 4).copied().unwrap_or(0),
            0x1000..=0x107f => self.bits(&self.pending, (off - 0x1000) / 4),
            0x2000..=0x20ff => {
                let ctx = (off - 0x2000) / 0x80;
                self.bits(&self.enable[ctx], (off - 0x2000) % 0x80 / 4)
            }
            0x20_0000..=0x20_1fff => {
                let ctx = (off - 0x20_0000) / 0x1000;
                match off & 0xfff {
                    0 => self.threshold[ctx],
                    4 => {
                        // Claim
                        let i = self.best(ctx);
                        if i != 0 {
                            self.pending[i] = false;
                            self.claimed[i] = true;
                            self.update();
                        }
                        i as u32
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        let off = off as usize;
        match off {
            0..=0xfff => {
                if let Some(p) = self.priority.get_mut(off / 4) {
                    *p = v & 7;
                }
            }
            0x2000..=0x20ff => {
                let ctx = (off - 0x2000) / 0x80;
                let word = (off - 0x2000) % 0x80 / 4;
                for b in 0..32 {
                    if let Some(e) = self.enable[ctx].get_mut(word * 32 + b) {
                        *e = v >> b & 1 != 0;
                    }
                }
                self.enable[ctx][0] = false;
            }
            0x20_0000..=0x20_1fff => {
                let ctx = (off - 0x20_0000) / 0x1000;
                match off & 0xfff {
                    0 => self.threshold[ctx] = v & 7,
                    4 => {
                        // Complete: a line that's still high is pending again
                        let i = v as usize;
                        if i < PLIC_SOURCES && self.claimed[i] {
                            self.claimed[i] = false;
                            if self.level[i] {
                                self.pending[i] = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        self.update();
    }

    fn bits(&self, v: &[bool; PLIC_SOURCES], word: usize) -> u32 {
        (0..32).filter(|b| v.get(word * 32 + b).copied().unwrap_or(false)).fold(0, |a, b| a | 1 << b)
    }
}

/// What the guest asked for through the sifive_test device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerRequest {
    None,
    Off,
    Reset,
}

pub struct Bus {
    pub ram: Vec<u8>,
    /// Where RAM starts (RAM_BASE; the CH32V003's SRAM is at 0x2000_0000).
    pub ram_base: u64,
    pub rom: Vec<u8>,
    /// Where the boot ROM is (ROM_BASE; the CH32V003's flash is at
    /// 0x0800_0000, and also at 0 with rom_alias0).
    pub rom_base: u64,
    pub rom_alias0: bool,
    /// The CH32V003's SysTick and interrupt controller (dev/ch32.rs).
    pub ch32: Option<Box<crate::dev::ch32::Ch32>>,
    pub power_request: PowerRequest,
    pub clint: Clint,
    pub plic: Plic,
    /// The machine's time, kept up to date by the CPU before device accesses.
    pub now_ns: u64,
    maps: Vec<Mapping>,
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
    pub fn new(ram_size: usize) -> Bus {
        Bus {
            ram: vec![0; ram_size],
            ram_base: RAM_BASE,
            rom: Vec::new(),
            rom_base: ROM_BASE,
            rom_alias0: false,
            ch32: None,
            power_request: PowerRequest::None,
            clint: Clint::default(),
            plic: Plic::default(),
            now_ns: 0,
            maps: Vec::new(),
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

    /// mtime: 10 MHz ticks.
    pub fn mtime(&self) -> u64 { self.now_ns / (1_000_000_000 / TIMEBASE_HZ) }

    /// Offset of [pa, pa + len) in RAM, if it's all RAM.
    #[inline(always)]
    pub fn ram_offset(&self, pa: u64, len: u64) -> Option<usize> {
        let off = pa.wrapping_sub(self.ram_base);
        if off < self.ram.len() as u64 && len <= self.ram.len() as u64 - off {
            Some(off as usize)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn read_ram(&self, off: usize, size: u32) -> u64 {
        read_le(&self.ram[off..], size)
    }

    #[inline(always)]
    pub fn write_ram(&mut self, off: usize, size: u32, value: u64) {
        self.written(off, size as usize);
        write_le(&mut self.ram[off..], size, value);
    }

    pub fn read(&mut self, pa: u64, size: u32) -> u64 {
        if let Some(off) = self.ram_offset(pa, size as u64) {
            return self.read_ram(off, size);
        }
        let v = self.read_mmio(pa, size);
        self.last_mmio_read = Some((pa, size, v));
        v
    }

    pub fn write(&mut self, pa: u64, size: u32, value: u64) {
        if let Some(off) = self.ram_offset(pa, size as u64) {
            return self.write_ram(off, size, value);
        }
        self.write_mmio(pa, size, value)
    }

    pub fn has_rom(&self, pa: u64) -> bool { self.rom_offset(pa).is_some() }

    /// A read of `size` bytes from the boot ROM or flash, if pa..pa+size is in it.
    #[inline]
    pub fn rom_read(&self, pa: u64, size: u32) -> Option<u64> {
        let off = self.rom_offset(pa)?;
        self.rom.get(off..off + size as usize).map(|b| read_le(b, size))
    }

    /// A byte of RAM or ROM (0 elsewhere), as DMA reads it.
    fn peek(&self, pa: u64) -> u8 {
        if let Some(off) = self.ram_offset(pa, 1) {
            self.ram[off]
        } else if let Some(off) = self.rom_offset(pa) {
            self.rom[off]
        } else {
            0
        }
    }

    pub fn rom_offset(&self, pa: u64) -> Option<usize> {
        let len = self.rom.len() as u64;
        let off = pa.wrapping_sub(self.rom_base);
        if off < len {
            Some(off as usize)
        } else if self.rom_alias0 && pa < len {
            Some(pa as usize)
        } else {
            None
        }
    }

    /// Does anything but RAM answer at pa? Other addresses are access faults.
    pub fn has_device(&self, pa: u64) -> bool {
        self.has_rom(pa)
            || self.ch32.as_ref().is_some_and(|c| c.claims(pa))
            || pa.wrapping_sub(CLINT_BASE) < CLINT_SIZE
            || pa.wrapping_sub(PLIC_BASE) < PLIC_SIZE
            || pa.wrapping_sub(TEST_BASE) < TEST_SIZE
            || self.maps.iter().any(|m| pa.wrapping_sub(m.base) < m.size)
    }

    fn read_mmio(&mut self, pa: u64, size: u32) -> u64 {
        if let Some(off) = self.rom_offset(pa) {
            let mut b = [0u8; 8];
            for (i, x) in b.iter_mut().enumerate().take(size as usize) {
                *x = self.rom.get(off + i).copied().unwrap_or(0);
            }
            return read_le(&b, size);
        }
        if let Some(c) = self.ch32.as_mut() {
            if let Some(v) = c.read(pa, size, self.now_ns) {
                return v;
            }
        }
        if pa.wrapping_sub(TEST_BASE) < TEST_SIZE {
            return 0;
        }
        if pa.wrapping_sub(CLINT_BASE) < CLINT_SIZE {
            let off = pa - CLINT_BASE;
            let v = match off & !7 {
                0 => self.clint.msip as u64,
                0x4000 => self.clint.mtimecmp,
                0xbff8 => self.mtime(),
                _ => 0,
            };
            return v >> ((off & 7) * 8) & mask(size);
        }
        if pa.wrapping_sub(PLIC_BASE) < PLIC_SIZE {
            return self.plic.read(pa - PLIC_BASE) as u64;
        }
        let Bus { ram, plic, maps, now_ns, .. } = self;
        for m in maps.iter_mut() {
            if pa.wrapping_sub(m.base) < m.size {
                let mut ctx = DevCtx { ram, plic, now_ns: *now_ns };
                return m.dev.read(pa - m.base, size, &mut ctx);
            }
        }
        if self.log_unmapped {
            eprintln!("rv64: unmapped read pa={pa:#x} size={size}");
        }
        0
    }

    fn write_mmio(&mut self, pa: u64, size: u32, value: u64) {
        if self.has_rom(pa) {
            return;
        }
        if let Some(c) = self.ch32.as_mut() {
            if c.write(pa, size, value, self.now_ns) {
                // DMA starting: it reads memory (RAM or flash)
                if let Some((addr, len)) = c.take_dma_request() {
                    let bytes = (0..len as u64).map(|i| self.peek(addr as u64 + i)).collect();
                    self.ch32.as_mut().unwrap().dma_supply(bytes, self.now_ns);
                }
                return;
            }
        }
        if pa.wrapping_sub(TEST_BASE) < TEST_SIZE {
            if pa == TEST_BASE {
                match value & 0xffff {
                    0x5555 | 0x3333 => self.power_request = PowerRequest::Off,
                    0x7777 => self.power_request = PowerRequest::Reset,
                    _ => {}
                }
            }
            return;
        }
        if pa.wrapping_sub(CLINT_BASE) < CLINT_SIZE {
            let off = pa - CLINT_BASE;
            let shift = (off & 7) * 8;
            let m = mask(size) << shift;
            match off & !7 {
                0 => self.clint.msip = value & 1 != 0,
                0x4000 => self.clint.mtimecmp = (self.clint.mtimecmp & !m) | (value << shift & m),
                // mtime is read-only here (it follows the machine's clock)
                _ => {}
            }
            return;
        }
        if pa.wrapping_sub(PLIC_BASE) < PLIC_SIZE {
            return self.plic.write(pa - PLIC_BASE, value as u32);
        }
        let Bus { ram, plic, maps, now_ns, .. } = self;
        for m in maps.iter_mut() {
            if pa.wrapping_sub(m.base) < m.size {
                let mut ctx = DevCtx { ram, plic, now_ns: *now_ns };
                return m.dev.write(pa - m.base, size, value, &mut ctx);
            }
        }
        if self.log_unmapped {
            eprintln!("rv64: unmapped write pa={pa:#x} size={size} value={value:#x}");
        }
    }

    pub fn poll_devices(&mut self) {
        let Bus { ram, plic, maps, now_ns, .. } = self;
        for m in maps.iter_mut() {
            let mut ctx = DevCtx { ram, plic, now_ns: *now_ns };
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
fn mask(size: u32) -> u64 { if size >= 8 { u64::MAX } else { (1 << (size * 8)) - 1 } }

#[inline(always)]
pub fn read_le(b: &[u8], size: u32) -> u64 {
    match size {
        1 => b[0] as u64,
        2 => u16::from_le_bytes([b[0], b[1]]) as u64,
        4 => u32::from_le_bytes(b[..4].try_into().unwrap()) as u64,
        _ => u64::from_le_bytes(b[..8].try_into().unwrap()),
    }
}

#[inline(always)]
pub fn write_le(b: &mut [u8], size: u32, v: u64) {
    match size {
        1 => b[0] = v as u8,
        2 => b[..2].copy_from_slice(&(v as u16).to_le_bytes()),
        4 => b[..4].copy_from_slice(&(v as u32).to_le_bytes()),
        _ => b[..8].copy_from_slice(&v.to_le_bytes()),
    }
}
