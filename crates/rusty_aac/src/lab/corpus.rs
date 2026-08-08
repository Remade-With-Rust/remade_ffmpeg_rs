//! The deterministic AAC content corpus — one signal per content class from
//! `docs/codec-aac-great-gate.md` §1.1.
//!
//! Every signal is generated from a formula or a seeded LCG: no binary fixtures,
//! no wall clock, no `rand`. Identical bytes on every machine and every run, which
//! is what makes a per-class ODG/NMR table comparable across sessions — and what
//! lets a gate's train/holdout split be *stable by name* (great-gate §4 rule 4).
//!
//! **The corpus law** (great-gate §2): a feature with a known physical premise
//! that the corpus cannot judge is a **corpus gap**, to be synthesized rather than
//! called done. Two classes here exist for exactly that reason and are marked
//! `gap: true`:
//!
//! * [`Class::QuietDynamic`] — arms A4 (absolute threshold of hearing) and A5
//!   (bit reservoir) have premises about *absolute level* and about level
//!   *varying over time*. A corpus of loudness-normalized music clips is
//!   structurally incapable of judging either.
//! * [`Class::MixedSpeechMusic`] — the audio analog of "variable scenes": the
//!   class that exposes every unfinished dispatch, because its character changes
//!   mid-stream.
//!
//! These are synthetic stand-ins, deliberately chosen to have the *physical*
//! property each arm keys on. They make the harness runnable and self-checking
//! offline; real recorded clips should join the corpus as first-class members
//! (great-gate §5) once available, and the per-class table is regenerated the
//! same way either side.

use core::f32::consts::PI;

/// Sample rate for the whole corpus. 44.1 kHz keeps us on the fs_index the
/// encoder's SWB tables are best exercised at, and matches the MP3/Opus ladders.
pub const SR: u32 = 44_100;

/// Samples per channel — 2.0 s, i.e. ~86 AAC frames. Long enough for the ladder's
/// alignment search and for a per-frame gate signal to have a distribution, short
/// enough that a full 8-class × 4-bitrate ladder stays interactive.
pub const LEN: usize = 88_200;

/// The content classes the corpus must cover (great-gate §2, audio family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// Voiced/unvoiced alternation with pauses. Stresses A3 (TNS), A2 (tonality).
    SpeechClean,
    /// Speech over a broadband noise floor. Stresses A6 (PNS), A4 (ATH).
    SpeechNoisy,
    /// Harmonic stack with plucked decay. Stresses A2, A8 (KBD); the **A6
    /// anti-class** — PNS on a harmonic band is audibly destructive.
    MusicTonal,
    /// Sharp repeated attacks over near-silence. THE block-switch stressor:
    /// A1 (short-block psy), A3 (TNS), A9 (detector threshold).
    Percussive,
    /// Dense random impulses, broadband and phase-incoherent. The **A6 win
    /// class** — noise substitution costs nothing perceptually here.
    NoiseLike,
    /// Decorrelated left/right content. Stresses A7 (intensity stereo) and A10
    /// (M/S objective); the **A7 anti-class** for image collapse.
    StereoWide,
    /// **Corpus gap.** A −40 dBFS passage spliced against a 0 dBFS passage.
    /// The only class that can judge A4 (ATH) and A5 (reservoir).
    QuietDynamic,
    /// **Corpus gap.** Speech and music alternating at ~0.5 s grain — the
    /// variable-content class that exposes unfinished dispatch.
    MixedSpeechMusic,
}

impl Class {
    /// Stable short name — also the clip key for train/holdout splits, so a split
    /// stays put across rungs (great-gate §4 rule 4).
    pub fn name(self) -> &'static str {
        match self {
            Class::SpeechClean => "speech-clean",
            Class::SpeechNoisy => "speech-noisy",
            Class::MusicTonal => "music-tonal",
            Class::Percussive => "percussive",
            Class::NoiseLike => "noise-like",
            Class::StereoWide => "stereo-wide",
            Class::QuietDynamic => "quiet-dynamic",
            Class::MixedSpeechMusic => "mixed-speech-music",
        }
    }

    /// True for the two classes synthesized to close a corpus gap.
    pub fn is_gap(self) -> bool {
        matches!(self, Class::QuietDynamic | Class::MixedSpeechMusic)
    }

    /// The campaign arms this class is the decisive evidence for.
    pub fn stresses(self) -> &'static [&'static str] {
        match self {
            Class::SpeechClean => &["A3", "A2"],
            Class::SpeechNoisy => &["A6", "A4"],
            Class::MusicTonal => &["A2", "A8", "A6-anti"],
            Class::Percussive => &["A1", "A3", "A9"],
            Class::NoiseLike => &["A6"],
            Class::StereoWide => &["A7", "A10"],
            Class::QuietDynamic => &["A4", "A5"],
            Class::MixedSpeechMusic => &["A5", "A9", "variable"],
        }
    }

    /// Every class, in fixed order.
    pub fn all() -> [Class; 8] {
        [
            Class::SpeechClean,
            Class::SpeechNoisy,
            Class::MusicTonal,
            Class::Percussive,
            Class::NoiseLike,
            Class::StereoWide,
            Class::QuietDynamic,
            Class::MixedSpeechMusic,
        ]
    }
}

/// One corpus signal. `pcm` is interleaved when `channels == 2`.
#[derive(Debug, Clone)]
pub struct Signal {
    pub class: Class,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm: Vec<f32>,
}

impl Signal {
    pub fn name(&self) -> &'static str {
        self.class.name()
    }

    /// Samples per channel.
    pub fn frames(&self) -> usize {
        self.pcm.len() / self.channels.max(1) as usize
    }

    /// De-interleave into one plane per channel.
    pub fn planes(&self) -> Vec<Vec<f32>> {
        let ch = self.channels.max(1) as usize;
        (0..ch)
            .map(|c| self.pcm.iter().skip(c).step_by(ch).copied().collect())
            .collect()
    }

    /// Channel 0 as mono — what the NMR metric scores.
    pub fn mono(&self) -> Vec<f32> {
        if self.channels <= 1 {
            self.pcm.clone()
        } else {
            self.planes().swap_remove(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic primitives
// ---------------------------------------------------------------------------

/// Seeded LCG in `[-1, 1]` (glibc constants). Deterministic across machines.
struct Rng(u32);

impl Rng {
    /// Build from a seed, **avalanching it first**.
    ///
    /// A raw LCG advanced from two adjacent seeds produces near-identical first
    /// outputs (they differ by only the multiplier, ~0.04% of the output range),
    /// so `Rng(s)` and `Rng(s+1)` make effectively the same signal. That bug made
    /// [`Class::StereoWide`]'s two channels play identical notes and read as
    /// fully correlated — caught by `stereo_wide_is_decorrelated`. The finalizer
    /// below (murmur3's) decorrelates adjacent seeds.
    fn new(seed: u32) -> Rng {
        let mut z = seed ^ 0x9E37_79B9;
        z = (z ^ (z >> 16)).wrapping_mul(0x85EB_CA6B);
        z = (z ^ (z >> 13)).wrapping_mul(0xC2B2_AE35);
        Rng(z ^ (z >> 16))
    }

    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    }
}

/// A two-pole resonator: `y[n] = x[n] + 2r·cos(ω)·y[n−1] − r²·y[n−2]`. Used both
/// as a speech formant and as the body of a percussive click.
struct Resonator {
    a1: f32,
    a2: f32,
    y1: f32,
    y2: f32,
}

impl Resonator {
    fn new(freq: f32, bandwidth: f32) -> Self {
        let r = (-PI * bandwidth / SR as f32).exp();
        let w = 2.0 * PI * freq / SR as f32;
        Resonator {
            a1: 2.0 * r * w.cos(),
            a2: -r * r,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn step(&mut self, x: f32) -> f32 {
        let y = x + self.a1 * self.y1 + self.a2 * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Normalize to unit RMS, in place.
///
/// Mixing components by *peak* is misleading when they differ in crest factor: a
/// resonant speech pulse train peaks high but carries little energy, while a
/// sustained harmonic stack is the reverse. Mixing peak-normalized components
/// let the shared centre channel dominate [`Class::StereoWide`]'s energy, which
/// collapsed both channels onto it (xcorr 0.9998) even though their music beds
/// were completely different — caught by `stereo_wide_is_decorrelated`. RMS
/// normalization makes the mix weights mean what they say.
fn rms_normalize(pcm: &mut [f32]) {
    let ms: f64 = pcm.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / pcm.len().max(1) as f64;
    let rms = ms.sqrt() as f32;
    if rms > 1e-9 {
        for x in pcm.iter_mut() {
            *x /= rms;
        }
    }
}

/// Peak-normalize to `peak`, in place. Silent input is left alone.
fn normalize(pcm: &mut [f32], peak: f32) {
    let m = pcm.iter().fold(0f32, |a, &x| a.max(x.abs()));
    if m > 1e-9 {
        let g = peak / m;
        for x in pcm.iter_mut() {
            *x *= g;
        }
    }
}

// ---------------------------------------------------------------------------
// Generators — one per class
// ---------------------------------------------------------------------------

/// Voiced speech: a glottal pulse train through three formants, with unvoiced
/// (noise-excited) stretches and silent pauses. `n` samples of channel 0.
fn speech_core(n: usize, seed: u32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    // Three formants roughly at /a/ — the resonances give it speech-like tonality
    // without being a pure tone, which is what makes it a real A2/A3 stressor.
    let mut f1 = Resonator::new(730.0, 90.0);
    let mut f2 = Resonator::new(1090.0, 110.0);
    let mut f3 = Resonator::new(2440.0, 170.0);
    let mut out = Vec::with_capacity(n);
    let mut phase = 0.0f32;
    for i in 0..n {
        let t = i as f32 / SR as f32;
        // Syllabic structure: ~4 Hz alternation of voiced / unvoiced / pause.
        let syl = (t * 4.0) as usize % 4;
        // Pitch glides over each syllable — a constant F0 would be unnaturally
        // easy for a pitch-locked psy model.
        let f0 = 115.0 + 25.0 * (2.0 * PI * 0.7 * t).sin();
        phase += f0 / SR as f32;
        let excite = match syl {
            0 | 1 => {
                // Voiced: a narrow glottal pulse once per period.
                if phase >= 1.0 {
                    phase -= 1.0;
                    1.0
                } else {
                    0.0
                }
            }
            2 => 0.08 * rng.next(), // unvoiced fricative
            _ => 0.0,               // pause — the silence the reservoir should exploit
        };
        out.push(f3.step(f2.step(f1.step(excite))) * 0.02);
    }
    out
}

/// Harmonic stack with plucked decay and note changes — the tonal class.
///
/// The note *sequence* and the transposition are both seeded, not just the
/// amplitude jitter. That matters for [`Class::StereoWide`], which builds its two
/// channels from two calls to this function: if only the amplitudes differed, the
/// channels would play the same notes and read as fully correlated — which is
/// exactly what the corpus truth-table test caught.
fn music_core(n: usize, seed: u32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0f32; n];
    // A note every 0.25 s; each is a decaying harmonic stack.
    let note_len = SR as usize / 4;
    let base = [220.0f32, 261.63, 329.63, 392.0, 440.0, 329.63, 261.63, 196.0];
    // Seeded transposition (±5 semitones) and a seeded note order.
    let transpose = 2f32.powf(rng.next() * 5.0 / 12.0);
    let mut order: Vec<usize> = (0..base.len()).collect();
    for i in (1..order.len()).rev() {
        let j = (rng.next().abs() * (i + 1) as f32) as usize % (i + 1);
        order.swap(i, j);
    }
    let scale: Vec<f32> = order.iter().map(|&i| base[i] * transpose).collect();
    let mut note = 0usize;
    let mut start = 0usize;
    while start < n {
        let f0 = scale[note % scale.len()];
        // Slight per-note amplitude variation so the psy model sees real dynamics.
        let amp = 0.6 + 0.25 * rng.next().abs();
        let end = (start + note_len * 2).min(n); // notes overlap-ring into each other
        for (k, slot) in out[start..end].iter_mut().enumerate() {
            let t = k as f32 / SR as f32;
            let env = (-t * 3.5).exp();
            let mut v = 0.0f32;
            // 8 harmonics, 1/h amplitude — a genuinely tonal spectrum, which is
            // what makes this the PNS anti-class.
            for h in 1..=8 {
                let hf = f0 * h as f32;
                if hf < SR as f32 * 0.45 {
                    v += (1.0 / h as f32) * (2.0 * PI * hf * t).sin();
                }
            }
            *slot += amp * env * v;
        }
        start += note_len;
        note += 1;
    }
    out
}

/// Castanet-like clicks over near-silence — the block-switch stressor.
fn percussive_core(n: usize, seed: u32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0f32; n];
    // A click every ~0.22 s: an impulse through a high-frequency resonator with a
    // ~4 ms decay. Sharp enough that coding it as a long block smears pre-echo
    // across the whole 2048-sample window — exactly the artifact A1/A3 target.
    let period = (SR as f32 * 0.22) as usize;
    let mut at = period / 2;
    while at < n {
        let mut res = Resonator::new(3200.0 + 900.0 * rng.next(), 260.0);
        let tail = (SR as usize / 40).min(n - at); // 25 ms of ring-down
        for k in 0..tail {
            let x = if k == 0 { 1.0 } else { 0.0 };
            let env = (-(k as f32) / (SR as f32 * 0.004)).exp();
            out[at + k] += res.step(x) * env * 0.6;
        }
        at += period;
    }
    // A very quiet floor so the frames between clicks are not literally zero
    // (a digital-silence frame is a different, degenerate test).
    for (i, v) in out.iter_mut().enumerate() {
        let t = i as f32 / SR as f32;
        *v += 0.0015 * (2.0 * PI * 200.0 * t).sin();
    }
    out
}

/// Applause-like: dense random impulses with short decays. Broadband and
/// phase-incoherent — the class PNS exists for.
fn applause_core(n: usize, seed: u32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0f32; n];
    // ~900 claps/second worth of impulses, each a short noise burst.
    let mut i = 0usize;
    while i < n {
        // Exponentially-ish distributed gaps via the LCG magnitude.
        let gap = 8 + (rng.next().abs() * 80.0) as usize;
        i += gap;
        if i >= n {
            break;
        }
        let amp = 0.3 + 0.7 * rng.next().abs();
        let tail = (SR as usize / 300).min(n - i); // ~3 ms
        for k in 0..tail {
            let env = (-(k as f32) / (SR as f32 * 0.0008)).exp();
            out[i + k] += amp * env * rng.next();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

fn mono(class: Class, mut pcm: Vec<f32>, peak: f32) -> Signal {
    normalize(&mut pcm, peak);
    Signal {
        class,
        sample_rate: SR,
        channels: 1,
        pcm,
    }
}

/// Build one class's signal.
pub fn signal(class: Class) -> Signal {
    match class {
        Class::SpeechClean => mono(class, speech_core(LEN, 0x5EED_0001), 0.7),

        Class::SpeechNoisy => {
            let mut s = speech_core(LEN, 0x5EED_0002);
            normalize(&mut s, 0.7);
            let mut rng = Rng::new(0x00B0_15E0 ^ 0x5EED_0002);
            // Noise floor ~30 dB below the speech peak.
            for v in s.iter_mut() {
                *v += 0.022 * rng.next();
            }
            mono(class, s, 0.7)
        }

        Class::MusicTonal => mono(class, music_core(LEN, 0x5EED_0003), 0.7),

        Class::Percussive => mono(class, percussive_core(LEN, 0x5EED_0004), 0.7),

        Class::NoiseLike => mono(class, applause_core(LEN, 0x5EED_0005), 0.7),

        Class::StereoWide => {
            // Two *different* harmonic stacks hard-panned, plus a shared centre
            // element. Inter-channel correlation is deliberately low, so M/S wins
            // little and intensity stereo would collapse a real image.
            let mut l_src = music_core(LEN, 0x5EED_0006);
            let mut r_src = music_core(LEN, 0x5EED_0007);
            let mut centre = speech_core(LEN, 0x5EED_0008);
            // Unit-RMS each component so the weights below set the ENERGY balance.
            // With uncorrelated beds and a shared centre, the resulting
            // inter-channel correlation is w_c² / (w_m² + w_c²) — here
            // 0.45²/(1.0²+0.45²) ≈ 0.17, comfortably inside the "wide" regime
            // while still leaving a real centre image to collapse.
            rms_normalize(&mut l_src);
            rms_normalize(&mut r_src);
            rms_normalize(&mut centre);
            const W_MUSIC: f32 = 1.00;
            const W_CENTRE: f32 = 0.45;
            let mut pcm = Vec::with_capacity(LEN * 2);
            for i in 0..LEN {
                pcm.push(l_src[i] * W_MUSIC + centre[i] * W_CENTRE);
                pcm.push(r_src[i] * W_MUSIC + centre[i] * W_CENTRE);
            }
            normalize(&mut pcm, 0.7);
            Signal {
                class,
                sample_rate: SR,
                channels: 2,
                pcm,
            }
        }

        Class::QuietDynamic => {
            // GAP CLASS. First half at −40 dBFS, second half at full scale, with a
            // short ramp so the splice is not a click. The 40 dB step is the whole
            // point: a constant per-frame bit budget spends the same bits on both
            // halves, and a relative-only psy model treats them identically.
            let mut base = music_core(LEN, 0x5EED_0009);
            normalize(&mut base, 1.0);
            let half = LEN / 2;
            let ramp = SR as usize / 100; // 10 ms
            for (i, v) in base.iter_mut().enumerate() {
                let g = if i < half {
                    0.01 // −40 dBFS
                } else if i < half + ramp {
                    let a = (i - half) as f32 / ramp as f32;
                    0.01 + a * (0.7 - 0.01)
                } else {
                    0.7
                };
                *v *= g;
            }
            // NOT normalized — the absolute levels ARE the test.
            Signal {
                class,
                sample_rate: SR,
                channels: 1,
                pcm: base,
            }
        }

        Class::MixedSpeechMusic => {
            // GAP CLASS. Alternating 0.5 s blocks of speech and music: the content
            // character changes mid-stream, which is where a gate fitted on
            // whole-clip statistics falls apart.
            let mut sp = speech_core(LEN, 0x5EED_000A);
            let mut mu = music_core(LEN, 0x5EED_000B);
            normalize(&mut sp, 0.7);
            normalize(&mut mu, 0.7);
            let block = SR as usize / 2;
            let pcm = (0..LEN)
                .map(|i| if (i / block) % 2 == 0 { sp[i] } else { mu[i] })
                .collect();
            mono(class, pcm, 0.7)
        }
    }
}

/// The full corpus, fixed order.
pub fn corpus() -> Vec<Signal> {
    Class::all().into_iter().map(signal).collect()
}

/// Look up one signal by class name.
pub fn by_name(name: &str) -> Option<Signal> {
    Class::all().into_iter().find(|c| c.name() == name).map(signal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every class generates, is the declared length, and is finite.
    #[test]
    fn corpus_is_well_formed() {
        for s in corpus() {
            assert_eq!(s.frames(), LEN, "{}: wrong length", s.name());
            assert!(
                s.pcm.iter().all(|v| v.is_finite()),
                "{}: non-finite sample",
                s.name()
            );
            let peak = s.pcm.iter().fold(0f32, |a, &x| a.max(x.abs()));
            assert!(peak <= 1.0, "{}: clipped at {peak}", s.name());
            assert!(peak > 0.05, "{}: essentially silent ({peak})", s.name());
        }
    }

    /// Generation is deterministic — the property the whole ladder rests on.
    #[test]
    fn corpus_is_deterministic() {
        for (a, b) in corpus().iter().zip(corpus().iter()) {
            assert_eq!(a.pcm, b.pcm, "{}: not reproducible", a.name());
        }
    }

    /// The gap classes must actually have the physical property they exist for.
    #[test]
    fn gap_classes_have_their_premise() {
        // QuietDynamic: the two halves must differ by ~40 dB, or it cannot judge A4/A5.
        let q = signal(Class::QuietDynamic);
        let half = LEN / 2;
        let rms = |s: &[f32]| (s.iter().map(|&x| (x * x) as f64).sum::<f64>() / s.len() as f64).sqrt();
        let lo = rms(&q.pcm[..half]);
        let hi = rms(&q.pcm[half + 4410..]);
        let db = 20.0 * (hi / lo.max(1e-12)).log10();
        assert!(
            db > 30.0,
            "quiet-dynamic must span a wide level range, got {db:.1} dB"
        );

        // MixedSpeechMusic: consecutive half-second blocks must differ in character.
        // Spectral centroid is a cheap proxy that separates our speech from our music.
        let m = signal(Class::MixedSpeechMusic);
        let block = SR as usize / 2;
        let energy = |s: &[f32]| s.iter().map(|&x| (x * x) as f64).sum::<f64>();
        let e0 = energy(&m.pcm[..block]);
        let e1 = energy(&m.pcm[block..2 * block]);
        assert!(
            (e0 - e1).abs() / e0.max(e1).max(1e-12) > 0.05,
            "mixed content blocks are indistinguishable (e0={e0:.3e}, e1={e1:.3e})"
        );
    }

    /// Stereo-wide must actually be wide: low inter-channel correlation, or it
    /// cannot serve as the A7 anti-class.
    #[test]
    fn stereo_wide_is_decorrelated() {
        let s = signal(Class::StereoWide);
        assert_eq!(s.channels, 2);
        let p = s.planes();
        let (l, r) = (&p[0], &p[1]);
        let dot: f64 = l.iter().zip(r).map(|(&a, &b)| (a * b) as f64).sum();
        let nl: f64 = l.iter().map(|&a| (a * a) as f64).sum::<f64>().sqrt();
        let nr: f64 = r.iter().map(|&b| (b * b) as f64).sum::<f64>().sqrt();
        let xcorr = dot / (nl * nr).max(1e-12);
        assert!(
            xcorr < 0.75,
            "stereo-wide must be decorrelated, got xcorr={xcorr:.3}"
        );
    }

    /// Percussive must contain sharp attacks the detector can see.
    #[test]
    fn percussive_has_attacks() {
        let s = signal(Class::Percussive);
        // Sub-block (128-sample) energy profile must have a large peak-to-median ratio.
        let mut e: Vec<f64> = s
            .pcm
            .chunks(128)
            .map(|c| c.iter().map(|&x| (x * x) as f64).sum())
            .collect();
        let peak = e.iter().copied().fold(0f64, f64::max);
        e.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = e[e.len() / 2];
        assert!(
            peak / median.max(1e-12) > 100.0,
            "percussive needs sharp attacks, peak/median={:.1}",
            peak / median.max(1e-12)
        );
    }
}

