//! Psychoacoustic model — the quality brain (bricks **Q1–Q4**).
//!
//! Estimates, per scalefactor band, how much quantization noise the ear will not
//! hear (the masking threshold). The quantizer's distortion loop (Q6) then shapes
//! noise to sit under it. All constants are computed from the **published**
//! psychoacoustics formulas — Terhardt's absolute threshold of hearing, Zwicker's
//! Bark scale, Schroeder's spreading function — not copied from any encoder.
//!
//! This is a first, untuned model: long blocks only (Q5 block-switching is
//! deferred), a fixed signal-to-mask offset, and the FFT power spectrum mapped to
//! the MDCT scalefactor bands by the `1024/1152` frequency-grid ratio. It captures
//! the dominant effect — noise tolerance proportional to per-band masking, spread
//! across neighbouring bands — which is what the distortion loop needs.

use std::sync::OnceLock;

use crate::frame::{BlockType, SFB_LONG};
use crate::tables;

use super::fft;

/// Psymodel FFT size (Q2). 1024-point, like LAME's long-block analysis.
const N_FFT: usize = 1024;
/// Signal-to-mask offset (dB): the masking energy is lowered by this to get the
/// just-masked noise threshold. A fixed value — tonality-dependent variation (Floor
/// 1A) was MEASURED to give zero corpus-ODG change at CBR (the rate loop dominates
/// the allocation, not the threshold shape), so it was reverted. The lever for these
/// clips was the block-type decision (attack detector), not the threshold values.
const SMR_OFFSET_DB: f32 = 12.0;

/// Per-granule perceptual analysis result.
#[derive(Debug, Clone, Default)]
pub struct PsyResult {
    /// Chosen window sequence for this granule.
    pub block_type: BlockType,
    /// Masking threshold (allowed noise energy) per long-block scalefactor band.
    pub thresholds: [f32; SFB_LONG],
    /// Perceptual entropy — the rough bit demand, used by reservoir budgeting.
    pub perceptual_entropy: f32,
}

/// Hann analysis window, `0.5·(1 − cos(2πn/N))`.
fn hann() -> &'static [f32; N_FFT] {
    static W: OnceLock<[f32; N_FFT]> = OnceLock::new();
    W.get_or_init(|| {
        let mut w = [0f32; N_FFT];
        for (n, wn) in w.iter_mut().enumerate() {
            *wn = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * n as f32 / N_FFT as f32).cos());
        }
        w
    })
}

/// Zwicker's critical-band rate (Bark) for a frequency in Hz.
fn bark(f: f32) -> f32 {
    13.0 * (0.000_76 * f).atan() + 3.5 * (f / 7500.0).powi(2).atan()
}

/// Schroeder's spreading function (dB) at a Bark distance `dz = bark_maskee −
/// bark_masker`. Peak ≈ 0 dB at the masker; asymmetric (spreads further upward).
fn spreading_db(dz: f32) -> f32 {
    let x = dz + 0.474;
    15.81 + 7.5 * x - 17.5 * (1.0 + x * x).sqrt()
}

/// Terhardt's absolute threshold of hearing (dB SPL) for a frequency in Hz.
fn ath_db(f: f32) -> f32 {
    let k = (f / 1000.0).max(0.01);
    3.64 * k.powf(-0.8) - 6.5 * (-0.6 * (k - 3.3).powi(2)).exp() + 1e-3 * k.powi(4)
}

/// **SIMD-house masking brick:** the signal-INDEPENDENT band geometry, computed
/// once per sample rate and cached. The Q3 masking loop used to recompute all of
/// this — ~22×22 `spreading_db` (`sqrt`) + `powf`, plus per-band `bark`/`ath`
/// transcendentals — for *every granule*. None of it depends on the signal.
struct BandModel {
    /// FFT-bin range `[lo, hi)` summed for each band's energy.
    bin_lo: [usize; SFB_LONG],
    bin_hi: [usize; SFB_LONG],
    /// Spreading matrix `10^(spreading_db(barkᵢ−barkⱼ)/10)` — the per-granule
    /// masking sum is now `Σⱼ energy[j]·spread[i][j]` (no transcendentals; a dot
    /// product that auto-vectorizes).
    spread: [[f32; SFB_LONG]; SFB_LONG],
    /// `10^(ath_db/10)` per band (scaled by the per-granule `ath_scale` at runtime).
    ath_base: [f32; SFB_LONG],
}

impl BandModel {
    /// Build the band model for `sample_rate` — each value computed by the exact
    /// same f32 formula the Q3 loop used inline, so the result is bit-identical.
    fn new(sample_rate: u32) -> BandModel {
        let sfb = tables::sfb_long_offsets(sample_rate);
        let bin_per_line = N_FFT as f32 / 1152.0;
        let mut bin_lo = [0usize; SFB_LONG];
        let mut bin_hi = [0usize; SFB_LONG];
        let mut center_bark = [0f32; SFB_LONG];
        let mut ath_base = [0f32; SFB_LONG];
        for b in 0..SFB_LONG {
            bin_lo[b] = (sfb[b] as f32 * bin_per_line).round() as usize;
            bin_hi[b] = ((sfb[b + 1] as f32 * bin_per_line).round() as usize).min(N_FFT / 2 + 1);
            let center_line = (sfb[b] as f32 + sfb[b + 1] as f32) * 0.5;
            center_bark[b] = bark(center_line * sample_rate as f32 / 1152.0);
            ath_base[b] = 10f32.powf(ath_db(center_line * sample_rate as f32 / 1152.0) / 10.0);
        }
        let mut spread = [[0f32; SFB_LONG]; SFB_LONG];
        for i in 0..SFB_LONG {
            for j in 0..SFB_LONG {
                spread[i][j] = 10f32.powf(spreading_db(center_bark[i] - center_bark[j]) / 10.0);
            }
        }
        BandModel {
            bin_lo,
            bin_hi,
            spread,
            ath_base,
        }
    }
}

/// Cached band model for a sample rate (leaked once per rate — at most the ~8 valid
/// rates ever, ~2 KB each). A thread-local keeps the common same-rate path lock-free.
fn band_model(sample_rate: u32) -> &'static BandModel {
    use std::cell::RefCell;
    thread_local! {
        static CACHE: RefCell<Option<(u32, &'static BandModel)>> = const { RefCell::new(None) };
    }
    CACHE.with(|cell| {
        let mut c = cell.borrow_mut();
        if let Some((sr, m)) = *c {
            if sr == sample_rate {
                return m;
            }
        }
        let m: &'static BandModel = Box::leak(Box::new(BandModel::new(sample_rate)));
        *c = Some((sample_rate, m));
        m
    })
}

/// Detect a transient/attack in a granule's PCM: a sub-block whose energy jumps
/// well above the recent running average. Such granules want short blocks so the
/// pre-echo a long window would smear before the attack is confined to one short
/// window. (Brick **Q5** — the block-type trigger; the FSM that turns it into a
/// valid Long/Start/Short/Stop sequence lives in `shortblock`.)
pub fn detect_attack(pcm: &[f32]) -> bool {
    const BLOCKS: usize = 8;
    const RATIO: f32 = 10.0;
    let n = pcm.len().min(crate::frame::GRANULE_LINES);
    let bs = n / BLOCKS;
    if bs == 0 {
        return false;
    }
    let energy = |b: usize| -> f32 {
        pcm[b * bs..(b + 1) * bs].iter().map(|x| x * x).sum::<f32>() / bs as f32
    };
    // **Quality fix:** seed the running baseline from the FIRST sub-block, not a
    // ~zero constant. The old `running = 1e-6` meant block 0's energy (any audible
    // signal) instantly exceeded `running·RATIO`, so a transient was "detected" in
    // essentially every non-silent granule — 95% short blocks on a ringtone, which
    // then quantize with flat scalefactors and bypass the psymodel. Now an attack is
    // a sub-block that genuinely SPIKES ≥RATIO× above the recent energy.
    let mut running = energy(0).max(1e-9);
    for b in 1..BLOCKS {
        let e = energy(b);
        if e > running * RATIO {
            return true;
        }
        running = (running * 2.0 + e) / 3.0; // smoothed recent energy
    }
    false
}

/// Run the psychoacoustic model over one granule of PCM at `sample_rate`.
pub fn analyze(pcm: &[f32], sample_rate: u32) -> PsyResult {
    let sfb = tables::sfb_long_offsets(sample_rate);
    let win = hann();
    let model = band_model(sample_rate); // signal-independent geometry, cached

    // Q2 — windowed FFT power spectrum.
    let mut re = [0f32; N_FFT];
    let mut im = [0f32; N_FFT];
    let navail = pcm.len().min(N_FFT);
    for i in 0..navail {
        re[i] = pcm[i] * win[i];
    }
    let mut power = [0f32; N_FFT / 2 + 1];
    fft::power_spectrum(&mut re, &mut im, &mut power);

    // Per-band signal energy, over the cached bin ranges.
    let mut energy = [0f32; SFB_LONG];
    for b in 0..SFB_LONG {
        let mut e = 1e-12f32;
        for &p in power.iter().take(model.bin_hi[b]).skip(model.bin_lo[b]) {
            e += p;
        }
        energy[b] = e;
    }

    // Q3 — spread energy across Bark (cached matrix → a dot product, no powf),
    // lower by the SMR offset, floor at the ATH.
    let smr = 10f32.powf(-SMR_OFFSET_DB / 10.0);
    let total_energy: f32 = energy.iter().sum();
    let ath_scale = (total_energy / N_FFT as f32).max(1e-9) * 1e-3;
    let mut thresholds = [0f32; SFB_LONG];
    for i in 0..SFB_LONG {
        let row = &model.spread[i];
        let mut masker = 0f32;
        for j in 0..SFB_LONG {
            masker += energy[j] * row[j];
        }
        let ath = model.ath_base[i] * ath_scale;
        // Cap at the band's own energy: a band quantized to zero already produces
        // noise = its energy, so a higher threshold means the same thing (drop it)
        // while bounding the wild ATH values in the inaudible top bands.
        thresholds[i] = (masker * smr).max(ath).min(energy[i]);
    }

    // Q4 — perceptual entropy: rough bit demand from the signal/threshold ratio.
    let mut pe = 0f32;
    for b in 0..SFB_LONG {
        let lines = (sfb[b + 1] - sfb[b]) as f32;
        pe += lines * (1.0 + energy[b] / thresholds[b]).log2().max(0.0);
    }

    PsyResult {
        block_type: BlockType::Long, // Q5 (block switching) deferred
        thresholds,
        perceptual_entropy: pe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bark_is_monotonic_and_spans_critical_bands() {
        assert!(bark(100.0) < bark(1000.0) && bark(1000.0) < bark(10000.0));
        assert!(
            (bark(1000.0) - 8.5).abs() < 1.5,
            "1 kHz ≈ 8.5 Bark, got {}",
            bark(1000.0)
        );
    }

    #[test]
    fn ath_has_a_minimum_in_the_ear_canal_band() {
        // Hearing is most sensitive ~3–4 kHz (ATH near its minimum, ~ -5 dB).
        assert!(ath_db(3500.0) < ath_db(200.0));
        assert!(ath_db(3500.0) < ath_db(15000.0));
    }

    #[test]
    fn spreading_peaks_at_zero_and_decays() {
        assert!(spreading_db(0.0).abs() < 0.5);
        assert!(spreading_db(2.0) < -3.0);
        assert!(spreading_db(-2.0) < -3.0);
        // Asymmetric: a masker spreads further toward higher frequencies (dz>0).
        assert!(spreading_db(1.5) > spreading_db(-1.5));
    }

    /// **Masking-brick gate.** The cached band model must hold the EXACT values the
    /// Q3 loop used to compute inline (same f32 formulas) — so precomputing them
    /// leaves the thresholds, and thus the output, bit-identical.
    #[test]
    fn band_model_matches_inline() {
        for &sr in &[44100u32, 48000, 32000, 24000] {
            let sfb = tables::sfb_long_offsets(sr);
            let m = BandModel::new(sr);
            let bin_per_line = N_FFT as f32 / 1152.0;
            let mut cb = [0f32; SFB_LONG];
            for b in 0..SFB_LONG {
                let lo = (sfb[b] as f32 * bin_per_line).round() as usize;
                let hi = ((sfb[b + 1] as f32 * bin_per_line).round() as usize).min(N_FFT / 2 + 1);
                assert_eq!(m.bin_lo[b], lo, "bin_lo b{b} sr{sr}");
                assert_eq!(m.bin_hi[b], hi, "bin_hi b{b} sr{sr}");
                let center_line = (sfb[b] as f32 + sfb[b + 1] as f32) * 0.5;
                cb[b] = bark(center_line * sr as f32 / 1152.0);
                let ath = 10f32.powf(ath_db(center_line * sr as f32 / 1152.0) / 10.0);
                assert_eq!(m.ath_base[b], ath, "ath_base b{b} sr{sr}");
            }
            for i in 0..SFB_LONG {
                for j in 0..SFB_LONG {
                    let s = 10f32.powf(spreading_db(cb[i] - cb[j]) / 10.0);
                    assert_eq!(m.spread[i][j], s, "spread[{i}][{j}] sr{sr}");
                }
            }
        }
    }

    #[test]
    fn tone_thresholds_are_bounded_by_signal_energy() {
        // A 1 kHz tone: every band's threshold sits below the loudest band's
        // energy (masking can't exceed the masker) and stays finite/positive.
        let sr = 44100;
        let pcm: Vec<f32> = (0..N_FFT)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        let psy = analyze(&pcm, sr);
        let peak_energy = {
            // recompute band energies the same way for the bound
            let sfb = tables::sfb_long_offsets(sr);
            let win = hann();
            let mut re = [0f32; N_FFT];
            let mut im = [0f32; N_FFT];
            for i in 0..N_FFT {
                re[i] = pcm[i] * win[i];
            }
            let mut p = [0f32; N_FFT / 2 + 1];
            fft::power_spectrum(&mut re, &mut im, &mut p);
            let mut m = 0f32;
            for b in 0..SFB_LONG {
                let lo = (sfb[b] as f32 * N_FFT as f32 / 1152.0).round() as usize;
                let hi = ((sfb[b + 1] as f32 * N_FFT as f32 / 1152.0).round() as usize)
                    .min(N_FFT / 2 + 1);
                let e: f32 = p[lo..hi].iter().sum();
                m = m.max(e);
            }
            m
        };
        for (b, &t) in psy.thresholds.iter().enumerate() {
            assert!(t.is_finite() && t > 0.0, "band {b} threshold {t}");
            assert!(
                t <= peak_energy * 1.01,
                "band {b} threshold {t} > peak {peak_energy}"
            );
        }
        assert!(psy.perceptual_entropy > 0.0);
    }
}
