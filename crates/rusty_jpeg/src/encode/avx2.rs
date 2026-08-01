mod fdct;
mod ycbcr;

use crate::encode::encoder::Operations;
pub use fdct::fdct_avx2;
pub use ycbcr::*;

pub(crate) struct AVX2Operations;

impl Operations for AVX2Operations {
    #[inline(always)]
    fn fdct(data: &mut [i16; 64]) {
        fdct_avx2(data);
    }

    #[inline(always)]
    fn quantize_block(
        block: &[i16; 64],
        q_block: &mut [i16; 64],
        table: &crate::encode::quantization::QuantizationTable,
    ) {
        // SAFETY: `AVX2Operations` is only ever instantiated behind a runtime
        // `is_x86_feature_detected!("avx2")` check in `Encoder::encode_image`,
        // which is the same guard `fdct_avx2` above relies on. The kernel only
        // touches fixed-size 64-element arrays at compile-time-known offsets.
        unsafe {
            crate::encode::quantization::avx2::quantize_block_avx2(block, q_block, table);
        }
    }
}
