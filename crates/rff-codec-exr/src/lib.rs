//! OpenEXR still-image codec, backed by the pure-Rust [`rusty_exr`] crate.
//!
//! An EXR file is self-describing, so a packet *is* the whole file.
//!
//! **Decode** reads part 0 (level 0 of a tiled part) with every compression
//! EXR defines, and produces packed [`Rgba`](PixelFormat::Rgba) (when the
//! image has alpha) or [`Rgb24`](PixelFormat::Rgb24) over the display
//! window, the way FFmpeg's EXR decoder frames it (pixels outside the data
//! window are zero). The base layer's `R`/`G`/`B`(/`A`) are used, or `Y` as
//! grey; failing both, the first layer that has them. Values are linear and
//! are clamped to `[0, 1]` and scaled to 8 bits without a transfer curve
//! (FFmpeg's default, `-apply_trc` unset). The full float data stays
//! available through `rusty_exr` itself; this pipeline speaks 8-bit RGB.
//!
//! **Encode** writes `R`,`G`,`B`(,`A`) as half floats (or 32-bit floats with
//! `-exr_format float`), value / 255, ZIP compressed by default
//! (`-exr_compression none|rle|zip1|zips|zip16|zip|piz|pxr24|b44|b44a|dwaa|dwab`).

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_exr::{Box2i, Compression, EncodeOptions, Image, Samples};

/// Register the EXR codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Exr,
        name: "exr",
        long_name: "OpenEXR image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(ExrDecoder::default())),
        encoder: Some(|| Box::new(ExrEncoder::default())),
    });
}

fn map_err(e: rusty_exr::Error) -> Error {
    match e {
        rusty_exr::Error::Unsupported(m) => Error::unsupported(format!("exr: {m}")),
        other => Error::invalid(format!("exr: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ExrDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for ExrDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_exr(&packet.data)?);
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

/// Linear float to 8 bits: clamp to [0, 1], scale, round (NaN -> 0).
#[inline]
fn to_u8(v: f32) -> u8 {
    if v >= 1.0 {
        255
    } else if v > 0.0 {
        (v * 255.0 + 0.5) as u8
    } else {
        0
    }
}

/// The layer to show: the base layer if it has colour or `Y`, else the
/// first layer that does.
fn pick_layer(img: &Image) -> Option<(String, Vec<f32>)> {
    if let Some(px) = img.to_rgba_f32("") {
        return Some((String::new(), px));
    }
    img.layers()
        .into_iter()
        .find_map(|l| img.to_rgba_f32(&l).map(|px| (l, px)))
}

fn decode_exr(data: &[u8]) -> Result<Frame> {
    let img = rusty_exr::decode(data).map_err(map_err)?;
    let (layer, rgba) = pick_layer(&img)
        .ok_or_else(|| Error::unsupported("exr: no R/G/B or Y channels to show"))?;
    let alpha_name = if layer.is_empty() {
        "A".to_string()
    } else {
        format!("{layer}.A")
    };
    let has_alpha = img.channel(&alpha_name).is_some();
    let dw = img.data_window;
    let disp = img
        .header
        .display_window()
        .filter(|d| {
            let (w, h) = (d.width(), d.height());
            w > 0 && h > 0 && (w as u64) * (h as u64) <= 1 << 28
        })
        .unwrap_or(dw);
    let (w, h) = (disp.width() as usize, disp.height() as usize);
    let ch = if has_alpha { 4 } else { 3 };
    let mut out = vec![0u8; w * h * ch];
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    for y in 0..h as i64 {
        let sy = i64::from(disp.y_min) + y - i64::from(dw.y_min);
        if sy < 0 || sy >= ih {
            continue;
        }
        for x in 0..w as i64 {
            let sx = i64::from(disp.x_min) + x - i64::from(dw.x_min);
            if sx < 0 || sx >= iw {
                continue;
            }
            let s = ((sy * iw + sx) * 4) as usize;
            let d = ((y as usize) * w + x as usize) * ch;
            for k in 0..ch {
                out[d + k] = to_u8(rgba[s + k]);
            }
        }
    }
    Ok(Frame::Video(VideoFrame {
        width: w as u32,
        height: h as u32,
        format: if has_alpha {
            PixelFormat::Rgba
        } else {
            PixelFormat::Rgb24
        },
        planes: vec![out],
        strides: vec![w * ch],
        pts: None,
    }))
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Settings {
    compression: Compression,
    float: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            compression: Compression::Zip,
            float: false,
        }
    }
}

#[derive(Default)]
struct ExrEncoder {
    settings: Settings,
    packet: Option<Packet>,
    eof: bool,
}

fn compression_from_name(s: &str) -> Option<Compression> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "none" | "raw" => Compression::None,
        "rle" => Compression::Rle,
        "zip1" | "zips" => Compression::Zips,
        "zip16" | "zip" => Compression::Zip,
        "piz" => Compression::Piz,
        "pxr24" => Compression::Pxr24,
        "b44" => Compression::B44,
        "b44a" => Compression::B44a,
        "dwaa" => Compression::Dwaa,
        "dwab" => Compression::Dwab,
        _ => return None,
    })
}

impl Encoder for ExrEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(v) = options.get("exr_compression") {
            self.settings.compression = compression_from_name(v).ok_or_else(|| {
                Error::unsupported(format!(
                    "exr encode: unknown -exr_compression `{v}` (want none/rle/zip1/zip16/piz/pxr24/b44/b44a/dwaa/dwab)"
                ))
            })?;
        }
        if let Some(v) = options.get("exr_format") {
            self.settings.float = match v.trim() {
                "half" => false,
                "float" => true,
                _ => {
                    return Err(Error::unsupported(format!(
                        "exr encode: -exr_format wants half or float, got `{v}`"
                    )))
                }
            };
        }
        Ok(())
    }

    fn accepted_pixel_formats(&self) -> Option<Vec<PixelFormat>> {
        Some(vec![PixelFormat::Rgba, PixelFormat::Rgb24])
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "exr encode: audio frame on an image codec",
                ))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_exr(vf, self.settings)?));
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

fn encode_exr(vf: &VideoFrame, settings: Settings) -> Result<Vec<u8>> {
    let ch = match vf.format {
        PixelFormat::Rgb24 => 3usize,
        PixelFormat::Rgba => 4usize,
        other => {
            return Err(Error::unsupported(format!(
                "exr encode: needs rgb24/rgba, got `{}`",
                other.name()
            )))
        }
    };
    let (w, h) = (vf.width as usize, vf.height as usize);
    if w == 0 || h == 0 {
        return Err(Error::invalid("exr encode: empty frame"));
    }
    let stride = vf.strides[0];
    let plane = &vf.planes[0];
    if plane.len() < stride * (h - 1) + w * ch {
        return Err(Error::invalid("exr encode: frame plane too small"));
    }
    let names = ["R", "G", "B", "A"];
    let mut img = Image::new(vf.width, vf.height);
    img.data_window = Box2i::from_size(vf.width, vf.height);
    for (k, name) in names.iter().enumerate().take(ch) {
        let vals = (0..h)
            .flat_map(|y| (0..w).map(move |x| f32::from(plane[y * stride + x * ch + k]) / 255.0));
        let samples = if settings.float {
            Samples::F32(vals.collect())
        } else {
            Samples::F16(vals.map(rusty_exr::f32_to_f16).collect())
        };
        img = img.with_channel(*name, samples);
    }
    rusty_exr::encode(&img, &EncodeOptions::new(settings.compression)).map_err(map_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossy_compressions_round_trip_closely() {
        let (w, h) = (37u32, 21u32);
        let px: Vec<u8> = (0..w * h * 3)
            .map(|i| ((i % (w * 3)) * 255 / (w * 3)) as u8)
            .collect();
        let vf = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![px.clone()],
            strides: vec![w as usize * 3],
            pts: None,
        };
        for c in ["b44", "b44a", "dwaa", "dwab"] {
            let s = Settings {
                compression: compression_from_name(c).unwrap(),
                float: false,
            };
            let file = encode_exr(&vf, s).unwrap();
            let Frame::Video(back) = decode_exr(&file).unwrap() else {
                panic!("video");
            };
            let worst = back.planes[0]
                .iter()
                .zip(&px)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap();
            assert!(worst <= 8, "{c}: worst 8-bit error {worst}");
        }
    }

    #[test]
    fn rgba_round_trip_is_exact_in_8_bits() {
        let (w, h) = (5u32, 3u32);
        let px: Vec<u8> = (0..w * h * 4).map(|i| (i * 17 % 256) as u8).collect();
        let vf = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgba,
            planes: vec![px.clone()],
            strides: vec![w as usize * 4],
            pts: None,
        };
        for c in ["none", "rle", "zip1", "zip16", "piz", "pxr24"] {
            for float in [false, true] {
                let s = Settings {
                    compression: compression_from_name(c).unwrap(),
                    float,
                };
                let file = encode_exr(&vf, s).unwrap();
                let Frame::Video(back) = decode_exr(&file).unwrap() else {
                    panic!("video");
                };
                assert_eq!(back.format, PixelFormat::Rgba);
                assert_eq!(back.planes[0], px, "{c} float={float}");
            }
        }
    }

    #[test]
    fn rgb_decodes_without_alpha() {
        let vf = VideoFrame {
            width: 2,
            height: 2,
            format: PixelFormat::Rgb24,
            planes: vec![vec![0, 64, 128, 255, 1, 2, 3, 4, 5, 6, 7, 8]],
            strides: vec![6],
            pts: None,
        };
        let file = encode_exr(&vf, Settings::default()).unwrap();
        let Frame::Video(back) = decode_exr(&file).unwrap() else {
            panic!("video");
        };
        assert_eq!(back.format, PixelFormat::Rgb24);
        assert_eq!(back.planes[0], vf.planes[0]);
    }
}
