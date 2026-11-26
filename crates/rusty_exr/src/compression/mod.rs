//! Chunk compression and decompression.
//!
//! Every codec here works on one chunk (a scanline block or a tile) and
//! produces, or consumes, the chunk's *raw* layout: for each line of the
//! chunk, top to bottom, for each channel in channel-list order that has a
//! sample row on that line, that row's samples, little-endian. That is the
//! layout of an uncompressed chunk on disk.

pub(crate) mod b44;
pub(crate) mod dwa;
pub(crate) mod huffman;
pub(crate) mod piz;
pub(crate) mod pxr24;
pub(crate) mod rle;
pub(crate) mod zip;

use crate::error::{Error, Result};
use crate::header::{Compression, SampleType};

/// One channel's share of a chunk.
#[derive(Debug, Clone)]
pub(crate) struct ChanGeom<'a> {
    pub name: &'a str,
    pub ty: SampleType,
    pub p_linear: bool,
    /// Samples per row inside this chunk.
    pub width: usize,
    /// Sample rows inside this chunk.
    pub height: usize,
    pub x_sampling: i32,
    pub y_sampling: i32,
}

impl ChanGeom<'_> {
    pub fn bytes(&self) -> usize {
        self.width * self.height * self.ty.bytes()
    }
    pub fn row_bytes(&self) -> usize {
        self.width * self.ty.bytes()
    }
}

/// A chunk's geometry.
#[derive(Debug, Clone)]
pub(crate) struct BlockGeom<'a> {
    /// First line (absolute y) of the chunk.
    pub y0: i32,
    /// Lines in the chunk.
    pub lines: usize,
    /// Pixel width of the chunk.
    pub width: usize,
    pub chans: Vec<ChanGeom<'a>>,
    /// Size of the raw layout in bytes.
    pub raw_size: usize,
}

/// `y mod s == 0` for possibly negative `y`.
#[inline]
pub(crate) fn sampled(y: i32, s: i32) -> bool {
    y.rem_euclid(s) == 0
}

/// Number of multiples of `s` in `[a, a + n)`.
pub(crate) fn count_sampled(a: i32, n: usize, s: i32) -> usize {
    if n == 0 {
        return 0;
    }
    let a = i64::from(a);
    let b = a + n as i64 - 1;
    let s = i64::from(s);
    (b.div_euclid(s) - (a - 1).div_euclid(s)) as usize
}

impl BlockGeom<'_> {
    /// Convert a channel-planar buffer (each channel's rows contiguous, in
    /// channel order) to the raw line-interleaved layout.
    pub fn planar_to_raw(&self, planar: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; self.raw_size];
        let mut starts = Vec::with_capacity(self.chans.len());
        let mut s = 0;
        for c in &self.chans {
            starts.push(s);
            s += c.bytes();
        }
        let mut o = 0;
        for line in 0..self.lines {
            let y = self.y0 + line as i32;
            for (ci, c) in self.chans.iter().enumerate() {
                if !sampled(y, c.y_sampling) {
                    continue;
                }
                let n = c.row_bytes();
                out[o..o + n].copy_from_slice(&planar[starts[ci]..starts[ci] + n]);
                starts[ci] += n;
                o += n;
            }
        }
        out
    }

    /// The inverse of [`Self::planar_to_raw`].
    pub fn raw_to_planar(&self, raw: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; self.raw_size];
        let mut starts = Vec::with_capacity(self.chans.len());
        let mut s = 0;
        for c in &self.chans {
            starts.push(s);
            s += c.bytes();
        }
        let mut o = 0;
        for line in 0..self.lines {
            let y = self.y0 + line as i32;
            for (ci, c) in self.chans.iter().enumerate() {
                if !sampled(y, c.y_sampling) {
                    continue;
                }
                let n = c.row_bytes();
                out[starts[ci]..starts[ci] + n].copy_from_slice(&raw[o..o + n]);
                starts[ci] += n;
                o += n;
            }
        }
        out
    }
}

/// Inflate a zlib stream into exactly `out.len()` bytes.
pub(crate) fn inflate_exact(src: &[u8], out: &mut [u8]) -> Result<()> {
    let mut d = flate2::Decompress::new(true);
    let status = d
        .decompress(src, out, flate2::FlushDecompress::Finish)
        .map_err(|e| Error::invalid(format!("zlib data: {e}")))?;
    if status != flate2::Status::StreamEnd || d.total_out() != out.len() as u64 {
        return Err(Error::invalid(format!(
            "zlib data inflates to {} bytes, expected {}",
            d.total_out(),
            out.len()
        )));
    }
    Ok(())
}

/// Deflate with miniz_oxide at `level` (zlib framing).
pub(crate) fn deflate(data: &[u8], level: u8) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, level)
}

/// Decompress one chunk into its raw layout.
pub(crate) fn decompress(
    compression: Compression,
    geom: &BlockGeom<'_>,
    data: &[u8],
    max_chunk_bytes: u64,
) -> Result<Vec<u8>> {
    if geom.raw_size as u64 > max_chunk_bytes {
        return Err(Error::limit(format!("chunk of {} bytes", geom.raw_size)));
    }
    // A chunk no smaller than its raw size was stored uncompressed (writers
    // fall back to raw when compression does not help).
    if data.len() >= geom.raw_size || compression == Compression::None {
        if data.len() != geom.raw_size {
            return Err(Error::invalid(format!(
                "uncompressed chunk of {} bytes, expected {}",
                data.len(),
                geom.raw_size
            )));
        }
        return Ok(data.to_vec());
    }
    match compression {
        Compression::None => unreachable!(),
        Compression::Rle => rle::decompress(data, geom.raw_size),
        Compression::Zips | Compression::Zip => zip::decompress(data, geom.raw_size),
        Compression::Piz => piz::decompress(geom, data),
        Compression::Pxr24 => pxr24::decompress(geom, data),
        Compression::B44 | Compression::B44a => b44::decompress(geom, data),
        Compression::Dwaa | Compression::Dwab => dwa::decompress(geom, data),
    }
}

/// Compress one chunk from its raw layout. Returns the bytes to store
/// (the raw bytes themselves when compression would not make them smaller).
pub(crate) fn compress(
    compression: Compression,
    geom: &BlockGeom<'_>,
    raw: &[u8],
) -> Result<Vec<u8>> {
    debug_assert_eq!(raw.len(), geom.raw_size);
    let packed = match compression {
        Compression::None => return Ok(raw.to_vec()),
        Compression::Rle => rle::compress(raw),
        Compression::Zips | Compression::Zip => zip::compress(raw),
        Compression::Piz => piz::compress(geom, raw),
        Compression::Pxr24 => pxr24::compress(geom, raw),
        c => return Err(Error::unsupported(format!("writing {c:?} compression"))),
    };
    if packed.len() < raw.len() {
        Ok(packed)
    } else {
        Ok(raw.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampled_counts() {
        assert_eq!(count_sampled(0, 4, 2), 2);
        assert_eq!(count_sampled(1, 4, 2), 2);
        assert_eq!(count_sampled(-3, 4, 2), 2);
        assert_eq!(count_sampled(-4, 1, 2), 1);
        assert_eq!(count_sampled(5, 1, 2), 0);
        assert_eq!(count_sampled(0, 0, 3), 0);
        assert_eq!(count_sampled(0, 7, 3), 3);
    }
}
