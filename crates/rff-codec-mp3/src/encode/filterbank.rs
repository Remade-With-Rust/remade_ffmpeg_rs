//! Brick **L1** — the analysis polyphase filterbank, the inverse of decode's
//! synthesis stage (`decode/synthesis.rs`).
//!
//! Splits 32 PCM samples into 32 subband samples per pass (18 passes per
//! granule). Each pass shifts 32 new samples into a 512-tap FIFO, windows it with
//! the analysis window `C[]`, folds 512→64, then a 32×64 cosine matrix produces
//! the 32 subband outputs.
//!
//! Two ISO facts make this exact rather than guessed:
//! * The analysis window is the synthesis window scaled: `C[i] = D[i] / 32`
//!   (ISO/IEC 11172-3) — so we derive it from the already-sourced [`SYNTH_D`]
//!   rather than transcribing a second table.
//! * The analysis matrix `M[k][i] = cos((2k+1)(i−16)·π/64)` is the cosine-modulated
//!   partner of the decoder's synthesis matrix `N[i][k] = cos((16+i)(2k+1)·π/64)`.
//!   The two together are a (near-)perfect-reconstruction pseudo-QMF pair, which
//!   is exactly the round-trip the test exercises.

use std::f64::consts::PI;
use std::sync::OnceLock;

use crate::decode::synth_window::SYNTH_D;
use crate::frame::{SUBBANDS, SUBBAND_LINES};

/// Analysis window `C[i] = D[i] / 32` (ISO identity vs. the synthesis window).
fn window() -> &'static [f32; 512] {
    static C: OnceLock<[f32; 512]> = OnceLock::new();
    C.get_or_init(|| {
        let mut c = [0f32; 512];
        for (i, ci) in c.iter_mut().enumerate() {
            *ci = SYNTH_D[i] / 32.0;
        }
        c
    })
}

/// Analysis matrixing `M[k][i] = cos((2k+1)(i−16)·π/64)`, 32×64.
fn matrix() -> &'static [[f32; 64]; SUBBANDS] {
    static M: OnceLock<[[f32; 64]; SUBBANDS]> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = [[0f32; 64]; SUBBANDS];
        for (k, row) in m.iter_mut().enumerate() {
            for (i, c) in row.iter_mut().enumerate() {
                *c = (PI / 64.0 * (2 * k + 1) as f64 * (i as f64 - 16.0)).cos() as f32;
            }
        }
        m
    })
}

/// Analyze one granule of mono PCM (`pcm[0..576]`) into subband samples
/// `[subband][line]`, advancing the channel's filterbank FIFO `X[]`.
pub fn analyze(pcm: &[f32], fifo: &mut [f32; 512]) -> [[f32; SUBBAND_LINES]; SUBBANDS] {
    let c = window();
    let m = matrix();
    let mut out = [[0f32; SUBBAND_LINES]; SUBBANDS];

    for v in 0..SUBBAND_LINES {
        // Shift the FIFO up by 32 and push the 32 new samples in, newest at X[0]
        // (ISO: X[i]=X[i-32]; then X[31]..X[0] take this pass's samples in order).
        fifo.copy_within(0..512 - 32, 32);
        for t in 0..32 {
            fifo[31 - t] = pcm[v * 32 + t];
        }

        // Window + fold 512 → 64: Y[i] = Σ_{j=0..7} C[i+64j]·X[i+64j].
        let mut y = [0f32; 64];
        for (i, yi) in y.iter_mut().enumerate() {
            let mut acc = 0f32;
            for j in 0..8 {
                acc += c[i + 64 * j] * fifo[i + 64 * j];
            }
            *yi = acc;
        }

        // Matrix 64 → 32: S[k] = Σ_{i=0..63} M[k][i]·Y[i].
        for k in 0..SUBBANDS {
            let mut acc = 0f32;
            for i in 0..64 {
                acc += m[k][i] * y[i];
            }
            out[k][v] = acc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::synthesis;
    use crate::frame::{GRANULE_LINES, SUBBANDS, SUBBAND_LINES};
    use std::f32::consts::PI as PIf;

    /// Flatten subband-major `[subband][line]` into the decoder's `time[k*18+v]`.
    fn flatten(sb: &[[f32; SUBBAND_LINES]; SUBBANDS]) -> [f32; GRANULE_LINES] {
        let mut t = [0f32; GRANULE_LINES];
        for k in 0..SUBBANDS {
            for v in 0..SUBBAND_LINES {
                t[k * SUBBAND_LINES + v] = sb[k][v];
            }
        }
        t
    }

    /// Drive a tone through analysis→synthesis and find the best-aligned
    /// reconstruction SNR (the filterbank has an inherent delay).
    #[test]
    fn analysis_synthesis_reconstructs_a_tone() {
        let granules = 16;
        let n = granules * GRANULE_LINES;
        let input: Vec<f32> = (0..n)
            .map(|i| 0.5 * (2.0 * PIf * 1000.0 * i as f32 / 44100.0).sin())
            .collect();

        let mut afifo = [0f32; 512];
        let mut sfifo = [0f32; 1024];
        let mut output = Vec::with_capacity(n);
        for g in 0..granules {
            let sb = analyze(&input[g * GRANULE_LINES..], &mut afifo);
            let pcm = synthesis::polyphase(&flatten(&sb), &mut sfifo);
            output.extend_from_slice(&pcm);
        }

        // Search the small delay range for the best reconstruction.
        let (mut best_snr, mut best_delay) = (f64::NEG_INFINITY, 0usize);
        for delay in 480..=482 {
            let mut sig = 0f64;
            let mut err = 0f64;
            for i in delay..n {
                let r = input[i - delay] as f64;
                let o = output[i] as f64;
                sig += r * r;
                err += (r - o) * (r - o);
            }
            let snr = 10.0 * (sig / err).log10();
            if snr > best_snr {
                best_snr = snr;
                best_delay = delay;
            }
        }
        eprintln!("[L1] best reconstruction SNR {best_snr:.1} dB at delay {best_delay}");
        // The MPEG pseudo-QMF reconstructs to better than ~80 dB; unity gain.
        assert!(
            best_snr > 70.0,
            "analysis/synthesis SNR too low: {best_snr:.1} dB"
        );
    }

    #[test]
    fn dc_maps_to_subband_zero() {
        // A DC input lands entirely in subband 0; higher subbands stay ~silent
        // once the FIFO has primed.
        let input = [1.0f32; GRANULE_LINES * 4];
        let mut afifo = [0f32; 512];
        let mut sb = [[0f32; SUBBAND_LINES]; SUBBANDS];
        for g in 0..4 {
            sb = analyze(&input[g * GRANULE_LINES..], &mut afifo);
        }
        let sb0: f32 = sb[0].iter().map(|x| x.abs()).sum();
        let high: f32 = (1..SUBBANDS)
            .map(|k| sb[k].iter().map(|x| x.abs()).sum::<f32>())
            .sum();
        assert!(
            sb0 > high,
            "DC energy must concentrate in subband 0 (sb0={sb0}, high={high})"
        );
    }
}
