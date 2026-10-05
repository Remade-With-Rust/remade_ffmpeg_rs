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

// Untrusted input: narrowing casts are lint-enforced here (H-17).
#![warn(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::{Error, Result};

use crate::bits::BitReader;

/// A Huffman codebook: parallel codeword / bit-length tables.
pub struct HuffBook {
    codes: &'static [u32],
    lens: &'static [u8],
    max_len: u8,
    /// Two-level lookup, built on first use (see [`Lut`]).
    lut: std::sync::OnceLock<Lut>,
}

/// Two-level decode table. `prim` is indexed by the next `bits` bits: an entry
/// is `sym << 8 | len` for a code that fits, `SUB | offset << 5 | subbits` when
/// longer codes share that prefix (then `sub[offset + next subbits bits]` holds
/// `sym << 8 | len`), or 0 for an invalid prefix.
struct Lut {
    prim: Vec<u32>,
    sub: Vec<u32>,
    bits: u32,
}

/// Marks a primary entry that points into the second-level table.
const SUB: u32 = 1 << 31;

/// Width of the primary lookup (2^11 entries, 8 KiB per book).
const LUT_BITS: u32 = 11;

impl HuffBook {
    pub const fn new(codes: &'static [u32], lens: &'static [u8]) -> Self {
        let mut max = 0u8;
        let mut i = 0;
        while i < lens.len() {
            if lens[i] > max {
                max = lens[i];
            }
            i += 1;
        }
        Self {
            codes,
            lens,
            max_len: max,
            lut: std::sync::OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.codes.len()
    }

    /// The `(codeword, bit-length)` for symbol index `i` — the encoder side.
    pub fn code(&self, i: usize) -> (u32, u8) {
        (self.codes[i], self.lens[i])
    }

    fn build_lut(&self) -> Lut {
        let bits = LUT_BITS.min(u32::from(self.max_len));
        let mut prim = vec![0u32; 1 << bits];
        // Longest code under each primary prefix that overflows the table.
        let mut deepest = vec![0u32; 1 << bits];
        for (i, (&c, &l)) in self.codes.iter().zip(self.lens).enumerate() {
            let l = u32::from(l);
            if l == 0 {
                continue;
            }
            if l <= bits {
                let base = (c << (bits - l)) as usize;
                for e in prim.iter_mut().skip(base).take(1 << (bits - l)) {
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "a symbol index (< 289 entries) fits in u32"
                    )]
                    let v = ((i as u32) << 8) | l;
                    *e = v;
                }
            } else {
                let p = (c >> (l - bits)) as usize;
                deepest[p] = deepest[p].max(l - bits);
            }
        }
        let mut sub = Vec::new();
        for (p, &sb) in deepest.iter().enumerate() {
            if sb > 0 {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "the sub-table holds at most 2^16 entries"
                )]
                let v = SUB | ((sub.len() as u32) << 5) | sb;
                prim[p] = v;
                sub.resize(sub.len() + (1 << sb), 0);
            }
        }
        for (i, (&c, &l)) in self.codes.iter().zip(self.lens).enumerate() {
            let l = u32::from(l);
            if l <= bits {
                continue;
            }
            let e = prim[(c >> (l - bits)) as usize];
            let (off, sb) = (((e & !SUB) >> 5) as usize, e & 31);
            let tail = l - bits; // bits of this code below the primary prefix
            let base = off + ((c & ((1 << tail) - 1)) << (sb - tail)) as usize;
            for x in sub.iter_mut().skip(base).take(1 << (sb - tail)) {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a symbol index (< 289 entries) fits in u32"
                )]
                let v = ((i as u32) << 8) | l;
                *x = v;
            }
        }
        Lut { prim, sub, bits }
    }

    /// Decode the next codeword, returning its symbol index. One table lookup
    /// for codes up to `LUT_BITS` long; the scan below (the original decoder,
    /// kept as the oracle) for the rare longer ones.
    #[inline]
    pub fn decode(&self, r: &mut BitReader) -> Result<u16> {
        let lut = self.lut.get_or_init(|| self.build_lut());
        let mut e = lut.prim[r.peek_bits(lut.bits) as usize];
        if e & SUB != 0 {
            // A long code: index the second level with the bits after the prefix.
            let sb = e & 31;
            let idx = r.peek_bits(lut.bits + sb) & ((1 << sb) - 1);
            e = lut.sub[((e & !SUB) >> 5) as usize + idx as usize];
        }
        if e != 0 {
            r.skip((e & 0xFF) as usize)?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "entries store a symbol index (< 289) above the length byte"
            )]
            let v = (e >> 8) as u16;
            return Ok(v);
        }
        self.decode_scan(r)
    }

    /// The bit-serial scan decoder (test oracle and long-code fallback).
    pub fn decode_scan(&self, r: &mut BitReader) -> Result<u16> {
        let mut code = 0u32;
        for len in 1..=self.max_len {
            code = (code << 1) | r.read_bit()?;
            for i in 0..self.codes.len() {
                if self.lens[i] == len && self.codes[i] == code {
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "a symbol index (< 289 entries) fits in u16"
                    )]
                    let v = i as u16;
                    return Ok(v);
                }
            }
        }
        Err(Error::invalid("aac: invalid Huffman codeword"))
    }

    /// Kraft sum Σ 2^-len — 1.0 for a complete code, slightly less if incomplete.
    #[cfg(test)]
    pub fn kraft_sum(&self) -> f64 {
        self.lens.iter().map(|&l| 2f64.powi(-i32::from(l))).sum()
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
#[allow(clippy::cast_possible_truncation, reason = "test data generators")]
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

    /// The table decoder must agree with the scan on every codeword of every
    /// spectral and scalefactor book, followed by arbitrary bits.
    #[test]
    fn lut_matches_scan_on_every_book() {
        let mut books: Vec<&HuffBook> = vec![&crate::tables::SCALEFACTOR_BOOK];
        for cb in 1..=11u8 {
            books.push(crate::tables::spectral_book(cb));
        }
        for book in books {
            for i in 0..book.count() {
                let (code, len) = book.code(i);
                // codeword, then a tail of alternating bits, MSB-first into bytes.
                let mut bitsv: Vec<u8> = (0..len).rev().map(|b| ((code >> b) & 1) as u8).collect();
                bitsv.extend((0..23).map(|k| u8::from(k % 3 == 0)));
                let mut bytes = vec![0u8; bitsv.len().div_ceil(8) + 1];
                for (k, &b) in bitsv.iter().enumerate() {
                    bytes[k / 8] |= b << (7 - k % 8);
                }
                let mut a = BitReader::new(&bytes);
                let mut b = BitReader::new(&bytes);
                assert_eq!(book.decode(&mut a).unwrap(), i as u16);
                assert_eq!(book.decode_scan(&mut b).unwrap(), i as u16);
                assert_eq!(a.position(), b.position());
            }
        }
    }

    #[test]
    fn structural_helpers() {
        let book = HuffBook::new(TEST_CODES, TEST_LENS);
        assert!((book.kraft_sum() - 1.0).abs() < 1e-12);
        assert!(book.is_prefix_free());
    }
}
