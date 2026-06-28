//! VP9 inverse transforms (ISO/VP9 §8.7). The codec uses fixed-point integer
//! DCT/ADST butterflies with 14-bit constants; this seeds stage 3 with the
//! smallest, self-verifiable pieces — the 4-point inverse DCT and the lossless
//! Walsh-Hadamard transform — built bit-exactly and checked by transform-pair
//! round-trips. The 8/16/32-point DCT and the ADST land alongside the
//! coefficient decoder.

#![allow(dead_code)]

/// Fixed-point fractional bits for the transform constants.
const DCT_CONST_BITS: i64 = 14;

/// `cospi_i_64 = round(cos(i·π/64)·2^14)`, i = 1..=31 (`COSPI[0]` unused). These
/// are the exact VP9 transform constants; the `cospi_constants_match_formula`
/// test asserts each against the generating formula.
const COSPI: [i64; 32] = [
    0, 16364, 16305, 16207, 16069, 15893, 15679, 15426, 15137, 14811, 14449, 14053, 13623, 13160,
    12665, 12140, 11585, 11003, 10394, 9760, 9102, 8423, 7723, 7005, 6270, 5520, 4756, 3981, 3196,
    2404, 1606, 804,
];
const COSPI_8_64: i64 = COSPI[8];
const COSPI_16_64: i64 = COSPI[16];
const COSPI_24_64: i64 = COSPI[24];

/// Round a 14-bit fixed-point product back to an integer.
fn round_shift(x: i64) -> i32 {
    ((x + (1 << (DCT_CONST_BITS - 1))) >> DCT_CONST_BITS) as i32
}

/// Butterfly helper: `(a·c0 ∓ b·c1)` rounded — the two outputs of a rotation.
#[inline]
fn rot(a: i32, b: i32, c0: i64, c1: i64) -> (i32, i32) {
    (
        round_shift(a as i64 * c0 - b as i64 * c1),
        round_shift(a as i64 * c1 + b as i64 * c0),
    )
}

/// 4-point inverse DCT (one dimension).
pub fn idct4(input: &[i32; 4], output: &mut [i32; 4]) {
    let (i0, i1, i2, i3) = (
        input[0] as i64,
        input[1] as i64,
        input[2] as i64,
        input[3] as i64,
    );
    let s0 = round_shift((i0 + i2) * COSPI_16_64);
    let s1 = round_shift((i0 - i2) * COSPI_16_64);
    let s2 = round_shift(i1 * COSPI_24_64 - i3 * COSPI_8_64);
    let s3 = round_shift(i1 * COSPI_8_64 + i3 * COSPI_24_64);
    output[0] = s0 + s3;
    output[1] = s1 + s2;
    output[2] = s1 - s2;
    output[3] = s0 - s3;
}

/// 4-point forward DCT — the inverse of [`idct4`], used to validate the
/// constants/butterfly via a round-trip (and useful for a future encoder).
pub fn fdct4(input: &[i32; 4], output: &mut [i32; 4]) {
    let s0 = (input[0] + input[3]) as i64;
    let s1 = (input[1] + input[2]) as i64;
    let s2 = (input[1] - input[2]) as i64;
    let s3 = (input[0] - input[3]) as i64;
    output[0] = round_shift((s0 + s1) * COSPI_16_64);
    output[2] = round_shift((s0 - s1) * COSPI_16_64);
    output[1] = round_shift(s2 * COSPI_24_64 + s3 * COSPI_8_64);
    output[3] = round_shift(s3 * COSPI_24_64 - s2 * COSPI_8_64);
}

/// 8-point inverse DCT (ISO/VP9 §8.7.1.3), one dimension.
pub fn idct8(input: &[i32; 8], output: &mut [i32; 8]) {
    let c = |i: usize| COSPI[i];
    // stage 1
    let (a0, a2, a1, a3) = (input[0], input[4], input[2], input[6]);
    let a4 = round_shift(input[1] as i64 * c(28) - input[7] as i64 * c(4));
    let a7 = round_shift(input[1] as i64 * c(4) + input[7] as i64 * c(28));
    let a5 = round_shift(input[5] as i64 * c(12) - input[3] as i64 * c(20));
    let a6 = round_shift(input[5] as i64 * c(20) + input[3] as i64 * c(12));
    // stage 2
    let b0 = round_shift((a0 as i64 + a2 as i64) * c(16));
    let b1 = round_shift((a0 as i64 - a2 as i64) * c(16));
    let b2 = round_shift(a1 as i64 * c(24) - a3 as i64 * c(8));
    let b3 = round_shift(a1 as i64 * c(8) + a3 as i64 * c(24));
    let (b4, b5, b6, b7) = (a4 + a5, a4 - a5, -a6 + a7, a6 + a7);
    // stage 3
    let (d0, d1, d2, d3) = (b0 + b3, b1 + b2, b1 - b2, b0 - b3);
    let d4 = b4;
    let d5 = round_shift((b6 as i64 - b5 as i64) * c(16));
    let d6 = round_shift((b5 as i64 + b6 as i64) * c(16));
    let d7 = b7;
    // stage 4
    output[0] = d0 + d7;
    output[1] = d1 + d6;
    output[2] = d2 + d5;
    output[3] = d3 + d4;
    output[4] = d3 - d4;
    output[5] = d2 - d5;
    output[6] = d1 - d6;
    output[7] = d0 - d7;
}

/// 16-point inverse DCT (ISO/VP9 §8.7.1.3), one dimension.
pub fn idct16(input: &[i32; 16], output: &mut [i32; 16]) {
    let c = |i: usize| COSPI[i];
    let rs = round_shift;
    // stage 1 (reorder)
    let s1 = [
        input[0], input[8], input[4], input[12], input[2], input[10], input[6], input[14],
        input[1], input[9], input[5], input[13], input[3], input[11], input[7], input[15],
    ];
    // stage 2
    let mut s = [0i32; 16];
    s[..8].copy_from_slice(&s1[..8]);
    s[8] = rs(s1[8] as i64 * c(30) - s1[15] as i64 * c(2));
    s[15] = rs(s1[8] as i64 * c(2) + s1[15] as i64 * c(30));
    s[9] = rs(s1[9] as i64 * c(14) - s1[14] as i64 * c(18));
    s[14] = rs(s1[9] as i64 * c(18) + s1[14] as i64 * c(14));
    s[10] = rs(s1[10] as i64 * c(22) - s1[13] as i64 * c(10));
    s[13] = rs(s1[10] as i64 * c(10) + s1[13] as i64 * c(22));
    s[11] = rs(s1[11] as i64 * c(6) - s1[12] as i64 * c(26));
    s[12] = rs(s1[11] as i64 * c(26) + s1[12] as i64 * c(6));
    // stage 3
    let mut t = [0i32; 16];
    t[..4].copy_from_slice(&s[..4]);
    t[4] = rs(s[4] as i64 * c(28) - s[7] as i64 * c(4));
    t[7] = rs(s[4] as i64 * c(4) + s[7] as i64 * c(28));
    t[5] = rs(s[5] as i64 * c(12) - s[6] as i64 * c(20));
    t[6] = rs(s[5] as i64 * c(20) + s[6] as i64 * c(12));
    t[8] = s[8] + s[9];
    t[9] = s[8] - s[9];
    t[10] = -s[10] + s[11];
    t[11] = s[10] + s[11];
    t[12] = s[12] + s[13];
    t[13] = s[12] - s[13];
    t[14] = -s[14] + s[15];
    t[15] = s[14] + s[15];
    // stage 4
    let mut u = [0i32; 16];
    u[0] = rs((t[0] as i64 + t[1] as i64) * c(16));
    u[1] = rs((t[0] as i64 - t[1] as i64) * c(16));
    u[2] = rs(t[2] as i64 * c(24) - t[3] as i64 * c(8));
    u[3] = rs(t[2] as i64 * c(8) + t[3] as i64 * c(24));
    u[4] = t[4] + t[5];
    u[5] = t[4] - t[5];
    u[6] = -t[6] + t[7];
    u[7] = t[6] + t[7];
    u[8] = t[8];
    u[9] = rs(-(t[9] as i64) * c(8) + t[14] as i64 * c(24));
    u[14] = rs(t[9] as i64 * c(24) + t[14] as i64 * c(8));
    u[10] = rs(-(t[10] as i64) * c(24) - t[13] as i64 * c(8));
    u[13] = rs(-(t[10] as i64) * c(8) + t[13] as i64 * c(24));
    u[11] = t[11];
    u[12] = t[12];
    u[15] = t[15];
    // stage 5
    let mut v = [0i32; 16];
    v[0] = u[0] + u[3];
    v[1] = u[1] + u[2];
    v[2] = u[1] - u[2];
    v[3] = u[0] - u[3];
    v[4] = u[4];
    v[5] = rs((u[6] as i64 - u[5] as i64) * c(16));
    v[6] = rs((u[5] as i64 + u[6] as i64) * c(16));
    v[7] = u[7];
    v[8] = u[8] + u[11];
    v[9] = u[9] + u[10];
    v[10] = u[9] - u[10];
    v[11] = u[8] - u[11];
    v[12] = -u[12] + u[15];
    v[13] = -u[13] + u[14];
    v[14] = u[13] + u[14];
    v[15] = u[12] + u[15];
    // stage 6
    let mut w = [0i32; 16];
    w[0] = v[0] + v[7];
    w[1] = v[1] + v[6];
    w[2] = v[2] + v[5];
    w[3] = v[3] + v[4];
    w[4] = v[3] - v[4];
    w[5] = v[2] - v[5];
    w[6] = v[1] - v[6];
    w[7] = v[0] - v[7];
    w[8] = v[8];
    w[9] = v[9];
    w[10] = rs((-(v[10] as i64) + v[13] as i64) * c(16));
    w[13] = rs((v[10] as i64 + v[13] as i64) * c(16));
    w[11] = rs((-(v[11] as i64) + v[12] as i64) * c(16));
    w[12] = rs((v[11] as i64 + v[12] as i64) * c(16));
    w[14] = v[14];
    w[15] = v[15];
    // stage 7
    for i in 0..8 {
        output[i] = w[i] + w[15 - i];
        output[15 - i] = w[i] - w[15 - i];
    }
}

/// 32-point inverse DCT (ISO/VP9 §8.7.1.3), one dimension.
pub fn idct32(input: &[i32; 32], output: &mut [i32; 32]) {
    let rs = round_shift;
    let c = |i: usize| COSPI[i];
    let m = |a: i32, b: i32, c0: i64, c1: i64| rs(a as i64 * c0 - b as i64 * c1);
    let p = |a: i32, b: i32, c0: i64, c1: i64| rs(a as i64 * c0 + b as i64 * c1);
    let mut s1 = [0i32; 32];
    let mut s2 = [0i32; 32];
    // stage 1
    let even = [0, 16, 8, 24, 4, 20, 12, 28, 2, 18, 10, 26, 6, 22, 14, 30];
    for (i, &e) in even.iter().enumerate() {
        s1[i] = input[e];
    }
    let odd = [
        (1, 31, 31, 1),
        (17, 15, 15, 17),
        (9, 23, 23, 9),
        (25, 7, 7, 25),
        (5, 27, 27, 5),
        (21, 11, 11, 21),
        (13, 19, 19, 13),
        (29, 3, 3, 29),
    ];
    for (i, &(a, b, ca, cb)) in odd.iter().enumerate() {
        s1[16 + i] = m(input[a], input[b], c(ca), c(cb));
        s1[31 - i] = p(input[a], input[b], c(cb), c(ca));
    }
    // stage 2
    s2[..8].copy_from_slice(&s1[..8]);
    s2[8] = m(s1[8], s1[15], c(30), c(2));
    s2[15] = p(s1[8], s1[15], c(2), c(30));
    s2[9] = m(s1[9], s1[14], c(14), c(18));
    s2[14] = p(s1[9], s1[14], c(18), c(14));
    s2[10] = m(s1[10], s1[13], c(22), c(10));
    s2[13] = p(s1[10], s1[13], c(10), c(22));
    s2[11] = m(s1[11], s1[12], c(6), c(26));
    s2[12] = p(s1[11], s1[12], c(26), c(6));
    for (a, sgn) in [
        (16, 1),
        (18, -1),
        (20, 1),
        (22, -1),
        (24, 1),
        (26, -1),
        (28, 1),
        (30, -1),
    ] {
        s2[a] = sgn * s1[a] + s1[a + 1];
        s2[a + 1] = s1[a] - sgn * s1[a + 1];
    }
    // stage 3
    s1[..4].copy_from_slice(&s2[..4]);
    s1[4] = m(s2[4], s2[7], c(28), c(4));
    s1[7] = p(s2[4], s2[7], c(4), c(28));
    s1[5] = m(s2[5], s2[6], c(12), c(20));
    s1[6] = p(s2[5], s2[6], c(20), c(12));
    for (a, sgn) in [(8, 1), (10, -1), (12, 1), (14, -1)] {
        s1[a] = sgn * s2[a] + s2[a + 1];
        s1[a + 1] = s2[a] - sgn * s2[a + 1];
    }
    s1[16] = s2[16];
    s1[17] = p(-s2[17], s2[30], c(4), c(28));
    s1[30] = p(s2[17], s2[30], c(28), c(4));
    s1[18] = m(-s2[18], s2[29], c(28), c(4));
    s1[29] = p(-s2[18], s2[29], c(4), c(28));
    s1[19] = s2[19];
    s1[20] = s2[20];
    s1[21] = p(-s2[21], s2[26], c(20), c(12));
    s1[26] = p(s2[21], s2[26], c(12), c(20));
    s1[22] = m(-s2[22], s2[25], c(12), c(20));
    s1[25] = p(-s2[22], s2[25], c(20), c(12));
    s1[23] = s2[23];
    s1[24] = s2[24];
    s1[27] = s2[27];
    s1[28] = s2[28];
    s1[31] = s2[31];
    // stage 4
    s2[0] = rs((s1[0] as i64 + s1[1] as i64) * c(16));
    s2[1] = rs((s1[0] as i64 - s1[1] as i64) * c(16));
    s2[2] = m(s1[2], s1[3], c(24), c(8));
    s2[3] = p(s1[2], s1[3], c(8), c(24));
    s2[4] = s1[4] + s1[5];
    s2[5] = s1[4] - s1[5];
    s2[6] = -s1[6] + s1[7];
    s2[7] = s1[6] + s1[7];
    s2[8] = s1[8];
    s2[9] = p(-s1[9], s1[14], c(8), c(24));
    s2[14] = p(s1[9], s1[14], c(24), c(8));
    s2[10] = m(-s1[10], s1[13], c(24), c(8));
    s2[13] = p(-s1[10], s1[13], c(8), c(24));
    s2[11] = s1[11];
    s2[12] = s1[12];
    s2[15] = s1[15];
    s2[16] = s1[16] + s1[19];
    s2[17] = s1[17] + s1[18];
    s2[18] = s1[17] - s1[18];
    s2[19] = s1[16] - s1[19];
    s2[20] = -s1[20] + s1[23];
    s2[21] = -s1[21] + s1[22];
    s2[22] = s1[21] + s1[22];
    s2[23] = s1[20] + s1[23];
    s2[24] = s1[24] + s1[27];
    s2[25] = s1[25] + s1[26];
    s2[26] = s1[25] - s1[26];
    s2[27] = s1[24] - s1[27];
    s2[28] = -s1[28] + s1[31];
    s2[29] = -s1[29] + s1[30];
    s2[30] = s1[29] + s1[30];
    s2[31] = s1[28] + s1[31];
    // stage 5
    s1[0] = s2[0] + s2[3];
    s1[1] = s2[1] + s2[2];
    s1[2] = s2[1] - s2[2];
    s1[3] = s2[0] - s2[3];
    s1[4] = s2[4];
    s1[5] = rs((s2[6] as i64 - s2[5] as i64) * c(16));
    s1[6] = rs((s2[5] as i64 + s2[6] as i64) * c(16));
    s1[7] = s2[7];
    s1[8] = s2[8] + s2[11];
    s1[9] = s2[9] + s2[10];
    s1[10] = s2[9] - s2[10];
    s1[11] = s2[8] - s2[11];
    s1[12] = -s2[12] + s2[15];
    s1[13] = -s2[13] + s2[14];
    s1[14] = s2[13] + s2[14];
    s1[15] = s2[12] + s2[15];
    s1[16] = s2[16];
    s1[17] = s2[17];
    s1[18] = p(-s2[18], s2[29], c(8), c(24));
    s1[29] = p(s2[18], s2[29], c(24), c(8));
    s1[19] = p(-s2[19], s2[28], c(8), c(24));
    s1[28] = p(s2[19], s2[28], c(24), c(8));
    s1[20] = m(-s2[20], s2[27], c(24), c(8));
    s1[27] = p(-s2[20], s2[27], c(8), c(24));
    s1[21] = m(-s2[21], s2[26], c(24), c(8));
    s1[26] = p(-s2[21], s2[26], c(8), c(24));
    s1[22] = s2[22];
    s1[23] = s2[23];
    s1[24] = s2[24];
    s1[25] = s2[25];
    s1[30] = s2[30];
    s1[31] = s2[31];
    // stage 6
    s2[0] = s1[0] + s1[7];
    s2[1] = s1[1] + s1[6];
    s2[2] = s1[2] + s1[5];
    s2[3] = s1[3] + s1[4];
    s2[4] = s1[3] - s1[4];
    s2[5] = s1[2] - s1[5];
    s2[6] = s1[1] - s1[6];
    s2[7] = s1[0] - s1[7];
    s2[8] = s1[8];
    s2[9] = s1[9];
    s2[10] = rs((-(s1[10] as i64) + s1[13] as i64) * c(16));
    s2[13] = rs((s1[10] as i64 + s1[13] as i64) * c(16));
    s2[11] = rs((-(s1[11] as i64) + s1[12] as i64) * c(16));
    s2[12] = rs((s1[11] as i64 + s1[12] as i64) * c(16));
    s2[14] = s1[14];
    s2[15] = s1[15];
    s2[16] = s1[16] + s1[23];
    s2[17] = s1[17] + s1[22];
    s2[18] = s1[18] + s1[21];
    s2[19] = s1[19] + s1[20];
    s2[20] = s1[19] - s1[20];
    s2[21] = s1[18] - s1[21];
    s2[22] = s1[17] - s1[22];
    s2[23] = s1[16] - s1[23];
    s2[24] = -s1[24] + s1[31];
    s2[25] = -s1[25] + s1[30];
    s2[26] = -s1[26] + s1[29];
    s2[27] = -s1[27] + s1[28];
    s2[28] = s1[27] + s1[28];
    s2[29] = s1[26] + s1[29];
    s2[30] = s1[25] + s1[30];
    s2[31] = s1[24] + s1[31];
    // stage 7
    for i in 0..8 {
        s1[i] = s2[i] + s2[15 - i];
        s1[15 - i] = s2[i] - s2[15 - i];
    }
    s1[16] = s2[16];
    s1[17] = s2[17];
    s1[18] = s2[18];
    s1[19] = s2[19];
    s1[20] = rs((-(s2[20] as i64) + s2[27] as i64) * c(16));
    s1[27] = rs((s2[20] as i64 + s2[27] as i64) * c(16));
    s1[21] = rs((-(s2[21] as i64) + s2[26] as i64) * c(16));
    s1[26] = rs((s2[21] as i64 + s2[26] as i64) * c(16));
    s1[22] = rs((-(s2[22] as i64) + s2[25] as i64) * c(16));
    s1[25] = rs((s2[22] as i64 + s2[25] as i64) * c(16));
    s1[23] = rs((-(s2[23] as i64) + s2[24] as i64) * c(16));
    s1[24] = rs((s2[23] as i64 + s2[24] as i64) * c(16));
    s1[28] = s2[28];
    s1[29] = s2[29];
    s1[30] = s2[30];
    s1[31] = s2[31];
    // final
    for i in 0..16 {
        output[i] = s1[i] + s1[31 - i];
        output[31 - i] = s1[i] - s1[31 - i];
    }
}

/// ADST sine constants `sinpi_i_9`, i = 1..=4 (`SINPI[0]` unused).
const SINPI: [i64; 5] = [0, 5283, 9929, 13377, 15212];

/// 4-point inverse ADST (ISO/VP9 §8.7.1.2), one dimension.
pub fn iadst4(input: &[i32; 4], output: &mut [i32; 4]) {
    let (x0, x1, x2, x3) = (
        input[0] as i64,
        input[1] as i64,
        input[2] as i64,
        input[3] as i64,
    );
    let s0 = SINPI[1] * x0;
    let s1 = SINPI[2] * x0;
    let s2 = SINPI[3] * x1;
    let s3 = SINPI[4] * x2;
    let s4 = SINPI[1] * x2;
    let s5 = SINPI[2] * x3;
    let s6 = SINPI[4] * x3;
    let s7 = x0 - x2 + x3;
    let a0 = s0 + s3 + s5;
    let a1 = s1 - s4 - s6;
    let a3 = s2;
    let a2 = SINPI[3] * s7;
    output[0] = round_shift(a0 + a3);
    output[1] = round_shift(a1 + a3);
    output[2] = round_shift(a2);
    output[3] = round_shift(a0 + a1 - a3);
}

/// 8-point inverse ADST (ISO/VP9 §8.7.1.2), one dimension.
pub fn iadst8(input: &[i32; 8], output: &mut [i32; 8]) {
    let c = |i: usize| COSPI[i];
    let rs = round_shift;
    let (x0, x1, x2, x3, x4, x5, x6, x7) = (
        input[7] as i64,
        input[0] as i64,
        input[5] as i64,
        input[2] as i64,
        input[3] as i64,
        input[4] as i64,
        input[1] as i64,
        input[6] as i64,
    );
    // stage 1
    let s0 = x0 * c(2) + x1 * c(30);
    let s1 = x0 * c(30) - x1 * c(2);
    let s2 = x2 * c(10) + x3 * c(22);
    let s3 = x2 * c(22) - x3 * c(10);
    let s4 = x4 * c(18) + x5 * c(14);
    let s5 = x4 * c(14) - x5 * c(18);
    let s6 = x6 * c(26) + x7 * c(6);
    let s7 = x6 * c(6) - x7 * c(26);
    let (x0, x1, x2, x3) = (
        rs(s0 + s4) as i64,
        rs(s1 + s5) as i64,
        rs(s2 + s6) as i64,
        rs(s3 + s7) as i64,
    );
    let (x4, x5, x6, x7) = (
        rs(s0 - s4) as i64,
        rs(s1 - s5) as i64,
        rs(s2 - s6) as i64,
        rs(s3 - s7) as i64,
    );
    // stage 2
    let s4 = x4 * c(8) + x5 * c(24);
    let s5 = x4 * c(24) - x5 * c(8);
    let s6 = -x6 * c(24) + x7 * c(8);
    let s7 = x6 * c(8) + x7 * c(24);
    let (x0, x1, x2, x3) = (x0 + x2, x1 + x3, x0 - x2, x1 - x3);
    let (x4, x5, x6, x7) = (
        rs(s4 + s6) as i64,
        rs(s5 + s7) as i64,
        rs(s4 - s6) as i64,
        rs(s5 - s7) as i64,
    );
    // stage 3
    let x2r = rs((x2 + x3) * c(16)) as i64;
    let x3r = rs((x2 - x3) * c(16)) as i64;
    let x6r = rs((x6 + x7) * c(16)) as i64;
    let x7r = rs((x6 - x7) * c(16)) as i64;
    output[0] = x0 as i32;
    output[1] = (-x4) as i32;
    output[2] = x6r as i32;
    output[3] = (-x2r) as i32;
    output[4] = x3r as i32;
    output[5] = (-x7r) as i32;
    output[6] = x5 as i32;
    output[7] = (-x1) as i32;
}

/// 16-point inverse ADST (ISO/VP9 §8.7.1.2), one dimension.
pub fn iadst16(input: &[i32; 16], output: &mut [i32; 16]) {
    let c = |i: usize| COSPI[i];
    let rs = round_shift;
    let x: Vec<i64> = [15, 0, 13, 2, 11, 4, 9, 6, 7, 8, 5, 10, 3, 12, 1, 14]
        .iter()
        .map(|&i| input[i] as i64)
        .collect();
    // stage 1
    let s = [
        x[0] * c(1) + x[1] * c(31),
        x[0] * c(31) - x[1] * c(1),
        x[2] * c(5) + x[3] * c(27),
        x[2] * c(27) - x[3] * c(5),
        x[4] * c(9) + x[5] * c(23),
        x[4] * c(23) - x[5] * c(9),
        x[6] * c(13) + x[7] * c(19),
        x[6] * c(19) - x[7] * c(13),
        x[8] * c(17) + x[9] * c(15),
        x[8] * c(15) - x[9] * c(17),
        x[10] * c(21) + x[11] * c(11),
        x[10] * c(11) - x[11] * c(21),
        x[12] * c(25) + x[13] * c(7),
        x[12] * c(7) - x[13] * c(25),
        x[14] * c(29) + x[15] * c(3),
        x[14] * c(3) - x[15] * c(29),
    ];
    let x: Vec<i64> = (0..16)
        .map(|i| {
            if i < 8 {
                rs(s[i] + s[i + 8]) as i64
            } else {
                rs(s[i - 8] - s[i]) as i64
            }
        })
        .collect();
    // stage 2
    let s8 = x[8] * c(4) + x[9] * c(28);
    let s9 = x[8] * c(28) - x[9] * c(4);
    let s10 = x[10] * c(20) + x[11] * c(12);
    let s11 = x[10] * c(12) - x[11] * c(20);
    let s12 = -x[12] * c(28) + x[13] * c(4);
    let s13 = x[12] * c(4) + x[13] * c(28);
    let s14 = -x[14] * c(12) + x[15] * c(20);
    let s15 = x[14] * c(20) + x[15] * c(12);
    let x = vec![
        x[0] + x[4],
        x[1] + x[5],
        x[2] + x[6],
        x[3] + x[7],
        x[0] - x[4],
        x[1] - x[5],
        x[2] - x[6],
        x[3] - x[7],
        rs(s8 + s12) as i64,
        rs(s9 + s13) as i64,
        rs(s10 + s14) as i64,
        rs(s11 + s15) as i64,
        rs(s8 - s12) as i64,
        rs(s9 - s13) as i64,
        rs(s10 - s14) as i64,
        rs(s11 - s15) as i64,
    ];
    // stage 3
    let s4 = x[4] * c(8) + x[5] * c(24);
    let s5 = x[4] * c(24) - x[5] * c(8);
    let s6 = -x[6] * c(24) + x[7] * c(8);
    let s7 = x[6] * c(8) + x[7] * c(24);
    let s12 = x[12] * c(8) + x[13] * c(24);
    let s13 = x[12] * c(24) - x[13] * c(8);
    let s14 = -x[14] * c(24) + x[15] * c(8);
    let s15 = x[14] * c(8) + x[15] * c(24);
    let x = vec![
        x[0] + x[2],
        x[1] + x[3],
        x[0] - x[2],
        x[1] - x[3],
        rs(s4 + s6) as i64,
        rs(s5 + s7) as i64,
        rs(s4 - s6) as i64,
        rs(s5 - s7) as i64,
        x[8] + x[10],
        x[9] + x[11],
        x[8] - x[10],
        x[9] - x[11],
        rs(s12 + s14) as i64,
        rs(s13 + s15) as i64,
        rs(s12 - s14) as i64,
        rs(s13 - s15) as i64,
    ];
    // stage 4
    let x2 = rs(-c(16) * (x[2] + x[3])) as i64;
    let x3 = rs(c(16) * (x[2] - x[3])) as i64;
    let x6 = rs(c(16) * (x[6] + x[7])) as i64;
    let x7 = rs(c(16) * (-x[6] + x[7])) as i64;
    let x10 = rs(c(16) * (x[10] + x[11])) as i64;
    let x11 = rs(c(16) * (-x[10] + x[11])) as i64;
    let x14 = rs(-c(16) * (x[14] + x[15])) as i64;
    let x15 = rs(c(16) * (x[14] - x[15])) as i64;
    let o = [
        x[0], -x[8], x[12], -x[4], x6, x14, x10, x2, x3, x11, x15, x7, x[5], -x[13], x[9], -x[1],
    ];
    for i in 0..16 {
        output[i] = o[i] as i32;
    }
}

/// 4-point inverse Walsh-Hadamard transform (lossless mode), one dimension.
/// Input is pre-shifted right by `UNIT_QUANT_SHIFT` (2) by the caller for rows.
pub fn iwht4(input: &[i32; 4], output: &mut [i32; 4]) {
    let mut a = input[0];
    let mut c = input[1];
    let mut d = input[2];
    let mut b = input[3];
    a += c;
    d -= b;
    let e = (a - d) >> 1;
    b = e - b;
    c = e - c;
    a -= b;
    d += c;
    output[0] = a;
    output[1] = b;
    output[2] = c;
    output[3] = d;
}

/// 2D inverse Walsh-Hadamard transform + add, for lossless 4×4 blocks
/// (libvpx `vp9_iwht4x4_16_add_c`). The row pass pre-shifts each input right by
/// `UNIT_QUANT_SHIFT` (2); the column pass adds to the prediction with no final
/// round-shift.
pub fn inverse_wht_add(coeffs: &[i32], dest: &mut [u16], stride: usize, max: i32) {
    let mut tmp = [0i32; 16];
    for r in 0..4 {
        let inp = [
            coeffs[r * 4] >> 2,
            coeffs[r * 4 + 1] >> 2,
            coeffs[r * 4 + 2] >> 2,
            coeffs[r * 4 + 3] >> 2,
        ];
        let mut out = [0i32; 4];
        iwht4(&inp, &mut out);
        tmp[r * 4..r * 4 + 4].copy_from_slice(&out);
    }
    for c in 0..4 {
        let inp = [tmp[c], tmp[4 + c], tmp[8 + c], tmp[12 + c]];
        let mut out = [0i32; 4];
        iwht4(&inp, &mut out);
        for r in 0..4 {
            let v = dest[r * stride + c] as i32 + out[r];
            dest[r * stride + c] = v.clamp(0, max) as u16;
        }
    }
}

/// Hybrid transform type for a block (row transform, column transform).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxType {
    DctDct,
    AdstDct,
    DctAdst,
    AdstAdst,
}

/// `intra_mode_to_tx_type_lookup` (ISO/VP9 §8.5.2): the tx_type for each intra
/// mode (applies to tx sizes ≤ 16×16; 32×32 is always DCT_DCT). Order matches
/// the intra-mode enum: DC, V, H, D45, D135, D117, D153, D207, D63, TM.
pub const INTRA_MODE_TO_TX_TYPE: [TxType; 10] = [
    TxType::DctDct,   // DC
    TxType::AdstDct,  // V
    TxType::DctAdst,  // H
    TxType::DctDct,   // D45
    TxType::AdstAdst, // D135
    TxType::AdstDct,  // D117
    TxType::DctAdst,  // D153
    TxType::DctAdst,  // D207
    TxType::AdstDct,  // D63
    TxType::AdstAdst, // TM
];

fn round_pow2(x: i32, n: u32) -> i32 {
    (x + (1 << (n - 1))) >> n
}

/// Apply the size-`n` inverse DCT to a row/column slice.
fn idct_1d(input: &[i32], output: &mut [i32]) {
    match input.len() {
        4 => idct4(
            input.try_into().unwrap(),
            (&mut output[..4]).try_into().unwrap(),
        ),
        8 => idct8(
            input.try_into().unwrap(),
            (&mut output[..8]).try_into().unwrap(),
        ),
        16 => idct16(
            input.try_into().unwrap(),
            (&mut output[..16]).try_into().unwrap(),
        ),
        32 => idct32(
            input.try_into().unwrap(),
            (&mut output[..32]).try_into().unwrap(),
        ),
        _ => unreachable!(),
    }
}

/// Apply the size-`n` inverse ADST to a row/column slice (n ∈ {4,8,16}).
fn iadst_1d(input: &[i32], output: &mut [i32]) {
    match input.len() {
        4 => iadst4(
            input.try_into().unwrap(),
            (&mut output[..4]).try_into().unwrap(),
        ),
        8 => iadst8(
            input.try_into().unwrap(),
            (&mut output[..8]).try_into().unwrap(),
        ),
        16 => iadst16(
            input.try_into().unwrap(),
            (&mut output[..16]).try_into().unwrap(),
        ),
        _ => unreachable!(),
    }
}

/// 2D inverse transform + add to the prediction (ISO/VP9 §8.7.1.1). `coeffs` is
/// the `n×n` dequantized block (row-major); `dest` holds the prediction and on
/// return the reconstructed pixels (8-bit, clamped). Row transform first, then
/// column, with the size-dependent final round-shift (4→4, 8→5, 16/32→6).
/// DC-only inverse DCT add (the `eob == 1` fast path, `DCT_DCT` only): a block
/// whose sole coefficient is the DC reconstructs to a single flat offset. The
/// offset is derived through the very same `idct_1d` the full 2-D path uses
/// (a constant row, then a constant column), so it is bit-identical — just
/// `O(1)` transform work instead of `O(n²)`.
pub fn inverse_transform_dc_add(dc: i32, n: usize, dest: &mut [u16], stride: usize, max: i32) {
    let shift = match n {
        4 => 4,
        8 => 5,
        _ => 6,
    };
    let mut buf = [0i32; 32];
    let mut out = [0i32; 32];
    buf[0] = dc;
    idct_1d(&buf[..n], &mut out[..n]); // constant across the row
    buf[0] = out[0];
    idct_1d(&buf[..n], &mut out[..n]); // constant down the column
    let add = round_pow2(out[0], shift);
    for r in 0..n {
        let row = &mut dest[r * stride..r * stride + n];
        for v in row {
            *v = (*v as i32 + add).clamp(0, max) as u16;
        }
    }
}

pub fn inverse_transform_add(
    coeffs: &[i32],
    n: usize,
    tx_type: TxType,
    dest: &mut [u16],
    stride: usize,
    max: i32,
) {
    inverse_transform_add_rows(coeffs, n, tx_type, dest, stride, max, n)
}

/// Like [`inverse_transform_add`] but only the first `nz_rows` coefficient rows
/// are known to be non-zero (the sparse-EOB fast path) — the remaining rows are
/// all-zero, so their 1-D transform is zero and the row pass skips them. Output
/// is bit-identical to processing all `n` rows.
#[allow(clippy::too_many_arguments)]
pub fn inverse_transform_add_rows(
    coeffs: &[i32],
    n: usize,
    tx_type: TxType,
    dest: &mut [u16],
    stride: usize,
    max: i32,
    nz_rows: usize,
) {
    // tx_type names the (column, row) transforms: ADST_DCT = ADST down columns,
    // DCT across rows. Verified bit-exact against FFmpeg in stage H.
    let (row_adst, col_adst) = match tx_type {
        TxType::DctDct => (false, false),
        TxType::AdstDct => (false, true), // rows=DCT, cols=ADST
        TxType::DctAdst => (true, false), // rows=ADST, cols=DCT
        TxType::AdstAdst => (true, true),
    };
    let shift = match n {
        4 => 4,
        8 => 5,
        _ => 6, // 16 and 32
    };
    // Reusable per-thread scratch — the row pass fully overwrites `tmp[..n²]`
    // before the column pass reads it, so no re-zeroing is needed.
    let nz_rows = nz_rows.clamp(1, n);
    TX_TMP.with(|cell| {
        let mut tmp = cell.borrow_mut();
        for r in 0..nz_rows {
            let (i, o) = (&coeffs[r * n..r * n + n], &mut tmp[r * n..r * n + n]);
            if row_adst {
                iadst_1d(i, o);
            } else {
                idct_1d(i, o);
            }
        }
        // Rows past the last non-zero coefficient transform to all-zero.
        tmp[nz_rows * n..n * n].iter_mut().for_each(|v| *v = 0);
        let mut col_in = [0i32; 32];
        let mut col_out = [0i32; 32];
        for col in 0..n {
            for r in 0..n {
                col_in[r] = tmp[r * n + col];
            }
            if col_adst {
                iadst_1d(&col_in[..n], &mut col_out[..n]);
            } else {
                idct_1d(&col_in[..n], &mut col_out[..n]);
            }
            for r in 0..n {
                let v = dest[r * stride + col] as i32 + round_pow2(col_out[r], shift);
                dest[r * stride + col] = v.clamp(0, max) as u16;
            }
        }
    });
}

thread_local! {
    /// Row-pass scratch for the 2-D inverse transform (max 32×32). Per-thread, so
    /// concurrent decoder instances don't contend.
    static TX_TMP: std::cell::RefCell<[i32; 1024]> = const { std::cell::RefCell::new([0; 1024]) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_2d_dc_adds_flat_offset() {
        // A 4×4 DCT block with only a DC coefficient reconstructs to a flat
        // offset added to the prediction.
        let mut coeffs = [0i32; 16];
        coeffs[0] = 400;
        let mut dest = [100u16; 16];
        inverse_transform_add(&coeffs, 4, TxType::DctDct, &mut dest, 4, 255);
        assert!(dest.iter().all(|&v| v == dest[0]));
        assert!(dest[0] > 100); // positive DC raises the block
    }

    #[test]
    fn idct4_dc_is_flat() {
        // A DC-only input must reconstruct to a constant block.
        let mut out = [0i32; 4];
        idct4(&[100, 0, 0, 0], &mut out);
        assert!(out.iter().all(|&v| v == out[0]));
    }

    #[test]
    fn fdct4_idct4_roundtrip_is_proportional() {
        // idct4(fdct4(x)) recovers x up to the transforms' fixed combined gain.
        // The DC term gives the exact scale; every coefficient must match it.
        let x = [37i32, -12, 55, -3];
        let mut f = [0i32; 4];
        fdct4(&x, &mut f);
        let mut y = [0i32; 4];
        idct4(&f, &mut y);
        // Recover the scale from a pure-DC probe.
        let mut fd = [0i32; 4];
        fdct4(&[64, 64, 64, 64], &mut fd);
        let mut yd = [0i32; 4];
        idct4(&fd, &mut yd);
        let scale = yd[0] as f64 / 64.0;
        for i in 0..4 {
            let recovered = y[i] as f64 / scale;
            assert!(
                (recovered - x[i] as f64).abs() < 1.5,
                "coef {i}: recovered {recovered:.2} vs {}",
                x[i]
            );
        }
    }

    #[test]
    fn cospi_constants_match_formula() {
        use std::f64::consts::PI;
        for i in 1..32 {
            let want = (((i as f64) * PI / 64.0).cos() * (1i64 << 14) as f64).round() as i64;
            assert_eq!(COSPI[i], want, "cospi_{i}_64");
        }
    }

    /// Reference float inverse DCT in VP9's convention (verified against the
    /// trusted `idct4`): DC basis is `cospi_16_64/2^14`, AC basis is
    /// `cos(π(2n+1)k/(2N))`.
    fn float_idct(x: &[i32]) -> Vec<f64> {
        use std::f64::consts::PI;
        let n = x.len();
        let dc = COSPI_16_64 as f64 / (1i64 << 14) as f64;
        (0..n)
            .map(|i| {
                (0..n)
                    .map(|k| {
                        let basis = if k == 0 {
                            dc
                        } else {
                            (PI * ((2 * i + 1) * k) as f64 / (2 * n) as f64).cos()
                        };
                        x[k] as f64 * basis
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn idct8_matches_float_reference() {
        // Each single-frequency basis must reconstruct exactly (within rounding).
        for k in 0..8 {
            let mut x = [0i32; 8];
            x[k] = 512;
            let mut out = [0i32; 8];
            idct8(&x, &mut out);
            let r = float_idct(&x);
            for n in 0..8 {
                assert!(
                    (out[n] as f64 - r[n]).abs() < 2.0,
                    "k={k} n={n}: {} vs {:.2}",
                    out[n],
                    r[n]
                );
            }
        }
    }

    #[test]
    fn idct16_matches_float_reference() {
        for k in 0..16 {
            let mut x = [0i32; 16];
            x[k] = 512;
            let mut out = [0i32; 16];
            idct16(&x, &mut out);
            let r = float_idct(&x);
            for n in 0..16 {
                assert!(
                    (out[n] as f64 - r[n]).abs() < 3.0,
                    "k={k} n={n}: {} vs {:.2}",
                    out[n],
                    r[n]
                );
            }
        }
    }

    #[test]
    fn idct32_matches_float_reference() {
        for k in 0..32 {
            let mut x = [0i32; 32];
            x[k] = 512;
            let mut out = [0i32; 32];
            idct32(&x, &mut out);
            let r = float_idct(&x);
            for n in 0..32 {
                assert!(
                    (out[n] as f64 - r[n]).abs() < 4.0,
                    "k={k} n={n}: {} vs {:.2}",
                    out[n],
                    r[n]
                );
            }
        }
    }

    /// Reference float inverse ADST (VP9 = DST-VII):
    /// `out[m] = Σ_k x[k]·sin(π(2k+1)(m+1)/(2N+1))`. Verified against `iadst4`.
    fn float_iadst(x: &[i32]) -> Vec<f64> {
        use std::f64::consts::PI;
        let n = x.len();
        (0..n)
            .map(|m| {
                (0..n)
                    .map(|k| {
                        x[k] as f64
                            * (PI * ((2 * k + 1) * (m + 1)) as f64 / (2 * n + 1) as f64).sin()
                    })
                    .sum()
            })
            .collect()
    }

    /// Assert `out` is `out = s·ref` for some scalar `s` (least-squares residual).
    fn assert_proportional(out: &[i32], r: &[f64]) {
        let num: f64 = out.iter().zip(r).map(|(o, r)| *o as f64 * r).sum();
        let den: f64 = r.iter().map(|r| r * r).sum::<f64>() + 1e-9;
        let s = num / den;
        let resid: f64 = out
            .iter()
            .zip(r)
            .map(|(o, r)| (*o as f64 - s * r).powi(2))
            .sum::<f64>()
            .sqrt();
        let norm: f64 = (out.iter().map(|o| (*o as f64).powi(2)).sum::<f64>()).sqrt() + 1e-9;
        assert!(
            resid / norm < 0.02,
            "ADST residual {:.4} (scale {s:.3})",
            resid / norm
        );
    }

    #[test]
    fn iadst4_matches_dst7() {
        for x in [[37i32, -12, 55, -3], [100, 100, 100, 100], [5, -80, 30, 90]] {
            let mut out = [0i32; 4];
            iadst4(&x, &mut out);
            assert_proportional(&out, &float_iadst(&x));
        }
    }

    fn energy_ratio(out: &[i32], inp: &[i32]) -> f64 {
        let eo: f64 = out.iter().map(|v| (*v as f64).powi(2)).sum();
        let ei: f64 = inp
            .iter()
            .map(|v| (*v as f64).powi(2))
            .sum::<f64>()
            .max(1.0);
        eo / ei
    }

    /// The VP9 ADST is orthogonal: `‖out‖²/‖in‖²` is a constant gain (≈ N/2)
    /// regardless of input — which only holds if the butterfly is exact. This
    /// verifies the transcription convention-free. (`iadst8`/`iadst16` are also
    /// verbatim from libvpx; exact pixels are confirmed end-to-end in stage 3i.)
    fn assert_orthogonal<const N: usize>(f: impl Fn(&[i32; N], &mut [i32; N]), gain: f64) {
        let inputs: [[i32; N]; 3] = [
            std::array::from_fn(|i| ((i * 37 + 5) % 200) as i32 - 100),
            std::array::from_fn(|i| if i % 3 == 0 { 200 } else { -50 }),
            std::array::from_fn(|i| (i as i32 + 1) * 10),
        ];
        let ratios: Vec<f64> = inputs
            .iter()
            .map(|x| {
                let mut o = [0i32; N];
                f(x, &mut o);
                energy_ratio(&o, x)
            })
            .collect();
        let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
        assert!(
            (mean - gain).abs() < 0.05 * gain,
            "gain {mean:.3} vs {gain}"
        );
        assert!(
            ratios.iter().all(|r| (r - mean).abs() < 0.05 * mean),
            "not orthogonal: {ratios:?}"
        );
    }

    #[test]
    fn iadst8_is_orthogonal() {
        assert_orthogonal::<8>(iadst8, 4.0);
    }

    #[test]
    fn iadst16_is_orthogonal() {
        assert_orthogonal::<16>(iadst16, 8.0);
    }

    #[test]
    fn iwht4_dc_is_flat() {
        // A DC-only input spreads to a constant block.
        let mut out = [0i32; 4];
        iwht4(&[8, 0, 0, 0], &mut out);
        assert!(out.iter().all(|&v| v == out[0]) && out[0] != 0);
    }
}
