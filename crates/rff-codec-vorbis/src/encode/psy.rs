//! Psychoacoustic model (brick 5): a masking threshold used as the floor-1 target.
//!
//! The floor is the perceptual noise floor — quantization noise sitting at the floor is
//! ~inaudible. So instead of an energy envelope (brick 3), the floor tracks a **masking
//! threshold**: per-Bark-band energy, spread across bands (a loud tone masks its neighbours),
//! sat a tonality-dependent ratio below the spread signal, and floored at a per-frame noise
//! floor. `quality` (0 = low, 1 = high) shifts the whole threshold — lower threshold → more
//! of the spectrum rises above the floor → more residue bits → higher quality.

/// Traunmüller Hz→Bark (critical-band rate).
fn hz_to_bark(f: f32) -> f32 {
    let f = f.max(1.0);
    26.81 * f / (1960.0 + f) - 0.53
}

/// Energy spreading of a masker to a maskee `dz` Bark away: steep ~27 dB/Bark toward lower
/// bands, gentle ~10 dB/Bark upward (matches the AAC model).
fn spreading(dz: f32) -> f32 {
    let slope_db = if dz >= 0.0 { -10.0 } else { -27.0 };
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
    let q_db = (quality - 0.5) * 30.0;
    let mut thr = vec![0.0f32; nb];
    for i in 0..nb {
        let mut spread = 0.0f32;
        for j in 0..nb {
            spread += energy[j] * spreading(bark_c[i] - bark_c[j]);
        }
        // Signal-to-mask ratio (dB), clamped ≥ 0 so the threshold never rises above the
        // spread signal — the loudest content is always coded (no total dropout at low `-q`).
        let offset_db = (6.0 + tonal[i] * 18.0 + q_db).max(0.0);
        thr[i] = spread * 10f32.powf(-offset_db / 10.0);
    }

    // Per-bin magnitude threshold, bounded to `[noise_floor, 0.5·peak]`. The lower bound is a
    // crude ATH; the upper bound keeps the floor below the frame peak so the loudest content
    // is always coded — dense signals whose summed masking exceeds every bin don't black out.
    let peak = spectrum.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    let noise_floor = peak * 1.0e-4;
    let ceiling = peak * 0.5;
    let mut out = vec![noise_floor; m];
    for b in 0..nb {
        let t = thr[b].max(0.0).sqrt().clamp(noise_floor, ceiling.max(noise_floor));
        for o in out[band_start[b]..band_start[b + 1]].iter_mut() {
            *o = t;
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
    if quality >= 0.55 {
        return m; // preserve full stereo at normal/high quality
    }
    // Low bitrate: collapse audible high-frequency stereo (from ~5.5 kHz) for bits.
    let point_hz = 5500.0;
    let bin = (point_hz * 2.0 * m as f32 / sample_rate as f32) as usize;
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
