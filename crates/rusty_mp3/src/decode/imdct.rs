//! Hybrid synthesis stage 1: the inverse MDCT with windowing and overlap-add.
//!
//! Per subband (32), the 18 frequency lines run through an IMDCT — one 36-point
//! transform for long/start/stop blocks, or three 12-point transforms for short
//! blocks — then the matching window (Long/Start/Short/Stop). The first 18
//! samples overlap-add the previous granule's stored tail; the second 18 become
//! the next overlap. Odd subbands then get frequency inversion (every odd time
//! sample negated) to align with the synthesis filterbank.

use std::f64::consts::PI;
use std::sync::OnceLock;

use crate::frame::{BlockType, GranuleSideInfo, GRANULE_LINES, SUBBANDS, SUBBAND_LINES};

struct Kernels {
    cos36: [[f32; 18]; 36],
    cos12: [[f32; 6]; 12],
    /// Long(0), Start(1), Stop(3) windows; index 2 unused (short handled apart).
    win: [[f32; 36]; 4],
    win_short: [f32; 12],
}

fn kernels() -> &'static Kernels {
    static T: OnceLock<Kernels> = OnceLock::new();
    T.get_or_init(|| {
        let mut cos36 = [[0f32; 18]; 36];
        for (n, row) in cos36.iter_mut().enumerate() {
            for (k, c) in row.iter_mut().enumerate() {
                *c = (PI / 72.0 * (2 * n + 1 + 18) as f64 * (2 * k + 1) as f64).cos() as f32;
            }
        }
        let mut cos12 = [[0f32; 6]; 12];
        for (n, row) in cos12.iter_mut().enumerate() {
            for (k, c) in row.iter_mut().enumerate() {
                *c = (PI / 24.0 * (2 * n + 1 + 6) as f64 * (2 * k + 1) as f64).cos() as f32;
            }
        }
        let sin = |x: f64| x.sin() as f32;
        let mut win = [[0f32; 36]; 4];
        for n in 0..36 {
            win[0][n] = sin(PI / 36.0 * (n as f64 + 0.5)); // Long
        }
        for n in 0..18 {
            win[1][n] = sin(PI / 36.0 * (n as f64 + 0.5)); // Start
        }
        for n in 18..24 {
            win[1][n] = 1.0;
        }
        for n in 24..30 {
            win[1][n] = sin(PI / 12.0 * ((n - 18) as f64 + 0.5));
        }
        for n in 6..12 {
            win[3][n] = sin(PI / 12.0 * ((n - 6) as f64 + 0.5)); // Stop
        }
        for n in 12..18 {
            win[3][n] = 1.0;
        }
        for n in 18..36 {
            win[3][n] = sin(PI / 36.0 * (n as f64 + 0.5));
        }
        let mut win_short = [0f32; 12];
        for (n, w) in win_short.iter_mut().enumerate() {
            *w = sin(PI / 12.0 * (n as f64 + 0.5));
        }
        Kernels {
            cos36,
            cos12,
            win,
            win_short,
        }
    })
}

/// Run the hybrid IMDCT for one channel's granule. `overlap` holds the previous
/// granule's tail on entry and is updated with this granule's tail on exit.
/// Returns 576 time-domain values (subband-major) for the synthesis stage.
pub fn hybrid(
    gi: &GranuleSideInfo,
    lines: &[f32; GRANULE_LINES],
    overlap: &mut [f32; GRANULE_LINES],
) -> [f32; GRANULE_LINES] {
    let t = kernels();
    let mut out = [0f32; GRANULE_LINES];
    let is_short = gi.window_switching && gi.block_type == BlockType::Short;

    for sb in 0..SUBBANDS {
        let base = sb * SUBBAND_LINES;
        let mut samp = [0f32; 36];
        // Mixed blocks keep the lowest two subbands long.
        let short_here = is_short && !(gi.mixed_block && sb < 2);

        if short_here {
            for w in 0..3 {
                let mut y = [0f32; 12];
                for n in 0..12 {
                    let mut acc = 0f32;
                    for k in 0..6 {
                        acc += lines[base + w + 3 * k] * t.cos12[n][k];
                    }
                    y[n] = acc * t.win_short[n];
                }
                for n in 0..12 {
                    samp[6 + w * 6 + n] += y[n];
                }
            }
        } else {
            let wt = match gi.block_type {
                BlockType::Start => 1,
                BlockType::Stop => 3,
                _ => 0,
            };
            for n in 0..36 {
                let mut acc = 0f32;
                for k in 0..18 {
                    acc += lines[base + k] * t.cos36[n][k];
                }
                samp[n] = acc * t.win[wt][n];
            }
        }

        // Overlap-add: first half with the saved tail, second half becomes tail.
        for n in 0..18 {
            out[base + n] = samp[n] + overlap[base + n];
            overlap[base + n] = samp[n + 18];
        }
        // Frequency inversion: negate odd samples of odd subbands.
        if sb & 1 == 1 {
            let mut i = 1;
            while i < 18 {
                out[base + i] = -out[base + i];
                i += 2;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_imdct_impulse_matches_formula() {
        // A single coefficient in subband 0 → out[n] = cos36[n][0]·win_long[n]
        // (overlap starts at zero).
        let mut lines = [0f32; GRANULE_LINES];
        lines[0] = 1.0;
        let mut overlap = [0f32; GRANULE_LINES];
        let out = hybrid(&GranuleSideInfo::default(), &lines, &mut overlap);
        let expected = (PI / 72.0 * 19.0).cos() as f32 * (PI / 36.0 * 0.5).sin() as f32;
        assert!(
            (out[0] - expected).abs() < 1e-5,
            "out[0]={} expected={}",
            out[0],
            expected
        );
        // The second half of the 36-sample frame is saved as the next overlap.
        let expect_ov = (PI / 72.0 * 55.0).cos() as f32 * (PI / 36.0 * 18.5).sin() as f32;
        assert!((overlap[0] - expect_ov).abs() < 1e-5);
    }

    #[test]
    fn odd_subband_frequency_inversion() {
        // Subband 1 (odd): odd-indexed output samples are negated vs the raw IMDCT.
        let mut lines = [0f32; GRANULE_LINES];
        lines[SUBBAND_LINES] = 1.0; // subband 1, coefficient 0
        let mut overlap = [0f32; GRANULE_LINES];
        let out = hybrid(&GranuleSideInfo::default(), &lines, &mut overlap);
        let raw1 = (PI / 72.0 * (2.0 + 1.0 + 18.0)).cos() as f32 * (PI / 36.0 * 1.5).sin() as f32;
        assert!(
            (out[SUBBAND_LINES + 1] + raw1).abs() < 1e-5,
            "odd sample must be negated"
        );
    }
}
