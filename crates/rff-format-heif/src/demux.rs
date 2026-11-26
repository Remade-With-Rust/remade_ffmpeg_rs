//! The rff demuxer: a HEIF file is one decoded picture, handed on as a single
//! raw-video packet (the way `y4m` hands on frames). Composition (grids,
//! transforms, alpha) is a container-level operation in HEIF, so it happens
//! here rather than in a codec.

use std::io::Read;

use rff_core::{CodecId, ColorInfo, ColorRange, Error, Packet, PixelFormat, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Stream};

use crate::image::{Chroma, Image, Plane};
use crate::{rgb, Heif};

/// Register the HEIF/HEIC demuxer into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "heif",
        long_name: "HEIF / HEIC (High Efficiency Image File Format, HEVC)",
        extensions: &["heic", "heif", "hif"],
        demuxer: Some(|input| Box::new(HeifDemuxer::new(input))),
        muxer: None,
        muxer_path: None,
        probe: Some(probe_heif),
        mux_caps: MuxCaps::NONE,
    });
}

/// Sniff a HEIF still-image file by its `ftyp` brands. HEVC image brands
/// (`heic`, `heix`, `heim`, `heis`) score 100; the generic still-image brands
/// (`mif1`, `miaf`) score 95, above the MP4 demuxer's 90 and AVIF's 50 for
/// them. A file with an `avif` brand is left to the AVIF demuxer, and a file
/// with only sequence brands (`msf1`, `hevc`) to the MP4 one.
fn probe_heif(data: &[u8]) -> i32 {
    if data.len() < 12 || &data[4..8] != b"ftyp" {
        return 0;
    }
    let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let end = size.clamp(12, data.len());
    let mut brands = vec![&data[8..12]];
    let mut i = 16;
    while i + 4 <= end {
        brands.push(&data[i..i + 4]);
        i += 4;
    }
    let has = |b: &[u8]| brands.contains(&b);
    if has(b"avif") || has(b"avis") {
        0
    } else if has(b"heic") || has(b"heix") || has(b"heim") || has(b"heis") {
        100
    } else if has(b"mif1") || has(b"miaf") {
        95
    } else {
        0
    }
}

struct HeifDemuxer {
    input: Option<Input>,
    packet: Option<Vec<u8>>,
}

impl HeifDemuxer {
    fn new(input: Input) -> HeifDemuxer {
        HeifDemuxer {
            input: Some(input),
            packet: None,
        }
    }
}

impl Demuxer for HeifDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("heif demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        let heif = Heif::parse(&buf)?;
        let decoded = heif.decode_primary()?;

        let mut stream = Stream::new(0, CodecId::RawVideo);
        stream.width = decoded.image.width;
        stream.height = decoded.image.height;
        stream.time_base = Rational::new(1, 1);
        stream.nb_frames = Some(1);
        if let Some(n) = decoded.colour.nclx {
            stream.color = ColorInfo {
                primaries: n.primaries,
                transfer: n.transfer,
                matrix: n.matrix,
            };
            stream.color_range = if n.full_range {
                ColorRange::Full
            } else {
                ColorRange::Limited
            };
        }

        let (format, data) = if decoded.alpha.is_some() {
            // rff has no YUV-with-alpha layout; RGBA keeps the alpha plane.
            stream.color_range = ColorRange::Full;
            stream.color = ColorInfo::UNSPECIFIED;
            (
                PixelFormat::Rgba,
                rgb::to_rgba8(&decoded.image, decoded.alpha.as_ref(), decoded.colour.nclx),
            )
        } else {
            let img = with_chroma(decoded.image);
            let format = match (img.chroma, img.bit_depth) {
                (Chroma::Yuv420, 8) => PixelFormat::Yuv420p,
                (Chroma::Yuv420, 10) => PixelFormat::Yuv420p10,
                (Chroma::Yuv420, 12) => PixelFormat::Yuv420p12,
                (Chroma::Yuv422, 8) => PixelFormat::Yuv422p,
                (Chroma::Yuv422, 10) => PixelFormat::Yuv422p10,
                (Chroma::Yuv422, 12) => PixelFormat::Yuv422p12,
                (Chroma::Yuv444, 8) => PixelFormat::Yuv444p,
                (Chroma::Yuv444, 10) => PixelFormat::Yuv444p10,
                (Chroma::Yuv444, 12) => PixelFormat::Yuv444p12,
                (c, b) => {
                    return Err(Error::unsupported(format!(
                        "heif demux: no raw layout for {c:?} at {b} bits"
                    )))
                }
            };
            (format, img.to_planar_bytes())
        };
        stream.pixel_format = Some(format);
        self.packet = Some(data);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        match self.packet.take() {
            Some(data) => {
                let mut p = Packet::from_data(0, data);
                p.flags.keyframe = true;
                p.pts = Some(0);
                Ok(p)
            }
            None => Err(Error::Eof),
        }
    }
}

/// rff has no grey layout: give a monochrome image neutral 4:2:0 chroma.
fn with_chroma(mut img: Image) -> Image {
    if img.chroma == Chroma::Monochrome {
        let (cw, ch) = Chroma::Yuv420.chroma_size(img.width, img.height);
        let mid = 1u16 << (img.bit_depth - 1);
        img.planes.push(Plane::filled(cw, ch, mid));
        img.planes.push(Plane::filled(cw, ch, mid));
        img.chroma = Chroma::Yuv420;
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let mut b = major.to_vec();
        b.extend_from_slice(&[0; 4]);
        for c in compat {
            b.extend_from_slice(*c);
        }
        let mut v = ((8 + b.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(&b);
        v
    }

    #[test]
    fn probe_by_brand() {
        assert_eq!(probe_heif(&ftyp(b"heic", &[b"mif1"])), 100);
        assert_eq!(probe_heif(&ftyp(b"mif1", &[b"heic"])), 100);
        assert_eq!(probe_heif(&ftyp(b"mif1", &[])), 95);
        assert_eq!(probe_heif(&ftyp(b"mif1", &[b"avif"])), 0);
        assert_eq!(probe_heif(&ftyp(b"msf1", &[b"hevc"])), 0);
        assert_eq!(probe_heif(b"not a box at all"), 0);
    }
}
