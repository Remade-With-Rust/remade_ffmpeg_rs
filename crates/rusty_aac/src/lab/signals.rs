//! **P1 — the content-signal vector.** One per-frame struct that every gate in
//! the campaign reads, replacing the duplicated probe skeletons scattered through
//! the encoder (`detect_transients` walks sub-block energies; `perceptual_offsets`
//! re-walks band energies; neither is reusable).
//!
//! Great-gate §2 binds every signal here:
//!
//! * **Harvested at decision time.** Each field is computable from the *input*
//!   spectrum before any quantization decision, so a gate reading it is not
//!   measuring its own effect.
//! * **Validated against a per-class truth table BEFORE wiring.** The tests at the
//!   foot of this file are that truth table: each signal must actually separate
//!   the classes it claims to. A signal that does not discriminate cannot route.
//! * **Population-relative normalization** (law 1). Raw values are recorded, but
//!   gates consume them through [`AacSignals::percentile_of`] — a threshold is a
//!   percentile of *this clip's own distribution*, never an absolute. That is the
//!   specific defect in the shipping transient detector (`RATIO = 10.0`), which
//!   will miss soft attacks on quiet content and false-fire on loud content.
//! * **Group context as a feature** (law 2). Decisions are per frame, but
//!   [`AacSignals`] carries clip-level aggregates so a gate can use both — in the
//!   reference fit the group feature took 52% of the importance.
//!
//! Nothing here changes encoder behavior. It is an observation layer, built so the
//! rungs can harvest before they route.

use crate::encode::{analyze_long, masking_thresholds, FRAME_LEN};
use crate::swb::swb_offsets;

/// The per-frame signal vector — one row of a gate harvest.
#[derive(Debug, Clone)]
pub struct FrameSignals {
    /// Frame index within the clip.
    pub frame: usize,

    // --- axis: transient density / attack -----------------------------------
    /// Per-sub-block (128-sample) energy ratio against the running average.
    ///
    /// **Population-relative** (law 1): the running average is floored at a
    /// fraction of *this clip's own* mean sub-block energy, not at an absolute
    /// constant. That difference is arm A9 — see [`FrameSignals::shipped_transient`].
    pub attack_ratio: [f32; 8],
    /// `max(attack_ratio)`: the frame's attack strength. Consumer: A1, A9.
    pub attack_max: f32,
    /// Index of the strongest sub-block. Consumer: A1 — it is where a window
    /// group should be split.
    pub attack_pos: u8,
    /// What the **shipping** detector (`encode::detect_transients`) decides for
    /// this frame. Carried so a harvest can populate the gate-calculator's
    /// `shipped` column: a candidate rule that merely re-flags what the shipping
    /// detector already flags must score exactly 0, not vanish silently.
    pub shipped_transient: bool,

    // --- axis: tonality vs noise --------------------------------------------
    /// Per-SFB tonality index in `[0, 1]`: 0 = noise-like, 1 = pure tone.
    /// Derived from the band's spectral flatness measure. Consumer: A2, A6, A8.
    pub tonality: Vec<f32>,
    /// Energy-weighted mean tonality over the frame.
    pub tonality_mean: f32,

    // --- axis: transient, spectral domain -----------------------------------
    /// Prediction gain of an order-8 LPC run **across the spectrum** — the
    /// classic TNS on/off criterion. High when the signal is impulsive in time.
    /// Consumer: A3.
    pub lpc_gain: f32,

    // --- axis: silence / activity -------------------------------------------
    /// Frame RMS in dBFS. Consumer: A4, A5.
    pub loudness_dbfs: f32,
    /// Frame peak in dBFS.
    pub peak_dbfs: f32,

    // --- derived: bit demand -------------------------------------------------
    /// Perceptual entropy, `Σ nlines·log2(1 + energy/threshold)` — the classic
    /// reservoir demand signal, in bits. Consumer: A5.
    pub pe: f32,

    // --- axis: bandwidth ------------------------------------------------------
    /// Frequency (Hz) below which 95% of the frame's energy lies. Consumer: A11.
    pub rolloff_hz: f32,

    // --- axis: stereo correlation --------------------------------------------
    /// Per-SFB inter-channel correlation in `[-1, 1]`, `None` for mono.
    /// Consumer: A7, A10.
    pub xcorr: Option<Vec<f32>>,
}

/// Every frame's signals for one clip, plus the clip-level aggregates a gate uses
/// as group context (law 2) and the percentile machinery it uses for thresholds
/// (law 1).
#[derive(Debug, Clone)]
pub struct AacSignals {
    pub sample_rate: u32,
    pub frames: Vec<FrameSignals>,
}

impl AacSignals {
    /// Analyze a clip. `planes` is one slice per channel; only channel 0 drives
    /// the mono signals, with `xcorr` filled when a second channel is present.
    pub fn analyze(planes: &[&[f32]], sample_rate: u32) -> AacSignals {
        let fs_index = crate::sf_index_for_rate(sample_rate).unwrap_or(4);
        let swb = swb_offsets(true, fs_index);
        let nbands = swb.len() - 1;
        let win = crate::dsp::sine_window(2 * FRAME_LEN);
        let ch0 = planes.first().copied().unwrap_or(&[]);
        let ch1 = planes.get(1).copied();
        let nframes = ch0.len() / FRAME_LEN;

        // What the SHIPPING detector decides, recorded alongside our replacement
        // signal (the calculator's `shipped` column).
        let shipped = crate::encode::detect_transients(ch0, nframes);

        // Law 1: the running average is floored relative to THIS CLIP's own mean
        // sub-block energy, never at an absolute constant. The shipping detector
        // floors at `avg > 1e-3` in absolute units, which is why it goes blind on
        // quiet content — see the `shipping_detector_is_level_dependent` test.
        let clip_mean_e: f64 = {
            let mut acc = 0f64;
            let mut cnt = 0u64;
            for c in ch0.chunks_exact(128) {
                acc += c.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
                cnt += 1;
            }
            acc / cnt.max(1) as f64
        };
        let avg_floor = (clip_mean_e * 1e-3).max(f64::MIN_POSITIVE);

        let mut avg = 0.0f64;
        let mut frames = Vec::with_capacity(nframes);

        for f in 0..nframes {
            let cur = &ch0[f * FRAME_LEN..(f + 1) * FRAME_LEN];
            let prev = if f == 0 {
                &ch0[0..FRAME_LEN]
            } else {
                &ch0[(f - 1) * FRAME_LEN..f * FRAME_LEN]
            };

            // --- attack profile (mirrors detect_transients) ---
            let mut attack_ratio = [1.0f32; 8];
            let (mut amax, mut apos) = (1.0f32, 0u8);
            for (sb, slot) in attack_ratio.iter_mut().enumerate() {
                let e: f64 = cur[sb * 128..(sb + 1) * 128]
                    .iter()
                    .map(|&x| (x as f64) * (x as f64))
                    .sum();
                // Always defined, and scale-invariant: both `e` and the floor
                // scale with the clip's level, so a 40 dB gain change leaves the
                // ratio untouched.
                *slot = (e / avg.max(avg_floor)) as f32;
                if *slot > amax {
                    amax = *slot;
                    apos = sb as u8;
                }
                avg = 0.75 * avg + 0.25 * e;
            }

            // --- spectrum ---
            let mut p = [0f32; FRAME_LEN];
            let mut c = [0f32; FRAME_LEN];
            p.copy_from_slice(prev);
            c.copy_from_slice(cur);
            let spec = analyze_long(&p, &c, &win);

            let tonality = band_tonality(&spec, swb);
            let thr = masking_thresholds(&spec, swb, sample_rate);

            // Energy-weighted mean tonality + PE, one walk.
            let (mut etot, mut twsum, mut pe) = (0f64, 0f64, 0f64);
            for b in 0..nbands {
                let (s, e) = (swb[b] as usize, swb[b + 1] as usize);
                let energy: f64 = spec[s..e.min(spec.len())]
                    .iter()
                    .map(|&x| (x as f64) * (x as f64))
                    .sum();
                etot += energy;
                twsum += energy * tonality[b] as f64;
                let nlines = (e - s) as f64;
                pe += nlines * (1.0 + energy / thr[b].max(1e-20)).log2();
            }
            let tonality_mean = if etot > 1e-9 {
                (twsum / etot) as f32
            } else {
                0.0
            };

            // --- loudness ---
            let ms: f64 = cur.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
                / FRAME_LEN as f64;
            let peak = cur.iter().fold(0f32, |a, &x| a.max(x.abs()));

            frames.push(FrameSignals {
                frame: f,
                attack_ratio,
                attack_max: amax,
                attack_pos: apos,
                shipped_transient: shipped.get(f).copied().unwrap_or(false),
                tonality,
                tonality_mean,
                lpc_gain: spectral_lpc_gain(&spec, 8),
                loudness_dbfs: db(ms.sqrt() as f32),
                peak_dbfs: db(peak),
                pe: pe as f32,
                rolloff_hz: rolloff(&spec, sample_rate, 0.95),
                xcorr: ch1.map(|r| {
                    let mut pr = [0f32; FRAME_LEN];
                    let mut cr = [0f32; FRAME_LEN];
                    let lo = if f == 0 { 0 } else { (f - 1) * FRAME_LEN };
                    pr.copy_from_slice(&r[lo..lo + FRAME_LEN]);
                    cr.copy_from_slice(&r[f * FRAME_LEN..(f + 1) * FRAME_LEN]);
                    let rspec = analyze_long(&pr, &cr, &win);
                    band_xcorr(&spec, &rspec, swb)
                }),
            });
        }

        AacSignals {
            sample_rate,
            frames,
        }
    }

    /// **Law 1 in one function.** The value of `feature` at percentile `q` of this
    /// clip's own distribution — the form every threshold in the campaign takes.
    /// An absolute threshold dies on content whose scale varies 50×; this does not.
    pub fn percentile_of<F: Fn(&FrameSignals) -> f32>(&self, feature: F, q: f32) -> f32 {
        let mut v: Vec<f32> = self.frames.iter().map(&feature).collect();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((v.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
        v[idx]
    }

    /// Clip-level mean of a feature — the group context a per-frame gate reads
    /// alongside its own unit's value (law 2).
    pub fn mean_of<F: Fn(&FrameSignals) -> f32>(&self, feature: F) -> f32 {
        if self.frames.is_empty() {
            return 0.0;
        }
        self.frames.iter().map(&feature).sum::<f32>() / self.frames.len() as f32
    }
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

fn db(x: f32) -> f32 {
    if x <= 1e-9 {
        -180.0
    } else {
        20.0 * x.log10()
    }
}

/// Per-band tonality in `[0, 1]` from the spectral flatness measure.
///
/// `SFM = geometric_mean(|X|²) / arithmetic_mean(|X|²)`, in dB, mapped through the
/// classic `min(SFM_dB / −60, 1)`: a pure tone has SFM → 0 (−∞ dB → 1.0), white
/// noise has SFM → 1 (0 dB → 0.0).
fn band_tonality(spec: &[f32], swb: &[u16]) -> Vec<f32> {
    let n = swb.len() - 1;
    let mut out = vec![0f32; n];
    for b in 0..n {
        let (s, e) = (swb[b] as usize, swb[b + 1] as usize);
        let e = e.min(spec.len());
        if e <= s {
            continue;
        }
        let cnt = (e - s) as f64;
        let (mut log_sum, mut lin_sum) = (0f64, 0f64);
        for &x in &spec[s..e] {
            // Floor well below any audible coefficient so a single exact zero
            // cannot drive the geometric mean to 0 and fake a "pure tone".
            let p = ((x as f64) * (x as f64)).max(1e-10);
            log_sum += p.ln();
            lin_sum += p;
        }
        let gm = (log_sum / cnt).exp();
        let am = lin_sum / cnt;
        let sfm_db = 10.0 * (gm / am.max(1e-30)).log10();
        out[b] = ((sfm_db / -60.0) as f32).clamp(0.0, 1.0);
    }
    out
}

/// Prediction gain of an order-`p` LPC fit **across the spectrum** — the TNS
/// criterion. A signal that is impulsive in time has a smoothly-predictable
/// spectral envelope, so this rises exactly where TNS pays.
///
/// Returns `R[0] / E_p` (≥ 1). Levinson-Durbin, with a small lag window for
/// numerical conditioning.
fn spectral_lpc_gain(spec: &[f32], order: usize) -> f32 {
    let n = spec.len();
    if n <= order + 1 {
        return 1.0;
    }
    let mut r = vec![0f64; order + 1];
    for (lag, slot) in r.iter_mut().enumerate() {
        let mut acc = 0f64;
        for i in 0..n - lag {
            acc += spec[i] as f64 * spec[i + lag] as f64;
        }
        *slot = acc;
    }
    if r[0] <= 1e-12 {
        return 1.0;
    }
    r[0] *= 1.0001; // white-noise correction — keeps Levinson stable
    let r0 = r[0];
    let mut a = vec![0f64; order + 1];
    let mut err = r0;
    for i in 1..=order {
        let mut acc = r[i];
        for j in 1..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / err;
        if !k.is_finite() || k.abs() >= 1.0 {
            break;
        }
        let prev: Vec<f64> = a[1..i].to_vec();
        for j in 1..i {
            a[j] = prev[j - 1] - k * prev[i - j - 1];
        }
        a[i] = k;
        err *= 1.0 - k * k;
        if err <= 1e-12 {
            break;
        }
    }
    ((r0 / err.max(1e-12)) as f32).max(1.0)
}

/// Frequency below which `frac` of the spectrum's energy lies.
fn rolloff(spec: &[f32], sample_rate: u32, frac: f32) -> f32 {
    let total: f64 = spec.iter().map(|&x| (x as f64) * (x as f64)).sum();
    if total <= 1e-12 {
        return 0.0;
    }
    let target = total * frac as f64;
    let mut acc = 0f64;
    for (i, &x) in spec.iter().enumerate() {
        acc += (x as f64) * (x as f64);
        if acc >= target {
            // Coefficient i covers [i, i+1) × (sr/2) / 1024.
            return (i as f32 + 0.5) * (sample_rate as f32 * 0.5) / spec.len() as f32;
        }
    }
    sample_rate as f32 * 0.5
}

/// Per-band inter-channel correlation of two spectra.
fn band_xcorr(l: &[f32], r: &[f32], swb: &[u16]) -> Vec<f32> {
    let n = swb.len() - 1;
    let mut out = vec![0f32; n];
    for b in 0..n {
        let (s, e) = (swb[b] as usize, swb[b + 1] as usize);
        let e = e.min(l.len()).min(r.len());
        if e <= s {
            continue;
        }
        let (mut dot, mut nl, mut nr) = (0f64, 0f64, 0f64);
        for k in s..e {
            let (a, c) = (l[k] as f64, r[k] as f64);
            dot += a * c;
            nl += a * a;
            nr += c * c;
        }
        out[b] = (dot / (nl.sqrt() * nr.sqrt()).max(1e-20)) as f32;
    }
    out
}

// ---------------------------------------------------------------------------
// P1b — the per-class truth table
//
// Great-gate §2: "Validate against a brute-force oracle or per-class truth table
// BEFORE wiring." A signal that fails here cannot route anything, and finding
// that out now costs a test run instead of a fitted gate.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod truth_table {
    use super::*;
    use crate::lab::corpus::{self, Class};

    fn analyze(class: Class) -> AacSignals {
        let s = corpus::signal(class);
        let planes = s.planes();
        let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
        AacSignals::analyze(&refs, s.sample_rate)
    }

    /// Every class produces a full, finite signal vector.
    #[test]
    fn signals_are_well_formed() {
        for c in Class::all() {
            let s = analyze(c);
            assert!(s.frames.len() > 20, "{}: too few frames", c.name());
            for f in &s.frames {
                assert!(f.attack_max.is_finite(), "{}: attack NaN", c.name());
                assert!(f.tonality_mean.is_finite() && (0.0..=1.0).contains(&f.tonality_mean),
                    "{}: tonality out of range: {}", c.name(), f.tonality_mean);
                assert!(f.lpc_gain.is_finite() && f.lpc_gain >= 1.0,
                    "{}: bad lpc_gain {}", c.name(), f.lpc_gain);
                assert!(f.pe.is_finite() && f.pe >= 0.0, "{}: bad pe {}", c.name(), f.pe);
                assert!(f.rolloff_hz.is_finite(), "{}: bad rolloff", c.name());
            }
        }
    }

    /// **A1/A9 axis.** The attack signal must separate percussive content from
    /// sustained tonal content. If it cannot, no block-switch gate can route.
    #[test]
    fn attack_separates_percussive_from_tonal() {
        let perc = analyze(Class::Percussive);
        let tonal = analyze(Class::MusicTonal);
        // Compare high percentiles: attacks are rare by construction, so a mean
        // would wash them out — which is itself the reason the shipping detector
        // works per sub-block rather than per frame.
        let p = perc.percentile_of(|f| f.attack_max, 0.90);
        let t = tonal.percentile_of(|f| f.attack_max, 0.90);
        assert!(
            p > t * 2.0,
            "attack_max must separate percussive ({p:.2}) from tonal ({t:.2})"
        );
    }

    /// **A2/A6/A8 axis.** Tonality must separate a harmonic stack from applause.
    /// This is the signal that today's constant 18 dB SMR ignores entirely, and
    /// the one that decides whether PNS is a win or a disaster.
    #[test]
    fn tonality_separates_harmonic_from_noise() {
        let tonal = analyze(Class::MusicTonal).mean_of(|f| f.tonality_mean);
        let noise = analyze(Class::NoiseLike).mean_of(|f| f.tonality_mean);
        assert!(
            tonal > noise + 0.05,
            "tonality must separate harmonic ({tonal:.3}) from noise-like ({noise:.3})"
        );
    }

    /// **A3 axis.** Spectral LPC gain must be higher on impulsive content than on
    /// steady content — the criterion TNS switches on.
    #[test]
    fn lpc_gain_separates_impulsive_from_steady() {
        let perc = analyze(Class::Percussive).percentile_of(|f| f.lpc_gain, 0.75);
        let tonal = analyze(Class::MusicTonal).percentile_of(|f| f.lpc_gain, 0.75);
        assert!(
            perc > tonal,
            "spectral LPC gain must be higher on percussive ({perc:.2}) \
             than sustained tonal ({tonal:.2})"
        );
    }

    /// **A4/A5 axis.** Loudness must resolve the quiet-dynamic class's two halves.
    /// This is the whole reason that gap class was synthesized: without a signal
    /// that sees absolute level, neither the ATH arm nor the reservoir can exist.
    #[test]
    fn loudness_resolves_the_quiet_class() {
        let s = analyze(Class::QuietDynamic);
        let lo = s.percentile_of(|f| f.loudness_dbfs, 0.25);
        let hi = s.percentile_of(|f| f.loudness_dbfs, 0.75);
        assert!(
            hi - lo > 25.0,
            "loudness must span the quiet/loud splice: {lo:.1} → {hi:.1} dBFS"
        );
        // And it must be an ABSOLUTE reading, not a normalized one: the quiet half
        // has to actually read as quiet.
        assert!(lo < -40.0, "quiet half should read well below -40 dBFS, got {lo:.1}");
    }

    /// **A7/A10 axis.** Inter-channel correlation must exist for stereo and mark
    /// the wide class as decorrelated.
    #[test]
    fn xcorr_marks_wide_stereo() {
        let s = analyze(Class::StereoWide);
        let with_xcorr = s.frames.iter().filter(|f| f.xcorr.is_some()).count();
        assert_eq!(with_xcorr, s.frames.len(), "stereo clip must carry xcorr");
        let mean: f32 = s
            .frames
            .iter()
            .filter_map(|f| f.xcorr.as_ref())
            .map(|v| v.iter().sum::<f32>() / v.len() as f32)
            .sum::<f32>()
            / s.frames.len() as f32;
        assert!(
            mean.abs() < 0.8,
            "stereo-wide must read as decorrelated, got mean xcorr {mean:.3}"
        );
        // Mono clips must NOT fabricate one.
        assert!(analyze(Class::MusicTonal).frames[0].xcorr.is_none());
    }

    /// **A5 axis.** Perceptual entropy must vary across the mixed-content class —
    /// that variation IS the reservoir's donation signal. A flat PE would mean
    /// there is nothing to redistribute.
    #[test]
    fn pe_varies_on_mixed_content() {
        let s = analyze(Class::MixedSpeechMusic);
        let lo = s.percentile_of(|f| f.pe, 0.10);
        let hi = s.percentile_of(|f| f.pe, 0.90);
        assert!(
            hi > lo * 1.3,
            "PE must vary across mixed content to drive a reservoir: {lo:.0} → {hi:.0} bits"
        );
    }

    /// **A9 FINDING (2026-08-08, found by this truth table before any gate was
    /// fitted).** The shipping detector's guard is `avg > 1e-3` — an *absolute*
    /// energy threshold. Scaling a clip down by 40 dB, which changes nothing
    /// perceptually about where its attacks are, drives the running average under
    /// that floor and the detector stops flagging transients **entirely**.
    ///
    /// This is worse than "misses soft transients": the detector is structurally
    /// blind on quiet content, so those frames code as long blocks and take the
    /// full pre-echo hit. Our replacement signal, floored population-relative,
    /// is unchanged by the same scaling.
    ///
    /// These tests assert the *current* behavior so the defect is pinned and
    /// cannot regress silently. When arm A9 lands, they flip to asserting parity.
    fn flagged(pcm: &[f32]) -> usize {
        let nframes = pcm.len() / crate::encode::FRAME_LEN;
        crate::encode::detect_transients(pcm, nframes)
            .iter()
            .filter(|&&b| b)
            .count()
    }

    /// Attack + sustained background at a controllable level — the shape the
    /// shipping detector was tuned on (cf. `transient_encodes_short_and_decodes`).
    fn burst_over_background(bg_amp: f32) -> Vec<f32> {
        use core::f32::consts::PI;
        let sr = corpus::SR as f32;
        let n = 16 * crate::encode::FRAME_LEN;
        (0..n)
            .map(|i| {
                let t = i as f32 / sr;
                let mut v = bg_amp * (2.0 * PI * 400.0 * t).sin();
                let k = i % (4 * crate::encode::FRAME_LEN);
                if k < 600 && i > crate::encode::FRAME_LEN {
                    let env = 1.0 - k as f32 / 600.0;
                    v += 0.9 * env * (2.0 * PI * 3000.0 * k as f32 / sr).sin();
                }
                v
            })
            .collect()
    }

    /// **A9 FINDING, part 1 — the detector is level-dependent.** The identical
    /// waveform, scaled by −40 dB, stops being detected. Nothing perceptual about
    /// where the attacks are has changed; only the absolute energy has.
    #[test]
    fn shipping_detector_is_level_dependent() {
        let loud = burst_over_background(0.05);
        let quiet: Vec<f32> = loud.iter().map(|&x| x * 0.01).collect(); // −40 dB

        let (hit_loud, hit_quiet) = (flagged(&loud), flagged(&quiet));
        assert!(
            hit_loud > 0,
            "control: this waveform must trigger the detector at full level"
        );
        assert_eq!(
            hit_quiet, 0,
            "A9: the absolute `avg > 1e-3` guard makes the detector blind at −40 dB \
             on the SAME waveform (loud: {hit_loud} frames flagged, quiet: {hit_quiet}). \
             Fix A9 and update this assertion."
        );

        // Our replacement signal must be immune to the same scaling.
        let a = AacSignals::analyze(&[&loud], corpus::SR);
        let b = AacSignals::analyze(&[&quiet], corpus::SR);
        let (pa, pb) = (
            a.percentile_of(|f| f.attack_max, 0.90),
            b.percentile_of(|f| f.attack_max, 0.90),
        );
        assert!(
            (pa - pb).abs() / pa.max(1e-6) < 0.05,
            "the replacement signal must be level-invariant: {pa:.2} vs {pb:.2}"
        );
    }

    /// **A9 FINDING, part 2 — the detector misses sparse attacks entirely.**
    ///
    /// Worse than a level offset: because the running average decays 0.75× per
    /// sub-block, it falls under the absolute guard during the ~75 quiet
    /// sub-blocks between castanet-like clicks. By the time the next attack
    /// arrives the guard is closed, so the ratio is never even evaluated.
    ///
    /// The percussive class is flagged **zero** times at FULL level, despite our
    /// level-invariant signal measuring very large attack ratios on the same
    /// audio. Every one of those frames codes as a long block and takes the full
    /// pre-echo hit — which is a large part of what arm A1 will be measured
    /// against.
    #[test]
    fn shipping_detector_misses_sparse_attacks() {
        let s = corpus::signal(Class::Percussive);
        let pcm = s.mono();

        let hits = flagged(&pcm);
        let sig = AacSignals::analyze(&[&pcm], s.sample_rate);
        let peak_attack = sig.percentile_of(|f| f.attack_max, 0.95);

        assert!(
            peak_attack > 20.0,
            "control: the percussive class must carry large attack ratios, got {peak_attack:.1}"
        );
        assert_eq!(
            hits, 0,
            "A9: the shipping detector flags {hits} frames on percussive content \
             whose p95 attack ratio is {peak_attack:.0}× — the sparse-attack blind \
             spot. Fix A9 and update this assertion."
        );
    }

    /// **Law 1.** The percentile machinery must be genuinely population-relative:
    /// scaling a clip's amplitude must not move a percentile-keyed attack
    /// threshold, though it obviously moves an absolute one. This is the property
    /// the shipping `RATIO = 10.0` lacks.
    #[test]
    fn percentiles_are_population_relative() {
        let s = corpus::signal(Class::Percussive);
        let loud = s.mono();
        let quiet: Vec<f32> = loud.iter().map(|&x| x * 0.01).collect();

        let a = AacSignals::analyze(&[&loud], s.sample_rate);
        let b = AacSignals::analyze(&[&quiet], s.sample_rate);

        // The attack RATIO is scale-invariant (energy ratio), so its percentile
        // must be too — a 40 dB level change must not reclassify the content.
        let pa = a.percentile_of(|f| f.attack_max, 0.90);
        let pb = b.percentile_of(|f| f.attack_max, 0.90);
        let rel = (pa - pb).abs() / pa.max(1e-6);
        assert!(
            rel < 0.05,
            "attack percentile must be level-invariant: {pa:.3} vs {pb:.3} ({:.1}% apart)",
            rel * 100.0
        );

        // Absolute loudness, by contrast, MUST move — that is the A4 signal.
        assert!(
            a.mean_of(|f| f.loudness_dbfs) > b.mean_of(|f| f.loudness_dbfs) + 30.0,
            "loudness must track absolute level"
        );
    }
}
