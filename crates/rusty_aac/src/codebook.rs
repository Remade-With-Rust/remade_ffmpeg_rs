//! AAC spectral Huffman codebooks: metadata + the index→coefficient decode
//! (ISO 14496-3 §4.6.3, Tables 4.A.2–4.A.13).
//!
//! Each codebook decodes a Huffman index into a 2- or 4-tuple of quantized
//! spectral coefficients. This module owns the *logic* — unpacking the index
//! into base-`modulo` digits, applying sign bits for the unsigned books, and
//! the codebook-11 escape sequence — all independent of the raw codeword
//! tables. The tables themselves (`HuffBook` per codebook) are fixed ISO data
//! transcribed separately; the decode here is what turns a correct lookup into
//! correct coefficients, and is verified with synthetic codebooks.

#![allow(dead_code)]

use rff_core::{Error, Result};

use crate::bits::BitReader;
use crate::huffman::HuffBook;

/// Properties of one spectral Huffman codebook.
#[derive(Debug, Clone, Copy)]
pub struct Codebook {
    /// Tuple size decoded per codeword: 4 (books 1–4) or 2 (books 5–11).
    pub dim: u8,
    /// `true` → magnitudes only; a sign bit follows each non-zero value.
    pub unsigned: bool,
    /// Largest absolute value the book codes directly (excludes escapes).
    pub lav: u8,
    /// `true` for book 11, which escape-codes values ≥ `lav`.
    pub esc: bool,
}

/// Codebook properties indexed by codebook number 0..=11 (0 = ZERO_HCB).
pub const CODEBOOKS: [Codebook; 12] = [
    Codebook {
        dim: 0,
        unsigned: false,
        lav: 0,
        esc: false,
    }, // 0 ZERO
    Codebook {
        dim: 4,
        unsigned: false,
        lav: 1,
        esc: false,
    }, // 1
    Codebook {
        dim: 4,
        unsigned: false,
        lav: 1,
        esc: false,
    }, // 2
    Codebook {
        dim: 4,
        unsigned: true,
        lav: 2,
        esc: false,
    }, // 3
    Codebook {
        dim: 4,
        unsigned: true,
        lav: 2,
        esc: false,
    }, // 4
    Codebook {
        dim: 2,
        unsigned: false,
        lav: 4,
        esc: false,
    }, // 5
    Codebook {
        dim: 2,
        unsigned: false,
        lav: 4,
        esc: false,
    }, // 6
    Codebook {
        dim: 2,
        unsigned: true,
        lav: 7,
        esc: false,
    }, // 7
    Codebook {
        dim: 2,
        unsigned: true,
        lav: 7,
        esc: false,
    }, // 8
    Codebook {
        dim: 2,
        unsigned: true,
        lav: 12,
        esc: false,
    }, // 9
    Codebook {
        dim: 2,
        unsigned: true,
        lav: 12,
        esc: false,
    }, // 10
    Codebook {
        dim: 2,
        unsigned: true,
        lav: 16,
        esc: true,
    }, // 11
];

/// First codebook number for intensity stereo (14, 15) and the special markers.
pub const ZERO_HCB: u8 = 0;
pub const ESC_HCB: u8 = 11;
pub const NOISE_HCB: u8 = 13;
pub const INTENSITY_HCB2: u8 = 14;
pub const INTENSITY_HCB: u8 = 15;

/// Decode one codeword from `book` for codebook `cb` into `out[..cb.dim]`,
/// returning quantized (signed) coefficients.
pub fn decode_tuple(
    cb: &Codebook,
    book: &HuffBook,
    r: &mut BitReader,
    out: &mut [i32],
) -> Result<()> {
    let idx = book.decode(r)?;
    apply_index(cb, idx, r, out)
}

/// Turn a decoded codebook `idx` into quantized coefficients, reading any sign
/// and escape bits that follow the codeword. Split out so it can be tested
/// directly with explicit indices.
pub fn apply_index(cb: &Codebook, idx: u16, r: &mut BitReader, out: &mut [i32]) -> Result<()> {
    let dim = cb.dim as usize;
    let modulo = if cb.unsigned {
        cb.lav as u32 + 1
    } else {
        2 * cb.lav as u32 + 1
    };

    // Unpack the Huffman index into `dim` base-`modulo` digits (MSB first).
    let mut v = idx as u32;
    let mut digits = [0u32; 4];
    for d in (0..dim).rev() {
        digits[d] = v % modulo;
        v /= modulo;
    }

    // Map digits to coefficients (signed books offset by -lav).
    for i in 0..dim {
        out[i] = if cb.unsigned {
            digits[i] as i32
        } else {
            digits[i] as i32 - cb.lav as i32
        };
    }

    // Unsigned books: one sign bit per non-zero magnitude, in order.
    if cb.unsigned {
        for o in out.iter_mut().take(dim) {
            if *o != 0 && r.read_bool()? {
                *o = -*o;
            }
        }
    }

    // Book 11: any magnitude equal to lav (16) is replaced by an escape value.
    if cb.esc {
        for o in out.iter_mut().take(dim) {
            if o.unsigned_abs() == cb.lav as u32 {
                let mag = read_escape(r)?;
                *o = if *o < 0 { -mag } else { mag };
            }
        }
    }
    Ok(())
}

/// Escape sequence (ISO 14496-3 §4.6.3.3): N leading 1-bits, a 0, then `N+4`
/// bits give `value = 2^(N+4) + word`.
fn read_escape(r: &mut BitReader) -> Result<i32> {
    let mut n = 0u32;
    while r.read_bool()? {
        n += 1;
        if n > 24 {
            return Err(Error::invalid("aac: runaway escape sequence"));
        }
    }
    let bits = n + 4;
    let word = r.read_bits(bits)?;
    Ok(((1u32 << bits) + word) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::spectral_book;

    #[test]
    fn signed_pair_offsets_by_lav() {
        // cb5: dim2, signed, lav4 → modulo 9. index 56 → digits (6,2) → (2,-2).
        let mut r = BitReader::new(&[]); // signed → no extra bits
        let mut out = [0i32; 4];
        apply_index(&CODEBOOKS[5], 56, &mut r, &mut out).unwrap();
        assert_eq!(&out[..2], &[2, -2]);
    }

    #[test]
    fn unsigned_pair_reads_sign_bits() {
        // cb7: dim2, unsigned, lav7 → modulo 8. index 24 → mags (3,0).
        // The non-zero value reads one sign bit (1 → negative).
        let mut r = BitReader::new(&[0x80]);
        let mut out = [0i32; 4];
        apply_index(&CODEBOOKS[7], 24, &mut r, &mut out).unwrap();
        assert_eq!(&out[..2], &[-3, 0]);
    }

    #[test]
    fn book11_applies_escape_to_max_magnitude() {
        // cb11: index 277 → mags (16,5). signs +,+; escape for 16: N=0, word
        // 0011 → 16+3 = 19. bits: 0 0 0 0011 → 0000_0110 = 0x06.
        let mut r = BitReader::new(&[0x06]);
        let mut out = [0i32; 4];
        apply_index(&CODEBOOKS[11], 277, &mut r, &mut out).unwrap();
        assert_eq!(&out[..2], &[19, 5]);
    }

    #[test]
    fn escape_with_leading_ones() {
        // cb11: index 272 → mags (16,0). sign(16)=+; escape: N=2 (110), word
        // 000101 (=5) → 64+5 = 69. bits: 0 110 000101 → 0110_0001 0100_0000.
        let mut r = BitReader::new(&[0x61, 0x40]);
        let mut out = [0i32; 4];
        apply_index(&CODEBOOKS[11], 272, &mut r, &mut out).unwrap();
        assert_eq!(&out[..2], &[69, 0]);
    }

    #[test]
    fn metadata_matches_spec() {
        assert!(CODEBOOKS[1].dim == 4 && !CODEBOOKS[1].unsigned && CODEBOOKS[1].lav == 1);
        assert!(CODEBOOKS[3].unsigned && CODEBOOKS[3].lav == 2);
        assert!(CODEBOOKS[5].dim == 2 && !CODEBOOKS[5].unsigned && CODEBOOKS[5].lav == 4);
        assert!(CODEBOOKS[11].esc && CODEBOOKS[11].lav == 16);
    }

    // End-to-end through the *real* codebook tables: Huffman lookup + unpack.
    #[test]
    fn real_codebook1_decodes_codewords() {
        // cb1 (dim4, signed, lav1). codeword 0 (1 bit) → index 40 → (0,0,0,0).
        let mut r = BitReader::new(&[0x00]);
        let mut out = [9i32; 4];
        decode_tuple(&CODEBOOKS[1], spectral_book(1), &mut r, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 0]);

        // codeword 0x011 (5 bits, 10001) → index 13 → digits (0,1,1,1) → (-1,0,0,0).
        let mut r = BitReader::new(&[0x88]); // 1000_1000
        let mut out = [9i32; 4];
        decode_tuple(&CODEBOOKS[1], spectral_book(1), &mut r, &mut out).unwrap();
        assert_eq!(out, [-1, 0, 0, 0]);
    }
}
