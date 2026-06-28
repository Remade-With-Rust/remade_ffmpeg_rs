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
    /// padding past the end of the buffer (matching [`read`]). The Huffman LUT
    /// peeks `max_len` bits, then [`skip`](Self::skip)s the matched codeword.
    pub fn peek(&self, n: u32) -> u32 {
        let mut v = 0u32;
        let mut p = self.pos;
        for _ in 0..n {
            let byte = self.data.get(p >> 3).copied().unwrap_or(0);
            let bit = (byte >> (7 - (p & 7))) & 1;
            v = (v << 1) | bit as u32;
            p += 1;
        }
        v
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
