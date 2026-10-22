//! 64-bit-buffered LSb-first bit reader for Vorbis packets.
//!
//! Vorbis packs bits least-significant-first within each byte, so a
//! little-endian 64-bit load at the current byte position yields the next 64
//! stream bits in order (bit 0 of the word is the next bit to read). The reader
//! keeps a 64-bit window `buf` holding `cnt` valid bits and refills it with one
//! unaligned load whenever fewer than 32 bits remain — so every Huffman lookup
//! (codewords are at most 32 bits) and every field read up to 32 bits is a peek,
//! a mask and a shift.
//!
//! **End of packet.** Vorbis defines reading past the end of a packet as the
//! end-of-packet condition, which is *normal* inside residue decode (the encoder
//! may truncate) and means "floor unused" inside floor decode. The reader pads
//! the stream with zero bits so lookups never branch on the boundary, and keeps a
//! signed count of the real bits left: a read that consumed padding drives it
//! negative, and the caller checks [`BitReader::eop`] after the read. Values
//! decoded from padding are discarded by every caller.

/// The LSb-first packet bit reader. See the module docs. `Copy`, and no
/// method takes it by address on a cold path, so a hot loop can work on a
/// local copy that LLVM keeps entirely in registers.
#[derive(Clone, Copy)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Next byte of `data` to load into `buf`.
    pos: usize,
    /// Bit window; bit 0 is the next stream bit. Bits at and above `cnt` are
    /// either zero or the correct following stream bits (a refill ORs the same
    /// bytes in again at the same place, which is idempotent).
    buf: u64,
    /// Valid bits in `buf` (real data or zero padding past the end).
    cnt: u32,
    /// Real (non-padding) bits not yet consumed; negative once a read ran past
    /// the end of the packet.
    left: isize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        let mut r = BitReader {
            data,
            pos: 0,
            buf: 0,
            cnt: 0,
            // A slice never exceeds isize::MAX bytes; saturate the *8 regardless.
            left: (data.len() as isize).saturating_mul(8),
        };
        r.refill();
        r
    }

    /// Top the window up to at least 57 valid bits.
    #[inline(always)]
    pub(crate) fn refill(&mut self) {
        if let Some(chunk) = self.data.get(self.pos..self.pos + 8) {
            let mut w = [0u8; 8];
            w.copy_from_slice(chunk);
            let w = u64::from_le_bytes(w);
            // `cnt <= 63` here (callers refill below 32; `new` starts at 0).
            self.buf |= w << self.cnt;
            let bytes = (63 - self.cnt) >> 3;
            self.pos += bytes as usize;
            self.cnt += bytes * 8;
        } else {
            self.refill_tail();
        }
    }

    /// The last few bytes of the packet, then zero padding forever.
    #[inline(always)]
    fn refill_tail(&mut self) {
        (self.pos, self.buf, self.cnt) = refill_tail(self.data, self.pos, self.buf, self.cnt);
    }

    /// The window: bit 0 is the next stream bit; at least 32 bits are valid
    /// after [`Self::ensure32`].
    #[inline(always)]
    pub(crate) fn window(&self) -> u64 {
        self.buf
    }

    /// Guarantee at least 32 valid bits in the window.
    #[inline(always)]
    pub(crate) fn ensure32(&mut self) {
        if self.cnt < 32 {
            self.refill();
        }
    }

    /// Drop `n` (≤ 32, and ≤ the valid count) bits from the window.
    #[inline(always)]
    pub(crate) fn consume(&mut self, n: u32) {
        debug_assert!(n <= self.cnt);
        self.buf >>= n;
        self.cnt -= n;
        self.left -= n as isize;
    }

    /// Read `n` (0..=32) bits as an unsigned value. Past the end of the packet
    /// the missing bits read as zero and [`Self::eop`] turns true.
    #[inline(always)]
    pub(crate) fn read(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        self.ensure32();
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.consume(n);
        v
    }

    /// Read `n` (0..=64) bits as a `u64`.
    pub(crate) fn read_u64(&mut self, n: u32) -> u64 {
        debug_assert!(n <= 64);
        if n <= 32 {
            u64::from(self.read(n))
        } else {
            let lo = u64::from(self.read(32));
            let hi = u64::from(self.read(n - 32));
            lo | (hi << 32)
        }
    }

    #[inline(always)]
    pub(crate) fn read_bool(&mut self) -> bool {
        self.read(1) == 1
    }

    /// True once any read has consumed bits past the end of the packet.
    #[inline(always)]
    pub(crate) fn eop(&self) -> bool {
        self.left < 0
    }

    /// Real bits not yet consumed (0 once past the end).
    pub(crate) fn bits_left(&self) -> usize {
        self.left.max(0) as usize
    }

    /// Bits consumed so far (may exceed the packet length after an overrun).
    pub(crate) fn bits_consumed(&self) -> usize {
        ((self.data.len() as isize).saturating_mul(8) - self.left) as usize
    }
}

/// Cold tail refill, by value (taking the reader by address here would pin
/// every caller's reader in memory).
#[cold]
#[inline(never)]
fn refill_tail(data: &[u8], mut pos: usize, mut buf: u64, mut cnt: u32) -> (usize, u64, u32) {
    while cnt <= 56 {
        let b = data.get(pos).copied().unwrap_or(0);
        buf |= u64::from(b) << cnt;
        if pos < data.len() {
            pos += 1;
        }
        cnt += 8;
    }
    (pos, buf, cnt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vorbis I spec §2.1.6 worked example.
    #[test]
    fn spec_example() {
        let d = [0b1111_1100u8, 0b0100_1000, 0b1100_1110, 0b0000_0110];
        let mut r = BitReader::new(&d);
        assert_eq!(r.read(4), 12);
        assert_eq!(r.read(3), 7);
        assert_eq!(r.read(7), 17);
        assert_eq!(r.read(13), 6969);
        assert!(!r.eop());
        assert_eq!(r.read(5), 0);
        assert!(!r.eop());
        assert_eq!(r.read(1), 0);
        assert!(r.eop());
    }

    /// Long reads across the fast/tail boundary agree with a bit-at-a-time model.
    #[test]
    fn matches_bitwise_model() {
        let data: Vec<u8> = (0..41u32).map(|i| (i * 97 + 13) as u8).collect();
        let bit = |i: usize| -> u32 { u32::from((data[i / 8] >> (i % 8)) & 1) };
        let mut r = BitReader::new(&data);
        let mut p = 0usize;
        let widths = [
            1u32, 7, 32, 0, 13, 31, 5, 32, 9, 17, 3, 24, 32, 11, 2, 29, 32, 6,
        ];
        let mut k = 0;
        while p + 32 <= data.len() * 8 {
            let n = widths[k % widths.len()];
            k += 1;
            let mut want = 0u32;
            for j in 0..n as usize {
                want |= bit(p + j) << j;
            }
            assert_eq!(r.read(n), want, "at bit {p} width {n}");
            p += n as usize;
            assert_eq!(r.bits_consumed(), p);
        }
        let v = r.read_u64(64);
        let _ = v;
        assert!(r.eop() == (p + 64 > data.len() * 8));
    }
}
