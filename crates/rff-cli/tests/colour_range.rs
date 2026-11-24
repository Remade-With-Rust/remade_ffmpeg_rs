//! Colour range end to end.
//!
//! * #213: full-range Y'CbCr (`-vf format=yuv420p` from an RGB picture, or a
//!   JFIF JPEG, which decodes to full-range planes) into AVIF, VP9 and H.264
//!   must signal full range in the bitstream (AV1 / VP9 `color_range`, H.264
//!   `video_full_range_flag`) so it decodes back to the source. Before the
//!   fix the bitstream said limited and the picture came back at ~29 dB.
//! * #214: `setrange` expands 10- and 12-bit limited-range frames too, so a
//!   10-bit AVIF decodes to PNG with black at 0 and white at 255 instead of
//!   16 and 235.
//!
//! Every case runs the real `rff` binary. The headers are read with the
//! container/bitstream parsers in `rff::format`, and the decoded pictures are
//! compared with the source.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rff::core::{CodecId, ColorRange, Frame, PixelFormat, VideoFrame};
use rff::Engine;

const W: usize = 96;
const H: usize = 64;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rff_colour_range_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn rff(args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_rff"))
        .arg("-hide_banner")
        .arg("-y")
        .args(args)
        .output()
        .expect("run rff");
    assert!(
        out.status.success(),
        "rff {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The test picture: colour ramps, a sine pattern, and a black and a white
/// patch (the range anchors).
fn pixel(x: usize, y: usize) -> [u16; 3] {
    if y < 16 && x < 16 {
        return [0; 3];
    }
    if y < 16 && x < 32 {
        return [65535; 3];
    }
    let r = x as f64 / (W - 1) as f64;
    let g = y as f64 / (H - 1) as f64;
    let b = 0.5 + 0.39 * (x as f64 / 7.0).sin() * (y as f64 / 5.0).cos();
    [r, g, b].map(|v| (v * 65535.0).round() as u16)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = data.chunks(65535).collect();
    for (i, c) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        out.extend_from_slice(&(c.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        out.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// The test picture as an 8- or 16-bit RGB PNG.
fn write_png(path: &Path, bit_depth: u8) {
    let mut raw = Vec::new();
    for y in 0..H {
        raw.push(0);
        for x in 0..W {
            for v in pixel(x, y) {
                if bit_depth == 16 {
                    raw.extend_from_slice(&v.to_be_bytes());
                } else {
                    raw.push(((v as u32 + 128) / 257) as u8);
                }
            }
        }
    }
    let chunk = |out: &mut Vec<u8>, ty: &[u8; 4], body: &[u8]| {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut typed = ty.to_vec();
        typed.extend_from_slice(body);
        out.extend_from_slice(&typed);
        out.extend_from_slice(&crc32(&typed).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(W as u32).to_be_bytes());
    ihdr.extend_from_slice(&(H as u32).to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, 2, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    fs::write(path, png).unwrap();
}

/// Open `path` with our demuxer for `format`; the first stream and packet.
fn demux(format: &str, path: &Path) -> (rff::format::Stream, Vec<u8>) {
    let engine = Engine::new();
    let mut dem = engine
        .formats
        .open_demuxer(format, Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    let streams = dem.read_header().unwrap();
    let packet = dem.read_packet().unwrap();
    (streams[0].clone(), packet.data)
}

/// Decode a PNG with our decoder, as packed 8-bit RGB.
fn decode_png(path: &Path) -> VideoFrame {
    let engine = Engine::new();
    let (_, data) = demux("png", path);
    let mut dec = engine.codecs.find_decoder(CodecId::Png).unwrap();
    dec.send_packet(&rff::core::Packet::from_data(0, data))
        .unwrap();
    dec.flush();
    match dec.receive_frame() {
        Ok(Frame::Video(v)) => {
            assert_eq!(v.format, PixelFormat::Rgb24, "{}", path.display());
            v
        }
        _ => panic!("no frame from {}", path.display()),
    }
}

fn rgb(v: &VideoFrame, x: usize, y: usize) -> [f64; 3] {
    let at = y * v.strides[0] + x * 3;
    [0, 1, 2].map(|i| v.planes[0][at + i] as f64)
}

fn psnr(a: &VideoFrame, b: &VideoFrame) -> f64 {
    let mut sse = 0.0;
    for y in 0..H {
        for x in 0..W {
            let (p, q) = (rgb(a, x, y), rgb(b, x, y));
            sse += (0..3).map(|i| (p[i] - q[i]).powi(2)).sum::<f64>();
        }
    }
    let mse = sse / (W * H * 3) as f64;
    10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
}

/// Mean RGB over a rectangle.
fn mean(v: &VideoFrame, x0: usize, y0: usize, x1: usize, y1: usize) -> [f64; 3] {
    let mut s = [0.0; 3];
    for y in y0..y1 {
        for x in x0..x1 {
            let p = rgb(v, x, y);
            for i in 0..3 {
                s[i] += p[i];
            }
        }
    }
    s.map(|c| c / ((x1 - x0) * (y1 - y0)) as f64)
}

/// The range the encoded file's own bitstream signals.
fn signalled_range(path: &Path) -> ColorRange {
    let ext = path.extension().unwrap().to_str().unwrap();
    match ext {
        // The AVIF demuxer reads the sequence header's `color_range`.
        "avif" => demux("avif", path).0.color_range,
        // VP9: the first key frame's uncompressed header (profile 0:
        // marker/profile/flags byte, 3 sync bytes, then color_space(3) and
        // color_range).
        "webm" | "ivf" => {
            let (_, frame) = demux(ext, path);
            assert_eq!(&frame[1..4], &[0x49, 0x83, 0x42], "not a key frame");
            if frame[4] & 0x10 != 0 {
                ColorRange::Full
            } else {
                ColorRange::Limited
            }
        }
        // H.264: the SPS VUI in the `avcC`.
        "mp4" => {
            let (stream, _) = demux("mp4", path);
            let cfg = rff::format::avc::parse_avcc(&stream.extradata).expect("avcC");
            let sps = rff::format::avc::find_sps_annexb(&cfg.headers_annexb).expect("SPS");
            // No VUI (or no video signal type in it) means limited range.
            match rff::format::avc::sps_full_range(sps) {
                Some(true) => ColorRange::Full,
                Some(false) | None => ColorRange::Limited,
            }
        }
        other => panic!("{other}"),
    }
}

const OUTPUTS: [(&str, &str); 4] = [
    ("avif", "avif"),
    ("webm", "vp9"),
    ("ivf", "vp9"),
    ("mp4", "h264"),
];

/// Encode `src` (+ `args`) to every output, check the bitstream signals
/// `want`, decode each back to PNG with rff and compare with `reference`.
fn round_trip(tag: &str, src: &Path, args: &[&str], want: ColorRange, reference: &VideoFrame) {
    for (ext, codec) in OUTPUTS {
        let out = scratch(&format!("{tag}.{ext}"));
        let mut cmd = vec!["-i", src.to_str().unwrap()];
        cmd.extend_from_slice(args);
        cmd.extend_from_slice(&["-c:v", codec, out.to_str().unwrap()]);
        rff(&cmd);
        assert_eq!(signalled_range(&out), want, "{tag}.{ext}: signalled range");

        let png = scratch(&format!("{tag}_{ext}.png"));
        rff(&["-i", out.to_str().unwrap(), png.to_str().unwrap()]);
        let back = decode_png(&png);
        let db = psnr(&back, reference);
        // Measured 36-43 dB; the mislabelled files came back at 29 dB.
        assert!(db > 34.0, "{tag}.{ext}: {db:.2} dB against the source");
        let black = mean(&back, 3, 3, 13, 13);
        let white = mean(&back, 19, 3, 29, 13);
        assert!(
            black.iter().all(|&c| c < 6.0),
            "{tag}.{ext}: black {black:?}"
        );
        assert!(
            white.iter().all(|&c| c > 249.0),
            "{tag}.{ext}: white {white:?}"
        );
    }
}

/// #213: `-vf format=yuv420p` follows the RGB source's full range; every
/// encoder now says so.
#[test]
fn full_range_yuv_from_rgb_is_signalled_and_round_trips() {
    let png = scratch("src.png");
    write_png(&png, 8);
    let source = decode_png(&png);
    round_trip(
        "vf420",
        &png,
        &["-vf", "format=yuv420p"],
        ColorRange::Full,
        &source,
    );
}

/// #213: a JFIF JPEG decodes to full-range planes, which go to the encoder
/// as they are.
#[test]
fn full_range_jpeg_is_signalled_and_round_trips() {
    let png = scratch("jsrc.png");
    write_png(&png, 8);
    let jpg = scratch("src.jpg");
    rff(&["-i", png.to_str().unwrap(), jpg.to_str().unwrap()]);
    // The reference is the JPEG itself, decoded by rff.
    let jpg_png = scratch("jpg_ref.png");
    rff(&["-i", jpg.to_str().unwrap(), jpg_png.to_str().unwrap()]);
    round_trip("jpg", &jpg, &[], ColorRange::Full, &decode_png(&jpg_png));
}

/// The automatic RGB → Y'CbCr conversion stays limited range, and says so.
#[test]
fn automatic_conversion_stays_limited() {
    let png = scratch("auto.png");
    write_png(&png, 8);
    let source = decode_png(&png);
    round_trip("auto", &png, &[], ColorRange::Limited, &source);
}

/// #214: a 10-bit limited-range AVIF decodes to PNG with full-range levels.
#[test]
fn ten_bit_limited_avif_to_png_is_expanded() {
    let png16 = scratch("deep.png");
    write_png(&png16, 16);
    let png8 = scratch("deep8.png");
    write_png(&png8, 8);
    let avif = scratch("deep10.avif");
    rff(&[
        "-i",
        png16.to_str().unwrap(),
        "-pix_fmt",
        "yuv420p10le",
        avif.to_str().unwrap(),
    ]);
    let (stream, _) = demux("avif", &avif);
    assert_eq!(stream.color_range, ColorRange::Limited);

    let out = scratch("deep10.png");
    rff(&["-i", avif.to_str().unwrap(), out.to_str().unwrap()]);
    let back = decode_png(&out);
    let black = mean(&back, 3, 3, 13, 13);
    let white = mean(&back, 19, 3, 29, 13);
    // Unexpanded, these were 16 and 235.
    assert!(black.iter().all(|&c| c < 3.0), "black {black:?}");
    assert!(white.iter().all(|&c| c > 252.0), "white {white:?}");
    // Measured 36.7 dB (27.9 dB before), the same as the 8-bit AVIF path.
    let db = psnr(&back, &decode_png(&png8));
    assert!(db > 34.0, "{db:.2} dB against the source");
}
