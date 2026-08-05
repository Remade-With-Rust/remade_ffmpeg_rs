//! JPEG (a.k.a. MJPEG) still-image codec.
//!
//! Decode is backed by the pure-Rust [`rusty_jpeg::decode`]; encode by the pure-Rust
//! [`rusty_jpeg::encode`]. Like PNG, JPEG is self-describing, so a packet is the whole
//! file.
//!
//! **Encode takes planar Y'CbCr directly** (`yuv420p`/`yuv422p`/`yuv444p`) —
//! JPEG is a Y'CbCr codec, so a planar frame is already in the right colour
//! space at the right chroma resolution. Routing it through RGB instead cost two
//! colour conversions and a chroma resample round-trip: measured 2x slower and
//! several dB worse. RGB frames (`rgb24`/`rgba`) still work and are converted.
//!
//! **Decode emits planar too**, for ordinary 3-component Y'CbCr: it taps the
//! component planes before the decoder's chroma upsampler and colour conversion
//! run, which measured 1.41x faster than decoding to RGB. Grayscale, CMYK, YCCK
//! and lossless still take the interleaved [`Rgb24`](PixelFormat::Rgb24) path.
//! Encoders that need RGB declare so via `accepted_pixel_formats`, and the
//! pipeline converts for them.
//!
//! JPEG is defined on **full-range** samples. The transcode pipeline converts
//! limited-range sources up front (see `rff::transcode`), so nothing here has to
//! think about range.

use std::io::Cursor;

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder as RffEncoder};
use rff_core::{Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};

/// Default encode quality (1–100) when no `-q` option is given.
const DEFAULT_QUALITY: u8 = 90;

/// Register the JPEG codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Jpeg,
        name: "mjpeg",
        long_name: "JPEG / MJPEG (Motion JPEG) image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(JpegDecoder::default())),
        encoder: Some(|| Box::new(JpegEncoder::default())),
    });
}

#[derive(Default)]
struct JpegDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for JpegDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_jpeg(&packet.data)?);
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

fn decode_jpeg(data: &[u8]) -> Result<Frame> {
    // Planar first: for ordinary 3-component Y'CbCr this skips the decoder's
    // chroma upsample and its YCbCr->RGB conversion entirely, and hands the
    // pipeline the layout video actually wants. Anything else (grayscale, CMYK,
    // YCCK, lossless) falls through to the interleaved path below.
    if let Some(frame) = decode_jpeg_planar(data)? {
        return Ok(frame);
    }
    decode_jpeg_rgb(data)
}

/// Decode straight to planar Y'CbCr, or `Ok(None)` if this image isn't a layout
/// we can express as one of our planar pixel formats.
fn decode_jpeg_planar(data: &[u8]) -> Result<Option<Frame>> {
    let mut decoder = rusty_jpeg::decode::Decoder::new(Cursor::new(data));
    // A decode failure here is a real error, not a reason to retry interleaved:
    // the second attempt would fail the same way, just more slowly.
    let planar = match decoder.decode_planar() {
        Ok(p) => p,
        Err(rusty_jpeg::decode::Error::Unsupported(_)) => return Ok(None),
        Err(e) => return Err(Error::invalid(format!("jpeg decode: {e}"))),
    };
    let Some(sub) = planar.chroma_subsampling() else {
        return Ok(None);
    };
    let format = match sub {
        (1, 1) => PixelFormat::Yuv444p,
        (2, 1) => PixelFormat::Yuv422p,
        (2, 2) => PixelFormat::Yuv420p,
        _ => return Ok(None),
    };
    // The planes are block-aligned, so `stride` can exceed `width`; consumers
    // honour strides, and this avoids a repack copy of every plane.
    let (mut planes, mut strides) = (Vec::with_capacity(3), Vec::with_capacity(3));
    for c in planar.components {
        strides.push(c.stride);
        planes.push(c.data);
    }
    Ok(Some(Frame::Video(VideoFrame {
        width: planar.width as u32,
        height: planar.height as u32,
        format,
        planes,
        strides,
        pts: None,
    })))
}

fn decode_jpeg_rgb(data: &[u8]) -> Result<Frame> {
    let mut decoder = rusty_jpeg::decode::Decoder::new(Cursor::new(data));
    let pixels = decoder
        .decode()
        .map_err(|e| Error::invalid(format!("jpeg decode: {e}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| Error::invalid("jpeg decode: missing image info"))?;
    let (w, h) = (info.width as usize, info.height as usize);

    let (planes, stride) = match info.pixel_format {
        rusty_jpeg::decode::PixelFormat::RGB24 => (pixels, w * 3),
        rusty_jpeg::decode::PixelFormat::L8 => {
            // Grayscale → replicate into packed RGB.
            let mut rgb = vec![0u8; w * h * 3];
            for (i, &g) in pixels.iter().enumerate() {
                rgb[i * 3..i * 3 + 3].copy_from_slice(&[g, g, g]);
            }
            (rgb, w * 3)
        }
        other => {
            return Err(Error::unsupported(format!(
                "jpeg decode: pixel format {other:?} (only RGB24/L8)"
            )))
        }
    };

    Ok(Frame::Video(VideoFrame {
        width: info.width as u32,
        height: info.height as u32,
        format: PixelFormat::Rgb24,
        planes: vec![planes],
        strides: vec![stride],
        pts: None,
    }))
}

/// Encoder settings resolved from the output options.
struct JpegSettings {
    /// JPEG quality, 1–100 (higher is better).
    quality: u8,
    /// `None` = derive from quality, mirroring the encoder's own default.
    sampling: Option<SamplingFactor>,
    progressive: bool,
    optimize_huffman: bool,
    /// RD-optimal coefficient decisions. Off by default: ~2.4x encode time.
    trellis: bool,
    restart_interval: Option<u16>,
}

impl Default for JpegSettings {
    fn default() -> Self {
        Self {
            quality: DEFAULT_QUALITY,
            sampling: None,
            progressive: false,
            // ON by default, matching FFmpeg's mjpeg (`-huffman optimal`).
            // Building Huffman tables from the image's own histogram is
            // lossless — decoded pixels are bit-identical — and measured 5.4%
            // (q95) to 8.0% (q70) smaller on 1080p detail. It costs ~60% encode
            // time in our implementation (measured in-process, not via the CLI),
            // which is a known speed brick: FFmpeg does the same work far
            // cheaper. `-optimize_huffman 0` opts out.
            optimize_huffman: true,
            // Off by default — it costs ~+144% encode time on real footage.
            // `-trellis 1` opts in for smaller files.
            trellis: false,
            restart_interval: None,
        }
    }
}

/// Map FFmpeg's `-q:v` qscale (2 = best … 31 = worst) onto JPEG quality (1–100).
///
/// Calibrated, not derived. Deriving it from the two encoders' nominal table
/// multipliers put us 4–6 dB low at every qscale, so this is fitted to measured
/// equal-PSNR points on 1080p detail at 4:4:4 — the quality at which our output
/// matches what FFmpeg's mjpeg produces at that qscale:
///
/// | qscale | FFmpeg PSNR | our matching quality | curve |
/// |---|---|---|---|
/// | 2 | 40.07 dB | 94.7 | 95.1 |
/// | 3 | 37.15 dB | 91.8 | 91.9 |
/// | 4 | 35.22 dB | 88.7 | 88.4 |
/// | 6 | 33.08 dB | 82.3 | 80.8 |
/// | 10 | 31.05 dB | 63.9 | 63.5 |
///
/// The intent is that `-q:v N` yields the same *visual quality* as FFmpeg's
/// `-q:v N`. File sizes still differ — our rate at matched PSNR is behind, which
/// is a separate open problem, not something to hide by bending this curve.
fn quality_from_qscale(qscale: f32) -> u8 {
    let q = 100.0 - 2.05 * qscale.max(1.0).powf(1.25);
    q.round().clamp(1.0, 100.0) as u8
}

fn parse_sampling(value: &str) -> Option<SamplingFactor> {
    match value.trim() {
        "444" | "4:4:4" => Some(SamplingFactor::R_4_4_4),
        "440" | "4:4:0" => Some(SamplingFactor::R_4_4_0),
        "422" | "4:2:2" => Some(SamplingFactor::R_4_2_2),
        "420" | "4:2:0" => Some(SamplingFactor::R_4_2_0),
        "411" | "4:1:1" => Some(SamplingFactor::R_4_1_1),
        _ => None,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Default)]
struct JpegEncoder {
    packet: Option<Packet>,
    eof: bool,
    settings: JpegSettings,
}

impl RffEncoder for JpegEncoder {
    /// Planar Y'CbCr first — JPEG's own colour space, encoded with no
    /// conversion and no chroma resampling. Packed RGB is accepted and
    /// converted internally; anything else the pipeline converts to `yuv420p`.
    fn accepted_pixel_formats(&self) -> Option<Vec<PixelFormat>> {
        Some(vec![
            PixelFormat::Yuv420p,
            PixelFormat::Yuv422p,
            PixelFormat::Yuv444p,
            PixelFormat::Rgb24,
            PixelFormat::Rgba,
        ])
    }

    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        // `-q:v` / `-qscale:v` follow FFmpeg's mjpeg convention so the CLI stays
        // drop-in; `-jpeg_quality` is the direct 1–100 knob.
        for key in ["q", "qscale", "qp", "crf"] {
            if let Some(v) = options.get(key).and_then(|v| v.trim().parse::<f32>().ok()) {
                self.settings.quality = quality_from_qscale(v);
                break;
            }
        }
        if let Some(v) = options
            .get("jpeg_quality")
            .and_then(|v| v.trim().parse::<u8>().ok())
        {
            self.settings.quality = v.clamp(1, 100);
        }
        if let Some(v) = options
            .get("sampling")
            .or_else(|| options.get("jpeg_sampling"))
        {
            match parse_sampling(v) {
                Some(s) => self.settings.sampling = Some(s),
                None => {
                    return Err(Error::unsupported(format!(
                        "jpeg encode: unknown sampling `{v}` (want 444/440/422/420/411)"
                    )))
                }
            }
        }
        if let Some(v) = options.get("progressive").and_then(|v| parse_bool(v)) {
            self.settings.progressive = v;
        }
        if let Some(v) = options.get("trellis").and_then(|v| parse_bool(v)) {
            self.settings.trellis = v;
        }
        if let Some(v) = options.get("optimize_huffman").and_then(|v| parse_bool(v)) {
            self.settings.optimize_huffman = v;
        }
        if let Some(v) = options
            .get("restart_interval")
            .and_then(|v| v.trim().parse::<u16>().ok())
        {
            self.settings.restart_interval = Some(v);
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "jpeg encode: audio frame on an image codec",
                ))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_jpeg(vf, &self.settings)?));
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

fn encode_jpeg(vf: &VideoFrame, settings: &JpegSettings) -> Result<Vec<u8>> {
    if vf.width > u16::MAX as u32 || vf.height > u16::MAX as u32 {
        return Err(Error::unsupported("jpeg encode: dimensions exceed 65535"));
    }
    // Planar Y'CbCr goes straight in. JPEG is a Y'CbCr codec, so a planar frame
    // is already in the codec's own colour space at the codec's own chroma
    // resolution — routing it through RGB would convert twice and resample
    // chroma up then back down, which is both slower and lossy.
    if let Some(sub) = planar_subsampling(vf.format) {
        return encode_planar(vf, settings, sub);
    }
    let (color, channels) = match vf.format {
        PixelFormat::Rgb24 => (ColorType::Rgb, 3usize),
        PixelFormat::Rgba => (ColorType::Rgba, 4usize),
        other => {
            return Err(Error::unsupported(format!(
            "jpeg encode: needs rgb24/rgba or planar yuv, got `{}` (convert with -vf format=rgb24)",
            other.name()
        )))
        }
    };
    let (w, h) = (vf.width as usize, vf.height as usize);
    let row = w * channels;
    let stride = vf.strides[0];

    // rusty_jpeg::encode wants tightly packed rows.
    let packed: Vec<u8> = if stride == row {
        vf.planes[0].clone()
    } else {
        let mut p = Vec::with_capacity(row * h);
        for j in 0..h {
            p.extend_from_slice(&vf.planes[0][j * stride..j * stride + row]);
        }
        p
    };

    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, settings.quality);
    if let Some(sampling) = settings.sampling {
        encoder.set_sampling_factor(sampling);
    }
    encoder.set_progressive(settings.progressive);
    encoder.set_optimized_huffman_tables(settings.optimize_huffman);
    encoder.set_trellis(settings.trellis);
    if let Some(interval) = settings.restart_interval {
        encoder.set_restart_interval(interval);
    }
    encoder
        .encode(&packed, vf.width as u16, vf.height as u16, color)
        .map_err(|e| Error::invalid(format!("jpeg encode: {e}")))?;
    Ok(out)
}

/// Chroma subsampling `(horizontal, vertical)` of a planar 8-bit Y'CbCr format,
/// or `None` if the format isn't one.
fn planar_subsampling(format: PixelFormat) -> Option<(usize, usize)> {
    match format {
        PixelFormat::Yuv444p => Some((1, 1)),
        PixelFormat::Yuv422p => Some((2, 1)),
        PixelFormat::Yuv420p => Some((2, 2)),
        _ => None,
    }
}

/// Encode a planar Y'CbCr frame with no colour conversion and no chroma
/// resampling — the planes are handed to the encoder as-is.
fn encode_planar(vf: &VideoFrame, settings: &JpegSettings, sub: (usize, usize)) -> Result<Vec<u8>> {
    if vf.planes.len() < 3 || vf.strides.len() < 3 {
        return Err(Error::invalid(
            "jpeg encode: planar yuv frame is missing planes",
        ));
    }
    let image = rusty_jpeg::encode::PlanarYcbcrImage::new(
        &vf.planes[0],
        &vf.planes[1],
        &vf.planes[2],
        [vf.strides[0], vf.strides[1], vf.strides[2]],
        vf.width as u16,
        vf.height as u16,
        sub,
    )
    .ok_or_else(|| Error::invalid("jpeg encode: planar yuv planes too small for the frame"))?;

    // The planar path is only lossless while the encoder's sampling factor
    // matches the source's. Honouring a conflicting `-sampling` would mean
    // resampling chroma here, silently undoing the reason this path exists — so
    // say so instead.
    let native = image.sampling_factor();
    if let Some(requested) = settings.sampling {
        if requested != native {
            return Err(Error::unsupported(format!(
                "jpeg encode: -sampling conflicts with the source's chroma layout \
                 (`{}`); convert first with `-vf format=...`, or drop -sampling to \
                 encode the source layout as-is",
                vf.format.name()
            )));
        }
    }

    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, settings.quality);
    encoder.set_sampling_factor(native);
    encoder.set_progressive(settings.progressive);
    encoder.set_optimized_huffman_tables(settings.optimize_huffman);
    encoder.set_trellis(settings.trellis);
    if let Some(interval) = settings.restart_interval {
        encoder.set_restart_interval(interval);
    }
    encoder
        .encode_image(image)
        .map_err(|e| Error::invalid(format!("jpeg encode: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_encode_decode_roundtrips_approximately() {
        let (w, h) = (48u32, 32u32);
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
        let src = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![rgb.clone()],
            strides: vec![wi * 3],
            pts: None,
        };

        let bytes = encode_jpeg(&src, &JpegSettings::default()).unwrap();
        assert_eq!(&bytes[0..3], &[0xFF, 0xD8, 0xFF]); // SOI marker

        let Frame::Video(decoded) = decode_jpeg(&bytes).unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (w, h));
        // Decode now returns PLANAR Y'CbCr, not packed RGB: a 3-component JPEG
        // is tapped before the decoder's chroma upsampler and colour conversion.
        // This encode is 4:4:4 (quality 90), so the planes are full resolution.
        assert_eq!(decoded.format, PixelFormat::Yuv444p);
        assert_eq!(decoded.planes.len(), 3);

        // JPEG is lossy; a smooth gradient should stay close. Convert the planes
        // back to RGB (BT.601 full-range, matching the encoder's own matrix) so
        // the comparison is against the original source.
        let (yp, cb, cr) = (&decoded.planes[0], &decoded.planes[1], &decoded.planes[2]);
        let (ys, cbs, crs) = (decoded.strides[0], decoded.strides[1], decoded.strides[2]);
        let mut total = 0u64;
        for j in 0..hi {
            for i in 0..wi {
                let yy = yp[j * ys + i] as f32;
                let b = cb[j * cbs + i] as f32 - 128.0;
                let r = cr[j * crs + i] as f32 - 128.0;
                let got = [
                    yy + 1.402 * r,
                    yy - 0.344_136 * b - 0.714_136 * r,
                    yy + 1.772 * b,
                ];
                for (c, g) in got.iter().enumerate() {
                    let want = rgb[(j * wi + i) * 3 + c] as f32;
                    total += (g.clamp(0.0, 255.0) - want).abs() as u64;
                }
            }
        }
        let mean = total as f64 / (wi * hi * 3) as f64;
        assert!(mean < 12.0, "jpeg round-trip drifted too far: {mean:.2}");
    }
}
