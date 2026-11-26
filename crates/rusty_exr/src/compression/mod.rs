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

/// Per-worker reusable decode state: buffers that keep their capacity
/// from chunk to chunk, and one zlib inflater that is reset, not rebuilt.
#[derive(Default)]
pub(crate) struct Scratch {
    pub a: Vec<u8>,
    pub b: Vec<u8>,
    pub w: Vec<u16>,
    pub lut: Vec<u16>,
    pub huf: huffman::HufScratch,
    inflater: Option<flate2::Decompress>,
}

/// Where a decompressed chunk's bytes are, and in which layout.
pub(crate) enum Layout<'s> {
    /// The raw line-interleaved layout.
    Raw(&'s [u8]),
    /// The raw layout split into its even bytes (first half) and odd bytes
    /// (second half): ZIP and RLE before their final interleave.
    Halves(&'s [u8]),
    /// Channel-planar little-endian bytes (each channel's rows contiguous,
    /// channels in list order).
    Planar(&'s [u8]),
    /// Channel-planar 16-bit words (32-bit samples as low, high pairs).
    PlanarU16(&'s [u16]),
}

/// Inflate a zlib stream into `out`, which ends up exactly `n` bytes long.
/// `out`'s capacity is reused and never zero-filled first.
pub(crate) fn inflate_vec(
    inflater: &mut Option<flate2::Decompress>,
    src: &[u8],
    out: &mut Vec<u8>,
    n: usize,
) -> Result<()> {
    let d = match inflater {
        Some(d) => {
            d.reset(true);
            d
        }
        None => inflater.insert(flate2::Decompress::new(true)),
    };
    out.clear();
    out.reserve_exact(n);
    let status = d
        .decompress_vec(src, out, flate2::FlushDecompress::Finish)
        .map_err(|e| Error::invalid(format!("zlib data: {e}")))?;
    if status != flate2::Status::StreamEnd || out.len() != n {
        return Err(Error::invalid(format!(
            "zlib data inflates to {} or more bytes, expected {n}",
            out.len()
        )));
    }
    Ok(())
}

impl Scratch {
    /// Inflate into `self.a`.
    pub fn inflate_a(&mut self, src: &[u8], n: usize) -> Result<()> {
        inflate_vec(&mut self.inflater, src, &mut self.a, n)
    }
    /// Inflate into `self.b`.
    pub fn inflate_b(&mut self, src: &[u8], n: usize) -> Result<()> {
        inflate_vec(&mut self.inflater, src, &mut self.b, n)
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

/// Deflate at level 4 (zlib framing): miniz_oxide (byte-identical to the
/// `exr` crate) or zlib-rs (about twice as fast, smaller output).
pub(crate) fn deflate(data: &[u8], deflater: crate::Deflate) -> Vec<u8> {
    match deflater {
        crate::Deflate::ExrCompatible => miniz_oxide::deflate::compress_to_vec_zlib(data, 4),
        crate::Deflate::Fast => {
            let mut c = flate2::Compress::new(flate2::Compression::new(4), true);
            // zlib's compressBound: one call normally finishes.
            let n = data.len();
            let mut out = Vec::with_capacity(n + (n >> 12) + (n >> 14) + (n >> 25) + 64);
            loop {
                let consumed = usize::try_from(c.total_in()).unwrap_or(n).min(n);
                let status = c
                    .compress_vec(&data[consumed..], &mut out, flate2::FlushCompress::Finish)
                    .expect("in-memory deflate cannot fail");
                if status == flate2::Status::StreamEnd {
                    break out;
                }
                out.reserve(out.capacity().max(4096));
            }
        }
    }
}

/// Decompress one chunk, leaving the result in `data` or `scratch`.
pub(crate) fn decompress_layout<'a>(
    compression: Compression,
    geom: &BlockGeom<'_>,
    data: &'a [u8],
    max_chunk_bytes: u64,
    scratch: &'a mut Scratch,
) -> Result<Layout<'a>> {
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
        return Ok(Layout::Raw(data));
    }
    match compression {
        Compression::None => unreachable!(),
        Compression::Rle => {
            rle::decompress_halves(data, geom.raw_size, scratch)?;
            Ok(Layout::Halves(&scratch.a))
        }
        Compression::Zips | Compression::Zip => {
            zip::decompress_halves(data, geom.raw_size, scratch)?;
            Ok(Layout::Halves(&scratch.a))
        }
        Compression::Piz => {
            piz::decompress(geom, data, scratch)?;
            Ok(Layout::PlanarU16(&scratch.w))
        }
        Compression::Pxr24 => {
            pxr24::decompress(geom, data, scratch)?;
            Ok(Layout::Raw(&scratch.a))
        }
        Compression::B44 | Compression::B44a => {
            scratch.a = b44::decompress(geom, data)?;
            Ok(Layout::Planar(&scratch.a))
        }
        Compression::Dwaa | Compression::Dwab => {
            scratch.a = dwa::decompress(geom, data)?;
            Ok(Layout::Planar(&scratch.a))
        }
    }
}

/// Decompress one chunk into a fresh raw-layout buffer (the chunk API).
pub(crate) fn decompress(
    compression: Compression,
    geom: &BlockGeom<'_>,
    data: &[u8],
    max_chunk_bytes: u64,
) -> Result<Vec<u8>> {
    let mut scratch = Scratch::default();
    Ok(
        match decompress_layout(compression, geom, data, max_chunk_bytes, &mut scratch)? {
            Layout::Raw(r) => r.to_vec(),
            Layout::Halves(h) => {
                let mut out = vec![0u8; h.len()];
                zip::interleave(h, &mut out);
                out
            }
            Layout::Planar(p) => geom.planar_to_raw(p),
            Layout::PlanarU16(w) => {
                let bytes: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
                geom.planar_to_raw(&bytes)
            }
        },
    )
}

/// Compress one chunk from its raw layout. Returns the bytes to store
/// (the raw bytes themselves when compression would not make them smaller).
pub(crate) fn compress(
    compression: Compression,
    geom: &BlockGeom<'_>,
    raw: Vec<u8>,
    deflater: crate::Deflate,
) -> Result<Vec<u8>> {
    debug_assert_eq!(raw.len(), geom.raw_size);
    let packed = match compression {
        Compression::None => return Ok(raw),
        Compression::Rle => rle::compress(&raw),
        Compression::Zips | Compression::Zip => zip::compress(&raw, deflater),
        Compression::Piz => piz::compress(geom, &raw),
        Compression::Pxr24 => pxr24::compress(geom, &raw, deflater),
        c => return Err(Error::unsupported(format!("writing {c:?} compression"))),
    };
    if packed.len() < raw.len() {
        Ok(packed)
    } else {
        Ok(raw)
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
