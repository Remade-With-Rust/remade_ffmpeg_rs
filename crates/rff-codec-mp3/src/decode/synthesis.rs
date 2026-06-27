//! Hybrid synthesis stage 2: the 32-band polyphase synthesis filterbank.
//!
//! Each of the 18 passes takes 32 subband samples and produces 32 PCM samples.
//! A 32→64 cosine matrixing feeds the 1024-sample FIFO `V[]`; a windowing step
//! gathers `U[]` from `V[]`, multiplies by the 512-tap `D[]` window, and sums to
//! 32 outputs. The matrixing is computed here; `D[]` is tabulated in [`tables`].

use std::f64::consts::PI;
use std::sync::OnceLock;

use crate::frame::{GRANULE_LINES, SUBBANDS, SUBBAND_LINES};
use crate::tables;

/// Matrixing coefficients `N[i][k] = cos((16+i)·(2k+1)·π/64)`, 64×32.
fn matrix() -> &'static [[f32; SUBBANDS]; 64] {
    static T: OnceLock<[[f32; SUBBANDS]; 64]> = OnceLock::new();
    T.get_or_init(|| {
        let mut n = [[0f32; SUBBANDS]; 64];
        for (i, row) in n.iter_mut().enumerate() {
            for (k, c) in row.iter_mut().enumerate() {
                *c = (PI / 64.0 * (16 + i) as f64 * (2 * k + 1) as f64).cos() as f32;
            }
        }
        n
    })
}

/// Run the synthesis filterbank for one channel's granule (subband-major `time`),
/// returning 576 PCM samples. `fifo` is the persistent `V[]` state.
pub fn polyphase(time: &[f32; GRANULE_LINES], fifo: &mut [f32; 1024]) -> [f32; GRANULE_LINES] {
    let n = matrix();
    let d = &tables::SYNTH_D;
    let mut pcm = [0f32; GRANULE_LINES];

    for v in 0..SUBBAND_LINES {
        // Gather this pass's 32 subband samples.
        let mut s = [0f32; SUBBANDS];
        for (k, sv) in s.iter_mut().enumerate() {
            *sv = time[k * SUBBAND_LINES + v];
        }
        // Shift V down by 64, then matrix the new 64 values into the front.
        fifo.copy_within(0..1024 - 64, 64);
        for (i, fi) in fifo.iter_mut().take(64).enumerate() {
            let mut acc = 0f32;
            for k in 0..SUBBANDS {
                acc += n[i][k] * s[k];
            }
            *fi = acc;
        }
        // Build U from V, window with D, sum 16 taps → one PCM sample per j.
        for j in 0..32 {
            let mut sum = 0f32;
            for i in 0..8 {
                sum += fifo[i * 128 + j] * d[i * 64 + j];
                sum += fifo[i * 128 + 96 + j] * d[i * 64 + 32 + j];
            }
            pcm[v * 32 + j] = sum;
        }
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_matches_cosine_formula() {
        let n = matrix();
        // Spot-check a couple of entries against N[i][k] = cos((16+i)(2k+1)π/64).
        let e = |i: usize, k: usize| (PI / 64.0 * (16 + i) as f64 * (2 * k + 1) as f64).cos() as f32;
        assert!((n[0][0] - e(0, 0)).abs() < 1e-6);
        assert!((n[33][7] - e(33, 7)).abs() < 1e-6);
        assert!((n[63][31] - e(63, 31)).abs() < 1e-6);
    }

    #[test]
    fn fifo_advances_and_output_is_finite() {
        // Feed every pass (all-ones) so the FIFO stays populated across the 18
        // shifts. With the placeholder D (zeros) the PCM is zero but finite; the
        // matrixing/FIFO must run cleanly and leave the FIFO non-empty.
        let time = [1f32; GRANULE_LINES];
        let mut fifo = [0f32; 1024];
        let pcm = polyphase(&time, &mut fifo);
        assert!(pcm.iter().all(|v| v.is_finite()));
        assert!(fifo.iter().any(|&v| v != 0.0), "matrixing must fill the FIFO");
    }
}
