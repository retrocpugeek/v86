//! Integer data processing and branches.

use crate::bits::*;
use crate::cpu::{Cpu, PSTATE_C};
use crate::exc::{undef, Res};

#[inline(always)]
fn width(sf: bool) -> u32 { if sf { 64 } else { 32 } }

/// Shift for the shifted-register forms (type 3 = ROR).
#[inline(always)]
pub(crate) fn shift_reg(v: u64, ty: u32, amount: u32, sf: bool) -> u64 {
    let w = width(sf);
    let v = v & ones(w);
    let r = match ty {
        0 => v << amount,
        1 => v >> amount,
        2 => (sext(v, w) >> amount) as u64,
        _ => ror(v, amount, w),
    };
    r & ones(w)
}

/// ExtendReg(): option selects UXTB..SXTX, then shift left by `shift`.
#[inline(always)]
pub fn extend_reg(v: u64, option: u32, shift: u32, sf: bool) -> u64 {
    let len = 8 << (option & 3);
    let e = if option & 4 != 0 { sext(v & ones(len), len) as u64 } else { v & ones(len) };
    (e << shift) & ones(width(sf))
}

fn crc32(acc: u32, data: u64, bytes: u32, poly: u32) -> u32 {
    let mut crc = acc;
    for i in 0..bytes {
        crc ^= (data >> (8 * i)) as u8 as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ poly } else { crc >> 1 };
        }
    }
    crc
}

impl Cpu {
    pub(crate) fn exec_dp_imm(&mut self, i: u32) -> Res<()> {
        let rd = i & 31;
        let rn = bits(i, 9, 5);
        let sf = bit(i, 31);
        let w = width(sf);
        match bits(i, 25, 23) {
            0b000 | 0b001 => {
                // ADR / ADRP
                let imm = sext(((bits(i, 23, 5) << 2) | bits(i, 30, 29)) as u64, 21);
                let pc = self.insn_pc;
                let r = if bit(i, 31) {
                    (pc & !0xfff).wrapping_add((imm << 12) as u64)
                } else {
                    pc.wrapping_add(imm as u64)
                };
                self.set_xr(rd, r);
            }
            0b010 => {
                // ADD/ADDS/SUB/SUBS (immediate)
                let sub = bit(i, 30);
                let setflags = bit(i, 29);
                let mut imm = bits(i, 21, 10) as u64;
                if bit(i, 22) {
                    imm <<= 12;
                }
                let a = self.xs(rn);
                let (r, nzcv) = if sub { add_with_carry(a, !imm, true, sf) } else { add_with_carry(a, imm, false, sf) };
                let r = r & ones(w);
                if setflags {
                    self.nzcv = nzcv;
                    self.set_xr(rd, r);
                } else {
                    self.set_xs(rd, r);
                }
            }
            0b100 => {
                // Logical (immediate)
                let n = bits(i, 22, 22);
                if !sf && n != 0 {
                    return undef();
                }
                let Some((imm, _)) = decode_bit_masks(n, bits(i, 15, 10), bits(i, 21, 16), true, w) else {
                    return undef();
                };
                let a = self.xr(rn);
                let opc = bits(i, 30, 29);
                let r = match opc {
                    0 | 3 => a & imm,
                    1 => a | imm,
                    _ => a ^ imm,
                } & ones(w);
                if opc == 3 {
                    self.nzcv = nz_flags(r, sf);
                    self.set_xr(rd, r);
                } else {
                    self.set_xs(rd, r);
                }
            }
            0b101 => {
                // MOVN / MOVZ / MOVK
                let opc = bits(i, 30, 29);
                let hw = bits(i, 22, 21);
                if opc == 1 || (!sf && hw > 1) {
                    return undef();
                }
                let pos = hw * 16;
                let imm = (bits(i, 20, 5) as u64) << pos;
                let r = match opc {
                    0 => !imm,
                    2 => imm,
                    _ => (self.xr(rd) & !(0xffffu64 << pos)) | imm,
                };
                self.set_xr_sf(rd, r, sf);
            }
            0b110 => {
                // SBFM / BFM / UBFM
                let opc = bits(i, 30, 29);
                let n = bits(i, 22, 22);
                let immr = bits(i, 21, 16);
                let imms = bits(i, 15, 10);
                if opc == 3 || n != sf as u32 || (!sf && (immr | imms) & 0x20 != 0) {
                    return undef();
                }
                let Some((wmask, tmask)) = decode_bit_masks(n, imms, immr, false, w) else {
                    return undef();
                };
                let src = self.xr(rn) & ones(w);
                let dst = if opc == 1 { self.xr(rd) & ones(w) } else { 0 };
                let bot = (dst & !wmask) | (ror(src, immr, w) & wmask);
                let top = match opc {
                    0 => {
                        if src >> imms & 1 != 0 {
                            ones(w)
                        } else {
                            0
                        }
                    }
                    1 => dst,
                    _ => 0,
                };
                self.set_xr_sf(rd, (top & !tmask) | (bot & tmask), sf);
            }
            0b111 => {
                // EXTR
                let rm = bits(i, 20, 16);
                let imms = bits(i, 15, 10);
                if bits(i, 30, 29) != 0 || bit(i, 21) || bit(i, 22) != sf || (!sf && imms >= 32) {
                    return undef();
                }
                let lo = self.xr(rm) & ones(w);
                let hi = self.xr(rn) & ones(w);
                let r = if imms == 0 { lo } else { (lo >> imms) | (hi << (w - imms)) };
                self.set_xr_sf(rd, r & ones(w), sf);
            }
            _ => return undef(),
        }
        Ok(())
    }

    pub(crate) fn exec_dp_reg(&mut self, i: u32) -> Res<()> {
        let rd = i & 31;
        let rn = bits(i, 9, 5);
        let rm = bits(i, 20, 16);
        let sf = bit(i, 31);
        let w = width(sf);
        let op2 = bits(i, 24, 21);
        if !bit(i, 28) {
            if op2 & 8 == 0 {
                // Logical (shifted register)
                let amount = bits(i, 15, 10);
                if !sf && amount >= 32 {
                    return undef();
                }
                let mut b = shift_reg(self.xr(rm), bits(i, 23, 22), amount, sf);
                if bit(i, 21) {
                    b = !b & ones(w);
                }
                let a = self.xr(rn);
                let opc = bits(i, 30, 29);
                let r = match opc {
                    0 | 3 => a & b,
                    1 => a | b,
                    _ => a ^ b,
                } & ones(w);
                if opc == 3 {
                    self.nzcv = nz_flags(r, sf);
                }
                self.set_xr(rd, r);
            } else if op2 & 1 == 0 {
                // ADD/SUB (shifted register)
                let ty = bits(i, 23, 22);
                let amount = bits(i, 15, 10);
                if ty == 3 || (!sf && amount >= 32) {
                    return undef();
                }
                let b = shift_reg(self.xr(rm), ty, amount, sf);
                self.addsub(i, self.xr(rn), b, sf, false);
            } else {
                // ADD/SUB (extended register)
                let shift = bits(i, 12, 10);
                if bits(i, 23, 22) != 0 || shift > 4 {
                    return undef();
                }
                let b = extend_reg(self.xr(rm), bits(i, 15, 13), shift, sf);
                self.addsub(i, self.xs(rn), b, sf, true);
            }
            return Ok(());
        }
        match op2 {
            0b0000 => {
                // ADC/ADCS/SBC/SBCS
                if bits(i, 15, 10) != 0 {
                    return undef();
                }
                let mut b = self.xr(rm);
                if bit(i, 30) {
                    b = !b;
                }
                let (r, nzcv) = add_with_carry(self.xr(rn), b, self.nzcv & PSTATE_C != 0, sf);
                if bit(i, 29) {
                    self.nzcv = nzcv;
                }
                self.set_xr_sf(rd, r, sf);
            }
            0b0010 => {
                // CCMN/CCMP (register or immediate)
                if !bit(i, 29) || bit(i, 10) || bit(i, 4) {
                    return undef();
                }
                let cond = bits(i, 15, 12);
                if cond_holds(cond, self.nzcv) {
                    let b = if bit(i, 11) { rm as u64 } else { self.xr(rm) };
                    let a = self.xr(rn);
                    let (_, nzcv) = if bit(i, 30) { add_with_carry(a, !b, true, sf) } else { add_with_carry(a, b, false, sf) };
                    self.nzcv = nzcv;
                } else {
                    self.nzcv = (i & 0xf) << 28;
                }
            }
            0b0100 => {
                // CSEL/CSINC/CSINV/CSNEG
                if bit(i, 29) || bit(i, 11) {
                    return undef();
                }
                let r = if cond_holds(bits(i, 15, 12), self.nzcv) {
                    self.xr(rn)
                } else {
                    let b = self.xr(rm);
                    match (bit(i, 30), bit(i, 10)) {
                        (false, false) => b,
                        (false, true) => b.wrapping_add(1),
                        (true, false) => !b,
                        (true, true) => b.wrapping_neg(),
                    }
                };
                self.set_xr_sf(rd, r, sf);
            }
            0b0110 => {
                if bit(i, 29) {
                    return undef();
                }
                if bit(i, 30) {
                    self.dp_1src(i, sf)?
                } else {
                    self.dp_2src(i, sf)?
                }
            }
            0b1000..=0b1111 => self.dp_3src(i, sf)?,
            _ => return undef(),
        }
        Ok(())
    }

    #[inline(always)]
    fn addsub(&mut self, i: u32, a: u64, b: u64, sf: bool, rd_sp: bool) {
        let rd = i & 31;
        let (r, nzcv) = if bit(i, 30) { add_with_carry(a, !b, true, sf) } else { add_with_carry(a, b, false, sf) };
        let r = r & ones(width(sf));
        if bit(i, 29) {
            self.nzcv = nzcv;
            self.set_xr(rd, r);
        } else if rd_sp {
            self.set_xs(rd, r);
        } else {
            self.set_xr(rd, r);
        }
    }

    fn dp_1src(&mut self, i: u32, sf: bool) -> Res<()> {
        if bits(i, 20, 16) != 0 {
            return undef();
        }
        let rd = i & 31;
        let x = self.xr(bits(i, 9, 5));
        let r = match (bits(i, 15, 10), sf) {
            (0, true) => x.reverse_bits(),
            (0, false) => (x as u32).reverse_bits() as u64,
            (1, _) => {
                let x = if sf { x } else { x & 0xffff_ffff };
                ((x & 0x00ff_00ff_00ff_00ff) << 8) | ((x >> 8) & 0x00ff_00ff_00ff_00ff)
            }
            (2, false) => (x as u32).swap_bytes() as u64,
            (2, true) => {
                let lo = (x as u32).swap_bytes() as u64;
                let hi = ((x >> 32) as u32).swap_bytes() as u64;
                lo | hi << 32
            }
            (3, true) => x.swap_bytes(),
            (4, true) => x.leading_zeros() as u64,
            (4, false) => (x as u32).leading_zeros() as u64,
            (5, true) => ((x ^ (x << 1)) | 1).leading_zeros() as u64,
            (5, false) => {
                let x = x as u32;
                ((x ^ (x << 1)) | 1).leading_zeros() as u64
            }
            _ => return undef(),
        };
        self.set_xr_sf(rd, r, sf);
        Ok(())
    }

    fn dp_2src(&mut self, i: u32, sf: bool) -> Res<()> {
        let rd = i & 31;
        let a = self.xr(bits(i, 9, 5));
        let b = self.xr(bits(i, 20, 16));
        let w = width(sf);
        let opc = bits(i, 15, 10);
        let r = match opc {
            2 => {
                if sf {
                    a.checked_div(b).unwrap_or(0)
                } else {
                    (a as u32).checked_div(b as u32).unwrap_or(0) as u64
                }
            }
            3 => {
                if sf {
                    if b == 0 { 0 } else { (a as i64).wrapping_div(b as i64) as u64 }
                } else if b as u32 == 0 {
                    0
                } else {
                    (a as i32).wrapping_div(b as i32) as u32 as u64
                }
            }
            8..=11 => shift_reg(a, opc - 8, (b % w as u64) as u32, sf),
            0x10..=0x17 => {
                let sz = opc & 3;
                if (sz == 3) != sf {
                    return undef();
                }
                let poly = if opc & 4 != 0 { 0x82f6_3b78 } else { 0xedb8_8320 };
                crc32(a as u32, b, 1 << sz, poly) as u64
            }
            _ => return undef(),
        };
        self.set_xr_sf(rd, r, sf);
        Ok(())
    }

    fn dp_3src(&mut self, i: u32, sf: bool) -> Res<()> {
        if bits(i, 30, 29) != 0 {
            return undef();
        }
        let rd = i & 31;
        let n = self.xr(bits(i, 9, 5));
        let m = self.xr(bits(i, 20, 16));
        let a = self.xr(bits(i, 14, 10));
        let sub = bit(i, 15);
        let r = match (bits(i, 23, 21), sf) {
            (0, _) => {
                let p = n.wrapping_mul(m);
                if sub { a.wrapping_sub(p) } else { a.wrapping_add(p) }
            }
            (1, true) => {
                let p = (n as i32 as i64).wrapping_mul(m as i32 as i64) as u64;
                if sub { a.wrapping_sub(p) } else { a.wrapping_add(p) }
            }
            (5, true) => {
                let p = (n as u32 as u64).wrapping_mul(m as u32 as u64);
                if sub { a.wrapping_sub(p) } else { a.wrapping_add(p) }
            }
            (2, true) if !sub => ((n as i64 as i128 * m as i64 as i128) >> 64) as u64,
            (6, true) if !sub => ((n as u128 * m as u128) >> 64) as u64,
            _ => return undef(),
        };
        self.set_xr_sf(rd, r, sf);
        Ok(())
    }

    pub(crate) fn exec_branch_sys(&mut self, i: u32) -> Res<()> {
        let pc = self.insn_pc;
        match bits(i, 31, 29) {
            0b010 => {
                // B.cond
                if bit(i, 25) || bit(i, 24) || bit(i, 4) {
                    return undef();
                }
                if cond_holds(i & 0xf, self.nzcv) {
                    self.pc = pc.wrapping_add((sext(bits(i, 23, 5) as u64, 19) << 2) as u64);
                }
            }
            0b000 | 0b100 => {
                // B / BL
                if bit(i, 31) {
                    self.x[30] = pc.wrapping_add(4);
                }
                self.pc = pc.wrapping_add((sext(bits(i, 25, 0) as u64, 26) << 2) as u64);
            }
            0b001 | 0b101 => {
                let rt = i & 31;
                if !bit(i, 25) {
                    // CBZ / CBNZ
                    let v = if bit(i, 31) { self.xr(rt) } else { self.xr(rt) & 0xffff_ffff };
                    if (v == 0) != bit(i, 24) {
                        self.pc = pc.wrapping_add((sext(bits(i, 23, 5) as u64, 19) << 2) as u64);
                    }
                } else {
                    // TBZ / TBNZ
                    let b = (bits(i, 31, 31) << 5) | bits(i, 23, 19);
                    if (self.xr(rt) >> b & 1 != 0) == bit(i, 24) {
                        self.pc = pc.wrapping_add((sext(bits(i, 18, 5) as u64, 14) << 2) as u64);
                    }
                }
            }
            0b110 => {
                if bit(i, 25) {
                    return self.branch_reg(i);
                }
                match bits(i, 25, 22) {
                    0b0000..=0b0011 => return self.exception_gen(i),
                    0b0100 => return self.exec_system(i),
                    _ => return undef(),
                }
            }
            _ => return undef(),
        }
        Ok(())
    }

    fn branch_reg(&mut self, i: u32) -> Res<()> {
        if bits(i, 20, 16) != 0x1f || bits(i, 15, 10) != 0 || bits(i, 4, 0) != 0 {
            return undef();
        }
        let rn = bits(i, 9, 5);
        match bits(i, 24, 21) {
            0 | 2 => self.pc = self.xr(rn),
            1 => {
                let t = self.xr(rn);
                self.x[30] = self.insn_pc.wrapping_add(4);
                self.pc = t;
            }
            4 if rn == 31 => return self.eret(),
            _ => return undef(),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc() {
        // CRC-32 of "123456789" is 0xcbf43926 (with pre/post inversion done by software).
        let mut c = 0xffff_ffffu32;
        for b in b"123456789" {
            c = crc32(c, *b as u64, 1, 0xedb8_8320);
        }
        assert_eq!(!c, 0xcbf4_3926);
        let mut c = 0xffff_ffffu32;
        for b in b"123456789" {
            c = crc32(c, *b as u64, 1, 0x82f6_3b78);
        }
        assert_eq!(!c, 0xe306_9283);
    }

    #[test]
    fn extend() {
        assert_eq!(extend_reg(0x80, 4, 0, true), 0xffff_ffff_ffff_ff80);
        assert_eq!(extend_reg(0xffff_ffff, 6, 2, true), 0xffff_ffff_ffff_fffc);
        assert_eq!(extend_reg(0x1_0000_00ff, 2, 0, true), 0xff);
    }
}
