//! AVX2 IDCT that transforms **two 8x8 blocks at once**.
//!
//! This is the SSSE3 kernel widened to 256 bits, with block A in the low
//! 128-bit lane and block B in the high one. That is safe to do mechanically
//! because every operation involved is lane-independent:
//!
//! - the arithmetic is elementwise (`mulhrs`/`adds`/`subs`/`slli`/`srai`);
//! - `_mm256_unpack*` interleave WITHIN each 128-bit lane, so the 8x8 transpose
//!   below transposes each block separately without any cross-lane traffic;
//! - `_mm256_packus_epi16` likewise packs per lane.
//!
//! So each lane executes exactly the instruction sequence SSSE3 executed, on
//! exactly the same inputs. The output is **byte-identical** to calling the
//! SSSE3 kernel twice -- which is what `avx2_pair_matches_ssse3_twice` asserts,
//! and why this needed no tolerance and no requantisation of the constants.

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// One IDCT pass over 8 rows, each holding two blocks' worth of coefficients.
///
/// Line-for-line the SSSE3 `idct8`; only the register width differs. The
/// fixed-point constants are unchanged, so the rounding is unchanged.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn idct8(data: &mut [__m256i; 8]) {
    let p2 = data[2];
    let p3 = data[6];
    let p1 = _mm256_mulhrs_epi16(_mm256_adds_epi16(p2, p3), _mm256_set1_epi16(17734)); // 0.5411961
    let t2 = _mm256_subs_epi16(
        _mm256_subs_epi16(p1, p3),
        _mm256_mulhrs_epi16(p3, _mm256_set1_epi16(27779)), // 0.847759065
    );
    let t3 = _mm256_adds_epi16(p1, _mm256_mulhrs_epi16(p2, _mm256_set1_epi16(25079))); // 0.765366865

    let p2 = data[0];
    let p3 = data[4];
    let t0 = _mm256_adds_epi16(p2, p3);
    let t1 = _mm256_subs_epi16(p2, p3);

    let x0 = _mm256_adds_epi16(t0, t3);
    let x3 = _mm256_subs_epi16(t0, t3);
    let x1 = _mm256_adds_epi16(t1, t2);
    let x2 = _mm256_subs_epi16(t1, t2);

    let t0 = data[7];
    let t1 = data[5];
    let t2 = data[3];
    let t3 = data[1];

    let p3 = _mm256_adds_epi16(t0, t2);
    let p4 = _mm256_adds_epi16(t1, t3);
    let p1 = _mm256_adds_epi16(t0, t3);
    let p2 = _mm256_adds_epi16(t1, t2);
    let p5 = _mm256_adds_epi16(p3, p4);
    let p5 = _mm256_adds_epi16(p5, _mm256_mulhrs_epi16(p5, _mm256_set1_epi16(5763))); // 0.175875602

    let t0 = _mm256_mulhrs_epi16(t0, _mm256_set1_epi16(9786)); // 0.298631336
    let t1 = _mm256_adds_epi16(
        _mm256_adds_epi16(t1, t1),
        _mm256_mulhrs_epi16(t1, _mm256_set1_epi16(1741)), // 0.053119869
    );
    let t2 = _mm256_adds_epi16(
        _mm256_adds_epi16(t2, _mm256_adds_epi16(t2, t2)),
        _mm256_mulhrs_epi16(t2, _mm256_set1_epi16(2383)), // 0.072711026
    );
    let t3 = _mm256_adds_epi16(t3, _mm256_mulhrs_epi16(t3, _mm256_set1_epi16(16427))); // 0.501321110

    let p1 = _mm256_subs_epi16(p5, _mm256_mulhrs_epi16(p1, _mm256_set1_epi16(29490))); // 0.899976223
    let p2 = _mm256_subs_epi16(
        _mm256_subs_epi16(_mm256_subs_epi16(p5, p2), p2),
        _mm256_mulhrs_epi16(p2, _mm256_set1_epi16(18446)), // 0.562915447
    );

    let p3 = _mm256_subs_epi16(
        _mm256_mulhrs_epi16(p3, _mm256_set1_epi16(-31509)), // -0.961570560
        p3,
    );
    let p4 = _mm256_mulhrs_epi16(p4, _mm256_set1_epi16(-12785)); // -0.390180644

    let t3 = _mm256_adds_epi16(_mm256_adds_epi16(p1, p4), t3);
    let t2 = _mm256_adds_epi16(_mm256_adds_epi16(p2, p3), t2);
    let t1 = _mm256_adds_epi16(_mm256_adds_epi16(p2, p4), t1);
    let t0 = _mm256_adds_epi16(_mm256_adds_epi16(p1, p3), t0);

    data[0] = _mm256_adds_epi16(x0, t3);
    data[7] = _mm256_subs_epi16(x0, t3);
    data[1] = _mm256_adds_epi16(x1, t2);
    data[6] = _mm256_subs_epi16(x1, t2);
    data[2] = _mm256_adds_epi16(x2, t1);
    data[5] = _mm256_subs_epi16(x2, t1);
    data[3] = _mm256_adds_epi16(x3, t0);
    data[4] = _mm256_subs_epi16(x3, t0);
}

/// Transpose both blocks. The unpack instructions never cross the 128-bit lane
/// boundary, so this is two independent 8x8 transposes for the price of one.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn transpose8(data: &mut [__m256i; 8]) {
    let d01l = _mm256_unpacklo_epi16(data[0], data[1]);
    let d23l = _mm256_unpacklo_epi16(data[2], data[3]);
    let d45l = _mm256_unpacklo_epi16(data[4], data[5]);
    let d67l = _mm256_unpacklo_epi16(data[6], data[7]);
    let d01h = _mm256_unpackhi_epi16(data[0], data[1]);
    let d23h = _mm256_unpackhi_epi16(data[2], data[3]);
    let d45h = _mm256_unpackhi_epi16(data[4], data[5]);
    let d67h = _mm256_unpackhi_epi16(data[6], data[7]);

    let d0123ll = _mm256_unpacklo_epi32(d01l, d23l);
    let d0123lh = _mm256_unpackhi_epi32(d01l, d23l);
    let d4567ll = _mm256_unpacklo_epi32(d45l, d67l);
    let d4567lh = _mm256_unpackhi_epi32(d45l, d67l);
    let d0123hl = _mm256_unpacklo_epi32(d01h, d23h);
    let d0123hh = _mm256_unpackhi_epi32(d01h, d23h);
    let d4567hl = _mm256_unpacklo_epi32(d45h, d67h);
    let d4567hh = _mm256_unpackhi_epi32(d45h, d67h);

    data[0] = _mm256_unpacklo_epi64(d0123ll, d4567ll);
    data[1] = _mm256_unpackhi_epi64(d0123ll, d4567ll);
    data[2] = _mm256_unpacklo_epi64(d0123lh, d4567lh);
    data[3] = _mm256_unpackhi_epi64(d0123lh, d4567lh);
    data[4] = _mm256_unpacklo_epi64(d0123hl, d4567hl);
    data[5] = _mm256_unpackhi_epi64(d0123hl, d4567hl);
    data[6] = _mm256_unpacklo_epi64(d0123hh, d4567hh);
    data[7] = _mm256_unpackhi_epi64(d0123hh, d4567hh);
}

/// Dequantize and inverse-transform two 8x8 blocks in one instruction stream.
///
/// Both blocks belong to the SAME component, so they share a quantization
/// table and a line stride, and both write into one plane. Taking a single
/// output slice plus block B's relative origin avoids handing out two
/// overlapping `&mut` slices -- the blocks interleave row by row without ever
/// touching the same byte, which is true but not something the borrow checker
/// can be told.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
pub unsafe fn dequantize_and_idct_block_8x8_pair(
    coefficients_a: &[i16; 64],
    coefficients_b: &[i16; 64],
    quantization_table: &[u16; 64],
    output_linestride: usize,
    output: &mut [u8],
    output_offset_b: usize,
) {
    // Last write is at offset_b + linestride * 7 + 7.
    assert!(
        output.len()
            > output_offset_b
                .checked_add(output_linestride.checked_mul(7).unwrap())
                .unwrap()
                .checked_add(7)
                .unwrap()
    );

    const SHIFT: i32 = 3;

    let mut data = [_mm256_setzero_si256(); 8];
    for (i, item) in data.iter_mut().enumerate() {
        // Lane 0 <- block A row i, lane 1 <- block B row i.
        let coef = _mm256_inserti128_si256::<1>(
            _mm256_castsi128_si256(_mm_loadu_si128(
                coefficients_a.as_ptr().wrapping_add(i * 8) as *const _
            )),
            _mm_loadu_si128(coefficients_b.as_ptr().wrapping_add(i * 8) as *const _),
        );
        // Same table in both lanes.
        let quant = _mm256_broadcastsi128_si256(_mm_loadu_si128(
            quantization_table.as_ptr().wrapping_add(i * 8) as *const _,
        ));
        *item = _mm256_slli_epi16::<SHIFT>(_mm256_mullo_epi16(coef, quant));
    }

    // A half-height column pass for the 82.7% of blocks whose rows 4-7 are zero
    // was built, gated bit-identical, and MEASURED WORSE (2/11, z -2.11, twice):
    // the per-pair zero test plus an unpredictable branch cost more than the ~15
    // instructions it saved, and the pair kernel needs BOTH blocks sparse where
    // 82.7% is a per-block figure. Removed rather than left behind a toggle,
    // because the branch was the cost. See WHYS.md D4g.
    idct8(&mut data);
    transpose8(&mut data);
    idct8(&mut data);
    transpose8(&mut data);

    for (i, item) in data.iter().enumerate() {
        const OFFSET: i16 = 128 << (SHIFT + 3);
        const ROUNDING_BIAS: i16 = (1 << (SHIFT + 3)) >> 1;

        let with_offset = _mm256_adds_epi16(*item, _mm256_set1_epi16(OFFSET + ROUNDING_BIAS));
        // packus is per-lane: lane 0's low 8 bytes are block A's row, lane 1's
        // low 8 bytes are block B's.
        let packed = _mm256_packus_epi16(
            _mm256_srai_epi16::<{ SHIFT + 3 }>(with_offset),
            _mm256_setzero_si256(),
        );

        let base = output.as_mut_ptr().wrapping_add(output_linestride * i);

        let mut buf = [0u8; 16];
        _mm_storeu_si128(buf.as_mut_ptr() as *mut _, _mm256_castsi256_si128(packed));
        core::ptr::copy_nonoverlapping::<u8>(buf.as_ptr(), base, 8);

        _mm_storeu_si128(
            buf.as_mut_ptr() as *mut _,
            _mm256_extracti128_si256::<1>(packed),
        );
        core::ptr::copy_nonoverlapping::<u8>(buf.as_ptr(), base.wrapping_add(output_offset_b), 8);
    }
}
