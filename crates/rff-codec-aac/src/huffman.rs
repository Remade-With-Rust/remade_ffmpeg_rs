//! Generic canonical-prefix Huffman decoder for the AAC spectral and
//! scalefactor codebooks (ISO 14496-3 §4.A.3).
//!
//! Each codebook is two parallel `'static` arrays — `codes[i]` is the codeword
//! and `lens[i]` its bit length — exactly the form the spec/reference tables
//! ship in. Decoding reads bits MSB-first, accumulating until a `(len, code)`
//! pair matches; the codes are prefix-free, so the first match is the symbol.
//! `decode` returns the array index `i`, which the codebook layer unpacks into
//! spectral coefficients. O(maxlen·count) per codeword — codebooks are small,
//! so it is plenty fast and trivially verifiable.

#![allow(dead_code)]

use rff_core::{Error, Result};

use crate::bits::BitReader;

/// A Huffman codebook: parallel codeword / bit-length tables.
pub struct HuffBook {
    codes: &'static [u32],
    lens: &'static [u8],
    max_len: u8,
}

impl HuffBook {
    pub const fn new(codes: &'static [u32], lens: &'static [u8]) -> HuffBook {
        let mut max = 0u8;
        let mut i = 0;
        while i < lens.len() {
            if lens[i] > max {
                max = lens[i];
            }
            i += 1;
        }
        HuffBook {
            codes,
            lens,
            max_len: max,
        }
    }

    pub fn count(&self) -> usize {
        self.codes.len()
    }

    /// Decode the next codeword, returning its symbol index.
    pub fn decode(&self, r: &mut BitReader) -> Result<u16> {
        let mut code = 0u32;
        for len in 1..=self.max_len {
            code = (code << 1) | r.read_bit()?;
            for i in 0..self.codes.len() {
                if self.lens[i] == len && self.codes[i] == code {
                    return Ok(i as u16);
                }
            }
        }
        Err(Error::invalid("aac: invalid Huffman codeword"))
    }

    /// Kraft sum Σ 2^-len — 1.0 for a complete code, slightly less if incomplete.
    #[cfg(test)]
    pub fn kraft_sum(&self) -> f64 {
        self.lens.iter().map(|&l| 2f64.powi(-(l as i32))).sum()
    }

    /// True if no codeword is a prefix of another.
    #[cfg(test)]
    pub fn is_prefix_free(&self) -> bool {
        for a in 0..self.codes.len() {
            for b in (a + 1)..self.codes.len() {
                let (la, ca) = (self.lens[a], self.codes[a]);
                let (lb, cb) = (self.lens[b], self.codes[b]);
                let (short_l, short_c, long_l, long_c) = if la <= lb {
                    (la, ca, lb, cb)
                } else {
                    (lb, cb, la, ca)
                };
                if long_c >> (long_l - short_l) == short_c {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny prefix-free book: index 0→"0", 1→"10", 2→"110", 3→"111".
    static TEST_CODES: &[u32] = &[0b0, 0b10, 0b110, 0b111];
    static TEST_LENS: &[u8] = &[1, 2, 3, 3];

    #[test]
    fn decodes_prefix_free_sequence() {
        let book = HuffBook::new(TEST_CODES, TEST_LENS);
        // Stream 0 10 110 111 0 → indices 0 1 2 3 0
        // bits: 0 10 110 111 0 → 0101 1011 10.. = 0x5B 0x80
        let mut r = BitReader::new(&[0x5B, 0x80]);
        let got: Vec<u16> = (0..5).map(|_| book.decode(&mut r).unwrap()).collect();
        assert_eq!(got, vec![0, 1, 2, 3, 0]);
    }

    #[test]
    fn structural_helpers() {
        let book = HuffBook::new(TEST_CODES, TEST_LENS);
        assert!((book.kraft_sum() - 1.0).abs() < 1e-12);
        assert!(book.is_prefix_free());
    }
}
