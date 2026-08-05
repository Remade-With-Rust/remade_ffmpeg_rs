//! JPEG decoding.
//!
//! Vendored from `jpeg-decoder` 0.3.2 (image-rs, MIT OR Apache-2.0).
//! See `NOTICE.md` for attribution and the list of local changes.

#![deny(unsafe_code)]
#![cfg_attr(feature = "platform_independent", forbid(unsafe_code))]

pub use decoder::{ColorTransform, Decoder, ImageInfo, PixelFormat, PlanarComponent, PlanarImage};
pub use error::{Error, UnsupportedFeature};
pub use parser::CodingProcess;

use std::io;

#[cfg(not(feature = "platform_independent"))]
mod arch;
mod decoder;
mod error;
mod huffman;
pub(crate) mod idct;
mod marker;
mod parser;
mod upsampler;
mod worker;

/// Test-facing view of which worker the last decode instantiated. See
/// [`worker::last_worker_was_immediate`].
#[cfg(feature = "std")]
pub fn last_worker_was_immediate() -> bool {
    worker::last_worker_was_immediate()
}

/// A buffered reader the whole decoder shares.
///
/// The entropy decoder refills its 64-bit accumulator a **byte at a time**, and
/// measured on a 1080p photographic frame that is **260,265 `read_u8` calls per
/// frame** — essentially the entire file, one `read_exact` each, at roughly
/// 50 cycles apiece. [`next_byte`](Buffered::next_byte) turns that into a bounds
/// check and an index.
///
/// It has to live at the *decoder* level, not inside the Huffman decoder: a
/// buffer that reads ahead would swallow bytes the marker and segment parsers
/// need next, and there is no way to push them back into a bare `Read`. Because
/// `Buffered<R>` itself implements `Read`, every existing generic call site
/// (`parse_sof`, `parse_dht`, …) keeps working unchanged and simply shares the
/// same buffer.
pub(crate) struct Buffered<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
}

impl<R: io::Read> Buffered<R> {
    const CAP: usize = 16 * 1024;

    pub(crate) fn new(inner: R) -> Self {
        Buffered {
            inner,
            buf: alloc::vec![0u8; Self::CAP].into_boxed_slice(),
            pos: 0,
            end: 0,
        }
    }

    #[cold]
    fn refill(&mut self) -> io::Result<()> {
        self.pos = 0;
        self.end = 0;
        loop {
            match self.inner.read(&mut self.buf) {
                Ok(n) => {
                    self.end = n;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// One byte, from the buffer when possible. The hot path of entropy decode.
    #[inline]
    pub(crate) fn next_byte(&mut self) -> io::Result<u8> {
        if self.pos == self.end {
            self.refill()?;
            if self.pos == self.end {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Bytes already buffered, for bulk refills. May be short or empty; callers
    /// must fall back to [`next_byte`](Buffered::next_byte).
    #[inline]
    pub(crate) fn buffered(&self) -> &[u8] {
        &self.buf[self.pos..self.end]
    }

    #[inline]
    pub(crate) fn consume(&mut self, n: usize) {
        debug_assert!(self.pos + n <= self.end);
        self.pos += n;
    }
}

impl<R: io::Read> io::Read for Buffered<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.end {
            // Large reads bypass the buffer entirely rather than churn it.
            if out.len() >= self.buf.len() {
                return self.inner.read(out);
            }
            self.refill()?;
            if self.pos == self.end {
                return Ok(0);
            }
        }
        let n = (self.end - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn read_u8<R: io::Read>(reader: &mut R) -> io::Result<u8> {
    let mut buf = [0];
    reader.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn read_u16_from_be<R: io::Read>(reader: &mut R) -> io::Result<u16> {
    let mut buf = [0, 0];
    reader.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}
