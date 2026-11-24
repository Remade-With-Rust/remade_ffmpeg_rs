//! Synthesis filterbank: half-length IMDCT + windowed overlap-add, for every
//! frame length the AAC family uses — 1024/960 (GA: long, start, stop, eight
//! short), 512/480 (ER AAC-LD, including its low-overlap window) and the
//! low-delay ELD filterbank — plus the LTP analysis/history hooks.
//!
//! The overlap logic follows the reference decoder's buffer discipline exactly:
//! each channel keeps `saved` (the not-yet-overlapped second half of the last
//! transform) and every frame is `out = window(saved, this_half)`. Window *shape*
//! transitions take the previous frame's shape on the rising half. Irregular
//! window-*sequence* transitions (the previous right slope and this left slope
//! of different lengths, e.g. `ONLY_LONG` → `EIGHT_SHORT`) are not folded into
//! one slope as FFmpeg does: each frame is windowed by its own sequence and the
//! halves are summed, as ISO/IEC 14496-3 defines it (`irregular_overlap`,
//! issue #208).

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
    pub fn new(frame_len: usize) -> Self {
        let short = frame_len / 8;
        Self {
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
    let (dst, src0, src1, win) = (
        &mut dst[..2 * len],
        &src0[..len],
        &src1[..len],
        &win[..2 * len],
    );
    // The mirrored halves defeat the auto-vectoriser (reversed writes into two
    // halves of one buffer), so the overlap window has explicit twins: SSE on
    // x86-64 (baseline, no detection) and NEON on aarch64, four samples per op
    // with a lane reversal for the mirrored streams. Same products and sums per
    // sample as the scalar path, so bit-identical (`fmul_window_matches_reference`).
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    {
        crate::prof::count(crate::prof::Kernel::FmulWindow, true, len);
        // SAFETY: SSE2 is baseline on x86-64; the slices are re-bounded above to
        // exactly the lengths `fmul_window_sse` reads and writes.
        unsafe { fmul_window_sse(dst, src0, src1, win, len) };
        return;
    }
    #[cfg(all(feature = "simd", target_arch = "aarch64"))]
    {
        crate::prof::count(crate::prof::Kernel::FmulWindow, true, len);
        // SAFETY: NEON is baseline on aarch64; bounds as above.
        unsafe { fmul_window_neon(dst, src0, src1, win, len) };
        return;
    }
    // On SIMD targets a twin above returns first; this is the scalar fallback.
    #[allow(unreachable_code)]
    {
        crate::prof::count(crate::prof::Kernel::FmulWindow, false, len);
        fmul_window_scalar(dst, src0, src1, win, len, 0);
    }
}

/// The scalar body for outputs `t >= from` (the SIMD twins' tail).
#[inline(always)]
fn fmul_window_scalar(
    dst: &mut [f32],
    src0: &[f32],
    src1: &[f32],
    win: &[f32],
    len: usize,
    from: usize,
) {
    for t in from..len {
        let jj = 2 * len - 1 - t;
        let (s0, s1) = (src0[t], src1[len - 1 - t]);
        let (wi, wj) = (win[t], win[jj]);
        dst[t] = s0 * wj - s1 * wi;
        dst[jj] = s0 * wi + s1 * wj;
    }
}

/// SSE twin of [`fmul_window`].
///
/// # Safety
/// `dst.len() == win.len() == 2·len`, `src0.len() == src1.len() == len`. A trip
/// at `t` (`t + 4 <= len`) touches `src0/win/dst[t..t+4]`,
/// `src1[len-4-t..len-t]` and `win/dst[2len-4-t..2len-t]`.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
unsafe fn fmul_window_sse(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
    use std::arch::x86_64::*;
    let (d, s0p, s1p, w) = (dst.as_mut_ptr(), src0.as_ptr(), src1.as_ptr(), win.as_ptr());
    let mut t = 0;
    while t + 4 <= len {
        let s0 = _mm_loadu_ps(s0p.add(t));
        let wi = _mm_loadu_ps(w.add(t));
        let s1 = _mm_shuffle_ps::<0x1B>(
            _mm_loadu_ps(s1p.add(len - 4 - t)),
            _mm_loadu_ps(s1p.add(len - 4 - t)),
        );
        let wj = _mm_shuffle_ps::<0x1B>(
            _mm_loadu_ps(w.add(2 * len - 4 - t)),
            _mm_loadu_ps(w.add(2 * len - 4 - t)),
        );
        _mm_storeu_ps(d.add(t), _mm_sub_ps(_mm_mul_ps(s0, wj), _mm_mul_ps(s1, wi)));
        let hi = _mm_add_ps(_mm_mul_ps(s0, wi), _mm_mul_ps(s1, wj));
        _mm_storeu_ps(d.add(2 * len - 4 - t), _mm_shuffle_ps::<0x1B>(hi, hi));
        t += 4;
    }
    fmul_window_scalar(dst, src0, src1, win, len, t);
}

/// NEON twin of [`fmul_window`].
///
/// # Safety
/// As for `fmul_window_sse`.
#[cfg(all(feature = "simd", target_arch = "aarch64"))]
#[target_feature(enable = "neon")]
unsafe fn fmul_window_neon(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
    use std::arch::aarch64::*;
    /// Reverse the four lanes.
    ///
    /// # Safety
    ///
    /// NEON must be available (baseline on aarch64); register-only, no memory.
    #[inline(always)]
    unsafe fn rev4(v: float32x4_t) -> float32x4_t {
        let r = vrev64q_f32(v);
        vextq_f32::<2>(r, r)
    }
    let (d, s0p, s1p, w) = (dst.as_mut_ptr(), src0.as_ptr(), src1.as_ptr(), win.as_ptr());
    let mut t = 0;
    while t + 4 <= len {
        let s0 = vld1q_f32(s0p.add(t));
        let wi = vld1q_f32(w.add(t));
        let s1 = rev4(vld1q_f32(s1p.add(len - 4 - t)));
        let wj = rev4(vld1q_f32(w.add(2 * len - 4 - t)));
        vst1q_f32(d.add(t), vsubq_f32(vmulq_f32(s0, wj), vmulq_f32(s1, wi)));
        let hi = vaddq_f32(vmulq_f32(s0, wi), vmulq_f32(s1, wj));
        vst1q_f32(d.add(2 * len - 4 - t), rev4(hi));
        t += 4;
    }
    fmul_window_scalar(dst, src0, src1, win, len, t);
}

/// The indexed form `fmul_window` replaced, kept as the bit-exact oracle.
#[cfg(test)]
fn fmul_window_reference(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
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

/// This frame's short-slope left half overlapped with `saved`: the copy of the
/// flat part of `saved`, the short overlap at `ov`, then either the eight-short
/// windows 1–4 (`EIGHT_SHORT`) or the flat body (`LONG_STOP`). `temp` receives
/// window 4's overlap, whose second half seeds the next `saved`.
///
/// Exact for every transition whose previous right slope is short too — then
/// `saved` already carries the previous frame's flat / zero regions. With a zero
/// `saved` it yields this frame's left half alone; with a zero `buf` and a
/// non-eight-short `seq`, the previous frame's short-slope right half alone.
#[allow(
    clippy::too_many_arguments,
    reason = "the two windows, the frame geometry and the four buffers are distinct borrows"
)]
#[inline(always)]
fn short_left(
    out: &mut [f32],
    saved: &[f32],
    buf: &[f32],
    temp: &mut [f32],
    swin_prev: &[f32],
    swin: &[f32],
    l: usize,
    seq: WindowSequence,
) {
    let sh = l / 8;
    let hs = sh / 2;
    let ov = l / 2 - hs;
    out[..ov].copy_from_slice(&saved[..ov]);
    fmul_window(&mut out[ov..], &saved[ov..], buf, swin_prev, hs);
    if seq == WindowSequence::EightShort {
        // The sources are read straight from `buf`: the destinations
        // (`out`, `temp`, `saved`) never alias it, so the per-window
        // `.to_vec()` copies were pure overhead.
        for k in 1..4 {
            let (a, b) = ((k - 1) * sh + hs, k * sh);
            fmul_window(
                &mut out[ov + k * sh..],
                &buf[a..a + hs],
                &buf[b..b + hs],
                swin,
                hs,
            );
        }
        fmul_window(
            temp,
            &buf[3 * sh + hs..4 * sh],
            &buf[4 * sh..4 * sh + hs],
            swin,
            hs,
        );
        out[ov + 4 * sh..ov + 4 * sh + hs].copy_from_slice(&temp[..hs]);
    } else {
        out[ov + sh..ov + sh + ov].copy_from_slice(&buf[hs..hs + ov]);
    }
}

/// An IRREGULAR window-sequence transition: the previous frame's right slope
/// and this frame's left slope differ in length (`ONLY_LONG`/`LONG_STOP` →
/// `EIGHT_SHORT`/`LONG_STOP`, or `LONG_START`/`EIGHT_SHORT` →
/// `ONLY_LONG`/`LONG_START`). The one-pass shortcut above assumes one shared
/// slope; here, as ISO/IEC 14496-3 4.6.11.3 (and the reference decoder, and
/// FAAD2) defines it, each frame is windowed by its OWN window sequence and the
/// two halves are overlap-added: `out = prev_right(saved) + cur_left(buf)`.
/// Both slopes take the previous frame's window shape (its right half is its
/// own shape; this frame's left half is `window_shape_previous`).
///
/// The overlap state written afterwards depends on this frame alone, so
/// `saved` keeps its meaning for the next frame and for LTP.
#[allow(
    clippy::too_many_arguments,
    reason = "the three windows, the frame geometry and the four buffers are distinct borrows"
)]
#[cold]
#[inline(never)]
fn irregular_overlap(
    out: &mut [f32],
    saved: &[f32],
    buf: &[f32],
    temp: &mut [f32],
    lwin_prev: &[f32],
    swin_prev: &[f32],
    swin: &[f32],
    l: usize,
    prev_long: bool,
    seq: WindowSequence,
) {
    let half = l / 2;
    let zeros = [0f32; 1024];
    let zeros = &zeros[..l];
    let mut prev = [0f32; 1024];
    let prev = &mut prev[..l];
    if prev_long {
        // Previous ONLY_LONG / LONG_STOP: `saved` is its unwindowed right half,
        // falling on the long slope across the whole frame.
        fmul_window(prev, saved, zeros, lwin_prev, half);
        // This LONG_STOP / EIGHT_SHORT frame's left half (zeros, short slope,
        // flat body or short windows), with no overlap state.
        short_left(out, zeros, buf, temp, swin_prev, swin, l, seq);
    } else {
        // Previous LONG_START / EIGHT_SHORT: `saved` holds its flat (or already
        // windowed) part and the unwindowed short falling slope; zeros follow.
        short_left(
            prev,
            saved,
            zeros,
            temp,
            swin_prev,
            swin,
            l,
            WindowSequence::LongStop,
        );
        // This ONLY_LONG / LONG_START frame's left half: the long rising slope.
        fmul_window(out, zeros, buf, lwin_prev, half);
    }
    for (o, p) in out[..l].iter_mut().zip(prev.iter()) {
        *o += *p;
    }
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
            dsp::imdct_half(
                &s.coeffs[i * 128..i * 128 + sh],
                &mut buf[i * sh..(i + 1) * sh],
                OUT_NORM,
            );
        }
    } else {
        dsp::imdct_half(&s.coeffs[..l], &mut buf[..l], OUT_NORM);
    }

    // Short-window scratch: a stack array (sh <= 128), not a zeroed heap `vec!`
    // per call - and only the eight-short path writes and reads it.
    let mut temp = [0f32; 128];
    let temp = &mut temp[..sh];
    // The overlap slope type on each side of the frame boundary: the previous
    // frame's RIGHT slope is long for ONLY_LONG / LONG_STOP, short for
    // LONG_START / EIGHT_SHORT; this frame's LEFT slope is long for
    // ONLY_LONG / LONG_START, short for LONG_STOP / EIGHT_SHORT.
    let prev_long = is_long_end(s.prev_seq);
    let cur_long = matches!(s.seq, WindowSequence::OnlyLong | WindowSequence::LongStart);
    if prev_long == cur_long {
        // Regular transition: both halves share one slope, so the falling
        // (previous) and rising (current) halves are windowed in one pass.
        if cur_long {
            fmul_window(out, saved, buf, lwin_prev, l / 2);
        } else {
            short_left(out, saved, buf, temp, swin_prev, swin, l, s.seq);
        }
    } else {
        irregular_overlap(
            out, saved, buf, temp, lwin_prev, swin_prev, swin, l, prev_long, s.seq,
        );
    }

    match s.seq {
        WindowSequence::EightShort => {
            saved[..hs].copy_from_slice(&temp[hs..sh]);
            for k in 0..3 {
                let (a, b) = ((4 + k) * sh + hs, (5 + k) * sh);
                fmul_window(
                    &mut saved[hs + k * sh..],
                    &buf[a..a + hs],
                    &buf[b..b + hs],
                    swin,
                    hs,
                );
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
#[allow(
    clippy::too_many_arguments,
    reason = "the two windows, the frame and the three state buffers are all distinct borrows of the decoder"
)]
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
pub fn imdct_and_window_eld(
    window: &[f32],
    n: usize,
    coeffs: &mut [f32],
    buf: &mut [f32],
    saved: &mut [f32],
    out: &mut [f32],
) {
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
    // Even outputs change sign (the float reference's (USE_FIXED + 1) factor is 1).
    for v in buf[..n].iter_mut().step_by(2) {
        *v = -*v;
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
pub fn ltp_update(
    w: &Windows,
    seq: WindowSequence,
    kbd: bool,
    buf: &[f32],
    saved: &[f32],
    output: &[f32],
    ltp_state: &mut [f32],
) {
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

#[cfg(test)]
mod fmul_window_twin {
    #[test]
    fn fmul_window_matches_reference() {
        for len in [1usize, 2, 3, 7, 32, 60, 64, 120, 128, 240, 480, 512] {
            let src0: Vec<f32> = (0..len).map(|i| (i as f32 * 0.7).sin() * 1000.0).collect();
            let src1: Vec<f32> = (0..len).map(|i| (i as f32 * 1.3).cos() * 900.0).collect();
            let win: Vec<f32> = (0..2 * len).map(|i| (i as f32 * 0.01).sin()).collect();
            let (mut got, mut want) = (vec![0f32; 2 * len], vec![0f32; 2 * len]);
            super::fmul_window(&mut got, &src0, &src1, &win, len);
            super::fmul_window_reference(&mut want, &src0, &src1, &win, len);
            assert!(
                got.iter()
                    .zip(&want)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "len={len}"
            );
        }
    }
}

/// Issue #208: [`imdct_and_window`] against a straightforward ISO/IEC 14496-3
/// 4.6.11 filterbank written from the spec text — a direct-formula f64 IMDCT,
/// the full `2N`-sample window of each `window_sequence` (left half in
/// `window_shape_previous`, right half in `window_shape`), and an explicit
/// overlap-add `out = z_cur[n] + z_prev[n + N/2]`. Every prev → cur sequence
/// pair (all 16, legal or not), both shapes on both sides, both frame lengths.
#[cfg(test)]
mod iso_filterbank {
    use super::{imdct_and_window, SynthIn, Windows};
    use crate::ics::WindowSequence::{self, EightShort, LongStart, LongStop, OnlyLong};
    use std::f64::consts::PI;

    const SEQS: [WindowSequence; 4] = [OnlyLong, LongStart, EightShort, LongStop];

    /// Spec IMDCT: `N/2` coefficients → `N` samples,
    /// `x[i] = (2/N)·Σ_k X[k]·cos((2π/N)(i + n0)(k + ½))`, `n0 = (N/2 + 1)/2`.
    fn imdct(spec: &[f32]) -> Vec<f64> {
        let n = 2 * spec.len();
        let n0 = n as f64 / 4.0 + 0.5; // (N/2 + 1)/2
        (0..n)
            .map(|i| {
                let acc: f64 = spec
                    .iter()
                    .enumerate()
                    .map(|(k, &x)| {
                        f64::from(x)
                            * (2.0 * PI / n as f64 * (i as f64 + n0) * (k as f64 + 0.5)).cos()
                    })
                    .sum();
                2.0 / n as f64 * acc
            })
            .collect()
    }

    fn bessel_i0(x: f64) -> f64 {
        let (mut sum, mut term) = (1.0, 1.0);
        for k in 1..60 {
            term *= x / 2.0 / f64::from(k);
            sum += term * term;
        }
        sum
    }

    /// The rising half (`n < N/2`) of the length-`N` window of one shape.
    fn rising(n_win: usize, kbd: bool) -> Vec<f64> {
        let half = n_win / 2;
        if !kbd {
            return (0..half)
                .map(|n| (PI / n_win as f64 * (n as f64 + 0.5)).sin())
                .collect();
        }
        let alpha = if n_win <= 256 { 6.0 } else { 4.0 };
        let q = n_win as f64 / 4.0;
        let k: Vec<f64> = (0..=half)
            .map(|p| bessel_i0(PI * alpha * (1.0 - ((p as f64 - q) / q).powi(2)).sqrt()))
            .collect();
        let total: f64 = k.iter().sum();
        let mut acc = 0.0;
        (0..half)
            .map(|n| {
                acc += k[n];
                (acc / total).sqrt()
            })
            .collect()
    }

    /// The windowed time signal `z[0..2L]` of one frame (frame length `l`).
    fn frame_time(
        coeffs: &[f32],
        l: usize,
        seq: WindowSequence,
        kbd: bool,
        prev_kbd: bool,
    ) -> Vec<f64> {
        let sh = l / 8;
        let ov = l / 2 - sh / 2; // 448 / 420
        let (ll, lr) = (rising(2 * l, prev_kbd), rising(2 * l, kbd));
        let (sl, sr) = (rising(2 * sh, prev_kbd), rising(2 * sh, kbd));
        let mut z = vec![0f64; 2 * l];
        if seq == EightShort {
            for w in 0..8 {
                let y = imdct(&coeffs[w * 128..w * 128 + sh]);
                let left = if w == 0 { &sl } else { &sr };
                for (n, &v) in y.iter().enumerate() {
                    let win = if n < sh { left[n] } else { sr[2 * sh - 1 - n] };
                    z[ov + w * sh + n] += v * win;
                }
            }
            return z;
        }
        let y = imdct(&coeffs[..l]);
        for (n, zn) in z.iter_mut().enumerate() {
            let win = if n < l {
                match seq {
                    LongStop if n < ov => 0.0,
                    LongStop if n < ov + sh => sl[n - ov],
                    LongStop => 1.0,
                    _ => ll[n],
                }
            } else {
                let m = n - l;
                match seq {
                    LongStart if m < ov => 1.0,
                    LongStart if m < ov + sh => sr[sh - 1 - (m - ov)],
                    LongStart => 0.0,
                    _ => lr[l - 1 - m],
                }
            };
            *zn = y[n] * win;
        }
        z
    }

    fn coeffs(seed: u32) -> Vec<f32> {
        // Deterministic, broadband, decaying with frequency; peaks O(0.5) after
        // the 1/32768 output scale.
        let mut s = seed;
        (0..1024)
            .map(|k| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let u = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                u * 4.0e6 / (1.0 + (k % 128) as f32 / 16.0)
            })
            .collect()
    }

    #[test]
    fn every_transition_matches_iso_overlap_add() {
        let (ca, cb) = (coeffs(1), coeffs(2));
        let mut worst_regular = 0f64;
        let mut worst_irregular = 0f64;
        for l in [1024usize, 960] {
            let w = Windows::new(l);
            for prev in SEQS {
                for cur in SEQS {
                    for prev_kbd in [false, true] {
                        for kbd in [false, true] {
                            let mut buf = vec![0f32; l];
                            let mut saved = vec![0f32; l / 2];
                            let mut out = vec![0f32; l];
                            // Frame A (`prev`): its own left half is irrelevant;
                            // only the overlap state it leaves matters.
                            let sa = SynthIn {
                                coeffs: &ca,
                                seq: prev,
                                prev_seq: prev,
                                kbd: prev_kbd,
                                prev_kbd,
                            };
                            imdct_and_window(&w, l, &sa, &mut buf, &mut saved, &mut out);
                            let sb = SynthIn {
                                coeffs: &cb,
                                seq: cur,
                                prev_seq: prev,
                                kbd,
                                prev_kbd,
                            };
                            imdct_and_window(&w, l, &sb, &mut buf, &mut saved, &mut out);

                            let za = frame_time(&ca, l, prev, prev_kbd, prev_kbd);
                            let zb = frame_time(&cb, l, cur, kbd, prev_kbd);
                            let err = (0..l)
                                .map(|n| {
                                    let want = (zb[n] + za[n + l]) / 32768.0;
                                    (f64::from(out[n]) - want).abs()
                                })
                                .fold(0f64, f64::max);
                            let regular = matches!(prev, OnlyLong | LongStop)
                                == matches!(cur, OnlyLong | LongStart);
                            if regular {
                                worst_regular = worst_regular.max(err);
                            } else {
                                worst_irregular = worst_irregular.max(err);
                            }
                            // 1e-5 of full scale is a third of one 16-bit LSB;
                            // the single-slope shortcut misses by ~1e-2 here.
                            assert!(
                                err < 1e-5,
                                "l={l} {prev:?}(kbd={prev_kbd}) -> {cur:?}(kbd={kbd}): max err {err:e}"
                            );
                        }
                    }
                }
            }
        }
        eprintln!("worst |err|: regular {worst_regular:e}, irregular {worst_irregular:e}");
    }
}
