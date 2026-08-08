//! **NMR (noise-to-mask ratio)** for AAC — the fast iteration metric.
//!
//! Per aligned analysis frame of original vs a codec's decoded output:
//! * `noise(sfb)` = `Σ |X_orig − X_coded|²` over the band's MDCT coefficients,
//! * `mask(sfb)`  = the encoder's own [`masking_thresholds`] on the original,
//! * `NMR(sfb)`   = noise / mask  (> 0 dB audible, < 0 dB inaudible).
//!
//! Unlike the MP3 lab's FFT-domain twin, this runs in the **MDCT domain with the
//! encoder's own SWB geometry**, so noise and mask share a band partition and an
//! energy domain exactly — no bin-remapping approximation.
//!
//! # What this metric is and is not
//!
//! It reuses OUR psy model, so it is biased toward what we optimize. Two rules
//! follow, both from `codec-tune-quality`:
//!
//! 1. **Compare relatively.** Ours-vs-anchor at matched bitrate; the shared bias
//!    largely cancels. An absolute NMR figure means little.
//! 2. **NMR is the screen, PEAQ is the verdict.** A self-metric provably flatters
//!    the encoder it came from. No rung in `docs/codec-aac-great-gate.md` is
//!    banked on NMR alone — it exists to make the inner loop fast, and the ladder
//!    (`super::ladder`) carries the external-oracle column beside it.
//!
//! The one place NMR is used *normatively* is the **null arm**: encoding a clip
//! twice with the same settings must produce an identical NMR, and scoring a
//! signal against itself must read as deeply inaudible. Those are correctness
//! properties of the harness, not quality claims, and they are asserted below.

use crate::encode::{analyze_long, masking_thresholds, FRAME_LEN};
use crate::swb::swb_offsets;

/// Aggregated NMR over a track.
#[derive(Debug, Clone)]
pub struct NmrReport {
    /// Analysis frames scored.
    pub frames: usize,
    /// Mean perceptual margin in dB over all (frame, band): `mean(10·log10(NMR))`.
    /// **Lower is better**; negative means coding noise sits below the mask.
    pub mean_nmr_db: f32,
    /// Worst single (frame, band) NMR, dB.
    pub max_nmr_db: f32,
    /// Fraction (%) of (frame, band) cells whose noise exceeds the mask.
    /// The best cross-encoder predictor in the MP3 calibration — read this first.
    pub pct_audible: f32,
    /// Mean NMR (dB) per scalefactor band — shows *where* an encoder is weak.
    pub per_band_db: Vec<f32>,
    /// Detected codec delay (samples) used to align the decoded output.
    pub delay: usize,
}

impl NmrReport {
    /// A single scalar for ranking: the audible percentage, tie-broken by mean.
    /// Lower is better.
    pub fn score(&self) -> f32 {
        self.pct_audible + self.mean_nmr_db * 1e-3
    }
}

/// Best alignment delay (samples) of `coded` against `orig`. AAC adds ~2048
/// samples of filterbank delay; a missed alignment manufactures enormous spurious
/// "noise", so this is load-bearing, not cosmetic.
fn best_delay(orig: &[f32], coded: &[f32]) -> usize {
    const MAXD: usize = 4096;
    let n = orig.len().min(coded.len());
    if n < MAXD + 8192 {
        return 0;
    }
    let w = (n - MAXD).min(60_000);
    let start = (n - MAXD - w) / 2;
    let mut best = (f64::INFINITY, 0usize);
    for d in 0..MAXD {
        let mut err = 0f64;
        let mut i = 0;
        while i < w {
            let e = (orig[start + i] - coded[start + d + i]) as f64;
            err += e * e;
            i += 32; // subsample the search
        }
        if err < best.0 {
            best = (err, d);
        }
    }
    best.1
}

/// Align `coded` to `orig` and aggregate per-frame, per-band NMR.
///
/// Both inputs are mono. `sample_rate` selects the SWB geometry, so it must be
/// the rate the audio is actually at.
pub fn track_nmr(orig: &[f32], coded: &[f32], sample_rate: u32) -> NmrReport {
    let fs_index = crate::sf_index_for_rate(sample_rate).unwrap_or(4);
    let swb = swb_offsets(true, fs_index);
    let nbands = swb.len() - 1;
    let win = crate::dsp::sine_window(2 * FRAME_LEN);
    let delay = best_delay(orig, coded);

    let mut sum_db = 0f64;
    let mut cells = 0u64;
    let mut audible = 0u64;
    let mut max_db = f32::NEG_INFINITY;
    let mut band_sum = vec![0f64; nbands];
    let mut band_cnt = vec![0u64; nbands];
    let mut frames = 0usize;

    // Walk long-block frames: each needs the previous 1024 plus the current 1024.
    let mut pos = FRAME_LEN; // skip one frame of filterbank warm-up
    while pos + FRAME_LEN <= orig.len() && pos + delay + FRAME_LEN <= coded.len() {
        let mut po = [0f32; FRAME_LEN];
        let mut co = [0f32; FRAME_LEN];
        let mut pc = [0f32; FRAME_LEN];
        let mut cc = [0f32; FRAME_LEN];
        po.copy_from_slice(&orig[pos - FRAME_LEN..pos]);
        co.copy_from_slice(&orig[pos..pos + FRAME_LEN]);
        pc.copy_from_slice(&coded[pos + delay - FRAME_LEN..pos + delay]);
        cc.copy_from_slice(&coded[pos + delay..pos + delay + FRAME_LEN]);

        let so = analyze_long(&po, &co, &win);
        let sc = analyze_long(&pc, &cc, &win);
        let mask = masking_thresholds(&so, swb, sample_rate);

        // Floor each band's mask relative to the loudest band's: a near-silent
        // band has a tiny mask and could otherwise manufacture a huge NMR. This
        // is the fix that made `max NMR` correlate with ODG at all in the MP3
        // calibration; the same artifact exists here.
        let mask_floor = mask.iter().copied().fold(0f64, f64::max) * 1e-5;

        for b in 0..nbands {
            let (s, e) = (swb[b] as usize, swb[b + 1] as usize);
            let mut noise = 1e-12f64;
            for k in s..e.min(so.len()) {
                let d = (so[k] - sc[k]) as f64;
                noise += d * d;
            }
            let v = noise / mask[b].max(mask_floor).max(1e-20);
            let db = (10.0 * v.log10()).clamp(-120.0, 120.0) as f32;
            sum_db += db as f64;
            cells += 1;
            if v > 1.0 {
                audible += 1;
            }
            max_db = max_db.max(db);
            band_sum[b] += db as f64;
            band_cnt[b] += 1;
        }
        frames += 1;
        pos += FRAME_LEN;
    }

    let cells_f = cells.max(1) as f64;
    let per_band_db = (0..nbands)
        .map(|b| (band_sum[b] / band_cnt[b].max(1) as f64) as f32)
        .collect();

    NmrReport {
        frames,
        mean_nmr_db: (sum_db / cells_f) as f32,
        max_nmr_db: if max_db.is_finite() { max_db } else { 0.0 },
        pct_audible: (audible as f64 / cells_f * 100.0) as f32,
        per_band_db,
        delay,
    }
}

/// Per-frame audible fraction (0..1) — the **per-unit** quantity a gate harvest
/// needs. [`track_nmr`]'s aggregate is a clip-level summary; the calculator fits
/// on units, and for an audio gate the unit is the frame.
///
/// Same alignment and same mask as [`track_nmr`], so a row here and the clip
/// total agree by construction.
pub fn per_frame_audible(orig: &[f32], coded: &[f32], sample_rate: u32) -> Vec<f32> {
    let fs_index = crate::sf_index_for_rate(sample_rate).unwrap_or(4);
    let swb = swb_offsets(true, fs_index);
    let nbands = swb.len() - 1;
    let win = crate::dsp::sine_window(2 * FRAME_LEN);
    let delay = best_delay(orig, coded);
    let mut out = Vec::new();

    let mut pos = FRAME_LEN;
    while pos + FRAME_LEN <= orig.len() && pos + delay + FRAME_LEN <= coded.len() {
        let mut po = [0f32; FRAME_LEN];
        let mut co = [0f32; FRAME_LEN];
        let mut pc = [0f32; FRAME_LEN];
        let mut cc = [0f32; FRAME_LEN];
        po.copy_from_slice(&orig[pos - FRAME_LEN..pos]);
        co.copy_from_slice(&orig[pos..pos + FRAME_LEN]);
        pc.copy_from_slice(&coded[pos + delay - FRAME_LEN..pos + delay]);
        cc.copy_from_slice(&coded[pos + delay..pos + delay + FRAME_LEN]);

        let so = analyze_long(&po, &co, &win);
        let sc = analyze_long(&pc, &cc, &win);
        let mask = masking_thresholds(&so, swb, sample_rate);
        let mask_floor = mask.iter().copied().fold(0f64, f64::max) * 1e-5;

        let mut audible = 0usize;
        for b in 0..nbands {
            let (s, e) = (swb[b] as usize, swb[b + 1] as usize);
            let mut noise = 1e-12f64;
            for k in s..e.min(so.len()) {
                let d = (so[k] - sc[k]) as f64;
                noise += d * d;
            }
            if noise / mask[b].max(mask_floor).max(1e-20) > 1.0 {
                audible += 1;
            }
        }
        out.push(audible as f32 / nbands as f32);
        pos += FRAME_LEN;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::corpus;

    /// A signal against itself has no coding noise: deeply masked, nothing
    /// audible. This is the metric's zero point — if it drifts, every ladder
    /// number above it is meaningless.
    #[test]
    fn identical_signal_is_inaudible() {
        for s in corpus::corpus() {
            let m = s.mono();
            let r = track_nmr(&m, &m, s.sample_rate);
            assert!(r.frames > 20, "{}: too few frames ({})", s.name(), r.frames);
            assert!(
                r.pct_audible < 0.1,
                "{}: identical → ~0% audible, got {:.3}%",
                s.name(),
                r.pct_audible
            );
            assert!(
                r.mean_nmr_db < -40.0,
                "{}: identical → deeply masked, got {:.1} dB",
                s.name(),
                r.mean_nmr_db
            );
        }
    }

    /// Added broadband noise must raise NMR and the audible fraction — the
    /// metric has to move in the right direction to be a metric at all.
    #[test]
    fn added_noise_raises_nmr() {
        let s = corpus::signal(corpus::Class::MusicTonal);
        let clean = s.mono();
        let mut seed = 0x1234_5678u32;
        let noisy: Vec<f32> = clean
            .iter()
            .map(|&x| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                x + 0.01 * (((seed >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0)
            })
            .collect();
        let base = track_nmr(&clean, &clean, s.sample_rate);
        let dirty = track_nmr(&clean, &noisy, s.sample_rate);
        assert!(
            dirty.mean_nmr_db > base.mean_nmr_db + 10.0,
            "noise must raise NMR: {:.1} → {:.1}",
            base.mean_nmr_db,
            dirty.mean_nmr_db
        );
        assert!(dirty.pct_audible > base.pct_audible);
    }

    /// More noise must score worse than less noise — monotonicity. A metric that
    /// is not monotone in the thing it measures cannot rank two encoders.
    #[test]
    fn nmr_is_monotone_in_noise() {
        let s = corpus::signal(corpus::Class::SpeechClean);
        let clean = s.mono();
        let mk = |amp: f32| -> Vec<f32> {
            let mut seed = 0xACE1_2345u32;
            clean
                .iter()
                .map(|&x| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    x + amp * (((seed >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0)
                })
                .collect()
        };
        let a = track_nmr(&clean, &mk(0.002), s.sample_rate);
        let b = track_nmr(&clean, &mk(0.010), s.sample_rate);
        assert!(
            b.mean_nmr_db > a.mean_nmr_db,
            "5x noise must score worse: {:.1} vs {:.1}",
            a.mean_nmr_db,
            b.mean_nmr_db
        );
    }

    /// Alignment must find a delay we inject deliberately. A missed alignment is
    /// the failure mode that silently invalidates a whole ladder.
    #[test]
    fn alignment_recovers_injected_delay() {
        let s = corpus::signal(corpus::Class::MusicTonal);
        let m = s.mono();
        const D: usize = 2048;
        let mut shifted = vec![0f32; D];
        shifted.extend_from_slice(&m);
        let r = track_nmr(&m, &shifted, s.sample_rate);
        assert_eq!(r.delay, D, "expected to recover a {D}-sample delay");
        assert!(
            r.pct_audible < 0.1,
            "a pure delay is not distortion once aligned, got {:.3}%",
            r.pct_audible
        );
    }
}
