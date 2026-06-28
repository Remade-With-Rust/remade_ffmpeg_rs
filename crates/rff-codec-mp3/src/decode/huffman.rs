//! Huffman decoding of the quantized spectrum.
//!
//! The `big_values` region is `2 * big_values` lines decoded as **pairs** (x, y)
//! by up to three sub-regions, each using one of the 32 ISO pair-tables (square,
//! side `dim`, with a `linbits` escape on the max value). After it, the `count1`
//! region decodes **quads** (v, w, x, y ∈ {0, ±1}) with one of two tables. The
//! rest of the 576 lines are implicit zeros.
//!
//! Following the AAC decoder's approach, the *logic* here is proven with
//! synthetic tables; the real ISO codebooks are transcribed into [`tables`] and
//! gated by Kraft/prefix-free validation. A table is two parallel arrays —
//! `codes[i]`/`lens[i]` in (x·dim + y) raster order — matched MSB-first.

use crate::bitio::BitReader;
use crate::frame::{BlockType, GranuleSideInfo, GRANULE_LINES};
use crate::header::FrameHeader;
use crate::tables;

use super::codebooks::{PAIR_TABLES, QUAD_A, QUAD_B};

/// A prefix-free codeword book: parallel codeword / bit-length arrays.
pub struct HuffBook {
    codes: &'static [u16],
    lens: &'static [u8],
    max_len: u8,
}

impl HuffBook {
    pub const fn new(codes: &'static [u16], lens: &'static [u8]) -> HuffBook {
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

    /// Decode the next codeword MSB-first, returning its symbol index, or `None`
    /// if the bits match no codeword (corrupt stream).
    pub fn decode_index(&self, r: &mut BitReader) -> Option<usize> {
        if self.codes.is_empty() {
            return Some(0); // the empty book (table 0) codes a constant 0 pair.
        }
        let mut code = 0u32;
        for len in 1..=self.max_len {
            code = (code << 1) | r.read(1);
            for i in 0..self.codes.len() {
                if self.lens[i] == len && self.codes[i] as u32 == code {
                    return Some(i);
                }
            }
        }
        None
    }

    #[cfg(test)]
    pub fn kraft_sum(&self) -> f64 {
        self.lens.iter().map(|&l| 2f64.powi(-(l as i32))).sum()
    }

    #[cfg(test)]
    pub fn is_prefix_free(&self) -> bool {
        for a in 0..self.codes.len() {
            for b in (a + 1)..self.codes.len() {
                let (la, lb) = (self.lens[a], self.lens[b]);
                let (short, long, ls, ll) = if la <= lb {
                    (self.codes[a], self.codes[b], la, lb)
                } else {
                    (self.codes[b], self.codes[a], lb, la)
                };
                if (long as u32 >> (ll - ls)) == short as u32 {
                    return false; // `short` is a prefix of `long`
                }
            }
        }
        true
    }
}

/// A `big_values` pair table: a book plus its square dimension and linbits.
pub struct PairTable {
    pub book: HuffBook,
    pub dim: u8,
    pub linbits: u8,
}

impl PairTable {
    /// Decode one (x, y) pair: Huffman index → coordinates, linbits escape on the
    /// max coordinate, then a sign bit for each non-zero coordinate.
    fn read(&self, r: &mut BitReader) -> (i32, i32) {
        if self.dim == 0 {
            return (0, 0);
        }
        let idx = match self.book.decode_index(r) {
            Some(i) => i,
            None => return (0, 0),
        };
        let mut x = (idx / self.dim as usize) as i32;
        let mut y = (idx % self.dim as usize) as i32;
        let maxc = self.dim as i32 - 1;
        if self.linbits > 0 && x == maxc {
            x += r.read(self.linbits as u32) as i32;
        }
        if x != 0 && r.read(1) == 1 {
            x = -x;
        }
        if self.linbits > 0 && y == maxc {
            y += r.read(self.linbits as u32) as i32;
        }
        if y != 0 && r.read(1) == 1 {
            y = -y;
        }
        (x, y)
    }
}

/// A `count1` quad table: a 16-entry book whose index is the 4 value bits.
pub struct QuadTable {
    pub book: HuffBook,
}

impl QuadTable {
    /// Decode one (v, w, x, y) quad: index bits give magnitudes (0/1), each
    /// non-zero followed by a sign bit.
    fn read(&self, r: &mut BitReader) -> (i32, i32, i32, i32) {
        let idx = self.book.decode_index(r).unwrap_or(0);
        let mut q = [
            ((idx >> 3) & 1) as i32,
            ((idx >> 2) & 1) as i32,
            ((idx >> 1) & 1) as i32,
            (idx & 1) as i32,
        ];
        for v in q.iter_mut() {
            if *v != 0 && r.read(1) == 1 {
                *v = -*v;
            }
        }
        (q[0], q[1], q[2], q[3])
    }
}

/// Big-value region boundaries (line indices) for one granule/channel.
fn region_bounds(gi: &GranuleSideInfo, sfb_long: &[u16; 23], bv2: usize) -> (usize, usize) {
    if gi.window_switching && gi.block_type != BlockType::Long {
        // Two regions: [0, 36) and [36, bv2); region2 is empty.
        (36.min(bv2), bv2)
    } else {
        let i1 = (gi.region0_count as usize + 1).min(22);
        let i2 = (gi.region0_count as usize + gi.region1_count as usize + 2).min(22);
        let r1 = (sfb_long[i1] as usize).min(bv2);
        let r2 = (sfb_long[i2] as usize).min(bv2).max(r1);
        (r1, r2)
    }
}

/// Decode one granule/channel's quantized coefficients from `main` starting at
/// `*bit_pos`, stopping at `part2_3_end`. Returns the 576 integer coefficients
/// and the count of non-zero (decoded) lines — the rzero boundary.
pub fn decode(
    main: &[u8],
    bit_pos: &mut usize,
    part2_3_end: usize,
    header: &FrameHeader,
    gi: &GranuleSideInfo,
) -> ([i32; GRANULE_LINES], usize) {
    let mut r = BitReader::new(main);
    r.seek_bits(*bit_pos);
    let mut out = [0i32; GRANULE_LINES];

    let sfb_long = tables::sfb_long_offsets(header.sample_rate);
    let bv2 = (gi.big_values as usize * 2).min(GRANULE_LINES);
    let (r1, r2) = region_bounds(gi, sfb_long, bv2);

    // big_values: pairs, choosing the table by region.
    let mut i = 0;
    while i + 1 < bv2 && r.bit_pos() < part2_3_end {
        let t = if i < r1 {
            gi.table_select[0]
        } else if i < r2 {
            gi.table_select[1]
        } else {
            gi.table_select[2]
        } as usize;
        let (x, y) = PAIR_TABLES[t.min(PAIR_TABLES.len() - 1)].read(&mut r);
        out[i] = x;
        out[i + 1] = y;
        i += 2;
    }

    // count1: quads until the part2_3 budget is spent.
    let quad = if gi.count1table_select {
        &QUAD_B
    } else {
        &QUAD_A
    };
    while i + 3 < GRANULE_LINES && r.bit_pos() < part2_3_end {
        let (v, w, x, y) = quad.read(&mut r);
        out[i] = v;
        out[i + 1] = w;
        out[i + 2] = x;
        out[i + 3] = y;
        i += 4;
    }

    let nonzero = i.min(GRANULE_LINES);
    // The granule's main-data ends at part2_3_end regardless of overrun/stuffing.
    *bit_pos = part2_3_end;
    (out, nonzero)
}

// The 32 pair-tables + 2 count1 quad-tables are the canonical ISO codebooks,
// generated into `codebooks.rs` and imported above.

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a main-data buffer from a list of (value, bit-length) tokens.
    fn pack(tokens: &[(u32, u32)]) -> Vec<u8> {
        let mut w = crate::bitio::BitWriter::new();
        for &(v, n) in tokens {
            w.write(v, n);
        }
        w.finish()
    }

    #[test]
    fn all_codebooks_complete_and_prefix_free() {
        // Every canonical ISO codebook must be a complete (Kraft = 1) prefix code.
        for (n, t) in PAIR_TABLES.iter().enumerate() {
            if t.dim == 0 {
                continue; // tables 0, 4, 14 are unused/empty
            }
            let k = t.book.kraft_sum();
            assert!((k - 1.0).abs() < 1e-9, "pair table {n}: Kraft = {k}");
            assert!(t.book.is_prefix_free(), "pair table {n} not prefix-free");
        }
        for (name, q) in [("A", &QUAD_A), ("B", &QUAD_B)] {
            let k = q.book.kraft_sum();
            assert!((k - 1.0).abs() < 1e-9, "count1 table {name}: Kraft = {k}");
            assert!(
                q.book.is_prefix_free(),
                "count1 table {name} not prefix-free"
            );
        }
    }

    #[test]
    fn pair_decode_with_signs() {
        // Real table 1: codeword "000" → (1,1); each non-zero gets a sign bit.
        // sign x = 1 (negative), sign y = 0 (positive) → (-1, 1).
        let bits = pack(&[(0b000, 3), (1, 1), (0, 1)]);
        let mut r = BitReader::new(&bits);
        assert_eq!(PAIR_TABLES[1].read(&mut r), (-1, 1));

        // Codeword "1" → (0,0), no sign bits.
        let bits = pack(&[(0b1, 1)]);
        let mut r = BitReader::new(&bits);
        assert_eq!(PAIR_TABLES[1].read(&mut r), (0, 0));
    }

    #[test]
    fn pair_decode_with_linbits() {
        // A synthetic 2×2 table with linbits=4: (1,1) is "000"; x==max(1) so read
        // 4 linbits then sign; same for y.
        static TL: PairTable = PairTable {
            book: HuffBook::new(&[0b1, 0b001, 0b01, 0b000], &[1, 3, 2, 3]),
            dim: 2,
            linbits: 4,
        };
        // (1,1) code "000"; x: +5 via linbits 0b0100=4 → x=1+4=5, sign 0 → +5;
        // y: linbits 0b0001=1 → y=1+1=2, sign 1 → -2.
        let bits = pack(&[(0b000, 3), (0b0100, 4), (0, 1), (0b0001, 4), (1, 1)]);
        let mut r = BitReader::new(&bits);
        assert_eq!(TL.read(&mut r), (5, -2));
    }

    #[test]
    fn quad_decode_bits_and_signs() {
        // count1 table B codes index i as the 4 bits of 15-i. Index 10 = (1,0,1,0)
        // → codeword 0b0101; then v=1 sign1→-1, x=1 sign0→+1.
        let bits = pack(&[(0b0101, 4), (1, 1), (0, 1)]);
        let mut r = BitReader::new(&bits);
        assert_eq!(QUAD_B.read(&mut r), (-1, 0, 1, 0));
    }

    #[test]
    fn region_bounds_long_and_short() {
        use crate::frame::GranuleSideInfo;
        let sfb = tables::sfb_long_offsets(44100);
        // Long: region0_count=7, region1_count=2 → i1=8 (sfb[8]=36), i2=11 (sfb[11]=62).
        let mut gi = GranuleSideInfo {
            big_values: 100,
            region0_count: 7,
            region1_count: 2,
            ..Default::default()
        };
        assert_eq!(region_bounds(&gi, sfb, 200), (36, 62));
        // Short window-switched: fixed (36, bv2).
        gi.window_switching = true;
        gi.block_type = BlockType::Short;
        assert_eq!(region_bounds(&gi, sfb, 200), (36, 200));
    }
}
