//! The Cortex-M4F's floating-point unit: FPv4-SP (single precision, 32
//! registers S0-S31, overlaid as D0-D15 for moves and loads/stores), as the
//! ARMv7-M Architecture Reference Manual describes it (A7.8 and A6.7, the
//! "Floating-point" instructions; B1.5.7 for its exception frames, in
//! cpu.rs).
//!
//! Thumb's 32-bit coprocessor instructions for coprocessors 10 and 11 carry
//! the same fields in bits [27:0] as their ARM encodings, which is how
//! they're decoded here.
//!
//! Arithmetic is IEEE 754 single precision, as the host's f32 does it, in
//! round-to-nearest; FPSCR's rounding mode is used by VCVTR (float to
//! integer) and VSQRT/arithmetic ignore it, as they do flush-to-zero and
//! default NaN mode (both off at reset, and left off by the firmware this
//! runs). The cumulative exception flags aren't kept.

use crate::cpu::Cpu;
use crate::scs;

fn bits(x: u32, hi: u32, lo: u32) -> u32 { x >> lo & ((1u64 << (hi - lo + 1)) - 1) as u32 }

/// VFPExpandImm (A6.4.1), for single precision.
fn expand_imm(imm8: u32) -> u32 {
    let b6 = imm8 >> 6 & 1;
    (imm8 >> 7) << 31 | (b6 ^ 1) << 30 | (if b6 != 0 { 0x1f } else { 0 }) << 25 | (imm8 >> 4 & 3) << 23 | (imm8 & 0xf) << 19
}

/// IEEE half precision to single.
fn f16_to_f32(h: u32) -> u32 {
    let sign = (h >> 15 & 1) << 31;
    let exp = h >> 10 & 0x1f;
    let frac = h & 0x3ff;
    match (exp, frac) {
        (0, 0) => sign,
        (0, _) => {
            // subnormal: normalise
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e as u32) << 23 | (f & 0x3ff) << 13
        }
        (0x1f, 0) => sign | 0x7f80_0000,
        (0x1f, _) => sign | 0x7fc0_0000 | frac << 13,
        _ => sign | (exp + 127 - 15) << 23 | frac << 13,
    }
}

/// Single to IEEE half precision, rounding to nearest even.
fn f32_to_f16(x: u32) -> u32 {
    let sign = (x >> 31) << 15;
    let exp = (x >> 23 & 0xff) as i32;
    let frac = x & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if frac != 0 { 0x200 | frac >> 13 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    // the significand with its implicit bit, and how far to shift it down
    let (m, shift) = if e <= 0 {
        if e < -10 {
            return sign;
        }
        (frac | 0x80_0000, (14 - e) as u32)
    } else {
        (frac, 13)
    };
    let mut h = m >> shift;
    let rest = m & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    if rest > half || (rest == half && h & 1 != 0) {
        h += 1;
    }
    if e <= 0 {
        sign | h
    } else {
        // (a carry out of the fraction moves into the exponent, as it should)
        sign | (((e as u32) << 10) + h)
    }
}

impl Cpu {
    fn sf(&self, n: usize) -> f32 { f32::from_bits(self.s[n]) }
    fn set_sf(&mut self, n: usize, v: f32) { self.s[n] = v.to_bits(); }

    /// An instruction for coprocessor 10 or 11: the FPU's, if there's one.
    pub(crate) fn fp_instruction(&mut self, i: u32) {
        let cp = bits(i, 11, 8);
        if !self.fpu || (cp != 10 && cp != 11) {
            return self.undefined(i);
        }
        // CPACR: CP10 (and CP11) access; unprivileged needs full access
        let access = self.bus.scs.cpacr >> 20 & 3;
        if access == 0 || (access == 1 && !self.privileged()) {
            let at = self.pc;
            return self.usage_fault_at(scs::UFSR_NOCP, at);
        }
        // Automatic state preservation (FPCCR.ASPEN): the first FP
        // instruction in a context makes it an FP context, with FPSCR's
        // defaults from FPDSCR
        if self.bus.scs.fpccr & 1 << 31 != 0 && self.control & 4 == 0 {
            self.control |= 4;
            self.fpscr = self.bus.scs.fpdscr & 0x07c0_0000;
        }
        if bits(i, 27, 25) == 0b110 {
            self.fp_load_store(i);
        } else if bits(i, 27, 24) == 0b1110 && i & 0x10 == 0 {
            self.fp_data(i);
        } else if bits(i, 27, 24) == 0b1110 {
            self.fp_transfer(i);
        } else {
            self.undefined(i);
        }
    }

    /// Extension register loads and stores, and 64-bit transfers (A6.7.1).
    fn fp_load_store(&mut self, i: u32) {
        let op = bits(i, 24, 20);
        let double = bits(i, 11, 8) == 11;
        let rn = bits(i, 19, 16);
        let d = bits(i, 22, 22);
        let vd = bits(i, 15, 12);
        // the first S register
        let sd = if double { 2 * (d << 4 | vd) } else { vd << 1 | d } as usize;
        if op & 0b11110 == 0b00100 {
            // VMOV: two core registers and two S registers / a D register
            let rt = bits(i, 15, 12);
            let rt2 = rn;
            let m = bits(i, 3, 0) << 1 | bits(i, 5, 5);
            let sm = if double { 2 * (bits(i, 5, 5) << 4 | bits(i, 3, 0)) } else { m } as usize;
            if sm + 1 >= 32 {
                return self.undefined(i);
            }
            if op & 1 != 0 {
                self.set_reg(rt, self.s[sm]);
                self.set_reg(rt2, self.s[sm + 1]);
            } else {
                self.s[sm] = self.reg(rt);
                self.s[sm + 1] = self.reg(rt2);
            }
            return;
        }
        let (p, u, w, l) = (op >> 4 & 1, op >> 3 & 1, op >> 1 & 1, op & 1);
        let imm8 = bits(i, 7, 0);
        if p == 1 && w == 0 {
            // VLDR / VSTR
            let base = if rn == 15 { self.pc.wrapping_add(4) & !3 } else { self.reg(rn) };
            let a = if u != 0 { base.wrapping_add(imm8 * 4) } else { base.wrapping_sub(imm8 * 4) };
            let n = if double { 2 } else { 1 };
            for k in 0..n {
                let r = (sd + k) & 31;
                if l != 0 {
                    self.s[r] = self.read(a + 4 * k as u32, 4);
                } else {
                    let v = self.s[r];
                    self.write(a + 4 * k as u32, 4, v);
                }
            }
            return;
        }
        // VLDM / VSTM (VPUSH / VPOP): increment after, or decrement before
        // with writeback
        if p == u || (p == 1 && w == 0) {
            return self.undefined(i);
        }
        // (FLDMX/FSTMX, an odd count with D registers, transfer imm8 - 1 words)
        let words = if double { imm8 & !1 } else { imm8 } as usize;
        if words == 0 || sd + words > 32 {
            return self.undefined(i);
        }
        let base = self.reg(rn);
        let start = if u != 0 { base } else { base.wrapping_sub(imm8 * 4) };
        if w != 0 {
            let wb = if u != 0 { base.wrapping_add(imm8 * 4) } else { base.wrapping_sub(imm8 * 4) };
            self.set_reg(rn, wb);
        }
        for k in 0..words {
            let a = start.wrapping_add(4 * k as u32);
            if l != 0 {
                self.s[sd + k] = self.read(a, 4);
            } else {
                let v = self.s[sd + k];
                self.write(a, 4, v);
            }
        }
    }

    /// Transfers between core registers and S registers, and FPSCR
    /// (A6.7.2).
    fn fp_transfer(&mut self, i: u32) {
        let l = bits(i, 20, 20);
        let a = bits(i, 23, 21);
        let rt = bits(i, 15, 12);
        let cp11 = bits(i, 11, 8) == 11;
        if !cp11 && a == 0 {
            // VMOV Sn <-> Rt
            let sn = (bits(i, 19, 16) << 1 | bits(i, 7, 7)) as usize;
            if l != 0 {
                self.set_reg(rt, self.s[sn]);
            } else {
                self.s[sn] = self.reg(rt);
            }
        } else if !cp11 && a == 0b111 {
            let reg = bits(i, 19, 16);
            if l != 0 {
                // VMRS: FPSCR, or its flags to APSR (Rt = 15)
                let v = match reg {
                    1 => self.fpscr,
                    // MVFR0, MVFR1
                    7 => 0x1011_0021,
                    6 => 0x1100_0011,
                    // FPSID
                    0 => 0x4100_20b0,
                    // FPEXC
                    8 => 0x4000_0000,
                    _ => 0,
                };
                if rt == 15 {
                    self.n = v >> 31 & 1 != 0;
                    self.z = v >> 30 & 1 != 0;
                    self.c = v >> 29 & 1 != 0;
                    self.v = v >> 28 & 1 != 0;
                } else {
                    self.set_reg(rt, v);
                }
            } else if reg == 1 {
                // VMSR FPSCR
                self.fpscr = self.reg(rt) & 0xf7c0_009f;
            }
        } else if cp11 && a & 0b110 == 0 {
            // VMOV Dd[x] <-> Rt: a half of a D register
            let dd = (bits(i, 7, 7) << 4 | bits(i, 19, 16)) as usize;
            let sn = 2 * dd + bits(i, 21, 21) as usize;
            if sn >= 32 {
                return self.undefined(i);
            }
            if l != 0 {
                self.set_reg(rt, self.s[sn]);
            } else {
                self.s[sn] = self.reg(rt);
            }
        } else {
            self.undefined(i);
        }
    }

    /// FPSCR.NZCV from a comparison.
    fn fp_compare(&mut self, x: f32, y: f32) {
        let nzcv = if x.is_nan() || y.is_nan() {
            0b0011
        } else if x == y {
            0b0110
        } else if x < y {
            0b1000
        } else {
            0b0010
        };
        self.fpscr = (self.fpscr & 0x0fff_ffff) | nzcv << 28;
    }

    /// Round to an integer in FPSCR's rounding mode (VCVTR).
    fn fp_round(&self, x: f32) -> f32 {
        match self.fpscr >> 22 & 3 {
            0 => x.round_ties_even(),
            1 => x.ceil(),
            2 => x.floor(),
            _ => x.trunc(),
        }
    }

    /// Floating-point data processing (A6.7.3).
    fn fp_data(&mut self, i: u32) {
        if bits(i, 8, 8) != 0 {
            // double precision: not on FPv4-SP
            return self.undefined(i);
        }
        let opc1 = bits(i, 23, 20) & 0b1011;
        let opc2 = bits(i, 19, 16);
        let opc3 = bits(i, 7, 6);
        let op = opc3 & 1 != 0;
        let sd = (bits(i, 15, 12) << 1 | bits(i, 22, 22)) as usize;
        let sn = (bits(i, 19, 16) << 1 | bits(i, 7, 7)) as usize;
        let sm = (bits(i, 3, 0) << 1 | bits(i, 5, 5)) as usize;
        let (d, n, m) = (self.sf(sd), self.sf(sn), self.sf(sm));
        let r = match opc1 {
            // VMLA, VMLS: the product rounded, then the sum
            0b0000 => if op { d - n * m } else { d + n * m },
            // VNMLS, VNMLA
            0b0001 => if op { -d - n * m } else { -d + n * m },
            // VMUL, VNMUL
            0b0010 => if op { -(n * m) } else { n * m },
            // VADD, VSUB
            0b0011 => if op { n - m } else { n + m },
            // VDIV
            0b1000 if !op => n / m,
            // VFNMS, VFNMA (fused)
            0b1001 => if op { (-n).mul_add(m, -d) } else { n.mul_add(m, -d) },
            // VFMA, VFMS (fused)
            0b1010 => if op { (-n).mul_add(m, d) } else { n.mul_add(m, d) },
            0b1011 => return self.fp_other(i, sd, sm, opc2, opc3),
            _ => return self.undefined(i),
        };
        self.set_sf(sd, r);
    }

    /// VMOV (immediate and register), VABS, VNEG, VSQRT, VCMP, the VCVTs.
    fn fp_other(&mut self, i: u32, sd: usize, sm: usize, opc2: u32, opc3: u32) {
        if opc3 & 1 == 0 {
            // VMOV immediate
            self.s[sd] = expand_imm(bits(i, 19, 16) << 4 | bits(i, 3, 0));
            return;
        }
        let m = self.sf(sm);
        let t = opc3 & 2 != 0;
        match opc2 {
            0b0000 => self.s[sd] = if t { self.s[sm] & 0x7fff_ffff } else { self.s[sm] },
            0b0001 => {
                if t {
                    self.set_sf(sd, m.sqrt());
                } else {
                    self.s[sd] = self.s[sm] ^ 0x8000_0000;
                }
            }
            // VCVTB / VCVTT: the bottom or top half of Sm or Sd
            0b0010 => {
                let h = if t { self.s[sm] >> 16 } else { self.s[sm] & 0xffff };
                self.s[sd] = f16_to_f32(h);
            }
            0b0011 => {
                let h = f32_to_f16(self.s[sm]);
                self.s[sd] = if t { (self.s[sd] & 0xffff) | h << 16 } else { (self.s[sd] & 0xffff_0000) | h };
            }
            // VCMP{E}, with a register or zero
            0b0100 => self.fp_compare(self.sf(sd), m),
            0b0101 => self.fp_compare(self.sf(sd), 0.0),
            // VCVT from integer (signed if opc3's top bit)
            0b1000 => {
                let v = if t { self.s[sm] as i32 as f32 } else { self.s[sm] as f32 };
                self.set_sf(sd, v);
            }
            // VCVT to integer: toward zero (opc3's top bit) or as FPSCR
            // rounds (VCVTR), saturating, NaN to 0
            0b1100 | 0b1101 => {
                let x = if t { m } else { self.fp_round(m) };
                self.s[sd] = if opc2 & 1 != 0 { x as i32 as u32 } else { x as u32 };
            }
            // VCVT between floating point and fixed point, in place
            0b1010 | 0b1011 | 0b1110 | 0b1111 => {
                let to_fixed = opc2 & 0b0100 != 0;
                let unsigned = opc2 & 1 != 0;
                let size = if t { 32 } else { 16 };
                let imm = bits(i, 3, 0) << 1 | bits(i, 5, 5);
                let frac = size as i32 - imm as i32;
                if frac < 0 {
                    return self.undefined(i);
                }
                let scale = (2.0f64).powi(frac);
                if to_fixed {
                    let x = (self.sf(sd) as f64 * scale).trunc();
                    let v = match (unsigned, size) {
                        (true, 32) => x.clamp(0.0, u32::MAX as f64) as u32,
                        (false, 32) => x.clamp(i32::MIN as f64, i32::MAX as f64) as i32 as u32,
                        (true, _) => x.clamp(0.0, u16::MAX as f64) as u32,
                        (false, _) => x.clamp(i16::MIN as f64, i16::MAX as f64) as i32 as u32 & 0xffff,
                    };
                    self.s[sd] = v;
                } else {
                    let raw = self.s[sd];
                    let x = match (unsigned, size) {
                        (true, 32) => raw as f64,
                        (false, 32) => raw as i32 as f64,
                        (true, _) => (raw & 0xffff) as f64,
                        (false, _) => raw as u16 as i16 as f64,
                    };
                    self.set_sf(sd, (x / scale) as f32);
                }
            }
            _ => self.undefined(i),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediates() {
        assert_eq!(f32::from_bits(expand_imm(0x70)), 1.0);
        assert_eq!(f32::from_bits(expand_imm(0x00)), 2.0);
        assert_eq!(f32::from_bits(expand_imm(0xe0)), -0.5);
    }

    #[test]
    fn halves() {
        for v in [0.0f32, 1.0, -2.5, 65504.0, 6.1035156e-5, 5.9604645e-8, f32::INFINITY] {
            assert_eq!(f32::from_bits(f16_to_f32(f32_to_f16(v.to_bits()))), v);
        }
        assert_eq!(f32_to_f16(1e6f32.to_bits()), 0x7c00);
    }
}
