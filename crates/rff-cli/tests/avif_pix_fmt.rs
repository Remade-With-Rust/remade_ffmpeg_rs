//! Issue #179: an RGB picture into a Y'CbCr encoder (AVIF here) is converted
//! automatically, and `-pix_fmt` picks the format — FFmpeg's behaviour.
//!
//! Every case runs the real `rff` binary, then decodes the `.avif` with our own
//! AV1 decoder and checks the pixels against an independent BT.601
//! limited-range conversion of the source. When Python with Pillow's AVIF
//! plugin (libavif + dav1d) is on PATH, the file is also decoded there and
//! compared with the source RGB; without it that half is skipped.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rff::core::{CodecId, Error, Frame, PixelFormat, VideoFrame};
use rff::Engine;

const W: usize = 48;
const H: usize = 32;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rff_avif_pix_fmt_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Run `rff -hide_banner -y ARGS`; return (success, stderr).
fn rff(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rff"))
        .arg("-hide_banner")
        .arg("-y")
        .args(args)
        .output()
        .expect("run rff");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The test picture: smooth colour ramps, 16-bit precision per channel.
fn pixel16(x: usize, y: usize) -> [u16; 4] {
    let r = (x * 65535 / (W - 1)) as u16;
    let g = (y * 65535 / (H - 1)) as u16;
    let b = (((x + y) * 65535) / (W + H - 2)) as u16;
    let a = (65535 - x * 65535 / (W - 1)) as u16;
    [r, g, b, a]
}

/// The 8-bit RGB the pipeline sees for pixel (x, y) — FFmpeg's 16 → 8
/// reduction, which our PNG decoder matches, for the 16-bit file.
fn rgb8(x: usize, y: usize, deep: bool) -> [f64; 3] {
    let p = pixel16(x, y);
    let c = |v: u16| {
        if deep {
            ((v as u32 + 128) >> 8).min(255) as f64
        } else {
            (v >> 8) as f64
        }
    };
    [c(p[0]), c(p[1]), c(p[2])]
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

/// zlib stream of stored (uncompressed) deflate blocks.
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

/// Write the test picture as a PNG: 8 or 16 bits, RGB or RGBA.
fn write_png(path: &Path, bit_depth: u8, alpha: bool) {
    let mut raw = Vec::new();
    for y in 0..H {
        raw.push(0); // filter: none
        for x in 0..W {
            let p = pixel16(x, y);
            for &v in &p[..if alpha { 4 } else { 3 }] {
                if bit_depth == 16 {
                    raw.extend_from_slice(&v.to_be_bytes());
                } else {
                    raw.push((v >> 8) as u8);
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
    ihdr.extend_from_slice(&[bit_depth, if alpha { 6 } else { 2 }, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    fs::write(path, png).unwrap();
}

/// Decode an `.avif` with our own demuxer + AV1 decoder.
fn decode_avif(path: &Path) -> VideoFrame {
    let engine = Engine::new();
    let mut dem = engine
        .formats
        .open_demuxer("avif", Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    dem.read_header().unwrap();
    let packet = dem.read_packet().unwrap();
    let mut dec = engine.codecs.find_decoder(CodecId::Avif).unwrap();
    dec.send_packet(&packet).unwrap();
    dec.flush();
    match dec.receive_frame() {
        Ok(Frame::Video(v)) => v,
        Ok(Frame::Audio(_)) => panic!("audio from an image"),
        Err(Error::Eof) | Err(_) => panic!("avif decode produced no frame"),
    }
}

fn sample(v: &VideoFrame, plane: usize, x: usize, y: usize) -> f64 {
    let bps = v.format.bytes_per_sample();
    let at = y * v.strides[plane] + x * bps;
    if bps == 2 {
        u16::from_le_bytes([v.planes[plane][at], v.planes[plane][at + 1]]) as f64
    } else {
        v.planes[plane][at] as f64
    }
}

fn subsampling(f: PixelFormat) -> (usize, usize) {
    match f {
        PixelFormat::Yuv420p | PixelFormat::Yuv420p10 => (2, 2),
        PixelFormat::Yuv422p | PixelFormat::Yuv422p10 => (2, 1),
        _ => (1, 1),
    }
}

/// PSNR of the decoded planes against BT.601 limited-range Y'CbCr computed
/// here from the source RGB (chroma from the RGB average of each block).
/// Returns (luma dB, chroma dB). A full-range encode, or one that mixed up
/// the matrix, lands far below the thresholds the tests use.
fn psnr_vs_bt601_limited(v: &VideoFrame, deep_source: bool) -> (f64, f64) {
    let scale = (1u32 << (v.format.bit_depth() - 8)) as f64;
    let peak = 255.0 * scale;
    let (kr, kb) = (0.299, 0.114);
    let kg = 1.0 - kr - kb;
    let luma = |[r, g, b]: [f64; 3]| (16.0 + 219.0 / 255.0 * (kr * r + kg * g + kb * b)) * scale;
    let cb = |[r, g, b]: [f64; 3]| {
        (128.0 + 224.0 / 255.0 * (-0.5 * kr / (1.0 - kb) * r - 0.5 * kg / (1.0 - kb) * g + 0.5 * b))
            * scale
    };
    let cr = |[r, g, b]: [f64; 3]| {
        (128.0 + 224.0 / 255.0 * (0.5 * r - 0.5 * kg / (1.0 - kr) * g - 0.5 * kb / (1.0 - kr) * b))
            * scale
    };
    let db = |sse: f64, n: f64| 10.0 * (peak * peak / (sse / n).max(1e-9)).log10();

    let mut sse = 0.0;
    for y in 0..H {
        for x in 0..W {
            sse += (sample(v, 0, x, y) - luma(rgb8(x, y, deep_source))).powi(2);
        }
    }
    let luma_db = db(sse, (W * H) as f64);

    let (sh, sv) = subsampling(v.format);
    let (cw, ch) = (W.div_ceil(sh), H.div_ceil(sv));
    let mut sse = 0.0;
    for cy in 0..ch {
        for cx in 0..cw {
            let mut avg = [0.0; 3];
            let mut n = 0.0;
            for y in cy * sv..(cy * sv + sv).min(H) {
                for x in cx * sh..(cx * sh + sh).min(W) {
                    let p = rgb8(x, y, deep_source);
                    for i in 0..3 {
                        avg[i] += p[i];
                    }
                    n += 1.0;
                }
            }
            let avg = avg.map(|s| s / n);
            sse += (sample(v, 1, cx, cy) - cb(avg)).powi(2);
            sse += (sample(v, 2, cx, cy) - cr(avg)).powi(2);
        }
    }
    (luma_db, db(sse, (2 * cw * ch) as f64))
}

/// Independent decode (Pillow → libavif → dav1d), PSNR vs the 8-bit source
/// RGB. `None` when Python/Pillow-AVIF is not available.
fn pillow_psnr(avif: &Path, png: &Path) -> Option<f64> {
    let script = "import sys,math\n\
from PIL import Image, features\n\
assert features.check('avif')\n\
a=Image.open(sys.argv[1]).convert('RGB').tobytes()\n\
b=Image.open(sys.argv[2]).convert('RGB').tobytes()\n\
m=sum((x-y)**2 for x,y in zip(a,b))/len(a)\n\
print(99.0 if m==0 else 10*math.log10(255*255/m))";
    for py in ["python", "python3"] {
        let Ok(out) = Command::new(py)
            .args(["-I", "-c", script])
            .arg(avif)
            .arg(png)
            .output()
        else {
            continue;
        };
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().parse().ok();
        }
    }
    None
}

/// Encode `png` to a fresh `.avif` with extra args; check the decoded format
/// and its fidelity, both decoders. Returns (output path, stderr).
fn encode_and_check(
    tag: &str,
    png: &Path,
    args: &[&str],
    want: PixelFormat,
    deep_source: bool,
) -> (PathBuf, String) {
    let out = scratch(&format!("{tag}.avif"));
    let mut full = vec!["-i", png.to_str().unwrap()];
    full.extend_from_slice(args);
    full.push(out.to_str().unwrap());
    let (ok, stderr) = rff(&full);
    assert!(ok, "{tag}: rff failed:\n{stderr}");

    let frame = decode_avif(&out);
    assert_eq!((frame.width, frame.height), (W as u32, H as u32), "{tag}");
    assert_eq!(frame.format, want, "{tag}: decoded format");
    let (y_db, c_db) = psnr_vs_bt601_limited(&frame, deep_source);
    assert!(
        y_db > 38.0 && c_db > 34.0,
        "{tag}: decoded planes vs BT.601 limited source: Y {y_db:.2} dB, C {c_db:.2} dB"
    );

    // A file written as 8-bit RGB is what Pillow compares against.
    let reference = scratch("reference_rgb8.png");
    if !reference.exists() {
        write_png(&reference, 8, false);
    }
    match pillow_psnr(&out, &reference) {
        Some(db) => assert!(db > 34.0, "{tag}: libavif/dav1d decode PSNR {db:.2} dB"),
        None => eprintln!("{tag}: Pillow AVIF not available, independent decode skipped"),
    }
    (out, stderr)
}

#[test]
fn rgb_png_to_avif_converts_automatically() {
    let png = scratch("rgb.png");
    write_png(&png, 8, false);
    let (auto, stderr) = encode_and_check("auto", &png, &[], PixelFormat::Yuv420p, false);
    assert!(!stderr.contains("[warning]"), "{stderr}");

    // `-pix_fmt yuv420p` and the explicit filter route are the same encode,
    // byte for byte.
    let (explicit, _) = encode_and_check(
        "pix_fmt_yuv420p",
        &png,
        &["-pix_fmt", "yuv420p"],
        PixelFormat::Yuv420p,
        false,
    );
    let (filtered, _) = encode_and_check(
        "vf_format",
        &png,
        &["-vf", "format=yuv420p:bt601:limited"],
        PixelFormat::Yuv420p,
        false,
    );
    let auto = fs::read(auto).unwrap();
    assert_eq!(
        auto,
        fs::read(explicit).unwrap(),
        "-pix_fmt yuv420p != auto"
    );
    assert_eq!(auto, fs::read(filtered).unwrap(), "-vf format=... != auto");
}

#[test]
fn pix_fmt_selects_the_encoder_format() {
    let png = scratch("rgb_sel.png");
    write_png(&png, 8, false);
    for (name, want) in [
        ("yuv444p", PixelFormat::Yuv444p),
        ("yuv422p", PixelFormat::Yuv422p),
        ("yuv420p10le", PixelFormat::Yuv420p10),
        ("yuv444p10le", PixelFormat::Yuv444p10),
    ] {
        let (_, stderr) = encode_and_check(name, &png, &["-pix_fmt", name], want, false);
        assert!(!stderr.contains("[warning]"), "{name}: {stderr}");
    }
}

#[test]
fn unsupported_pix_fmt_falls_back_with_a_warning() {
    let png = scratch("rgb_fallback.png");
    write_png(&png, 8, false);
    let (_, stderr) = encode_and_check(
        "fallback",
        &png,
        &["-pix_fmt", "rgb24"],
        PixelFormat::Yuv420p,
        false,
    );
    assert!(
        stderr.contains(
            "Incompatible pixel format 'rgb24' for codec 'avif', auto-selecting format 'yuv420p'"
        ),
        "{stderr}"
    );
    let (ok, stderr) = rff(&["-i", png.to_str().unwrap(), "-pix_fmt", "bogus", "x.avif"]);
    assert!(
        !ok && stderr.contains("Unknown pixel format requested: bogus"),
        "{stderr}"
    );
}

#[test]
fn rgba_png_drops_alpha_with_a_warning() {
    let png = scratch("rgba.png");
    write_png(&png, 8, true);
    let (_, stderr) = encode_and_check("rgba", &png, &[], PixelFormat::Yuv420p, false);
    assert!(stderr.contains("alpha"), "{stderr}");
}

#[test]
fn sixteen_bit_png_is_encoded() {
    // Our PNG decoder reduces 16-bit samples to 8 (the pipeline has no
    // 16-bit RGB format), so the automatic choice is the 8-bit one; asking
    // for 10 bits works.
    let png = scratch("rgb48.png");
    write_png(&png, 16, false);
    encode_and_check("rgb48", &png, &[], PixelFormat::Yuv420p, true);
    encode_and_check(
        "rgb48_10",
        &png,
        &["-pix_fmt", "yuv420p10le"],
        PixelFormat::Yuv420p10,
        true,
    );
}
