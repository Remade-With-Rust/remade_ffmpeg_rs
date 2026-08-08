//! MSB-first bit I/O over the main-data byte stream.
//!
//! MP3 main data (scalefactors + Huffman symbols) is a tight bitstream read
//! most-significant-bit-first, decoupled from frame boundaries by the bit
//! reservoir — so the reader operates over a reassembled buffer, not a raw frame.

/// Most-significant-bit-first reader. Tracks a bit position so the Huffman and
/// scalefactor stages can be byte-misaligned freely.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    /// Current bit position (used to enforce `part2_3_length` boundaries).
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// Seek to an absolute bit position (Huffman decode stops at the granule's
    /// `part2_3_length`; the next granule resumes from there).
    pub fn seek_bits(&mut self, bit: usize) {
        self.pos = bit;
    }

    /// Read `n` bits (0..=32) MSB-first as an unsigned integer.
    pub fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    /// Look at the next `n` bits (0..=32) MSB-first without consuming them, zero-
    /// padding past the end of the buffer (matching [`read`](Self::read)). The Huffman LUT
    /// peeks `max_len` bits, then [`skip`](Self::skip)s the matched codeword.
    ///
    /// Reads those bits in O(1) by loading eight bytes big-endian and shifting,
    /// instead of looping once per bit.
    ///
    /// Eight bytes always suffice: the field starts at a bit offset of 0..=7 and
    /// `n <= 32`, so `7 + 32 = 39` bits are always inside the 64-bit window. Past
    /// the end of `data` the window zero-fills, matching the old loop's
    /// `unwrap_or(0)`. Byte-for-byte the same bits — `peek_matches_bitwise_reference`
    /// pins it against the original implementation.
    pub fn peek(&self, n: u32) -> u32 {
        if n == 0 {
            return 0; // `>> 64` would overflow; the old loop returned 0 here.
        }
        let byte_idx = self.pos >> 3;
        let bit_off = (self.pos & 7) as u32;
        let word = match self.data.get(byte_idx..byte_idx + 8) {
            // Fast path: eight bytes are in bounds, one unaligned load.
            Some(w) => u64::from_be_bytes(w.try_into().unwrap()),
            // Tail: zero-fill past the end, as the bitwise loop did. `byte_idx`
            // may be entirely past `data`, so clamp the start before slicing —
            // `data[byte_idx..byte_idx]` still panics when byte_idx > len.
            None => {
                let start = byte_idx.min(self.data.len());
                let take = (self.data.len() - start).min(8);
                let mut buf = [0u8; 8];
                buf[..take].copy_from_slice(&self.data[start..start + take]);
                u64::from_be_bytes(buf)
            }
        };
        ((word << bit_off) >> (64 - n)) as u32
    }

    /// Advance the bit cursor by `n` bits (after a [`peek`](Self::peek)).
    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    /// Read a single bit as a bool.
    pub fn read_bool(&mut self) -> bool {
        self.read(1) != 0
    }
}

/// MSB-first writer — the encoder's main-data side. Accumulates bits and flushes
/// to bytes; the bitstream formatter pairs it with reservoir bookkeeping.
#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits filled in the in-progress final byte (0..8).
    nbits: u8,
    cur: u8,
}

impl BitWriter {
    pub fn new() -> BitWriter {
        BitWriter::default()
    }

    /// Append the low `n` bits of `v`, MSB-first.
    pub fn write(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            let bit = ((v >> i) & 1) as u8;
            self.cur = (self.cur << 1) | bit;
            self.nbits += 1;
            if self.nbits == 8 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }

    /// Bits written so far.
    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + self.nbits as usize
    }

    /// Flush the final partial byte (zero-padded) and return the buffer.
    pub fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.cur <<= 8 - self.nbits;
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

#[cfg(test)]
mod peek_tests {
    use super::*;

    /// The original bit-at-a-time implementation, kept forever as the oracle the
    /// word-load `peek` is gated against (codec-optimize: the slow version stays
    /// in the tree as the correctness reference).
    fn peek_bitwise(data: &[u8], pos: usize, n: u32) -> u32 {
        let mut v = 0u32;
        let mut p = pos;
        for _ in 0..n {
            let byte = data.get(p >> 3).copied().unwrap_or(0);
            let bit = (byte >> (7 - (p & 7))) & 1;
            v = (v << 1) | bit as u32;
            p += 1;
        }
        v
    }

    /// Every bit offset x every width, including n=0, widths up to 32, and
    /// positions that run off the end of the buffer (where both must zero-fill).
    #[test]
    fn peek_matches_bitwise_reference() {
        let mut s = 0x2545_F491u32;
        let data: Vec<u8> = (0..64)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 24) as u8
            })
            .collect();
        // Walk past the end so the zero-fill tail path is covered too.
        for pos in 0..(data.len() * 8 + 40) {
            for n in 0..=32u32 {
                let r = BitReader { data: &data, pos };
                assert_eq!(
                    r.peek(n),
                    peek_bitwise(&data, pos, n),
                    "mismatch at pos={pos} n={n}"
                );
            }
        }
    }

    /// An empty buffer must not panic and must read as all-zero.
    #[test]
    fn peek_on_empty_buffer_is_zero() {
        let r = BitReader { data: &[], pos: 0 };
        for n in 0..=32u32 {
            assert_eq!(r.peek(n), 0);
        }
    }
}
