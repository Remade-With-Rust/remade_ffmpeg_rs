//! Brick **N4** — the nonuniform quantizer power law, the lab's seed experiment.
//!
//! This is a *real* micro-experiment, not a placeholder: it measures the
//! requantization error of the MP3 `x^(3/4)` / `x^(4/3)` law as a function of the
//! rounding bias and quantizer step — exactly the knobs the rate loop tunes. It
//! runs today (every other brick is still `todo!()`), so it proves the whole
//! harness — corpus → variant → metric → report — works end to end.
//!
//! It is a *scalar* round-trip (quantize each line, requantize, compare); it is
//! not the full pipeline. When the real bricks land, each registers its own
//! variant table the same way and the runner dispatches to it.

use super::metrics::Metrics;
use super::signals::Signal;
use super::variant::Preset;

/// The quantizer's tunable parameters.
#[derive(Debug, Clone, Copy)]
pub struct QuantCfg {
    /// Quantizer step (stand-in for the `2^(-global_gain/4)` scale).
    pub step: f32,
    /// Rounding bias subtracted before `round()` — the ISO value is 0.0946.
    pub bias: f32,
    /// Maximum quantized level (ISO big-value magnitude cap is 8191).
    pub max_level: i32,
}

/// Variant table for N4. Add a row to test a new knob — no other changes needed.
pub static VARIANTS: &[Preset<QuantCfg>] = &[
    Preset {
        name: "iso",
        blurb: "ISO rounding bias 0.0946 (the spec value)",
        params: QuantCfg {
            step: 0.001,
            bias: 0.0946,
            max_level: 8191,
        },
    },
    Preset {
        name: "naive",
        blurb: "no rounding bias — round-to-nearest only",
        params: QuantCfg {
            step: 0.001,
            bias: 0.0,
            max_level: 8191,
        },
    },
    Preset {
        name: "half",
        blurb: "bias 0.5 — biases magnitudes downward",
        params: QuantCfg {
            step: 0.001,
            bias: 0.5,
            max_level: 8191,
        },
    },
    Preset {
        name: "coarse",
        blurb: "ISO bias, 4× coarser step (lower bitrate proxy)",
        params: QuantCfg {
            step: 0.004,
            bias: 0.0946,
            max_level: 8191,
        },
    },
];

/// Forward quantize one frequency line to an integer level.
pub fn quantize(cfg: QuantCfg, xr: f32) -> i32 {
    let m = (xr.abs() / cfg.step).powf(0.75) - cfg.bias;
    if m <= 0.0 {
        0
    } else {
        (m.round() as i32).min(cfg.max_level)
    }
}

/// Inverse: reconstruct the line from its level (sign carried separately, as in
/// the real bitstream).
pub fn requantize(cfg: QuantCfg, level: i32, sign: f32) -> f32 {
    sign * (level as f32).powf(4.0 / 3.0) * cfg.step
}

/// Round-trip a whole signal through the quantizer and score it.
pub fn eval(cfg: QuantCfg, sig: &Signal) -> Metrics {
    let out: Vec<f32> = sig
        .pcm
        .iter()
        .map(|&x| requantize(cfg, quantize(cfg, x), x.signum()))
        .collect();
    Metrics::compare(&sig.pcm, &out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::signals;

    #[test]
    fn zero_quantizes_to_zero() {
        for v in VARIANTS {
            assert_eq!(quantize(v.params, 0.0), 0);
        }
    }

    #[test]
    fn iso_bias_beats_no_quantizer_floor() {
        // A finite, sane PSNR on a tone — the harness produces a real number.
        let m = eval(VARIANTS[0].params, &signals::tone(1000.0));
        assert!(
            m.psnr_db.is_finite() && m.psnr_db > 20.0,
            "psnr={}",
            m.psnr_db
        );
        assert!(m.max_abs_err < 0.1);
    }

    #[test]
    fn coarser_step_loses_psnr() {
        let fine = eval(VARIANTS[0].params, &signals::white(1));
        let coarse = eval(VARIANTS[3].params, &signals::white(1));
        assert!(
            coarse.psnr_db < fine.psnr_db,
            "coarser step must be noisier"
        );
    }
}
