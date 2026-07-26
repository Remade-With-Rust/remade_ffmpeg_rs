//! Quality measurement, computed IN-PROCESS, frame index against frame index.
//!
//! Both encoders' output is decoded by the SAME external ffmpeg, so neither side
//! is scored by its own reconstruction — but the metric itself is computed here.
//!
//! Why not ffmpeg's `psnr`/`ssim` filters: they pair the two inputs through
//! framesync, i.e. by TIMESTAMP. Our IVF time base and the y4m's frame rate can
//! disagree (29.97 vs an assumed 25), which silently misaligns the streams and
//! reports a large, entirely fictitious quality loss. Indexing frame `i` against
//! frame `i` has no such failure mode.

use crate::y4m::Clip;
use std::path::Path;

fn psnr_from_mse(mse: f64) -> f64 {
    if mse <= 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// Mean SSIM over 8x8 non-overlapping luma windows (Wang et al. constants, L=255).
fn ssim_y(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let (mut acc, mut cnt) = (0f64, 0u64);
    let mut by = 0;
    while by + 8 <= h {
        let mut bx = 0;
        while bx + 8 <= w {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for y in 0..8 {
                for x in 0..8 {
                    let (pa, pb) = (
                        a[(by + y) * w + bx + x] as f64,
                        b[(by + y) * w + bx + x] as f64,
                    );
                    sa += pa;
                    sb += pb;
                    saa += pa * pa;
                    sbb += pb * pb;
                    sab += pa * pb;
                }
            }
            let n = 64.0;
            let (ma, mb) = (sa / n, sb / n);
            let (va, vb) = (saa / n - ma * ma, sbb / n - mb * mb);
            let cov = sab / n - ma * mb;
            acc += ((2.0 * ma * mb + C1) * (2.0 * cov + C2))
                / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            cnt += 1;
            bx += 8;
        }
        by += 8;
    }
    acc / cnt.max(1) as f64
}

/// (mean per-frame luma PSNR in dB, mean luma SSIM) of `stream` against the
/// first `frames` frames of `src`.
///
/// Returns `None` if the stream fails to decode or the frame count disagrees —
/// a silent frame-count mismatch would quietly corrupt the metric rather than
/// fail loudly.
pub fn measure(stream: &Path, src: &Clip, frames: usize) -> Option<(f64, f64)> {
    let raw = crate::ffmpeg::decode_to_yuv(stream)?;
    let (w, h) = (src.width, src.height);
    let (ys, cs) = (w * h, w.div_ceil(2) * h.div_ceil(2));
    let fsz = ys + 2 * cs;
    let n = raw.len() / fsz;
    if n == 0 || n != frames {
        eprintln!("    ! decoded {n} frames, expected {frames} — quality skipped");
        return None;
    }
    let (mut psnr, mut ssim) = (0.0, 0.0);
    for (i, f) in src.frames.iter().take(frames).enumerate() {
        let dec = &raw[i * fsz..i * fsz + ys];
        let mse = dec
            .iter()
            .zip(&f.y)
            .map(|(&a, &b)| {
                let d = a as f64 - b as f64;
                d * d
            })
            .sum::<f64>()
            / ys as f64;
        psnr += psnr_from_mse(mse);
        ssim += ssim_y(dec, &f.y, w, h);
    }
    Some((psnr / n as f64, ssim / n as f64))
}
