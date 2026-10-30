//! virtio-blk backed by an in-memory copy of a disk image. Writes stay in
//! memory (like QEMU's -snapshot).

use super::virtio::{gather, scatter, Queue, VirtioDevice};
use crate::bus::DevCtx;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const F_SEG_MAX: u64 = 1 << 2;
const F_GEOMETRY: u64 = 1 << 4;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
const F_TOPOLOGY: u64 = 1 << 10;
const F_CONFIG_WCE: u64 = 1 << 11;
const F_DISCARD: u64 = 1 << 13;
const F_WRITE_ZEROES: u64 = 1 << 14;

/// Max sectors per discard / write-zeroes request (QEMU's default).
const MAX_ZERO_SECTORS: u32 = 0x3f_ffff;

pub struct VirtioBlk {
    disk: Vec<u8>,
    writeback: u8,
}

impl VirtioBlk {
    pub fn new(disk: Vec<u8>) -> VirtioBlk { VirtioBlk { disk, writeback: 1 } }

    fn handle(&mut self, ctx: &mut DevCtx, segs: &[super::virtio::Seg]) -> u32 {
        let mut hdr = Vec::with_capacity(16);
        gather(ctx, segs, 0, &mut hdr, 16);
        let Some(status_seg) = segs.iter().rev().find(|s| s.write) else { return 0 };
        let status_addr = status_seg.addr + status_seg.len as u64 - 1;
        let write_status = |ctx: &mut DevCtx, s: u8| {
            if let Some(m) = ctx.ram_slice(status_addr, 1) {
                m[0] = s;
            }
        };
        if hdr.len() < 16 {
            write_status(ctx, S_IOERR);
            return 1;
        }
        let ty = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let off = (sector * 512) as usize;
        // Data segments exclude the trailing status byte.
        let mut data_segs: Vec<_> = segs.to_vec();
        if let Some(last) = data_segs.iter_mut().rev().find(|s| s.write) {
            last.len -= 1;
        }
        match ty {
            T_IN => {
                let want: usize = data_segs.iter().filter(|s| s.write).map(|s| s.len as usize).sum();
                if off + want > self.disk.len() {
                    write_status(ctx, S_IOERR);
                    return 1;
                }
                let n = scatter(ctx, &data_segs, &self.disk[off..off + want]);
                write_status(ctx, S_OK);
                n as u32 + 1
            }
            T_OUT => {
                let mut buf = Vec::new();
                gather(ctx, &data_segs, 16, &mut buf, usize::MAX);
                if off + buf.len() > self.disk.len() {
                    write_status(ctx, S_IOERR);
                    return 1;
                }
                self.disk[off..off + buf.len()].copy_from_slice(&buf);
                write_status(ctx, S_OK);
                1
            }
            T_FLUSH => {
                write_status(ctx, S_OK);
                1
            }
            T_DISCARD | T_WRITE_ZEROES => {
                // Payload: { sector: u64, num_sectors: u32, flags: u32 }*
                let mut buf = Vec::new();
                gather(ctx, &data_segs, 16, &mut buf, usize::MAX);
                for r in buf.as_chunks::<16>().0 {
                    let s = u64::from_le_bytes(r[0..8].try_into().unwrap()) as usize * 512;
                    let n = u32::from_le_bytes(r[8..12].try_into().unwrap()) as usize * 512;
                    if s + n > self.disk.len() {
                        write_status(ctx, S_IOERR);
                        return 1;
                    }
                    // Discarded data may read back as anything; zero both.
                    self.disk[s..s + n].fill(0);
                }
                write_status(ctx, S_OK);
                1
            }
            T_GET_ID => {
                let mut id = [0u8; 20];
                id[..9].copy_from_slice(b"v86-a64-0");
                let n = scatter(ctx, &data_segs, &id);
                write_status(ctx, S_OK);
                n as u32 + 1
            }
            _ => {
                write_status(ctx, S_UNSUPP);
                1
            }
        }
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_id(&self) -> u32 { 2 }

    // Same feature set QEMU offers, so the two stay comparable in lockstep.
    fn features(&self) -> u64 {
        F_SEG_MAX
            | F_GEOMETRY
            | F_BLK_SIZE
            | F_FLUSH
            | F_TOPOLOGY
            | F_CONFIG_WCE
            | F_DISCARD
            | F_WRITE_ZEROES
            | super::virtio::VIRTIO_F_RING_INDIRECT_DESC
            | super::virtio::VIRTIO_F_RING_EVENT_IDX
    }

    fn num_queues(&self) -> usize { 1 }

    fn config_read(&mut self, off: u64, size: u32) -> u64 {
        let mut cfg = [0u8; 0x40];
        cfg[0..8].copy_from_slice(&((self.disk.len() / 512) as u64).to_le_bytes());
        cfg[0x0c..0x10].copy_from_slice(&254u32.to_le_bytes()); // seg_max (queue size - 2)
        cfg[0x14..0x18].copy_from_slice(&512u32.to_le_bytes()); // blk_size
        cfg[0x20] = self.writeback;
        cfg[0x24..0x28].copy_from_slice(&MAX_ZERO_SECTORS.to_le_bytes()); // max_discard_sectors
        cfg[0x28..0x2c].copy_from_slice(&1u32.to_le_bytes()); // max_discard_seg
        cfg[0x2c..0x30].copy_from_slice(&1u32.to_le_bytes()); // discard_sector_alignment
        cfg[0x30..0x34].copy_from_slice(&MAX_ZERO_SECTORS.to_le_bytes()); // max_write_zeroes_sectors
        cfg[0x34..0x38].copy_from_slice(&1u32.to_le_bytes()); // max_write_zeroes_seg
        let off = off as usize;
        if off + size as usize > cfg.len() {
            return 0;
        }
        crate::bus::read_le(&cfg[off..], size)
    }

    fn config_write(&mut self, off: u64, _size: u32, v: u64) {
        if off == 0x20 {
            self.writeback = v as u8 & 1;
        }
    }

    fn notify(&mut self, q: usize, queues: &mut [Queue], ctx: &mut DevCtx) -> bool {
        let mut used = false;
        while let Some((head, segs)) = queues[q].pop(ctx) {
            let len = self.handle(ctx, &segs);
            queues[q].push_used(ctx, head, len);
            used = true;
        }
        used
    }
}
