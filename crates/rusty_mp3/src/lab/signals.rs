//! Deterministic test corpus.
//!
//! Every signal is generated from a formula or a seeded LCG — no binary
//! fixtures, no wall-clock, no `rand`. Identical bytes on every machine and every
//! run, which is what makes an experiment *repeatable*. Each signal is sized to a
//! few MP3 granules so a full encode is cheap.

use core::f32::consts::PI;

/// One mono test signal, normalised to roughly `[-1, 1]`.
#[derive(Debug, Clone)]
pub struct Signal {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm: Vec<f32>,
}

/// Samples per corpus signal — 4 MPEG-1 granules (4 × 576 × 2).
pub const LEN: usize = 4608;
const SR: u32 = 44_100;

/// Seeded linear-congruential noise in `[-1, 1]` (glibc constants). Deterministic.
fn lcg(state: &mut u32) -> f32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    ((*state >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
}

fn sig(name: &str, pcm: Vec<f32>) -> Signal {
    Signal {
        name: name.to_string(),
        sample_rate: SR,
        channels: 1,
        pcm,
    }
}

/// Pure tone at `freq` Hz, amplitude 0.5 — the easy masking case.
pub fn tone(freq: f32) -> Signal {
    let pcm = (0..LEN)
        .map(|i| 0.5 * (2.0 * PI * freq * i as f32 / SR as f32).sin())
        .collect();
    sig(&format!("tone-{}hz", freq as u32), pcm)
}

/// Two simultaneous tones — exercises inter-tone masking.
pub fn two_tones(f1: f32, f2: f32) -> Signal {
    let pcm = (0..LEN)
        .map(|i| {
            let t = i as f32 / SR as f32;
            0.35 * (2.0 * PI * f1 * t).sin() + 0.35 * (2.0 * PI * f2 * t).sin()
        })
        .collect();
    sig(&format!("two-tones-{}+{}", f1 as u32, f2 as u32), pcm)
}

/// Linear sweep across the band — every scalefactor band sees energy.
pub fn sweep(f0: f32, f1: f32) -> Signal {
    let mut phase = 0.0f32;
    let mut pcm = Vec::with_capacity(LEN);
    for i in 0..LEN {
        let frac = i as f32 / LEN as f32;
        let f = f0 + (f1 - f0) * frac;
        phase += 2.0 * PI * f / SR as f32;
        pcm.push(0.5 * phase.sin());
    }
    sig("sweep", pcm)
}

/// Broadband white noise — the worst case for the rate loop (no maskers).
pub fn white(seed: u32) -> Signal {
    let mut s = seed;
    let pcm = (0..LEN).map(|_| 0.4 * lcg(&mut s)).collect();
    sig("white-noise", pcm)
}

/// Silence then a hard burst — the pre-echo / block-switch torture test.
pub fn transient() -> Signal {
    let mut s = 0x1234_5678u32;
    let pcm = (0..LEN)
        .map(|i| {
            if i < LEN / 2 {
                0.0
            } else {
                // a decaying noise burst right after the granule boundary
                let k = (i - LEN / 2) as f32;
                0.9 * lcg(&mut s) * (-k / 256.0).exp()
            }
        })
        .collect();
    sig("transient", pcm)
}

/// DC / near-zero — degenerate edge case for the quantizer and reservoir.
pub fn dc() -> Signal {
    sig("dc", vec![0.3; LEN])
}

/// The standard corpus: one of each kind, fixed order.
pub fn corpus() -> Vec<Signal> {
    vec![
        tone(1000.0),
        two_tones(440.0, 5000.0),
        sweep(50.0, 18_000.0),
        white(0xC0FFEE),
        transient(),
        dc(),
    ]
}

/// Look up a corpus signal by name (for `--signal` filtering).
pub fn by_name(name: &str) -> Option<Signal> {
    corpus().into_iter().find(|s| s.name == name)
}
