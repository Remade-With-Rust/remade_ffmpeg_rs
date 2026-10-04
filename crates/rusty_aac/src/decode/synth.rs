//! Synthesis filterbank: half-length IMDCT + windowed overlap-add, for every
//! frame length the AAC family uses — 1024/960 (GA: long, start, stop, eight
//! short), 512/480 (ER AAC-LD, including its low-overlap window) and the
//! low-delay ELD filterbank — plus the LTP analysis/history hooks.
//!
//! The overlap logic follows the reference decoder's buffer discipline exactly:
//! each channel keeps `saved` (the not-yet-overlapped second half of the last
//! transform) and every frame is `out = window(saved, this_half)`. Window *shape*
//! transitions take the previous frame's shape on the rising half, and the
//! "meaningless" long↔short transitions some encoders emit are treated as
//! short↔short, which is what keeps such streams sample-identical to FFmpeg.

use crate::dsp;
use crate::ics::WindowSequence;

/// The ISO reconstruction is on a ~16-bit scale; output is float [-1, 1].
const OUT_NORM: f64 = 1.0 / 32768.0;

/// Half windows (rising half, `len` samples) for one frame length.
pub struct Windows {
    pub long_sine: Vec<f32>,
    pub long_kbd: Vec<f32>,
    pub short_sine: Vec<f32>,
    pub short_kbd: Vec<f32>,
}

impl Windows {
    pub fn new(frame_len: usize) -> Windows {
        let short = frame_len / 8;
        Windows {
            long_sine: dsp::sine_window(2 * frame_len)[..frame_len].to_vec(),
            long_kbd: dsp::kbd_window(2 * frame_len, 4.0)[..frame_len].to_vec(),
            short_sine: dsp::sine_window(2 * short)[..short].to_vec(),
            short_kbd: dsp::kbd_window(2 * short, 6.0)[..short].to_vec(),
        }
    }
    fn long(&self, kbd: bool) -> &[f32] {
        if kbd {
            &self.long_kbd
        } else {
            &self.long_sine
        }
    }
    fn short(&self, kbd: bool) -> &[f32] {
        if kbd {
            &self.short_kbd
        } else {
            &self.short_sine
        }
    }
}

/// `vector_fmul_window`: overlap a falling `src0` tail with a rising `src1` head
/// through the half window `win` (`2·len` samples), writing `2·len` outputs.
#[inline]
pub fn fmul_window(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
    for t in 0..len {
        let jj = 2 * len - 1 - t;
        let s0 = src0[t];
        let s1 = src1[len - 1 - t];
        let wi = win[t];
        let wj = win[jj];
        dst[t] = s0 * wj - s1 * wi;
        dst[jj] = s0 * wi + s1 * wj;
    }
}

fn is_long_end(s: WindowSequence) -> bool {
    matches!(s, WindowSequence::OnlyLong | WindowSequence::LongStop)
}

/// The inputs to one channel's synthesis.
pub struct SynthIn<'a> {
    pub coeffs: &'a [f32],
    pub seq: WindowSequence,
    pub prev_seq: WindowSequence,
    pub kbd: bool,
    pub prev_kbd: bool,
}

/// GA synthesis for frame length `L` (1024 or 960). `buf` (len `L`) receives the
/// half IMDCT (reused by LTP), `out` (len `L`) the PCM, `saved` (len `L/2`) is
/// the overlap state.
pub fn imdct_and_window(
    w: &Windows,
    l: usize,
    s: &SynthIn,
    buf: &mut [f32],
    saved: &mut [f32],
    out: &mut [f32],
) {
    let sh = l / 8; // short transform length (128/120)
    let hs = sh / 2;
    let ov = l / 2 - hs; // 448 / 420
    let swin = w.short(s.kbd);
    let swin_prev = w.short(s.prev_kbd);
    let lwin_prev = w.long(s.prev_kbd);

    if s.seq == WindowSequence::EightShort {
        for i in 0..8 {
            // Coefficients are laid out at a stride of 128 per window even when
            // the short transform is 120 long.
            dsp::imdct_half(&s.coeffs[i * 128..i * 128 + sh], &mut buf[i * sh..(i + 1) * sh], OUT_NORM);
        }
    } else {
        dsp::imdct_half(&s.coeffs[..l], &mut buf[..l], OUT_NORM);
    }

    let mut temp = vec![0f32; sh];
    if is_long_end(s.prev_seq)
        && matches!(s.seq, WindowSequence::OnlyLong | WindowSequence::LongStart)
    {
        fmul_window(out, saved, buf, lwin_prev, l / 2);
    } else {
        out[..ov].copy_from_slice(&saved[..ov]);
        if s.seq == WindowSequence::EightShort {
            fmul_window(&mut out[ov..], &saved[ov..], &buf[..], swin_prev, hs);
            for k in 1..4 {
                let (a, b) = ((k - 1) * sh + hs, k * sh);
                let (src0, src1) = (buf[a..a + hs].to_vec(), buf[b..b + hs].to_vec());
                fmul_window(&mut out[ov + k * sh..], &src0, &src1, swin, hs);
            }
            let (src0, src1) = (buf[3 * sh + hs..4 * sh].to_vec(), buf[4 * sh..4 * sh + hs].to_vec());
            fmul_window(&mut temp, &src0, &src1, swin, hs);
            out[ov + 4 * sh..ov + 4 * sh + hs].copy_from_slice(&temp[..hs]);
        } else {
            fmul_window(&mut out[ov..], &saved[ov..], &buf[..], swin_prev, hs);
            out[ov + sh..ov + sh + ov].copy_from_slice(&buf[hs..hs + ov]);
        }
    }

    match s.seq {
        WindowSequence::EightShort => {
            saved[..hs].copy_from_slice(&temp[hs..sh]);
            for k in 0..3 {
                let (a, b) = ((4 + k) * sh + hs, (5 + k) * sh);
                let (src0, src1) = (buf[a..a + hs].to_vec(), buf[b..b + hs].to_vec());
                fmul_window(&mut saved[hs + k * sh..], &src0, &src1, swin, hs);
            }
            saved[ov..ov + hs].copy_from_slice(&buf[7 * sh + hs..8 * sh]);
        }
        WindowSequence::LongStart => {
            saved[..ov].copy_from_slice(&buf[l / 2..l / 2 + ov]);
            saved[ov..ov + hs].copy_from_slice(&buf[l - hs..l]);
        }
        _ => saved[..l / 2].copy_from_slice(&buf[l / 2..l]),
    }
}

/// ER AAC-LD synthesis (frame length `n` = 512/480, long windows only). With the
/// KBD flag set, LD uses its LOW-OVERLAP window instead.
pub fn imdct_and_window_ld(
    long_sine: &[f32],
    short_sine_lo: &[f32],
    n: usize,
    coeffs: &[f32],
    prev_low_overlap: bool,
    buf: &mut [f32],
    saved: &mut [f32],
    out: &mut [f32],
) {
    dsp::imdct_half(&coeffs[..n], &mut buf[..n], OUT_NORM);
    if prev_low_overlap {
        let lo = short_sine_lo.len(); // 128 (or 120 at 480)
        let flat = (n - lo) / 2; // 192 / 180
        out[..flat].copy_from_slice(&saved[..flat]);
        fmul_window(&mut out[flat..], &saved[flat..], buf, short_sine_lo, lo / 2);
        out[flat + lo..n].copy_from_slice(&buf[lo / 2..lo / 2 + (n - flat - lo)]);
    } else {
        fmul_window(out, saved, buf, long_sine, n / 2);
    }
    saved[..n / 2].copy_from_slice(&buf[n / 2..n]);
}

/// ER AAC-ELD low-delay synthesis (frame length `n` = 512/480): an IMDCT of the
/// shuffled spectrum, then the 4n-tap low-delay window over three frames of
/// history. `saved` holds `3n` samples.
pub fn imdct_and_window_eld(window: &[f32], n: usize, coeffs: &mut [f32], buf: &mut [f32], saved: &mut [f32], out: &mut [f32]) {
    let n2 = n / 2;
    let n4 = n / 4;
    let mut i = 0;
    while i < n2 {
        let t = coeffs[i];
        coeffs[i] = -coeffs[n - 1 - i];
        coeffs[n - 1 - i] = t;
        let t = -coeffs[i + 1];
        coeffs[i + 1] = coeffs[n - 2 - i];
        coeffs[n - 2 - i] = t;
        i += 2;
    }
    dsp::imdct_half(&coeffs[..n], &mut buf[..n], OUT_NORM);
    let mut i = 0;
    while i < n {
        buf[i] = -2.0 * buf[i];
        buf[i + 1] = 2.0 * buf[i + 1];
        i += 2;
    }
    for i in n4..n2 {
        out[i - n4] = buf[n2 - 1 - i] * window[i - n4]
            + saved[i + n2] * window[i + n - n4]
            + -saved[n + n2 - 1 - i] * window[i + 2 * n - n4]
            + -saved[2 * n + n2 + i] * window[i + 3 * n - n4];
    }
    for i in 0..n2 {
        out[n4 + i] = buf[i] * window[i + n2 - n4]
            + -saved[n - 1 - i] * window[i + n2 + n - n4]
            + -saved[n + i] * window[i + n2 + 2 * n - n4]
            + saved[2 * n + n - 1 - i] * window[i + n2 + 3 * n - n4];
    }
    for i in 0..n4 {
        out[n2 + n4 + i] = buf[i + n2] * window[i + n - n4]
            + -saved[n2 - 1 - i] * window[i + 2 * n - n4]
            + -saved[n + n2 + i] * window[i + 3 * n - n4];
    }
    saved.copy_within(0..2 * n, n);
    saved[..n].copy_from_slice(&buf[..n]);
}

/// LTP: window the predicted time signal with the CURRENT frame's window shape
/// sequence and take the forward MDCT (2048 → 1024), in the decoder's coefficient
/// domain.
pub fn ltp_analysis(w: &Windows, s: &SynthIn, input: &mut [f32]) -> Vec<f32> {
    let lwin = w.long(s.kbd);
    let swin = w.short(s.kbd);
    let lwin_prev = w.long(s.prev_kbd);
    let swin_prev = w.short(s.prev_kbd);
    if s.seq != WindowSequence::LongStop {
        for i in 0..1024 {
            input[i] *= lwin_prev[i];
        }
    } else {
        input[..448].fill(0.0);
        for i in 0..128 {
            input[448 + i] *= swin_prev[i];
        }
    }
    if s.seq != WindowSequence::LongStart {
        for i in 0..1024 {
            input[1024 + i] *= lwin[1023 - i];
        }
    } else {
        for i in 0..128 {
            input[1024 + 448 + i] *= swin[127 - i];
        }
        input[1024 + 576..].fill(0.0);
    }
    // Our coefficient domain: synthesis output = imdct(c)/32768, so the inverse is
    // 32768 · mdct(time).
    dsp::mdct_fast(input).iter().map(|v| v * 32768.0).collect()
}

/// The LTP history update after synthesis (`saved_ltp` derivation + shift).
pub fn ltp_update(w: &Windows, seq: WindowSequence, kbd: bool, buf: &[f32], saved: &[f32], output: &[f32], ltp_state: &mut [f32]) {
    let lwin = w.long(kbd);
    let swin = w.short(kbd);
    let mut saved_ltp = [0f32; 1024];
    match seq {
        WindowSequence::EightShort | WindowSequence::LongStart => {
            if seq == WindowSequence::EightShort {
                saved_ltp[..512].copy_from_slice(&saved[..512]);
            } else {
                saved_ltp[..448].copy_from_slice(&buf[512..960]);
            }
            for i in 0..64 {
                saved_ltp[448 + i] = buf[960 + i] * swin[127 - i];
            }
            for i in 0..64 {
                saved_ltp[i + 512] = buf[1023 - i] * swin[63 - i];
            }
        }
        _ => {
            for i in 0..512 {
                saved_ltp[i] = buf[512 + i] * lwin[1023 - i];
            }
            for i in 0..512 {
                saved_ltp[i + 512] = buf[1023 - i] * lwin[511 - i];
            }
        }
    }
    ltp_state.copy_within(1024..2048, 0);
    ltp_state[1024..2048].copy_from_slice(&output[..1024]);
    ltp_state[2048..3072].copy_from_slice(&saved_ltp);
}
