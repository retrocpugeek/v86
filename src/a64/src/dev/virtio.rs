//! virtio-mmio transport (version 2, "modern") with split virtqueues.

use crate::bus::{read_le, write_le, DevCtx, Device};

pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// Offered by QEMU's transports; only matters if the driver resets a single queue.
pub const VIRTIO_F_RING_RESET: u64 = 1 << 40;
pub const VIRTIO_F_RING_INDIRECT_DESC: u64 = 1 << 28;
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << 29;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;

/// One buffer segment of a descriptor chain.
#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub addr: u64,
    pub len: u32,
    pub write: bool,
}

#[derive(Default, Clone)]
pub struct Queue {
    pub num: u32,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    last_avail: u16,
    /// VIRTIO_F_RING_EVENT_IDX negotiated.
    pub event_idx: bool,
}

fn rd(ctx: &mut DevCtx, pa: u64, size: u32) -> u64 {
    ctx.ram_slice(pa, size as usize).map_or(0, |s| read_le(s, size))
}

fn wr(ctx: &mut DevCtx, pa: u64, size: u32, v: u64) {
    if let Some(s) = ctx.ram_slice(pa, size as usize) {
        write_le(s, size, v);
    }
}

impl Queue {
    /// Next available descriptor chain: (head index, segments).
    pub fn pop(&mut self, ctx: &mut DevCtx) -> Option<(u16, Vec<Seg>)> {
        if !self.ready || self.num == 0 {
            return None;
        }
        let avail_idx = rd(ctx, self.avail + 2, 2) as u16;
        if avail_idx == self.last_avail {
            return None;
        }
        let slot = self.last_avail as u64 % self.num as u64;
        let head = rd(ctx, self.avail + 4 + 2 * slot, 2) as u16;
        self.last_avail = self.last_avail.wrapping_add(1);
        let mut segs = Vec::new();
        let (mut table, mut table_len) = (self.desc, self.num);
        let mut i = head;
        for _ in 0..1024 {
            if i as u32 >= table_len {
                break;
            }
            let d = table + 16 * i as u64;
            let addr = rd(ctx, d, 8);
            let len = rd(ctx, d + 8, 4) as u32;
            let flags = rd(ctx, d + 12, 2) as u16;
            let next = rd(ctx, d + 14, 2) as u16;
            if flags & DESC_F_INDIRECT != 0 {
                table = addr;
                table_len = len / 16;
                i = 0;
                continue;
            }
            segs.push(Seg { addr, len, write: flags & DESC_F_WRITE != 0 });
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            i = next;
        }
        Some((head, segs))
    }

    pub fn used_idx(&self, ctx: &mut DevCtx) -> u16 { rd(ctx, self.used + 2, 2) as u16 }

    /// With EVENT_IDX, ask to be notified for anything past the current avail
    /// index (as QEMU's virtio_queue_set_notification(vq, 1) does).
    pub fn set_avail_event(&mut self, ctx: &mut DevCtx) {
        if self.event_idx && self.ready && self.num != 0 {
            let avail_idx = rd(ctx, self.avail + 2, 2);
            wr(ctx, self.used + 4 + 8 * self.num as u64, 2, avail_idx);
        }
    }

    /// Whether the driver wants an interrupt for used entries old..new.
    pub fn need_interrupt(&self, ctx: &mut DevCtx, old: u16, new: u16) -> bool {
        if !self.event_idx {
            // VIRTQ_AVAIL_F_NO_INTERRUPT
            return rd(ctx, self.avail, 2) & 1 == 0;
        }
        let event = rd(ctx, self.avail + 4 + 2 * self.num as u64, 2) as u16;
        new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
    }

    pub fn push_used(&mut self, ctx: &mut DevCtx, head: u16, len: u32) {
        let idx = rd(ctx, self.used + 2, 2) as u16;
        let slot = idx as u64 % self.num as u64;
        wr(ctx, self.used + 4 + 8 * slot, 4, head as u64);
        wr(ctx, self.used + 8 + 8 * slot, 4, len as u64);
        wr(ctx, self.used + 2, 2, idx.wrapping_add(1) as u64);
    }
}

/// Copy guest memory described by `segs` (device-readable ones) starting at `skip` bytes.
pub fn gather(ctx: &mut DevCtx, segs: &[Seg], mut skip: usize, out: &mut Vec<u8>, max: usize) {
    for s in segs.iter().filter(|s| !s.write) {
        let len = s.len as usize;
        if skip >= len {
            skip -= len;
            continue;
        }
        let take = (len - skip).min(max - out.len());
        if let Some(m) = ctx.ram_slice(s.addr + skip as u64, take) {
            out.extend_from_slice(m);
        }
        skip = 0;
        if out.len() >= max {
            break;
        }
    }
}

/// Copy `data` into the device-writable segments; returns bytes written.
pub fn scatter(ctx: &mut DevCtx, segs: &[Seg], data: &[u8]) -> usize {
    let mut off = 0;
    for s in segs.iter().filter(|s| s.write) {
        if off >= data.len() {
            break;
        }
        let take = (s.len as usize).min(data.len() - off);
        if let Some(m) = ctx.ram_slice(s.addr, take) {
            m.copy_from_slice(&data[off..off + take]);
        }
        off += take;
    }
    off
}

pub trait VirtioDevice {
    fn device_id(&self) -> u32;
    fn features(&self) -> u64;
    fn num_queues(&self) -> usize;
    fn queue_max(&self) -> u32 { 1024 }
    fn config_read(&mut self, _off: u64, _size: u32) -> u64 { 0 }
    fn config_write(&mut self, _off: u64, _size: u32, _v: u64) {}
    /// Process available buffers on queue `q`. Returns true if any were used.
    fn notify(&mut self, q: usize, queues: &mut [Queue], ctx: &mut DevCtx) -> bool;
    /// Called periodically (e.g. to deliver received packets).
    fn poll(&mut self, _queues: &mut [Queue], _ctx: &mut DevCtx) -> bool { false }
    fn reset(&mut self) {}
}

/// A slot with no device behind it (DeviceID 0), like QEMU's unused transports.
pub struct Empty;

impl VirtioDevice for Empty {
    fn device_id(&self) -> u32 { 0 }
    fn features(&self) -> u64 { 0 }
    fn num_queues(&self) -> usize { 0 }
    fn notify(&mut self, _: usize, _: &mut [Queue], _: &mut DevCtx) -> bool { false }
}

pub struct VirtioMmio {
    spi: u32,
    dev: Box<dyn VirtioDevice>,
    queues: Vec<Queue>,
    queue_sel: u32,
    dev_feat_sel: u32,
    drv_feat_sel: u32,
    driver_features: u64,
    status: u32,
    int_status: u32,
    config_gen: u32,
}

impl VirtioMmio {
    pub fn new(spi: u32, dev: Box<dyn VirtioDevice>) -> VirtioMmio {
        let nq = dev.num_queues();
        VirtioMmio {
            spi,
            dev,
            queues: vec![Queue::default(); nq],
            queue_sel: 0,
            dev_feat_sel: 0,
            drv_feat_sel: 0,
            driver_features: 0,
            status: 0,
            int_status: 0,
            config_gen: 0,
        }
    }

    fn reset(&mut self) {
        for q in &mut self.queues {
            *q = Queue::default();
        }
        self.queue_sel = 0;
        self.driver_features = 0;
        self.status = 0;
        self.int_status = 0;
        self.dev.reset();
    }

    fn raise(&mut self, ctx: &mut DevCtx) {
        self.int_status |= 1;
        ctx.gic.set_spi(self.spi, true);
    }

    fn queue(&mut self) -> Option<&mut Queue> { self.queues.get_mut(self.queue_sel as usize) }
}

fn set_lo(x: &mut u64, v: u32) { *x = (*x & !0xffff_ffff) | v as u64 }
fn set_hi(x: &mut u64, v: u32) { *x = (*x & 0xffff_ffff) | (v as u64) << 32 }

impl Device for VirtioMmio {
    fn name(&self) -> &str { "virtio-mmio" }

    fn read(&mut self, off: u64, size: u32, _ctx: &mut DevCtx) -> u64 {
        if off >= 0x100 {
            return self.dev.config_read(off - 0x100, size);
        }
        let feats = self.dev.features()
            | if self.dev.device_id() != 0 { VIRTIO_F_VERSION_1 | VIRTIO_F_RING_RESET } else { 0 };
        (match off {
            0x000 => 0x7472_6976,
            0x004 => 2,
            0x008 => self.dev.device_id(),
            0x00c => 0x554d_4551,
            0x010 => match self.dev_feat_sel {
                0 => feats as u32,
                1 => (feats >> 32) as u32,
                _ => 0,
            },
            0x034 => {
                let max = self.dev.queue_max();
                self.queue().map_or(0, |_| max)
            }
            0x044 => self.queue().map_or(0, |q| q.ready as u32),
            0x060 => self.int_status,
            0x070 => self.status,
            0x0fc => self.config_gen,
            _ => 0,
        }) as u64
    }

    fn write(&mut self, off: u64, size: u32, v: u64, ctx: &mut DevCtx) {
        if off >= 0x100 {
            self.dev.config_write(off - 0x100, size, v);
            return;
        }
        let v32 = v as u32;
        match off {
            0x014 => self.dev_feat_sel = v32,
            0x020 => match self.drv_feat_sel {
                0 => set_lo(&mut self.driver_features, v32),
                1 => set_hi(&mut self.driver_features, v32),
                _ => {}
            },
            0x024 => self.drv_feat_sel = v32,
            0x030 => self.queue_sel = v32,
            0x038 => {
                if let Some(q) = self.queue() {
                    q.num = v32
                }
            }
            0x044 => {
                if let Some(q) = self.queue() {
                    q.ready = v32 & 1 != 0
                }
            }
            0x050 => {
                let qi = v32 as usize;
                if qi < self.queues.len() {
                    let old = self.queues[qi].used_idx(ctx);
                    let used = self.dev.notify(qi, &mut self.queues, ctx);
                    self.queues[qi].set_avail_event(ctx);
                    let new = self.queues[qi].used_idx(ctx);
                    if used && self.queues[qi].need_interrupt(ctx, old, new) {
                        self.raise(ctx);
                    }
                }
            }
            0x064 => {
                self.int_status &= !v32;
                if self.int_status == 0 {
                    ctx.gic.set_spi(self.spi, false);
                }
            }
            0x070 => {
                if v32 == 0 {
                    self.reset();
                    ctx.gic.set_spi(self.spi, false);
                } else {
                    self.status = v32;
                    let event_idx = self.driver_features & VIRTIO_F_RING_EVENT_IDX != 0;
                    for q in &mut self.queues {
                        q.event_idx = event_idx;
                    }
                }
            }
            0x080 => self.queue().map_or((), |q| set_lo(&mut q.desc, v32)),
            0x084 => self.queue().map_or((), |q| set_hi(&mut q.desc, v32)),
            0x090 => self.queue().map_or((), |q| set_lo(&mut q.avail, v32)),
            0x094 => self.queue().map_or((), |q| set_hi(&mut q.avail, v32)),
            0x0a0 => self.queue().map_or((), |q| set_lo(&mut q.used, v32)),
            0x0a4 => self.queue().map_or((), |q| set_hi(&mut q.used, v32)),
            _ => {}
        }
    }

    fn poll(&mut self, ctx: &mut DevCtx) {
        if self.status & 4 == 0 {
            return;
        }
        let old: Vec<u16> = self.queues.iter().map(|q| if q.ready { q.used_idx(ctx) } else { 0 }).collect();
        if self.dev.poll(&mut self.queues, ctx) {
            let mut want = false;
            for (i, q) in self.queues.iter().enumerate() {
                if q.ready {
                    let new = q.used_idx(ctx);
                    want |= new != old[i] && q.need_interrupt(ctx, old[i], new);
                }
            }
            if want {
                self.raise(ctx);
            }
        }
    }
}
