//! Bit-twiddling helpers shared by the decoders.

#[inline(always)]
pub fn bits(x: u32, hi: u32, lo: u32) -> u32 { (x >> lo) & ((1u32 << (hi - lo + 1)) - 1) }

#[inline(always)]
pub fn bit(x: u32, n: u32) -> bool { (x >> n) & 1 != 0 }

#[inline(always)]
pub fn sext(x: u64, width: u32) -> i64 {
    let s = 64 - width;
    ((x << s) as i64) >> s
}

#[inline(always)]
pub fn ones(n: u32) -> u64 { if n >= 64 { !0 } else { (1u64 << n) - 1 } }

/// Rotate right within `width` bits.
#[inline(always)]
pub fn ror(x: u64, r: u32, width: u32) -> u64 {
    let r = r % width;
    if r == 0 {
        return x & ones(width);
    }
    ((x >> r) | (x << (width - r))) & ones(width)
}

#[inline(always)]
fn replicate(x: u64, esize: u32, width: u32) -> u64 {
    let mut r = 0;
    let mut i = 0;
    while i < width {
        r |= x << i;
        i += esize;
    }
    r & ones(width)
}

/// DecodeBitMasks() from the ARM ARM. Returns (wmask, tmask), or None if reserved.
pub fn decode_bit_masks(n: u32, imms: u32, immr: u32, immediate: bool, width: u32) -> Option<(u64, u64)> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros(); // highest set bit index
    if len < 1 {
        return None;
    }
    let levels = (1u32 << len) - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let diff = s.wrapping_sub(r) & levels;
    let esize = 1u32 << len;
    let d = diff & levels;
    let welem = ones(s + 1);
    let telem = ones(d + 1);
    let wmask = replicate(ror(welem, r, esize), esize, width);
    let tmask = replicate(telem, esize, width);
    Some((wmask, tmask))
}

/// Condition evaluation; `nzcv` holds flags in bits 31..28.
#[inline(always)]
pub fn cond_holds(cond: u32, nzcv: u32) -> bool {
    let n = nzcv >> 31 & 1 != 0;
    let z = nzcv >> 30 & 1 != 0;
    let c = nzcv >> 29 & 1 != 0;
    let v = nzcv >> 28 & 1 != 0;
    let r = match cond >> 1 {
        0 => z,
        1 => c,
        2 => n,
        3 => v,
        4 => c && !z,
        5 => n == v,
        6 => n == v && !z,
        _ => true,
    };
    if cond & 1 == 1 && cond != 0xf { !r } else { r }
}

/// AddWithCarry for 32- or 64-bit operands. Returns (result, nzcv in bits 31..28).
#[inline(always)]
pub fn add_with_carry(x: u64, y: u64, carry: bool, sf: bool) -> (u64, u32) {
    if sf {
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(carry as u64);
        let c = c1 || c2;
        let v = ((x ^ r) & (y ^ r)) >> 63 != 0;
        let n = r >> 63;
        let z = (r == 0) as u64;
        (r, ((n << 31) | (z << 30) | ((c as u64) << 29) | ((v as u64) << 28)) as u32)
    } else {
        let x = x as u32;
        let y = y as u32;
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(carry as u32);
        let c = c1 || c2;
        let v = ((x ^ r) & (y ^ r)) >> 31 != 0;
        let n = r >> 31;
        let z = (r == 0) as u32;
        (r as u64, (n << 31) | (z << 30) | ((c as u32) << 29) | ((v as u32) << 28))
    }
}

#[inline(always)]
pub fn nz_flags(r: u64, sf: bool) -> u32 {
    if sf {
        (((r >> 63) as u32) << 31) | (((r == 0) as u32) << 30)
    } else {
        let r = r as u32;
        ((r >> 31) << 31) | (((r == 0) as u32) << 30)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmasks() {
        // and x0, x0, #0xff  => N=1 immr=0 imms=7
        assert_eq!(decode_bit_masks(1, 7, 0, true, 64).unwrap().0, 0xff);
        // and w0, w0, #0x55555555 => N=0 immr=0 imms=0b111100
        assert_eq!(decode_bit_masks(0, 0x3c, 0, true, 32).unwrap().0, 0x5555_5555);
        // lsr x0, x1, #4 == ubfm x0, x1, #4, #63
        let (w, t) = decode_bit_masks(1, 63, 4, false, 64).unwrap();
        assert_eq!(w, !0);
        assert_eq!(t, 0x0fff_ffff_ffff_ffff);
    }

    #[test]
    fn adc() {
        let (r, f) = add_with_carry(5, !3u64, true, true); // 5 - 3
        assert_eq!(r, 2);
        assert_eq!(f, 0x2000_0000); // C set
        let (_, f) = add_with_carry(0x7fff_ffff, 1, false, false);
        assert_eq!(f, 0x9000_0000); // N and V
    }
}
