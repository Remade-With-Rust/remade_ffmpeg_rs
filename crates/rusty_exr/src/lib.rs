//! # rusty_exr
//!
//! A pure-Rust OpenEXR decoder and encoder.
//!
//! **Decode:** single- and multi-part files; scanline and tiled parts with
//! every resolution level (one level, mipmaps, ripmaps); HALF, FLOAT and
//! UINT channels with any names (`diffuse.R`) and x/y sampling; every
//! compression: None, RLE, ZIPS, ZIP, PIZ, PXR24, B44, B44A, DWAA, DWAB.
//! Decoded samples are bit-identical to OpenEXR C++ 3.5. Header attributes
//! come back typed where standard (chromaticities, preview, timecode ...)
//! and as raw bytes otherwise. A chunk-level API ([`Decoder::decode_chunk`])
//! hands out individual decompressed scanline blocks or tiles. Deep parts
//! are reported as [`Error::Unsupported`]; other parts of the same file
//! still decode.
//!
//! **Into your buffers:** [`Decoder::decode_part_into`] decodes straight
//! into caller-owned planes, one [`ChannelSink`] per channel (converting
//! HALF or UINT to `f32` on the way if asked), in one pass;
//! [`Decoder::decode_bands`] hands the same work out as jobs for your own
//! thread pool, and [`Decoder::decode_chunk_into`] does it chunk by chunk.
//!
//! **Encode:** single-part files, scanline or tiled (one resolution level),
//! with every compression (None, RLE, ZIPS, ZIP, PIZ, PXR24, B44, B44A,
//! DWAA, DWAB), HALF/FLOAT/UINT channels, any channel names, and header
//! attributes. Output is byte-identical to the `exr` crate's for the same
//! image and settings, the lossy codecs included. [`encode_ref`] writes the
//! same file from borrowed planes ([`ImageRef`]), without copying them.
//!
//! Hostile input returns an error, never panics, and every allocation is
//! bounded by [`Limits`] before it is made.
//!
//! ```no_run
//! let bytes = std::fs::read("in.exr").unwrap();
//! let image = rusty_exr::decode(&bytes).unwrap();
//! let rgba = image.to_rgba_f32("").unwrap();
//! let out = rusty_exr::encode(&image, &rusty_exr::EncodeOptions::default()).unwrap();
//! # let _ = (rgba, out);
//! ```

#![deny(unsafe_code)]

mod compression;
mod decode;
mod encode;
mod error;
mod half;
mod header;
mod image;
mod luts;
mod simd;

pub use decode::{
    Block, ChannelSink, ChunkPos, ChunkRect, DecodeBand, DecodeScratch, Decoder, PartKind, Threads,
};
pub use encode::{encode, encode_ref, window, Deflate, EncodeOptions};
pub use error::{Error, Limits, Result};
pub use half::{f16_to_f32, f32_to_f16, f32_to_f16_clamped};
pub use header::{
    Attribute, AttributeValue, Box2i, ChannelDesc, Chromaticities, Compression, Header, KeyCode,
    LevelMode, LineOrder, Preview, RoundingMode, SampleType, TileDesc, TimeCode,
};
pub use image::{Channel, ChannelRef, Image, ImageRef, Samples, SamplesRef};

/// Decode the first part of an EXR file (level 0 if tiled).
pub fn decode(data: &[u8]) -> Result<Image> {
    Decoder::new(data)?.decode_part(0)
}

/// Read only the headers of an EXR file (one per part).
pub fn read_headers(data: &[u8]) -> Result<Vec<Header>> {
    let d = Decoder::new(data)?;
    Ok((0..d.part_count())
        .filter_map(|i| d.header(i).cloned())
        .collect())
}
