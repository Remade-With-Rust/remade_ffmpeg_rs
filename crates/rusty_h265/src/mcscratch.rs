//! Motion-compensation scratch and the weighted-prediction write-out.
//!
//! The interpolation kernels live in `rusty_h265-accel`; what stays here is
//! the geometry around them — reusable scratch so no prediction unit
//! allocates, an edge-extended footprint for blocks whose filter support
//! leaves the picture (§8.5.3.3.2), and explicit weighted prediction
//! (§8.5.3.3.4.3), which is rare enough not to want a kernel.

use crate::frame::Plane;
use crate::slice::WeightEntry;

/// Largest luma prediction block, and the filter margin around it.
const MAX_PB: usize = 64;
const MAX_MARGIN: usize = 7;

/// Buffers reused by every prediction unit in a slice.
///
/// Before this existed, each prediction unit allocated three `Vec`s per plane
/// plus an interpolation temporary — in the function that is 61 % of decode.
pub(crate) struct McScratch {
    /// The 14-bit intermediate for each reference list.
    pub pred: [Vec<i16>; 2],
    /// The horizontal pass's output, consumed by the vertical pass.
    pub tmp: Vec<i16>,
    /// Edge-extended footprint, for blocks that reach outside the picture.
    pub pad: Vec<u16>,
}

impl McScratch {
    pub fn new() -> McScratch {
        McScratch {
            pred: [vec![0; MAX_PB * MAX_PB], vec![0; MAX_PB * MAX_PB]],
            tmp: vec![0; MAX_PB * (MAX_PB + MAX_MARGIN)],
            pad: vec![0; (MAX_PB + MAX_MARGIN) * (MAX_PB + MAX_MARGIN)],
        }
    }
}

/// The Cb and Cr filter footprints of one prediction unit, as
/// `accel::mc::interp_chroma2` takes them: slices of the reference planes at
/// `inner` when the footprint is inside the picture, and otherwise
/// edge-extended copies in the two halves of `pad`. The chroma planes share
/// their geometry, so one interior answer serves both.
pub(crate) fn chroma_pair_src<'a>(planes: &'a [Plane; 3], inner: Option<usize>, x0: i32, y0: i32, fw: usize, fh: usize, pad: &'a mut [u16]) -> [(&'a [u16], usize); 2] {
    match inner {
        Some(o) => [(&planes[1].data[o..], planes[1].stride), (&planes[2].data[o..], planes[2].stride)],
        None => {
            if rusty_h265_accel::census::ALWAYS {
                rusty_h265_accel::census::bump(&rusty_h265_accel::census::MC_EDGE_PAD, 2);
            }
            let (a, b) = pad.split_at_mut(fw * fh);
            pad_footprint(a, &planes[1], x0, y0, fw, fh);
            pad_footprint(b, &planes[2], x0, y0, fw, fh);
            [(&*a, fw), (&*b, fw)]
        }
    }
}

/// Copies the `fw × fh` footprint at (`x0`, `y0`) out of `plane` into `pad`
/// (see [`McScratch::pad`]). A free function over the buffer rather than a
/// method, so a caller holding the scratch's other fields by `&mut` can call
/// it.
pub(crate) fn pad_footprint(pad: &mut [u16], plane: &Plane, x0: i32, y0: i32, fw: usize, fh: usize) {
    crate::prof_scope!(crate::prof::Stage::Pad);
    // `pad` is built at the largest footprint HEVC can ask for -- a 64x64
    // prediction block plus the 7-tap margin, which is exactly
    // `(64 + 7) * (64 + 7)` -- so this can only ever be a no-op. It is a
    // `debug_assert` rather than a runtime resize so the release path does
    // not carry a compare and a call to `Vec::resize` it can never take.
    // `scratch_holds_the_largest_block` is the test that keeps it true.
    debug_assert!(pad.len() >= fw * fh, "pad footprint {fw}x{fh} exceeds the scratch");
    let pw = plane.width as i32;
    let ph = plane.height as i32;
    let left = (-x0).clamp(0, fw as i32) as usize;
    let right = ((x0 + fw as i32) - pw).clamp(0, fw as i32) as usize;
    let mid = fw - left - right;
    // Rows above the picture all clamp to row 0, and rows below all clamp
    // to the last one -- so a footprint hanging off an edge asked for the
    // SAME output row several times over, rebuilding it each time: a clamp,
    // a copy and up to two fills per repeat. Building it once and
    // replicating is strictly less work, and blocks at a picture edge are
    // 12% of motion compensation (`MC_EDGE_PAD` = 31,137 a clip).
    // The source column range is the same for every row, so it is resolved
    // once rather than per row.
    let sx_mid = (x0 + left as i32) as usize;
    let sx_all = x0.clamp(0, pw - 1) as usize;
    let last = plane.width - 1;
    let mut prev_sy = usize::MAX;
    for y in 0..fh {
        let sy = (y0 + y as i32).clamp(0, ph - 1) as usize;
        if sy == prev_sy {
            // The previous row is already exactly what this one would be.
            let (prev, cur) = pad.split_at_mut(y * fw);
            copy_run(&mut cur[..fw], &prev[(y - 1) * fw..y * fw]);
            continue;
        }
        prev_sy = sy;
        let rb = sy * plane.stride;
        let row = &plane.data[rb..rb + plane.width];
        let out = &mut pad[y * fw..y * fw + fw];
        if mid > 0 {
            copy_run(&mut out[left..left + mid], &row[sx_mid..sx_mid + mid]);
            if left > 0 {
                out[..left].fill(row[0]);
            }
            if right > 0 {
                out[left + mid..].fill(row[last]);
            }
        } else {
            // The whole footprint is off one side: one edge sample.
            out.fill(row[sx_all]);
        }
    }
}

/// Copies `src` into `dst` (equal lengths) in eight-sample chunks, the last
/// one overlapping the one before it.
///
/// `copy_from_slice` with a RUNTIME length is a `memcpy` CALL, and these runs
/// are footprint rows of 11 to 71 samples -- 5.9 M calls on a 20 s clip, one
/// per padded row. A constant-length copy is one or two vector moves, and
/// re-copying the overlap is exact because it writes the same bytes. Runs
/// shorter than a chunk (an almost entirely off-picture footprint) keep the
/// call.
#[inline]
fn copy_run(dst: &mut [u16], src: &[u16]) {
    let n = dst.len();
    debug_assert_eq!(n, src.len());
    if n < 8 {
        dst.copy_from_slice(src);
        return;
    }
    let mut i = 0;
    while i + 8 <= n {
        dst[i..i + 8].copy_from_slice(&src[i..i + 8]);
        i += 8;
    }
    if i < n {
        dst[n - 8..n].copy_from_slice(&src[n - 8..n]);
    }
}

/// §8.5.3.3.4.3 explicit weighted prediction, uni or bi.
#[allow(clippy::too_many_arguments)]
pub(crate) fn weighted_write(
    dst: &mut [u16],
    dst_stride: usize,
    pred: &[Vec<i16>; 2],
    used: [bool; 2],
    e0: Option<&WeightEntry>,
    e1: Option<&WeightEntry>,
    denom: u32,
    c: usize,
    w: usize,
    h: usize,
    bit_depth: u8,
) {
    let max = (1i32 << bit_depth) - 1;
    let shift1 = 14i32 - bit_depth as i32;
    let log2wd = denom as i32 + shift1;
    let wt = |e: Option<&WeightEntry>| -> (i32, i32) {
        match e {
            Some(e) if c == 0 => (e.luma_weight, e.luma_offset),
            Some(e) => (e.chroma_weight[c - 1], e.chroma_offset[c - 1]),
            None => (1 << denom, 0),
        }
    };
    match (used[0], used[1]) {
        (true, true) => {
            let (w0, o0) = wt(e0);
            let (w1, o1) = wt(e1);
            crate::accel::pixel::weighted_bi(dst, dst_stride, &pred[0], &pred[1], w, h, w0, w1, (o0 + o1 + 1) << log2wd, log2wd, max);
        }
        (used0, _) => {
            let l = if used0 { 0 } else { 1 };
            let (wv, ov) = wt(if used0 { e0 } else { e1 });
            crate::accel::pixel::weighted_uni(dst, dst_stride, &pred[l], w, h, wv, ov, log2wd, max);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane_with(w: usize, h: usize) -> Plane {
        let mut p = Plane::new(w, h);
        for y in 0..h {
            for x in 0..w {
                p.set(x, y, (y * w + x) as u16 % 255);
            }
        }
        p
    }

    /// The padded footprint must equal a per-sample clamped read — the
    /// definition it replaces.
    #[test]
    fn pad_footprint_matches_clamped_reads() {
        let p = plane_with(19, 11);
        let mut s = McScratch::new();
        for y0 in -9i32..14 {
            for x0 in -9i32..22 {
                for &(fw, fh) in &[(11usize, 11usize), (5, 7), (1, 1), (12, 3)] {
                    pad_footprint(&mut s.pad, &p, x0, y0, fw, fh);
                    for y in 0..fh {
                        for x in 0..fw {
                            let sx = (x0 + x as i32).clamp(0, p.width as i32 - 1) as usize;
                            let sy = (y0 + y as i32).clamp(0, p.height as i32 - 1) as usize;
                            assert_eq!(s.pad[y * fw + x], p.get(sx, sy), "({x0},{y0}) {fw}x{fh} at ({x},{y})");
                        }
                    }
                }
            }
        }
    }

    /// The scratch must be big enough for the largest block HEVC allows, or a
    /// 64x64 prediction unit would reallocate mid-picture.
    #[test]
    fn scratch_holds_the_largest_block() {
        let s = McScratch::new();
        assert!(s.pred[0].len() >= 64 * 64);
        assert!(s.tmp.len() >= 64 * (64 + 7));
        assert!(s.pad.len() >= (64 + 7) * (64 + 7));
    }
}
