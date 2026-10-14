//! Cryptographic extension: AES (AESE/AESD/AESMC/AESIMC) and SHA1/SHA256.
//!
//! ID_AA64ISAR0_EL1 advertises AES+PMULL, SHA1 and SHA2 (as on cortex-a53), so
//! userspace (OpenSSL, mbedtls, the kernel's crypto drivers) selects these
//! instructions from HWCAP. PMULL lives in exec_simd.rs.

use crate::bits::*;
use crate::cpu::Cpu;
use crate::exc::{undef, Res};

const fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 };
        b >>= 1;
    }
    r
}

const SBOX: [u8; 256] = {
    let mut s = [0u8; 256];
    let mut x = 0;
    while x < 256 {
        // multiplicative inverse (x^254), then the affine transform
        let mut inv = 1u8;
        let mut k = 0;
        while k < 254 {
            inv = gf_mul(inv, x as u8);
            k += 1;
        }
        if x == 0 {
            inv = 0;
        }
        s[x] = inv ^ inv.rotate_left(1) ^ inv.rotate_left(2) ^ inv.rotate_left(3) ^ inv.rotate_left(4) ^ 0x63;
        x += 1;
    }
    s
};

const INV_SBOX: [u8; 256] = {
    let mut s = [0u8; 256];
    let mut x = 0;
    while x < 256 {
        s[SBOX[x] as usize] = x as u8;
        x += 1;
    }
    s
};

// The state is the 128-bit register as 16 bytes, byte 4*c + r = row r, column c.

fn aes_round(state: u128, inverse: bool) -> u128 {
    let b = state.to_le_bytes();
    let mut o = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            let src = if inverse { r + 4 * ((c + 4 - r) % 4) } else { r + 4 * ((c + r) % 4) };
            o[r + 4 * c] = if inverse { INV_SBOX[b[src] as usize] } else { SBOX[b[src] as usize] };
        }
    }
    u128::from_le_bytes(o)
}

fn aes_mix(state: u128, inverse: bool) -> u128 {
    let b = state.to_le_bytes();
    let mut o = [0u8; 16];
    let m: [u8; 4] = if inverse { [0x0e, 0x0b, 0x0d, 0x09] } else { [2, 3, 1, 1] };
    for c in 0..4 {
        for r in 0..4 {
            let mut v = 0;
            for k in 0..4 {
                v ^= gf_mul(b[4 * c + (r + k) % 4], m[k]);
            }
            o[4 * c + r] = v;
        }
    }
    u128::from_le_bytes(o)
}

/// AESE: AddRoundKey, ShiftRows, SubBytes
pub fn aese(d: u128, n: u128) -> u128 { aes_round(d ^ n, false) }
/// AESD: AddRoundKey, InvShiftRows, InvSubBytes
pub fn aesd(d: u128, n: u128) -> u128 { aes_round(d ^ n, true) }
pub fn aesmc(n: u128) -> u128 { aes_mix(n, false) }
pub fn aesimc(n: u128) -> u128 { aes_mix(n, true) }

fn w(v: u128, e: u32) -> u32 { (v >> (32 * e)) as u32 }

fn from_words(x: [u32; 4]) -> u128 {
    x.iter().enumerate().map(|(e, &v)| (v as u128) << (32 * e)).sum()
}

/// SHA1C/SHA1P/SHA1M (op 0/1/2): four rounds on abcd = d, e = n<31:0>, wk = m.
pub fn sha1_rounds(op: u32, d: u128, n: u32, m: u128) -> u128 {
    let mut x = d;
    let mut y = n;
    for e in 0..4 {
        let (b, c, dd) = (w(x, 1), w(x, 2), w(x, 3));
        let t = match op {
            0 => ((c ^ dd) & b) ^ dd,
            1 => b ^ c ^ dd,
            _ => (b & c) | ((b | c) & dd),
        };
        y = y.wrapping_add(w(x, 0).rotate_left(5)).wrapping_add(t).wrapping_add(w(m, e));
        x = (x & !(0xffff_ffffu128 << 32)) | (w(x, 1).rotate_left(30) as u128) << 32;
        // <Y, X> = ROL(Y:X, 32)
        let top = w(x, 3);
        x = x << 32 | y as u128;
        y = top;
    }
    x
}

pub fn sha1su0(d: u128, n: u128, m: u128) -> u128 { ((n << 64) | (d >> 64)) ^ d ^ m }

pub fn sha1su1(d: u128, n: u128) -> u128 {
    let t = d ^ (n >> 32);
    let r = [w(t, 0).rotate_left(1), w(t, 1).rotate_left(1), w(t, 2).rotate_left(1), w(t, 3).rotate_left(1) ^ w(t, 0).rotate_left(2)];
    from_words(r)
}

/// SHA256H (part1, x = abcd = d, y = efgh = n) and SHA256H2 (!part1, x = n, y = d).
pub fn sha256_hash(mut x: u128, mut y: u128, wk: u128, part1: bool) -> u128 {
    for e in 0..4 {
        let chs = ((w(y, 1) ^ w(y, 2)) & w(y, 0)) ^ w(y, 2);
        let maj = (w(x, 0) & w(x, 1)) | ((w(x, 0) | w(x, 1)) & w(x, 2));
        let s1 = w(y, 0).rotate_right(6) ^ w(y, 0).rotate_right(11) ^ w(y, 0).rotate_right(25);
        let s0 = w(x, 0).rotate_right(2) ^ w(x, 0).rotate_right(13) ^ w(x, 0).rotate_right(22);
        let t1 = w(y, 3).wrapping_add(s1).wrapping_add(chs).wrapping_add(w(wk, e));
        let x3 = t1.wrapping_add(w(x, 3));
        let y3 = t1.wrapping_add(s0).wrapping_add(maj);
        x = (x & !(0xffff_ffffu128 << 96)) | (x3 as u128) << 96;
        y = (y & !(0xffff_ffffu128 << 96)) | (y3 as u128) << 96;
        // <Y, X> = ROL(Y:X, 32)
        let (nx, ny) = (x << 32 | (y >> 96), y << 32 | (x >> 96));
        x = nx;
        y = ny;
    }
    if part1 { x } else { y }
}

pub fn sha256su0(d: u128, n: u128) -> u128 {
    let t = (n << 96) | (d >> 32);
    let mut r = [0u32; 4];
    for e in 0..4 {
        let elt = w(t, e);
        r[e as usize] = (elt.rotate_right(7) ^ elt.rotate_right(18) ^ (elt >> 3)).wrapping_add(w(d, e));
    }
    from_words(r)
}

pub fn sha256su1(d: u128, n: u128, m: u128) -> u128 {
    let t0 = (m << 96) | (n >> 32);
    let sigma1 = |x: u32| x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10);
    let mut r = [0u32; 4];
    for e in 0..4 {
        let src = if e < 2 { w(m, 2 + e) } else { r[e as usize - 2] };
        r[e as usize] = sigma1(src).wrapping_add(w(d, e)).wrapping_add(w(t0, e));
    }
    from_words(r)
}

impl Cpu {
    /// Returns None if `i` isn't in the crypto encoding space.
    pub(crate) fn exec_crypto(&mut self, i: u32) -> Option<Res<()>> {
        let (rd, rn, rm) = ((i & 31) as usize, bits(i, 9, 5) as usize, bits(i, 20, 16) as usize);
        let (d, n, m) = (self.v[rd], self.v[rn], self.v[rm]);
        let r = if i & 0xff3e_0c00 == 0x4e28_0800 {
            // AES: 0100 1110 0010 1000 0 opcode(5) 10 Rn Rd
            match bits(i, 16, 12) {
                0b00100 => aese(d, n),
                0b00101 => aesd(d, n),
                0b00110 => aesmc(n),
                0b00111 => aesimc(n),
                _ => return Some(undef()),
            }
        } else if i & 0xff20_8c00 == 0x5e00_0000 {
            // SHA three-register: 0101 1110 000 Rm 0 opcode(3) 00 Rn Rd
            match bits(i, 14, 12) {
                op @ 0..=2 => sha1_rounds(op, d, n as u32, m),
                3 => sha1su0(d, n, m),
                4 => sha256_hash(d, n, m, true),
                5 => sha256_hash(n, d, m, false),
                6 => sha256su1(d, n, m),
                _ => return Some(undef()),
            }
        } else if i & 0xff3e_0c00 == 0x5e28_0800 {
            // SHA two-register: 0101 1110 0010 1000 0 opcode(5) 10 Rn Rd
            match bits(i, 16, 12) {
                0 => (n as u32).rotate_left(30) as u128, // SHA1H
                1 => sha1su1(d, n),
                2 => sha256su0(d, n),
                _ => return Some(undef()),
            }
        } else {
            return None;
        };
        self.v[rd] = r;
        Some(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be_words(bytes: &[u8]) -> u128 {
        from_words([0, 1, 2, 3].map(|k| u32::from_be_bytes(bytes[4 * k..4 * k + 4].try_into().unwrap())))
    }

    fn add32(a: u128, b: u128) -> u128 { from_words([0, 1, 2, 3].map(|e| w(a, e).wrapping_add(w(b, e)))) }

    #[test]
    fn sbox() {
        assert_eq!(SBOX[0x00], 0x63);
        assert_eq!(SBOX[0x53], 0xed);
        assert_eq!(INV_SBOX[0x63], 0x00);
    }

    #[test]
    fn aes128_fips197() {
        // FIPS-197 appendix C.1, key expansion done in software
        let key: [u8; 16] = core::array::from_fn(|k| k as u8);
        let pt = u128::from_le_bytes([0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        let ct = u128::from_le_bytes([0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a]);
        let mut rk = [[0u8; 16]; 11];
        rk[0] = key;
        let mut rcon = 1u8;
        for r in 1..11 {
            let p = rk[r - 1];
            let mut t = [SBOX[p[13] as usize] ^ rcon, SBOX[p[14] as usize], SBOX[p[15] as usize], SBOX[p[12] as usize]];
            for k in 0..16 {
                rk[r][k] = p[k] ^ t[k % 4];
                t[k % 4] = rk[r][k];
            }
            rcon = gf_mul(rcon, 2);
        }
        let rk = rk.map(u128::from_le_bytes);

        let mut s = pt;
        for r in 0..9 {
            s = aesmc(aese(s, rk[r]));
        }
        s = aese(s, rk[9]) ^ rk[10];
        assert_eq!(s, ct);

        let mut s = aesd(ct, rk[10]);
        for r in (1..10).rev() {
            s = aesd(aesimc(s), aesimc(rk[r]));
        }
        assert_eq!(s ^ rk[0], pt);
    }

    fn pad_abc() -> [u8; 64] {
        let mut blk = [0u8; 64];
        blk[..3].copy_from_slice(b"abc");
        blk[3] = 0x80;
        blk[63] = 24;
        blk
    }

    #[test]
    fn sha1_abc() {
        // the instruction sequence used by Linux's sha1-ce
        let blk = pad_abc();
        let h = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
        let k = [0x5a827999u32, 0x6ed9eba1, 0x8f1bbcdc, 0xca62c1d6];
        let mut msg = [0, 1, 2, 3].map(|q| be_words(&blk[16 * q..16 * q + 16]));
        let mut abcd = from_words([h[0], h[1], h[2], h[3]]);
        let mut e = h[4];
        for i in 0..20 {
            let wk = add32(msg[i % 4], from_words([k[i / 5]; 4]));
            let e_next = (abcd as u32).rotate_left(30);
            let op = [0, 1, 2, 1][i / 5];
            abcd = sha1_rounds(op, abcd, e, wk);
            e = e_next;
            if i < 16 {
                msg[i % 4] = sha1su1(sha1su0(msg[i % 4], msg[(i + 1) % 4], msg[(i + 2) % 4]), msg[(i + 3) % 4]);
            }
        }
        let out = [w(abcd, 0).wrapping_add(h[0]), w(abcd, 1).wrapping_add(h[1]), w(abcd, 2).wrapping_add(h[2]), w(abcd, 3).wrapping_add(h[3]), e.wrapping_add(h[4])];
        assert_eq!(out, [0xa9993e36, 0x4706816a, 0xba3e2571, 0x7850c26c, 0x9cd0d89d]);
    }

    #[test]
    fn sha256_abc() {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
            0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
            0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
            0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
            0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
            0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
            0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
        ];
        let h0 = from_words([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a]);
        let h1 = from_words([0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
        let blk = pad_abc();
        let mut msg = [0, 1, 2, 3].map(|q| be_words(&blk[16 * q..16 * q + 16]));
        let (mut abcd, mut efgh) = (h0, h1);
        for i in 0..16 {
            let wk = add32(msg[i % 4], from_words([K[4 * i], K[4 * i + 1], K[4 * i + 2], K[4 * i + 3]]));
            let tmp = abcd;
            abcd = sha256_hash(abcd, efgh, wk, true);
            efgh = sha256_hash(tmp, efgh, wk, false);
            if i < 12 {
                msg[i % 4] = sha256su1(sha256su0(msg[i % 4], msg[(i + 1) % 4]), msg[(i + 2) % 4], msg[(i + 3) % 4]);
            }
        }
        let (a, b) = (add32(abcd, h0), add32(efgh, h1));
        let out: Vec<u32> = (0..4).map(|e| w(a, e)).chain((0..4).map(|e| w(b, e))).collect();
        assert_eq!(out, [0xba7816bf, 0x8f01cfea, 0x414140de, 0x5dae2223, 0xb00361a3, 0x96177a9c, 0xb410ff61, 0xf20015ad]);
    }

    #[test]
    fn decode() {
        let mut c = Cpu::new(crate::bus::Bus::new(0x4000_0000, 0x1000));
        c.sr.cpacr_el1 = 3 << 20;
        c.v[0] = 0x1234;
        c.v[1] = 0x5678;
        // aese v0.16b, v1.16b (the instruction from mbedtls)
        c.exec(0x4e28_4820).unwrap();
        assert_eq!(c.v[0], aese(0x1234, 0x5678));
        // aesmc v2.16b, v0.16b
        c.exec(0x4e28_6802).unwrap();
        assert_eq!(c.v[2], aesmc(c.v[0]));
        // sha1h s3, s1
        c.exec(0x5e28_0823).unwrap();
        assert_eq!(c.v[3], 0x5678u32.rotate_left(30) as u128);
        // sha256h q0, q1, v2.4s; sha256h2 q4, q1, v2.4s
        let (d, n, m) = (c.v[0], c.v[1], c.v[2]);
        c.exec(0x5e02_4020).unwrap();
        assert_eq!(c.v[0], sha256_hash(d, n, m, true));
        c.v[4] = 7;
        c.exec(0x5e02_5024).unwrap();
        assert_eq!(c.v[4], sha256_hash(n, 7, m, false));
    }
}
