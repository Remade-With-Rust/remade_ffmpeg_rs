//! VP9 intra prediction (ISO/VP9 §8.5.2 / libvpx `vpx_dsp/intrapred.c` +
//! `vp9_reconintra.c`). Ten prediction modes build a block from the
//! reconstructed `above` row and `left` column; `build_intra_predictors`
//! assembles those edges from the frame buffer with the exact availability
//! rules (127 above / 129 left when absent, above-right extension at frame
//! borders). VP9 applies no intra edge-smoothing filter.
//!
//! The predictors are a direct port of the C reference, cross-checked
//! bit-exact against an independent Python port (see the embedded vectors);
//! the edge assembly is proven end-to-end against FFmpeg in stage H.
//!
//! `above[0]` is the above-left corner (the C `above[-1]`), so the C index
//! `above[k]` maps to `above[k + 1]` here.

#![allow(dead_code)]

#[inline]
fn avg2(a: u16, b: u16) -> u16 {
    ((a as u32 + b as u32 + 1) >> 1) as u16
}
#[inline]
fn avg3(a: u16, b: u16, c: u16) -> u16 {
    ((a as u32 + 2 * b as u32 + c as u32 + 2) >> 2) as u16
}
#[inline]
fn clip(v: i32, max: i32) -> u16 {
    v.clamp(0, max) as u16
}

// Edge accessors: `a(k)` = C `above[k]` (k >= -1), `l(k)` = C `left[k]`.
#[inline]
fn a(above: &[u16], k: i32) -> u16 {
    above[(k + 1) as usize]
}

// Prediction modes.
pub const DC_PRED: u8 = 0;
pub const V_PRED: u8 = 1;
pub const H_PRED: u8 = 2;
pub const D45_PRED: u8 = 3;
pub const D135_PRED: u8 = 4;
pub const D117_PRED: u8 = 5;
pub const D153_PRED: u8 = 6;
pub const D207_PRED: u8 = 7;
pub const D63_PRED: u8 = 8;
pub const TM_PRED: u8 = 9;

/// `extend_modes`: which edges each mode requires (NEED_LEFT=2, ABOVE=4,
/// ABOVERIGHT=8), from libvpx.
pub const NEED_LEFT: u8 = 1 << 1;
pub const NEED_ABOVE: u8 = 1 << 2;
pub const NEED_ABOVERIGHT: u8 = 1 << 3;
pub const EXTEND_MODES: [u8; 10] = [
    NEED_ABOVE | NEED_LEFT, // DC
    NEED_ABOVE,             // V
    NEED_LEFT,              // H
    NEED_ABOVERIGHT,        // D45
    NEED_LEFT | NEED_ABOVE, // D135
    NEED_LEFT | NEED_ABOVE, // D117
    NEED_LEFT | NEED_ABOVE, // D153
    NEED_LEFT,              // D207
    NEED_ABOVERIGHT,        // D63
    NEED_LEFT | NEED_ABOVE, // TM
];

#[inline]
fn d(dst: &mut [u16], stride: usize, r: usize, c: usize) -> &mut u16 {
    &mut dst[r * stride + c]
}
#[inline]
fn g(dst: &[u16], stride: usize, r: usize, c: usize) -> u16 {
    dst[r * stride + c]
}

fn v_pred(dst: &mut [u16], stride: usize, bs: usize, above: &[u16]) {
    for r in 0..bs {
        for c in 0..bs {
            *d(dst, stride, r, c) = a(above, c as i32);
        }
    }
}

fn h_pred(dst: &mut [u16], stride: usize, bs: usize, left: &[u16]) {
    for r in 0..bs {
        for c in 0..bs {
            *d(dst, stride, r, c) = left[r];
        }
    }
}

fn tm_pred(dst: &mut [u16], stride: usize, bs: usize, above: &[u16], left: &[u16], max: i32) {
    let tl = a(above, -1) as i32;
    for r in 0..bs {
        for c in 0..bs {
            *d(dst, stride, r, c) = clip(left[r] as i32 + a(above, c as i32) as i32 - tl, max);
        }
    }
}

fn dc_pred(
    dst: &mut [u16],
    stride: usize,
    bs: usize,
    above: &[u16],
    left: &[u16],
    left_avail: bool,
    up_avail: bool,
    max: i32,
) {
    let dc: u16 = if left_avail && up_avail {
        let s: u32 = (0..bs)
            .map(|i| a(above, i as i32) as u32 + left[i] as u32)
            .sum();
        ((s + bs as u32) / (2 * bs as u32)) as u16
    } else if up_avail {
        let s: u32 = (0..bs).map(|i| a(above, i as i32) as u32).sum();
        ((s + (bs as u32 >> 1)) / bs as u32) as u16
    } else if left_avail {
        let s: u32 = (0..bs).map(|i| left[i] as u32).sum();
        ((s + (bs as u32 >> 1)) / bs as u32) as u16
    } else {
        ((max + 1) >> 1) as u16
    };
    for r in 0..bs {
        for c in 0..bs {
            *d(dst, stride, r, c) = dc;
        }
    }
}

fn d45_pred(dst: &mut [u16], stride: usize, bs: usize, above: &[u16]) {
    // libvpx ships a *distinct* 4×4 predictor (`vpx_d45_predictor_4x4_c`) that
    // uses the full above-right diagonal, whereas the 8×8/16×16/32×32
    // `d45_predictor` plateaus the lower-right triangle at `above[bs-1]`. The
    // encoder mirrors this split, so we must too.
    if bs == 4 {
        for r in 0..4 {
            for c in 0..4 {
                let v = if r + c + 2 < 8 {
                    avg3(
                        a(above, (r + c) as i32),
                        a(above, (r + c + 1) as i32),
                        a(above, (r + c + 2) as i32),
                    )
                } else {
                    a(above, 7)
                };
                *d(dst, stride, r, c) = v;
            }
        }
        return;
    }
    let ar = a(above, bs as i32 - 1);
    for x in 0..bs - 1 {
        *d(dst, stride, 0, x) = avg3(
            a(above, x as i32),
            a(above, x as i32 + 1),
            a(above, x as i32 + 2),
        );
    }
    *d(dst, stride, 0, bs - 1) = ar;
    for x in 1..bs {
        let size = bs - 1 - x;
        for k in 0..size {
            *d(dst, stride, x, k) = g(dst, stride, 0, x + k);
        }
        for k in size..bs {
            *d(dst, stride, x, k) = ar;
        }
    }
}

fn d63_pred(dst: &mut [u16], stride: usize, bs: usize, above: &[u16]) {
    // Like d45, the 4×4 predictor (`vpx_d63_predictor_4x4_c`) uses the full
    // above-right diagonal; the 8×8+ `d63_predictor` plateaus at `above[bs-1]`.
    if bs == 4 {
        for r in 0..4 {
            for c in 0..4 {
                let i = (r >> 1) + c;
                let v = if r & 1 == 0 {
                    avg2(a(above, i as i32), a(above, i as i32 + 1))
                } else {
                    avg3(
                        a(above, i as i32),
                        a(above, i as i32 + 1),
                        a(above, i as i32 + 2),
                    )
                };
                *d(dst, stride, r, c) = v;
            }
        }
        return;
    }
    let last = a(above, bs as i32 - 1);
    for c in 0..bs {
        *d(dst, stride, 0, c) = avg2(a(above, c as i32), a(above, c as i32 + 1));
        *d(dst, stride, 1, c) = avg3(
            a(above, c as i32),
            a(above, c as i32 + 1),
            a(above, c as i32 + 2),
        );
    }
    let (mut r, mut size) = (2usize, bs - 2);
    while r < bs {
        for k in 0..size {
            *d(dst, stride, r, k) = g(dst, stride, 0, (r >> 1) + k);
        }
        for k in size..bs {
            *d(dst, stride, r, k) = last;
        }
        for k in 0..size {
            *d(dst, stride, r + 1, k) = g(dst, stride, 1, (r >> 1) + k);
        }
        for k in size..bs {
            *d(dst, stride, r + 1, k) = last;
        }
        r += 2;
        size -= 1;
    }
}

fn d207_pred(dst: &mut [u16], stride: usize, bs: usize, left: &[u16]) {
    for r in 0..bs - 1 {
        *d(dst, stride, r, 0) = avg2(left[r], left[r + 1]);
    }
    *d(dst, stride, bs - 1, 0) = left[bs - 1];
    for r in 0..bs - 2 {
        *d(dst, stride, r, 1) = avg3(left[r], left[r + 1], left[r + 2]);
    }
    *d(dst, stride, bs - 2, 1) = avg3(left[bs - 2], left[bs - 1], left[bs - 1]);
    *d(dst, stride, bs - 1, 1) = left[bs - 1];
    for c in 0..bs - 2 {
        *d(dst, stride, bs - 1, 2 + c) = left[bs - 1];
    }
    for r in (0..bs - 1).rev() {
        for c in 0..bs - 2 {
            *d(dst, stride, r, 2 + c) = g(dst, stride, r + 1, c);
        }
    }
}

fn d117_pred(dst: &mut [u16], stride: usize, bs: usize, above: &[u16], left: &[u16]) {
    for c in 0..bs {
        *d(dst, stride, 0, c) = avg2(a(above, c as i32 - 1), a(above, c as i32));
    }
    *d(dst, stride, 1, 0) = avg3(left[0], a(above, -1), a(above, 0));
    for c in 1..bs {
        *d(dst, stride, 1, c) = avg3(
            a(above, c as i32 - 2),
            a(above, c as i32 - 1),
            a(above, c as i32),
        );
    }
    *d(dst, stride, 2, 0) = avg3(a(above, -1), left[0], left[1]);
    for r in 3..bs {
        *d(dst, stride, r, 0) = avg3(left[r - 3], left[r - 2], left[r - 1]);
    }
    for r in 2..bs {
        for c in 1..bs {
            *d(dst, stride, r, c) = g(dst, stride, r - 2, c - 1);
        }
    }
}

fn d135_pred(dst: &mut [u16], stride: usize, bs: usize, above: &[u16], left: &[u16]) {
    let mut border = [0u16; 2 * 32];
    for i in 0..bs - 2 {
        border[i] = avg3(left[bs - 3 - i], left[bs - 2 - i], left[bs - 1 - i]);
    }
    border[bs - 2] = avg3(a(above, -1), left[0], left[1]);
    border[bs - 1] = avg3(left[0], a(above, -1), a(above, 0));
    border[bs] = avg3(a(above, -1), a(above, 0), a(above, 1));
    for i in 0..bs - 2 {
        border[bs + 1 + i] = avg3(
            a(above, i as i32),
            a(above, i as i32 + 1),
            a(above, i as i32 + 2),
        );
    }
    for i in 0..bs {
        for c in 0..bs {
            *d(dst, stride, i, c) = border[bs - 1 - i + c];
        }
    }
}

fn d153_pred(dst: &mut [u16], stride: usize, bs: usize, above: &[u16], left: &[u16]) {
    *d(dst, stride, 0, 0) = avg2(a(above, -1), left[0]);
    for r in 1..bs {
        *d(dst, stride, r, 0) = avg2(left[r - 1], left[r]);
    }
    *d(dst, stride, 0, 1) = avg3(left[0], a(above, -1), a(above, 0));
    *d(dst, stride, 1, 1) = avg3(a(above, -1), left[0], left[1]);
    for r in 2..bs {
        *d(dst, stride, r, 1) = avg3(left[r - 2], left[r - 1], left[r]);
    }
    for c in 0..bs - 2 {
        *d(dst, stride, 0, 2 + c) = avg3(
            a(above, c as i32 - 1),
            a(above, c as i32),
            a(above, c as i32 + 1),
        );
    }
    for r in 1..bs {
        for c in 0..bs - 2 {
            *d(dst, stride, r, 2 + c) = g(dst, stride, r - 1, c);
        }
    }
}

/// Run intra prediction for `mode` into `dst`. `above` holds the above-left
/// corner at index 0 then the above row (length ≥ `1 + 2*bs`); `left` holds the
/// left column (length ≥ `bs`).
pub fn predict(
    dst: &mut [u16],
    stride: usize,
    mode: u8,
    bs: usize,
    above: &[u16],
    left: &[u16],
    left_avail: bool,
    up_avail: bool,
    max: i32,
) {
    match mode {
        DC_PRED => dc_pred(dst, stride, bs, above, left, left_avail, up_avail, max),
        V_PRED => v_pred(dst, stride, bs, above),
        H_PRED => h_pred(dst, stride, bs, left),
        D45_PRED => d45_pred(dst, stride, bs, above),
        D135_PRED => d135_pred(dst, stride, bs, above, left),
        D117_PRED => d117_pred(dst, stride, bs, above, left),
        D153_PRED => d153_pred(dst, stride, bs, above, left),
        D207_PRED => d207_pred(dst, stride, bs, left),
        D63_PRED => d63_pred(dst, stride, bs, above),
        TM_PRED => tm_pred(dst, stride, bs, above, left, max),
        _ => unreachable!(),
    }
}

/// Build the `above` (index 0 = above-left, then the above row of length
/// `2*bs`) and `left` (length `bs`) edge buffers from the reconstructed frame,
/// then run the predictor for `mode`. Exact port of libvpx
/// `build_intra_predictors`: 129 for an absent left, 127 for an absent above,
/// and the frame-border extension / above-right replication rules. `x0`,`y0`
/// are the block's pixel position in the plane; `mb_to_*_edge` are the coding
/// block's signed distances to the frame edges (negative ⇒ needs extension).
#[allow(clippy::too_many_arguments)]
pub fn build_intra_edges(
    mode: u8,
    bs: usize,
    up_avail: bool,
    left_avail: bool,
    right_avail: bool,
    frame: &[u16],
    stride: usize,
    frame_w: i32,
    frame_h: i32,
    x0: i32,
    y0: i32,
    mb_to_right_edge: i32,
    mb_to_bottom_edge: i32,
    above: &mut [u16],
    left: &mut [u16],
    max: i32,
) {
    let em = EXTEND_MODES[mode as usize];
    let base = y0 as usize * stride + x0 as usize; // index of `ref`
    let ar = base.wrapping_sub(stride); // `above_ref` = ref - stride
                                        // Absent-edge defaults scale with bit depth: 127/129 at 8-bit become
                                        // `(1<<(bd-1))∓1`. `pbase` is `1<<(bd-1)` = `(max+1)/2`.
    let pbase = ((max + 1) >> 1) as u16;
    let (def_above, def_left) = (pbase - 1, pbase + 1);

    if em & NEED_LEFT != 0 {
        if left_avail {
            if mb_to_bottom_edge < 0 && y0 + bs as i32 > frame_h {
                // Partial / fully-below-frame block: read the in-frame rows, then
                // replicate the bottom-most in-frame left pixel (row frame_h-1).
                // `ext` can be 0 when the block starts at/under the frame edge
                // (4×4 sub-block in the bottom-padding of a non-8-aligned height),
                // so clamp instead of computing `(ext-1)` in `usize`.
                let ext = (frame_h - y0).max(0) as usize;
                let n_in = ext.min(bs);
                for i in 0..n_in {
                    left[i] = frame[base + i * stride - 1];
                }
                if n_in < bs {
                    let rep = frame[(frame_h - 1).max(0) as usize * stride + x0 as usize - 1];
                    for i in n_in..bs {
                        left[i] = rep;
                    }
                }
            } else {
                for i in 0..bs {
                    left[i] = frame[base + i * stride - 1];
                }
            }
        } else {
            left[..bs].fill(def_left);
        }
    }

    if em & NEED_ABOVE != 0 {
        if up_avail {
            if mb_to_right_edge < 0 {
                if x0 + bs as i32 <= frame_w {
                    for i in 0..bs {
                        above[1 + i] = frame[ar + i];
                    }
                } else if x0 <= frame_w {
                    let r = (frame_w - x0) as usize;
                    for i in 0..r {
                        above[1 + i] = frame[ar + i];
                    }
                    for i in r..bs {
                        above[1 + i] = above[r]; // = above_row[r-1]
                    }
                }
            } else {
                for i in 0..bs {
                    above[1 + i] = frame[ar + i];
                }
            }
            above[0] = if left_avail { frame[ar - 1] } else { def_left };
        } else {
            above[1..1 + bs].fill(def_above);
            above[0] = def_above;
        }
    }

    if em & NEED_ABOVERIGHT != 0 {
        if up_avail {
            if mb_to_right_edge < 0 {
                if x0 + 2 * bs as i32 <= frame_w {
                    if right_avail && bs == 4 {
                        for i in 0..2 * bs {
                            above[1 + i] = frame[ar + i];
                        }
                    } else {
                        for i in 0..bs {
                            above[1 + i] = frame[ar + i];
                        }
                        for i in bs..2 * bs {
                            above[1 + i] = above[bs];
                        }
                    }
                } else if x0 + bs as i32 <= frame_w {
                    let r = (frame_w - x0) as usize;
                    if right_avail && bs == 4 {
                        for i in 0..r {
                            above[1 + i] = frame[ar + i];
                        }
                        for i in r..2 * bs {
                            above[1 + i] = above[r];
                        }
                    } else {
                        for i in 0..bs {
                            above[1 + i] = frame[ar + i];
                        }
                        for i in bs..2 * bs {
                            above[1 + i] = above[bs];
                        }
                    }
                } else if x0 <= frame_w {
                    let r = (frame_w - x0) as usize;
                    for i in 0..r {
                        above[1 + i] = frame[ar + i];
                    }
                    for i in r..2 * bs {
                        above[1 + i] = above[r];
                    }
                }
                above[0] = if left_avail { frame[ar - 1] } else { def_left };
            } else if bs == 4 && right_avail && left_avail {
                for i in 0..2 * bs {
                    above[1 + i] = frame[ar + i];
                }
                above[0] = frame[ar - 1];
            } else {
                for i in 0..bs {
                    above[1 + i] = frame[ar + i];
                }
                if bs == 4 && right_avail {
                    for i in bs..2 * bs {
                        above[1 + i] = frame[ar + i];
                    }
                } else {
                    for i in bs..2 * bs {
                        above[1 + i] = above[bs];
                    }
                }
                above[0] = if left_avail { frame[ar - 1] } else { def_left };
            }
        } else {
            above[1..1 + 2 * bs].fill(def_above);
            above[0] = def_above;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // above-left = 100; above row (incl. above-right) = 110..180; left = 90,80,70,60.
    fn edges() -> (Vec<u16>, Vec<u16>) {
        let above = vec![100u16, 110, 120, 130, 140, 150, 160, 170, 180];
        let left = vec![90u16, 80, 70, 60];
        (above, left)
    }

    fn run(mode: u8) -> Vec<u16> {
        let (above, left) = edges();
        let mut dst = vec![0u16; 16];
        predict(&mut dst, 4, mode, 4, &above, &left, true, true, 255);
        dst
    }

    // Expected outputs from the independent Python port of intrapred.c.
    #[test]
    fn predictors_match_reference_vectors() {
        assert_eq!(
            run(DC_PRED),
            [100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100]
        );
        assert_eq!(
            run(V_PRED),
            [110, 120, 130, 140, 110, 120, 130, 140, 110, 120, 130, 140, 110, 120, 130, 140]
        );
        assert_eq!(
            run(H_PRED),
            [90, 90, 90, 90, 80, 80, 80, 80, 70, 70, 70, 70, 60, 60, 60, 60]
        );
        assert_eq!(
            run(D45_PRED),
            [120, 130, 140, 150, 130, 140, 150, 160, 140, 150, 160, 170, 150, 160, 170, 180]
        );
        assert_eq!(
            run(D135_PRED),
            [100, 110, 120, 130, 90, 100, 110, 120, 80, 90, 100, 110, 70, 80, 90, 100]
        );
        assert_eq!(
            run(D117_PRED),
            [105, 115, 125, 135, 100, 110, 120, 130, 90, 105, 115, 125, 80, 100, 110, 120]
        );
        assert_eq!(
            run(D153_PRED),
            [95, 100, 110, 120, 85, 90, 95, 100, 75, 80, 85, 90, 65, 70, 75, 80]
        );
        assert_eq!(
            run(D207_PRED),
            [85, 80, 75, 70, 75, 70, 65, 63, 65, 63, 60, 60, 60, 60, 60, 60]
        );
        assert_eq!(
            run(D63_PRED),
            [115, 125, 135, 145, 120, 130, 140, 150, 125, 135, 145, 155, 130, 140, 150, 160]
        );
        assert_eq!(
            run(TM_PRED),
            [100, 110, 120, 130, 90, 100, 110, 120, 80, 90, 100, 110, 70, 80, 90, 100]
        );
    }

    #[test]
    fn edges_interior_copies_frame() {
        // 16×16 frame with frame[i] = i; block at (4,4), interior, all available.
        let frame: Vec<u16> = (0..256u32).map(|i| i as u16).collect();
        let mut above = [0u16; 1 + 8];
        let mut left = [0u16; 4];
        build_intra_edges(
            V_PRED, 4, true, true, true, &frame, 16, 16, 16, 4, 4, 64, 64, &mut above, &mut left,
            255,
        );
        // above row = the 4 pixels at row 3, cols 4..8; above-left = (3,3).
        assert_eq!(
            &above[1..5],
            &[3 * 16 + 4, 3 * 16 + 5, 3 * 16 + 6, 3 * 16 + 7]
        );
        assert_eq!(above[0], 3 * 16 + 3);
        // H mode reads the left column (4,3),(5,3),(6,3),(7,3).
        build_intra_edges(
            H_PRED, 4, true, true, true, &frame, 16, 16, 16, 4, 4, 64, 64, &mut above, &mut left,
            255,
        );
        assert_eq!(left, [4 * 16 + 3, 5 * 16 + 3, 6 * 16 + 3, 7 * 16 + 3]);
    }

    #[test]
    fn edges_unavailable_defaults() {
        let frame = vec![50u16; 256];
        let mut above = [0u16; 1 + 8];
        let mut left = [0u16; 4];
        // No above → above row 127, above-left 127.
        build_intra_edges(
            V_PRED, 4, false, true, false, &frame, 16, 16, 16, 4, 4, 64, 64, &mut above, &mut left,
            255,
        );
        assert!(above[..5].iter().all(|&v| v == 127));
        // No left → left column 129.
        build_intra_edges(
            H_PRED, 4, true, false, false, &frame, 16, 16, 16, 4, 4, 64, 64, &mut above, &mut left,
            255,
        );
        assert!(left.iter().all(|&v| v == 129));
    }

    #[test]
    fn dc_variants() {
        let (above, left) = edges();
        let mut dst = vec![0u16; 16];
        // top-only: average of above (110+120+130+140+2)/4 = 125.
        predict(&mut dst, 4, DC_PRED, 4, &above, &left, false, true, 255);
        assert!(dst.iter().all(|&v| v == 125));
        // left-only: (90+80+70+60+2)/4 = 75.
        predict(&mut dst, 4, DC_PRED, 4, &above, &left, true, false, 255);
        assert!(dst.iter().all(|&v| v == 75));
        // neither: 128.
        predict(&mut dst, 4, DC_PRED, 4, &above, &left, false, false, 255);
        assert!(dst.iter().all(|&v| v == 128));
    }
}
