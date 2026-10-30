//! F and D: IEEE 754 single and double precision with RISC-V's NaN rules
//! (results are canonical NaNs; singles are NaN-boxed), all five rounding
//! modes and the exception flags.
//!
//! Arithmetic uses the host's double precision (round to nearest, ties to
//! even) together with the operation's exact rounding error from an
//! error-free transformation (TwoSum, FMA). Whether the exact result is above
//! or below the rounded one (and whether it's a tie) is all it takes to round
//! in the other modes and to set NX, OF and UF. Single precision is computed
//! in double and rounded again: for +, -, *, / and sqrt this gives the
//! correctly rounded single result, and the double's own error decides the
//! direction when the second rounding is exact.

use crate::cpu::*;

// fflags
const NX: u32 = 1;
const UF: u32 = 2;
const OF: u32 = 4;
const DZ: u32 = 8;
const NV: u32 = 16;

// Rounding modes
const RNE: u32 = 0;
const RTZ: u32 = 1;
const RDN: u32 = 2;
const RUP: u32 = 3;
const RMM: u32 = 4;

const QNAN32: u32 = 0x7fc0_0000;
const QNAN64: u64 = 0x7ff8_0000_0000_0000;
const BOX: u64 = 0xffff_ffff_0000_0000;

fn is_snan32(b: u32) -> bool { b & 0x7f80_0000 == 0x7f80_0000 && b & 0x007f_ffff != 0 && b & 0x0040_0000 == 0 }
fn is_snan64(b: u64) -> bool {
    b & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 && b & 0x000f_ffff_ffff_ffff != 0 && b & 0x0008_0000_0000_0000 == 0
}

/// A floating-point operand: the value (as a double) and whether it was a
/// signaling NaN.
#[derive(Clone, Copy)]
struct Fv {
    v: f64,
    snan: bool,
}

/// Next representable value in the precision, up or down.
fn step(x: f64, up: bool, single: bool) -> f64 {
    if single {
        let b = (x as f32).to_bits();
        let r = if x == 0.0 {
            if up { 1 } else { 0x8000_0001 }
        } else if (x > 0.0) == up {
            b + 1
        } else {
            b - 1
        };
        f32::from_bits(r) as f64
    } else {
        let b = x.to_bits();
        let r = if x == 0.0 {
            if up { 1 } else { 0x8000_0000_0000_0001 }
        } else if (x > 0.0) == up {
            b + 1
        } else {
            b - 1
        };
        f64::from_bits(r)
    }
}

fn max_finite(single: bool) -> f64 { if single { f32::MAX as f64 } else { f64::MAX } }
fn min_normal(single: bool) -> f64 { if single { f32::MIN_POSITIVE as f64 } else { f64::MIN_POSITIVE } }

/// The result of an operation whose exact value overflowed the precision.
fn overflow(neg: bool, single: bool, rm: u32, flags: &mut u32) -> f64 {
    *flags |= OF | NX;
    let inf = match rm {
        RTZ => false,
        RDN => neg,
        RUP => !neg,
        _ => true,
    };
    let m = if inf { f64::INFINITY } else { max_finite(single) };
    if neg { -m } else { m }
}

/// Round the exact value d + e (d a double, e its rounding error with
/// |e| <= ulp(d) / 2, or just the error's sign when |e| < ulp(d) / 2) to
/// the precision in mode rm.
fn round(d: f64, e: f64, single: bool, rm: u32, flags: &mut u32) -> f64 {
    if d.is_nan() {
        return d;
    }
    if d.is_infinite() {
        return d;
    }
    let (mut r, err, half) = if single {
        let r = d as f32;
        if r.is_infinite() {
            return overflow(d < 0.0, true, rm, flags);
        }
        let r = r as f64;
        // d - r is exact (both are doubles within an ulp of each other)
        let err = d - r;
        let ulp = (step(r.abs(), true, true) - r.abs()).abs();
        (r, if err != 0.0 { err } else { e }, err.abs() == ulp / 2.0 && e == 0.0)
    } else {
        let ulp = (step(d.abs(), true, false) - d.abs()).abs();
        (d, e, e.abs() == ulp / 2.0)
    };
    if err == 0.0 {
        return r;
    }
    *flags |= NX;
    // err > 0: the exact value is above r
    match rm {
        RTZ => {
            if r > 0.0 && err < 0.0 {
                r = step(r, false, single);
            } else if r < 0.0 && err > 0.0 {
                r = step(r, true, single);
            }
        }
        RDN => {
            if err < 0.0 {
                r = step(r, false, single);
            }
        }
        RUP => {
            if err > 0.0 {
                r = step(r, true, single);
            }
        }
        RMM
            // Ties away from zero; ties to even may have gone towards zero
            if half && ((r > 0.0 && err > 0.0) || (r < 0.0 && err < 0.0)) => {
                r = step(r, err > 0.0, single);
            }
        _ => {}
    }
    if r.is_infinite() {
        *flags |= OF;
    } else if r.abs() < min_normal(single) {
        // Tiny (detected after rounding) and inexact
        *flags |= UF;
    }
    // A result rounded to zero has the exact value's sign (d's)
    if r == 0.0 {
        r = 0.0f64.copysign(d);
    }
    r
}

/// Error of a + b (TwoSum).
fn two_sum_err(a: f64, b: f64, s: f64) -> f64 {
    let bb = s - a;
    (a - (s - bb)) + (b - bb)
}

impl Cpu {
    /// Single operand from an FP register: NaN-boxed, else the canonical NaN.
    fn f32_bits(&self, r: u32) -> u32 {
        let v = self.f[r as usize];
        if v & BOX == BOX { v as u32 } else { QNAN32 }
    }

    fn get(&self, r: u32, single: bool) -> Fv {
        if single {
            let b = self.f32_bits(r);
            Fv { v: f32::from_bits(b) as f64, snan: is_snan32(b) }
        } else {
            let b = self.f[r as usize];
            Fv { v: f64::from_bits(b), snan: is_snan64(b) }
        }
    }

    /// Write a result: NaNs become the canonical NaN, singles are boxed.
    fn set(&mut self, r: u32, v: f64, single: bool) {
        self.f[r as usize] = if single {
            BOX | if v.is_nan() { QNAN32 } else { (v as f32).to_bits() } as u64
        } else if v.is_nan() {
            QNAN64
        } else {
            v.to_bits()
        };
        self.fs_dirty();
    }

    fn raise(&mut self, flags: u32) {
        if flags != 0 {
            self.csr.fcsr |= flags;
            self.fs_dirty();
        }
    }

    /// The instruction's rounding mode (7: fcsr.frm); None if reserved.
    fn rm(&self, i: u32) -> Option<u32> {
        let rm = i >> 12 & 7;
        let rm = if rm == 7 { self.csr.fcsr >> 5 & 7 } else { rm };
        (rm <= 4).then_some(rm)
    }

    pub fn exec_fp(&mut self, i: u32, raw: u32) -> Res<()> {
        let ill = || illegal(raw);
        if self.csr.mstatus & MSTATUS_FS == 0 {
            return Err(ill());
        }
        let (rd, rs1, rs2) = (i >> 7 & 31, i >> 15 & 31, i >> 20 & 31);
        let f3 = i >> 12 & 7;
        match i & 0x7f {
            0x07 => {
                // FLW, FLD
                let va = self.rx(rs1).wrapping_add((i as i32 >> 20) as i64 as u64);
                let v = match f3 {
                    2 => BOX | self.load(va, 4)?,
                    3 => self.load(va, 8)?,
                    _ => return Err(ill()),
                };
                self.f[rd as usize] = v;
                self.fs_dirty();
            }
            0x27 => {
                // FSW, FSD
                let imm = ((i as i32 >> 25) << 5) as u32 | (i >> 7 & 31);
                let va = self.rx(rs1).wrapping_add(imm as i32 as i64 as u64);
                match f3 {
                    2 => self.store(va, 4, self.f[rs2 as usize] & 0xffff_ffff)?,
                    3 => self.store(va, 8, self.f[rs2 as usize])?,
                    _ => return Err(ill()),
                }
            }
            0x43 | 0x47 | 0x4b | 0x4f => {
                let single = match i >> 25 & 3 {
                    0 => true,
                    1 => false,
                    _ => return Err(ill()),
                };
                let rm = self.rm(i).ok_or_else(ill)?;
                let rs3 = i >> 27;
                let (a, b, c) = (self.get(rs1, single), self.get(rs2, single), self.get(rs3, single));
                let (neg_prod, neg_c) = match i & 0x7f {
                    0x43 => (false, false), // FMADD
                    0x47 => (false, true),  // FMSUB
                    0x4b => (true, false),  // FNMSUB
                    _ => (true, true),      // FNMADD
                };
                let mut flags = 0;
                let r = fma(a, b, c, neg_prod, neg_c, single, rm, &mut flags);
                self.set(rd, r, single);
                self.raise(flags);
            }
            0x53 => self.op_fp(i, raw)?,
            _ => return Err(ill()),
        }
        Ok(())
    }

    fn op_fp(&mut self, i: u32, raw: u32) -> Res<()> {
        let ill = || illegal(raw);
        let (rd, rs1, rs2) = (i >> 7 & 31, i >> 15 & 31, i >> 20 & 31);
        let f3 = i >> 12 & 7;
        let single = match i >> 25 & 3 {
            0 => true,
            1 => false,
            _ => return Err(ill()),
        };
        let mut flags = 0;
        match i >> 27 {
            0x00..=0x03 => {
                let rm = self.rm(i).ok_or_else(ill)?;
                let (a, b) = (self.get(rs1, single), self.get(rs2, single));
                let r = arith(i >> 27, a, b, single, rm, &mut flags);
                self.set(rd, r, single);
            }
            0x0b => {
                // FSQRT
                if rs2 != 0 {
                    return Err(ill());
                }
                let rm = self.rm(i).ok_or_else(ill)?;
                let a = self.get(rs1, single);
                let r = if a.v.is_nan() {
                    if a.snan {
                        flags |= NV;
                    }
                    f64::NAN
                } else if a.v < 0.0 {
                    flags |= NV;
                    f64::NAN
                } else if a.v == 0.0 || a.v.is_infinite() {
                    a.v
                } else {
                    let s = a.v.sqrt();
                    // sqrt of a single is exact enough in double for the
                    // error's sign; for doubles use the residual a - s * s
                    let e = if single { a.v - s * s } else { (-s).mul_add(s, a.v) };
                    round(s, e, single, rm, &mut flags)
                };
                self.set(rd, r, single);
            }
            0x04 => {
                // FSGNJ, FSGNJN, FSGNJX: bit operations, no NaN handling
                if single {
                    let (a, b) = (self.f32_bits(rs1), self.f32_bits(rs2));
                    let s = match f3 {
                        0 => b,
                        1 => !b,
                        2 => a ^ b,
                        _ => return Err(ill()),
                    } & 0x8000_0000;
                    self.f[rd as usize] = BOX | ((a & 0x7fff_ffff) | s) as u64;
                } else {
                    let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                    let s = match f3 {
                        0 => b,
                        1 => !b,
                        2 => a ^ b,
                        _ => return Err(ill()),
                    } & 1 << 63;
                    self.f[rd as usize] = (a & !(1 << 63)) | s;
                }
                self.fs_dirty();
            }
            0x05 => {
                // FMIN, FMAX (IEEE 754-2019 minimumNumber / maximumNumber)
                if f3 > 1 {
                    return Err(ill());
                }
                let (a, b) = (self.get(rs1, single), self.get(rs2, single));
                if a.snan || b.snan {
                    flags |= NV;
                }
                let r = match (a.v.is_nan(), b.v.is_nan()) {
                    (true, true) => f64::NAN,
                    (true, false) => b.v,
                    (false, true) => a.v,
                    _ => {
                        let a_less = a.v < b.v || (a.v == b.v && a.v.is_sign_negative());
                        if (f3 == 0) == a_less { a.v } else { b.v }
                    }
                };
                self.set(rd, r, single);
            }
            0x08 => {
                // FCVT.S.D (single, rs2 = 1), FCVT.D.S (double, rs2 = 0)
                let rm = self.rm(i).ok_or_else(ill)?;
                match (single, rs2) {
                    (true, 1) => {
                        let a = self.get(rs1, false);
                        if a.snan {
                            flags |= NV;
                        }
                        let r = if a.v.is_nan() { f64::NAN } else { round(a.v, 0.0, true, rm, &mut flags) };
                        self.set(rd, r, true);
                    }
                    (false, 0) => {
                        let a = self.get(rs1, true);
                        if a.snan {
                            flags |= NV;
                        }
                        self.set(rd, a.v, false);
                    }
                    _ => return Err(ill()),
                }
            }
            0x14 => {
                // FLE, FLT (signaling), FEQ (quiet)
                let (a, b) = (self.get(rs1, single), self.get(rs2, single));
                let nan = a.v.is_nan() || b.v.is_nan();
                let r = match f3 {
                    0 | 1 => {
                        if nan {
                            flags |= NV;
                        }
                        if f3 == 0 { a.v <= b.v } else { a.v < b.v }
                    }
                    2 => {
                        if a.snan || b.snan {
                            flags |= NV;
                        }
                        a.v == b.v
                    }
                    _ => return Err(ill()),
                };
                self.wx(rd, r as u64);
            }
            0x18 => {
                // FCVT.{W,WU,L,LU}.{S,D}
                let rm = self.rm(i).ok_or_else(ill)?;
                let a = self.get(rs1, single);
                let v = to_int(a.v, rs2, rm, &mut flags).ok_or_else(ill)?;
                self.wx(rd, v);
            }
            0x1a => {
                // FCVT.{S,D}.{W,WU,L,LU}
                let rm = self.rm(i).ok_or_else(ill)?;
                let x = self.rx(rs1);
                let exact: i128 = match rs2 {
                    0 => x as i32 as i128,
                    1 => x as u32 as i128,
                    2 => x as i64 as i128,
                    3 => x as i128,
                    _ => return Err(ill()),
                };
                let d = exact as f64;
                // d's error, exact as an integer difference
                let e = (exact - d as i128) as f64;
                let r = round(d, e, single, rm, &mut flags);
                self.set(rd, r, single);
            }
            0x1c => {
                match f3 {
                    0 => {
                        // FMV.X.W, FMV.X.D: raw bits
                        let v = self.f[rs1 as usize];
                        self.wx(rd, if single { v as i32 as i64 as u64 } else { v });
                    }
                    1 => {
                        let a = self.get(rs1, single);
                        let bits = if single { self.f32_bits(rs1) as u64 } else { self.f[rs1 as usize] };
                        self.wx(rd, classify(a, bits, single));
                    }
                    _ => return Err(ill()),
                }
                if rs2 != 0 {
                    return Err(ill());
                }
            }
            0x1e => {
                // FMV.W.X, FMV.D.X
                if f3 != 0 || rs2 != 0 {
                    return Err(ill());
                }
                let x = self.rx(rs1);
                self.f[rd as usize] = if single { BOX | (x & 0xffff_ffff) } else { x };
                self.fs_dirty();
            }
            _ => return Err(ill()),
        }
        self.raise(flags);
        Ok(())
    }
}

/// FADD (0), FSUB (1), FMUL (2), FDIV (3).
fn arith(op: u32, a: Fv, b: Fv, single: bool, rm: u32, flags: &mut u32) -> f64 {
    if a.snan || b.snan {
        *flags |= NV;
    }
    if a.v.is_nan() || b.v.is_nan() {
        return f64::NAN;
    }
    let (x, y) = (a.v, if op == 1 { -b.v } else { b.v });
    match op {
        0 | 1 => {
            if x.is_infinite() && y.is_infinite() && x != y {
                *flags |= NV;
                return f64::NAN;
            }
            let s = x + y;
            if s.is_infinite() && !x.is_infinite() && !y.is_infinite() {
                return overflow(s < 0.0, single, rm, flags);
            }
            let e = if s.is_finite() { two_sum_err(x, y, s) } else { 0.0 };
            // An exact zero sum is -0 only in round down (or for -0 + -0)
            if s == 0.0 && e == 0.0 {
                let neg = if x == 0.0 && y == 0.0 && x.is_sign_negative() == y.is_sign_negative() {
                    x.is_sign_negative()
                } else {
                    rm == RDN
                };
                return if neg { -0.0 } else { 0.0 };
            }
            round(s, e, single, rm, flags)
        }
        2 => {
            if (x == 0.0 && y.is_infinite()) || (x.is_infinite() && y == 0.0) {
                *flags |= NV;
                return f64::NAN;
            }
            let p = x * y;
            if p.is_infinite() && !x.is_infinite() && !y.is_infinite() {
                return overflow(p < 0.0, single, rm, flags);
            }
            // Singles' products are exact in double
            let e = if single || !p.is_finite() { 0.0 } else { x.mul_add(y, -p) };
            round(p, e, single, rm, flags)
        }
        _ => {
            if (x == 0.0 && y == 0.0) || (x.is_infinite() && y.is_infinite()) {
                *flags |= NV;
                return f64::NAN;
            }
            if y == 0.0 && !x.is_infinite() {
                *flags |= DZ;
                return x / y;
            }
            let q = x / y;
            if q.is_infinite() && !x.is_infinite() {
                return overflow(q < 0.0, single, rm, flags);
            }
            if !q.is_finite() || q == 0.0 && x == 0.0 {
                return q;
            }
            // The remainder's sign gives the error's: x = q * y + r
            let r = (-q).mul_add(y, x);
            let e = if r == 0.0 { 0.0 } else { r / y };
            let e = if e == 0.0 && r != 0.0 { if (r > 0.0) == (y > 0.0) { f64::MIN_POSITIVE } else { -f64::MIN_POSITIVE } } else { e };
            round(q, e, single, rm, flags)
        }
    }
}

/// a * b + c with the given negations, rounded once.
#[allow(clippy::too_many_arguments)]
fn fma(a: Fv, b: Fv, c: Fv, neg_prod: bool, neg_c: bool, single: bool, rm: u32, flags: &mut u32) -> f64 {
    if a.snan || b.snan || c.snan {
        *flags |= NV;
    }
    // inf * 0 is invalid even with a NaN addend
    if (a.v.is_infinite() && b.v == 0.0) || (a.v == 0.0 && b.v.is_infinite()) {
        *flags |= NV;
        return f64::NAN;
    }
    if a.v.is_nan() || b.v.is_nan() || c.v.is_nan() {
        return f64::NAN;
    }
    let x = if neg_prod { -a.v } else { a.v };
    let y = b.v;
    let z = if neg_c { -c.v } else { c.v };
    let p = x * y;
    if x.is_infinite() || y.is_infinite() || z.is_infinite() {
        // An infinite operand gives an infinite (exact) result, or NaN for
        // inf - inf
        let r = p + z;
        if r.is_nan() {
            *flags |= NV;
        }
        return r;
    }
    if single {
        // The product is exact; the sum's error is exact too
        let s = p + z;
        if s.is_infinite() {
            return overflow(s < 0.0, true, rm, flags);
        }
        if s == 0.0 && two_sum_err(p, z, s) == 0.0 {
            let neg = if p == 0.0 && z == 0.0 && p.is_sign_negative() == z.is_sign_negative() {
                p.is_sign_negative()
            } else {
                rm == RDN
            };
            return if neg { -0.0 } else { 0.0 };
        }
        return round(s, two_sum_err(p, z, s), true, rm, flags);
    }
    let r = x.mul_add(y, z);
    if r.is_infinite() {
        return overflow(r < 0.0, false, rm, flags);
    }
    // Exact value: p + pe + z, with p + pe = x * y exactly
    let pe = x.mul_add(y, -p);
    let s = p + z;
    let se = if s.is_finite() { two_sum_err(p, z, s) } else { 0.0 };
    let e = ((s - r) + se) + pe;
    if r == 0.0 && e == 0.0 {
        let neg = if p == 0.0 && z == 0.0 && p.is_sign_negative() == z.is_sign_negative() {
            p.is_sign_negative()
        } else {
            rm == RDN
        };
        return if neg { -0.0 } else { 0.0 };
    }
    round(r, e, false, rm, flags)
}

/// FCVT to an integer: kind 0 W, 1 WU, 2 L, 3 LU (sign-extended result).
fn to_int(x: f64, kind: u32, rm: u32, flags: &mut u32) -> Option<u64> {
    let (lo, hi): (f64, f64) = match kind {
        0 => (-2147483648.0, 2147483647.0),
        1 => (0.0, 4294967295.0),
        2 => (-9223372036854775808.0, 9223372036854775807.0),
        3 => (0.0, 18446744073709551615.0),
        _ => return None,
    };
    let sat = |neg: bool| -> u64 {
        match (kind, neg) {
            (0, false) => 0x7fff_ffff,
            (0, true) => 0xffff_ffff_8000_0000,
            (1, false) => u64::MAX,
            (1, true) => 0,
            (2, false) => i64::MAX as u64,
            (2, true) => i64::MIN as u64,
            (_, false) => u64::MAX,
            (_, true) => 0,
        }
    };
    if x.is_nan() {
        *flags |= NV;
        return Some(sat(false));
    }
    let r = match rm {
        RNE => x.round_ties_even(),
        RTZ => x.trunc(),
        RDN => x.floor(),
        RUP => x.ceil(),
        _ => x.round(),
    };
    // hi isn't exactly representable for L/LU; compare against 2^63 / 2^64
    let too_big = match kind {
        2 => r >= 9223372036854775808.0,
        3 => r >= 18446744073709551616.0,
        _ => r > hi,
    };
    if too_big || r < lo {
        *flags |= NV;
        return Some(sat(r < 0.0));
    }
    if r != x {
        *flags |= NX;
    }
    Some(match kind {
        0 => r as i32 as i64 as u64,
        1 => r as u32 as i32 as i64 as u64,
        2 => r as i64 as u64,
        _ => r as u64,
    })
}

fn classify(a: Fv, bits: u64, single: bool) -> u64 {
    let v = a.v;
    let neg = v.is_sign_negative();
    let sub = if single { (v as f32).is_subnormal() } else { v.is_subnormal() };
    let bit = if v.is_nan() {
        if (single && is_snan32(bits as u32)) || (!single && is_snan64(bits)) { 8 } else { 9 }
    } else if v.is_infinite() {
        if neg { 0 } else { 7 }
    } else if v == 0.0 {
        if neg { 3 } else { 4 }
    } else if sub {
        if neg { 2 } else { 5 }
    } else if neg {
        1
    } else {
        6
    };
    1 << bit
}
