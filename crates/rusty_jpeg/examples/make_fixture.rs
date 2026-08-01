//! Write a **photographic** JPEG fixture to disk.
//!
//! ```text
//! make_fixture <out.jpg> [width] [height] [quality]
//! ```
//!
//! Decoder benchmarking needs content with a realistic coefficient
//! distribution, and the synthetic clips lying around this repo are useless for
//! it: `testsrc2` decodes 92.6% DC-only blocks and noise decodes 0.0%. A
//! photograph sits between those, and the difference is not cosmetic — a
//! DC-only block skips the entire 8x8 inverse transform, so the two extremes
//! measure two different decoders.
//!
//! The source is multi-scale 1/f (fractal) detail, which is the spectrum
//! natural images actually have, at the amplitude calibrated in
//! `calibrate_photo_amplitude` to land near 11.5x whole-frame compression.

use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};

/// Deterministic value-noise fBm: octaves at halving amplitude, doubling
/// frequency.
fn fbm(x: usize, y: usize, amplitude: f32) -> f32 {
    fn hash(xi: i32, yi: i32) -> f32 {
        let mut h = (xi as u32).wrapping_mul(0x9E37_79B9) ^ (yi as u32).wrapping_mul(0x85EB_CA6B);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2545_F491);
        h ^= h >> 13;
        (h >> 8) as f32 / 8_388_608.0 - 1.0
    }
    fn value_noise(fx: f32, fy: f32) -> f32 {
        let (x0, y0) = (fx.floor() as i32, fy.floor() as i32);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
        let a = hash(x0, y0) + (hash(x0 + 1, y0) - hash(x0, y0)) * sx;
        let b = hash(x0, y0 + 1) + (hash(x0 + 1, y0 + 1) - hash(x0, y0 + 1)) * sx;
        a + (b - a) * sy
    }
    let (mut sum, mut amp, mut freq) = (0.0, amplitude, 1.0 / 64.0);
    for _ in 0..6 {
        sum += amp * value_noise(x as f32 * freq, y as f32 * freq);
        amp *= 0.5;
        freq *= 2.0;
    }
    sum
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .expect("usage: make_fixture <out.jpg> [w] [h] [q]");
    let w: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(1920);
    let h: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(1080);
    let quality: u8 = args.next().and_then(|v| v.parse().ok()).unwrap_or(90);
    // `interleaved` forces the streaming optimize route. Without it the encoder
    // picks the route on a MEMORY budget, which at every practical resolution
    // lands on the block-materializing one -- and that writes one scan PER
    // COMPONENT. A fixture built that way exercises the decoder's rare
    // non-interleaved path, which is not what real JPEGs look like.
    let interleaved: bool = args.next().map(|v| v == "interleaved").unwrap_or(false);

    let mut rgb = vec![0u8; w * h * 3];
    for j in 0..h {
        for i in 0..w {
            let (fi, fj) = (i as f32 / w as f32, j as f32 / h as f32);
            // Broad lighting plus soft blobs: the part a DCT codes almost free.
            let base = 96.0 + 90.0 * (fi * 2.3 + fj * 1.7).sin() * (fj * 1.9).cos();
            let lum = (base + fbm(i, j, 70.0)).clamp(0.0, 255.0);
            // Give the chroma planes their own structure so they are not flat.
            let o = (j * w + i) * 3;
            rgb[o] = (lum + 26.0 * (fi * 3.1).sin()).clamp(0.0, 255.0) as u8;
            rgb[o + 1] = lum as u8;
            rgb[o + 2] = (lum + 22.0 * (fj * 2.7).cos()).clamp(0.0, 255.0) as u8;
        }
    }

    let mut jpeg = Vec::new();
    let mut enc = Encoder::new(&mut jpeg, quality);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(true);
    if std::env::var("RUSTY_JPEG_TRELLIS").is_ok() {
        enc.set_trellis(true);
    }
    if interleaved {
        enc.set_streaming_optimize(true);
    }
    enc.encode(&rgb, w as u16, h as u16, ColorType::Rgb)
        .expect("encode");

    let ratio = (w * h * 3 / 2) as f64 / jpeg.len() as f64;
    std::fs::write(&out, &jpeg).expect("write");
    println!(
        "{out}: {w}x{h} q{quality}, {} B, {ratio:.1}x compression",
        jpeg.len()
    );
}
