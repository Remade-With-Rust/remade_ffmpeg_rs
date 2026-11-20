//! Bounds-checked big-endian reading and writing. Every read of untrusted
//! profile bytes goes through [`Reader`]; nothing indexes the input directly.

use crate::error::{Error, Result};

/// A cursor over a byte slice whose every read is bounds-checked.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) const fn len(&self) -> usize {
        self.data.len()
    }

    /// A reader over `data[start..start + len]`.
    pub(crate) fn sub(&self, start: usize, len: usize, what: &'static str) -> Result<Reader<'a>> {
        let end = start.checked_add(len).ok_or(Error::Malformed(what))?;
        let data = self.data.get(start..end).ok_or(Error::Malformed(what))?;
        Ok(Reader::new(data))
    }

    pub(crate) fn seek(&mut self, pos: usize, what: &'static str) -> Result<()> {
        if pos > self.data.len() {
            return Err(Error::Malformed(what));
        }
        self.pos = pos;
        Ok(())
    }

    pub(crate) const fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn bytes(&mut self, n: usize, what: &'static str) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Malformed(what))?;
        let s = self.data.get(self.pos..end).ok_or(Error::Malformed(what))?;
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn skip(&mut self, n: usize, what: &'static str) -> Result<()> {
        self.bytes(n, what).map(|_| ())
    }

    pub(crate) fn u8(&mut self, what: &'static str) -> Result<u8> {
        Ok(self.bytes(1, what)?[0])
    }

    pub(crate) fn u16(&mut self, what: &'static str) -> Result<u16> {
        let b = self.bytes(2, what)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self, what: &'static str) -> Result<u32> {
        let b = self.bytes(4, what)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self, what: &'static str) -> Result<u64> {
        let hi = u64::from(self.u32(what)?);
        let lo = u64::from(self.u32(what)?);
        Ok(hi << 32 | lo)
    }

    pub(crate) fn s15f16(&mut self, what: &'static str) -> Result<f64> {
        let v = self.u32(what)? as i32;
        Ok(f64::from(v) / 65536.0)
    }

    pub(crate) fn sig(&mut self, what: &'static str) -> Result<[u8; 4]> {
        let b = self.bytes(4, what)?;
        Ok([b[0], b[1], b[2], b[3]])
    }

    /// Reads `n` big-endian `u16`s, refusing before allocating if the bytes
    /// are not present (allocation is bounded by the input).
    pub(crate) fn u16_vec(&mut self, n: usize, what: &'static str) -> Result<Vec<u16>> {
        let bytes = self.bytes(n.checked_mul(2).ok_or(Error::Malformed(what))?, what)?;
        Ok(bytes
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect())
    }

    /// Reads `n` bytes as `u16` values scaled from 8 bits (`v * 257`).
    pub(crate) fn u8_as_u16_vec(&mut self, n: usize, what: &'static str) -> Result<Vec<u16>> {
        Ok(self
            .bytes(n, what)?
            .iter()
            .map(|&b| u16::from(b) * 257)
            .collect())
    }
}

/// Big-endian writer.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub(crate) fn s15f16(&mut self, v: f64) {
        self.buf
            .extend_from_slice(&crate::color::to_s15f16(v).to_be_bytes());
    }
    /// Writes `v` as big-endian `u16`s: one length extension, then a loop
    /// with no per-element capacity check.
    pub(crate) fn u16s(&mut self, v: &[u16]) {
        let start = self.buf.len();
        self.buf.resize(start + 2 * v.len(), 0);
        for (d, &x) in self.buf[start..].chunks_exact_mut(2).zip(v) {
            d.copy_from_slice(&x.to_be_bytes());
        }
    }
    /// Writes 8-bit values held as `v * 257` (rounded back to 8 bits).
    pub(crate) fn u8s_from_u16(&mut self, v: &[u16]) {
        let start = self.buf.len();
        self.buf.resize(start + v.len(), 0);
        for (d, &x) in self.buf[start..].iter_mut().zip(v) {
            *d = ((u32::from(x) + 128) / 257) as u8;
        }
    }
    pub(crate) fn sig(&mut self, s: [u8; 4]) {
        self.buf.extend_from_slice(&s);
    }
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    pub(crate) fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }
    pub(crate) fn pad4(&mut self) {
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }
}
