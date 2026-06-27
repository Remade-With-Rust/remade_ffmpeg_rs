//! MSB-first bit reader for AAC bitstreams.

use rff_core::{Error, Result};

/// Reads bits most-significant-first from a byte slice (the order AAC uses).
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    /// Total bits remaining.
    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// Read a single bit.
    pub fn read_bit(&mut self) -> Result<u32> {
        let byte = self.pos / 8;
        if byte >= self.data.len() {
            return Err(Error::invalid("aac: bit reader past end of data"));
        }
        let shift = 7 - (self.pos % 8);
        let bit = (self.data[byte] >> shift) & 1;
        self.pos += 1;
        Ok(bit as u32)
    }

    /// Read `n` bits (0..=32) into a `u32`, MSB-first.
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::invalid("aac: read_bits > 32"));
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()?;
        }
        Ok(v)
    }

    /// Read one bit as a bool.
    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_bit()? != 0)
    }

    /// Skip `n` bits.
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

    #[test]
    fn errors_past_end() {
        let mut r = BitReader::new(&[0x00]);
        assert!(r.read_bits(8).is_ok());
        assert!(r.read_bit().is_err());
    }
}
