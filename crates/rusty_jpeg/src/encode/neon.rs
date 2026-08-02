//! NEON operations for the encoder.
//!
//! Overrides both `fdct` and `quantize_block`.
//!
//! The FDCT is not a transcription of the 484-line AVX2 kernel. It is the
//! generic butterfly in [`fdct_simd`](crate::encode::fdct_simd), written once
//! and instantiated over a NEON backend — so the transform is verified BIT-EXACT
//! on x86 (`generic_butterfly_matches_scalar_fdct`) and only the fifteen
//! one-line intrinsic mappings are certified elsewhere, by ARM CI.
//!
//! NEON is baseline on aarch64, so no runtime detection is required.

use crate::encode::encoder::Operations;

pub(crate) struct NeonOperations;

/// `RUSTY_JPEG_NEON_FDCT=0` falls back to the scalar forward DCT.
///
/// This kernel is the one piece of the crate whose intrinsic mapping is
/// certified only by ARM CI and not by the machine it was written on, so it
/// ships with a runtime off-switch that needs no rebuild. Resolved once.
fn neon_fdct_enabled() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("RUSTY_JPEG_NEON_FDCT")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

impl Operations for NeonOperations {
    #[inline(always)]
    fn fdct(data: &mut [i16; 64]) {
        if neon_fdct_enabled() {
            crate::encode::fdct_simd::neon::fdct_neon(data);
        } else {
            crate::encode::fdct::fdct(data);
        }
    }

    #[inline(always)]
    fn quantize_block(
        block: &[i16; 64],
        q_block: &mut [i16; 64],
        table: &crate::encode::quantization::QuantizationTable,
    ) {
        // SAFETY: NEON is guaranteed present on aarch64, and this module is only
        // compiled for that target. The kernel touches fixed-size 64-element
        // arrays at compile-time-known offsets.
        unsafe {
            crate::encode::quantization::neon::quantize_block_neon(block, q_block, table);
        }
    }
}
