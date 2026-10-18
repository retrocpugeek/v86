//! The C extension: compressed instructions, expanded to their 32-bit forms.

#[inline(always)]
fn b(i: u32, hi: u32, lo: u32) -> u32 { (i >> lo) & ((1 << (hi - lo + 1)) - 1) }

fn itype(imm: i32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    ((imm as u32 & 0xfff) << 20) | rs1 << 15 | f3 << 12 | rd << 7 | op
}

fn stype(imm: i32, rs2: u32, rs1: u32, f3: u32, op: u32) -> u32 {
    let imm = imm as u32;
    (imm >> 5 & 0x7f) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (imm & 0x1f) << 7 | op
}

fn btype(imm: i32, rs2: u32, rs1: u32, f3: u32) -> u32 {
    let imm = imm as u32;
    (imm >> 12 & 1) << 31
        | (imm >> 5 & 0x3f) << 25
        | rs2 << 20
        | rs1 << 15
        | f3 << 12
        | (imm >> 1 & 0xf) << 8
        | (imm >> 11 & 1) << 7
        | 0x63
}

fn jal(imm: i32, rd: u32) -> u32 {
    let imm = imm as u32;
    (imm >> 20 & 1) << 31 | (imm >> 1 & 0x3ff) << 21 | (imm >> 11 & 1) << 20 | (imm >> 12 & 0xff) << 12 | rd << 7 | 0x6f
}

fn rtype(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
}

/// Sign-extend the low `bits` bits.
fn sx(v: u32, bits: u32) -> i32 { ((v << (32 - bits)) as i32) >> (32 - bits) }

const OP_LOAD: u32 = 0x03;
const OP_LOAD_FP: u32 = 0x07;
const OP_IMM: u32 = 0x13;
const OP_IMM32: u32 = 0x1b;
const OP_STORE: u32 = 0x23;
const OP_STORE_FP: u32 = 0x27;
const OP: u32 = 0x33;
const OP_LUI: u32 = 0x37;
const OP_32: u32 = 0x3b;
const OP_JALR: u32 = 0x67;

/// The 32-bit instruction a compressed one stands for, or None (illegal).
pub fn expand(i: u32) -> Option<u32> {
    let rd = b(i, 11, 7);
    let rs2 = b(i, 6, 2);
    // The 3-bit register fields name x8..x15
    let rdp = b(i, 4, 2) + 8;
    let rs1p = b(i, 9, 7) + 8;
    Some(match (i & 3, b(i, 15, 13)) {
        (0, 0) => {
            // C.ADDI4SPN
            let imm = b(i, 12, 11) << 4 | b(i, 10, 7) << 6 | b(i, 6, 6) << 2 | b(i, 5, 5) << 3;
            if imm == 0 {
                return None;
            }
            itype(imm as i32, 2, 0, rdp, OP_IMM)
        }
        (0, 1) => itype((b(i, 12, 10) << 3 | b(i, 6, 5) << 6) as i32, rs1p, 3, rdp, OP_LOAD_FP), // C.FLD
        (0, 2) => itype((b(i, 12, 10) << 3 | b(i, 6, 6) << 2 | b(i, 5, 5) << 6) as i32, rs1p, 2, rdp, OP_LOAD), // C.LW
        (0, 3) => itype((b(i, 12, 10) << 3 | b(i, 6, 5) << 6) as i32, rs1p, 3, rdp, OP_LOAD), // C.LD
        (0, 5) => stype((b(i, 12, 10) << 3 | b(i, 6, 5) << 6) as i32, rdp, rs1p, 3, OP_STORE_FP), // C.FSD
        (0, 6) => stype((b(i, 12, 10) << 3 | b(i, 6, 6) << 2 | b(i, 5, 5) << 6) as i32, rdp, rs1p, 2, OP_STORE), // C.SW
        (0, 7) => stype((b(i, 12, 10) << 3 | b(i, 6, 5) << 6) as i32, rdp, rs1p, 3, OP_STORE), // C.SD
        (1, 0) => itype(sx(b(i, 12, 12) << 5 | rs2, 6), rd, 0, rd, OP_IMM), // C.ADDI
        (1, 1) => {
            // C.ADDIW
            if rd == 0 {
                return None;
            }
            itype(sx(b(i, 12, 12) << 5 | rs2, 6), rd, 0, rd, OP_IMM32)
        }
        (1, 2) => itype(sx(b(i, 12, 12) << 5 | rs2, 6), 0, 0, rd, OP_IMM), // C.LI
        (1, 3) => {
            if rd == 2 {
                // C.ADDI16SP
                let imm = b(i, 12, 12) << 9 | b(i, 6, 6) << 4 | b(i, 5, 5) << 6 | b(i, 4, 3) << 7 | b(i, 2, 2) << 5;
                if imm == 0 {
                    return None;
                }
                itype(sx(imm, 10), 2, 0, 2, OP_IMM)
            } else {
                // C.LUI
                let imm = sx(b(i, 12, 12) << 17 | rs2 << 12, 18);
                if imm == 0 {
                    return None;
                }
                (imm as u32 & 0xffff_f000) | rd << 7 | OP_LUI
            }
        }
        (1, 4) => {
            let shamt = b(i, 12, 12) << 5 | rs2;
            match b(i, 11, 10) {
                0 => itype(shamt as i32, rs1p, 5, rs1p, OP_IMM),            // C.SRLI
                1 => itype((shamt | 0x400) as i32, rs1p, 5, rs1p, OP_IMM),  // C.SRAI
                2 => itype(sx(shamt, 6), rs1p, 7, rs1p, OP_IMM),            // C.ANDI
                _ => {
                    let rs2p = b(i, 4, 2) + 8;
                    match (b(i, 12, 12), b(i, 6, 5)) {
                        (0, 0) => rtype(0x20, rs2p, rs1p, 0, rs1p, OP), // C.SUB
                        (0, 1) => rtype(0, rs2p, rs1p, 4, rs1p, OP),    // C.XOR
                        (0, 2) => rtype(0, rs2p, rs1p, 6, rs1p, OP),    // C.OR
                        (0, 3) => rtype(0, rs2p, rs1p, 7, rs1p, OP),    // C.AND
                        (1, 0) => rtype(0x20, rs2p, rs1p, 0, rs1p, OP_32), // C.SUBW
                        (1, 1) => rtype(0, rs2p, rs1p, 0, rs1p, OP_32), // C.ADDW
                        _ => return None,
                    }
                }
            }
        }
        (1, 5) => {
            // C.J
            let imm = b(i, 12, 12) << 11
                | b(i, 11, 11) << 4
                | b(i, 10, 9) << 8
                | b(i, 8, 8) << 10
                | b(i, 7, 7) << 6
                | b(i, 6, 6) << 7
                | b(i, 5, 3) << 1
                | b(i, 2, 2) << 5;
            jal(sx(imm, 12), 0)
        }
        (1, 6) | (1, 7) => {
            // C.BEQZ, C.BNEZ
            let imm = b(i, 12, 12) << 8 | b(i, 11, 10) << 3 | b(i, 6, 5) << 6 | b(i, 4, 3) << 1 | b(i, 2, 2) << 5;
            btype(sx(imm, 9), 0, rs1p, b(i, 13, 13))
        }
        (2, 0) => itype((b(i, 12, 12) << 5 | rs2) as i32, rd, 1, rd, OP_IMM), // C.SLLI
        (2, 1) => itype((b(i, 12, 12) << 5 | b(i, 6, 5) << 3 | b(i, 4, 2) << 6) as i32, 2, 3, rd, OP_LOAD_FP), // C.FLDSP
        (2, 2) => {
            // C.LWSP
            if rd == 0 {
                return None;
            }
            itype((b(i, 12, 12) << 5 | b(i, 6, 4) << 2 | b(i, 3, 2) << 6) as i32, 2, 2, rd, OP_LOAD)
        }
        (2, 3) => {
            // C.LDSP
            if rd == 0 {
                return None;
            }
            itype((b(i, 12, 12) << 5 | b(i, 6, 5) << 3 | b(i, 4, 2) << 6) as i32, 2, 3, rd, OP_LOAD)
        }
        (2, 4) => match (b(i, 12, 12), rd, rs2) {
            (0, 0, 0) => return None,
            (0, _, 0) => itype(0, rd, 0, 0, OP_JALR),         // C.JR
            (0, _, _) => rtype(0, rs2, 0, 0, rd, OP),          // C.MV
            (1, 0, 0) => 0x0010_0073,                          // C.EBREAK
            (1, _, 0) => itype(0, rd, 0, 1, OP_JALR),          // C.JALR
            _ => rtype(0, rs2, rd, 0, rd, OP),                 // C.ADD
        },
        (2, 5) => stype((b(i, 12, 10) << 3 | b(i, 9, 7) << 6) as i32, rs2, 2, 3, OP_STORE_FP), // C.FSDSP
        (2, 6) => stype((b(i, 12, 9) << 2 | b(i, 8, 7) << 6) as i32, rs2, 2, 2, OP_STORE), // C.SWSP
        (2, 7) => stype((b(i, 12, 10) << 3 | b(i, 9, 7) << 6) as i32, rs2, 2, 3, OP_STORE), // C.SDSP
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::expand;

    /// Compressed encodings and the same instructions assembled uncompressed
    /// (GNU as, -march=rv64gc with .option rvc / norvc).
    #[test]
    fn matches_assembler() {
        let pairs: [(u32, u32); 44] = [
            (0x1fe0, 0x3fc10413), // addi s0, sp, 1020,
            (0x003c, 0x00810793), // addi a5, sp, 8,
            (0x3fe8, 0x0f87b507), // fld fa0, 248(a5),
            (0x5ef8, 0x07c6a703), // lw a4, 124(a3),
            (0x7c64, 0x0f843483), // ld s1, 248(s0),
            (0xa504, 0x00953427), // fsd fs1, 8(a0),
            (0xc1a8, 0x04a5a023), // sw a0, 64(a1),
            (0xe4fc, 0x0cf4b423), // sd a5, 200(s1),
            (0x1e01, 0xfe0e0e13), // addi t3, t3, -32,
            (0x057d, 0x01f50513), // addi a0, a0, 31,
            (0x35fd, 0xfff5859b), // addiw a1, a1, -1,
            (0x2945, 0x0119091b), // addiw s2, s2, 17,
            (0x5601, 0xfe000613), // li a2, -32,
            (0x4fbd, 0x00f00f93), // li t6, 15,
            (0x7101, 0xe0010113), // addi sp, sp, -512,
            (0x617d, 0x1f010113), // addi sp, sp, 496,
            (0x66fd, 0x0001f6b7), // lui a3, 0x1f,
            (0x7c81, 0xfffe0cb7), // lui s9, 0xfffe0,
            (0x917d, 0x03f55513), // srli a0, a0, 63,
            (0x8485, 0x4014d493), // srai s1, s1, 1,
            (0x9a65, 0xff967613), // andi a2, a2, -7,
            (0x8d0d, 0x40b50533), // sub a0, a0, a1,
            (0x8c25, 0x00944433), // xor s0, s0, s1,
            (0x8f5d, 0x00f76733), // or a4, a4, a5,
            (0x8ef1, 0x00c6f6b3), // and a3, a3, a2,
            (0x9d1d, 0x40f5053b), // subw a0, a0, a5,
            (0x9ca9, 0x00a484bb), // addw s1, s1, a0,
            (0xb009, 0x803ff06f), // j .-2046,
            (0xaffd, 0x7fe0006f), // j .+2046,
            (0xd101, 0xf00500e3), // beqz a0, .-256,
            (0xecfd, 0x0e049f63), // bnez s1, .+254,
            (0x1286, 0x02129293), // slli t0, t0, 33,
            (0x307e, 0x1f813007), // fld ft0, 504(sp),
            (0x50fe, 0x0fc12083), // lw ra, 252(sp),
            (0x7dfe, 0x1f813d83), // ld s11, 504(sp),
            (0x8082, 0x00008067), // jr ra,
            (0x8f82, 0x000f8067), // jr t6,
            (0x8526, 0x00900533), // mv a0, s1 (c.mv is add a0, zero, s1),
            (0x9002, 0x00100073), // ebreak,
            (0x9782, 0x000780e7), // jalr a5,
            (0x9522, 0x00850533), // add a0, a0, s0,
            (0xbfc6, 0x1f113c27), // fsd fa7, 504(sp),
            (0xdfea, 0x0fa12e23), // sw s10, 252(sp),
            (0xfffa, 0x1fe13c23), // sd t5, 504(sp),
        ];
        for (c, n) in pairs {
            assert_eq!(expand(c), Some(n), "{c:04x}");
        }
        // Reserved: all zeros, C.ADDI4SPN with imm 0, C.LWSP x0, C.JR x0
        for c in [0x0000, 0x0004, 0x4002, 0x8002] {
            assert_eq!(expand(c), None, "{c:04x}");
        }
    }
}
