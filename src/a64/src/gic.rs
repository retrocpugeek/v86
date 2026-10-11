//! GICv2 distributor + CPU interface for a single CPU, no Security Extensions.
//!
//! INTIDs 0-15 are SGIs, 16-31 PPIs, 32+ SPIs. Everything is signalled as IRQ.

pub const NUM_IRQS: usize = 288; // as QEMU virt (GICD_TYPER.ITLinesNumber = 8)
const WORDS: usize = NUM_IRQS / 32;
pub const SPURIOUS: u32 = 1023;

pub const PPI_VTIMER: u32 = 27;
pub const PPI_PTIMER: u32 = 30;

pub struct Gic {
    dist_ctlr: u32,
    group: [u32; WORDS],
    enabled: [u32; WORDS],
    /// Latched pending state (edge, SGI, software-set).
    latch: [u32; WORDS],
    /// Input line level from devices.
    level: [u32; WORDS],
    active: [u32; WORDS],
    /// 1 = edge-triggered (ICFGR bit 1 of each field).
    edge: [u32; WORDS],
    priority: [u8; NUM_IRQS],
    /// Source CPU of a pending SGI (bits 12:10 of IAR); single CPU so always 0.
    cpu_ctlr: u32,
    pmr: u32,
    bpr: u32,
    abpr: u32,
    /// Stack of (intid, priority) for acknowledged but not yet EOI'd interrupts.
    running: Vec<(u32, u8)>,
    /// Output to the CPU: an interrupt is deliverable.
    pub irq_out: bool,
}

impl Default for Gic {
    fn default() -> Self { Self::new() }
}

impl Gic {
    pub fn new() -> Gic {
        let mut g = Gic {
            dist_ctlr: 0,
            group: [0; WORDS],
            enabled: [0; WORDS],
            latch: [0; WORDS],
            level: [0; WORDS],
            active: [0; WORDS],
            edge: [0; WORDS],
            priority: [0; NUM_IRQS],
            cpu_ctlr: 0,
            pmr: 0,
            bpr: 2,
            abpr: 3,
            running: Vec::new(),
            irq_out: false,
        };
        // SGIs are always enabled and edge-triggered.
        g.enabled[0] = 0xffff;
        g.edge[0] = 0xffff;
        g
    }

    #[inline]
    fn get(a: &[u32; WORDS], irq: u32) -> bool { a[(irq / 32) as usize] >> (irq % 32) & 1 != 0 }

    #[inline]
    fn put(a: &mut [u32; WORDS], irq: u32, v: bool) {
        let m = 1 << (irq % 32);
        if v {
            a[(irq / 32) as usize] |= m;
        } else {
            a[(irq / 32) as usize] &= !m;
        }
    }

    fn is_pending(&self, irq: u32) -> bool {
        Self::get(&self.latch, irq) || (!Self::get(&self.edge, irq) && Self::get(&self.level, irq))
    }

    /// Device input line. Edge-triggered interrupts latch on a rising edge.
    pub fn set_irq(&mut self, irq: u32, high: bool) {
        let was = Self::get(&self.level, irq);
        Self::put(&mut self.level, irq, high);
        if high && !was && Self::get(&self.edge, irq) {
            Self::put(&mut self.latch, irq, true);
        }
        self.update();
    }

    /// Convenience for SPIs as numbered in the device tree (`<0 n flags>`).
    pub fn set_spi(&mut self, n: u32, high: bool) { self.set_irq(32 + n, high) }

    /// Pending+enabled INTIDs, for diagnostics.
    pub fn pending_list(&self) -> Vec<u32> {
        (0..NUM_IRQS as u32).filter(|&i| self.is_pending(i) && Self::get(&self.enabled, i)).collect()
    }

    fn running_priority(&self) -> u32 { self.running.last().map(|&(_, p)| p as u32).unwrap_or(0x100) }

    /// Highest-priority pending, enabled, inactive interrupt targeting CPU 0.
    fn highest_pending(&self) -> Option<(u32, u8)> {
        let mut best: Option<(u32, u8)> = None;
        for w in 0..WORDS {
            let mut cand = (self.latch[w] | (self.level[w] & !self.edge[w])) & self.enabled[w] & !self.active[w];
            while cand != 0 {
                let b = cand.trailing_zeros();
                cand &= cand - 1;
                let irq = w as u32 * 32 + b;
                let p = self.priority[irq as usize];
                if best.map_or(true, |(_, bp)| p < bp) {
                    best = Some((irq, p));
                }
            }
        }
        best
    }

    fn update(&mut self) {
        self.irq_out = self.dist_ctlr & 1 != 0
            && self.cpu_ctlr & 1 != 0
            && match self.highest_pending() {
                Some((_, p)) => (p as u32) < self.pmr && (p as u32) < self.running_priority(),
                None => false,
            };
    }

    fn acknowledge(&mut self) -> u32 {
        if self.dist_ctlr & 1 == 0 || self.cpu_ctlr & 1 == 0 {
            return SPURIOUS;
        }
        match self.highest_pending() {
            Some((irq, p)) if (p as u32) < self.pmr && (p as u32) < self.running_priority() => {
                Self::put(&mut self.latch, irq, false);
                Self::put(&mut self.active, irq, true);
                self.running.push((irq, p));
                self.update();
                irq
            }
            _ => SPURIOUS,
        }
    }

    fn end_of_interrupt(&mut self, irq: u32) {
        let irq = irq & 0x3ff;
        if let Some(pos) = self.running.iter().rposition(|&(i, _)| i == irq) {
            self.running.remove(pos);
        }
        if (irq as usize) < NUM_IRQS {
            Self::put(&mut self.active, irq, false);
        }
        self.update();
    }

    fn bitmap_reg(&mut self, off: u64) -> Option<usize> {
        let idx = ((off & 0x7f) / 4) as usize;
        if idx < WORDS { Some(idx) } else { None }
    }

    pub fn dist_read(&mut self, off: u64, size: u32) -> u32 {
        match off {
            0x000 => self.dist_ctlr,
            0x004 => (WORDS as u32 - 1) & 0x1f, // ITLinesNumber; CPUNumber = 0
            0x008 => 0x0100_143b,
            0x080..=0x0fc => self.bitmap_reg(off).map_or(0, |i| self.group[i]),
            0x100..=0x17c | 0x180..=0x1fc => self.bitmap_reg(off).map_or(0, |i| self.enabled[i]),
            0x200..=0x27c | 0x280..=0x2fc => self.bitmap_reg(off).map_or(0, |i| {
                let mut v = 0;
                for b in 0..32 {
                    if self.is_pending(i as u32 * 32 + b) {
                        v |= 1 << b;
                    }
                }
                v
            }),
            0x300..=0x37c | 0x380..=0x3fc => self.bitmap_reg(off).map_or(0, |i| self.active[i]),
            0x400..=0x7fb => self.read_bytes(&|g: &Gic, i| g.priority.get(i).copied().unwrap_or(0), off - 0x400, size),
            // Uniprocessor GIC: ITARGETSR is RAZ/WI and everything targets CPU 0.
            0x800..=0xbfb => 0,
            0xc00..=0xcfc => {
                let base = ((off - 0xc00) / 4 * 16) as u32;
                let mut v = 0;
                for i in 0..16 {
                    let irq = base + i;
                    if (irq as usize) < NUM_IRQS && Self::get(&self.edge, irq) {
                        v |= 2 << (i * 2);
                    }
                }
                v
            }
            0xf10..=0xf2c => 0,
            0xfe8 => 0x2b, // PIDR2: ArchRev = 2
            _ => {
                eprintln!("gic: unhandled dist read {off:#x}");
                0
            }
        }
    }

    fn read_bytes(&self, f: &dyn Fn(&Gic, usize) -> u8, off: u64, size: u32) -> u32 {
        let mut v = 0;
        for i in 0..size {
            v |= (f(self, (off + i as u64) as usize) as u32) << (8 * i);
        }
        v
    }

    pub fn dist_write(&mut self, off: u64, size: u32, val: u32) {
        match off {
            0x000 => self.dist_ctlr = val & 3,
            0x080..=0x0fc => {
                if let Some(i) = self.bitmap_reg(off) {
                    self.group[i] = val
                }
            }
            0x100..=0x17c => {
                if let Some(i) = self.bitmap_reg(off) {
                    self.enabled[i] |= val
                }
            }
            0x180..=0x1fc => {
                if let Some(i) = self.bitmap_reg(off) {
                    let keep = if i == 0 { 0xffff } else { 0 };
                    self.enabled[i] &= !val | keep;
                }
            }
            0x200..=0x27c => {
                if let Some(i) = self.bitmap_reg(off) {
                    self.latch[i] |= val
                }
            }
            0x280..=0x2fc => {
                if let Some(i) = self.bitmap_reg(off) {
                    self.latch[i] &= !val
                }
            }
            0x300..=0x37c => {
                if let Some(i) = self.bitmap_reg(off) {
                    self.active[i] |= val
                }
            }
            0x380..=0x3fc => {
                if let Some(i) = self.bitmap_reg(off) {
                    self.active[i] &= !val
                }
            }
            0x400..=0x7fb => {
                for i in 0..size as u64 {
                    if let Some(p) = self.priority.get_mut((off - 0x400 + i) as usize) {
                        *p = (val >> (8 * i)) as u8;
                    }
                }
            }
            0x800..=0xbfb => {}
            0xc00..=0xcfc => {
                let base = ((off - 0xc00) / 4 * 16) as u32;
                for i in 0..16 {
                    let irq = base + i;
                    if irq >= 16 && (irq as usize) < NUM_IRQS {
                        Self::put(&mut self.edge, irq, val >> (i * 2 + 1) & 1 != 0);
                    }
                }
            }
            0xf00 => {
                // SGIR: only CPU 0 exists, so any filter that includes it pends the SGI.
                let filter = val >> 24 & 3;
                let list = val >> 16 & 0xff;
                let sgi = val & 0xf;
                if filter == 2 || (filter == 0 && list & 1 != 0) {
                    Self::put(&mut self.latch, sgi, true);
                }
            }
            0xf10..=0xf2c => {}
            _ => eprintln!("gic: unhandled dist write {off:#x} = {val:#x}"),
        }
        self.update();
    }

    pub fn cpu_read(&mut self, off: u64, _size: u32) -> u32 {
        match off {
            0x00 => self.cpu_ctlr,
            0x04 => self.pmr,
            0x08 => self.bpr,
            0x0c => self.acknowledge(),
            0x14 => self.running_priority().min(0xff),
            0x18 => self.highest_pending().map_or(SPURIOUS, |(i, _)| i),
            0x1c => self.abpr,
            0xd0..=0xdc => 0,
            0xfc => 0x0002_043b, // GICC_IIDR as QEMU
            _ => {
                eprintln!("gic: unhandled cpu read {off:#x}");
                0
            }
        }
    }

    pub fn cpu_write(&mut self, off: u64, _size: u32, val: u32) {
        match off {
            0x00 => self.cpu_ctlr = val & 0x3ff,
            0x04 => self.pmr = val & 0xff,
            0x08 => self.bpr = val & 7,
            0x10 => self.end_of_interrupt(val),
            0x1c => self.abpr = val & 7,
            0xd0..=0xdc => {}
            0x1000 => {
                let irq = val & 0x3ff;
                if (irq as usize) < NUM_IRQS {
                    Self::put(&mut self.active, irq, false);
                }
            }
            _ => eprintln!("gic: unhandled cpu write {off:#x} = {val:#x}"),
        }
        self.update();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_spi_roundtrip() {
        let mut g = Gic::new();
        g.dist_write(0, 4, 1);
        g.cpu_write(0, 4, 1);
        g.cpu_write(4, 4, 0xf0);
        g.dist_write(0x104, 4, 1 << 1); // enable INTID 33
        g.set_spi(1, true);
        assert!(g.irq_out);
        assert_eq!(g.cpu_read(0x0c, 4), 33);
        assert!(!g.irq_out); // active
        g.cpu_write(0x10, 4, 33);
        assert!(g.irq_out); // level still high -> pending again
        g.set_spi(1, false);
        assert!(!g.irq_out);
    }

    #[test]
    fn sgi_to_self() {
        let mut g = Gic::new();
        g.dist_write(0, 4, 1);
        g.cpu_write(0, 4, 1);
        g.cpu_write(4, 4, 0xf0);
        g.dist_write(0xf00, 4, 0x0001_0005);
        assert_eq!(g.cpu_read(0x0c, 4), 5);
        g.cpu_write(0x10, 4, 5);
        assert!(!g.irq_out);
    }
}
