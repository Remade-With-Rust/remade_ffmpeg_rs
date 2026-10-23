//! Spectral Huffman decode, table-driven down to the signed tuple.
//!
//! The generic path (`HuffBook::decode` → `tuple_table` → `finish_tuple`) costs
//! one lazily-initialised lookup per codeword, a two-level LUT walk, and then a
//! data-dependent branch plus a one-bit read per sign. Here each spectral book
//! gets ONE flat table indexed by the next [`FAST_BITS`] bits whose entry, when
//! the codeword AND its sign bits both fit in the window, is the final signed
//! tuple plus the total bit count: one load, one skip, no per-sign branch.
//!
//! Entries that do not fit (long codewords, or a codeword whose sign bits run
//! past the window) fall back to the generic path, which stays in-tree as the
//! oracle. Both paths produce the same integers bit for bit
//! (`fast_tables_match_generic_decode`), and the dequantiser that follows is
//! unchanged, so the PCM is unchanged.

// Untrusted input: narrowing casts are lint-enforced here (H-17).
#![warn(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::bits::BitReader;
use crate::codebook::{finish_tuple, tuple_table, Codebook, CODEBOOKS};
use crate::tables::spectral_book;
use crate::Result;
use std::sync::OnceLock;

/// Width of the per-book window (2^11 entries x 4 bytes = 8 KiB per book).
pub const FAST_BITS: u32 = 11;

/// Entry kinds (bits 5..6).
const KIND_SLOW: u32 = 0;
const KIND_FULL: u32 = 1 << 5;
const KIND_SYM: u32 = 2 << 5;
const KIND_MASK: u32 = 3 << 5;
/// A FULL book-11 entry holding an escape magnitude (16): escapes follow.
const ESC: u32 = 1 << 7;

/// One book's flat table. Entry layout:
/// * bits 0..5   — bits to consume (codeword + signs for FULL, codeword for SYM);
/// * bits 5..7   — kind: SLOW (generic decode), FULL (values final), SYM (symbol
///   known, signs still to read);
/// * bit 7       — ESC (FULL only);
/// * bits 8..24  — FULL: the tuple, four signed nibbles (dim 4) or two signed
///   bytes (dim 2); SYM: the symbol index.
pub struct FastBook {
    pub lut: Vec<u32>,
}

fn build(cbn: u8) -> FastBook {
    let cb = &CODEBOOKS[cbn as usize];
    let book = spectral_book(cbn);
    let tuples = tuple_table(cbn);
    let n = FAST_BITS;
    let mut lut = vec![KIND_SLOW; 1 << n];
    for (sym, t) in tuples.iter().enumerate() {
        let (code, len) = book.code(sym);
        let len = u32::from(len);
        if len == 0 || len > n {
            continue; // longer codes: the generic decoder (entry stays SLOW)
        }
        let dim = cb.dim as usize;
        let nz = if cb.unsigned {
            t[..dim].iter().map(|&v| u32::from(v != 0)).sum::<u32>()
        } else {
            0
        };
        let base = (code << (n - len)) as usize;
        for suffix in 0..(1u32 << (n - len)) {
            let e = if len + nz <= n {
                // Sign bits sit in the window right after the codeword, one per
                // non-zero magnitude, in order; a 1 makes it negative.
                let mut signs = suffix >> (n - len - nz);
                let mut vals = [0i32; 4];
                for k in (0..dim).rev() {
                    let v = i32::from(t[k]);
                    vals[k] = if cb.unsigned && v != 0 {
                        let neg = signs & 1 != 0;
                        signs >>= 1;
                        if neg {
                            -v
                        } else {
                            v
                        }
                    } else {
                        v
                    };
                }
                let mut e = KIND_FULL | (len + nz);
                if cb.esc
                    && vals[..dim]
                        .iter()
                        .any(|v| v.unsigned_abs() == u32::from(cb.lav))
                {
                    e |= ESC;
                }
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "two's-complement packing of small signed lanes"
                )]
                if dim == 4 {
                    for (k, v) in vals.iter().enumerate() {
                        e |= ((*v as u32) & 0xF) << (8 + 4 * k);
                    }
                } else {
                    for (k, v) in vals[..2].iter().enumerate() {
                        e |= ((*v as u32) & 0xFF) << (8 + 8 * k);
                    }
                }
                e
            } else {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a symbol index (< 289) fits in the 16-bit field"
                )]
                let s = sym as u32;
                KIND_SYM | len | (s << 8)
            };
            lut[base + suffix as usize] = e;
        }
    }
    FastBook { lut }
}

/// The fast table for spectral book `cbn` (1..=11), built on first use.
pub fn fast_book(cbn: u8) -> &'static FastBook {
    static T: [OnceLock<FastBook>; 12] = [const { OnceLock::new() }; 12];
    T[cbn as usize].get_or_init(|| build(cbn))
}

/// The generic decode of one tuple (the oracle path, and the fallback).
#[inline(never)]
#[cold]
fn slow_tuple(cbn: u8, cb: Codebook, r: &mut BitReader, e: u32, out: &mut [i32]) -> Result<()> {
    crate::prof::bump(crate::prof::Counter::SlowTuples, 1);
    let tab = tuple_table(cbn);
    let sym = if e & KIND_MASK == KIND_SYM {
        r.skip((e & 31) as usize)?;
        (e >> 8) as usize
    } else {
        spectral_book(cbn).decode(r)? as usize
    };
    let t = tab
        .get(sym)
        .ok_or_else(|| crate::Error::invalid("aac: bad spectral codeword"))?;
    let mut tuple = [0i32; 4];
    for (o, &v) in tuple.iter_mut().zip(t) {
        *o = i32::from(v);
    }
    finish_tuple(&cb, r, &mut tuple)?;
    out.copy_from_slice(&tuple[..out.len()]);
    Ok(())
}

/// Book 11 after a FULL entry flagged ESC: replace each magnitude-16 value by
/// its escape sequence (in order), keeping the sign.
#[inline(never)]
#[cold]
fn escapes(r: &mut BitReader, out: &mut [i32]) -> Result<()> {
    crate::prof::bump(crate::prof::Counter::EscTuples, 1);
    let mut t = [0i32; 4];
    t[..2].copy_from_slice(out);
    // Signs are already applied, so only the escape half of `finish_tuple` runs.
    let esc_only = Codebook {
        dim: 2,
        unsigned: false,
        lav: 16,
        esc: true,
    };
    finish_tuple(&esc_only, r, &mut t)?;
    out.copy_from_slice(&t[..2]);
    Ok(())
}

/// Decode `out.len() / DIM` tuples of spectral book `cbn` into `out`.
#[inline(always)]
pub fn decode_run<const DIM: usize>(
    cbn: u8,
    fb: &FastBook,
    r: &mut BitReader,
    out: &mut [i32],
) -> Result<()> {
    let cb = &CODEBOOKS[cbn as usize];
    let lut: &[u32; 1 << FAST_BITS] = fb.lut[..].try_into().expect("a full window table");
    for o in out.chunks_exact_mut(DIM) {
        let w = r.peek_bits(FAST_BITS);
        let e = lut[(w & ((1 << FAST_BITS) - 1)) as usize];
        if e & KIND_MASK == KIND_FULL {
            r.skip((e & 31) as usize)?;
            if DIM == 4 {
                for (k, v) in o.iter_mut().enumerate() {
                    #[allow(clippy::cast_possible_wrap, reason = "nibble sign extension")]
                    let x = ((e << (20 - 4 * k)) as i32) >> 28;
                    *v = x;
                }
            } else {
                for (k, v) in o.iter_mut().enumerate() {
                    #[allow(clippy::cast_possible_wrap, reason = "byte sign extension")]
                    let x = ((e << (16 - 8 * k)) as i32) >> 24;
                    *v = x;
                }
                if e & ESC != 0 {
                    escapes(r, o)?;
                }
            }
        } else {
            slow_tuple(cbn, *cb, r, e, o)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every codeword of every book, followed by every sign pattern (and an
    /// escape for book 11), decodes to the same integers and the same bit
    /// position through the fast table and through the generic path.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn fast_tables_match_generic_decode() {
        for cbn in 1..=11u8 {
            let cb = CODEBOOKS[cbn as usize];
            let dim = cb.dim as usize;
            let book = spectral_book(cbn);
            let fb = fast_book(cbn);
            for sym in 0..tuple_table(cbn).len() {
                let (code, len) = book.code(sym);
                for tail in [
                    0u32,
                    0xFFFF_FFFF,
                    0xA5A5_A5A5,
                    0x5A5A_5A5A,
                    0x0F0F_0F0F,
                    0x8000_0001,
                ] {
                    // codeword, then 32 tail bits, then zero padding.
                    let mut bits: Vec<u8> =
                        (0..len).rev().map(|b| ((code >> b) & 1) as u8).collect();
                    bits.extend((0..32).rev().map(|b| ((tail >> b) & 1) as u8));
                    let mut bytes = vec![0u8; bits.len().div_ceil(8) + 8];
                    for (k, &b) in bits.iter().enumerate() {
                        bytes[k / 8] |= b << (7 - k % 8);
                    }
                    let mut a = BitReader::new(&bytes);
                    let mut b = BitReader::new(&bytes);
                    let mut got = [0i32; 4];
                    let ra = if dim == 4 {
                        decode_run::<4>(cbn, fb, &mut a, &mut got[..4])
                    } else {
                        decode_run::<2>(cbn, fb, &mut a, &mut got[..2])
                    };
                    let mut want = [0i32; 4];
                    let rb = crate::codebook::decode_tuple(&cb, book, &mut b, &mut want);
                    assert_eq!(ra.is_ok(), rb.is_ok(), "book {cbn} sym {sym}");
                    assert_eq!(
                        got[..dim],
                        want[..dim],
                        "book {cbn} sym {sym} tail {tail:x}"
                    );
                    assert_eq!(a.position(), b.position(), "book {cbn} sym {sym}");
                }
            }
        }
    }
}
