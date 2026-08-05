//! Price the two optimized-Huffman routes against each other.
//!
//! Both produce the same optimized tables. They differ in what they write:
//! the block-materializing route emits one scan PER COMPONENT (non-interleaved),
//! the streaming route emits a single interleaved scan like every mainstream
//! encoder. Until this was measured the choice between them was made purely on
//! a memory budget, so the scan layout of our output depended on the resolution.
//!
//! Usage: scanmode [width] [height] [quality] [reps]

use rusty_jpeg::encode::{ColorType, Encoder, PlanarYcbcrImage, SamplingFactor};
use std::time::Instant;

fn fbm(x: usize, y: usize, amp: f32) -> f32 {
    let mut v = 0.0;
    let mut a = amp;
    let mut f = 1.0f32;
    for _ in 0..5 {
        let xs = x as f32 * f * 0.021;
        let ys = y as f32 * f * 0.021;
        v += a * (xs.sin() * ys.cos() + (xs * 1.7 + ys * 0.9).sin() * 0.6);
        a *= 0.5;
        f *= 2.0;
    }
    v
}

fn encode(rgb: &[u8], w: usize, h: usize, q: u8, streaming: Option<bool>) -> Vec<u8> {
    let mut jpeg = Vec::new();
    let mut enc = Encoder::new(&mut jpeg, q);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(true);
    if let Some(s) = streaming {
        enc.set_streaming_optimize(s);
    }
    enc.encode(rgb, w as u16, h as u16, ColorType::Rgb)
        .expect("encode");
    jpeg
}

/// Number of components in the first SOS: >1 means interleaved.
fn first_sos_components(d: &[u8]) -> usize {
    let mut i = 2;
    while i < d.len() - 1 {
        if d[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = d[i + 1];
        if m == 0xD8 || m == 0xD9 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        let ln = ((d[i + 2] as usize) << 8) | d[i + 3] as usize;
        if m == 0xDA {
            return d[i + 4] as usize;
        }
        i += 2 + ln;
    }
    0
}

fn main() {
    let dump_counts = std::env::var("RUSTY_JPEG_COUNTS").is_ok();
    let mut a = std::env::args().skip(1);
    let w: usize = a.next().and_then(|v| v.parse().ok()).unwrap_or(1920);
    let h: usize = a.next().and_then(|v| v.parse().ok()).unwrap_or(1080);
    let q: u8 = a.next().and_then(|v| v.parse().ok()).unwrap_or(90);
    let reps: usize = a.next().and_then(|v| v.parse().ok()).unwrap_or(20);

    let mut rgb = vec![0u8; w * h * 3];
    for j in 0..h {
        for i in 0..w {
            let (fi, fj) = (i as f32 / w as f32, j as f32 / h as f32);
            let base = 96.0 + 90.0 * (fi * 2.3 + fj * 1.7).sin() * (fj * 1.9).cos();
            let lum = (base + fbm(i, j, 70.0)).clamp(0.0, 255.0);
            let o = (j * w + i) * 3;
            rgb[o] = (lum + 26.0 * (fi * 3.1).sin()).clamp(0.0, 255.0) as u8;
            rgb[o + 1] = lum as u8;
            rgb[o + 2] = (lum + 22.0 * (fj * 2.7).cos()).clamp(0.0, 255.0) as u8;
        }
    }

    if dump_counts {
        rusty_jpeg::prof::reset();
    }
    for (label, streaming) in [("materialize", Some(false)), ("streaming", Some(true))] {
        let out = encode(&rgb, w, h, q, streaming);
        let comps = first_sos_components(&out);
        let mut best = f64::INFINITY;
        for _ in 0..reps {
            let t = Instant::now();
            let o = encode(&rgb, w, h, q, streaming);
            std::hint::black_box(&o);
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        println!(
            "{label:12} {:>9} B  first SOS = {comps} comp(s) [{}]  best {best:8.1} ms",
            out.len(),
            if comps > 1 {
                "interleaved"
            } else {
                "NON-INTERLEAVED"
            },
        );
    }

    // The CLI feeds yuv420p planes through `PlanarYcbcrImage`, not RGB. Time
    // that path too: a gap between them is CLI-path cost, not encoder cost.
    {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut yp = vec![0u8; w * h];
        let mut cb = vec![128u8; cw * ch];
        let mut cr = vec![128u8; cw * ch];
        for j in 0..h {
            for i in 0..w {
                yp[j * w + i] = rgb[(j * w + i) * 3 + 1];
            }
        }
        for j in 0..ch {
            for i in 0..cw {
                cb[j * cw + i] = rgb[((j * 2).min(h - 1) * w + (i * 2).min(w - 1)) * 3];
                cr[j * cw + i] = rgb[((j * 2).min(h - 1) * w + (i * 2).min(w - 1)) * 3 + 2];
            }
        }
        let mut best = f64::INFINITY;
        let mut bytes = 0usize;
        for _ in 0..reps.max(1) {
            let t = Instant::now();
            let mut out = Vec::new();
            {
                let img =
                    PlanarYcbcrImage::new(&yp, &cb, &cr, [w, cw, cw], w as u16, h as u16, (2, 2))
                        .expect("planar image");
                let mut enc = Encoder::new(&mut out, q);
                enc.set_sampling_factor(img.sampling_factor());
                enc.set_optimized_huffman_tables(true);
                enc.encode_image(img).expect("planar encode");
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
            bytes = out.len();
        }
        println!(
            "planar (CLI path) {:>9} B  yuv420p in                       best {:8.1} ms",
            bytes, best
        );
    }
}
