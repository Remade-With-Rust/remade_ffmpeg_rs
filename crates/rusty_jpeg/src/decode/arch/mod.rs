#![allow(unsafe_code)]

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod avx2;
mod neon;
mod ssse3;
mod wasm;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use std::is_x86_feature_detected;

/// Arch-specific implementation of YCbCr conversion. Returns the number of pixels that were
/// converted.
#[allow(clippy::type_complexity)]
pub fn get_color_convert_line_ycbcr() -> Option<unsafe fn(&[u8], &[u8], &[u8], &mut [u8]) -> usize>
{
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[allow(unsafe_code)]
    {
        if is_x86_feature_detected!("ssse3") {
            return Some(ssse3::color_convert_line_ycbcr);
        }
    }
    // Runtime detection is not needed on aarch64.
    #[cfg(all(feature = "nightly_aarch64_neon", target_arch = "aarch64"))]
    {
        return Some(neon::color_convert_line_ycbcr);
    }
    #[cfg(all(target_feature = "simd128", target_arch = "wasm32"))]
    {
        return Some(wasm::color_convert_line_ycbcr);
    }
    #[allow(unreachable_code)]
    None
}

/// Arch-specific implementation of 8x8 IDCT.
#[allow(clippy::type_complexity)]
pub fn get_dequantize_and_idct_block_8x8(
) -> Option<unsafe fn(&[i16; 64], &[u16; 64], usize, &mut [u8])> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[allow(unsafe_code)]
    {
        if is_x86_feature_detected!("ssse3") {
            return Some(ssse3::dequantize_and_idct_block_8x8);
        }
    }
    // Runtime detection is not needed on aarch64.
    #[cfg(all(feature = "nightly_aarch64_neon", target_arch = "aarch64"))]
    {
        return Some(neon::dequantize_and_idct_block_8x8);
    }
    #[cfg(all(target_feature = "simd128", target_arch = "wasm32"))]
    {
        return Some(wasm::dequantize_and_idct_block_8x8);
    }
    #[allow(unreachable_code)]
    None
}

/// Arch-specific IDCT that does **two** 8x8 blocks per call.
///
/// Only offered when the pair actually pays: the caller must have two blocks
/// that both need a full transform (see `append_row_immediate`).
#[allow(clippy::type_complexity)]
pub fn get_dequantize_and_idct_block_8x8_pair(
) -> Option<unsafe fn(&[i16; 64], &[i16; 64], &[u16; 64], usize, &mut [u8], usize)> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[allow(unsafe_code)]
    {
        if is_x86_feature_detected!("avx2") {
            return Some(avx2::dequantize_and_idct_block_8x8_pair);
        }
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64")))]
mod pair_tests {
    /// The AVX2 pair kernel must reproduce the SSSE3 kernel EXACTLY.
    ///
    /// It is the same instruction sequence per 128-bit lane on the same inputs,
    /// so anything less than byte-identical means a lane got crossed -- most
    /// likely in the transpose or the final pack. No tolerance is appropriate
    /// here even though an IDCT is approximate: the two paths must agree bit
    /// for bit or the decoder's output depends on which CPU ran it.
    #[test]
    fn avx2_pair_matches_ssse3_twice() {
        if !std::is_x86_feature_detected!("avx2") || !std::is_x86_feature_detected!("ssse3") {
            return;
        }

        // A cheap deterministic PRNG; the point is coverage of sign, magnitude
        // and saturation, not statistical quality.
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for round in 0..64 {
            let mut ca = [0i16; 64];
            let mut cb = [0i16; 64];
            let mut qa = [0u16; 64];
            for i in 0..64 {
                // Round 0 exercises the extremes, the rest are pseudo-random.
                if round == 0 {
                    ca[i] = i16::MAX;
                    cb[i] = i16::MIN;
                    qa[i] = u16::MAX;
                } else {
                    ca[i] = (next() % 2048) as i16 - 1024;
                    cb[i] = (next() % 2048) as i16 - 1024;
                    qa[i] = (next() % 255 + 1) as u16;
                }
                // Half the rounds drive the SPARSE column pass (rows 4-7 zero),
                // which 82.7% of real blocks take. Dense random data almost
                // never triggers it, so without this the sparse path would be
                // shipped untested.
                if round % 2 == 1 && i >= 32 {
                    ca[i] = 0;
                    cb[i] = 0;
                }
                // ...and one round drives the boundary: sparse in A, dense in B,
                // which must fall back to the general path for BOTH lanes.
                if round == 3 && i >= 32 {
                    cb[i] = (next() % 512) as i16 - 256;
                }
            }

            // Non-trivial, and DIFFERENT, strides: a lane-crossing bug that
            // happened to use one block's stride for both would survive equal
            // strides.
            // One plane, two horizontally adjacent blocks, exactly as the
            // decoder lays them out.
            let stride = 24usize;
            let offset_b = 8usize;
            let len = stride * 8 + 32;
            let mut want = vec![0u8; len];
            let mut got = vec![0u8; len];

            unsafe {
                super::ssse3::dequantize_and_idct_block_8x8(&ca, &qa, stride, &mut want);
                super::ssse3::dequantize_and_idct_block_8x8(
                    &cb,
                    &qa,
                    stride,
                    &mut want[offset_b..],
                );
                super::avx2::dequantize_and_idct_block_8x8_pair(
                    &ca, &cb, &qa, stride, &mut got, offset_b,
                );
            }

            assert_eq!(got, want, "pair diverged from SSSE3 in round {round}");
        }
    }
}
