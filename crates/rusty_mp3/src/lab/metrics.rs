//! Quality metrics — how an experiment scores a brick's output against a
//! reference (the input PCM, or a decode of our own output).
//!
//! For the conformance bricks a single number suffices: `max_abs_err == 0` means
//! a bit-exact round-trip. For the quality bricks PSNR is the headline.

/// Comparison of an output signal against a reference of the same length.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    /// Samples compared.
    pub n: usize,
    /// Largest single-sample absolute error (0.0 ⇒ bit-exact round-trip).
    pub max_abs_err: f32,
    /// Root-mean-square error.
    pub rmse: f32,
    /// Peak-signal-to-noise ratio in dB (peak = 1.0 full-scale). `inf` if exact.
    pub psnr_db: f32,
}

impl Metrics {
    pub const ZERO: Metrics = Metrics {
        n: 0,
        max_abs_err: 0.0,
        rmse: 0.0,
        psnr_db: f32::INFINITY,
    };

    /// Compare `output` to `reference` over their common length.
    pub fn compare(reference: &[f32], output: &[f32]) -> Metrics {
        let n = reference.len().min(output.len());
        let mut max = 0.0f32;
        let mut sumsq = 0.0f64;
        for i in 0..n {
            let e = (reference[i] - output[i]).abs();
            if e > max {
                max = e;
            }
            sumsq += (e as f64) * (e as f64);
        }
        let rmse = if n > 0 {
            (sumsq / n as f64).sqrt() as f32
        } else {
            0.0
        };
        let psnr_db = if rmse > 0.0 {
            (20.0 * (1.0 / rmse as f64).log10()) as f32
        } else {
            f32::INFINITY
        };
        Metrics {
            n,
            max_abs_err: max,
            rmse,
            psnr_db,
        }
    }

    /// Per-sample-weighted mean of several metric rows (for a corpus summary).
    pub fn mean(rows: &[Metrics]) -> Metrics {
        if rows.is_empty() {
            return Metrics::ZERO;
        }
        let mut max = 0.0f32;
        let mut sumsq = 0.0f64;
        let mut n = 0usize;
        for m in rows {
            if m.max_abs_err > max {
                max = m.max_abs_err;
            }
            sumsq += (m.rmse as f64) * (m.rmse as f64) * m.n as f64;
            n += m.n;
        }
        let rmse = if n > 0 {
            (sumsq / n as f64).sqrt() as f32
        } else {
            0.0
        };
        let psnr_db = if rmse > 0.0 {
            (20.0 * (1.0 / rmse as f64).log10()) as f32
        } else {
            f32::INFINITY
        };
        Metrics {
            n,
            max_abs_err: max,
            rmse,
            psnr_db,
        }
    }

    /// PSNR as a JSON number, or `null` when infinite (bit-exact).
    pub fn psnr_json(&self) -> String {
        if self.psnr_db.is_finite() {
            format!("{:.3}", self.psnr_db)
        } else {
            "null".to_string()
        }
    }

    /// Compact one-line summary.
    pub fn summary(&self) -> String {
        let psnr = if self.psnr_db.is_finite() {
            format!("{:.2} dB", self.psnr_db)
        } else {
            "exact".to_string()
        };
        format!(
            "maxerr={:.6}  rmse={:.6}  psnr={psnr}",
            self.max_abs_err, self.rmse
        )
    }
}
