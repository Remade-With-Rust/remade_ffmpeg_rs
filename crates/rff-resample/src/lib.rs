//! Audio sample-rate conversion — the `swresample` equivalent.
//!
//! A streaming, band-limited resampler built on a windowed-sinc FIR kernel.
//! Feed interleaved `f32` samples with [`Resampler::process`]; at end of stream
//! call [`Resampler::finish`] to flush the filter tail. Works in `f64`
//! internally for precision and normalises each output by its kernel weight sum
//! so DC gain stays at unity for any ratio (up- or down-sampling).
//!
//! The cutoff tracks the lower of the input/output Nyquist limits, so
//! downsampling is anti-aliased rather than naively decimated.

/// Half-width of the FIR kernel, in input samples (so 2·TAPS taps total).
const TAPS: isize = 16;

/// A stateful interleaved-`f32` audio resampler from one rate to another.
pub struct Resampler {
    out_rate: u32,
    channels: usize,
    ratio: f64,  // out_rate / in_rate
    cutoff: f64, // normalised cutoff (cycles/input-sample), ≤ 0.5
    /// Per-channel pending input, each pre-padded with TAPS zeros of history.
    buf: Vec<Vec<f64>>,
    /// Read position (input-sample index into `buf`) of the next output sample.
    pos: f64,
}

impl Resampler {
    /// Create a resampler converting `in_rate` → `out_rate` for `channels`.
    pub fn new(in_rate: u32, out_rate: u32, channels: u16) -> Resampler {
        let channels = channels.max(1) as usize;
        let ratio = out_rate.max(1) as f64 / in_rate.max(1) as f64;
        Resampler {
            out_rate,
            channels,
            ratio,
            cutoff: 0.5 * ratio.min(1.0),
            buf: vec![vec![0.0; TAPS as usize]; channels],
            pos: TAPS as f64,
        }
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels as u16
    }

    /// Resample a block of interleaved `f32` samples, returning interleaved
    /// output. Output lags input by ~TAPS/ratio samples (flushed by `finish`).
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let ch = self.channels;
        for (i, s) in input.iter().enumerate() {
            self.buf[i % ch].push(*s as f64);
        }
        self.run()
    }

    /// Flush the remaining tail by feeding TAPS zero samples of input.
    pub fn finish(&mut self) -> Vec<f32> {
        for c in 0..self.channels {
            self.buf[c].extend(std::iter::repeat(0.0).take(TAPS as usize));
        }
        self.run()
    }

    /// Produce every output whose kernel window is fully covered by `buf`, then
    /// trim consumed history.
    fn run(&mut self) -> Vec<f32> {
        let ch = self.channels;
        let len = self.buf[0].len() as isize;
        let mut out: Vec<f32> = Vec::new();
        // An output at `pos` needs input indices floor(pos)-TAPS+1 ..= floor(pos)+TAPS.
        while (self.pos.floor() as isize) + TAPS < len {
            let center = self.pos.floor() as isize;
            // Kernel weights are identical across channels — compute once.
            let mut weights = [0.0f64; (2 * TAPS) as usize];
            let mut wsum = 0.0;
            for (k, w) in weights.iter_mut().enumerate() {
                let j = center - TAPS + 1 + k as isize;
                let x = self.pos - j as f64;
                *w = kernel(x, self.cutoff);
                wsum += *w;
            }
            if wsum == 0.0 {
                wsum = 1.0;
            }
            for c in 0..ch {
                let mut acc = 0.0;
                for (k, w) in weights.iter().enumerate() {
                    let j = (center - TAPS + 1 + k as isize) as usize;
                    acc += self.buf[c][j] * w;
                }
                out.push((acc / wsum) as f32);
            }
            self.pos += 1.0 / self.ratio;
        }

        // Drop history we no longer need (keep TAPS-1 samples before `pos`).
        let keep_from = (self.pos.floor() as isize - TAPS + 1).max(0);
        if keep_from > 0 {
            for c in 0..ch {
                self.buf[c].drain(0..keep_from as usize);
            }
            self.pos -= keep_from as f64;
        }
        out
    }
}

/// Windowed-sinc kernel value at distance `x` (input samples) for cutoff `fc`.
fn kernel(x: f64, fc: f64) -> f64 {
    let n = TAPS as f64;
    if x.abs() > n {
        return 0.0;
    }
    // Blackman window over [-TAPS, TAPS].
    let t = (x + n) / (2.0 * n); // 0..1
    let two_pi = std::f64::consts::TAU;
    let window = 0.42 - 0.5 * (two_pi * t).cos() + 0.08 * (2.0 * two_pi * t).cos();
    2.0 * fc * sinc(2.0 * fc * x) * window
}

/// Normalised sinc: sin(πx)/(πx), with sinc(0) = 1.
fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a sine at `freq` Hz sampled at `rate` for `n` samples (mono).
    fn sine(freq: f64, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    #[test]
    fn upsample_preserves_tone_length_and_energy() {
        // 44.1 kHz → 48 kHz: output length ≈ ratio, and a mid-band tone keeps
        // its amplitude (RMS of a full-scale sine ≈ 0.707).
        let input = sine(1000.0, 44_100, 44_100); // 1 s
        let mut rs = Resampler::new(44_100, 48_000, 1);
        let mut out = rs.process(&input);
        out.extend(rs.finish());

        let expected = (44_100.0 * 48_000.0 / 44_100.0) as usize;
        let drift = (out.len() as isize - expected as isize).abs();
        assert!(drift < 64, "length {} vs expected ~{expected}", out.len());

        // Skip the filter warm-up region before measuring RMS.
        let body = &out[2000..out.len() - 2000];
        let rms = (body.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / body.len() as f64).sqrt();
        assert!((rms - 0.707).abs() < 0.05, "rms {rms:.3}");
    }

    #[test]
    fn downsample_anti_aliases_above_output_nyquist() {
        // A 15 kHz tone sampled at 48 kHz, resampled to 16 kHz (Nyquist 8 kHz):
        // it must be filtered out, not aliased back into the band as a loud tone.
        let input = sine(15_000.0, 48_000, 48_000);
        let mut rs = Resampler::new(48_000, 16_000, 1);
        let mut out = rs.process(&input);
        out.extend(rs.finish());

        let body = &out[1000..out.len() - 1000];
        let rms = (body.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / body.len() as f64).sqrt();
        assert!(rms < 0.1, "aliasing not suppressed: rms {rms:.3}");
    }

    #[test]
    fn stereo_interleaving_is_preserved() {
        // Left = +0.5 DC, right = -0.5 DC. After resampling the channels must
        // stay separated (not bleed into each other).
        let input: Vec<f32> = (0..2000).flat_map(|_| [0.5f32, -0.5]).collect();
        let mut rs = Resampler::new(8000, 12000, 2);
        let mut out = rs.process(&input);
        out.extend(rs.finish());
        let mid = out.len() / 2 & !1; // even index
        assert!((out[mid] - 0.5).abs() < 0.02, "left {}", out[mid]);
        assert!((out[mid + 1] + 0.5).abs() < 0.02, "right {}", out[mid + 1]);
    }
}
