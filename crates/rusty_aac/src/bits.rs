//! MSB-first bit reader for AAC bitstreams.

// Untrusted input: narrowing casts are lint-enforced here (H-17).
#![warn(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::{Error, Result};

/// Reads bits most-significant-first from a byte slice (the order AAC uses).
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Total bits remaining.
    #[must_use]
    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// Read a single bit.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidData`] when no bits remain.
    pub fn read_bit(&mut self) -> Result<u32> {
        let byte = self.pos / 8;
        if byte >= self.data.len() {
            return Err(Error::invalid("aac: bit reader past end of data"));
        }
        let shift = 7 - (self.pos % 8);
        let bit = (self.data[byte] >> shift) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    /// Read `n` bits (0..=32) into a `u32`, MSB-first.
    ///
    /// Word-at-a-time: one unaligned big-endian 64-bit window per call instead of
    /// a loop over single bits. A read that would cross the end of the data is an
    /// error, exactly as the bit loop it replaces (kept as `read_bits_slow`, the
    /// test oracle).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidData`] if `n > 32` or fewer than `n` bits remain.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::invalid("aac: read_bits > 32"));
        }
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(Error::invalid("aac: bit reader past end of data"));
        }
        let v = self.peek_window();
        self.pos += n as usize;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a 64-bit window shifted right by 64 - n (n <= 32) fits in u32"
        )]
        let out = (v >> (64 - n)) as u32;
        Ok(out)
    }

    /// The 64 bits starting at the current position, left-aligned; bytes past the
    /// end read as zero.
    #[inline]
    fn peek_window(&self) -> u64 {
        let byte = self.pos / 8;
        // A full window is one constant-length load; only the last seven bytes
        // of the data take the zero-padded copy (a runtime-length copy is a real
        // `memcpy` call, which every Huffman peek used to pay).
        if let Some(w) = self.data.get(byte..byte + 8) {
            // Cannot fail: `get(byte..byte + 8)` returned exactly eight bytes.
            let w: [u8; 8] = w.try_into().expect("an 8-byte slice");
            return u64::from_be_bytes(w) << (self.pos % 8);
        }
        let mut buf = [0u8; 8];
        let end = (byte + 8).min(self.data.len());
        if byte < end {
            buf[..end - byte].copy_from_slice(&self.data[byte..end]);
        }
        u64::from_be_bytes(buf) << (self.pos % 8)
    }

    /// Look at the next `n` (<= 32) bits without consuming them; bits past the end
    /// read as zero (for table-driven Huffman decoding).
    #[inline]
    #[must_use]
    pub fn peek_bits(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a 64-bit window shifted right by 64 - n (n <= 32) fits in u32"
        )]
        let out = (self.peek_window() >> (64 - n)) as u32;
        out
    }

    /// Absolute bit position from the start of the data.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Seek to an absolute bit position (clamped to the end of the data).
    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len() * 8);
    }

    #[cfg(test)]
    fn read_bits_slow(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()?;
        }
        Ok(v)
    }

    /// Read `n` (<= 8) bits as a `u8`: the value fits by construction.
    ///
    /// # Errors
    ///
    /// As [`read_bits`](Self::read_bits).
    #[inline]
    pub fn read_u8(&mut self, n: u32) -> Result<u8> {
        debug_assert!(n <= 8);
        // An `n`-bit read is below 2^n <= 256; the mask makes that local.
        Ok((self.read_bits(n)? & 0xFF) as u8)
    }

    /// Read `n` (<= 16) bits as a `u16`: the value fits by construction.
    ///
    /// # Errors
    ///
    /// As [`read_bits`](Self::read_bits).
    #[inline]
    pub fn read_u16(&mut self, n: u32) -> Result<u16> {
        debug_assert!(n <= 16);
        // An `n`-bit read is below 2^n <= 65536; the mask makes that local.
        Ok((self.read_bits(n)? & 0xFFFF) as u16)
    }

    /// Read `n` (<= 31) bits as a non-negative `i32`: the value fits by
    /// construction.
    ///
    /// # Errors
    ///
    /// As [`read_bits`](Self::read_bits).
    #[inline]
    pub fn read_i32(&mut self, n: u32) -> Result<i32> {
        debug_assert!(n <= 31);
        // An `n`-bit read is below 2^n <= 2^31; the mask makes that local.
        let v = self.read_bits(n)? & 0x7FFF_FFFF;
        #[allow(
            clippy::cast_possible_wrap,
            reason = "masked to 31 bits, so the value is below 2^31"
        )]
        let out = v as i32;
        Ok(out)
    }

    /// Read one bit as a bool.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidData`] when no bits remain.
    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_bit()? != 0)
    }

    /// Skip `n` bits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidData`] if fewer than `n` bits remain; the position is then unchanged.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() * 8 {
            return Err(Error::invalid("aac: skip past end of data"));
        }
        self.pos += n;
        Ok(())
    }

    /// Advance to the next byte boundary.
    pub fn byte_align(&mut self) {
        if self.pos % 8 != 0 {
            self.pos += 8 - (self.pos % 8);
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, reason = "test data generators")]
mod tests {
    use super::*;

    #[test]
    fn reads_bits_msb_first() {
        // 0b1011_0010, 0b1100_0001
        let mut r = BitReader::new(&[0xB2, 0xC1]);
        assert_eq!(r.read_bits(4).unwrap(), 0b1011);
        assert_eq!(r.read_bits(4).unwrap(), 0b0010);
        assert_eq!(r.read_bits(3).unwrap(), 0b110);
        assert_eq!(r.read_bit().unwrap(), 0);
        assert_eq!(r.read_bits(4).unwrap(), 0b0001);
        assert_eq!(r.bits_left(), 0);
    }

    #[test]
    fn skip_and_align() {
        let mut r = BitReader::new(&[0xFF, 0x0F]);
        r.skip(4).unwrap();
        r.byte_align(); // jump to bit 8
        assert_eq!(r.read_bits(4).unwrap(), 0x0);
        assert_eq!(r.read_bits(4).unwrap(), 0xF);
    }

    /// The word-at-a-time reader must agree with the bit loop on every width and
    /// alignment, including reads that touch the last byte.
    #[test]
    fn fast_read_matches_bit_loop() {
        let data: Vec<u8> = (0..37u32)
            .map(|i| (i.wrapping_mul(97) ^ 0x5A) as u8)
            .collect();
        let mut seed = 12345u32;
        let mut a = BitReader::new(&data);
        let mut b = BitReader::new(&data);
        loop {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = (seed >> 27) + 1; // 1..=32
            let (x, y) = (a.read_bits(n), b.read_bits_slow(n));
            match (x, y) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "n={n} pos={}", a.position()),
                (Err(_), Err(_)) => break,
                (x, y) => panic!("disagree at end: {x:?} vs {y:?}"),
            }
        }
    }

    #[test]
    fn errors_past_end() {
        let mut r = BitReader::new(&[0x00]);
        assert!(r.read_bits(8).is_ok());
        assert!(r.read_bit().is_err());
    }
}
