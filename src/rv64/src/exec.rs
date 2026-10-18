//! Instruction decoding and execution: RV64I, M, A, Zicsr, Zifencei and the
//! privileged instructions. F and D are in fpu.rs, C is expanded by rvc.rs.

use crate::cpu::*;
use crate::mmu::Acc;

#[inline(always)]
fn sx32(v: u64) -> u64 { v as i32 as i64 as u64 }

impl Cpu {
    #[inline(always)]
    pub fn wx(&mut self, r: u32, v: u64) {
        if r != 0 {
            self.x[r as usize] = v;
        }
    }

    #[inline(always)]
    pub fn rx(&self, r: u32) -> u64 { self.x[r as usize] }

    /// Execute instruction `i` (compressed ones already expanded; `raw` is the
    /// encoding as fetched, for illegal-instruction tval), whose pc is
    /// insn_pc; self.pc is already the next instruction's.
    #[inline(always)]
    pub fn exec(&mut self, i: u32, raw: u32) -> Res<()> {
        if self.xlen32 {
            self.exec32(i, raw)
        } else {
            self.exec64(i, raw)
        }
    }

    /// RV32: registers hold their 32-bit values sign-extended to 64 bits,
    /// which most RV64 instructions keep that way. Those that don't have
    /// RV64 equivalents that do: ADD is ADDW, SLL is SLLW, DIVU is DIVUW,
    /// etc. The rest is handled here: 64-bit only instructions are illegal,
    /// pc-relative results and jump targets are 32 bits, and so are the
    /// addresses of loads and stores (load(), store() and amo_addr()).
    fn exec32(&mut self, i: u32, raw: u32) -> Res<()> {
        let ill = || illegal(raw);
        let rd = i >> 7 & 31;
        let f3 = i >> 12 & 7;
        let rs1 = i >> 15 & 31;
        let rs2 = i >> 20 & 31;
        let f7 = i >> 25;
        let pc = self.insn_pc;
        let w = |op: u32| i & !0x7f | op;
        match i & 0x7f {
            0x17 => self.wx(rd, sx32(pc.wrapping_add((i & 0xffff_f000) as i32 as i64 as u64))), // AUIPC
            0x6f | 0x67 => {
                // JAL, JALR: the link is sign-extended, the target 32 bits
                let link = sx32(self.pc);
                self.exec64(i, raw)?;
                self.pc &= 0xffff_ffff;
                self.wx(rd, link);
            }
            0x63 => {
                self.exec64(i, raw)?;
                self.pc &= 0xffff_ffff;
            }
            0x03 if f3 == 3 || f3 == 6 => return Err(ill()), // LD, LWU
            0x23 if f3 == 3 => return Err(ill()),             // SD
            0x13 if matches!(f3, 0 | 1 | 5) => return self.exec64(w(0x1b), raw), // ADDI, shifts: as ADDIW etc.
            0x33 => match (f7, f3) {
                (0, 0) | (0x20, 0) | (0, 1) | (0, 5) | (0x20, 5) | (1, 0) | (1, 4..=7) => return self.exec64(w(0x3b), raw),
                (1, 1..=3) => {
                    // MULH, MULHSU, MULHU
                    let (a, b) = (self.rx(rs1), self.rx(rs2));
                    let (sa, ua) = (a as i32 as i64, a as u32 as i64);
                    let (sb, ub) = (b as i32 as i64, b as u32 as i64);
                    let p = match f3 {
                        1 => sa * sb,
                        2 => sa * ub,
                        _ => ua * ub,
                    };
                    self.wx(rd, sx32((p >> 32) as u64));
                }
                _ => return self.exec64(i, raw),
            },
            0x1b | 0x3b => return Err(ill()),
            0x2f if f3 != 2 => return Err(ill()), // AMOs: .W only
            // FMV.X.D, FMV.D.X, FCVT to and from L and LU
            0x53 if (f7 == 0x71 && f3 == 0) || f7 == 0x79 => return Err(ill()),
            0x53 if matches!(f7, 0x60 | 0x61 | 0x68 | 0x69) && rs2 >= 2 => return Err(ill()),
            _ => return self.exec64(i, raw),
        }
        Ok(())
    }

    /// exec() for RV64.
    fn exec64(&mut self, i: u32, raw: u32) -> Res<()> {
        let ill = || illegal(raw);
        let rd = i >> 7 & 31;
        let f3 = i >> 12 & 7;
        let rs1 = i >> 15 & 31;
        let rs2 = i >> 20 & 31;
        let f7 = i >> 25;
        let imm_i = (i as i32 >> 20) as i64 as u64;
        let pc = self.insn_pc;
        match i & 0x7f {
            0x37 => self.wx(rd, (i & 0xffff_f000) as i32 as i64 as u64), // LUI
            0x17 => self.wx(rd, pc.wrapping_add((i & 0xffff_f000) as i32 as i64 as u64)), // AUIPC
            0x6f => {
                // JAL
                let imm = ((i as i32 >> 31) << 20) as u32
                    | (i >> 21 & 0x3ff) << 1
                    | (i >> 20 & 1) << 11
                    | (i >> 12 & 0xff) << 12;
                self.wx(rd, self.pc); // pc + length
                self.pc = pc.wrapping_add(imm as i32 as i64 as u64);
            }
            0x67 => {
                // JALR
                if f3 != 0 {
                    return Err(ill());
                }
                let target = self.rx(rs1).wrapping_add(imm_i) & !1;
                self.wx(rd, self.pc); // pc + length
                self.pc = target;
            }
            0x63 => {
                let (a, b) = (self.rx(rs1), self.rx(rs2));
                let taken = match f3 {
                    0 => a == b,
                    1 => a != b,
                    4 => (a as i64) < (b as i64),
                    5 => (a as i64) >= (b as i64),
                    6 => a < b,
                    7 => a >= b,
                    _ => return Err(ill()),
                };
                if taken {
                    let imm = ((i as i32 >> 31) << 12) as u32
                        | (i >> 25 & 0x3f) << 5
                        | (i >> 8 & 0xf) << 1
                        | (i >> 7 & 1) << 11;
                    self.pc = pc.wrapping_add(imm as i32 as i64 as u64);
                }
            }
            0x03 => {
                let va = self.rx(rs1).wrapping_add(imm_i);
                let v = match f3 {
                    0 => self.load(va, 1)? as i8 as i64 as u64,
                    1 => self.load(va, 2)? as i16 as i64 as u64,
                    2 => self.load(va, 4)? as i32 as i64 as u64,
                    3 => self.load(va, 8)?,
                    4 => self.load(va, 1)?,
                    5 => self.load(va, 2)?,
                    6 => self.load(va, 4)?,
                    _ => return Err(ill()),
                };
                self.wx(rd, v);
            }
            0x23 => {
                if f3 > 3 {
                    return Err(ill());
                }
                let imm = ((i as i32 >> 25) << 5) as u32 | (i >> 7 & 31);
                let va = self.rx(rs1).wrapping_add(imm as i32 as i64 as u64);
                self.store(va, 1 << f3, self.rx(rs2))?;
            }
            0x13 => {
                let a = self.rx(rs1);
                let shamt = i >> 20 & 63;
                let v = match f3 {
                    0 => a.wrapping_add(imm_i),
                    1 => {
                        if i >> 26 != 0 {
                            return Err(ill());
                        }
                        a << shamt
                    }
                    2 => ((a as i64) < (imm_i as i64)) as u64,
                    3 => (a < imm_i) as u64,
                    4 => a ^ imm_i,
                    5 => match i >> 26 {
                        0 => a >> shamt,
                        0x10 => ((a as i64) >> shamt) as u64,
                        _ => return Err(ill()),
                    },
                    6 => a | imm_i,
                    _ => a & imm_i,
                };
                self.wx(rd, v);
            }
            0x1b => {
                let a = self.rx(rs1);
                let shamt = rs2;
                let v = match (f3, f7) {
                    (0, _) => sx32(a.wrapping_add(imm_i)),
                    (1, 0) => sx32(a << shamt),
                    (5, 0) => sx32((a as u32 >> shamt) as u64),
                    (5, 0x20) => ((a as i32) >> shamt) as i64 as u64,
                    _ => return Err(ill()),
                };
                self.wx(rd, v);
            }
            0x33 => {
                let (a, b) = (self.rx(rs1), self.rx(rs2));
                let v = match (f7, f3) {
                    (0, 0) => a.wrapping_add(b),
                    (0x20, 0) => a.wrapping_sub(b),
                    (0, 1) => a << (b & 63),
                    (0, 2) => ((a as i64) < (b as i64)) as u64,
                    (0, 3) => (a < b) as u64,
                    (0, 4) => a ^ b,
                    (0, 5) => a >> (b & 63),
                    (0x20, 5) => ((a as i64) >> (b & 63)) as u64,
                    (0, 6) => a | b,
                    (0, 7) => a & b,
                    (1, 0) => a.wrapping_mul(b),
                    (1, 1) => ((a as i64 as i128 * b as i64 as i128) >> 64) as u64,
                    (1, 2) => ((a as i64 as i128 * b as i128) >> 64) as u64,
                    (1, 3) => ((a as u128 * b as u128) >> 64) as u64,
                    (1, 4) => {
                        let (a, b) = (a as i64, b as i64);
                        if b == 0 { u64::MAX } else { a.wrapping_div(b) as u64 }
                    }
                    (1, 5) => a.checked_div(b).unwrap_or(u64::MAX),
                    (1, 6) => {
                        let (a, b) = (a as i64, b as i64);
                        if b == 0 { a as u64 } else { a.wrapping_rem(b) as u64 }
                    }
                    (1, 7) => if b == 0 { a } else { a % b },
                    _ => return Err(ill()),
                };
                self.wx(rd, v);
            }
            0x3b => {
                let (a, b) = (self.rx(rs1) as u32, self.rx(rs2) as u32);
                let v = match (f7, f3) {
                    (0, 0) => a.wrapping_add(b) as i32 as i64 as u64,
                    (0x20, 0) => a.wrapping_sub(b) as i32 as i64 as u64,
                    (0, 1) => (a << (b & 31)) as i32 as i64 as u64,
                    (0, 5) => (a >> (b & 31)) as i32 as i64 as u64,
                    (0x20, 5) => ((a as i32) >> (b & 31)) as i64 as u64,
                    (1, 0) => a.wrapping_mul(b) as i32 as i64 as u64,
                    (1, 4) => {
                        let (a, b) = (a as i32, b as i32);
                        (if b == 0 { -1 } else { a.wrapping_div(b) }) as i64 as u64
                    }
                    (1, 5) => a.checked_div(b).unwrap_or(u32::MAX) as i32 as i64 as u64,
                    (1, 6) => {
                        let (a, b) = (a as i32, b as i32);
                        (if b == 0 { a } else { a.wrapping_rem(b) }) as i64 as u64
                    }
                    (1, 7) => (if b == 0 { a } else { a % b }) as i32 as i64 as u64,
                    _ => return Err(ill()),
                };
                self.wx(rd, v);
            }
            0x0f => match f3 {
                0 => {}    // FENCE
                1 => self.icache_flush(), // FENCE.I
                _ => return Err(ill()),
            },
            0x2f => self.exec_amo(i, raw)?,
            0x73 => self.exec_system(i, raw)?,
            0x07 | 0x27 | 0x43 | 0x47 | 0x4b | 0x4f | 0x53 => self.exec_fp(i, raw)?,
            _ => return Err(ill()),
        }
        Ok(())
    }

    fn exec_amo(&mut self, i: u32, raw: u32) -> Res<()> {
        let f3 = i >> 12 & 7;
        let size = match f3 {
            2 => 4,
            3 => 8,
            _ => return Err(illegal(raw)),
        };
        let (rd, rs1, rs2) = (i >> 7 & 31, i >> 15 & 31, i >> 20 & 31);
        let va = self.rx(rs1);
        let ext = |v: u64| if size == 4 { sx32(v) } else { v };
        match i >> 27 {
            0x02 => {
                // LR
                if rs2 != 0 {
                    return Err(illegal(raw));
                }
                let pa = self.amo_addr(va, size, Acc::Read)?;
                let v = self.bus.read(pa, size);
                self.reservation = Some(pa);
                self.wx(rd, ext(v));
            }
            0x03 => {
                // SC
                let pa = self.amo_addr(va, size, Acc::Write)?;
                if self.reservation == Some(pa) {
                    self.bus.write(pa, size, self.rx(rs2));
                    self.wx(rd, 0);
                } else {
                    self.wx(rd, 1);
                }
                self.reservation = None;
            }
            op => {
                let pa = self.amo_addr(va, size, Acc::Write)?;
                let old = self.bus.read(pa, size);
                let b = self.rx(rs2);
                let (sa, sb) = if size == 4 { (sx32(old) as i64, sx32(b) as i64) } else { (old as i64, b as i64) };
                let (ua, ub) = if size == 4 { (old as u32 as u64, b as u32 as u64) } else { (old, b) };
                let new = match op {
                    0x01 => b,
                    0x00 => old.wrapping_add(b),
                    0x04 => old ^ b,
                    0x0c => old & b,
                    0x08 => old | b,
                    0x10 => sa.min(sb) as u64,
                    0x14 => sa.max(sb) as u64,
                    0x18 => ua.min(ub),
                    0x1c => ua.max(ub),
                    _ => return Err(illegal(raw)),
                };
                self.bus.write(pa, size, new);
                self.wx(rd, ext(old));
            }
        }
        Ok(())
    }

    fn exec_system(&mut self, i: u32, raw: u32) -> Res<()> {
        let f3 = i >> 12 & 7;
        let (rd, rs1) = (i >> 7 & 31, i >> 15 & 31);
        if f3 == 0 {
            match i {
                0x0000_0073 => {
                    let cause = match self.prv {
                        PRV_U => cause::ECALL_U,
                        PRV_S => cause::ECALL_S,
                        _ => cause::ECALL_M,
                    };
                    return Err(Exception { cause, tval: 0 });
                }
                0x0010_0073 => return Err(Exception { cause: cause::BREAKPOINT, tval: self.insn_pc }),
                0x1020_0073 => {
                    // SRET
                    if self.prv < PRV_S || (self.prv == PRV_S && self.csr.mstatus & MSTATUS_TSR != 0) {
                        return Err(illegal(raw));
                    }
                    self.sret();
                }
                0x3020_0073 => {
                    if self.prv < PRV_M {
                        return Err(illegal(raw));
                    }
                    self.mret();
                }
                0x1050_0073 => {
                    // WFI
                    if self.prv == PRV_U || (self.prv == PRV_S && self.csr.mstatus & MSTATUS_TW != 0) {
                        return Err(illegal(raw));
                    }
                    if self.csr.mip & self.csr.mie == 0 {
                        self.waiting = true;
                    }
                }
                _ if i >> 25 == 0x09 && rd == 0 => {
                    // SFENCE.VMA
                    if self.prv == PRV_U || (self.prv == PRV_S && self.csr.mstatus & MSTATUS_TVM != 0) {
                        return Err(illegal(raw));
                    }
                    if rs1 != 0 {
                        let va = self.xva(self.rx(rs1));
                        self.tlb.flush_va(va);
                    } else {
                        self.tlb.flush();
                    }
                    self.icache_flush();
                }
                _ => return Err(illegal(raw)),
            }
            return Ok(());
        }
        if f3 == 4 {
            return Err(illegal(raw));
        }
        // CSR instructions
        let csr = i >> 20;
        let src = if f3 >= 5 { rs1 as u64 } else { self.rx(rs1) };
        let ill = |_| illegal(raw);
        match f3 & 3 {
            1 => {
                // CSRRW: no read when rd is x0
                let old = if rd != 0 { Some(self.csr_read(csr).map_err(ill)?) } else { None };
                if (csr >> 8 & 3) as u64 > self.prv {
                    return Err(illegal(raw));
                }
                self.csr_write(csr, src).map_err(ill)?;
                if let Some(old) = old {
                    self.wx(rd, old);
                }
            }
            op => {
                // CSRRS/CSRRC: no write when the source is x0 (or 0 for the
                // immediate forms)
                let old = self.csr_read(csr).map_err(ill)?;
                if rs1 != 0 {
                    // mip's SEIP reads as the PLIC's line or'ed with the
                    // software bit, but a read-modify-write must only see the
                    // software bit, or it would latch the PLIC's line into it
                    let base = if csr == 0x344 {
                        (old & !MIP_SEIP) | if self.csr.seip_sw { MIP_SEIP } else { 0 }
                    } else {
                        old
                    };
                    let new = if op == 2 { base | src } else { base & !src };
                    self.csr_write(csr, new).map_err(ill)?;
                }
                self.wx(rd, old);
            }
        }
        Ok(())
    }
}
