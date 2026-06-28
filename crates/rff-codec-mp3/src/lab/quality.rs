//! **NMR (noise-to-mask ratio)** — a perceptual quality metric that reuses the
//! encoder's own psychoacoustic masking threshold to score coding noise the way the
//! ear would. SNR can't tell a good MP3 encoder from a bad one (both spend bits, but
//! a good one hides noise under the mask); NMR can. See
//! `docs/mp3-quality-harness-plan.md`.
//!
//! Per aligned analysis frame of original vs a codec's decoded output:
//! * noise(band) = `Σ |X_orig − X_coded|²` over the band's FFT bins,
//! * mask(band)  = `psychoacoustic::analyze(orig).thresholds[band]`,
//! * NMR(band)   = noise / mask  (> 0 dB audible, < 0 dB inaudible).
//!
//! Same Hann window + 1024-pt FFT as the psymodel, so noise and mask share an energy
//! domain. The metric reuses OUR psymodel (so it's biased toward what we optimize) —
//! hence always compare codecs **relatively** (ours vs LAME); shared bias cancels.

use std::f32::consts::PI;

use crate::encode::{fft, psychoacoustic};
use crate::frame::SFB_LONG;
use crate::tables;

const N_FFT: usize = 1024; // match the psymodel
const HOP: usize = 512;

/// Aggregated NMR over a track.
#[derive(Debug, Clone)]
pub struct NmrReport {
    pub frames: usize,
    /// Mean perceptual margin in dB over all (frame, band): `mean(10·log10(NMR))`.
    /// Lower is better; negative means coding noise sits, on average, below the mask.
    pub mean_nmr_db: f32,
    /// Worst single (frame, band) NMR, dB.
    pub max_nmr_db: f32,
    /// Fraction (%) of (frame, band) cells whose noise exceeds the mask (NMR > 0 dB).
    pub pct_audible: f32,
    /// Mean NMR (dB) per scalefactor band — shows *where* a codec is weak.
    pub per_band_db: [f32; SFB_LONG],
    /// Detected codec delay (samples) used to align the decoded output.
    pub delay: usize,
}

fn hann() -> [f32; N_FFT] {
    let mut w = [0f32; N_FFT];
    for (n, wn) in w.iter_mut().enumerate() {
        *wn = 0.5 * (1.0 - (2.0 * PI * n as f32 / N_FFT as f32).cos());
    }
    w
}

/// FFT-bin range `[lo, hi)` per scalefactor band — the same mapping the psymodel
/// uses (`bin ≈ line · 1024/1152`), so band noise and band mask agree.
fn band_bins(sample_rate: u32) -> [(usize, usize); SFB_LONG] {
    let sfb = tables::sfb_long_offsets(sample_rate);
    let bpl = N_FFT as f32 / 1152.0;
    let mut bins = [(0usize, 0usize); SFB_LONG];
    for (b, slot) in bins.iter_mut().enumerate() {
        let lo = (sfb[b] as f32 * bpl).round() as usize;
        let hi = ((sfb[b + 1] as f32 * bpl).round() as usize).min(N_FFT / 2 + 1);
        *slot = (lo, hi.max(lo));
    }
    bins
}

/// Per-band NMR (linear) for one frame. `orig`/`coded` are `N_FFT` samples.
fn frame_nmr(
    orig: &[f32],
    coded: &[f32],
    sample_rate: u32,
    win: &[f32; N_FFT],
    bins: &[(usize, usize); SFB_LONG],
) -> [f32; SFB_LONG] {
    // Mask = our psymodel's threshold for the original (it windows + FFTs internally).
    let mask = psychoacoustic::analyze(orig, sample_rate).thresholds;
    // Noise = power spectrum of the coding error, same window + FFT as the psymodel.
    let mut ro = [0f32; N_FFT];
    let mut io = [0f32; N_FFT];
    let mut rc = [0f32; N_FFT];
    let mut ic = [0f32; N_FFT];
    for i in 0..N_FFT {
        ro[i] = orig[i] * win[i];
        rc[i] = coded[i] * win[i];
    }
    fft::fft(&mut ro, &mut io);
    fft::fft(&mut rc, &mut ic);
    let mut nmr = [0f32; SFB_LONG];
    for b in 0..SFB_LONG {
        let (lo, hi) = bins[b];
        let mut noise = 1e-12f32;
        for k in lo..hi {
            let dr = ro[k] - rc[k];
            let di = io[k] - ic[k];
            noise += dr * dr + di * di;
        }
        nmr[b] = noise / mask[b].max(1e-20);
    }
    nmr
}

/// Best alignment delay (samples) of `coded` against `orig`: the shift minimising
/// squared error over a mid-signal window (the codec adds ~1100+ samples of delay).
fn best_delay(orig: &[f32], coded: &[f32]) -> usize {
    const MAXD: usize = 3000;
    let n = orig.len().min(coded.len());
    if n < MAXD + 8000 {
        return 0; // too short to align reliably
    }
    // Compare a centred window; adapt its length to short clips so alignment still
    // runs (a missed alignment manufactures huge spurious "noise").
    let w = (n - MAXD).min(80_000);
    let start = (n - MAXD - w) / 2;
    let mut best = (f64::INFINITY, 0usize);
    for d in 0..MAXD {
        let mut err = 0f64;
        let mut i = 0;
        while i < w {
            let e = (orig[start + i] - coded[start + d + i]) as f64;
            err += e * e;
            i += 64; // subsample the search for speed
        }
        if err < best.0 {
            best = (err, d);
        }
    }
    best.1
}

/// Align `coded` to `orig` and aggregate per-frame NMR into a report.
pub fn track_nmr(orig: &[f32], coded: &[f32], sample_rate: u32) -> NmrReport {
    let win = hann();
    let bins = band_bins(sample_rate);
    let delay = best_delay(orig, coded);

    let mut sum_db = 0f64;
    let mut cells = 0u64;
    let mut audible = 0u64;
    let mut max_db = f32::NEG_INFINITY;
    let mut band_sum = [0f64; SFB_LONG];
    let mut band_cnt = [0u64; SFB_LONG];

    let mut pos = N_FFT; // skip one frame of warm-up
    while pos + N_FFT <= orig.len() && pos + delay + N_FFT <= coded.len() {
        let o = &orig[pos..pos + N_FFT];
        let c = &coded[pos + delay..pos + delay + N_FFT];
        let nmr = frame_nmr(o, c, sample_rate, &win, &bins);
        for (b, &v) in nmr.iter().enumerate() {
            let db = (10.0 * v.log10()).clamp(-120.0, 120.0);
            sum_db += db as f64;
            cells += 1;
            if v > 1.0 {
                audible += 1;
            }
            max_db = max_db.max(db);
            band_sum[b] += db as f64;
            band_cnt[b] += 1;
        }
        pos += HOP;
    }

    let cells_f = cells.max(1) as f64;
    let mut per_band_db = [0f32; SFB_LONG];
    for b in 0..SFB_LONG {
        per_band_db[b] = (band_sum[b] / band_cnt[b].max(1) as f64) as f32;
    }
    NmrReport {
        frames: (cells / SFB_LONG as u64) as usize,
        mean_nmr_db: (sum_db / cells_f) as f32,
        max_nmr_db: if max_db.is_finite() { max_db } else { 0.0 },
        pct_audible: (audible as f64 / cells_f * 100.0) as f32,
        per_band_db,
        delay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identical signals → zero coding noise → deeply negative NMR, nothing audible.
    #[test]
    fn identical_signal_has_no_audible_noise() {
        let sr = 44100;
        let sig: Vec<f32> = (0..40_000)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.3 * (2.0 * PI * 440.0 * t).sin() + 0.2 * (2.0 * PI * 2500.0 * t).sin()
            })
            .collect();
        let r = track_nmr(&sig, &sig, sr);
        assert!(r.frames > 10);
        assert!(
            r.pct_audible < 0.1,
            "identical → ~0% audible, got {}",
            r.pct_audible
        );
        assert!(
            r.mean_nmr_db < -20.0,
            "identical → deeply masked, got {}",
            r.mean_nmr_db
        );
    }

    /// Adding broadband noise above the mask must raise NMR and the audible %.
    #[test]
    fn added_noise_raises_nmr() {
        let sr = 44100;
        let clean: Vec<f32> = (0..40_000)
            .map(|i| 0.4 * (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        let mut s = 12345u32;
        let noisy: Vec<f32> = clean
            .iter()
            .map(|&x| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                x + 0.05 * ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
            })
            .collect();
        let clean_r = track_nmr(&clean, &clean, sr);
        let noisy_r = track_nmr(&clean, &noisy, sr);
        assert!(
            noisy_r.mean_nmr_db > clean_r.mean_nmr_db + 10.0,
            "noise must raise NMR: clean {} vs noisy {}",
            clean_r.mean_nmr_db,
            noisy_r.mean_nmr_db
        );
        assert!(noisy_r.pct_audible > clean_r.pct_audible);
    }
}
