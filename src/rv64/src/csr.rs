//! Control and status registers.

use crate::cpu::*;

/// misa: RV64 with A C D F I M S U.
pub const MISA: u64 = 2 << 62 | MISA_EXT;
/// misa for RV32.
pub const MISA32: u64 = 1 << 30 | MISA_EXT;
const MISA_EXT: u64 = 1 << 0 | 1 << 2 | 1 << 3 | 1 << 5 | 1 << 8 | 1 << 12 | 1 << 18 | 1 << 20;

/// mstatus bits M mode can write.
const MSTATUS_WMASK: u64 = MSTATUS_SIE
    | MSTATUS_MIE
    | MSTATUS_SPIE
    | MSTATUS_MPIE
    | MSTATUS_SPP
    | MSTATUS_MPP
    | MSTATUS_FS
    | MSTATUS_MPRV
    | MSTATUS_SUM
    | MSTATUS_MXR
    | MSTATUS_TVM
    | MSTATUS_TW
    | MSTATUS_TSR;
/// The bits of mstatus visible as sstatus.
const SSTATUS_MASK: u64 = MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP | MSTATUS_FS | MSTATUS_SUM | MSTATUS_MXR | MSTATUS_SD | 3 << 32;
/// UXL = SXL = 64 bits
const MSTATUS_XL: u64 = 2 << 32 | 2 << 34;

/// Exceptions M mode can delegate (not ecall from M).
// Like QEMU, cause 18 (software check) is delegable though it never occurs.
const MEDELEG_MASK: u64 = 0x4_b3ff;
const MIDELEG_MASK: u64 = MIP_SSIP | MIP_STIP | MIP_SEIP;
const MIE_MASK: u64 = MIP_SSIP | MIP_STIP | MIP_SEIP | MIP_MSIP | MIP_MTIP | MIP_MEIP;
/// mip bits software can write (MSIP, MTIP and MEIP follow the CLINT and PLIC).
const MIP_WMASK: u64 = MIP_SSIP | MIP_STIP | MIP_SEIP;

pub const SATP_MODE_BARE: u64 = 0;
pub const SATP_MODE_SV39: u64 = 8;

pub struct Csrs {
    pub mstatus: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    pub mie: u64,
    /// mip, with SEIP the PLIC's line or'ed with seip_sw.
    pub mip: u64,
    /// The SEIP bit as M mode wrote it.
    pub seip_sw: bool,
    pub mtvec: u64,
    pub mcounteren: u64,
    pub mscratch: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub menvcfg: u64,
    pub mcountinhibit: u64,
    pub stvec: u64,
    pub scounteren: u64,
    pub senvcfg: u64,
    pub sscratch: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub satp: u64,
    pub pmpcfg: [u64; 2],
    pub pmpaddr: [u64; 16],
    /// fcsr: frm (bits 7:5) and fflags (4:0).
    pub fcsr: u32,
    /// cycle/instret = icount - 1 + counter_offset: the instructions retired
    /// before the current one (icount counts it already). icount also drives
    /// time, so writes to mcycle/minstret go here.
    pub counter_offset: u64,
    /// Debug trigger select (there are no triggers: tdata1 reads 0).
    pub tselect: u64,
    /// CH32V003 (QingKe V2A) INTSYSCR: interrupt nesting and the hardware
    /// prologue/epilogue, which isn't modelled (see dev/ch32.rs).
    pub intsyscr: u64,
}

impl Default for Csrs {
    fn default() -> Self {
        Csrs {
            mstatus: MSTATUS_XL,
            medeleg: 0,
            mideleg: 0,
            mie: 0,
            mip: 0,
            seip_sw: false,
            mtvec: 0,
            mcounteren: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            menvcfg: 0,
            mcountinhibit: 0,
            stvec: 0,
            scounteren: 0,
            senvcfg: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            satp: 0,
            pmpcfg: [0; 2],
            pmpaddr: [0; 16],
            fcsr: 0,
            counter_offset: 0,
            tselect: 0,
            intsyscr: 0,
        }
    }
}

/// mstatus with SD computed from FS.
fn with_sd(s: u64) -> u64 { if s & MSTATUS_FS == MSTATUS_FS { s | MSTATUS_SD } else { s & !MSTATUS_SD } }

impl Cpu {
    fn fp_enabled(&self) -> bool { self.csr.mstatus & MSTATUS_FS != 0 }

    /// Mark the FP state dirty (after writing an FP register or fflags).
    #[inline]
    pub fn fs_dirty(&mut self) { self.csr.mstatus |= MSTATUS_FS; }

    /// Counter CSRs (cycle, time, instret, hpmcounterN) from below M mode.
    fn counter_allowed(&self, n: u32) -> bool {
        let bit = 1 << n;
        match self.prv {
            PRV_M => true,
            PRV_S => self.csr.mcounteren & bit != 0,
            _ => self.csr.mcounteren & bit != 0 && self.csr.scounteren & bit != 0,
        }
    }

    /// Read CSR `n`; Err is illegal instruction (the caller fills in tval).
    /// For RV32, the value is sign-extended (as registers hold it).
    pub fn csr_read(&mut self, n: u32) -> Result<u64, ()> {
        if !self.xlen32 {
            return self.csr_read64(n);
        }
        if (n >> 8 & 3) as u64 > self.prv {
            return Err(());
        }
        let hi = |v: u64| v >> 32;
        let v = match n {
            // mstatus, sstatus: SD is bit 31; mstatush (MBE, SBE) is 0
            0x300 | 0x100 => {
                let v = self.csr_read64(n)?;
                (v & 0x7fff_ffff) | (v >> 63) << 31
            }
            0x310 | 0x31a => 0, // mstatush, menvcfgh
            0x804 if self.bus.ch32.is_some() => self.csr.intsyscr,
            0xbc0 if self.bus.ch32.is_some() => 0x1f,
            0x301 => MISA32,
            0xc80..=0xc82 => hi(self.csr_read64(n - 0x80)?), // cycleh, timeh, instreth
            0xb80 | 0xb82 => hi(self.csr_read64(n - 0x80)?), // mcycleh, minstreth
            // pmpcfg0..3: the halves of RV64's pmpcfg0 and pmpcfg2
            0x3a0..=0x3a3 => self.csr.pmpcfg[(n as usize - 0x3a0) / 2] >> (32 * (n & 1)),
            _ => self.csr_read64(n)?,
        };
        Ok(v as i32 as i64 as u64)
    }

    /// Write CSR `n` (csr_write64 for RV64).
    pub fn csr_write(&mut self, n: u32, v: u64) -> Result<(), ()> {
        if !self.xlen32 {
            return self.csr_write64(n, v);
        }
        if (n >> 8 & 3) as u64 > self.prv || n >> 10 == 3 {
            return Err(());
        }
        let v = v & 0xffff_ffff;
        // Replace the low or high half of a 64-bit value
        let half = |old: u64, high: bool| if high { (old & 0xffff_ffff) | v << 32 } else { (old & !0xffff_ffff) | v };
        match n {
            0x300 | 0x100 => self.csr_write64(n, v & 0x7fff_ffff),
            0x310 | 0x31a => Ok(()),
            0x804 if self.bus.ch32.is_some() => {
                self.csr.intsyscr = v & 0x3f;
                Ok(())
            }
            // QingKe V4 CORECFGR (pipeline and prediction settings)
            0xbc0 if self.bus.ch32.is_some() => Ok(()),
            // mtvec: QingKe's vector table modes use both low bits
            0x305 if self.bus.ch32.is_some() => {
                self.csr.mtvec = v;
                Ok(())
            }
            0x301 => Ok(()),
            0x180 => {
                if self.prv == PRV_S && self.csr.mstatus & MSTATUS_TVM != 0 {
                    return Err(());
                }
                // Bare or Sv32
                self.csr.satp = v;
                self.tlb.flush();
                self.icache_flush();
                Ok(())
            }
            0xb00 | 0xb02 | 0xb80 | 0xb82 => {
                // As csr_write64: the next instruction reads the new value
                let old = self.icount.wrapping_sub(1).wrapping_add(self.csr.counter_offset);
                self.csr.counter_offset = half(old, n >= 0xb80).wrapping_sub(self.icount);
                Ok(())
            }
            0x3a0..=0x3a3 => {
                let k = (n as usize - 0x3a0) / 2;
                self.csr.pmpcfg[k] = half(self.csr.pmpcfg[k], n & 1 != 0);
                Ok(())
            }
            _ => self.csr_write64(n, v),
        }
    }

    /// csr_read() for RV64.
    fn csr_read64(&mut self, n: u32) -> Result<u64, ()> {
        if (n >> 8 & 3) as u64 > self.prv {
            return Err(());
        }
        let c = &self.csr;
        Ok(match n {
            0x001..=0x003 => {
                if !self.fp_enabled() {
                    return Err(());
                }
                match n {
                    1 => (c.fcsr & 0x1f) as u64,
                    2 => (c.fcsr >> 5 & 7) as u64,
                    _ => (c.fcsr & 0xff) as u64,
                }
            }
            0xc00..=0xc02 => {
                let k = n - 0xc00;
                if !self.counter_allowed(k) {
                    return Err(());
                }
                match k {
                    0 | 2 => self.icount.wrapping_sub(1).wrapping_add(c.counter_offset),
                    1 => {
                        self.bus.now_ns = self.now_ns();
                        self.bus.mtime()
                    }
                    _ => unreachable!(),
                }
            }
            // Supervisor
            0x100 => with_sd(c.mstatus) & SSTATUS_MASK,
            0x104 => c.mie & c.mideleg,
            0x105 => c.stvec,
            0x106 => c.scounteren,
            0x10a => c.senvcfg,
            0x140 => c.sscratch,
            0x141 => c.sepc,
            0x142 => c.scause,
            0x143 => c.stval,
            0x144 => c.mip & c.mideleg,
            0x180 => {
                if self.prv == PRV_S && c.mstatus & MSTATUS_TVM != 0 {
                    return Err(());
                }
                c.satp
            }
            // Machine
            0xf11..=0xf14 => 0, // mvendorid, marchid, mimpid, mhartid
            0xf15 => 0,         // mconfigptr
            0x300 => with_sd(c.mstatus),
            0x301 => MISA,
            0x302 => c.medeleg,
            0x303 => c.mideleg,
            0x304 => c.mie,
            0x305 => c.mtvec,
            0x306 => c.mcounteren,
            0x30a => c.menvcfg,
            0x320 => c.mcountinhibit,
            0x340 => c.mscratch,
            0x341 => c.mepc,
            0x342 => c.mcause,
            0x343 => c.mtval,
            0x344 => c.mip,
            0x3a0 => c.pmpcfg[0],
            0x3a2 => c.pmpcfg[1],
            0x3b0..=0x3bf => c.pmpaddr[(n - 0x3b0) as usize],
            0x3c0..=0x3ef => 0,
            0xb00 | 0xb02 => self.icount.wrapping_sub(1).wrapping_add(c.counter_offset),
            0x7a0 => c.tselect,
            0x7a1..=0x7a3 => 0, // tdata1..3: no trigger
            0x7a4 => 1,         // tinfo: type 0 only
            _ => return Err(()),
        })
    }

    /// Write CSR `n`; Err is illegal instruction.
    fn csr_write64(&mut self, n: u32, v: u64) -> Result<(), ()> {
        if (n >> 8 & 3) as u64 > self.prv || n >> 10 == 3 {
            return Err(());
        }
        let c = &mut self.csr;
        match n {
            0x001..=0x003 => {
                if c.mstatus & MSTATUS_FS == 0 {
                    return Err(());
                }
                c.fcsr = match n {
                    1 => (c.fcsr & !0x1f) | (v as u32 & 0x1f),
                    2 => (c.fcsr & !0xe0) | (v as u32 & 7) << 5,
                    _ => v as u32 & 0xff,
                };
                c.mstatus |= MSTATUS_FS;
            }
            0x100 => {
                let m = SSTATUS_MASK & MSTATUS_WMASK;
                c.mstatus = (c.mstatus & !m) | (v & m);
                self.stop_batch = true;
            }
            0x104 => {
                let m = c.mideleg & MIE_MASK;
                c.mie = (c.mie & !m) | (v & m);
                self.stop_batch = true;
            }
            0x105 => c.stvec = v & !2,
            0x106 => c.scounteren = v & 0xffff_ffff,
            0x10a => c.senvcfg = v & 1,
            0x140 => c.sscratch = v,
            0x141 => c.sepc = v & !1,
            0x142 => c.scause = v,
            0x143 => c.stval = v,
            0x144 => {
                // Only SSIP, and only when delegated
                let m = c.mideleg & MIP_SSIP;
                c.mip = (c.mip & !m) | (v & m);
                self.stop_batch = true;
            }
            0x180 => {
                if self.prv == PRV_S && c.mstatus & MSTATUS_TVM != 0 {
                    return Err(());
                }
                // WARL: a mode other than Bare and Sv39 leaves satp as it is
                let mode = v >> 60;
                if mode == SATP_MODE_BARE || mode == SATP_MODE_SV39 {
                    c.satp = v;
                    self.tlb.flush();
                    self.icache_flush();
                }
            }
            0x300 => {
                let mut s = (c.mstatus & !MSTATUS_WMASK) | (v & MSTATUS_WMASK);
                // MPP can't hold the reserved value 2
                if (s & MSTATUS_MPP) >> MSTATUS_MPP_SHIFT == 2 {
                    s = (s & !MSTATUS_MPP) | (c.mstatus & MSTATUS_MPP);
                }
                c.mstatus = s;
                self.stop_batch = true;
            }
            0x301 => {} // misa is fixed
            0x302 => c.medeleg = v & MEDELEG_MASK,
            0x303 => {
                c.mideleg = v & MIDELEG_MASK;
                self.stop_batch = true;
            }
            0x304 => {
                c.mie = v & MIE_MASK;
                self.stop_batch = true;
            }
            0x305 => c.mtvec = v & !2,
            0x306 => c.mcounteren = v & 0xffff_ffff,
            0x30a => c.menvcfg = v & 1,
            0x320 => c.mcountinhibit = v & 5, // CY, IR (no hpm counters)
            0x340 => c.mscratch = v,
            0x341 => c.mepc = v & !1,
            0x342 => c.mcause = v,
            0x343 => c.mtval = v,
            0x344 => {
                c.seip_sw = v & MIP_SEIP != 0;
                c.mip = (c.mip & !(MIP_SSIP | MIP_STIP)) | (v & (MIP_WMASK & !MIP_SEIP));
                self.update_mip();
            }
            0x3a0 => c.pmpcfg[0] = v,
            0x3a2 => c.pmpcfg[1] = v,
            0x3b0..=0x3bf => c.pmpaddr[(n - 0x3b0) as usize] = v & ((1 << 54) - 1),
            0x3c0..=0x3ef => {}
            // The next instruction reads v (the write's own increment is suppressed)
            0xb00 | 0xb02 => c.counter_offset = v.wrapping_sub(self.icount),
            0x7a0 => c.tselect = v,
            0x7a1..=0x7a3 => {}
            _ => return Err(()),
        }
        Ok(())
    }
}
