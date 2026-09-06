//! Spec constant tables, built at compile time and validated by tests:
//! scan orders (§6.5.3–6.5.5), the 32×32 DCT matrix (§8.6.4.2), the 4×4 DST,
//! intra angles (Tables 8-4/8-5), the chroma QP map (Table 8-10), the
//! dequantisation scale (§8.6.3) and the deblocking β/tC tables (Table 8-12).

/// Up-right diagonal scan (§6.5.3) for a `SIZE`×`SIZE` block, as (x, y).
const fn diag_scan<const SIZE: usize, const N: usize>() -> [(u8, u8); N] {
    let mut out = [(0u8, 0u8); N];
    let mut i = 0;
    let mut x: i32 = 0;
    let mut y: i32 = 0;
    let mut stop = false;
    while !stop {
        while y >= 0 {
            if x < SIZE as i32 && y < SIZE as i32 {
                out[i] = (x as u8, y as u8);
                i += 1;
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
        if i >= N {
            stop = true;
        }
    }
    out
}

const fn horiz_scan<const SIZE: usize, const N: usize>() -> [(u8, u8); N] {
    let mut out = [(0u8, 0u8); N];
    let mut i = 0;
    while i < N {
        out[i] = ((i % SIZE) as u8, (i / SIZE) as u8);
        i += 1;
    }
    out
}

const fn vert_scan<const SIZE: usize, const N: usize>() -> [(u8, u8); N] {
    let mut out = [(0u8, 0u8); N];
    let mut i = 0;
    while i < N {
        out[i] = ((i / SIZE) as u8, (i % SIZE) as u8);
        i += 1;
    }
    out
}

pub static DIAG_2X2: [(u8, u8); 4] = diag_scan::<2, 4>();
pub static DIAG_4X4: [(u8, u8); 16] = diag_scan::<4, 16>();
pub static DIAG_8X8: [(u8, u8); 64] = diag_scan::<8, 64>();
pub static HORIZ_2X2: [(u8, u8); 4] = horiz_scan::<2, 4>();
pub static HORIZ_4X4: [(u8, u8); 16] = horiz_scan::<4, 16>();
pub static HORIZ_8X8: [(u8, u8); 64] = horiz_scan::<8, 64>();
pub static VERT_2X2: [(u8, u8); 4] = vert_scan::<2, 4>();
pub static VERT_4X4: [(u8, u8); 16] = vert_scan::<4, 16>();
pub static VERT_8X8: [(u8, u8); 64] = vert_scan::<8, 64>();
static ONE: [(u8, u8); 1] = [(0, 0)];

/// `ScanOrder[log2BlockSize][scanIdx]` for block sizes 1, 2, 4, 8
/// (scanIdx 0 = diagonal, 1 = horizontal, 2 = vertical).
pub fn scan_order(log2_block_size: usize, scan_idx: usize) -> &'static [(u8, u8)] {
    match (log2_block_size, scan_idx) {
        (0, _) => &ONE,
        (1, 0) => &DIAG_2X2,
        (1, 1) => &HORIZ_2X2,
        (1, _) => &VERT_2X2,
        (2, 0) => &DIAG_4X4,
        (2, 1) => &HORIZ_4X4,
        (2, _) => &VERT_4X4,
        (_, 0) => &DIAG_8X8,
        (_, 1) => &HORIZ_8X8,
        (_, _) => &VERT_8X8,
    }
}

/// `transMatrix` column 0 = the unique DCT coefficient magnitudes at angles
/// m·π/64 for m = 0..=32 (§8.6.4.2). Every other entry follows from the
/// cosine symmetries, which is how [`DCT32`] is generated.
const DCT_BASE: [i16; 33] = [
    64, 90, 90, 90, 89, 88, 87, 85, 83, 82, 80, 78, 75, 73, 70, 67, 64, 61, 57, 54, 50, 46, 43, 38, 36, 31, 25, 22,
    18, 13, 9, 4, 0,
];

const fn build_dct32() -> [[i16; 32]; 32] {
    let mut m = [[0i16; 32]; 32];
    let mut k = 0;
    while k < 32 {
        let mut n = 0;
        while n < 32 {
            let a = ((2 * n + 1) * k) % 128;
            m[k][n] = if k == 0 {
                64
            } else if a <= 32 {
                DCT_BASE[a]
            } else if a <= 64 {
                -DCT_BASE[64 - a]
            } else if a <= 96 {
                -DCT_BASE[a - 64]
            } else {
                DCT_BASE[128 - a]
            };
            n += 1;
        }
        k += 1;
    }
    m
}

/// The 32×32 inverse-DCT matrix `transMatrix[k][n]`; the N-point matrix is
/// rows `k · 32/N`, columns `0..N`.
pub static DCT32: [[i16; 32]; 32] = build_dct32();

/// 4×4 DST-VII for intra luma 4×4 blocks (§8.6.4.2).
pub static DST4: [[i16; 4]; 4] = [[29, 55, 74, 84], [74, 74, 0, -74], [84, -29, -74, 55], [55, -84, 74, -29]];

/// `intraPredAngle` by mode 2..=34 (Table 8-4); modes 0/1 are unused.
pub static INTRA_PRED_ANGLE: [i32; 35] = [
    0, 0, 32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13, -9, -5, -2, 0, 2,
    5, 9, 13, 17, 21, 26, 32,
];

/// `invAngle` by mode 11..=25 (Table 8-5), indexed by `mode - 11`.
pub static INV_ANGLE: [i32; 15] =
    [-4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096];

/// `QpC` as a function of `qPi` for ChromaArrayType 1 (Table 8-10), qPi 0..=57.
pub static CHROMA_QP_420: [u8; 58] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 29,
    30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
];

/// `levelScale[qP % 6]` (§8.6.3).
pub static LEVEL_SCALE: [i32; 6] = [40, 45, 51, 57, 64, 72];

/// Deblocking `β′` by Q (Table 8-12), Q 0..=51.
pub static BETA_TABLE: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24, 26,
    28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64,
];

/// Deblocking `tC′` by Q (Table 8-12), Q 0..=53.
pub static TC_TABLE: [u8; 54] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4,
    5, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 22, 24,
];

/// `ctxIdxMap` for `sig_coeff_flag` in 4×4 blocks (§9.3.4.2.5), index `(yC << 2) + xC`.
pub static SIG_CTX_MAP_4X4: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diag_scan_matches_spec_order() {
        assert_eq!(&DIAG_4X4[..6], &[(0, 0), (0, 1), (1, 0), (0, 2), (1, 1), (2, 0)]);
        assert_eq!(DIAG_4X4[15], (3, 3));
        assert_eq!(DIAG_2X2, [(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(DIAG_8X8[63], (7, 7));
        // permutation checks
        for s in [&DIAG_8X8[..], &HORIZ_8X8[..], &VERT_8X8[..]] {
            let mut seen = [false; 64];
            for &(x, y) in s {
                assert!(!seen[(y as usize) * 8 + x as usize]);
                seen[(y as usize) * 8 + x as usize] = true;
            }
        }
    }

    #[test]
    fn dct_matrix_has_the_spec_sub_matrices() {
        // 4-point: rows 0, 8, 16, 24
        assert_eq!(&DCT32[0][..4], &[64, 64, 64, 64]);
        assert_eq!(&DCT32[8][..4], &[83, 36, -36, -83]);
        assert_eq!(&DCT32[16][..4], &[64, -64, -64, 64]);
        assert_eq!(&DCT32[24][..4], &[36, -83, 83, -36]);
        // 8-point row 1 = DCT32 row 4
        assert_eq!(&DCT32[4][..8], &[89, 75, 50, 18, -18, -50, -75, -89]);
        // 16-point row 1 = DCT32 row 2
        assert_eq!(&DCT32[2][..16], &[90, 87, 80, 70, 57, 43, 25, 9, -9, -25, -43, -57, -70, -80, -87, -90]);
        // 32-point row 1
        assert_eq!(&DCT32[1][..8], &[90, 90, 88, 85, 82, 78, 73, 67]);
        assert_eq!(DCT32[1][31], -90);
        assert_eq!(DCT32[31][0], 4);
        assert_eq!(DCT32[31][1], -13);
        // near-orthogonality: every row has energy ≈ 32·64²
        for k in 0..32 {
            let e: i64 = DCT32[k].iter().map(|&v| (v as i64) * (v as i64)).sum();
            assert!((e - 32 * 64 * 64).abs() < 32 * 64 * 64 / 50, "row {k} energy {e}");
        }
        for k in 1..32 {
            let dot: i64 = (0..32).map(|n| DCT32[0][n] as i64 * DCT32[k][n] as i64).sum();
            assert!(dot.abs() <= 64 * 4, "row {k} not orthogonal to DC: {dot}");
        }
    }

    #[test]
    fn misc_tables() {
        assert_eq!(INTRA_PRED_ANGLE[10], 0);
        assert_eq!(INTRA_PRED_ANGLE[26], 0);
        assert_eq!(INTRA_PRED_ANGLE[2], 32);
        assert_eq!(INTRA_PRED_ANGLE[18], -32);
        assert_eq!(INV_ANGLE[18 - 11], -256);
        assert_eq!(CHROMA_QP_420[29], 29);
        assert_eq!(CHROMA_QP_420[30], 29);
        assert_eq!(CHROMA_QP_420[43], 37);
        assert_eq!(CHROMA_QP_420[44], 38);
        assert_eq!(CHROMA_QP_420[57], 51);
        assert_eq!(BETA_TABLE[51], 64);
        assert_eq!(TC_TABLE[53], 24);
    }
}
