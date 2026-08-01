//! NEON operations for the encoder.
//!
//! Only `quantize_block` is overridden. The forward DCT stays on the scalar
//! path: porting the 484-line AVX2 FDCT is a much larger job, and it would be
//! shipped **unexecuted** — nothing here can run NEON, so a large hand-written
//! kernel would rest entirely on a cross-compile succeeding. The quantize kernel
//! is ~40 lines against an exhaustive scalar oracle, which is a defensible
//! amount of unverified code; the FDCT is not, and is left as a deliberate gap
//! rather than an unmeasured claim.
//!
//! NEON is baseline on aarch64, so no runtime detection is required.

use crate::encode::encoder::Operations;

pub(crate) struct NeonOperations;

impl Operations for NeonOperations {
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
