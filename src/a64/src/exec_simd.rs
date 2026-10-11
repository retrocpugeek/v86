//! Advanced SIMD and scalar floating point.
//!
//! Coverage follows what the reference buildroot system executes (glibc string
//! routines, kernel FP save/restore and CRC) plus the common scalar FP set.
//! Anything else is UNDEFINED and logged.

use crate::bits::*;
use crate::cpu::Cpu;
use crate::exc::{undef, Res};

#[inline(always)]
fn lane(v: u128, idx: u32, esz: u32) -> u64 { (v >> (idx * esz)) as u64 & ones(esz) }

#[inline(always)]
fn set_lane(v: &mut u128, idx: u32, esz: u32, val: u64) {
    let m = (ones(esz) as u128) << (idx * esz);
    *v = (*v & !m) | (((val & ones(esz)) as u128) << (idx * esz));
}

#[inline(always)]
fn sx(v: u64, esz: u32) -> i64 { sext(v, esz) }

#[inline(always)]
fn vmask(q: bool) -> u128 { if q { !0 } else { u64::MAX as u128 } }

/// Replicate a 64-bit pattern across the vector.
#[inline(always)]
fn dup64(x: u64) -> u128 { x as u128 | (x as u128) << 64 }

fn replicate_lane(x: u64, esz: u32) -> u64 {
    let mut r = 0u64;
    let mut i = 0;
    while i < 64 {
        r |= (x & ones(esz)) << i;
        i += esz;
    }
    r
}

/// Polynomial (carry-less) multiply.
fn clmul(a: u64, b: u64, esz: u32) -> u128 {
    let mut r = 0u128;
    for k in 0..esz {
        if b >> k & 1 != 0 {
            r ^= (a as u128) << k;
        }
    }
    r
}

fn rbit_esz(x: u64, esz: u32) -> u64 { x.reverse_bits() >> (64 - esz) }

/// Elementwise integer operation for the "three same" group. None = unallocated.
fn int3(opc: u32, u: bool, esz: u32, a: u64, b: u64, d: u64) -> Option<u64> {
    let m = ones(esz);
    let (sa, sb) = (sx(a, esz), sx(b, esz));
    Some(
        match (opc, u) {
            (0x00, false) => ((sa as i128 + sb as i128) >> 1) as u64,
            (0x00, true) => ((a as u128 + b as u128) >> 1) as u64,
            (0x02, false) => ((sa as i128 + sb as i128 + 1) >> 1) as u64,
            (0x02, true) => ((a as u128 + b as u128 + 1) >> 1) as u64,
            (0x04, false) => ((sa as i128 - sb as i128) >> 1) as u64,
            (0x04, true) => ((a as i128 - b as i128) >> 1) as u64,
            (0x06, false) => if sa > sb { m } else { 0 },
            (0x06, true) => if a > b { m } else { 0 },
            (0x07, false) => if sa >= sb { m } else { 0 },
            (0x07, true) => if a >= b { m } else { 0 },
            (0x08, _) => {
                // SSHL/USHL: shift by signed low byte of b.
                let sh = b as u8 as i8 as i32;
                if sh >= 0 {
                    if sh >= esz as i32 { 0 } else { a << sh }
                } else {
                    let sh = (-sh) as u32;
                    if u {
                        if sh >= esz { 0 } else { a >> sh }
                    } else {
                        (sa >> sh.min(63)) as u64
                    }
                }
            }
            (0x0c, false) => sa.max(sb) as u64,
            (0x0c, true) => a.max(b),
            (0x0d, false) => sa.min(sb) as u64,
            (0x0d, true) => a.min(b),
            (0x0e, false) => (sa - sb).unsigned_abs(),
            (0x0e, true) => a.abs_diff(b),
            (0x0f, false) => d.wrapping_add((sa - sb).unsigned_abs()),
            (0x0f, true) => d.wrapping_add(a.abs_diff(b)),
            (0x10, false) => a.wrapping_add(b),
            (0x10, true) => a.wrapping_sub(b),
            (0x11, false) => if a & b != 0 { m } else { 0 },
            (0x11, true) => if a == b { m } else { 0 },
            (0x12, false) => d.wrapping_add(a.wrapping_mul(b)),
            (0x12, true) => d.wrapping_sub(a.wrapping_mul(b)),
            (0x13, false) => a.wrapping_mul(b),
            (0x13, true) if esz == 8 => clmul(a, b, 8) as u64,
            _ => return None,
        } & m,
    )
}

fn f32_of(v: u64) -> f32 { f32::from_bits(v as u32) }
fn f64_of(v: u64) -> f64 { f64::from_bits(v) }

fn fmax_arm(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return a + b;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() && b.is_sign_negative() { -0.0 } else { 0.0 };
    }
    if a > b { a } else { b }
}

fn fmin_arm(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return a + b;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() || b.is_sign_negative() { -0.0 } else { 0.0 };
    }
    if a < b { a } else { b }
}

fn fmaxnm(a: f64, b: f64) -> f64 {
    match (a.is_nan(), b.is_nan()) {
        (true, false) => b,
        (false, true) => a,
        _ => fmax_arm(a, b),
    }
}

fn fminnm(a: f64, b: f64) -> f64 {
    match (a.is_nan(), b.is_nan()) {
        (true, false) => b,
        (false, true) => a,
        _ => fmin_arm(a, b),
    }
}

fn fcmp_flags(a: f64, b: f64) -> u32 {
    if a.is_nan() || b.is_nan() {
        0x3000_0000
    } else if a == b {
        0x6000_0000
    } else if a < b {
        0x8000_0000
    } else {
        0x2000_0000
    }
}

/// Round according to an FPRounding mode: 0=N(even) 1=P 2=M 3=Z 4=A(away).
fn fround(x: f64, mode: u32) -> f64 {
    match mode {
        0 => x.round_ties_even(),
        1 => x.ceil(),
        2 => x.floor(),
        3 => x.trunc(),
        _ => x.round(),
    }
}

fn vfp_expand_imm(imm8: u32, double: bool) -> u64 {
    let a = (imm8 >> 7) as u64;
    let b = (imm8 >> 6 & 1) as u64;
    let cd = (imm8 >> 4 & 3) as u64;
    let efgh = (imm8 & 0xf) as u64;
    if double {
        let exp = ((b ^ 1) << 10) | (if b != 0 { 0xff } else { 0 } << 2) | cd;
        (a << 63) | (exp << 52) | (efgh << 48)
    } else {
        let exp = ((b ^ 1) << 7) | (if b != 0 { 0x1f } else { 0 } << 2) | cd;
        (a << 31) | (exp << 23) | (efgh << 19)
    }
}

fn adv_simd_expand_imm(op: bool, cmode: u32, imm8: u32) -> Option<u64> {
    let imm8 = imm8 as u64;
    Some(match cmode >> 1 {
        0 => replicate_lane(imm8, 32),
        1 => replicate_lane(imm8 << 8, 32),
        2 => replicate_lane(imm8 << 16, 32),
        3 => replicate_lane(imm8 << 24, 32),
        4 => replicate_lane(imm8, 16),
        5 => replicate_lane(imm8 << 8, 16),
        6 => {
            if cmode & 1 == 0 {
                replicate_lane((imm8 << 8) | 0xff, 32)
            } else {
                replicate_lane((imm8 << 16) | 0xffff, 32)
            }
        }
        _ => {
            if cmode & 1 == 0 {
                if !op {
                    replicate_lane(imm8, 8)
                } else {
                    let mut r = 0u64;
                    for k in 0..8 {
                        if imm8 >> k & 1 != 0 {
                            r |= 0xff << (8 * k);
                        }
                    }
                    r
                }
            } else if !op {
                replicate_lane(vfp_expand_imm(imm8 as u32, false), 32)
            } else {
                vfp_expand_imm(imm8 as u32, true)
            }
        }
    })
}

impl Cpu {
    #[inline(always)]
    fn vd(&mut self, rd: u32, val: u128, q: bool) { self.v[rd as usize] = val & vmask(q); }

    #[inline(always)]
    fn vr(&self, r: u32) -> u128 { self.v[r as usize] }

    pub(crate) fn exec_simd(&mut self, i: u32) -> Res<()> {
        self.check_fp()?;
        if bits(i, 30, 24) == 0b0011110 {
            return self.fp_dp(i);
        }
        if bits(i, 30, 24) == 0b0011111 {
            return self.fp_3src(i);
        }
        if bit(i, 31) {
            return undef();
        }
        match bits(i, 28, 24) {
            0b01110 => {
                if bit(i, 21) {
                    match bits(i, 11, 10) {
                        0b01 | 0b11 => self.three_same(i, false),
                        0b00 => self.three_diff(i),
                        _ => match bits(i, 20, 17) {
                            0b0000 => self.two_misc(i, false),
                            0b1000 => self.across(i),
                            _ => undef(),
                        },
                    }
                } else if bits(i, 23, 21) == 0 && !bit(i, 15) && bit(i, 10) {
                    self.copy(i)
                } else if !bit(i, 29) && !bit(i, 15) && bits(i, 11, 10) == 0b10 {
                    self.permute(i)
                } else if bit(i, 29) && bits(i, 23, 22) == 0 && !bit(i, 15) && !bit(i, 10) {
                    self.ext(i)
                } else if !bit(i, 29) && bits(i, 23, 22) == 0 && !bit(i, 15) && bits(i, 11, 10) == 0 {
                    self.tbl(i)
                } else {
                    undef()
                }
            }
            0b01111 => {
                if !bit(i, 10) {
                    undef()
                } else if bits(i, 22, 19) == 0 {
                    self.mod_imm(i)
                } else {
                    self.shift_imm(i, false)
                }
            }
            0b11110 if bit(i, 30) => {
                if bit(i, 21) {
                    match bits(i, 11, 10) {
                        0b01 | 0b11 => self.three_same(i, true),
                        0b10 => match bits(i, 20, 17) {
                            0b0000 => self.two_misc(i, true),
                            0b1000 => self.scalar_pairwise(i),
                            _ => undef(),
                        },
                        _ => undef(),
                    }
                } else if bits(i, 23, 21) == 0 && !bit(i, 15) && bit(i, 10) && !bit(i, 29) && bits(i, 14, 11) == 0 {
                    // DUP (element), scalar
                    let imm5 = bits(i, 20, 16);
                    if imm5 & 0xf == 0 {
                        return undef();
                    }
                    let s = imm5.trailing_zeros();
                    let esz = 8 << s;
                    let idx = imm5 >> (s + 1);
                    let x = lane(self.vr(bits(i, 9, 5)), idx, esz);
                    self.v[(i & 31) as usize] = x as u128;
                    Ok(())
                } else {
                    undef()
                }
            }
            0b11111 if bit(i, 30) => {
                if bit(i, 10) && bits(i, 22, 19) != 0 {
                    self.shift_imm(i, true)
                } else {
                    undef()
                }
            }
            _ => undef(),
        }
    }

    fn three_same(&mut self, i: u32, scalar: bool) -> Res<()> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 15, 11);
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        let (a, b, d) = (self.vr(rn), self.vr(rm), self.vr(rd));
        if opc == 0x03 && !scalar {
            // Logical ops on the whole register.
            let r = match (u, size) {
                (false, 0) => a & b,
                (false, 1) => a & !b,
                (false, 2) => a | b,
                (false, _) => a | !b,
                (true, 0) => a ^ b,
                (true, 1) => (d & a) | (!d & b), // BSL
                (true, 2) => (d & !b) | (a & b), // BIT
                (true, _) => (d & b) | (a & !b), // BIF
            };
            self.vd(rd, r, q);
            return Ok(());
        }
        if opc >= 0x18 {
            return self.fp_three_same(i, scalar);
        }
        let esz = 8 << size;
        if scalar {
            // Only the 64-bit compare/add/sub/shift forms exist as scalars.
            if size != 3 || !matches!(opc, 0x06 | 0x07 | 0x08 | 0x10 | 0x11) {
                return undef();
            }
            let Some(r) = int3(opc, u, 64, a as u64, b as u64, d as u64) else { return undef() };
            self.v[rd as usize] = r as u128;
            return Ok(());
        }
        if size == 3 && !q {
            return undef();
        }
        let lanes = if q { 128 } else { 64 } / esz;
        let mut r = 0u128;
        if matches!(opc, 0x14 | 0x15 | 0x17) {
            // Pairwise: SMAXP/UMAXP, SMINP/UMINP, ADDP.
            if opc == 0x17 && u {
                return undef();
            }
            let half = lanes / 2;
            for k in 0..lanes {
                let (src, j) = if k < half { (a, 2 * k) } else { (b, 2 * (k - half)) };
                let (x, y) = (lane(src, j, esz), lane(src, j + 1, esz));
                let v = match opc {
                    0x17 => x.wrapping_add(y),
                    0x14 => int3(0x0c, u, esz, x, y, 0).unwrap(),
                    _ => int3(0x0d, u, esz, x, y, 0).unwrap(),
                };
                set_lane(&mut r, k, esz, v);
            }
            self.vd(rd, r, q);
            return Ok(());
        }
        if matches!(opc, 0x12 | 0x13) && size == 3 {
            return undef();
        }
        for k in 0..lanes {
            let Some(v) = int3(opc, u, esz, lane(a, k, esz), lane(b, k, esz), lane(d, k, esz)) else {
                return undef();
            };
            set_lane(&mut r, k, esz, v);
        }
        self.vd(rd, r, q);
        Ok(())
    }

    fn three_diff(&mut self, i: u32) -> Res<()> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 15, 12);
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        let (a, b, d) = (self.vr(rn), self.vr(rm), self.vr(rd));
        if opc == 0xe {
            // PMULL/PMULL2
            if u {
                return undef();
            }
            let (x, y) = if q { ((a >> 64) as u64, (b >> 64) as u64) } else { (a as u64, b as u64) };
            let r = match size {
                0 => {
                    let mut r = 0u128;
                    for k in 0..8 {
                        set_lane(&mut r, k, 16, clmul(lane(x as u128, k, 8), lane(y as u128, k, 8), 8) as u64);
                    }
                    r
                }
                3 => clmul(x, y, 64),
                _ => return undef(),
            };
            self.v[rd as usize] = r;
            return Ok(());
        }
        if size == 3 {
            return undef();
        }
        let esz = 8 << size;
        let wsz = esz * 2;
        let lanes = 64 / esz;
        // Narrow sources come from the upper half for the "2" forms.
        let hi = if q { 64 } else { 0 };
        let narrow = |v: u128, k: u32| lane(v >> hi, k, esz);
        let ext = |x: u64| if u { x } else { sx(x, esz) as u64 };
        let mut r = 0u128;
        match opc {
            0x0 | 0x2 | 0x5 | 0x7 | 0x8 | 0xa | 0xc => {
                for k in 0..lanes {
                    let (x, y) = (ext(narrow(a, k)), ext(narrow(b, k)));
                    let acc = lane(d, k, wsz);
                    let absd = if u { x.abs_diff(y) } else { (x as i64 - y as i64).unsigned_abs() };
                    let v = match opc {
                        0x0 => x.wrapping_add(y),
                        0x2 => x.wrapping_sub(y),
                        0x5 => acc.wrapping_add(absd),
                        0x7 => absd,
                        0x8 => acc.wrapping_add(x.wrapping_mul(y)),
                        0xa => acc.wrapping_sub(x.wrapping_mul(y)),
                        _ => x.wrapping_mul(y),
                    };
                    set_lane(&mut r, k, wsz, v);
                }
                self.v[rd as usize] = r;
            }
            0x1 | 0x3 => {
                // ADDW/SUBW: wide first operand.
                for k in 0..lanes {
                    let x = lane(a, k, wsz);
                    let y = ext(narrow(b, k));
                    set_lane(&mut r, k, wsz, if opc == 1 { x.wrapping_add(y) } else { x.wrapping_sub(y) });
                }
                self.v[rd as usize] = r;
            }
            0x4 | 0x6 => {
                // ADDHN/RADDHN/SUBHN/RSUBHN
                let round = if u { 1u64 << (esz - 1) } else { 0 };
                let mut n = 0u128;
                for k in 0..lanes {
                    let (x, y) = (lane(a, k, wsz), lane(b, k, wsz));
                    let s = if opc == 4 { x.wrapping_add(y) } else { x.wrapping_sub(y) };
                    set_lane(&mut n, k, esz, s.wrapping_add(round) >> esz);
                }
                if q {
                    self.v[rd as usize] = (d & u64::MAX as u128) | n << 64;
                } else {
                    self.v[rd as usize] = n;
                }
            }
            _ => return undef(),
        }
        Ok(())
    }

    fn two_misc(&mut self, i: u32, scalar: bool) -> Res<()> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 16, 12);
        let (rd, rn) = (i & 31, bits(i, 9, 5));
        let a = self.vr(rn);
        let esz = 8 << size;
        if opc >= 0x0c && (opc >= 0x16 || size & 2 != 0) {
            if let Some(r) = self.fp_misc(i, scalar)? {
                self.v[rd as usize] = r;
                return Ok(());
            }
        }
        if scalar {
            if size != 3 {
                return undef();
            }
            let x = a as u64;
            let r = match (opc, u) {
                (0x08, false) => if (x as i64) > 0 { !0 } else { 0 },
                (0x08, true) => if (x as i64) >= 0 { !0 } else { 0 },
                (0x09, false) => if x == 0 { !0 } else { 0 },
                (0x09, true) => if (x as i64) <= 0 { !0 } else { 0 },
                (0x0a, false) => if (x as i64) < 0 { !0 } else { 0 },
                (0x0b, false) => (x as i64).unsigned_abs(),
                (0x0b, true) => x.wrapping_neg(),
                _ => return undef(),
            };
            self.v[rd as usize] = r as u128;
            return Ok(());
        }
        let lanes = if q { 128 } else { 64 } / esz;
        let mut r = 0u128;
        match (opc, u) {
            (0x00, _) | (0x01, false) => {
                // REV64 / REV32 / REV16: reverse elements within containers.
                let csz = match (opc, u) {
                    (0, false) => 64,
                    (0, true) => 32,
                    _ => 16,
                };
                if esz >= csz {
                    return undef();
                }
                let per = csz / esz;
                for k in 0..lanes {
                    let base = k / per * per;
                    let j = base + (per - 1 - (k - base));
                    set_lane(&mut r, k, esz, lane(a, j, esz));
                }
            }
            (0x02, _) | (0x06, _) => {
                // SADDLP/UADDLP, SADALP/UADALP
                if size == 3 {
                    return undef();
                }
                let d = self.vr(rd);
                for k in 0..lanes / 2 {
                    let (x, y) = (lane(a, 2 * k, esz), lane(a, 2 * k + 1, esz));
                    let s = if u { x + y } else { (sx(x, esz) + sx(y, esz)) as u64 };
                    let acc = if opc == 6 { lane(d, k, esz * 2) } else { 0 };
                    set_lane(&mut r, k, esz * 2, acc.wrapping_add(s));
                }
            }
            (0x04, _) => {
                if size == 3 {
                    return undef();
                }
                for k in 0..lanes {
                    let x = lane(a, k, esz);
                    let v = if u {
                        (x << (64 - esz)).leading_zeros().min(esz) as u64
                    } else {
                        let s = (sx(x, esz) as u64) << (64 - esz);
                        (((s ^ (s << 1)) | (1u64 << (64 - esz))).leading_zeros()) as u64
                    };
                    set_lane(&mut r, k, esz, v);
                }
            }
            (0x05, false) => {
                if size != 0 {
                    return undef();
                }
                for k in 0..lanes {
                    set_lane(&mut r, k, 8, lane(a, k, 8).count_ones() as u64);
                }
            }
            (0x05, true) => match size {
                0 => r = !a,
                1 => {
                    for k in 0..lanes {
                        set_lane(&mut r, k, 8, rbit_esz(lane(a, k, 8), 8));
                    }
                }
                _ => return undef(),
            },
            (0x08..=0x0b, _) => {
                if size == 3 && !q {
                    return undef();
                }
                let m = ones(esz);
                for k in 0..lanes {
                    let x = sx(lane(a, k, esz), esz);
                    let v = match (opc, u) {
                        (0x08, false) => if x > 0 { m } else { 0 },
                        (0x08, true) => if x >= 0 { m } else { 0 },
                        (0x09, false) => if x == 0 { m } else { 0 },
                        (0x09, true) => if x <= 0 { m } else { 0 },
                        (0x0a, false) => if x < 0 { m } else { 0 },
                        (0x0b, false) => x.unsigned_abs(),
                        (0x0b, true) => (x as u64).wrapping_neg(),
                        _ => return undef(),
                    };
                    set_lane(&mut r, k, esz, v);
                }
            }
            (0x12, false) => {
                // XTN/XTN2
                if size == 3 {
                    return undef();
                }
                let mut n = 0u128;
                for k in 0..64 / esz {
                    set_lane(&mut n, k, esz, lane(a, k, esz * 2));
                }
                let d = self.vr(rd);
                self.v[rd as usize] = if q { (d & u64::MAX as u128) | n << 64 } else { n };
                return Ok(());
            }
            (0x13, true) => {
                // SHLL/SHLL2
                if size == 3 {
                    return undef();
                }
                let src = if q { a >> 64 } else { a };
                for k in 0..64 / esz {
                    set_lane(&mut r, k, esz * 2, lane(src, k, esz) << esz);
                }
                self.v[rd as usize] = r;
                return Ok(());
            }
            _ => return undef(),
        }
        self.vd(rd, r, q);
        Ok(())
    }

    /// FP forms of the three-same group (opcode 0x18..0x1f), vector and scalar.
    fn fp_three_same(&mut self, i: u32, scalar: bool) -> Res<()> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let a_bit = bit(i, 23);
        let dbl = bit(i, 22);
        let opc = bits(i, 15, 11);
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        if dbl && !q && !scalar {
            return undef();
        }
        let esz = if dbl { 64 } else { 32 };
        let lanes = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
        let (va, vb, vd) = (self.vr(rn), self.vr(rm), self.vr(rd));
        let get = |v: u128, k: u32| -> f64 {
            let x = lane(v, k, esz);
            if dbl { f64_of(x) } else { f32_of(x) as f64 }
        };
        let put = |x: f64| -> u64 { if dbl { x.to_bits() } else { (x as f32).to_bits() as u64 } };
        // Round to the element precision for single-precision arithmetic.
        let rnd = |x: f64| -> f64 { if dbl { x } else { x as f32 as f64 } };
        let m = ones(esz);
        let mask = |c: bool| if c { m } else { 0 };
        let fused = |a: f64, b: f64, c: f64| -> f64 {
            if dbl { a.mul_add(b, c) } else { (a as f32).mul_add(b as f32, c as f32) as f64 }
        };

        let pairwise = u && matches!(opc, 0x18 | 0x1a | 0x1e) && !(opc == 0x1a && a_bit);
        if pairwise {
            if scalar {
                return undef();
            }
            let op = |x: f64, y: f64| -> f64 {
                match (opc, a_bit) {
                    (0x18, false) => fmaxnm(x, y),
                    (0x18, true) => fminnm(x, y),
                    (0x1a, _) => rnd(x + y),
                    (0x1e, false) => fmax_arm(x, y),
                    _ => fmin_arm(x, y),
                }
            };
            let half = lanes / 2;
            let mut r = 0u128;
            for k in 0..lanes {
                let (src, j) = if k < half { (va, 2 * k) } else { (vb, 2 * (k - half)) };
                set_lane(&mut r, k, esz, put(op(get(src, j), get(src, j + 1))));
            }
            self.vd(rd, r, q);
            return Ok(());
        }

        let mut r = 0u128;
        for k in 0..lanes {
            let (x, y, acc) = (get(va, k), get(vb, k), get(vd, k));
            let v = match (u, a_bit, opc) {
                (false, false, 0x18) => put(fmaxnm(x, y)),
                (false, true, 0x18) => put(fminnm(x, y)),
                (false, false, 0x19) => put(fused(x, y, acc)),
                (false, true, 0x19) => put(fused(-x, y, acc)),
                (false, false, 0x1a) => put(rnd(x + y)),
                (false, true, 0x1a) => put(rnd(x - y)),
                (false, false, 0x1b) => {
                    // FMULX: 0 * inf = 2 (with the product's sign)
                    let p = if (x == 0.0 && y.is_infinite()) || (x.is_infinite() && y == 0.0) {
                        if x.is_sign_negative() != y.is_sign_negative() { -2.0 } else { 2.0 }
                    } else {
                        rnd(x * y)
                    };
                    put(p)
                }
                (false, false, 0x1c) => mask(x == y),
                (false, false, 0x1e) => put(fmax_arm(x, y)),
                (false, true, 0x1e) => put(fmin_arm(x, y)),
                (false, false, 0x1f) => put(fused(-x, y, 2.0)),
                (false, true, 0x1f) => put(fused(-x, y, 3.0) / 2.0),
                (true, false, 0x1b) => put(rnd(x * y)),
                (true, false, 0x1c) => mask(x >= y),
                (true, true, 0x1c) => mask(x > y),
                (true, false, 0x1d) => mask(x.abs() >= y.abs()),
                (true, true, 0x1d) => mask(x.abs() > y.abs()),
                (true, false, 0x1f) => put(rnd(x / y)),
                (true, true, 0x1a) => put(rnd((x - y).abs())),
                _ => return undef(),
            };
            set_lane(&mut r, k, esz, v);
        }
        if scalar {
            // Only some of these exist as scalars; the rest are unallocated.
            let ok = matches!((u, a_bit, opc), (false, false, 0x1b | 0x1c | 0x1f) | (false, true, 0x1f)
                | (true, _, 0x1c | 0x1d) | (true, true, 0x1a));
            if !ok {
                return undef();
            }
            self.v[rd as usize] = r & ones(esz) as u128;
        } else {
            self.vd(rd, r, q);
        }
        Ok(())
    }

    /// FP forms of the two-register-misc group (vector and scalar).
    /// Ok(None) = not an FP op handled here.
    fn fp_misc(&mut self, i: u32, scalar: bool) -> Res<Option<u128>> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let hi = bit(i, 23); // size<1>
        let dbl = bit(i, 22); // size<0>
        let opc = bits(i, 16, 12);
        if dbl && !q && !scalar {
            return undef();
        }
        let a = self.vr(bits(i, 9, 5));
        let esz = if dbl { 64 } else { 32 };
        let lanes = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
        let get = |k: u32| -> f64 {
            let x = lane(a, k, esz);
            if dbl { f64_of(x) } else { f32_of(x) as f64 }
        };
        let put = |x: f64| -> u64 { if dbl { x.to_bits() } else { (x as f32).to_bits() as u64 } };
        let to_int = |x: f64, signed: bool| -> u64 {
            match (dbl, signed) {
                (true, true) => x as i64 as u64,
                (true, false) => x as u64,
                (false, true) => x as i32 as u32 as u64,
                (false, false) => x as u32 as u64,
            }
        };
        let ones_ = ones(esz);
        let op: Box<dyn Fn(u64, f64) -> u64> = match (opc, hi, u) {
            // FCVT{N,M,A}{S,U} (size<1> = 0), FCVT{P,Z}{S,U} (size<1> = 1)
            (0x1a, false, _) => Box::new(move |_, x| to_int(fround(x, 0), !u)),
            (0x1b, false, _) => Box::new(move |_, x| to_int(fround(x, 2), !u)),
            (0x1c, false, _) => Box::new(move |_, x| to_int(fround(x, 4), !u)),
            (0x1a, true, _) => Box::new(move |_, x| to_int(fround(x, 1), !u)),
            (0x1b, true, _) => Box::new(move |_, x| to_int(fround(x, 3), !u)),
            // SCVTF / UCVTF
            (0x1d, false, _) => Box::new(move |raw, _| {
                let v = match (dbl, u) {
                    (true, false) => raw as i64 as f64,
                    (true, true) => raw as f64,
                    (false, false) => raw as u32 as i32 as f32 as f64,
                    (false, true) => raw as u32 as f32 as f64,
                };
                put(v)
            }),
            // FCMGT/FCMGE/FCMEQ/FCMLE/FCMLT against zero
            (0x0c, true, false) => Box::new(move |_, x| if x > 0.0 { ones_ } else { 0 }),
            (0x0c, true, true) => Box::new(move |_, x| if x >= 0.0 { ones_ } else { 0 }),
            (0x0d, true, false) => Box::new(move |_, x| if x == 0.0 { ones_ } else { 0 }),
            (0x0d, true, true) => Box::new(move |_, x| if x <= 0.0 { ones_ } else { 0 }),
            (0x0e, true, false) => Box::new(move |_, x| if x < 0.0 { ones_ } else { 0 }),
            // FABS / FNEG (bit operations, NaNs untouched)
            (0x0f, true, false) if !scalar => Box::new(move |raw, _| raw & (ones_ >> 1)),
            (0x0f, true, true) if !scalar => Box::new(move |raw, _| raw ^ (1u64 << (esz - 1))),
            (0x1f, true, true) if !scalar => Box::new(move |_, x| put(if dbl { x.sqrt() } else { (x as f32).sqrt() as f64 })),
            // FRINTN/M (size<1>=0), FRINTP/Z (size<1>=1), FRINTA/X (U=1, size<1>=0), FRINTI (U=1, size<1>=1)
            (0x18, false, false) if !scalar => Box::new(move |_, x| put(fround(x, 0))),
            (0x19, false, false) if !scalar => Box::new(move |_, x| put(fround(x, 2))),
            (0x18, true, false) if !scalar => Box::new(move |_, x| put(fround(x, 1))),
            (0x19, true, false) if !scalar => Box::new(move |_, x| put(fround(x, 3))),
            (0x18, false, true) if !scalar => Box::new(move |_, x| put(fround(x, 4))),
            (0x19, false, true) | (0x19, true, true) if !scalar => Box::new(move |_, x| put(fround(x, 0))),
            _ => return Ok(None),
        };
        let mut r = 0u128;
        for k in 0..lanes {
            set_lane(&mut r, k, esz, op(lane(a, k, esz), get(k)));
        }
        Ok(Some(r & vmask(q || scalar) & if scalar { ones(esz) as u128 } else { !0 }))
    }

    fn across(&mut self, i: u32) -> Res<()> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 16, 12);
        let (rd, rn) = (i & 31, bits(i, 9, 5));
        let a = self.vr(rn);
        let esz = 8 << size;
        if size == 3 || (size == 2 && !q) {
            return undef();
        }
        let lanes = if q { 128 } else { 64 } / esz;
        let r = match opc {
            0x03 => {
                // SADDLV/UADDLV
                let mut s = 0u64;
                for k in 0..lanes {
                    let x = lane(a, k, esz);
                    s = s.wrapping_add(if u { x } else { sx(x, esz) as u64 });
                }
                s & ones(esz * 2)
            }
            0x0a | 0x1a => {
                let mut acc = lane(a, 0, esz);
                for k in 1..lanes {
                    let op = if opc == 0x0a { 0x0c } else { 0x0d };
                    acc = int3(op, u, esz, acc, lane(a, k, esz), 0).unwrap();
                }
                acc
            }
            0x1b if !u => {
                let mut s = 0u64;
                for k in 0..lanes {
                    s = s.wrapping_add(lane(a, k, esz));
                }
                s & ones(esz)
            }
            _ => return undef(),
        };
        self.v[rd as usize] = r as u128;
        Ok(())
    }

    fn scalar_pairwise(&mut self, i: u32) -> Res<()> {
        if bit(i, 29) {
            // FP pairwise on the two lanes of Vn: FMAXNMP/FMINNMP, FADDP, FMAXP/FMINP.
            let dbl = bit(i, 22);
            let min = bit(i, 23);
            let a = self.vr(bits(i, 9, 5));
            let esz = if dbl { 64 } else { 32 };
            let get = |k: u32| {
                let x = lane(a, k, esz);
                if dbl { f64_of(x) } else { f32_of(x) as f64 }
            };
            let (x, y) = (get(0), get(1));
            let r = match (bits(i, 16, 12), min) {
                (0x0c, false) => fmaxnm(x, y),
                (0x0c, true) => fminnm(x, y),
                (0x0d, false) => if dbl { x + y } else { (x as f32 + y as f32) as f64 },
                (0x0f, false) => fmax_arm(x, y),
                (0x0f, true) => fmin_arm(x, y),
                _ => return undef(),
            };
            self.fp_set(i & 31, r, dbl);
            return Ok(());
        }
        // ADDP (scalar): Dd = Vn.D[0] + Vn.D[1]
        if bits(i, 23, 22) != 3 || bits(i, 16, 12) != 0x1b {
            return undef();
        }
        let a = self.vr(bits(i, 9, 5));
        self.v[(i & 31) as usize] = (a as u64).wrapping_add((a >> 64) as u64) as u128;
        Ok(())
    }

    fn copy(&mut self, i: u32) -> Res<()> {
        let q = bit(i, 30);
        let op = bit(i, 29);
        let imm5 = bits(i, 20, 16);
        let imm4 = bits(i, 14, 11);
        let (rd, rn) = (i & 31, bits(i, 9, 5));
        if imm5 & 0xf == 0 {
            return undef();
        }
        let s = imm5.trailing_zeros();
        let esz = 8 << s;
        let idx = imm5 >> (s + 1);
        if op {
            // INS (element)
            if !q {
                return undef();
            }
            let src_idx = imm4 >> s;
            let x = lane(self.vr(rn), src_idx, esz);
            let mut d = self.vr(rd);
            set_lane(&mut d, idx, esz, x);
            self.v[rd as usize] = d;
            return Ok(());
        }
        match imm4 {
            0b0000 => {
                // DUP (element)
                if esz == 64 && !q {
                    return undef();
                }
                let x = lane(self.vr(rn), idx, esz);
                self.vd(rd, dup64(replicate_lane(x, esz)), q);
            }
            0b0001 => {
                // DUP (general)
                if esz == 64 && !q {
                    return undef();
                }
                let x = self.xr(rn);
                self.vd(rd, dup64(replicate_lane(x & ones(esz), esz)), q);
            }
            0b0011 => {
                // INS (general)
                if !q {
                    return undef();
                }
                let x = self.xr(rn);
                let mut d = self.vr(rd);
                set_lane(&mut d, idx, esz, x);
                self.v[rd as usize] = d;
            }
            0b0101 => {
                // SMOV
                if esz == 64 || (esz == 32 && !q) {
                    return undef();
                }
                let x = sx(lane(self.vr(rn), idx, esz), esz) as u64;
                self.set_xr(rd, if q { x } else { x as u32 as u64 });
            }
            0b0111 => {
                // UMOV
                if (esz == 64) != q {
                    return undef();
                }
                let x = lane(self.vr(rn), idx, esz);
                self.set_xr(rd, x);
            }
            _ => return undef(),
        }
        Ok(())
    }

    fn permute(&mut self, i: u32) -> Res<()> {
        let q = bit(i, 30);
        let size = bits(i, 23, 22);
        let opc = bits(i, 14, 12);
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        if size == 3 && !q {
            return undef();
        }
        let (a, b) = (self.vr(rn), self.vr(rm));
        let esz = 8 << size;
        let lanes = if q { 128 } else { 64 } / esz;
        let half = lanes / 2;
        let mut r = 0u128;
        for k in 0..lanes {
            let v = match opc {
                1 | 5 => {
                    // UZP1 / UZP2
                    let part = (opc == 5) as u32;
                    let j = 2 * k + part;
                    if j < lanes { lane(a, j, esz) } else { lane(b, j - lanes, esz) }
                }
                2 | 6 => {
                    // TRN1 / TRN2
                    let part = (opc == 6) as u32;
                    if k % 2 == 0 { lane(a, k + part, esz) } else { lane(b, k - 1 + part, esz) }
                }
                3 | 7 => {
                    // ZIP1 / ZIP2
                    let base = if opc == 7 { half } else { 0 };
                    let j = base + k / 2;
                    if k % 2 == 0 { lane(a, j, esz) } else { lane(b, j, esz) }
                }
                _ => return undef(),
            };
            set_lane(&mut r, k, esz, v);
        }
        self.vd(rd, r, q);
        Ok(())
    }

    fn ext(&mut self, i: u32) -> Res<()> {
        let q = bit(i, 30);
        let imm4 = bits(i, 14, 11);
        if !q && imm4 >= 8 {
            return undef();
        }
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        let bytes = if q { 16 } else { 8 };
        let (a, b) = (self.vr(rn), self.vr(rm));
        let mut r = 0u128;
        for k in 0..bytes {
            let j = k + imm4;
            let v = if j < bytes { lane(a, j, 8) } else { lane(b, j - bytes, 8) };
            set_lane(&mut r, k, 8, v);
        }
        self.vd(rd, r, q);
        Ok(())
    }

    fn tbl(&mut self, i: u32) -> Res<()> {
        let q = bit(i, 30);
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        let regs = bits(i, 14, 13) + 1;
        let tbx = bit(i, 12);
        let idx = self.vr(rm);
        let mut r = if tbx { self.vr(rd) } else { 0 };
        let bytes = if q { 16 } else { 8 };
        for k in 0..bytes {
            let j = lane(idx, k, 8) as u32;
            if j < 16 * regs {
                let t = self.vr((rn + j / 16) % 32);
                set_lane(&mut r, k, 8, lane(t, j % 16, 8));
            } else if !tbx {
                set_lane(&mut r, k, 8, 0);
            }
        }
        self.vd(rd, r, q);
        Ok(())
    }

    fn mod_imm(&mut self, i: u32) -> Res<()> {
        let q = bit(i, 30);
        let op = bit(i, 29);
        let cmode = bits(i, 15, 12);
        let o2 = bit(i, 11);
        let rd = i & 31;
        if o2 {
            return undef();
        }
        let imm8 = (bits(i, 18, 16) << 5) | bits(i, 9, 5);
        if cmode == 0xf && op && !q {
            return undef();
        }
        let Some(imm) = adv_simd_expand_imm(op, cmode, imm8) else { return undef() };
        let imm = dup64(imm);
        let d = self.vr(rd);
        let r = if cmode < 12 && cmode & 1 == 1 {
            if op { d & !imm } else { d | imm } // BIC / ORR
        } else if op && cmode < 14 {
            !imm // MVNI
        } else {
            imm // MOVI / FMOV
        };
        self.vd(rd, r, q);
        Ok(())
    }

    fn shift_imm(&mut self, i: u32, scalar: bool) -> Res<()> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let immh = bits(i, 22, 19);
        let immhb = bits(i, 22, 16);
        let opc = bits(i, 15, 11);
        let (rd, rn) = (i & 31, bits(i, 9, 5));
        let hs = 31 - immh.leading_zeros(); // highest set bit
        let esz = 8u32 << hs;
        if scalar && esz != 64 {
            return undef();
        }
        if !scalar && esz == 64 && !q && !matches!(opc, 0x10 | 0x11 | 0x14) {
            return undef();
        }
        let rsh = 2 * esz - immhb; // right shift amount (1..=esz)
        let lsh = immhb - esz; // left shift amount
        let (a, d) = (self.vr(rn), self.vr(rd));
        let lanes = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
        let mut r = 0u128;
        let shr = |x: u64, signed: bool, round: bool| -> u64 {
            let x = if signed { sx(x, esz) as i128 } else { x as i128 };
            let x = if round { x + (1i128 << (rsh - 1)) } else { x };
            (x >> rsh) as u64 & ones(esz)
        };
        match opc {
            0x00 | 0x02 | 0x04 | 0x06 => {
                // SSHR/USHR, SSRA/USRA, SRSHR/URSHR, SRSRA/URSRA
                let round = opc & 4 != 0;
                let acc = opc & 2 != 0;
                for k in 0..lanes {
                    let s = shr(lane(a, k, esz), !u, round);
                    let v = if acc { lane(d, k, esz).wrapping_add(s) } else { s };
                    set_lane(&mut r, k, esz, v);
                }
            }
            0x08 if u => {
                // SRI
                let m = if rsh >= esz { 0 } else { ones(esz) >> rsh };
                for k in 0..lanes {
                    let v = (lane(d, k, esz) & !m) | (lane(a, k, esz) >> rsh.min(63) & m);
                    set_lane(&mut r, k, esz, v);
                }
            }
            0x0a => {
                // SHL / SLI
                let m = (ones(esz) << lsh) & ones(esz);
                for k in 0..lanes {
                    let s = lane(a, k, esz) << lsh;
                    let v = if u { (lane(d, k, esz) & !m) | (s & m) } else { s };
                    set_lane(&mut r, k, esz, v);
                }
            }
            0x10 | 0x11 if !u && !scalar => {
                // SHRN / RSHRN (narrowing: source elements are 2*esz)
                if esz == 64 {
                    return undef();
                }
                // esz is the narrow element size here; rsh = 2*esz - immh:immb as usual.
                let wsz = esz * 2;
                let round = opc == 0x11;
                let mut n = 0u128;
                for k in 0..64 / esz {
                    let x = lane(a, k, wsz) as u128;
                    let x = if round { x + (1u128 << (rsh - 1)) } else { x };
                    set_lane(&mut n, k, esz, (x >> rsh) as u64);
                }
                self.v[rd as usize] = if q { (d & u64::MAX as u128) | n << 64 } else { n };
                return Ok(());
            }
            0x14 if !scalar => {
                // SSHLL/USHLL (UXTL/SXTL with shift 0)
                if esz == 64 {
                    return undef();
                }
                let src = if q { a >> 64 } else { a };
                for k in 0..64 / esz {
                    let x = lane(src, k, esz);
                    let x = if u { x } else { sx(x, esz) as u64 };
                    set_lane(&mut r, k, esz * 2, x << lsh);
                }
                self.v[rd as usize] = r;
                return Ok(());
            }
            _ => return undef(),
        }
        if scalar {
            self.v[rd as usize] = r & u64::MAX as u128;
        } else {
            self.vd(rd, r, q);
        }
        Ok(())
    }

    // ---- scalar floating point ----

    fn fp_get(&self, r: u32, dbl: bool) -> f64 {
        let v = self.v[r as usize] as u64;
        if dbl { f64_of(v) } else { f32_of(v) as f64 }
    }

    fn fp_set(&mut self, r: u32, x: f64, dbl: bool) {
        self.v[r as usize] = if dbl { x.to_bits() as u128 } else { (x as f32).to_bits() as u128 };
    }

    fn fp_dp(&mut self, i: u32) -> Res<()> {
        let ftype = bits(i, 23, 22);
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        if bit(i, 29) || !bit(i, 21) {
            return undef();
        }
        if bits(i, 15, 10) == 0 {
            return self.fp_int_conv(i);
        }
        if bit(i, 31) || ftype >= 2 {
            return undef();
        }
        let dbl = ftype == 1;
        if bits(i, 14, 10) == 0b10000 {
            let x = self.fp_get(rn, dbl);
            let opc = bits(i, 20, 15);
            match opc {
                0 => self.v[rd as usize] = self.v[rn as usize] & if dbl { u64::MAX as u128 } else { u32::MAX as u128 },
                1 => {
                    let m = if dbl { 1u128 << 63 } else { 1 << 31 };
                    self.v[rd as usize] = self.v[rn as usize] & (m - 1);
                }
                2 => {
                    let m = if dbl { 1u128 << 63 } else { 1 << 31 };
                    let w = if dbl { u64::MAX as u128 } else { u32::MAX as u128 };
                    self.v[rd as usize] = (self.v[rn as usize] ^ m) & w;
                }
                3 => self.fp_set(rd, if dbl { x.sqrt() } else { (x as f32).sqrt() as f64 }, dbl),
                4 => self.fp_set(rd, x, false),
                5 => self.fp_set(rd, x, true),
                8..=15 => {
                    let mode = match opc - 8 {
                        0 => 0,
                        1 => 1,
                        2 => 2,
                        3 => 3,
                        4 => 4,
                        6 | 7 => 0,
                        _ => return undef(),
                    };
                    self.fp_set(rd, fround(x, mode), dbl)
                }
                _ => return undef(),
            }
            return Ok(());
        }
        if bits(i, 13, 10) == 0b1000 {
            // FCMP/FCMPE
            if bits(i, 15, 14) != 0 || bits(i, 2, 0) != 0 {
                return undef();
            }
            let a = self.fp_get(rn, dbl);
            let b = if bit(i, 3) { 0.0 } else { self.fp_get(rm, dbl) };
            self.nzcv = fcmp_flags(a, b);
            return Ok(());
        }
        if bits(i, 12, 10) == 0b100 {
            // FMOV (scalar, immediate)
            if bits(i, 9, 5) != 0 {
                return undef();
            }
            self.v[rd as usize] = vfp_expand_imm(bits(i, 20, 13), dbl) as u128;
            return Ok(());
        }
        match bits(i, 11, 10) {
            0b01 => {
                // FCCMP/FCCMPE
                if crate::bits::cond_holds(bits(i, 15, 12), self.nzcv) {
                    self.nzcv = fcmp_flags(self.fp_get(rn, dbl), self.fp_get(rm, dbl));
                } else {
                    self.nzcv = (i & 0xf) << 28;
                }
            }
            0b10 => {
                let (a, b) = (self.fp_get(rn, dbl), self.fp_get(rm, dbl));
                let r = if dbl {
                    match bits(i, 15, 12) {
                        0 => a * b,
                        1 => a / b,
                        2 => a + b,
                        3 => a - b,
                        4 => fmax_arm(a, b),
                        5 => fmin_arm(a, b),
                        6 => fmaxnm(a, b),
                        7 => fminnm(a, b),
                        8 => -(a * b),
                        _ => return undef(),
                    }
                } else {
                    let (a, b) = (a as f32, b as f32);
                    (match bits(i, 15, 12) {
                        0 => a * b,
                        1 => a / b,
                        2 => a + b,
                        3 => a - b,
                        4 => fmax_arm(a as f64, b as f64) as f32,
                        5 => fmin_arm(a as f64, b as f64) as f32,
                        6 => fmaxnm(a as f64, b as f64) as f32,
                        7 => fminnm(a as f64, b as f64) as f32,
                        8 => -(a * b),
                        _ => return undef(),
                    }) as f64
                };
                self.fp_set(rd, r, dbl);
            }
            _ => {
                // FCSEL
                let src = if crate::bits::cond_holds(bits(i, 15, 12), self.nzcv) { rn } else { rm };
                let w = if dbl { u64::MAX as u128 } else { u32::MAX as u128 };
                self.v[rd as usize] = self.v[src as usize] & w;
            }
        }
        Ok(())
    }

    fn fp_int_conv(&mut self, i: u32) -> Res<()> {
        let sf = bit(i, 31);
        let ftype = bits(i, 23, 22);
        let rmode = bits(i, 20, 19);
        let opc = bits(i, 18, 16);
        let (rd, rn) = (i & 31, bits(i, 9, 5));
        match (opc, ftype, rmode, sf) {
            (6, 0, 0, false) => self.set_xr(rd, self.v[rn as usize] as u32 as u64),
            (6, 1, 0, true) => self.set_xr(rd, self.v[rn as usize] as u64),
            (6, 2, 1, true) => self.set_xr(rd, (self.v[rn as usize] >> 64) as u64),
            (7, 0, 0, false) => self.v[rd as usize] = self.xr(rn) as u32 as u128,
            (7, 1, 0, true) => self.v[rd as usize] = self.xr(rn) as u128,
            (7, 2, 1, true) => {
                let lo = self.v[rd as usize] & u64::MAX as u128;
                self.v[rd as usize] = lo | (self.xr(rn) as u128) << 64;
            }
            (2 | 3, 0 | 1, 0, _) => {
                // SCVTF / UCVTF
                let x = self.xr(rn);
                let dbl = ftype == 1;
                let signed = opc == 2;
                let r = match (sf, signed) {
                    (true, true) => if dbl { x as i64 as f64 } else { x as i64 as f32 as f64 },
                    (true, false) => if dbl { x as f64 } else { x as f32 as f64 },
                    (false, true) => x as i32 as f64,
                    (false, false) => x as u32 as f64,
                };
                self.fp_set(rd, r, dbl);
            }
            (0 | 1 | 4 | 5, 0 | 1, _, _) => {
                // FCVT{N,P,M,Z}{S,U}, FCVTA{S,U}
                if opc >= 4 && rmode != 0 {
                    return undef();
                }
                let x = self.fp_get(rn, ftype == 1);
                let mode = if opc >= 4 { 4 } else { [0, 1, 2, 3][rmode as usize] };
                let x = fround(x, mode);
                let signed = opc & 1 == 0;
                let r = match (sf, signed) {
                    (true, true) => x as i64 as u64,
                    (true, false) => x as u64,
                    (false, true) => x as i32 as u32 as u64,
                    (false, false) => x as u32 as u64,
                };
                self.set_xr(rd, r);
            }
            _ => return undef(),
        }
        Ok(())
    }

    fn fp_3src(&mut self, i: u32) -> Res<()> {
        let ftype = bits(i, 23, 22);
        if bit(i, 31) || bit(i, 29) || ftype >= 2 {
            return undef();
        }
        let dbl = ftype == 1;
        let (rd, rn, rm, ra) = (i & 31, bits(i, 9, 5), bits(i, 20, 16), bits(i, 14, 10));
        let (n, m, a) = (self.fp_get(rn, dbl), self.fp_get(rm, dbl), self.fp_get(ra, dbl));
        let (neg_a, neg_p) = match (bit(i, 21), bit(i, 15)) {
            (false, false) => (false, false), // FMADD:  a + n*m
            (false, true) => (false, true),   // FMSUB:  a - n*m
            (true, false) => (true, true),    // FNMADD: -a - n*m
            (true, true) => (true, false),    // FNMSUB: -a + n*m
        };
        let n = if neg_p { -n } else { n };
        let a = if neg_a { -a } else { a };
        let r = if dbl { n.mul_add(m, a) } else { (n as f32).mul_add(m as f32, a as f32) as f64 };
        self.fp_set(rd, r, dbl);
        Ok(())
    }

    // ---- structure loads/stores ----

    pub(crate) fn ldst_simd_struct(&mut self, i: u32) -> Res<()> {
        self.check_fp()?;
        let q = bit(i, 30);
        let load = bit(i, 22);
        let post = bit(i, 23);
        let single = bit(i, 24);
        let (rt, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        if !post && rm != 0 {
            return undef();
        }
        let base = self.xs(rn);
        let size = bits(i, 11, 10);
        let total;
        if !single {
            if bit(i, 21) {
                return undef();
            }
            let (rpt, selem) = match bits(i, 15, 12) {
                0b0000 => (1, 4),
                0b0010 => (4, 1),
                0b0100 => (1, 3),
                0b0110 => (3, 1),
                0b0111 => (1, 1),
                0b1000 => (1, 2),
                0b1010 => (2, 1),
                _ => return undef(),
            };
            if size == 3 && !q && selem > 1 {
                return undef();
            }
            let esz = 8 << size;
            let ebytes = esz / 8;
            let lanes = if q { 128 } else { 64 } / esz;
            let regbytes = if q { 16 } else { 8 };
            if selem == 1 {
                // Whole-register transfers.
                for r in 0..rpt {
                    let t = ((rt + r) % 32) as usize;
                    let va = base.wrapping_add((r * regbytes) as u64);
                    if load {
                        self.v[t] = self.read_v(va, regbytes)?;
                    } else {
                        let val = self.v[t] & vmask(q);
                        self.write_v(va, regbytes, val)?;
                    }
                }
            } else {
                let mut regs = [0u128; 4];
                for s in 0..selem {
                    regs[s as usize] = self.v[((rt + s) % 32) as usize];
                }
                let mut off = 0u64;
                for e in 0..lanes {
                    for s in 0..selem as usize {
                        let va = base.wrapping_add(off);
                        if load {
                            let x = self.read(va, ebytes, false)?;
                            set_lane(&mut regs[s], e, esz, x);
                        } else {
                            self.write(va, ebytes, lane(regs[s], e, esz), false)?;
                        }
                        off += ebytes as u64;
                    }
                }
                if load {
                    for s in 0..selem {
                        self.v[((rt + s) % 32) as usize] = regs[s as usize] & vmask(q);
                    }
                }
            }
            total = (rpt * selem * regbytes) as u64;
        } else {
            let opc = bits(i, 15, 13);
            let s = bit(i, 12) as u32;
            let r = bit(i, 21) as u32;
            let selem = (((opc & 1) << 1) | r) + 1;
            let scale = opc >> 1;
            let (esz, index) = match scale {
                0 => (8, (q as u32) << 3 | s << 2 | size),
                1 => {
                    if size & 1 != 0 {
                        return undef();
                    }
                    (16, (q as u32) << 2 | s << 1 | size >> 1)
                }
                2 => {
                    if size & 2 != 0 {
                        return undef();
                    }
                    if size & 1 == 0 {
                        (32, (q as u32) << 1 | s)
                    } else {
                        if s != 0 {
                            return undef();
                        }
                        (64, q as u32)
                    }
                }
                _ => {
                    // LDnR (replicate)
                    if !load || s != 0 {
                        return undef();
                    }
                    let esz = 8 << size;
                    let ebytes = esz / 8;
                    for k in 0..selem {
                        let x = self.read(base.wrapping_add((k * ebytes) as u64), ebytes, false)?;
                        self.v[((rt + k) % 32) as usize] = dup64(replicate_lane(x, esz)) & vmask(q);
                    }
                    let total = (selem * ebytes) as u64;
                    return self.struct_writeback(rn, rm, post, base, total);
                }
            };
            let ebytes = esz / 8;
            for k in 0..selem {
                let t = ((rt + k) % 32) as usize;
                let va = base.wrapping_add((k * ebytes) as u64);
                if load {
                    let x = self.read(va, ebytes, false)?;
                    set_lane(&mut self.v[t], index, esz, x);
                } else {
                    self.write(va, ebytes, lane(self.v[t], index, esz), false)?;
                }
            }
            total = (selem * ebytes) as u64;
        }
        self.struct_writeback(rn, rm, post, base, total)
    }

    fn struct_writeback(&mut self, rn: u32, rm: u32, post: bool, base: u64, total: u64) -> Res<()> {
        if post {
            let off = if rm == 31 { total } else { self.xr(rm) };
            self.set_xs(rn, base.wrapping_add(off));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::Cpu;

    #[test]
    fn expand_imm() {
        assert_eq!(vfp_expand_imm(0x70, true), 1.0f64.to_bits());
        assert_eq!(vfp_expand_imm(0x70, false), 1.0f32.to_bits() as u64);
        assert_eq!(vfp_expand_imm(0x00, true), 2.0f64.to_bits());
        assert_eq!(adv_simd_expand_imm(true, 0xe, 0x81), Some(0xff00_0000_0000_00ff));
    }

    fn cpu() -> Cpu {
        let mut c = Cpu::new(crate::bus::Bus::new(0x4000_0000, 0x1000));
        c.sr.cpacr_el1 = 3 << 20; // FP/SIMD enabled
        c
    }

    #[test]
    fn fp_three_same() {
        let mut c = cpu();
        // fabd d30, d30, d15 (from iperf3)
        c.v[30] = 1.5f64.to_bits() as u128;
        c.v[15] = 4.0f64.to_bits() as u128;
        c.exec(0x7eef_d7de).unwrap();
        assert_eq!(f64::from_bits(c.v[30] as u64), 2.5);
        // fadd v0.4s, v1.4s, v2.4s
        c.v[1] = [1.0f32, 2.0, 3.0, 4.0].iter().enumerate().map(|(k, f)| (f.to_bits() as u128) << (32 * k)).sum();
        c.v[2] = [0.5f32, 0.5, 0.5, 0.5].iter().enumerate().map(|(k, f)| (f.to_bits() as u128) << (32 * k)).sum();
        c.exec(0x4e22_d420).unwrap();
        assert_eq!(f32::from_bits((c.v[0] >> 96) as u32), 4.5);
        // fmla v0.2d, v1.2d, v2.2d: v0 += v1 * v2
        c.v[0] = 1.0f64.to_bits() as u128 | (1.0f64.to_bits() as u128) << 64;
        c.v[1] = 2.0f64.to_bits() as u128 | (3.0f64.to_bits() as u128) << 64;
        c.v[2] = 10.0f64.to_bits() as u128 | (10.0f64.to_bits() as u128) << 64;
        c.exec(0x4e62_cc20).unwrap();
        assert_eq!(f64::from_bits((c.v[0] >> 64) as u64), 31.0);
    }

    #[test]
    fn pmull64() {
        assert_eq!(clmul(3, 3, 64), 5);
        assert_eq!(clmul(1 << 63, 2, 64), 1u128 << 64);
    }
}
