use alloc::boxed::Box;
use core::num::NonZeroU16;

/// # Quantization table used for encoding
///
/// Tables are based on tables from mozjpeg
#[derive(Debug, Clone)]
pub enum QuantizationTableType {
    /// Sample quantization tables given in Annex K (Clause K.1) of Recommendation ITU-T T.81 (1992) | ISO/IEC 10918-1:1994.
    Default,

    /// Flat
    Flat,

    /// Custom, tuned for MS-SSIM
    CustomMsSsim,

    /// Custom, tuned for PSNR-HVS
    CustomPsnrHvs,

    /// ImageMagick table by N. Robidoux
    ///
    /// From <http://www.imagemagick.org/discourse-server/viewtopic.php?f=22&t=20333&p=98008#p98008>
    ImageMagick,

    /// Relevance of human vision to JPEG-DCT compression (1992) Klein, Silverstein and Carney.
    KleinSilversteinCarney,

    /// DCTune perceptual optimization of compressed dental X-Rays (1997) Watson, Taylor, Borthwick
    DentalXRays,

    /// A visual detection model for DCT coefficient quantization (12/9/93) Ahumada, Watson, Peterson
    VisualDetectionModel,

    /// An improved detection model for DCT coefficient quantization (1993) Peterson, Ahumada and Watson
    ImprovedDetectionModel,

    /// A user supplied quantization table
    Custom(Box<[u16; 64]>),
}

impl QuantizationTableType {
    fn index(&self) -> usize {
        use QuantizationTableType::*;

        match self {
            Default => 0,
            Flat => 1,
            CustomMsSsim => 2,
            CustomPsnrHvs => 3,
            ImageMagick => 4,
            KleinSilversteinCarney => 5,
            DentalXRays => 6,
            VisualDetectionModel => 7,
            ImprovedDetectionModel => 8,
            Custom(_) => panic!("Custom types not supported"),
        }
    }
}

// Tables are based on mozjpeg jcparam.c
static DEFAULT_LUMA_TABLES: [[u16; 64]; 9] = [
    [
        // Annex K
        16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69,
        56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81,
        104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
    ],
    [
        // Flat
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    ],
    [
        // Custom, tuned for MS-SSIM
        12, 17, 20, 21, 30, 34, 56, 63, 18, 20, 20, 26, 28, 51, 61, 55, 19, 20, 21, 26, 33, 58, 69,
        55, 26, 26, 26, 30, 46, 87, 86, 66, 31, 33, 36, 40, 46, 96, 100, 73, 40, 35, 46, 62, 81,
        100, 111, 91, 46, 66, 76, 86, 102, 121, 120, 101, 68, 90, 90, 96, 113, 102, 105, 103,
    ],
    [
        // Custom, tuned for PSNR-HVS
        9, 10, 12, 14, 27, 32, 51, 62, 11, 12, 14, 19, 27, 44, 59, 73, 12, 14, 18, 25, 42, 59, 79,
        78, 17, 18, 25, 42, 61, 92, 87, 92, 23, 28, 42, 75, 79, 112, 112, 99, 40, 42, 59, 84, 88,
        124, 132, 111, 42, 64, 78, 95, 105, 126, 125, 99, 70, 75, 100, 102, 116, 100, 107, 98,
    ],
    [
        // ImageMagick table by N. Robidoux
        // From http://www.imagemagick.org/discourse-server/viewtopic.php?f=22&t=20333&p=98008#p98008
        16, 16, 16, 18, 25, 37, 56, 85, 16, 17, 20, 27, 34, 40, 53, 75, 16, 20, 24, 31, 43, 62, 91,
        135, 18, 27, 31, 40, 53, 74, 106, 156, 25, 34, 43, 53, 69, 94, 131, 189, 37, 40, 62, 74,
        94, 124, 169, 238, 56, 53, 91, 106, 131, 169, 226, 311, 85, 75, 135, 156, 189, 238, 311,
        418,
    ],
    [
        // Relevance of human vision to JPEG-DCT compression (1992) Klein, Silverstein and Carney.
        10, 12, 14, 19, 26, 38, 57, 86, 12, 18, 21, 28, 35, 41, 54, 76, 14, 21, 25, 32, 44, 63, 92,
        136, 19, 28, 32, 41, 54, 75, 107, 157, 26, 35, 44, 54, 70, 95, 132, 190, 38, 41, 63, 75,
        95, 125, 170, 239, 57, 54, 92, 107, 132, 170, 227, 312, 86, 76, 136, 157, 190, 239, 312,
        419,
    ],
    [
        // DCTune perceptual optimization of compressed dental X-Rays (1997) Watson, Taylor, Borthwick
        7, 8, 10, 14, 23, 44, 95, 241, 8, 8, 11, 15, 25, 47, 102, 255, 10, 11, 13, 19, 31, 58, 127,
        255, 14, 15, 19, 27, 44, 83, 181, 255, 23, 25, 31, 44, 72, 136, 255, 255, 44, 47, 58, 83,
        136, 255, 255, 255, 95, 102, 127, 181, 255, 255, 255, 255, 241, 255, 255, 255, 255, 255,
        255, 255,
    ],
    [
        // A visual detection model for DCT coefficient quantization (12/9/93) Ahumada, Watson, Peterson
        15, 11, 11, 12, 15, 19, 25, 32, 11, 13, 10, 10, 12, 15, 19, 24, 11, 10, 14, 14, 16, 18, 22,
        27, 12, 10, 14, 18, 21, 24, 28, 33, 15, 12, 16, 21, 26, 31, 36, 42, 19, 15, 18, 24, 31, 38,
        45, 53, 25, 19, 22, 28, 36, 45, 55, 65, 32, 24, 27, 33, 42, 53, 65, 77,
    ],
    [
        // An improved detection model for DCT coefficient quantization (1993) Peterson, Ahumada and Watson
        14, 10, 11, 14, 19, 25, 34, 45, 10, 11, 11, 12, 15, 20, 26, 33, 11, 11, 15, 18, 21, 25, 31,
        38, 14, 12, 18, 24, 28, 33, 39, 47, 19, 15, 21, 28, 36, 43, 51, 59, 25, 20, 25, 33, 43, 54,
        64, 74, 34, 26, 31, 39, 51, 64, 77, 91, 45, 33, 38, 47, 59, 74, 91, 108,
    ],
];

// Tables are based on mozjpeg jcparam.c
static DEFAULT_CHROMA_TABLES: [[u16; 64]; 9] = [
    [
        // Annex K
        17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99,
        99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    ],
    [
        // Flat
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    ],
    [
        // Custom, tuned for MS-SSIM
        8, 12, 15, 15, 86, 96, 96, 98, 13, 13, 15, 26, 90, 96, 99, 98, 12, 15, 18, 96, 99, 99, 99,
        99, 17, 16, 90, 96, 99, 99, 99, 99, 96, 96, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    ],
    [
        //Custom, tuned for PSNR-HVS
        9, 10, 17, 19, 62, 89, 91, 97, 12, 13, 18, 29, 84, 91, 88, 98, 14, 19, 29, 93, 95, 95, 98,
        97, 20, 26, 84, 88, 95, 95, 98, 94, 26, 86, 91, 93, 97, 99, 98, 99, 99, 100, 98, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 97, 97, 99, 99, 99, 99, 97, 99,
    ],
    [
        // ImageMagick table by N. Robidoux
        // From http://www.imagemagick.org/discourse-server/viewtopic.php?f=22&t=20333&p=98008#p98008
        16, 16, 16, 18, 25, 37, 56, 85, 16, 17, 20, 27, 34, 40, 53, 75, 16, 20, 24, 31, 43, 62, 91,
        135, 18, 27, 31, 40, 53, 74, 106, 156, 25, 34, 43, 53, 69, 94, 131, 189, 37, 40, 62, 74,
        94, 124, 169, 238, 56, 53, 91, 106, 131, 169, 226, 311, 85, 75, 135, 156, 189, 238, 311,
        418,
    ],
    [
        // Relevance of human vision to JPEG-DCT compression (1992) Klein, Silverstein and Carney.
        10, 12, 14, 19, 26, 38, 57, 86, 12, 18, 21, 28, 35, 41, 54, 76, 14, 21, 25, 32, 44, 63, 92,
        136, 19, 28, 32, 41, 54, 75, 107, 157, 26, 35, 44, 54, 70, 95, 132, 190, 38, 41, 63, 75,
        95, 125, 170, 239, 57, 54, 92, 107, 132, 170, 227, 312, 86, 76, 136, 157, 190, 239, 312,
        419,
    ],
    [
        // DCTune perceptual optimization of compressed dental X-Rays (1997) Watson, Taylor, Borthwick
        7, 8, 10, 14, 23, 44, 95, 241, 8, 8, 11, 15, 25, 47, 102, 255, 10, 11, 13, 19, 31, 58, 127,
        255, 14, 15, 19, 27, 44, 83, 181, 255, 23, 25, 31, 44, 72, 136, 255, 255, 44, 47, 58, 83,
        136, 255, 255, 255, 95, 102, 127, 181, 255, 255, 255, 255, 241, 255, 255, 255, 255, 255,
        255, 255,
    ],
    [
        // A visual detection model for DCT coefficient quantization (12/9/93) Ahumada, Watson, Peterson
        15, 11, 11, 12, 15, 19, 25, 32, 11, 13, 10, 10, 12, 15, 19, 24, 11, 10, 14, 14, 16, 18, 22,
        27, 12, 10, 14, 18, 21, 24, 28, 33, 15, 12, 16, 21, 26, 31, 36, 42, 19, 15, 18, 24, 31, 38,
        45, 53, 25, 19, 22, 28, 36, 45, 55, 65, 32, 24, 27, 33, 42, 53, 65, 77,
    ],
    [
        // An improved detection model for DCT coefficient quantization (1993) Peterson, Ahumada and Watson
        14, 10, 11, 14, 19, 25, 34, 45, 10, 11, 11, 12, 15, 20, 26, 33, 11, 11, 15, 18, 21, 25, 31,
        38, 14, 12, 18, 24, 28, 33, 39, 47, 19, 15, 21, 28, 36, 43, 51, 59, 25, 20, 25, 33, 43, 54,
        64, 74, 34, 26, 31, 39, 51, 64, 77, 91, 45, 33, 38, 47, 59, 74, 91, 108,
    ],
];

const SHIFT: u32 = 2 * 8 - 1;

fn compute_reciprocal(divisor: u32) -> (i32, i32) {
    if divisor <= 1 {
        return (1, 0);
    }

    let mut reciprocals = (1 << SHIFT) / divisor;
    let fractional = (1 << SHIFT) % divisor;

    // Correction for rounding errors in division
    let mut correction = divisor / 2;

    if fractional != 0 {
        if fractional <= correction {
            correction += 1;
        } else {
            reciprocals += 1;
        }
    }

    (reciprocals as i32, correction as i32)
}

pub struct QuantizationTable {
    table: [NonZeroU16; 64],
    reciprocals: [i32; 64],
    corrections: [i32; 64],
    /// `reciprocals` / `corrections` pre-permuted into **zig-zag order**.
    ///
    /// The quantize loop runs `q[i] = f(block[ZIGZAG[i]], recip[ZIGZAG[i]],
    /// corr[ZIGZAG[i]])`, i.e. three permuted reads per coefficient. Two of
    /// them are into tables that are constant for the whole image, so the
    /// permutation can be applied once at table-build time instead of 64 times
    /// per block — 174M redundant index computations for one 4K frame. Only the
    /// read of `block` genuinely has to be permuted.
    reciprocals_zz: [i32; 64],
    corrections_zz: [i32; 64],
}

impl QuantizationTable {
    pub fn new_with_quality(
        table: &QuantizationTableType,
        quality: u8,
        luma: bool,
    ) -> QuantizationTable {
        let table = match table {
            QuantizationTableType::Custom(table) => Self::get_user_table(table),
            table => {
                let table = if luma {
                    &DEFAULT_LUMA_TABLES[table.index()]
                } else {
                    &DEFAULT_CHROMA_TABLES[table.index()]
                };
                Self::get_with_quality(table, quality)
            }
        };

        let mut reciprocals = [0i32; 64];
        let mut corrections = [0i32; 64];

        for i in 0..64 {
            let (reciprocal, correction) = compute_reciprocal(table[i].get() as u32);

            reciprocals[i] = reciprocal;
            corrections[i] = correction;
        }

        let mut reciprocals_zz = [0i32; 64];
        let mut corrections_zz = [0i32; 64];
        for i in 0..64 {
            let z = crate::encode::writer::ZIGZAG[i] as usize & 0x3f;
            reciprocals_zz[i] = reciprocals[z];
            corrections_zz[i] = corrections[z];
        }

        QuantizationTable {
            table,
            reciprocals,
            corrections,
            reciprocals_zz,
            corrections_zz,
        }
    }

    fn get_user_table(table: &[u16; 64]) -> [NonZeroU16; 64] {
        let mut q_table = [NonZeroU16::new(1).unwrap(); 64];
        for (i, &v) in table.iter().enumerate() {
            q_table[i] = match NonZeroU16::new(v.clamp(1, 2 << 10) << 3) {
                Some(v) => v,
                None => panic!("Invalid quantization table value: {}", v),
            };
        }
        q_table
    }

    fn get_with_quality(table: &[u16; 64], quality: u8) -> [NonZeroU16; 64] {
        let quality = quality.clamp(1, 100) as u32;

        let scale = if quality < 50 {
            5000 / quality
        } else {
            200 - quality * 2
        };

        let mut q_table = [NonZeroU16::new(1).unwrap(); 64];

        for (i, &v) in table.iter().enumerate() {
            let v = v as u32;

            let v = (v * scale + 50) / 100;

            let v = v.clamp(1, 255) as u16;

            // Table values are premultiplied with 8 because dct is scaled by 8
            q_table[i] = NonZeroU16::new(v << 3).unwrap();
        }
        q_table
    }

    #[inline]
    pub fn get(&self, index: usize) -> u8 {
        (self.table[index].get() >> 3) as u8
    }

    /// Quantize one coefficient.
    ///
    /// The trailing `if value != abs_value { product *= -1 }` looks like a
    /// coin-flip branch on the sign of every DCT coefficient — ~174M
    /// unpredictable branches for one 4K frame. **It is not.** LLVM compiles it
    /// to a conditional move; the crate emits 156 `cmov`s. A hand-written
    /// branchless form (`(v ^ sign) - sign`, kept below) measured *no better*
    /// and, on both content classes, marginally worse. See `WHYS.md` D4a.
    #[inline]
    pub fn quantize(&self, in_value: i16, index: usize) -> i16 {
        let value = in_value as i32;

        let reciprocal = self.reciprocals[index];
        let corrections = self.corrections[index];

        let abs_value = value.abs();

        let mut product = (abs_value + corrections) * reciprocal;
        product >>= SHIFT;

        if value != abs_value {
            product *= -1;
        }

        product as i16
    }

    /// Quantize the coefficient destined for **zig-zag position `i`**.
    ///
    /// Identical arithmetic to [`quantize`](Self::quantize); the only difference
    /// is that the two table reads are sequential (`[i]`) rather than permuted
    /// (`[ZIGZAG[i]]`), because the permutation was folded into the tables when
    /// they were built. The caller still passes the permuted coefficient.
    #[inline]
    pub(crate) fn quantize_zz(&self, in_value: i16, i: usize) -> i16 {
        let value = in_value as i32;

        let reciprocal = self.reciprocals_zz[i];
        let corrections = self.corrections_zz[i];

        let abs_value = value.abs();

        let mut product = (abs_value + corrections) * reciprocal;
        product >>= SHIFT;

        if value != abs_value {
            product *= -1;
        }

        product as i16
    }

    /// Hand-branchless variant — **measured, refuted, retained**.
    ///
    /// `sign` is `0` for non-negative and `-1` for negative, so `(v ^ sign) -
    /// sign` is `v` / `-v`. Bit-identical to [`quantize`](Self::quantize) for
    /// every `i16` input (`quantize_matches_branchy` proves it exhaustively).
    ///
    /// Kept, with its A/B knob, because re-testing costs nothing and the verdict
    /// may change if the surrounding stages shrink or on a target whose compiler
    /// does not emit the conditional move.
    #[inline]
    // Used only by `quantize_matches_branchy`. Retained deliberately: this is
    // the branchless variant that measured SLOWER than the branchy one it was
    // meant to replace, and keeping it with its equivalence proof is what stops
    // the idea being re-litigated.
    #[allow(dead_code)]
    pub(crate) fn quantize_branchless(&self, in_value: i16, index: usize) -> i16 {
        let value = in_value as i32;
        let reciprocal = self.reciprocals[index];
        let corrections = self.corrections[index];
        let sign = value >> 31;
        let abs_value = (value ^ sign) - sign;
        let product = ((abs_value + corrections) * reciprocal) >> SHIFT;
        ((product ^ sign) - sign) as i16
    }
}

#[cfg(test)]
mod branchless_tests {
    use crate::encode::quantization::{QuantizationTable, QuantizationTableType};

    /// Exhaustive: every `i16` input, at every one of the 64 table positions,
    /// for both a luma and a chroma table at several qualities. A branchless
    /// rewrite that is wrong on one value in 65536 would corrupt real images
    /// rarely enough to survive any sampled test, so this samples nothing.
    #[test]
    fn quantize_matches_branchy() {
        for quality in [1u8, 25, 50, 75, 90, 100] {
            for luma in [true, false] {
                let t = QuantizationTable::new_with_quality(
                    &QuantizationTableType::Default,
                    quality,
                    luma,
                );
                for index in 0..64 {
                    for v in i16::MIN..=i16::MAX {
                        let got = t.quantize(v, index);
                        let want = t.quantize_branchless(v, index);
                        assert_eq!(
                            got, want,
                            "quality {quality} luma {luma} index {index} value {v}"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::encode::quantization::{QuantizationTable, QuantizationTableType};

    #[test]
    fn test_new_100() {
        let q = QuantizationTable::new_with_quality(&QuantizationTableType::Default, 100, true);

        for &v in &q.table {
            let v = v.get();
            assert_eq!(v, 1 << 3);
        }

        let q = QuantizationTable::new_with_quality(&QuantizationTableType::Default, 100, false);

        for &v in &q.table {
            let v = v.get();
            assert_eq!(v, 1 << 3);
        }
    }

    #[test]
    fn test_new_100_quantize() {
        let q = QuantizationTable::new_with_quality(&QuantizationTableType::Default, 100, true);

        for i in -255..255 {
            assert_eq!(i, q.quantize(i << 3, 0));
        }
    }
}

#[cfg(test)]
mod zigzag_table_tests {
    use crate::encode::quantization::{QuantizationTable, QuantizationTableType};
    use crate::encode::writer::ZIGZAG;

    /// The pre-permuted tables must reproduce the permuted lookups exactly, at
    /// every position and every quality — otherwise coefficients get quantized
    /// by the wrong divisor, which corrupts the image without failing to decode.
    #[test]
    fn quantize_zz_matches_permuted_lookup() {
        for quality in [1u8, 25, 50, 75, 90, 100] {
            for luma in [true, false] {
                let t = QuantizationTable::new_with_quality(
                    &QuantizationTableType::Default,
                    quality,
                    luma,
                );
                for i in 0..64 {
                    let z = ZIGZAG[i] as usize & 0x3f;
                    for v in i16::MIN..=i16::MAX {
                        assert_eq!(
                            t.quantize_zz(v, i),
                            t.quantize(v, z),
                            "quality {quality} luma {luma} i {i} z {z} value {v}"
                        );
                    }
                }
            }
        }
    }
}

/// Scalar reference quantizer for a whole block — **the oracle**.
///
/// `block` is in natural (row-major) order; `q_block` comes out in **zig-zag**
/// order, which is what the entropy coder consumes.
///
/// Kept `#[inline(never)]` so it has a real symbol to inspect in `--emit asm`:
/// "the compiler already vectorized it" is an empirical claim, and an inlined
/// function cannot be checked. It is also the permanent fallback on any CPU
/// without AVX2 and the correctness oracle for the SIMD twin.
#[inline(never)]
pub(crate) fn quantize_block_scalar(
    block: &[i16; 64],
    q_block: &mut [i16; 64],
    table: &QuantizationTable,
) {
    for i in 0..64 {
        let z = crate::encode::writer::ZIGZAG[i] as usize & 0x3f;
        q_block[i] = table.quantize_zz(block[z], i);
    }
}

#[cfg(test)]
mod avx2_precondition_tests {
    use crate::encode::quantization::{QuantizationTable, QuantizationTableType};

    /// `_mm256_sign_epi32(q, v)` returns **0** when `v == 0`, whereas the scalar
    /// path returns `((0 + correction) * reciprocal) >> SHIFT` and never negates.
    /// The two agree only if that expression is 0 at every table position.
    ///
    /// If this ever fails, the AVX2 kernel must special-case zero rather than
    /// lean on `sign_epi32` — so it is checked, not assumed.
    #[test]
    fn zero_coefficient_quantizes_to_zero() {
        let types = [
            QuantizationTableType::Default,
            QuantizationTableType::Flat,
            QuantizationTableType::CustomMsSsim,
            QuantizationTableType::CustomPsnrHvs,
            QuantizationTableType::ImageMagick,
            QuantizationTableType::KleinSilversteinCarney,
            QuantizationTableType::DentalXRays,
            QuantizationTableType::VisualDetectionModel,
            QuantizationTableType::ImprovedDetectionModel,
        ];
        for ty in &types {
            for quality in 1u8..=100 {
                for luma in [true, false] {
                    let t = QuantizationTable::new_with_quality(ty, quality, luma);
                    for i in 0..64 {
                        assert_eq!(
                            t.quantize(0, i),
                            0,
                            "{ty:?} quality {quality} luma {luma} index {i}"
                        );
                    }
                }
            }
        }
    }

    /// The AVX2 twin packs i32 -> i16 with a **saturating** pack, while the
    /// scalar path does `product as i16`, which **truncates**. They differ only
    /// if a quantized coefficient can leave i16 range. Bound it here.
    #[test]
    fn quantized_magnitude_stays_in_i16() {
        for quality in [1u8, 50, 100] {
            for luma in [true, false] {
                let t = QuantizationTable::new_with_quality(
                    &QuantizationTableType::Default,
                    quality,
                    luma,
                );
                for i in 0..64 {
                    for v in [i16::MIN, i16::MIN + 1, -1, 0, 1, i16::MAX] {
                        let q = t.quantize(v, i) as i32;
                        assert!(
                            (i16::MIN as i32..=i16::MAX as i32).contains(&q),
                            "quality {quality} i {i} v {v} -> {q}"
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AVX2 quantizer
// ---------------------------------------------------------------------------
//
// Step 0 of codec-vectorize-kernel, answered before a line of this was written:
//
//  1. Redundancy first? Yes — three cheaper hypotheses were tried and refuted
//     (branchless sign fixup, in-place block write, pre-permuted tables). See
//     WHYS.md D4a/D4c/D4d.
//  2. Does it already auto-vectorize? **No.** `quantize_block_scalar` is
//     `#[inline(never)]` precisely so this could be checked, and its disassembly
//     is a 64-iteration scalar loop with **zero packed instructions**.
//  3. Can the blocker be NAMED? **Yes: a gather.** The scalar loop's only
//     permuted access is `movzwl (%rcx,%r10,2)` — `block[ZIGZAG[i]]`. LLVM will
//     not synthesise a 64-element cross-lane permutation, so the whole loop
//     stays scalar on account of one load.
//
// The fix follows from the diagnosis: quantize in **natural order**, where every
// access is sequential and vectorizes, then apply the zig-zag permutation once
// as pure data movement afterwards.

#[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
pub(crate) mod avx2 {
    use super::{QuantizationTable, SHIFT};
    use crate::encode::writer::ZIGZAG;

    /// AVX2 twin of [`quantize_block_scalar`](super::quantize_block_scalar).
    ///
    /// **Bit-identical** to the scalar oracle — this is integer arithmetic doing
    /// the same operations in the same order, so the gate is `assert_eq!`, not a
    /// tolerance. Three facts make that hold, each checked by a test rather than
    /// assumed:
    ///
    /// - `_mm256_sign_epi32(q, v)` yields 0 when `v == 0`, while the scalar path
    ///   computes `((0 + corr) * recip) >> SHIFT`. Those agree because that
    ///   expression is 0 at every position of every shipped table
    ///   (`zero_coefficient_quantizes_to_zero`).
    /// - `_mm256_packs_epi32` **saturates** where `product as i16` truncates.
    ///   They agree because quantized magnitudes stay inside i16
    ///   (`quantized_magnitude_stays_in_i16`).
    /// - `(abs + corr) * recip` cannot overflow i32: `abs <= 32767` and
    ///   `recip = 32768 / divisor <= 4096` (divisors are stored pre-shifted, so
    ///   the smallest is 8), giving at most ~1.34e8.
    ///
    /// # Safety
    /// Caller must have verified AVX2 is available. All accesses are to
    /// fixed-size 64-element arrays at compile-time-known offsets.
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn quantize_block_avx2(
        block: &[i16; 64],
        q_block: &mut [i16; 64],
        table: &QuantizationTable,
    ) {
        #[cfg(target_arch = "x86")]
        use core::arch::x86::*;
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::*;

        // Quantize in natural order: block, reciprocals and corrections are all
        // read sequentially, so there is no gather to block the vector path.
        let mut tmp = [0i16; 64];
        let recip = table.reciprocals.as_ptr();
        let corr = table.corrections.as_ptr();
        let src = block.as_ptr();

        for c in 0..4 {
            let base = c * 16;

            // Two groups of 8 coefficients, widened i16 -> i32.
            let v_lo = _mm256_cvtepi16_epi32(_mm_loadu_si128(src.add(base) as *const __m128i));
            let v_hi = _mm256_cvtepi16_epi32(_mm_loadu_si128(src.add(base + 8) as *const __m128i));

            // ((|v| + correction) * reciprocal) >> SHIFT
            let q_lo = _mm256_srli_epi32::<{ SHIFT as i32 }>(_mm256_mullo_epi32(
                _mm256_add_epi32(
                    _mm256_abs_epi32(v_lo),
                    _mm256_loadu_si256(corr.add(base) as *const __m256i),
                ),
                _mm256_loadu_si256(recip.add(base) as *const __m256i),
            ));
            let q_hi = _mm256_srli_epi32::<{ SHIFT as i32 }>(_mm256_mullo_epi32(
                _mm256_add_epi32(
                    _mm256_abs_epi32(v_hi),
                    _mm256_loadu_si256(corr.add(base + 8) as *const __m256i),
                ),
                _mm256_loadu_si256(recip.add(base + 8) as *const __m256i),
            ));

            // Restore the original sign (and zero where the input was zero).
            let s_lo = _mm256_sign_epi32(q_lo, v_lo);
            let s_hi = _mm256_sign_epi32(q_hi, v_hi);

            // Narrow to i16. `packs` works within 128-bit lanes, so the halves
            // interleave as [lo0..3, hi0..3, lo4..7, hi4..7]; the permute puts
            // the 64-bit groups back into order.
            let packed = _mm256_packs_epi32(s_lo, s_hi);
            let ordered = _mm256_permute4x64_epi64::<0b11_01_10_00>(packed);
            _mm256_storeu_si256(tmp.as_mut_ptr().add(base) as *mut __m256i, ordered);
        }

        // The permutation, once, as pure data movement.
        for i in 0..64 {
            *q_block.get_unchecked_mut(i) =
                *tmp.get_unchecked(*ZIGZAG.get_unchecked(i) as usize & 0x3f);
        }
    }
}

#[cfg(all(
    test,
    feature = "simd",
    any(target_arch = "x86", target_arch = "x86_64")
))]
mod avx2_kernel_tests {
    use super::*;

    /// The scalar oracle gate. Integer kernel, so `assert_eq!` — no tolerance.
    #[test]
    fn avx2_matches_scalar() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("skipping: no AVX2 on this host");
            return;
        }
        let types = [
            QuantizationTableType::Default,
            QuantizationTableType::Flat,
            QuantizationTableType::ImageMagick,
            QuantizationTableType::DentalXRays,
        ];
        // Deterministic PRNG plus deliberate edges: the extremes of i16, the
        // zero that `sign_epi32` treats specially, and +-1 around it.
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as i16
        };
        for ty in &types {
            for quality in [1u8, 10, 50, 90, 100] {
                for luma in [true, false] {
                    let t = QuantizationTable::new_with_quality(ty, quality, luma);
                    for case in 0..64 {
                        let mut block = [0i16; 64];
                        for (i, b) in block.iter_mut().enumerate() {
                            *b = match case {
                                0 => 0,
                                1 => i16::MAX,
                                2 => i16::MIN,
                                3 => {
                                    if i % 2 == 0 {
                                        i16::MAX
                                    } else {
                                        i16::MIN
                                    }
                                }
                                4 => (i as i16) - 32,
                                _ => rnd(),
                            };
                        }
                        let mut want = [0i16; 64];
                        let mut got = [0i16; 64];
                        quantize_block_scalar(&block, &mut want, &t);
                        // SAFETY: AVX2 confirmed available above.
                        unsafe { avx2::quantize_block_avx2(&block, &mut got, &t) };
                        assert_eq!(
                            got, want,
                            "{ty:?} quality {quality} luma {luma} case {case}\nblock {block:?}"
                        );
                    }
                }
            }
        }
    }
}
