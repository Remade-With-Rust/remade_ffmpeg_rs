//! MSB-first bit I/O — the order FLAC packs bits.
//!
//! The writer keeps a 64-bit accumulator and drains whole bytes, so a
//! `write_bits(v, n)` costs O(bytes emitted), not O(n) single-bit steps.
//! The reader mirrors it: a 64-bit window refilled 4 bytes at a time.

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------
pub struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    /// Bits currently held in the low end of `acc` (kept < 8 between calls).
    nbits: u32,
}

impl BitWriter {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn with_capacity(bytes: usize) -> Self {
        BitWriter {
            buf: Vec::with_capacity(bytes),
            acc: 0,
            nbits: 0,
        }
    }

    /// Write the low `n` bits of `val`, most-significant bit first. `n <= 56`.
    #[inline]
    pub fn write_bits(&mut self, val: u64, n: u32) {
        debug_assert!(n <= 56);
        let masked = if n >= 64 {
            val
        } else {
            val & ((1u64 << n) - 1)
        };
        self.acc = (self.acc << n) | masked;
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.buf.push((self.acc >> self.nbits) as u8);
        }
    }

    /// Write `val` as an `n`-bit two's-complement signed field. `n <= 56`.
    #[inline]
    pub fn write_signed(&mut self, val: i64, n: u32) {
        self.write_bits(val as u64, n);
    }

    /// Write `q` zero bits — safe for large `q` (a unary Rice quotient).
    #[inline]
    pub fn write_zeros(&mut self, q: u32) {
        let mut q = q;
        // Drain the partial byte, then emit whole zero bytes directly.
        if self.nbits != 0 {
            let take = (8 - self.nbits).min(q);
            self.write_bits(0, take);
            q -= take;
        }
        debug_assert!(self.nbits == 0 || q == 0);
        while q >= 8 {
            self.buf.push(0);
            q -= 8;
        }
        if q > 0 {
            self.write_bits(0, q);
        }
    }

    /// Pad the current partial byte with zero bits so the stream is byte-aligned.
    pub fn align_to_byte(&mut self) {
        if self.nbits != 0 {
            let pad = 8 - self.nbits;
            self.write_bits(0, pad);
        }
        debug_assert_eq!(self.nbits, 0);
    }

    /// The complete bytes written so far. Only meaningful when byte-aligned
    /// (used to CRC a header/frame that has just been aligned).
    pub fn bytes(&self) -> &[u8] {
        debug_assert_eq!(self.nbits, 0, "bytes() called mid-byte");
        &self.buf
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.buf
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------
/// MSB-first bit reader over a byte slice, with a LEFT-ALIGNED 64-bit window
/// refilled a word at a time. Reads past the end return `None` (the decoder
/// surfaces that as a truncated-stream error).
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Next unread byte.
    pos: usize,
    /// The next unread bits, left-aligned (bit 63 is the next bit).
    bitbuf: u64,
    /// Valid bits in `bitbuf`.
    nbits: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            bitbuf: 0,
            nbits: 0,
        }
    }

    /// Byte offset of the first bit not yet consumed (only meaningful when
    /// byte-aligned).
    pub fn byte_pos(&self) -> usize {
        self.pos - (self.nbits as usize) / 8
    }

    pub fn is_byte_aligned(&self) -> bool {
        self.nbits % 8 == 0
    }

    #[inline]
    fn refill(&mut self) {
        if self.pos + 8 <= self.data.len() {
            // Fast path: splice a big-endian word under the valid bits.
            let w = u64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
            self.bitbuf |= w >> self.nbits;
            let take_bytes = (63 - self.nbits) >> 3;
            self.pos += take_bytes as usize;
            self.nbits += take_bytes * 8;
            // Zero the tail below the valid bits — the OR-refill above relies
            // on it (when nbits % 8 != 0 the spliced word overhangs).
            debug_assert!(self.nbits < 64);
            self.bitbuf &= !((1u64 << (64 - self.nbits)) - 1);
        } else {
            while self.nbits <= 56 && self.pos < self.data.len() {
                self.bitbuf |= (self.data[self.pos] as u64) << (56 - self.nbits);
                self.pos += 1;
                self.nbits += 8;
            }
        }
    }

    /// Read `n` bits (n <= 32) MSB-first. `None` past end of stream.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        if self.nbits < n {
            self.refill();
            if self.nbits < n {
                return None;
            }
        }
        if n == 0 {
            return Some(0);
        }
        let v = (self.bitbuf >> (64 - n)) as u32;
        self.bitbuf <<= n;
        self.nbits -= n;
        Some(v)
    }

    /// Read an `n`-bit two's-complement signed value (n <= 32).
    #[inline]
    pub fn read_signed(&mut self, n: u32) -> Option<i32> {
        let v = self.read_bits(n)?;
        // Sign-extend from bit n-1.
        let shift = 32 - n;
        Some(((v << shift) as i32) >> shift)
    }

    /// Read a unary quantity: count of 0 bits before the terminating 1.
    #[inline]
    pub fn read_unary(&mut self) -> Option<u32> {
        let mut q = 0u32;
        loop {
            if self.nbits == 0 {
                self.refill();
                if self.nbits == 0 {
                    return None;
                }
            }
            let window = self.bitbuf;
            let lead = window.leading_zeros();
            if lead >= self.nbits {
                // All valid bits are zero — consume and continue.
                q += self.nbits;
                self.bitbuf = 0;
                self.nbits = 0;
                continue;
            }
            q += lead;
            self.bitbuf <<= lead + 1; // consume the zeros and the 1
            self.nbits -= lead + 1;
            return Some(q);
        }
    }

    /// Skip to the next byte boundary.
    pub fn align_to_byte(&mut self) {
        let drop = self.nbits % 8;
        self.bitbuf <<= drop;
        self.nbits -= drop;
    }

    /// Read one Rice code (unary quotient, then `k` low bits) with a single
    /// refill on the hot path. `k <= 30`.
    #[inline]
    pub fn read_rice(&mut self, k: u32) -> Option<u32> {
        if self.nbits < 48 {
            self.refill();
        }
        let lead = self.bitbuf.leading_zeros();
        // Fast path: the whole code sits in the valid window.
        if lead + 1 + k <= self.nbits {
            let q = lead;
            let consumed = lead + 1;
            let low = if k > 0 {
                ((self.bitbuf << consumed) >> (64 - k)) as u32
            } else {
                0
            };
            self.bitbuf <<= consumed + k;
            self.nbits -= consumed + k;
            return Some(((q as u64) << k) as u32 | low);
        }
        // Slow path (long quotient / end of stream): compose from primitives.
        // The u64 shift tolerates absurd quotients from malformed streams.
        let q = self.read_unary()?;
        let low = if k > 0 { self.read_bits(k)? } else { 0 };
        Some(((q as u64) << k) as u32 | low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_matches_bit_by_bit_reference() {
        // Reference: naive single-bit writer.
        let mut naive: Vec<u8> = Vec::new();
        let mut cur = 0u8;
        let mut nb = 0u8;
        let mut push_bit = |bit: u8, naive: &mut Vec<u8>| {
            cur = (cur << 1) | bit;
            nb += 1;
            if nb == 8 {
                naive.push(cur);
                cur = 0;
                nb = 0;
            }
        };

        let mut bw = BitWriter::new();
        let vals: &[(u64, u32)] = &[
            (0x3FFE, 14),
            (0, 1),
            (5, 3),
            (0xABCD, 16),
            (1, 1),
            (0x12345678, 32),
            (0x7F, 7),
            (0, 2),
        ];
        for &(v, n) in vals {
            bw.write_bits(v, n);
            for i in (0..n).rev() {
                push_bit(((v >> i) & 1) as u8, &mut naive);
            }
        }
        bw.write_zeros(77);
        for _ in 0..77 {
            push_bit(0, &mut naive);
        }
        bw.write_bits(1, 1);
        push_bit(1, &mut naive);

        let got = bw.into_bytes();
        // Pad naive to byte.
        if nb > 0 {
            naive.push(cur << (8 - nb));
        }
        assert_eq!(got, naive);
    }

    #[test]
    fn reader_round_trips_writer() {
        let mut bw = BitWriter::new();
        bw.write_bits(0x3FFE, 14);
        bw.write_signed(-5, 6);
        bw.write_zeros(40);
        bw.write_bits(1, 1);
        bw.write_bits(0x155, 9);
        bw.write_signed(-1, 17);
        let bytes = bw.into_bytes();

        let mut br = BitReader::new(&bytes);
        assert_eq!(br.read_bits(14), Some(0x3FFE));
        assert_eq!(br.read_signed(6), Some(-5));
        assert_eq!(br.read_unary(), Some(40));
        assert_eq!(br.read_bits(9), Some(0x155));
        assert_eq!(br.read_signed(17), Some(-1));
    }

    #[test]
    fn reader_none_past_end() {
        let bytes = [0xFFu8];
        let mut br = BitReader::new(&bytes);
        assert_eq!(br.read_bits(8), Some(0xFF));
        assert_eq!(br.read_bits(1), None);
    }

    #[test]
    fn unary_across_byte_boundaries() {
        let mut bw = BitWriter::new();
        for q in [0u32, 1, 7, 8, 9, 63, 64, 65, 200] {
            bw.write_zeros(q);
            bw.write_bits(1, 1);
        }
        let bytes = bw.into_bytes();
        let mut br = BitReader::new(&bytes);
        for q in [0u32, 1, 7, 8, 9, 63, 64, 65, 200] {
            assert_eq!(br.read_unary(), Some(q), "q={q}");
        }
    }
}
