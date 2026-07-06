//! Psychoacoustic model (brick 5): a masking threshold used as the floor-1 target.
//!
//! The floor is the perceptual noise floor — quantization noise sitting at the floor is
//! ~inaudible. So instead of an energy envelope (brick 3), the floor tracks a **masking
//! threshold**: per-Bark-band energy, spread across bands (a loud tone masks its neighbours),
//! sat a tonality-dependent ratio below the spread signal, and floored at a per-frame noise
//! floor. `quality` (0 = low, 1 = high) shifts the whole threshold — lower threshold → more
//! of the spectrum rises above the floor → more residue bits → higher quality.

/// Env-tunable psychoacoustic parameters, loaded once. Defaults reproduce the shipped model, so
/// an unset environment is a no-op; the `VORBIS_*` overrides drive PEAQ-gated A/B tuning.
pub(crate) struct PsyTune {
    pub smr_base: f32,   // base signal-to-mask ratio (dB) added to every band
    pub smr_tonal: f32,  // extra SMR (dB) for fully tonal bands (deeper mask → coded finer)
    pub spread_up: f32,  // spreading slope (dB/Bark) toward higher bands
    pub spread_dn: f32,  // spreading slope (dB/Bark) toward lower bands
    pub q_scale: f32,    // dB the quality knob shifts the threshold across [0,1]
    pub ceil: f32,       // floor ceiling as a fraction of frame peak
    pub nfloor: f32,     // noise floor as a fraction of frame peak (at DC)
    pub ath_tilt: f32,   // extra noise floor (dB, magnitude) at Nyquist vs DC — a crude ATH: the
    // ear can't hear quiet HF, so raise the floor there and don't waste bits coding it
    pub ps_gate: f32,    // point-stereo active below this quality
    pub ps_hz: f32,      // point-stereo cutoff frequency (Hz)
}

fn envf(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

pub(crate) fn tune() -> &'static PsyTune {
    use std::sync::OnceLock;
    static T: OnceLock<PsyTune> = OnceLock::new();
    T.get_or_init(|| PsyTune {
        smr_base: envf("VORBIS_SMR_BASE", 6.0),
        smr_tonal: envf("VORBIS_SMR_TONAL", 18.0),
        spread_up: envf("VORBIS_SPREAD_UP", -10.0),
        spread_dn: envf("VORBIS_SPREAD_DN", -27.0),
        q_scale: envf("VORBIS_Q_SCALE", 30.0),
        ceil: envf("VORBIS_CEIL", 0.5),
        nfloor: envf("VORBIS_NFLOOR", 1.0e-4),
        ath_tilt: envf("VORBIS_ATH_TILT", 60.0),
        ps_gate: envf("VORBIS_PS_GATE", 0.55),
        ps_hz: envf("VORBIS_PS_HZ", 5500.0),
    })
}

/// Traunmüller Hz→Bark (critical-band rate).
fn hz_to_bark(f: f32) -> f32 {
    let f = f.max(1.0);
    26.81 * f / (1960.0 + f) - 0.53
}

/// Energy spreading of a masker to a maskee `dz` Bark away: steep toward lower bands, gentle
/// upward (matches the AAC model). Slopes are tunable (`spread_up`/`spread_dn`).
fn spreading(dz: f32, t: &PsyTune) -> f32 {
    let slope_db = if dz >= 0.0 { t.spread_up } else { t.spread_dn };
    10f32.powf(slope_db * dz.abs() / 10.0)
}

/// Per-bin masking threshold (magnitude) for one channel's `m` MDCT coefficients.
/// `quality` in [0, 1] shifts the threshold (higher = lower threshold = more bits).
pub fn masking_curve(spectrum: &[f32], sample_rate: u32, quality: f32) -> Vec<f32> {
    let m = spectrum.len();
    let bin_hz = sample_rate as f32 / (2.0 * m as f32);

    // Partition bins into ~0.5-Bark bands.
    let mut band_start = vec![0usize];
    let mut last_bark = hz_to_bark(0.5 * bin_hz);
    for k in 1..m {
        let bark = hz_to_bark((k as f32 + 0.5) * bin_hz);
        if bark - last_bark >= 0.5 {
            band_start.push(k);
            last_bark = bark;
        }
    }
    band_start.push(m);
    let nb = band_start.len() - 1;

    // Per-band energy, Bark centre, and tonality (from spectral flatness).
    let mut energy = vec![0.0f32; nb];
    let mut bark_c = vec![0.0f32; nb];
    let mut tonal = vec![0.0f32; nb];
    for b in 0..nb {
        let (s, e) = (band_start[b], band_start[b + 1]);
        bark_c[b] = hz_to_bark((s + e) as f32 / 2.0 * bin_hz);
        let cnt = (e - s).max(1) as f32;
        let mut sum = 0.0f32;
        let mut logsum = 0.0f32;
        for &x in &spectrum[s..e] {
            let p = x * x + 1e-12;
            sum += p;
            logsum += p.ln();
        }
        energy[b] = sum;
        // Spectral flatness (geo/arith mean): ~1 = noise-like, ~0 = tonal.
        let geo = (logsum / cnt).exp();
        let arith = sum / cnt + 1e-20;
        let sfm_db = 10.0 * (geo / arith).clamp(1e-10, 1.0).log10();
        tonal[b] = (sfm_db / -60.0).clamp(0.0, 1.0);
    }

    // Spread + threshold per band. Tonal bands get a deeper mask (coded more precisely);
    // noise-like bands a shallower one (coded coarsely). `quality` shifts it globally.
    let t = tune();
    let q_db = (quality - 0.5) * t.q_scale;
    let mut thr = vec![0.0f32; nb];
    for i in 0..nb {
        let mut spread = 0.0f32;
        for j in 0..nb {
            spread += energy[j] * spreading(bark_c[i] - bark_c[j], t);
        }
        // Signal-to-mask ratio (dB), clamped ≥ 0 so the threshold never rises above the
        // spread signal — the loudest content is always coded (no total dropout at low `-q`).
        let offset_db = (t.smr_base + tonal[i] * t.smr_tonal + q_db).max(0.0);
        thr[i] = spread * 10f32.powf(-offset_db / 10.0);
    }

    // Per-bin magnitude threshold, bounded to `[noise_floor, ceil·peak]`. The lower bound is a
    // crude ATH; the upper bound keeps the floor below the frame peak so the loudest content
    // is always coded — dense signals whose summed masking exceeds every bin don't black out.
    let peak = spectrum.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    let noise_floor = peak * t.nfloor;
    let ceiling = peak * t.ceil;
    let nyq = sample_rate as f32 * 0.5;
    // The ATH tilt only helps at LOWER rates — at high `-q` the quiet HF is actually audible and
    // must be coded, so ramp the tilt fully off by q≈0.78 (measured: win ≤ q0.7, neutral by q0.8).
    let ath = t.ath_tilt * ((0.78 - quality) / 0.10).clamp(0.0, 1.0);
    let mut out = vec![noise_floor; m];
    for b in 0..nb {
        // Crude ATH: raise the noise floor toward Nyquist (∝ (f/nyq)²), so quiet high-frequency
        // content that sits below the raised floor isn't coded (inaudible → bits go elsewhere).
        let fc = (band_start[b] + band_start[b + 1]) as f32 / 2.0 * bin_hz;
        let frac = (fc / nyq).clamp(0.0, 1.0);
        let nf_b = noise_floor * 10f32.powf(ath * frac * frac / 20.0);
        let tv = thr[b].max(0.0).sqrt().clamp(nf_b, ceiling.max(nf_b));
        for o in out[band_start[b]..band_start[b + 1]].iter_mut() {
            *o = tv;
        }
    }
    out
}

/// Map a normalized quality `q01` in [0, 1] to the residue rate-distortion `lambda`
/// (higher quality → less rate pressure). Paired with the masking threshold's `quality`
/// shift, this gives one `-q` knob two coordinated effects.
pub fn lambda_for_quality(q01: f32) -> f32 {
    // 0.15 at q=0.5, ~0.04 at q=1, ~0.6 at q=0.
    0.15 * 2f32.powf((0.5 - q01) * 4.0)
}

/// Point-stereo cutoff bin: above this frequency the coupling angle is collapsed to a point
/// (mono). Collapsing *audible* high-frequency stereo is what actually saves bits (dropping
/// already-masked content saves nothing), so it is a genuine quality/rate trade reserved for
/// **low bitrate**: active only below q 0.5 with an aggressive ~6–9 kHz cutoff, and off (full
/// stereo preserved) at normal/high quality. A no-op for correlated stereo (angle already ~0).
pub fn point_stereo_bin(quality: f32, m: usize, sample_rate: u32) -> usize {
    let t = tune();
    if quality >= t.ps_gate {
        return m; // preserve full stereo at normal/high quality
    }
    // Low bitrate: collapse audible high-frequency stereo (from ~`ps_hz`) for bits.
    let bin = (t.ps_hz * 2.0 * m as f32 / sample_rate as f32) as usize;
    bin.min(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_drops_with_quality() {
        // A tone: higher quality must not raise the threshold anywhere (→ never fewer bits).
        let mut spec = vec![0.01f32; 1024];
        spec[40] = 1.0;
        let lo = masking_curve(&spec, 44_100, 0.2);
        let hi = masking_curve(&spec, 44_100, 0.8);
        for (l, h) in lo.iter().zip(&hi) {
            assert!(*h <= *l + 1e-6, "higher quality raised the threshold");
        }
    }

    #[test]
    fn masks_near_a_loud_tone() {
        // Spreading: a loud tone lifts the threshold of its spectral neighbours above a
        // distant quiet region's.
        let mut spec = vec![1e-4f32; 1024];
        spec[200] = 1.0;
        let curve = masking_curve(&spec, 44_100, 0.5);
        assert!(curve[210] > curve[900], "no masking spread near the tone");
    }

    #[test]
    fn lambda_monotonic_in_quality() {
        assert!(lambda_for_quality(0.0) > lambda_for_quality(0.5));
        assert!(lambda_for_quality(0.5) > lambda_for_quality(1.0));
    }
}
