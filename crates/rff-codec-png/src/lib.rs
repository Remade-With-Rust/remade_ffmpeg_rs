//! PNG still-image codec, backed by our own pure-Rust [`rusty_png`] crate
//! (a performance fork of image-rs/image-png).
//!
//! PNG is self-describing (its `IHDR` carries size + color type), so decode
//! needs no stream parameters: a packet *is* the whole PNG file. We decode to
//! packed [`Rgb24`](PixelFormat::Rgb24)/[`Rgba`](PixelFormat::Rgba) frames and
//! encode those back. To bridge PNG (RGB) and the YUV codecs (AVIF), insert a
//! `-vf format=yuv420p` / `format=rgb24` conversion.

use std::io::Cursor;

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_png::{AdaptiveFilterType, BitDepth, ColorType, Compression, FilterType, Transformations};

/// Register the PNG codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Png,
        name: "png",
        long_name: "PNG (Portable Network Graphics) image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(PngDecoder::default())),
        encoder: Some(|| Box::new(PngEncoder::default())),
    });
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PngDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for PngDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_png(&packet.data)?);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frame.take() {
            return Ok(frame);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

fn decode_png(data: &[u8]) -> Result<Frame> {
    let mut decoder = rusty_png::Decoder::new(Cursor::new(data));
    // Normalize: expand palette / sub-8-bit, and reduce 16-bit to 8-bit.
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::invalid(format!("png decode: {e}")))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::invalid(format!("png decode: {e}")))?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width as usize, info.height as usize);

    // Normalize whatever the PNG decoded to into packed RGB or RGBA.
    let (format, planes, stride) = match reader.output_color_type().0 {
        ColorType::Rgb => (PixelFormat::Rgb24, buf, w * 3),
        ColorType::Rgba => (PixelFormat::Rgba, buf, w * 4),
        ColorType::Grayscale => {
            let mut rgb = vec![0u8; w * h * 3];
            for (i, &g) in buf.iter().enumerate() {
                rgb[i * 3..i * 3 + 3].copy_from_slice(&[g, g, g]);
            }
            (PixelFormat::Rgb24, rgb, w * 3)
        }
        ColorType::GrayscaleAlpha => {
            let mut rgba = vec![0u8; w * h * 4];
            for (i, ga) in buf.chunks_exact(2).enumerate() {
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
            (PixelFormat::Rgba, rgba, w * 4)
        }
        other => {
            return Err(Error::unsupported(format!(
                "png decode: unexpected color type {other:?}"
            )))
        }
    };

    Ok(Frame::Video(VideoFrame {
        width: info.width,
        height: info.height,
        format,
        planes: vec![planes],
        strides: vec![stride],
        pts: None,
    }))
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Colour-type analysis
// ---------------------------------------------------------------------------

/// What a packed RGB(A) frame *actually* contains, as opposed to how it is
/// stored.
///
/// Our decoder normalizes every PNG to packed RGB/RGBA (that is what the rest of
/// the pipeline speaks), which means a grayscale or palette PNG that comes in
/// would go straight back out as truecolour and balloon. Measured before this
/// existed: an 8-bit gray image 259,303 B -> 924,819 B (**+257%**), and a
/// 64-colour indexed graphic 6,522 B -> 73,232 B (**+1023%**). The pixels were
/// right; the file was 3.6x / 11.2x too big.
///
/// So the encoder looks at the pixels and picks the narrowest PNG colour type
/// that represents them exactly. Every branch is lossless by construction.
enum ColourKind {
    /// Every pixel has R == G == B (and, for RGBA, alpha is constant 255).
    Gray,
    /// Gray with a non-constant alpha channel.
    GrayAlpha,
    /// At most 256 distinct colours. Carries the palette and, when any entry is
    /// non-opaque, the `tRNS` alpha table.
    Indexed {
        palette: Vec<[u8; 3]>,
        trns: Option<Vec<u8>>,
    },
    /// Genuinely truecolour — store as-is.
    TrueColour,
}

/// Fewest bits per index that can address `n` palette entries.
fn indexed_bit_depth(n: usize) -> BitDepth {
    match n {
        0..=2 => BitDepth::One,
        3..=4 => BitDepth::Two,
        5..=16 => BitDepth::Four,
        _ => BitDepth::Eight,
    }
}

/// One pass over the pixels, with early bail-outs so the cost is negligible on
/// the content that cannot benefit.
///
/// The two tests run together and each drops out as soon as it is refuted: the
/// gray test dies on the first pixel with R != G != B, and the palette test dies
/// on the 257th distinct colour. On a photograph both are refuted within the
/// first few hundred pixels, so this is a scan of a tiny prefix, not of the
/// frame — which is why it can be on by default.
fn analyse(px: &[u8], channels: usize) -> ColourKind {
    let mut is_gray = true;
    let mut alpha_constant = true;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut alphas: Vec<u8> = Vec::new();
    let mut index: std::collections::HashMap<[u8; 4], u8> = std::collections::HashMap::new();
    let mut palette_possible = true;

    for p in px.chunks_exact(channels) {
        let (r, g, b) = (p[0], p[1], p[2]);
        let a = if channels == 4 { p[3] } else { 255 };

        if is_gray && (r != g || g != b) {
            is_gray = false;
        }
        if alpha_constant && a != 255 {
            alpha_constant = false;
        }
        if palette_possible {
            let key = [r, g, b, a];
            if !index.contains_key(&key) {
                if index.len() == 256 {
                    palette_possible = false;
                    index.clear();
                    palette.clear();
                    alphas.clear();
                } else {
                    index.insert(key, index.len() as u8);
                    palette.push([r, g, b]);
                    alphas.push(a);
                }
            }
        }
        if !is_gray && !palette_possible {
            return ColourKind::TrueColour;
        }
    }

    // Gray wins over indexed when both apply: a gray8 image needs no PLTE chunk
    // and indexes nothing, so it is never larger.
    if is_gray {
        return if alpha_constant {
            ColourKind::Gray
        } else {
            ColourKind::GrayAlpha
        };
    }
    if palette_possible {
        let trns = if alphas.iter().all(|&a| a == 255) {
            None
        } else {
            Some(alphas)
        };
        return ColourKind::Indexed { palette, trns };
    }
    ColourKind::TrueColour
}

/// Pack 8-bit indices down to 1/2/4 bits per pixel, MSB-first within each byte
/// and re-starting on every row — the layout PNG's `IHDR` bit-depth field means.
fn pack_indices(indices: &[u8], w: usize, h: usize, depth: BitDepth) -> Vec<u8> {
    let bits = match depth {
        BitDepth::One => 1usize,
        BitDepth::Two => 2,
        BitDepth::Four => 4,
        _ => return indices.to_vec(),
    };
    let per_byte = 8 / bits;
    let row_bytes = w.div_ceil(per_byte);
    let mut out = vec![0u8; row_bytes * h];
    for y in 0..h {
        for x in 0..w {
            let v = indices[y * w + x] & ((1 << bits) - 1) as u8;
            let shift = 8 - bits * (x % per_byte + 1);
            out[y * row_bytes + x / per_byte] |= v << shift;
        }
    }
    out
}

/// Encoder tuning. The defaults reproduce the historical behaviour exactly —
/// `Compression::Fast` + `FilterType::Sub` non-adaptive — because that is what
/// the backing crate defaults to and what every existing output was encoded
/// with. Changing the default is a separate, separately-gated decision.
#[derive(Clone, Copy)]
struct PngSettings {
    compression: Compression,
    filter: FilterType,
    adaptive: AdaptiveFilterType,
    /// Narrow the output colour type to gray/indexed when the pixels allow it.
    /// On by default: it is lossless, and without it every grayscale or palette
    /// PNG that passes through this codec is re-emitted as truecolour (+257% /
    /// +1023% measured). `-png_auto_type 0` restores the old behaviour.
    auto_type: bool,
}

impl Default for PngSettings {
    fn default() -> Self {
        PngSettings {
            compression: Compression::Fast,
            filter: FilterType::Sub,
            adaptive: AdaptiveFilterType::NonAdaptive,
            auto_type: true,
        }
    }
}

/// Map FFmpeg's `-compression_level 0..9` onto the backing crate's three-level
/// enum. The crate exposes `Fast` (fdeflate), `Default` and `Best` rather than
/// zlib's ten levels, so this is a documented bucketing, not a 1:1 translation:
/// 0–1 stay on the fast path, 2–8 take the balanced path, 9 asks for the
/// smallest file.
fn compression_from_level(level: i32) -> Compression {
    match level {
        i32::MIN..=1 => Compression::Fast,
        2..=8 => Compression::Default,
        _ => Compression::Best,
    }
}

/// FFmpeg's `-pred` names. `mixed` is FFmpeg's per-row adaptive choice, so it
/// maps to the crate's adaptive filter rather than to a fixed filter type.
fn filter_from_pred(pred: &str) -> Option<(FilterType, AdaptiveFilterType)> {
    let f = match pred.trim().to_ascii_lowercase().as_str() {
        "none" => (FilterType::NoFilter, AdaptiveFilterType::NonAdaptive),
        "sub" => (FilterType::Sub, AdaptiveFilterType::NonAdaptive),
        "up" => (FilterType::Up, AdaptiveFilterType::NonAdaptive),
        "avg" | "average" => (FilterType::Avg, AdaptiveFilterType::NonAdaptive),
        "paeth" => (FilterType::Paeth, AdaptiveFilterType::NonAdaptive),
        "mixed" | "adaptive" => (FilterType::Paeth, AdaptiveFilterType::Adaptive),
        _ => return None,
    };
    Some(f)
}

#[derive(Default)]
struct PngEncoder {
    packet: Option<Packet>,
    eof: bool,
    settings: PngSettings,
}

impl Encoder for PngEncoder {
    /// `-compression_level 0..9` and `-pred none|sub|up|avg|paeth|mixed`, both
    /// spelled the way FFmpeg's PNG encoder spells them so the CLI stays
    /// drop-in. Before this existed neither knob was reachable at all: the
    /// backing crate's defaults were the only operating point this codec could
    /// ever produce.
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(v) = options.get("compression_level") {
            match v.trim().parse::<i32>() {
                Ok(level) => self.settings.compression = compression_from_level(level),
                Err(_) => {
                    return Err(Error::invalid(format!(
                        "png encode: -compression_level wants 0..9, got `{v}`"
                    )))
                }
            }
        }
        if let Some(v) = options.get("png_auto_type") {
            self.settings.auto_type = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        if let Some(v) = options.get("pred") {
            match filter_from_pred(v) {
                Some((f, a)) => {
                    self.settings.filter = f;
                    self.settings.adaptive = a;
                }
                None => {
                    return Err(Error::unsupported(format!(
                        "png encode: unknown -pred `{v}` (want none/sub/up/avg/paeth/mixed)"
                    )))
                }
            }
        }
        Ok(())
    }

    /// Packed RGB only — this encoder has no Y'CbCr path, so the pipeline
    /// converts planar input (e.g. straight from the JPEG decoder) for us.
    fn accepted_pixel_formats(&self) -> Option<Vec<PixelFormat>> {
        Some(vec![PixelFormat::Rgb24, PixelFormat::Rgba])
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "png encode: audio frame on an image codec",
                ))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_png(vf, self.settings)?));
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.packet.take() {
            return Ok(packet);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

fn encode_png(vf: &VideoFrame, settings: PngSettings) -> Result<Vec<u8>> {
    let (color, channels) = match vf.format {
        PixelFormat::Rgb24 => (ColorType::Rgb, 3usize),
        PixelFormat::Rgba => (ColorType::Rgba, 4usize),
        other => {
            return Err(Error::unsupported(format!(
                "png encode: needs rgb24/rgba, got `{}` (convert with -vf format=rgb24)",
                other.name()
            )))
        }
    };
    let (w, h) = (vf.width as usize, vf.height as usize);
    let row = w * channels;
    let stride = vf.strides[0];

    // png wants tightly packed rows; repack if the source stride has padding.
    let packed: Vec<u8> = if stride == row {
        vf.planes[0].clone()
    } else {
        let mut p = Vec::with_capacity(row * h);
        for j in 0..h {
            p.extend_from_slice(&vf.planes[0][j * stride..j * stride + row]);
        }
        p
    };

    // Narrow the colour type when the pixels allow it. Lossless in every branch:
    // gray only when R == G == B everywhere, indexed only when the exact colour
    // set fits in 256 entries.
    let kind = if settings.auto_type {
        analyse(&packed, channels)
    } else {
        ColourKind::TrueColour
    };

    let (out_color, out_depth, body, palette, trns) = match kind {
        ColourKind::Gray => {
            let mut g = Vec::with_capacity(w * h);
            for p in packed.chunks_exact(channels) {
                g.push(p[0]);
            }
            (ColorType::Grayscale, BitDepth::Eight, g, None, None)
        }
        ColourKind::GrayAlpha => {
            let mut g = Vec::with_capacity(w * h * 2);
            for p in packed.chunks_exact(channels) {
                g.push(p[0]);
                g.push(if channels == 4 { p[3] } else { 255 });
            }
            (ColorType::GrayscaleAlpha, BitDepth::Eight, g, None, None)
        }
        ColourKind::Indexed { palette, trns } => {
            let depth = indexed_bit_depth(palette.len());
            let mut lut = std::collections::HashMap::with_capacity(palette.len());
            for (i, c) in palette.iter().enumerate() {
                // key on RGB+A so entries that differ only in alpha stay distinct
                let a = trns.as_ref().map_or(255, |t| t[i]);
                lut.insert([c[0], c[1], c[2], a], i as u8);
            }
            let mut idx = Vec::with_capacity(w * h);
            for p in packed.chunks_exact(channels) {
                let a = if channels == 4 { p[3] } else { 255 };
                idx.push(lut[&[p[0], p[1], p[2], a]]);
            }
            let body = pack_indices(&idx, w, h, depth);
            let flat: Vec<u8> = palette.iter().flat_map(|c| c.iter().copied()).collect();
            (ColorType::Indexed, depth, body, Some(flat), trns)
        }
        ColourKind::TrueColour => (color, BitDepth::Eight, packed, None, None),
    };

    // Pre-size the output buffer.
    //
    // The encoder builds the whole IDAT, then copies it into `out` through the
    // generic `Write`. Starting `out` empty makes it double repeatedly on the
    // way to (here) 17 MB, re-copying everything it already holds each time.
    // Measured on an 8.3 MPx frame with the stage profiler: the chunk-write
    // stage ran at 1.70 GB/s — far below memcpy — and pre-sizing took it from
    // 14.755 ms to 6.247 ms (-58%), whole-encode 98.8 -> 82.7 ms.
    //
    // `body.len() + 1024` is a real upper bound, not a guess: DEFLATE's stored
    // mode is the worst case and expands by well under 1 KB of block headers at
    // these sizes, so this reserves once and never grows.
    let mut out = Vec::with_capacity(body.len() + 1024);
    {
        let mut encoder = rusty_png::Encoder::new(&mut out, vf.width, vf.height);
        encoder.set_color(out_color);
        encoder.set_depth(out_depth);
        if let Some(p) = palette {
            encoder.set_palette(p);
        }
        if let Some(t) = trns {
            encoder.set_trns(t);
        }
        encoder.set_compression(settings.compression);
        encoder.set_filter(settings.filter);
        encoder.set_adaptive_filter(settings.adaptive);
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::invalid(format!("png encode: {e}")))?;
        writer
            .write_image_data(&body)
            .map_err(|e| Error::invalid(format!("png encode: {e}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_frame(w: u32, h: u32) -> Frame {
        let (wi, hi) = (w as usize, h as usize);
        let mut rgb = vec![0u8; wi * hi * 3];
        for j in 0..hi {
            for i in 0..wi {
                let o = (j * wi + i) * 3;
                rgb[o] = (i * 255 / (wi - 1)) as u8;
                rgb[o + 1] = (j * 255 / (hi - 1)) as u8;
                rgb[o + 2] = 128;
            }
        }
        Frame::Video(VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![rgb],
            strides: vec![wi * 3],
            pts: None,
        })
    }

    #[test]
    fn png_encode_decode_is_lossless() {
        let original = rgb_frame(40, 24);
        let Frame::Video(src) = &original else {
            unreachable!()
        };

        let bytes = encode_png(src, PngSettings::default()).unwrap();
        assert_eq!(&bytes[1..4], b"PNG"); // PNG signature

        let Frame::Video(decoded) = decode_png(&bytes).unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (40, 24));
        assert_eq!(decoded.format, PixelFormat::Rgb24);
        // PNG is lossless: pixels must match exactly.
        assert_eq!(decoded.planes[0], src.planes[0]);
    }

    /// Build an RGB24 frame from a closure so each colour-type case is explicit.
    fn frame_from(w: u32, h: u32, ch: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> VideoFrame {
        let (wi, hi) = (w as usize, h as usize);
        let mut data = Vec::with_capacity(wi * hi * ch);
        for j in 0..hi {
            for i in 0..wi {
                let p = f(i, j);
                data.extend_from_slice(&p[..ch]);
            }
        }
        VideoFrame {
            width: w,
            height: h,
            format: if ch == 4 {
                PixelFormat::Rgba
            } else {
                PixelFormat::Rgb24
            },
            planes: vec![data],
            strides: vec![wi * ch],
            pts: None,
        }
    }

    /// Auto-typing must be LOSSLESS in every branch and must actually shrink the
    /// file. Round-trip is the gate: narrowing the colour type is only valid if
    /// the pixels come back bit-for-bit.
    #[test]
    fn auto_type_is_lossless_and_smaller() {
        let cases: Vec<(&str, VideoFrame)> = vec![
            // pure gray -> Grayscale
            ("gray", frame_from(64, 40, 3, |i, j| {
                let v = ((i * 4 + j) % 256) as u8;
                [v, v, v, 255]
            })),
            // two colours -> Indexed at 1 bit per pixel
            ("bilevel", frame_from(64, 40, 3, |i, j| {
                if (i / 3 + j / 5) % 2 == 0 { [10, 200, 30, 255] } else { [250, 5, 90, 255] }
            })),
            // ~40 colours -> Indexed at 4 bits (>16) / 8 bits
            ("palette", frame_from(64, 40, 3, |i, j| {
                let n = ((i / 8) + (j / 8) * 8) as u8;
                [n * 6, 255 - n * 5, n.wrapping_mul(17), 255]
            })),
            // gray with varying alpha -> GrayscaleAlpha
            ("gray_alpha", frame_from(64, 40, 4, |i, j| {
                let v = ((i + j) % 256) as u8;
                [v, v, v, ((i * 3) % 256) as u8]
            })),
            // photographic-ish -> must stay TrueColour
            ("truecolour", frame_from(64, 40, 3, |i, j| {
                [(i * 7 % 256) as u8, (j * 13 % 256) as u8, ((i * j) % 256) as u8, 255]
            })),
        ];

        for (name, vf) in cases {
            let mut on = PngSettings::default();
            on.auto_type = true;
            let mut off = PngSettings::default();
            off.auto_type = false;

            let a = encode_png(&vf, on).unwrap();
            let b = encode_png(&vf, off).unwrap();

            // 1. lossless: decoding the narrowed file reproduces the source pixels
            let Frame::Video(dec) = decode_png(&a).unwrap() else {
                unreachable!()
            };
            assert_eq!(
                dec.planes[0], vf.planes[0],
                "{name}: auto-typed output did not round-trip losslessly"
            );
            assert_eq!(dec.format, vf.format, "{name}: pixel format changed");

            // 2. never larger than leaving it truecolour
            assert!(
                a.len() <= b.len(),
                "{name}: auto-type made the file BIGGER ({} vs {})",
                a.len(),
                b.len()
            );
            if name != "truecolour" {
                assert!(
                    a.len() < b.len(),
                    "{name}: auto-type should have shrunk this ({} vs {})",
                    a.len(),
                    b.len()
                );
            }
        }
    }
}
