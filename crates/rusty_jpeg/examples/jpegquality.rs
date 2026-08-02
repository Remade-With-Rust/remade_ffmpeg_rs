//! Rate-distortion harness: size and PSNR across a quality ladder, per content.
//!
//! The gate for any bitstream-changing encoder work. `codec-measurement` §0:
//! such a change is gated by a **corpus BD-rate over ≥4 quality points plus a
//! decoder round-trip**, never a single operating point.
//!
//! # The measurement trap this is built to avoid
//!
//! PSNR must be computed on the **reconstructed full-resolution RGB** against
//! the original — never on the subsampled chroma planes. A chroma downsampler
//! that point-samples reproduces exactly the samples it kept, so scoring it on
//! its own subsampled output makes it look *perfect*. The error it actually
//! commits is aliasing, and aliasing is only visible after upsampling back to
//! full resolution. Score the wrong thing and the defect is invisible.
//!
//! # Content
//!
//! Three kinds, because chroma error is content-dependent (§9). Smooth
//! photographic content hides downsampling error; saturated chroma edges — red
//! text, logos, flags — are where it shows. Reporting only the first would
//! understate the defect by design.
//!
//! Run: `cargo run --release -p rusty_jpeg --example jpegquality`

use rusty_jpeg::decode::Decoder;
use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
use std::io::Cursor;

const W: usize = 512;
const H: usize = 512;
const QUALITIES: [u8; 5] = [50, 70, 80, 90, 95];

fn fbm(x: usize, y: usize, amp: f32) -> f32 {
    let (mut v, mut a, mut f) = (0.0f32, amp, 1.0f32);
    for _ in 0..5 {
        let (xs, ys) = (x as f32 * f * 0.021, y as f32 * f * 0.021);
        v += a * (xs.sin() * ys.cos() + (xs * 1.7 + ys * 0.9).sin() * 0.6);
        a *= 0.5;
        f *= 2.0;
    }
    v
}

/// Smooth photographic content — the easy case for a chroma downsampler.
fn content_photo() -> Vec<u8> {
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let (fx, fy) = (x as f32 / W as f32, y as f32 / H as f32);
            let base = 96.0 + 90.0 * (fx * 2.3 + fy * 1.7).sin() * (fy * 1.9).cos();
            let lum = (base + fbm(x, y, 70.0)).clamp(0.0, 255.0);
            let o = (y * W + x) * 3;
            rgb[o] = (lum + 26.0 * (fx * 3.1).sin()).clamp(0.0, 255.0) as u8;
            rgb[o + 1] = lum as u8;
            rgb[o + 2] = (lum + 22.0 * (fy * 2.7).cos()).clamp(0.0, 255.0) as u8;
        }
    }
    rgb
}

/// Saturated chroma edges at luma-neutral transitions — red/cyan and
/// green/magenta bars. Luma barely moves across these edges, so essentially all
/// the signal is chroma, and a point-sampling downsampler aliases it badly.
/// This is the "red text on a coloured background" case.
fn content_chroma_edges() -> Vec<u8> {
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let o = (y * W + x) * 3;
            // Vertical bars a few pixels wide: right at the chroma Nyquist.
            let bar = (x / 3) % 2 == 0;
            let band = (y / 64) % 2 == 0;
            let (r, g, b) = match (bar, band) {
                (true, true) => (200, 60, 60),
                (false, true) => (60, 200, 200),
                (true, false) => (60, 200, 60),
                (false, false) => (200, 60, 200),
            };
            rgb[o] = r;
            rgb[o + 1] = g;
            rgb[o + 2] = b;
        }
    }
    rgb
}

/// A hard diagonal chroma edge — the orientation box-averaging handles worst
/// and point-sampling handles worst of all.
fn content_diagonal() -> Vec<u8> {
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let o = (y * W + x) * 3;
            let side = (x + y) % 24 < 12;
            let (r, g, b) = if side { (220, 40, 90) } else { (40, 160, 220) };
            rgb[o] = r;
            rgb[o + 1] = g;
            rgb[o + 2] = b;
        }
    }
    rgb
}

/// Sharp bilevel glyph-like blocks — the "text and line art" case, where
/// coefficients are large and lowering one is expensive.
fn content_text() -> Vec<u8> {
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let o = (y * W + x) * 3;
            let gx = (x / 4) % 3;
            let gy = (y / 6) % 4;
            let ink = (gx == 0 && gy != 3) || (gy == 0 && gx != 2) || (x / 4 + y / 6) % 11 == 0;
            let v = if ink { 20u8 } else { 235 };
            rgb[o] = v;
            rgb[o + 1] = v;
            rgb[o + 2] = v;
        }
    }
    rgb
}

/// Near-white-noise luma: high entropy, coefficients spread across the whole
/// block, so EOB placement has little to work with.
fn content_noise() -> Vec<u8> {
    let mut rgb = vec![0u8; W * H * 3];
    let mut s = 0x243F_6A88_85A3_08D3u64;
    for p in rgb.chunks_exact_mut(3) {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let v = (s >> 24) as u8;
        p[0] = v;
        p[1] = v.wrapping_add(11);
        p[2] = v.wrapping_sub(7);
    }
    rgb
}

/// A very smooth gradient — almost all energy in DC and the first few AC terms.
fn content_gradient() -> Vec<u8> {
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let o = (y * W + x) * 3;
            rgb[o] = (x * 255 / W) as u8;
            rgb[o + 1] = (y * 255 / H) as u8;
            rgb[o + 2] = ((x + y) * 255 / (W + H)) as u8;
        }
    }
    rgb
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse <= 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

/// PSNR of one interleaved RGB channel, so chroma damage is not averaged away
/// behind a luma channel that the codec reproduces well.
fn psnr_channel(a: &[u8], b: &[u8], ch: usize) -> f64 {
    let av: Vec<u8> = a.iter().skip(ch).step_by(3).copied().collect();
    let bv: Vec<u8> = b.iter().skip(ch).step_by(3).copied().collect();
    psnr(&av, &bv)
}

fn encode(rgb: &[u8], quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, quality);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    // Trellis is ON by default in the encoder now, so the baseline arm must
    // switch it OFF explicitly. Relying on the default made both arms identical
    // and every BD-rate read exactly 0.00% -- the tell that the comparison was
    // measuring one binary against itself.
    match std::env::var("RUSTY_JPEG_ARM").as_deref() {
        Ok("notrellis") => enc.set_trellis(false),
        Ok("trellis") => enc.set_trellis(true),
        _ => {}
    }
    enc.encode(rgb, W as u16, H as u16, ColorType::Rgb)
        .expect("encode");
    out
}

/// Bjontegaard-style average bitrate delta, trapezoid over log-rate vs PSNR.
/// Negative = the second curve needs FEWER bits for the same quality.
fn bd_rate(base: &[(f64, f64)], test: &[(f64, f64)]) -> f64 {
    let lo = base
        .iter()
        .map(|p| p.1)
        .fold(f64::INFINITY, f64::min)
        .max(test.iter().map(|p| p.1).fold(f64::INFINITY, f64::min));
    let hi = base
        .iter()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max)
        .min(test.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max));
    // Deliberately negated rather than `hi <= lo`: this must also bail when
    // either bound is NaN, which the positive form would not.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(hi > lo) {
        return f64::NAN;
    }
    let interp = |c: &[(f64, f64)], q: f64| -> f64 {
        for w in c.windows(2) {
            let ((r0, q0), (r1, q1)) = (w[0], w[1]);
            if (q0 <= q && q <= q1) || (q1 <= q && q <= q0) {
                if (q1 - q0).abs() < 1e-12 {
                    return r0.ln();
                }
                let t = (q - q0) / (q1 - q0);
                return r0.ln() + t * (r1.ln() - r0.ln());
            }
        }
        f64::NAN
    };
    let n = 64;
    let (mut sa, mut sb) = (0.0, 0.0);
    for i in 0..=n {
        let q = lo + (hi - lo) * i as f64 / n as f64;
        let (a, b) = (interp(base, q), interp(test, q));
        if a.is_nan() || b.is_nan() {
            return f64::NAN;
        }
        let w = if i == 0 || i == n { 0.5 } else { 1.0 };
        sa += w * a;
        sb += w * b;
    }
    ((sb - sa) / n as f64).exp_m1() * 100.0
}

fn main() {
    let arm = std::env::var("RUSTY_JPEG_ARM").unwrap_or_else(|_| "current".into());
    println!("arm: {arm}   {W}x{H}  4:2:0   qualities {QUALITIES:?}");
    println!(
        "\nPSNR is measured on the RECONSTRUCTED RGB against the original -- not on\n\
         the subsampled chroma planes, where a point-sampling downsampler would\n\
         score perfectly by reproducing exactly the samples it chose to keep.\n"
    );

    for (name, src) in [
        ("photo", content_photo()),
        ("chroma_edges", content_chroma_edges()),
        ("diagonal", content_diagonal()),
        ("text", content_text()),
        ("noise", content_noise()),
        ("gradient", content_gradient()),
    ] {
        println!("--- {name} ---");
        println!(
            "{:>3}  {:>9}  {:>8}  {:>8}  {:>8}  {:>8}",
            "q", "bytes", "PSNR", "R", "G", "B"
        );
        let mut curve = Vec::new();
        for q in QUALITIES {
            let jpg = encode(&src, q);
            let mut d = Decoder::new(Cursor::new(&jpg));
            let out = d.decode().expect("decode");
            let p = psnr(&src, &out);
            curve.push((jpg.len() as f64, p));
            println!(
                "{q:>3}  {:>9}  {p:>8.3}  {:>8.3}  {:>8.3}  {:>8.3}",
                jpg.len(),
                psnr_channel(&src, &out, 0),
                psnr_channel(&src, &out, 1),
                psnr_channel(&src, &out, 2),
            );
        }
        // Machine-readable, so an A/B script can diff two arms without parsing
        // the table above.
        print!("CURVE {name}");
        for (r, q) in &curve {
            print!(" {r:.0},{q:.4}");
        }
        println!("\n");
    }
    let _ = bd_rate(&[(1.0, 1.0)], &[(1.0, 1.0)]);
}
