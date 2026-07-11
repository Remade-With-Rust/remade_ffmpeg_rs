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
/// Total kernel width (number of taps).
const WIDTH: usize = 2 * TAPS as usize;
/// Largest polyphase table (distinct sub-sample phases) we precompute. Every
/// common audio-rate pair reduces to a small denominator (44.1↔48k → 160,
/// 22.05→48k → 320, …); anything larger falls back to the scalar kernel.
const MAX_PHASES: usize = 8192;

/// Precomputed polyphase filter bank for a fixed rational ratio. The kernel
/// weights depend only on the sub-sample phase (and cutoff), both
/// signal-independent, and the phase set repeats *exactly* every `den` outputs
/// — so all per-output transcendentals collapse to a table lookup.
struct Poly {
    /// `den` phases × `WIDTH` taps, row-major (`weights[phase*WIDTH + k]`).
    weights: Vec<f64>,
    /// Reciprocal kernel-weight sum per phase (turns the per-output divide into
    /// a multiply). `1.0` where the sum was zero.
    inv_wsum: Vec<f64>,
    /// Phase step per output (= in_rate/gcd) and modulus (= out_rate/gcd).
    num: usize,
    den: usize,
}

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
    /// Polyphase bank + integer position/phase (present iff the ratio is
    /// rational with `den ≤ MAX_PHASES`). `None` → scalar per-output kernel.
    poly: Option<Poly>,
    ipos: usize,   // integer input index of the next output (mirrors pos.floor())
    iphase: usize, // sub-sample phase in [0, den)
    force_scalar: bool, // test-only: bypass the polyphase path to exercise the oracle
}

/// Greatest common divisor (Euclid).
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.max(1)
}

impl Resampler {
    /// Create a resampler converting `in_rate` → `out_rate` for `channels`.
    pub fn new(in_rate: u32, out_rate: u32, channels: u16) -> Resampler {
        let channels = channels.max(1) as usize;
        let in_rate = in_rate.max(1);
        let out_rate = out_rate.max(1);
        let ratio = out_rate as f64 / in_rate as f64;
        let cutoff = 0.5 * ratio.min(1.0);

        // Build the polyphase bank when the reduced denominator is small enough.
        let g = gcd(in_rate, out_rate);
        let den = (out_rate / g) as usize;
        let num = (in_rate / g) as usize;
        let poly = (den <= MAX_PHASES).then(|| {
            let mut weights = vec![0.0f64; den * WIDTH];
            let mut inv_wsum = vec![0.0f64; den];
            for p in 0..den {
                let frac = p as f64 / den as f64; // sub-sample offset in [0,1)
                let mut wsum = 0.0;
                for k in 0..WIDTH {
                    // Matches the scalar path's x = pos - j with j = center-TAPS+1+k:
                    // x = frac + (TAPS-1-k).
                    let x = frac + (TAPS - 1 - k as isize) as f64;
                    let w = kernel(x, cutoff);
                    weights[p * WIDTH + k] = w;
                    wsum += w;
                }
                inv_wsum[p] = if wsum == 0.0 { 1.0 } else { 1.0 / wsum };
            }
            Poly { weights, inv_wsum, num, den }
        });

        Resampler {
            out_rate,
            channels,
            ratio,
            cutoff,
            buf: vec![vec![0.0; TAPS as usize]; channels],
            pos: TAPS as f64,
            poly,
            ipos: TAPS as usize,
            iphase: 0,
            force_scalar: false,
        }
    }

    /// Test-only: force the scalar per-output kernel (the oracle) so the
    /// polyphase output can be gated against it.
    #[doc(hidden)]
    pub fn force_scalar_oracle(&mut self) {
        self.force_scalar = true;
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
        // Deinterleave into per-channel history, reserving up front. Specialise
        // the mono/stereo hot paths so the per-sample `i % ch` divide is gone.
        match ch {
            1 => {
                let b = &mut self.buf[0];
                b.reserve(input.len());
                for &s in input {
                    b.push(s as f64);
                }
            }
            2 => {
                let n = input.len() / 2;
                self.buf[0].reserve(n + 1);
                self.buf[1].reserve(n + 1);
                for frame in input.chunks_exact(2) {
                    self.buf[0].push(frame[0] as f64);
                    self.buf[1].push(frame[1] as f64);
                }
                if input.len() & 1 == 1 {
                    self.buf[0].push(input[input.len() - 1] as f64);
                }
            }
            _ => {
                for c in 0..ch {
                    self.buf[c].reserve(input.len() / ch + 1);
                }
                for (i, s) in input.iter().enumerate() {
                    self.buf[i % ch].push(*s as f64);
                }
            }
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
    /// trim consumed history. Dispatches to the precomputed polyphase bank when
    /// available (the common case), else the scalar per-output kernel.
    fn run(&mut self) -> Vec<f32> {
        if self.poly.is_some() && !self.force_scalar {
            return self.run_poly();
        }
        self.run_scalar()
    }

    /// Polyphase hot path: per output, look up the phase's precomputed weights
    /// and do the FIR dot product — zero transcendentals in steady state.
    fn run_poly(&mut self) -> Vec<f32> {
        let ch = self.channels;
        let len = self.buf[0].len();
        let mut out: Vec<f32> = Vec::new();
        let poly = self.poly.as_ref().unwrap();
        let (num, den) = (poly.num, poly.den);
        // Reserve output up front (avoid repeated Vec growth): each output
        // advances `ipos` by num/den on average, so consuming the available
        // input positions yields ≈ avail·den/num outputs per channel.
        let avail = len.saturating_sub(self.ipos + TAPS as usize);
        out.reserve(avail.saturating_mul(den) / num.max(1) * ch + ch);
        // An output at `ipos` needs input indices ipos-TAPS+1 ..= ipos+TAPS.
        while self.ipos + TAPS as usize + 1 <= len {
            let base = self.ipos + 1 - TAPS as usize;
            let w = &poly.weights[self.iphase * WIDTH..self.iphase * WIDTH + WIDTH];
            let inv = poly.inv_wsum[self.iphase];
            for c in 0..ch {
                let bc = &self.buf[c][base..base + WIDTH];
                out.push((dot_width(bc, w) * inv) as f32);
            }
            self.iphase += num;
            while self.iphase >= den {
                self.iphase -= den;
                self.ipos += 1;
            }
        }

        // Drop history we no longer need (keep TAPS-1 samples before `ipos`).
        let keep_from = (self.ipos as isize - TAPS + 1).max(0) as usize;
        if keep_from > 0 {
            for c in 0..ch {
                self.buf[c].drain(0..keep_from);
            }
            self.ipos -= keep_from;
        }
        out
    }

    /// Scalar per-output kernel (the correctness oracle + fallback for
    /// irrational / large-denominator ratios).
    fn run_scalar(&mut self) -> Vec<f32> {
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

/// `WIDTH`-tap FIR dot product `Σ a[k]·b[k]`. AVX2+FMA on x86 (4 independent
/// partial sums break the reduction dependency; FMA rounds once per term —
/// differs from the scalar chain by ~1 ULP/term, gated by SNR vs the oracle),
/// scalar elsewhere. Both slices are exactly `WIDTH` long.
#[inline]
fn dot_width(a: &[f64], b: &[f64]) -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            return unsafe { dot_width_avx2(a, b) };
        }
    }
    let mut acc = 0.0;
    for k in 0..WIDTH {
        acc += a[k] * b[k];
    }
    acc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_width_avx2(a: &[f64], b: &[f64]) -> f64 {
    use std::arch::x86_64::*;
    let (mut s0, mut s1, mut s2, mut s3) = (
        _mm256_setzero_pd(),
        _mm256_setzero_pd(),
        _mm256_setzero_pd(),
        _mm256_setzero_pd(),
    );
    let (pa, pb) = (a.as_ptr(), b.as_ptr());
    let mut i = 0;
    while i + 16 <= WIDTH {
        s0 = _mm256_fmadd_pd(_mm256_loadu_pd(pa.add(i)), _mm256_loadu_pd(pb.add(i)), s0);
        s1 = _mm256_fmadd_pd(_mm256_loadu_pd(pa.add(i + 4)), _mm256_loadu_pd(pb.add(i + 4)), s1);
        s2 = _mm256_fmadd_pd(_mm256_loadu_pd(pa.add(i + 8)), _mm256_loadu_pd(pb.add(i + 8)), s2);
        s3 = _mm256_fmadd_pd(_mm256_loadu_pd(pa.add(i + 12)), _mm256_loadu_pd(pb.add(i + 12)), s3);
        i += 16;
    }
    while i + 4 <= WIDTH {
        s0 = _mm256_fmadd_pd(_mm256_loadu_pd(pa.add(i)), _mm256_loadu_pd(pb.add(i)), s0);
        i += 4;
    }
    let s = _mm256_add_pd(_mm256_add_pd(s0, s1), _mm256_add_pd(s2, s3));
    let lo = _mm256_castpd256_pd128(s);
    let hi = _mm256_extractf128_pd(s, 1);
    let sum2 = _mm_add_pd(lo, hi);
    let hi2 = _mm_unpackhi_pd(sum2, sum2);
    _mm_cvtsd_f64(_mm_add_sd(sum2, hi2))
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
        let rms =
            (body.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / body.len() as f64).sqrt();
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
        let rms =
            (body.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / body.len() as f64).sqrt();
        assert!(rms < 0.1, "aliasing not suppressed: rms {rms:.3}");
    }

    #[test]
    fn polyphase_matches_scalar_oracle() {
        // The polyphase bank must reproduce the scalar per-output kernel to well
        // within float precision (the only difference is exact-rational phase vs
        // f64-accumulated phase — sub-ULP). Gate: SNR > 100 dB on a real signal.
        let input: Vec<f32> = (0..44_100 * 4)
            .flat_map(|i| {
                let t = i as f64 / 44_100.0;
                let s = (std::f64::consts::TAU * 997.0 * t).sin()
                    + 0.5 * (std::f64::consts::TAU * 5000.0 * t).sin();
                [(s * 0.4) as f32]
            })
            .collect();

        let mut poly = Resampler::new(44_100, 48_000, 1);
        let mut po = poly.process(&input);
        po.extend(poly.finish());

        let mut scal = Resampler::new(44_100, 48_000, 1);
        scal.force_scalar_oracle();
        let mut so = scal.process(&input);
        so.extend(scal.finish());

        assert_eq!(po.len(), so.len(), "poly/scalar length mismatch");
        let n = po.len().min(so.len());
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        for i in 0..n {
            let a = so[i] as f64;
            let d = po[i] as f64 - a;
            sig += a * a;
            err += d * d;
        }
        let snr = 10.0 * (sig / err.max(1e-30)).log10();
        assert!(snr > 100.0, "polyphase vs oracle SNR only {snr:.1} dB");
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
