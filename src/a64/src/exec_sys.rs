//! System instructions, exception generation and the PSCI firmware interface.

use crate::bits::*;
use crate::cpu::{Cpu, Power};
use crate::exc::{ec, undef, Exception, Res};
use crate::mmu::Acc;

const PSCI_VERSION: u64 = 0x8400_0000;
const PSCI_CPU_SUSPEND: u64 = 0x8400_0001;
const PSCI_CPU_OFF: u64 = 0x8400_0002;
const PSCI_CPU_ON: u64 = 0x8400_0003;
const PSCI_AFFINITY_INFO: u64 = 0x8400_0004;
const PSCI_MIGRATE_INFO_TYPE: u64 = 0x8400_0006;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
const PSCI_FEATURES: u64 = 0x8400_000a;
const SMC64: u64 = 0x4000_0000;

const PSCI_NOT_SUPPORTED: i64 = -1;
const PSCI_INVALID_PARAMS: i64 = -2;
const PSCI_ALREADY_ON: i64 = -4;

fn is_psci_call(fid: u64) -> bool {
    let fid = fid & 0xffff_ffff;
    (0x8400_0000..=0x8400_001f).contains(&fid) || (0xc400_0000..=0xc400_001f).contains(&fid)
}

impl Cpu {
    pub(crate) fn exception_gen(&mut self, i: u32) -> Res<()> {
        let imm16 = bits(i, 20, 5);
        if bits(i, 4, 2) != 0 {
            return undef();
        }
        match (bits(i, 23, 21), bits(i, 1, 0)) {
            (0, 1) => Err(Exception { ec: ec::SVC64, iss: imm16, far: None, ret_next: true }),
            (0, 2) => {
                // HVC: no EL2, but QEMU's virt machine services PSCI calls here.
                if self.el == 0 || imm16 != 0 || !is_psci_call(self.x[0]) {
                    return undef();
                }
                self.psci();
                Ok(())
            }
            (1, 0) => Err(Exception::new(ec::BRK64, imm16)),
            _ => undef(),
        }
    }

    fn psci(&mut self) {
        let fid = self.x[0] & 0xffff_ffff;
        let base = fid & !SMC64;
        let ret: i64 = match base {
            PSCI_VERSION => 0x0001_0001,
            PSCI_CPU_SUSPEND => {
                if !self.bus.gic.irq_out {
                    self.waiting = true;
                }
                0
            }
            PSCI_CPU_OFF => {
                self.power = Power::Off;
                0
            }
            PSCI_CPU_ON => {
                if self.x[1] & 0xff_00ff_ffff == 0 {
                    PSCI_ALREADY_ON
                } else {
                    PSCI_INVALID_PARAMS
                }
            }
            PSCI_AFFINITY_INFO => {
                if self.x[1] & 0xff_00ff_ffff == 0 {
                    0
                } else {
                    PSCI_INVALID_PARAMS
                }
            }
            PSCI_MIGRATE_INFO_TYPE => 2,
            PSCI_SYSTEM_OFF => {
                self.power = Power::Off;
                0
            }
            PSCI_SYSTEM_RESET => {
                self.power = Power::Reset;
                0
            }
            PSCI_FEATURES => {
                let f = self.x[1] & 0xffff_ffff & !SMC64;
                match f {
                    PSCI_VERSION | PSCI_CPU_SUSPEND | PSCI_CPU_OFF | PSCI_CPU_ON | PSCI_AFFINITY_INFO
                    | PSCI_MIGRATE_INFO_TYPE | PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET | PSCI_FEATURES => 0,
                    _ => PSCI_NOT_SUPPORTED,
                }
            }
            _ => PSCI_NOT_SUPPORTED,
        };
        self.x[0] = ret as u64;
    }

    pub(crate) fn exec_system(&mut self, i: u32) -> Res<()> {
        let l = bit(i, 21);
        let op0 = bits(i, 20, 19);
        let op1 = bits(i, 18, 16);
        let crn = bits(i, 15, 12);
        let crm = bits(i, 11, 8);
        let op2 = bits(i, 7, 5);
        let rt = i & 31;
        match op0 {
            0 => {
                if l || rt != 31 {
                    return undef();
                }
                match crn {
                    2 => self.hint((crm << 3) | op2),
                    3 => {
                        if op2 == 2 {
                            self.excl = None; // CLREX
                        }
                        Ok(())
                    }
                    4 => self.msr_pstate(op1, op2, crm),
                    _ => undef(),
                }
            }
            1 => {
                if l {
                    return undef();
                }
                self.sys_op(op1, crn, crm, op2, rt)
            }
            _ => {
                let e = crate::sysregs::enc(op0, op1, crn, crm, op2);
                if l {
                    let v = self.mrs(e, rt)?;
                    self.set_xr(rt, v);
                } else {
                    let v = self.xr(rt);
                    self.msr(e, rt, v)?;
                }
                Ok(())
            }
        }
    }

    fn hint(&mut self, op: u32) -> Res<()> {
        let sctlr = self.sr.sctlr_el1;
        match op {
            3 => {
                // WFI
                if self.el == 0 && sctlr >> 16 & 1 == 0 {
                    return Err(Exception::new(ec::WFX, 0x1e0_0000));
                }
                if !self.bus.gic.irq_out {
                    self.waiting = true;
                }
            }
            2 => {
                // WFE: single CPU, nothing to wait for.
                if self.el == 0 && sctlr >> 18 & 1 == 0 {
                    return Err(Exception::new(ec::WFX, 0x1e0_0001));
                }
            }
            // NOP, YIELD, SEV, SEVL, and PAuth/BTI/ESB/CSDB hints (not implemented -> NOP).
            _ => {}
        }
        Ok(())
    }

    fn msr_pstate(&mut self, op1: u32, op2: u32, crm: u32) -> Res<()> {
        match (op1, op2) {
            (0, 5) => {
                if self.el == 0 {
                    return undef();
                }
                self.spsel = crm & 1;
            }
            (3, 6) | (3, 7) => {
                if self.el == 0 && self.sr.sctlr_el1 >> 9 & 1 == 0 {
                    return Err(Exception::new(
                        ec::SYSREG,
                        (op2 << 17) | (op1 << 14) | (4 << 10) | (31 << 5) | (crm << 1),
                    ));
                }
                if op2 == 6 {
                    self.daif |= (crm & 0xf) << 6;
                } else {
                    self.daif &= !((crm & 0xf) << 6);
                }
            }
            _ => return undef(),
        }
        Ok(())
    }

    fn sys_trap(&self, op1: u32, crn: u32, crm: u32, op2: u32, rt: u32) -> Exception {
        Exception::new(ec::SYSREG, (1 << 20) | (op2 << 17) | (op1 << 14) | (crn << 10) | (rt << 5) | (crm << 1))
    }

    fn sys_op(&mut self, op1: u32, crn: u32, crm: u32, op2: u32, rt: u32) -> Res<()> {
        let el0 = self.el == 0;
        let sctlr = self.sr.sctlr_el1;
        let uci = sctlr >> 26 & 1 != 0;
        let va = self.xr(rt);
        match (op1, crn, crm, op2) {
            // IC IALLUIS / IC IALLU
            (0, 7, 1, 0) | (0, 7, 5, 0) => {
                if el0 {
                    return undef();
                }
            }
            // IC IVAU
            (3, 7, 5, 1) => {
                if el0 && !uci {
                    return Err(self.sys_trap(op1, crn, crm, op2, rt));
                }
                self.translate_cm(va, Acc::Read)?;
            }
            // DC IVAC (needs write permission)
            (0, 7, 6, 1) => {
                if el0 {
                    return undef();
                }
                self.translate_cm(va, Acc::Write)?;
            }
            // DC ISW / CSW / CISW
            (0, 7, 6, 2) | (0, 7, 10, 2) | (0, 7, 14, 2) => {
                if el0 {
                    return undef();
                }
            }
            // DC CVAC / CVAU / CIVAC
            (3, 7, 10, 1) | (3, 7, 11, 1) | (3, 7, 14, 1) => {
                if el0 && !uci {
                    return Err(self.sys_trap(op1, crn, crm, op2, rt));
                }
                self.translate_cm(va, Acc::Read)?;
            }
            // DC ZVA
            (3, 7, 4, 1) => {
                if el0 && sctlr >> 14 & 1 == 0 {
                    return Err(self.sys_trap(op1, crn, crm, op2, rt));
                }
                let base = va & !63;
                // Check the whole (page-contained) block before zeroing it.
                self.translate(base, Acc::Write, false)?;
                for k in 0..8 {
                    self.write(base + k * 8, 8, 0, false)?;
                }
            }
            // AT S1E1R / S1E1W / S1E0R / S1E0W
            (0, 7, 8, 0..=3) => {
                if el0 {
                    return undef();
                }
                self.sr.par_el1 = self.address_translate(va, op2 & 2 != 0, op2 & 1 != 0);
            }
            // TLBI (all variants flush everything)
            (0, 8, _, _) => {
                if el0 {
                    return undef();
                }
                self.tlb.flush();
            }
            _ => {
                if self.log_undef {
                    eprintln!("a64: unknown SYS op1={op1} C{crn} C{crm} op2={op2} at {:#x}", self.insn_pc);
                }
                return undef();
            }
        }
        Ok(())
    }
}
