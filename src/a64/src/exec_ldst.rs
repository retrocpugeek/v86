//! Loads and stores (general-purpose and FP/SIMD scalar registers).

use crate::bits::*;
use crate::cpu::Cpu;
use crate::exc::{undef, Res};
use crate::exec_int::extend_reg;

enum Addr {
    /// Plain address, no writeback.
    Offset(u64),
    /// Access at the address, then write back `wb` to Rn.
    Post(u64, u64),
    /// Access at the address and write it back to Rn.
    Pre(u64),
}

impl Addr {
    fn va(&self) -> u64 {
        match *self {
            Addr::Offset(a) | Addr::Post(a, _) | Addr::Pre(a) => a,
        }
    }
}

impl Cpu {
    fn writeback(&mut self, rn: u32, a: &Addr) {
        match *a {
            Addr::Offset(_) => {}
            Addr::Post(_, wb) => self.set_xs(rn, wb),
            Addr::Pre(va) => self.set_xs(rn, va),
        }
    }

    pub(crate) fn exec_ldst(&mut self, i: u32) -> Res<()> {
        let v = bit(i, 26);
        let op2 = bits(i, 24, 23);
        match bits(i, 29, 28) {
            0 => {
                if v {
                    if bit(i, 31) {
                        return undef();
                    }
                    self.ldst_simd_struct(i)
                } else if op2 & 2 == 0 {
                    self.ldst_exclusive(i)
                } else {
                    undef()
                }
            }
            1 => {
                if op2 & 2 != 0 {
                    return undef();
                }
                self.ldr_literal(i)
            }
            2 => self.ldst_pair(i),
            _ => {
                let rn = bits(i, 9, 5);
                let base = self.xs(rn);
                let size = bits(i, 31, 30);
                let opc = bits(i, 23, 22);
                let scale = if v && opc & 2 != 0 { 4 } else { size };
                if op2 & 2 != 0 {
                    let off = (bits(i, 21, 10) as u64) << scale;
                    return self.ldst_single(i, Addr::Offset(base.wrapping_add(off)), false);
                }
                if !bit(i, 21) {
                    let imm = sext(bits(i, 20, 12) as u64, 9) as u64;
                    let (a, unpriv) = match bits(i, 11, 10) {
                        0 => (Addr::Offset(base.wrapping_add(imm)), false),
                        1 => (Addr::Post(base, base.wrapping_add(imm)), false),
                        2 => {
                            if v {
                                return undef();
                            }
                            (Addr::Offset(base.wrapping_add(imm)), true)
                        }
                        _ => (Addr::Pre(base.wrapping_add(imm)), false),
                    };
                    return self.ldst_single(i, a, unpriv);
                }
                if bits(i, 11, 10) == 2 {
                    let option = bits(i, 15, 13);
                    if option & 2 == 0 {
                        return undef();
                    }
                    let shift = if bit(i, 12) { scale } else { 0 };
                    let off = extend_reg(self.xr(bits(i, 20, 16)), option, shift, true);
                    return self.ldst_single(i, Addr::Offset(base.wrapping_add(off)), false);
                }
                // Atomic memory operations (LSE) and PAC loads are not implemented.
                undef()
            }
        }
    }

    fn ldst_single(&mut self, i: u32, a: Addr, unpriv: bool) -> Res<()> {
        let rt = i & 31;
        let rn = bits(i, 9, 5);
        let size = bits(i, 31, 30);
        let opc = bits(i, 23, 22);
        let va = a.va();
        let pre_post = !matches!(a, Addr::Offset(_));
        if bit(i, 26) {
            self.check_fp()?;
            let bytes = if opc & 2 != 0 {
                if size != 0 {
                    return undef();
                }
                16
            } else {
                1 << size
            };
            if opc & 1 != 0 {
                let val = self.read_v(va, bytes)?;
                self.v[rt as usize] = val;
            } else {
                let val = self.v[rt as usize];
                self.write_v(va, bytes, val)?;
            }
            self.writeback(rn, &a);
            return Ok(());
        }
        let bytes = 1u32 << size;
        match opc {
            0 => {
                let val = self.xr(rt);
                self.write(va, bytes, val, unpriv)?;
            }
            1 => {
                let val = self.read(va, bytes, unpriv)?;
                self.writeback(rn, &a);
                self.set_xr(rt, val);
                return Ok(());
            }
            2 => {
                if size == 3 {
                    // PRFM / PRFUM: no-op, but only for the non-writeback forms.
                    if pre_post || unpriv {
                        return undef();
                    }
                    return Ok(());
                }
                let val = self.read(va, bytes, unpriv)?;
                self.writeback(rn, &a);
                self.set_xr(rt, sext(val, 8 * bytes) as u64);
                return Ok(());
            }
            _ => {
                if size >= 2 {
                    return undef();
                }
                let val = self.read(va, bytes, unpriv)?;
                self.writeback(rn, &a);
                self.set_xr(rt, sext(val, 8 * bytes) as u32 as u64);
                return Ok(());
            }
        }
        self.writeback(rn, &a);
        Ok(())
    }

    fn ldr_literal(&mut self, i: u32) -> Res<()> {
        let rt = i & 31;
        let va = self.insn_pc.wrapping_add((sext(bits(i, 23, 5) as u64, 19) << 2) as u64);
        let opc = bits(i, 31, 30);
        if bit(i, 26) {
            self.check_fp()?;
            let bytes = match opc {
                0 => 4,
                1 => 8,
                2 => 16,
                _ => return undef(),
            };
            self.v[rt as usize] = self.read_v(va, bytes)?;
            return Ok(());
        }
        match opc {
            0 => {
                let v = self.read(va, 4, false)?;
                self.set_xr(rt, v);
            }
            1 => {
                let v = self.read(va, 8, false)?;
                self.set_xr(rt, v);
            }
            2 => {
                let v = self.read(va, 4, false)?;
                self.set_xr(rt, sext(v, 32) as u64);
            }
            _ => {} // PRFM (literal)
        }
        Ok(())
    }

    fn ldst_pair(&mut self, i: u32) -> Res<()> {
        let rt = i & 31;
        let rt2 = bits(i, 14, 10);
        let rn = bits(i, 9, 5);
        let opc = bits(i, 31, 30);
        let load = bit(i, 22);
        let v = bit(i, 26);
        let (bytes, signed) = if v {
            self.check_fp()?;
            match opc {
                0 => (4, false),
                1 => (8, false),
                2 => (16, false),
                _ => return undef(),
            }
        } else {
            match opc {
                0 => (4, false),
                1 if load => (4, true),
                2 => (8, false),
                _ => return undef(),
            }
        };
        let off = (sext(bits(i, 21, 15) as u64, 7) as u64).wrapping_mul(bytes as u64);
        let base = self.xs(rn);
        let a = match bits(i, 24, 23) {
            0 | 2 => Addr::Offset(base.wrapping_add(off)),
            1 => Addr::Post(base, base.wrapping_add(off)),
            _ => Addr::Pre(base.wrapping_add(off)),
        };
        let va = a.va();
        let va2 = va.wrapping_add(bytes as u64);
        if v {
            if load {
                let a1 = self.read_v(va, bytes)?;
                let a2 = self.read_v(va2, bytes)?;
                self.v[rt as usize] = a1;
                self.v[rt2 as usize] = a2;
            } else {
                let (a1, a2) = (self.v[rt as usize], self.v[rt2 as usize]);
                self.write_v(va, bytes, a1)?;
                self.write_v(va2, bytes, a2)?;
            }
        } else if load {
            let a1 = self.read(va, bytes, false)?;
            let a2 = self.read(va2, bytes, false)?;
            let (a1, a2) = if signed { (sext(a1, 32) as u64, sext(a2, 32) as u64) } else { (a1, a2) };
            self.writeback(rn, &a);
            self.set_xr(rt, a1);
            self.set_xr(rt2, a2);
            return Ok(());
        } else {
            let (a1, a2) = (self.xr(rt), self.xr(rt2));
            self.write(va, bytes, a1, false)?;
            self.write(va2, bytes, a2, false)?;
        }
        self.writeback(rn, &a);
        Ok(())
    }

    fn ldst_exclusive(&mut self, i: u32) -> Res<()> {
        let rt = i & 31;
        let rt2 = bits(i, 14, 10);
        let rs = bits(i, 20, 16);
        let rn = bits(i, 9, 5);
        let size = bits(i, 31, 30);
        let load = bit(i, 22);
        let o2 = bit(i, 23);
        let o1 = bit(i, 21);
        let o0 = bit(i, 15);
        let va = self.xs(rn);
        match (o2, o1) {
            (false, pair) => {
                if pair && size < 2 {
                    return undef(); // CASP
                }
                let elem = 1u32 << size;
                let total = if pair { elem * 2 } else { elem };
                if va & (total as u64 - 1) != 0 {
                    return Err(self.alignment_fault(va, !load));
                }
                if load {
                    if pair {
                        let a = self.read(va, elem, false)?;
                        let b = self.read(va + elem as u64, elem, false)?;
                        self.set_xr(rt, a);
                        self.set_xr(rt2, b);
                    } else {
                        let a = self.read(va, elem, false)?;
                        self.set_xr(rt, a);
                    }
                    (self.excl_addr, self.excl_size) = (va, total);
                } else {
                    let ok = self.excl_size == total && self.excl_addr == va;
                    self.excl_size = 0;
                    if ok {
                        if pair {
                            if elem == 8 {
                                let v = self.xr(rt) as u128 | (self.xr(rt2) as u128) << 64;
                                self.write128(va, v)?;
                            } else {
                                let v = (self.xr(rt) & 0xffff_ffff) | self.xr(rt2) << 32;
                                self.write(va, 8, v, false)?;
                            }
                        } else {
                            let v = self.xr(rt);
                            self.write(va, elem, v, false)?;
                        }
                    }
                    self.set_xr(rs, if ok { 0 } else { 1 });
                }
            }
            (true, false) => {
                // LDAR / STLR (o0 = 1); LDLAR/STLLR (o0 = 0) are ARMv8.1.
                if !o0 {
                    return undef();
                }
                let bytes = 1u32 << size;
                if va & (bytes as u64 - 1) != 0 {
                    return Err(self.alignment_fault(va, !load));
                }
                if load {
                    let v = self.read(va, bytes, false)?;
                    self.set_xr(rt, v);
                } else {
                    let v = self.xr(rt);
                    self.write(va, bytes, v, false)?;
                }
            }
            (true, true) => return undef(), // CAS
        }
        Ok(())
    }
}
