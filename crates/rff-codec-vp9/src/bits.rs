//! Bitstream readers for VP9.
//!
//! Two readers are needed: a plain MSB-first reader for the *uncompressed*
//! header (`f(n)` / `s(n)` in the spec), and the VP9 **boolean arithmetic
//! decoder** used for the compressed header and tile/residual data.

use rff_core::{Error, Result};

/// MSB-first bit reader for the uncompressed header.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // bit position
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// Read one bit.
    pub fn f1(&mut self) -> Result<u32> {
        let byte = self.pos / 8;
        if byte >= self.data.len() {
            return Err(Error::invalid("vp9: bit reader past end"));
        }
        let bit = (self.data[byte] >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(bit as u32)
    }

    /// Read `n` bits (0..=32), MSB-first (`f(n)`).
    pub fn f(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.f1()?;
        }
        Ok(v)
    }

    /// Read a signed value: `n` magnitude bits then a sign bit (`s(n)`).
    pub fn s(&mut self, n: u32) -> Result<i32> {
        let value = self.f(n)? as i32;
        if self.f1()? == 1 {
            Ok(-value)
        } else {
            Ok(value)
        }
    }
}

/// VP9 boolean arithmetic decoder (ISO/VP9 spec §9.2), structured as libvpx
/// `vpx_reader`: a 64-bit left-justified `value` window with a bulk byte refill,
/// so renormalization is a single shift + a `count` decrement instead of a
/// per-bit loop. Bit-identical output to the spec; just much faster bookkeeping.
pub struct BoolDecoder<'a> {
    data: &'a [u8],
    pos: usize, // next byte to pull from `data`
    value: u64, // left-justified bit window (current symbol in the top bits)
    count: i32, // valid bits buffered below the top byte; refills when < 0
    range: u32,
}

/// Sentinel added to `count` once the input is exhausted, so we stop refilling
/// (libvpx `LOTS_OF_BITS`); trailing reads then return zero bits.
const LOTS_OF_BITS: i32 = 0x4000_0000;

impl<'a> BoolDecoder<'a> {
    /// Initialize over `data` (consumes the marker bool that must be 0).
    pub fn new(data: &'a [u8]) -> Result<BoolDecoder<'a>> {
        if data.is_empty() {
            return Err(Error::invalid("vp9: empty bool decoder input"));
        }
        let mut d = BoolDecoder { data, pos: 0, value: 0, count: -8, range: 255 };
        d.fill();
        let _marker = d.read_bool(128);
        Ok(d)
    }

    /// Approximate read cursor in bits — how far into the buffer the window has
    /// pulled. Used only for the compressed-header-size sanity check (the live
    /// decode never calls it); matches the old per-bit reader's "bits read" at
    /// end-of-stream, where every byte has been loaded.
    pub fn bit_pos(&self) -> usize {
        self.pos.min(self.data.len()) * 8
    }

    /// Refill the window with as many whole bytes as fit (libvpx `vpx_reader_fill`).
    #[inline]
    fn fill(&mut self) {
        let mut shift = 64 - 8 - (self.count + 8); // = 48 - count
        let bytes_left = self.data.len() - self.pos;
        if bytes_left * 8 > 64 {
            // Fast path: one 8-byte big-endian load.
            let bits = (shift & !7) + 8;
            let mut be = [0u8; 8];
            be.copy_from_slice(&self.data[self.pos..self.pos + 8]);
            let nv = u64::from_be_bytes(be) >> (64 - bits);
            self.count += bits;
            self.pos += (bits >> 3) as usize;
            self.value |= nv << (shift & 7);
        } else {
            // Tail path: byte at a time until the window is full or data ends.
            let bits_over = shift + 8 - (bytes_left * 8) as i32;
            let mut loop_end = 0;
            if bits_over >= 0 {
                self.count += LOTS_OF_BITS;
                loop_end = bits_over;
            }
            if bits_over < 0 || bytes_left > 0 {
                while shift >= loop_end {
                    self.count += 8;
                    let byte = self.data.get(self.pos).copied().unwrap_or(0);
                    self.pos += 1;
                    self.value |= (byte as u64) << shift;
                    shift -= 8;
                }
            }
        }
    }

    /// Decode one boolean with probability `prob` (1..=255 of 256).
    #[inline]
    pub fn read_bool(&mut self, prob: u8) -> u32 {
        let split = (self.range * prob as u32 + (256 - prob as u32)) >> 8;
        if self.count < 0 {
            self.fill();
        }
        let bigsplit = (split as u64) << 56;
        let mut range = split;
        let mut value = self.value;
        let bit = if value >= bigsplit {
            range = self.range - split;
            value -= bigsplit;
            1
        } else {
            0
        };
        // Normalize: `range` ends in [128,255]; shift the window up to match.
        let s = (range as u8).leading_zeros();
        self.range = range << s;
        self.value = value << s;
        self.count -= s as i32;
        bit
    }

    /// Decode `n` literal bits (each at probability 128), MSB-first.
    pub fn literal(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bool(128);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitreader_msb_first() {
        // 0xA2 = 1010_0010
        let mut r = BitReader::new(&[0xA2, 0x49]);
        assert_eq!(r.f(2).unwrap(), 0b10); // frame_marker
        assert_eq!(r.f1().unwrap(), 1); // profile_low
        assert_eq!(r.f1().unwrap(), 0); // profile_high
        assert_eq!(r.f(4).unwrap(), 0b0010);
        assert_eq!(r.f(8).unwrap(), 0x49);
    }

    #[test]
    fn bool_decoder_runs() {
        // Mechanics smoke test: a literal read consumes bits without panicking
        // and stays in range. (Exactness is covered by real-stream decode.)
        let mut b = BoolDecoder::new(&[0x80, 0x00, 0xFF, 0x55]).unwrap();
        let _ = b.literal(8);
        let _ = b.read_bool(128);
        assert!(b.range >= 128);
    }
}
