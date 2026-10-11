//! System registers (MRS/MSR). ID values mirror QEMU's `-cpu cortex-a53`.

use std::collections::HashMap;

use crate::cpu::{Cpu, CNTFRQ};
use crate::exc::{ec, undef, Exception, Res};

/// Pack an encoding the same way Linux's sys_reg() does.
pub const fn enc(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

pub const MIDR_EL1: u32 = enc(3, 0, 0, 0, 0);
pub const MPIDR_EL1: u32 = enc(3, 0, 0, 0, 5);
pub const REVIDR_EL1: u32 = enc(3, 0, 0, 0, 6);
pub const ID_AA64PFR0_EL1: u32 = enc(3, 0, 0, 4, 0);
pub const ID_AA64DFR0_EL1: u32 = enc(3, 0, 0, 5, 0);
pub const ID_AA64ISAR0_EL1: u32 = enc(3, 0, 0, 6, 0);
pub const ID_AA64MMFR0_EL1: u32 = enc(3, 0, 0, 7, 0);
pub const CCSIDR_EL1: u32 = enc(3, 1, 0, 0, 0);
pub const CLIDR_EL1: u32 = enc(3, 1, 0, 0, 1);
pub const AIDR_EL1: u32 = enc(3, 1, 0, 0, 7);
pub const CSSELR_EL1: u32 = enc(3, 2, 0, 0, 0);
pub const CTR_EL0: u32 = enc(3, 3, 0, 0, 1);
pub const DCZID_EL0: u32 = enc(3, 3, 0, 0, 7);
pub const SCTLR_EL1: u32 = enc(3, 0, 1, 0, 0);
pub const ACTLR_EL1: u32 = enc(3, 0, 1, 0, 1);
pub const CPACR_EL1: u32 = enc(3, 0, 1, 0, 2);
pub const TTBR0_EL1: u32 = enc(3, 0, 2, 0, 0);
pub const TTBR1_EL1: u32 = enc(3, 0, 2, 0, 1);
pub const TCR_EL1: u32 = enc(3, 0, 2, 0, 2);
pub const SPSR_EL1: u32 = enc(3, 0, 4, 0, 0);
pub const ELR_EL1: u32 = enc(3, 0, 4, 0, 1);
pub const SP_EL0: u32 = enc(3, 0, 4, 1, 0);
pub const SPSEL: u32 = enc(3, 0, 4, 2, 0);
pub const CURRENTEL: u32 = enc(3, 0, 4, 2, 2);
pub const NZCV: u32 = enc(3, 3, 4, 2, 0);
pub const DAIF: u32 = enc(3, 3, 4, 2, 1);
pub const FPCR: u32 = enc(3, 3, 4, 4, 0);
pub const FPSR: u32 = enc(3, 3, 4, 4, 1);
pub const AFSR0_EL1: u32 = enc(3, 0, 5, 1, 0);
pub const AFSR1_EL1: u32 = enc(3, 0, 5, 1, 1);
pub const ESR_EL1: u32 = enc(3, 0, 5, 2, 0);
pub const FAR_EL1: u32 = enc(3, 0, 6, 0, 0);
pub const PAR_EL1: u32 = enc(3, 0, 7, 4, 0);
pub const MAIR_EL1: u32 = enc(3, 0, 10, 2, 0);
pub const AMAIR_EL1: u32 = enc(3, 0, 10, 3, 0);
pub const VBAR_EL1: u32 = enc(3, 0, 12, 0, 0);
pub const ISR_EL1: u32 = enc(3, 0, 12, 1, 0);
pub const CONTEXTIDR_EL1: u32 = enc(3, 0, 13, 0, 1);
pub const TPIDR_EL1: u32 = enc(3, 0, 13, 0, 4);
pub const TPIDR_EL0: u32 = enc(3, 3, 13, 0, 2);
pub const TPIDRRO_EL0: u32 = enc(3, 3, 13, 0, 3);
pub const CNTKCTL_EL1: u32 = enc(3, 0, 14, 1, 0);
pub const CNTFRQ_EL0: u32 = enc(3, 3, 14, 0, 0);
pub const CNTPCT_EL0: u32 = enc(3, 3, 14, 0, 1);
pub const CNTVCT_EL0: u32 = enc(3, 3, 14, 0, 2);
pub const CNTP_TVAL_EL0: u32 = enc(3, 3, 14, 2, 0);
pub const CNTP_CTL_EL0: u32 = enc(3, 3, 14, 2, 1);
pub const CNTP_CVAL_EL0: u32 = enc(3, 3, 14, 2, 2);
pub const CNTV_TVAL_EL0: u32 = enc(3, 3, 14, 3, 0);
pub const CNTV_CTL_EL0: u32 = enc(3, 3, 14, 3, 1);
pub const CNTV_CVAL_EL0: u32 = enc(3, 3, 14, 3, 2);
pub const PMCR_EL0: u32 = enc(3, 3, 9, 12, 0);
pub const PMUSERENR_EL0: u32 = enc(3, 3, 9, 14, 0);
pub const MDSCR_EL1: u32 = enc(2, 0, 0, 2, 2);
pub const OSLAR_EL1: u32 = enc(2, 0, 1, 0, 4);
pub const OSLSR_EL1: u32 = enc(2, 0, 1, 1, 4);
pub const OSDLR_EL1: u32 = enc(2, 0, 1, 3, 4);
pub const MDCCSR_EL0: u32 = enc(2, 3, 0, 1, 0);

/// Fixed ID register values for cortex-a53 as QEMU presents it at EL1 (no EL2/EL3).
/// Values captured with tools/a64-ref/idprobe.
fn id_value(e: u32) -> Option<u64> {
    Some(match e {
        MIDR_EL1 => 0x410f_d034,
        MPIDR_EL1 => 0x8000_0000,
        REVIDR_EL1 => 0x100,
        // AArch32 ID registers (EL0 AArch32 is advertised but never entered).
        x if x == enc(3, 0, 0, 1, 0) => 0x0000_0131,
        x if x == enc(3, 0, 0, 1, 1) => 0x0001_0001,
        x if x == enc(3, 0, 0, 1, 2) => 0x0300_0006,
        x if x == enc(3, 0, 0, 1, 3) => 0,
        x if x == enc(3, 0, 0, 1, 4) => 0x1010_1105,
        x if x == enc(3, 0, 0, 1, 5) => 0x4000_0000,
        x if x == enc(3, 0, 0, 1, 6) => 0x0126_0000,
        x if x == enc(3, 0, 0, 1, 7) => 0x0210_2211,
        x if x == enc(3, 0, 0, 2, 0) => 0x0210_1110,
        x if x == enc(3, 0, 0, 2, 1) => 0x1311_2111,
        x if x == enc(3, 0, 0, 2, 2) => 0x2123_2042,
        x if x == enc(3, 0, 0, 2, 3) => 0x0111_2131,
        x if x == enc(3, 0, 0, 2, 4) => 0x0001_1142,
        x if x == enc(3, 0, 0, 2, 5) => 0x0001_1121,
        x if x == enc(3, 0, 0, 3, 0) => 0x1011_0222,
        x if x == enc(3, 0, 0, 3, 1) => 0x1211_1111,
        x if x == enc(3, 0, 0, 3, 2) => 0x0000_0043,
        // EL0/EL1 AArch64+AArch32, EL2/EL3 absent, FP and AdvSIMD present.
        ID_AA64PFR0_EL1 => 0x0000_0022,
        ID_AA64DFR0_EL1 => 0x1030_5106,
        ID_AA64ISAR0_EL1 => 0x0001_1120,
        ID_AA64MMFR0_EL1 => 0x0000_1122,
        CLIDR_EL1 => 0x0a20_0023,
        AIDR_EL1 => 0,
        CTR_EL0 => 0x8444_8004,
        DCZID_EL0 => 4,
        // Remaining ID space (op0=3, op1=0, CRn=0, CRm=1..7) reads as zero.
        x if x >> 7 == enc(3, 0, 0, 0, 0) >> 7 && (x >> 3 & 0xf) >= 1 => 0,
        _ => return None,
    })
}

pub struct SysRegs {
    pub sctlr_el1: u64,
    pub actlr_el1: u64,
    pub cpacr_el1: u64,
    pub ttbr0_el1: u64,
    pub ttbr1_el1: u64,
    pub tcr_el1: u64,
    pub spsr_el1: u64,
    pub elr_el1: u64,
    pub esr_el1: u64,
    pub far_el1: u64,
    pub par_el1: u64,
    pub mair_el1: u64,
    pub amair_el1: u64,
    pub vbar_el1: u64,
    pub contextidr_el1: u64,
    pub tpidr_el1: u64,
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    pub cntkctl_el1: u64,
    pub cntv_ctl: u64,
    pub cntv_cval: u64,
    pub cntp_ctl: u64,
    pub cntp_cval: u64,
    pub csselr_el1: u64,
    pub mdscr_el1: u64,
    pub afsr0_el1: u64,
    pub afsr1_el1: u64,
    /// Debug/PMU registers modelled as plain storage.
    pub misc: HashMap<u32, u64>,
}

impl Default for SysRegs {
    fn default() -> Self { Self::new() }
}

impl SysRegs {
    pub fn new() -> SysRegs {
        SysRegs {
            sctlr_el1: 0x00c5_0838,
            actlr_el1: 0,
            cpacr_el1: 0,
            ttbr0_el1: 0,
            ttbr1_el1: 0,
            tcr_el1: 0,
            spsr_el1: 0,
            elr_el1: 0,
            esr_el1: 0,
            far_el1: 0,
            par_el1: 0,
            mair_el1: 0,
            amair_el1: 0,
            vbar_el1: 0,
            contextidr_el1: 0,
            tpidr_el1: 0,
            tpidr_el0: 0,
            tpidrro_el0: 0,
            cntkctl_el1: 0,
            cntv_ctl: 0,
            cntv_cval: 0,
            cntp_ctl: 0,
            cntp_cval: 0,
            csselr_el1: 0,
            mdscr_el1: 0,
            afsr0_el1: 0,
            afsr1_el1: 0,
            misc: HashMap::new(),
        }
    }
}

/// Debug and PMU registers that are accepted as storage at EL1.
fn is_storage_reg(e: u32) -> bool {
    let op0 = e >> 14;
    let op1 = e >> 11 & 7;
    let crn = e >> 7 & 0xf;
    if op0 == 2 {
        // Debug: breakpoints/watchpoints, MDSCR, OS lock, claim tags, etc.
        return true;
    }
    // PMU: CRn=9 CRm=12..14 and the event counters CRn=14 CRm=8..15 (op1=3).
    op0 == 3 && ((crn == 9 && (12..=14).contains(&(e >> 3 & 0xf))) || (op1 == 3 && crn == 14 && (e >> 3 & 0xf) >= 8))
}

impl Cpu {
    fn sysreg_trap(&self, e: u32, rt: u32, read: bool) -> Exception {
        let op0 = e >> 14;
        let op1 = e >> 11 & 7;
        let crn = e >> 7 & 0xf;
        let crm = e >> 3 & 0xf;
        let op2 = e & 7;
        Exception::new(
            ec::SYSREG,
            (op0 << 20) | (op2 << 17) | (op1 << 14) | (crn << 10) | (rt << 5) | (crm << 1) | read as u32,
        )
    }

    /// EL0 access check. Ok(()) = allowed, Err = exception to raise.
    fn el0_check(&self, e: u32, rt: u32, read: bool) -> Res<()> {
        let sctlr = self.sr.sctlr_el1;
        let k = self.sr.cntkctl_el1;
        let trap = || Err(self.sysreg_trap(e, rt, read));
        match e {
            NZCV | TPIDR_EL0 => Ok(()),
            FPCR | FPSR => self.check_fp(),
            TPIDRRO_EL0 if read => Ok(()),
            DAIF => if sctlr >> 9 & 1 != 0 { Ok(()) } else { trap() },
            CTR_EL0 if read => if sctlr >> 15 & 1 != 0 { Ok(()) } else { trap() },
            DCZID_EL0 if read => Ok(()),
            CNTFRQ_EL0 if read => if k & 3 != 0 { Ok(()) } else { trap() },
            CNTPCT_EL0 if read => if k & 1 != 0 { Ok(()) } else { trap() },
            CNTVCT_EL0 if read => if k & 2 != 0 { Ok(()) } else { trap() },
            CNTV_CTL_EL0 | CNTV_CVAL_EL0 | CNTV_TVAL_EL0 => if k >> 8 & 1 != 0 { Ok(()) } else { trap() },
            CNTP_CTL_EL0 | CNTP_CVAL_EL0 | CNTP_TVAL_EL0 => if k >> 9 & 1 != 0 { Ok(()) } else { trap() },
            _ if is_storage_reg(e) && e >> 11 & 7 == 3 && e >> 14 == 3 => {
                // PMU EL0 registers: governed by PMUSERENR_EL0.EN.
                if self.sr.misc.get(&PMUSERENR_EL0).copied().unwrap_or(0) & 1 != 0 { Ok(()) } else { trap() }
            }
            MDCCSR_EL0 => trap(),
            _ => undef(),
        }
    }

    pub fn mrs(&mut self, e: u32, rt: u32) -> Res<u64> {
        if self.el == 0 {
            self.el0_check(e, rt, true)?;
        }
        if let Some(v) = id_value(e) {
            return Ok(v);
        }
        let s = &self.sr;
        Ok(match e {
            CCSIDR_EL1 => match s.csselr_el1 {
                0 => 0x700f_e01a,
                1 => 0x203f_e002,
                2 => 0x707f_e07a,
                _ => 0,
            },
            CSSELR_EL1 => s.csselr_el1,
            SCTLR_EL1 => s.sctlr_el1,
            ACTLR_EL1 => s.actlr_el1,
            CPACR_EL1 => s.cpacr_el1,
            TTBR0_EL1 => s.ttbr0_el1,
            TTBR1_EL1 => s.ttbr1_el1,
            TCR_EL1 => s.tcr_el1,
            SPSR_EL1 => s.spsr_el1,
            ELR_EL1 => s.elr_el1,
            SP_EL0 => self.sp_el0,
            SPSEL => self.spsel as u64,
            CURRENTEL => (self.el << 2) as u64,
            NZCV => self.nzcv as u64,
            DAIF => self.daif as u64,
            FPCR => self.fpcr as u64,
            FPSR => self.fpsr as u64,
            AFSR0_EL1 => s.afsr0_el1,
            AFSR1_EL1 => s.afsr1_el1,
            ESR_EL1 => s.esr_el1,
            FAR_EL1 => s.far_el1,
            PAR_EL1 => s.par_el1,
            MAIR_EL1 => s.mair_el1,
            AMAIR_EL1 => s.amair_el1,
            VBAR_EL1 => s.vbar_el1,
            ISR_EL1 => if self.bus.gic.irq_out { 1 << 7 } else { 0 },
            CONTEXTIDR_EL1 => s.contextidr_el1,
            TPIDR_EL1 => s.tpidr_el1,
            TPIDR_EL0 => s.tpidr_el0,
            TPIDRRO_EL0 => s.tpidrro_el0,
            CNTKCTL_EL1 => s.cntkctl_el1,
            CNTFRQ_EL0 => CNTFRQ,
            CNTPCT_EL0 | CNTVCT_EL0 => self.counter(),
            CNTV_CTL_EL0 => s.cntv_ctl | self.timer_istatus(s.cntv_ctl, s.cntv_cval),
            CNTV_CVAL_EL0 => s.cntv_cval,
            CNTV_TVAL_EL0 => s.cntv_cval.wrapping_sub(self.counter()) as u32 as u64,
            CNTP_CTL_EL0 => s.cntp_ctl | self.timer_istatus(s.cntp_ctl, s.cntp_cval),
            CNTP_CVAL_EL0 => s.cntp_cval,
            CNTP_TVAL_EL0 => s.cntp_cval.wrapping_sub(self.counter()) as u32 as u64,
            PMCR_EL0 => 0x4103_3000 | (s.misc.get(&PMCR_EL0).copied().unwrap_or(0) & 0x7f),
            OSLSR_EL1 => 0x8 | (s.misc.get(&OSLAR_EL1).copied().unwrap_or(1) & 1) << 1,
            MDSCR_EL1 => s.mdscr_el1,
            MDCCSR_EL0 => 0,
            _ if is_storage_reg(e) => s.misc.get(&e).copied().unwrap_or(0),
            _ => {
                self.log_sysreg(e, "read");
                return undef();
            }
        })
    }

    pub fn msr(&mut self, e: u32, rt: u32, v: u64) -> Res<()> {
        if self.el == 0 {
            self.el0_check(e, rt, false)?;
        }
        let s = &mut self.sr;
        match e {
            CSSELR_EL1 => s.csselr_el1 = v & 0xf,
            SCTLR_EL1 => {
                s.sctlr_el1 = v;
                self.tlb.flush();
            }
            ACTLR_EL1 => s.actlr_el1 = v,
            CPACR_EL1 => s.cpacr_el1 = v & 0x0030_0000,
            TTBR0_EL1 => {
                s.ttbr0_el1 = v;
                self.update_asid();
            }
            TTBR1_EL1 => {
                s.ttbr1_el1 = v;
                self.update_asid();
            }
            TCR_EL1 => {
                s.tcr_el1 = v;
                self.tlb.flush();
                self.update_asid();
            }
            SPSR_EL1 => s.spsr_el1 = v & 0xf03f_ffff,
            ELR_EL1 => s.elr_el1 = v,
            SP_EL0 => self.sp_el0 = v,
            SPSEL => self.spsel = (v & 1) as u32,
            NZCV => self.nzcv = (v as u32) & 0xf000_0000,
            DAIF => self.daif = (v as u32) & 0x3c0,
            FPCR => self.fpcr = (v as u32) & 0x07ff_9f00,
            FPSR => self.fpsr = (v as u32) & 0xf800_009f,
            AFSR0_EL1 => s.afsr0_el1 = v,
            AFSR1_EL1 => s.afsr1_el1 = v,
            ESR_EL1 => s.esr_el1 = v,
            FAR_EL1 => s.far_el1 = v,
            PAR_EL1 => s.par_el1 = v,
            MAIR_EL1 => s.mair_el1 = v,
            AMAIR_EL1 => s.amair_el1 = v,
            VBAR_EL1 => s.vbar_el1 = v & !0x7ff,
            CONTEXTIDR_EL1 => s.contextidr_el1 = v & 0xffff_ffff,
            TPIDR_EL1 => s.tpidr_el1 = v,
            TPIDR_EL0 => s.tpidr_el0 = v,
            TPIDRRO_EL0 => s.tpidrro_el0 = v,
            CNTKCTL_EL1 => s.cntkctl_el1 = v & 0x3ff,
            CNTV_CTL_EL0 => {
                s.cntv_ctl = v & 3;
                self.timer_update();
            }
            CNTV_CVAL_EL0 => {
                s.cntv_cval = v;
                self.timer_update();
            }
            CNTV_TVAL_EL0 => {
                let now = self.counter();
                self.sr.cntv_cval = now.wrapping_add(v as i32 as i64 as u64);
                self.timer_update();
            }
            CNTP_CTL_EL0 => {
                s.cntp_ctl = v & 3;
                self.timer_update();
            }
            CNTP_CVAL_EL0 => {
                s.cntp_cval = v;
                self.timer_update();
            }
            CNTP_TVAL_EL0 => {
                let now = self.counter();
                self.sr.cntp_cval = now.wrapping_add(v as i32 as i64 as u64);
                self.timer_update();
            }
            MDSCR_EL1 => s.mdscr_el1 = v,
            CNTFRQ_EL0 => {}
            _ if is_storage_reg(e) => {
                s.misc.insert(e, v);
            }
            _ => {
                self.log_sysreg(e, "write");
                return undef();
            }
        }
        Ok(())
    }

    fn log_sysreg(&self, e: u32, what: &str) {
        if self.log_undef {
            eprintln!(
                "a64: unknown sysreg {what} S{}_{}_C{}_C{}_{} at {:#x}",
                e >> 14,
                e >> 11 & 7,
                e >> 7 & 0xf,
                e >> 3 & 0xf,
                e & 7,
                self.insn_pc
            );
        }
    }

    /// CPACR_EL1.FPEN check for FP/SIMD instructions.
    #[inline(always)]
    pub fn check_fp(&self) -> Res<()> {
        let fpen = self.sr.cpacr_el1 >> 20 & 3;
        if fpen == 3 || (fpen == 1 && self.el != 0) {
            Ok(())
        } else {
            Err(Exception::new(ec::FP_ACCESS, 0x1e0_0000))
        }
    }
}
