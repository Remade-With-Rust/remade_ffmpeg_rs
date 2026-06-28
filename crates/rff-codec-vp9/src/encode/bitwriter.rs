//! VP9 encoder — the bitstream writer core (Foundation F2/F3).
//!
//! Three writers, each the exact inverse of a [`crate::bits`] reader and gated
//! by a round-trip *through that reader*:
//! * [`BoolEncoder`] — the boolean arithmetic encoder (libvpx `vpx_writer`),
//!   inverse of [`BoolDecoder`](crate::bits::BoolDecoder).
//! * [`BitWriter`] — MSB-first writer for the uncompressed header, inverse of
//!   [`BitReader`](crate::bits::BitReader).
//! * `write_literal` / `write_tree` on `BoolEncoder`, inverse of
//!   `BoolDecoder::literal` / [`read_tree`](crate::token::read_tree).

/// VP9 boolean arithmetic encoder — the exact inverse of
/// [`BoolDecoder`](crate::bits::BoolDecoder) (libvpx `vpx_writer`). A 32-bit
/// `low` accumulator + an 8-bit `range`; bytes are emitted (with carry
/// propagation back over trailing `0xff`s) as the window normalizes. The output
/// decodes bit-exactly through `BoolDecoder`.
pub struct BoolEncoder {
    low: u32,
    range: u32,
    count: i32,
    out: Vec<u8>,
}

impl BoolEncoder {
    pub fn new() -> BoolEncoder {
        let mut e = BoolEncoder {
            low: 0,
            range: 255,
            count: -24,
            out: Vec::new(),
        };
        // Marker bool — the decoder consumes a matching 0 in `BoolDecoder::new`.
        e.write_bool(0, 128);
        e
    }

    /// Encode one boolean `bit` (0/1) at probability `prob` (1..=255 of 256).
    /// Mirrors `BoolDecoder::read_bool` exactly, in reverse.
    pub fn write_bool(&mut self, bit: u32, prob: u8) {
        let range0 = self.range;
        // split = 1 + (((range-1)*prob) >> 8)  ==  decoder's
        // (range*prob + (256-prob)) >> 8.
        let split = 1 + (((range0 - 1) * prob as u32) >> 8);
        let mut low = self.low;
        let mut range = if bit != 0 {
            low = low.wrapping_add(split);
            range0 - split
        } else {
            split
        };
        // Normalize so `range` ends in [128,255]; same shift the decoder applies.
        let mut shift = (range as u8).leading_zeros() as i32;
        range <<= shift;
        let mut count = self.count + shift;
        if count >= 0 {
            let offset = shift - count; // in [1,7] at an emit
            if offset >= 1 && (low << ((offset - 1) as u32)) & 0x8000_0000 != 0 {
                // Carry: ripple +1 back over the trailing 0xff bytes.
                let mut x = self.out.len() as isize - 1;
                while x >= 0 && self.out[x as usize] == 0xff {
                    self.out[x as usize] = 0;
                    x -= 1;
                }
                debug_assert!(x >= 0, "vp9 bool encoder: carry underflow");
                if x >= 0 {
                    self.out[x as usize] += 1;
                }
            }
            self.out.push(((low >> ((24 - offset) as u32)) & 0xff) as u8);
            low <<= offset as u32;
            shift = count;
            low &= 0x00ff_ffff;
            count -= 8;
        }
        low <<= shift as u32;
        self.count = count;
        self.low = low;
        self.range = range;
    }

    /// Encode the low `n` bits of `v` MSB-first, each at prob 128. Inverse of
    /// `BoolDecoder::literal`.
    pub fn write_literal(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.write_bool((v >> i) & 1, 128);
        }
    }

    /// Encode `symbol` through a VP9 token `tree` with node `probs`. Inverse of
    /// [`read_tree`](crate::token::read_tree): walk the path to the leaf
    /// `-symbol`, emitting each branch bit at `probs[node >> 1]`.
    pub fn write_tree(&mut self, tree: &[i8], probs: &[u8], symbol: i32) {
        let mut path: Vec<(usize, u32)> = Vec::new();
        let found = find_tree_path(tree, 0, symbol, &mut path);
        debug_assert!(found, "vp9 write_tree: symbol {symbol} not in tree");
        for (node, bit) in path {
            self.write_bool(bit, probs[node >> 1]);
        }
    }

    /// Flush the final bytes (libvpx `vpx_stop_encode`) and return the buffer.
    pub fn finish(mut self) -> Vec<u8> {
        // 32 zero bits force every buffered bit out of the `low` window.
        for _ in 0..32 {
            self.write_bool(0, 128);
        }
        self.out
    }
}

impl Default for BoolEncoder {
    fn default() -> BoolEncoder {
        BoolEncoder::new()
    }
}

/// Depth-first search for the path from `node` to the leaf encoding `symbol`,
/// recording `(node_index, bit)` for each branch taken. Returns whether the
/// symbol was found (it always is for a well-formed tree).
fn find_tree_path(tree: &[i8], node: usize, symbol: i32, path: &mut Vec<(usize, u32)>) -> bool {
    for bit in 0..2u32 {
        let next = tree[node + bit as usize];
        path.push((node, bit));
        if next <= 0 {
            if -(next as i32) == symbol {
                return true;
            }
        } else if find_tree_path(tree, next as usize, symbol, path) {
            return true;
        }
        path.pop();
    }
    false
}

/// MSB-first bit writer for the VP9 uncompressed header — the inverse of
/// [`BitReader`](crate::bits::BitReader)'s `f` / `s`.
#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    pub fn new() -> BitWriter {
        BitWriter::default()
    }

    /// Number of bits written so far.
    pub fn bit_len(&self) -> usize {
        self.nbits
    }

    /// Write one bit MSB-first (`f(1)`).
    pub fn put_bit(&mut self, bit: u32) {
        if self.nbits % 8 == 0 {
            self.out.push(0);
        }
        if bit & 1 != 0 {
            let byte = self.nbits / 8;
            self.out[byte] |= 1 << (7 - (self.nbits % 8));
        }
        self.nbits += 1;
    }

    /// Write the low `n` bits of `v` MSB-first (`f(n)`).
    pub fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.put_bit((v >> i) & 1);
        }
    }

    /// Write a signed value: `n` magnitude bits then a sign bit (`s(n)`).
    pub fn put_signed(&mut self, v: i32, n: u32) {
        self.put(v.unsigned_abs(), n);
        self.put_bit((v < 0) as u32);
    }

    /// Consume the writer, returning the byte buffer (zero-padded to a byte).
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::{BitReader, BoolDecoder};

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    #[test]
    fn bool_roundtrip_random_streams() {
        let mut s = 0x1234_5678_9abc_def0u64;
        for _ in 0..300 {
            let n = 50 + (xs(&mut s) % 500) as usize;
            let pairs: Vec<(u32, u8)> = (0..n)
                .map(|_| ((xs(&mut s) & 1) as u32, (1 + xs(&mut s) % 255) as u8))
                .collect();
            let mut enc = BoolEncoder::new();
            for &(b, p) in &pairs {
                enc.write_bool(b, p);
            }
            let bytes = enc.finish();
            let mut dec = BoolDecoder::new(&bytes).unwrap();
            for (i, &(b, p)) in pairs.iter().enumerate() {
                assert_eq!(dec.read_bool(p), b, "mismatch at bool {i}");
            }
        }
    }

    #[test]
    fn bool_roundtrip_every_prob() {
        // Each probability 1..=255 with a long alternating bit run.
        for prob in 1u8..=255 {
            let bits: Vec<u32> = (0..96).map(|i| (i & 1) as u32).collect();
            let mut enc = BoolEncoder::new();
            for &b in &bits {
                enc.write_bool(b, prob);
            }
            let bytes = enc.finish();
            let mut dec = BoolDecoder::new(&bytes).unwrap();
            for &b in &bits {
                assert_eq!(dec.read_bool(prob), b, "prob {prob}");
            }
        }
    }

    #[test]
    fn write_literal_roundtrips() {
        let mut s = 0xdead_beef_0bad_f00du64;
        let mut enc = BoolEncoder::new();
        let mut vals = Vec::new();
        for _ in 0..1000 {
            let n = 1 + (xs(&mut s) % 16) as u32;
            let v = (xs(&mut s) as u32) & ((1u32 << n) - 1);
            enc.write_literal(v, n);
            vals.push((v, n));
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes).unwrap();
        for (v, n) in vals {
            assert_eq!(dec.literal(n), v);
        }
    }

    #[test]
    fn write_tree_roundtrips_partition_shape() {
        // PARTITION_TREE: NONE=0, HORZ=1, VERT=2, SPLIT=3.
        const PARTITION_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
        let probs = [100u8, 60, 200];
        let mut s = 0x99_44_22_11u64;
        let mut enc = BoolEncoder::new();
        let mut syms = Vec::new();
        for _ in 0..2000 {
            let sym = (xs(&mut s) % 4) as i32;
            enc.write_tree(&PARTITION_TREE, &probs, sym);
            syms.push(sym);
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes).unwrap();
        for sym in syms {
            assert_eq!(crate::token::read_tree(&mut dec, &PARTITION_TREE, &probs), sym);
        }
    }

    #[test]
    fn bitwriter_roundtrips_through_bitreader() {
        let mut s = 0x5a5a_a5a5u64;
        let mut w = BitWriter::new();
        let mut fields: Vec<(u32, u32)> = Vec::new();
        for _ in 0..500 {
            let n = 1 + (xs(&mut s) % 24) as u32;
            let v = (xs(&mut s) as u32) & ((1u64 << n) - 1) as u32;
            w.put(v, n);
            fields.push((v, n));
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for (v, n) in fields {
            assert_eq!(r.f(n).unwrap(), v);
        }
    }

    #[test]
    fn bitwriter_signed_roundtrips() {
        let mut w = BitWriter::new();
        let cases = [0i32, 1, -1, 7, -7, 31, -31, 100, -100];
        for &v in &cases {
            w.put_signed(v, 8);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for &v in &cases {
            assert_eq!(r.s(8).unwrap(), v);
        }
    }
}
