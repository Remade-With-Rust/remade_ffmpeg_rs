//! Does loop filtering on webp lossy images

#[inline]
fn c(val: i32) -> i32 {
    val.clamp(-128, 127)
}

//unsigned to signed
#[inline]
fn u2s(val: u8) -> i32 {
    i32::from(val) - 128
}

//signed to unsigned
#[inline]
fn s2u(val: i32) -> u8 {
    (c(val) + 128) as u8
}

#[inline]
const fn diff(val1: u8, val2: u8) -> u8 {
    u8::abs_diff(val1, val2)
}

/// Used in both the simple and normal filters described in 15.2 and 15.3
///
/// Adjusts the 2 middle pixels in a vertical loop filter
fn common_adjust_vertical(
    use_outer_taps: bool,
    pixels: &mut [u8],
    point: usize,
    stride: usize,
) -> i32 {
    let p1 = u2s(pixels[point - 2 * stride]);
    let p0 = u2s(pixels[point - stride]);
    let q0 = u2s(pixels[point]);
    let q1 = u2s(pixels[point + stride]);

    //value for the outer 2 pixels
    let outer = if use_outer_taps { c(p1 - q1) } else { 0 };

    let a = c(outer + 3 * (q0 - p0));

    let b = (c(a + 3)) >> 3;

    let a = (c(a + 4)) >> 3;

    pixels[point] = s2u(q0 - a);
    pixels[point - stride] = s2u(p0 + b);

    a
}

/// Used in both the simple and normal filters described in 15.2 and 15.3
///
/// Adjusts the 2 middle pixels in a horizontal loop filter
fn common_adjust_horizontal(use_outer_taps: bool, pixels: &mut [u8]) -> i32 {
    let p1 = u2s(pixels[2]);
    let p0 = u2s(pixels[3]);
    let q0 = u2s(pixels[4]);
    let q1 = u2s(pixels[5]);

    //value for the outer 2 pixels
    let outer = if use_outer_taps { c(p1 - q1) } else { 0 };

    let a = c(outer + 3 * (q0 - p0));

    let b = (c(a + 3)) >> 3;

    let a = (c(a + 4)) >> 3;

    pixels[4] = s2u(q0 - a);
    pixels[3] = s2u(p0 + b);

    a
}

#[inline]
fn simple_threshold_vertical(
    filter_limit: i32,
    pixels: &[u8],
    point: usize,
    stride: usize,
) -> bool {
    i32::from(diff(pixels[point - stride], pixels[point])) * 2
        + i32::from(diff(pixels[point - 2 * stride], pixels[point + stride])) / 2
        <= filter_limit
}

#[inline]
fn simple_threshold_horizontal(filter_limit: i32, pixels: &[u8]) -> bool {
    assert!(pixels.len() >= 6); // one bounds check up front eliminates all subsequent checks in this function
    i32::from(diff(pixels[3], pixels[4])) * 2 + i32::from(diff(pixels[2], pixels[5])) / 2
        <= filter_limit
}

fn should_filter_vertical(
    interior_limit: u8,
    edge_limit: u8,
    pixels: &[u8],
    point: usize,
    stride: usize,
) -> bool {
    simple_threshold_vertical(i32::from(edge_limit), pixels, point, stride)
        // this looks like an erroneous way to compute differences between 8 points, but isn't:
        // there are actually only 6 diff comparisons required as per the spec:
        // https://www.rfc-editor.org/rfc/rfc6386#section-20.6
        && diff(pixels[point - 4 * stride], pixels[point - 3 * stride]) <= interior_limit
        && diff(pixels[point - 3 * stride], pixels[point - 2 * stride]) <= interior_limit
        && diff(pixels[point - 2 * stride], pixels[point - stride]) <= interior_limit
        && diff(pixels[point + 3 * stride], pixels[point + 2 * stride]) <= interior_limit
        && diff(pixels[point + 2 * stride], pixels[point + stride]) <= interior_limit
        && diff(pixels[point + stride], pixels[point]) <= interior_limit
}

fn should_filter_horizontal(interior_limit: u8, edge_limit: u8, pixels: &[u8]) -> bool {
    assert!(pixels.len() >= 8); // one bounds check up front eliminates all subsequent checks in this function
    simple_threshold_horizontal(i32::from(edge_limit), pixels)
        // this looks like an erroneous way to compute differences between 8 points, but isn't:
        // there are actually only 6 diff comparisons required as per the spec:
        // https://www.rfc-editor.org/rfc/rfc6386#section-20.6
        && diff(pixels[0], pixels[1]) <= interior_limit
        && diff(pixels[1], pixels[2]) <= interior_limit
        && diff(pixels[2], pixels[3]) <= interior_limit
        && diff(pixels[7], pixels[6]) <= interior_limit
        && diff(pixels[6], pixels[5]) <= interior_limit
        && diff(pixels[5], pixels[4]) <= interior_limit
}

#[inline]
fn high_edge_variance_vertical(threshold: u8, pixels: &[u8], point: usize, stride: usize) -> bool {
    diff(pixels[point - 2 * stride], pixels[point - stride]) > threshold
        || diff(pixels[point + stride], pixels[point]) > threshold
}

#[inline]
fn high_edge_variance_horizontal(threshold: u8, pixels: &[u8]) -> bool {
    diff(pixels[2], pixels[3]) > threshold || diff(pixels[5], pixels[4]) > threshold
}

/// Part of the simple filter described in 15.2 in the specification
///
/// Affects 4 pixels on an edge(2 each side)
pub(crate) fn simple_segment_vertical(
    edge_limit: u8,
    pixels: &mut [u8],
    point: usize,
    stride: usize,
) {
    if simple_threshold_vertical(i32::from(edge_limit), pixels, point, stride) {
        common_adjust_vertical(true, pixels, point, stride);
    }
}

/// Part of the simple filter described in 15.2 in the specification
///
/// Affects 4 pixels on an edge(2 each side)
pub(crate) fn simple_segment_horizontal(edge_limit: u8, pixels: &mut [u8]) {
    if simple_threshold_horizontal(i32::from(edge_limit), pixels) {
        common_adjust_horizontal(true, pixels);
    }
}

/// Part of the normal filter described in 15.3 in the specification
///
/// Filters on the 8 pixels on the edges between subblocks inside a macroblock
pub(crate) fn subblock_filter_vertical(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
    point: usize,
    stride: usize,
) {
    if should_filter_vertical(interior_limit, edge_limit, pixels, point, stride) {
        let hv = high_edge_variance_vertical(hev_threshold, pixels, point, stride);

        let a = (common_adjust_vertical(hv, pixels, point, stride) + 1) >> 1;

        if !hv {
            pixels[point + stride] = s2u(u2s(pixels[point + stride]) - a);
            pixels[point - 2 * stride] = s2u(u2s(pixels[point - 2 * stride]) + a);
        }
    }
}

/// Part of the normal filter described in 15.3 in the specification
///
/// Filters on the 8 pixels on the edges between subblocks inside a macroblock
pub(crate) fn subblock_filter_horizontal(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
) {
    if should_filter_horizontal(interior_limit, edge_limit, pixels) {
        let hv = high_edge_variance_horizontal(hev_threshold, pixels);

        let a = (common_adjust_horizontal(hv, pixels) + 1) >> 1;

        if !hv {
            pixels[5] = s2u(u2s(pixels[5]) - a);
            pixels[2] = s2u(u2s(pixels[2]) + a);
        }
    }
}

/// Part of the normal filter described in 15.3 in the specification
///
/// Filters on the 8 pixels on the vertical edges between macroblocks\
/// The point passed in must be the first vertical pixel on the bottom macroblock
pub(crate) fn macroblock_filter_vertical(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
    point: usize,
    stride: usize,
) {
    if should_filter_vertical(interior_limit, edge_limit, pixels, point, stride) {
        if !high_edge_variance_vertical(hev_threshold, pixels, point, stride) {
            // p0-3 are the pixels on the left macroblock from right to left
            let p2 = u2s(pixels[point - 3 * stride]);
            let p1 = u2s(pixels[point - 2 * stride]);
            let p0 = u2s(pixels[point - stride]);
            // q0-3 are the pixels on the right macroblock from left to right
            let q0 = u2s(pixels[point]);
            let q1 = u2s(pixels[point + stride]);
            let q2 = u2s(pixels[point + 2 * stride]);

            let w = c(c(p1 - q1) + 3 * (q0 - p0));

            let a = c((27 * w + 63) >> 7);

            pixels[point] = s2u(q0 - a);
            pixels[point - stride] = s2u(p0 + a);

            let a = c((18 * w + 63) >> 7);

            pixels[point + stride] = s2u(q1 - a);
            pixels[point - 2 * stride] = s2u(p1 + a);

            let a = c((9 * w + 63) >> 7);

            pixels[point + 2 * stride] = s2u(q2 - a);
            pixels[point - 3 * stride] = s2u(p2 + a);
        } else {
            common_adjust_vertical(true, pixels, point, stride);
        }
    }
}

/// Part of the normal filter described in 15.3 in the specification
///
/// Filters on the 8 pixels on the horizontal edges between macroblocks\
/// The pixels passed in must be a slice containing the 4 pixels on each macroblock
pub(crate) fn macroblock_filter_horizontal(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
) {
    assert!(pixels.len() >= 8);
    if should_filter_horizontal(interior_limit, edge_limit, pixels) {
        if !high_edge_variance_horizontal(hev_threshold, pixels) {
            // p0-3 are the pixels on the left macroblock from right to left
            let p2 = u2s(pixels[1]);
            let p1 = u2s(pixels[2]);
            let p0 = u2s(pixels[3]);
            // q0-3 are the pixels on the right macroblock from left to right
            let q0 = u2s(pixels[4]);
            let q1 = u2s(pixels[5]);
            let q2 = u2s(pixels[6]);

            let w = c(c(p1 - q1) + 3 * (q0 - p0));

            let a = c((27 * w + 63) >> 7);

            pixels[4] = s2u(q0 - a);
            pixels[3] = s2u(p0 + a);

            let a = c((18 * w + 63) >> 7);

            pixels[5] = s2u(q1 - a);
            pixels[2] = s2u(p1 + a);

            let a = c((9 * w + 63) >> 7);

            pixels[6] = s2u(q2 - a);
            pixels[1] = s2u(p2 + a);
        } else {
            common_adjust_horizontal(true, pixels);
        }
    }
}

// ---------------------------------------------------------------------------
// Batched edge filters
//
// The per-pixel functions above filter one crossing at a time behind
// data-dependent branches, which blocks vectorization. These process a whole
// edge (N = 16 luma / 8 chroma lanes) as straight-line select arithmetic on
// i16 lanes — bit-exact with the scalar versions (same integer formulas per
// lane), which the tests below assert on random data.
// ---------------------------------------------------------------------------

#[inline(always)]
fn clamp8(v: i16) -> i16 {
    v.clamp(-128, 127)
}

/// Normal-filter macroblock-edge core on centred (value − 128) lanes.
/// Taps ordered [p3, p2, p1, p0, q0, q1, q2, q3]; writes p2..q2.
fn mb_edge_core<const N: usize>(hev_threshold: u8, interior_limit: u8, edge_limit: u8, t: &mut [[i16; N]; 8]) {
    let il = i16::from(interior_limit);
    let el = i16::from(edge_limit);
    let hev_t = i16::from(hev_threshold);
    let [p3, p2, p1, p0, q0, q1, q2, q3] = t;
    for i in 0..N {
        let (v3, v2, v1, v0) = (p3[i], p2[i], p1[i], p0[i]);
        let (w0, w1, w2, w3) = (q0[i], q1[i], q2[i], q3[i]);

        // Bitwise (not short-circuit) combining: every operand is a pure
        // comparison, and `&`/`|` keep the lane body branch-free so the whole
        // loop vectorizes as selects.
        let fmask = (2 * (v0 - w0).abs() + (v1 - w1).abs() / 2 <= el)
            & ((v3 - v2).abs() <= il)
            & ((v2 - v1).abs() <= il)
            & ((v1 - v0).abs() <= il)
            & ((w3 - w2).abs() <= il)
            & ((w2 - w1).abs() <= il)
            & ((w1 - w0).abs() <= il);
        let hev = ((v1 - v0).abs() > hev_t) | ((w1 - w0).abs() > hev_t);

        // hev lanes: common adjust with outer taps
        let a = clamp8(clamp8(v1 - w1) + 3 * (w0 - v0));
        let b_ca = clamp8(a + 3) >> 3;
        let a_ca = clamp8(a + 4) >> 3;
        let q0_ca = clamp8(w0 - a_ca);
        let p0_ca = clamp8(v0 + b_ca);

        // non-hev lanes: full six-tap filter (w equals `a` above)
        let a27 = clamp8((27 * a + 63) >> 7);
        let a18 = clamp8((18 * a + 63) >> 7);
        let a9 = clamp8((9 * a + 63) >> 7);
        let q0_bf = clamp8(w0 - a27);
        let p0_bf = clamp8(v0 + a27);
        let q1_bf = clamp8(w1 - a18);
        let p1_bf = clamp8(v1 + a18);
        let q2_bf = clamp8(w2 - a9);
        let p2_bf = clamp8(v2 + a9);

        let np0 = if hev { p0_ca } else { p0_bf };
        let nq0 = if hev { q0_ca } else { q0_bf };
        let np1 = if hev { v1 } else { p1_bf };
        let nq1 = if hev { w1 } else { q1_bf };
        let np2 = if hev { v2 } else { p2_bf };
        let nq2 = if hev { w2 } else { q2_bf };

        p0[i] = if fmask { np0 } else { v0 };
        q0[i] = if fmask { nq0 } else { w0 };
        p1[i] = if fmask { np1 } else { v1 };
        q1[i] = if fmask { nq1 } else { w1 };
        p2[i] = if fmask { np2 } else { v2 };
        q2[i] = if fmask { nq2 } else { w2 };
    }
}

/// Normal-filter subblock-edge core on centred lanes; writes p1..q1.
fn sb_edge_core<const N: usize>(hev_threshold: u8, interior_limit: u8, edge_limit: u8, t: &mut [[i16; N]; 8]) {
    let il = i16::from(interior_limit);
    let el = i16::from(edge_limit);
    let hev_t = i16::from(hev_threshold);
    let [p3, p2, p1, p0, q0, q1, q2, q3] = t;
    for i in 0..N {
        let (v3, v2, v1, v0) = (p3[i], p2[i], p1[i], p0[i]);
        let (w0, w1, w2, w3) = (q0[i], q1[i], q2[i], q3[i]);

        // Bitwise (not short-circuit) combining: every operand is a pure
        // comparison, and `&`/`|` keep the lane body branch-free so the whole
        // loop vectorizes as selects.
        let fmask = (2 * (v0 - w0).abs() + (v1 - w1).abs() / 2 <= el)
            & ((v3 - v2).abs() <= il)
            & ((v2 - v1).abs() <= il)
            & ((v1 - v0).abs() <= il)
            & ((w3 - w2).abs() <= il)
            & ((w2 - w1).abs() <= il)
            & ((w1 - w0).abs() <= il);
        let hev = ((v1 - v0).abs() > hev_t) | ((w1 - w0).abs() > hev_t);

        let outer = if hev { clamp8(v1 - w1) } else { 0 };
        let a = clamp8(outer + 3 * (w0 - v0));
        let b_ca = clamp8(a + 3) >> 3;
        let a_ca = clamp8(a + 4) >> 3;
        let nq0 = clamp8(w0 - a_ca);
        let np0 = clamp8(v0 + b_ca);

        let a3 = (a_ca + 1) >> 1;
        let nq1 = if hev { w1 } else { clamp8(w1 - a3) };
        let np1 = if hev { v1 } else { clamp8(v1 + a3) };

        p0[i] = if fmask { np0 } else { v0 };
        q0[i] = if fmask { nq0 } else { w0 };
        p1[i] = if fmask { np1 } else { v1 };
        q1[i] = if fmask { nq1 } else { w1 };
    }
}

/// Load 8 tap rows of N contiguous columns around a horizontal edge
/// (`point` = first q0 pixel), centred to i16.
#[inline(always)]
fn load_rows<const N: usize>(pixels: &[u8], point: usize, stride: usize) -> [[i16; N]; 8] {
    let mut t = [[0i16; N]; 8];
    for (k, row) in t.iter_mut().enumerate() {
        let base = point + k * stride - 4 * stride;
        for (i, v) in row.iter_mut().enumerate() {
            *v = i16::from(pixels[base + i]) - 128;
        }
    }
    t
}

#[inline(always)]
fn store_rows<const N: usize>(pixels: &mut [u8], point: usize, stride: usize, t: &[[i16; N]; 8], from: usize, to: usize) {
    for k in from..to {
        let base = point + k * stride - 4 * stride;
        for i in 0..N {
            pixels[base + i] = (t[k][i] + 128) as u8;
        }
    }
}

/// Load 8 contiguous taps for N rows around a vertical edge (`base` = the
/// p3 pixel of the first row), centred to i16 — a small transpose.
#[inline(always)]
fn load_cols<const N: usize>(pixels: &[u8], base: usize, stride: usize) -> [[i16; N]; 8] {
    let mut t = [[0i16; N]; 8];
    for i in 0..N {
        let row = base + i * stride;
        for (k, tap) in t.iter_mut().enumerate() {
            tap[i] = i16::from(pixels[row + k]) - 128;
        }
    }
    t
}

#[inline(always)]
fn store_cols<const N: usize>(pixels: &mut [u8], base: usize, stride: usize, t: &[[i16; N]; 8], from: usize, to: usize) {
    for i in 0..N {
        let row = base + i * stride;
        for k in from..to {
            pixels[row + k] = (t[k][i] + 128) as u8;
        }
    }
}

/// Batched [`macroblock_filter_vertical`]: N consecutive columns across a
/// horizontal macroblock edge, `point` = first pixel of the lower block.
pub(crate) fn macroblock_filter_vertical_batch<const N: usize>(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
    point: usize,
    stride: usize,
) {
    let mut t = load_rows::<N>(pixels, point, stride);
    mb_edge_core(hev_threshold, interior_limit, edge_limit, &mut t);
    store_rows(pixels, point, stride, &t, 1, 7);
}

/// Batched [`subblock_filter_vertical`]: N consecutive columns across a
/// horizontal subblock edge.
pub(crate) fn subblock_filter_vertical_batch<const N: usize>(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
    point: usize,
    stride: usize,
) {
    let mut t = load_rows::<N>(pixels, point, stride);
    sb_edge_core(hev_threshold, interior_limit, edge_limit, &mut t);
    store_rows(pixels, point, stride, &t, 2, 6);
}

/// Batched [`macroblock_filter_horizontal`]: N consecutive rows across a
/// vertical macroblock edge, `base` = the p3 pixel (x0 − 4) of the first row.
pub(crate) fn macroblock_filter_horizontal_batch<const N: usize>(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
    base: usize,
    stride: usize,
) {
    let mut t = load_cols::<N>(pixels, base, stride);
    mb_edge_core(hev_threshold, interior_limit, edge_limit, &mut t);
    store_cols(pixels, base, stride, &t, 1, 7);
}

/// Batched [`subblock_filter_horizontal`]: N consecutive rows across a
/// vertical subblock edge.
pub(crate) fn subblock_filter_horizontal_batch<const N: usize>(
    hev_threshold: u8,
    interior_limit: u8,
    edge_limit: u8,
    pixels: &mut [u8],
    base: usize,
    stride: usize,
) {
    let mut t = load_cols::<N>(pixels, base, stride);
    sb_edge_core(hev_threshold, interior_limit, edge_limit, &mut t);
    store_cols(pixels, base, stride, &t, 2, 6);
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    fn lcg(state: &mut u64) -> u8 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*state >> 33) as u8
    }

    /// Batched filters must be bit-exact with the per-pixel oracles over
    /// random content and every (hev, interior, edge) parameter mix.
    #[test]
    fn batch_matches_scalar() {
        let stride = 40usize;
        let mut seed = 0x00c0ffee_u64;
        for (hev_t, il, el) in [(0u8, 1u8, 2u8), (5, 15, 15), (2, 63, 40), (10, 30, 80)] {
            let mut base = vec![0u8; stride * 40];
            for v in base.iter_mut() {
                *v = lcg(&mut seed);
            }

            // vertical (across a horizontal edge), 16 columns at point (8, 8)
            let point = 8 * stride + 8;
            let mut a = base.clone();
            for x in 0..16 {
                macroblock_filter_vertical(hev_t, il, el, &mut a, point + x, stride);
            }
            let mut b = base.clone();
            macroblock_filter_vertical_batch::<16>(hev_t, il, el, &mut b, point, stride);
            assert_eq!(a, b, "mb vertical mismatch");

            let mut a = base.clone();
            for x in 0..16 {
                subblock_filter_vertical(hev_t, il, el, &mut a, point + x, stride);
            }
            let mut b = base.clone();
            subblock_filter_vertical_batch::<16>(hev_t, il, el, &mut b, point, stride);
            assert_eq!(a, b, "sb vertical mismatch");

            // horizontal (across a vertical edge), 16 rows at x0 = 8
            let mut a = base.clone();
            for y in 0..16 {
                let o = (8 + y) * stride + 8 - 4;
                macroblock_filter_horizontal(hev_t, il, el, &mut a[o..][..8]);
            }
            let mut b = base.clone();
            macroblock_filter_horizontal_batch::<16>(hev_t, il, el, &mut b, 8 * stride + 8 - 4, stride);
            assert_eq!(a, b, "mb horizontal mismatch");

            let mut a = base.clone();
            for y in 0..16 {
                let o = (8 + y) * stride + 8 - 4;
                subblock_filter_horizontal(hev_t, il, el, &mut a[o..][..8]);
            }
            let mut b = base.clone();
            subblock_filter_horizontal_batch::<16>(hev_t, il, el, &mut b, 8 * stride + 8 - 4, stride);
            assert_eq!(a, b, "sb horizontal mismatch");
        }
    }
}

#[cfg(all(test, feature = "_benchmarks"))]
mod benches {
    use super::*;
    use test::{black_box, Bencher};

    #[rustfmt::skip]
    const TEST_DATA: [u8; 8 * 8] = [
        177, 192, 179, 181, 185, 174, 186, 193,
        185, 180, 175, 179, 175, 190, 189, 190,
        185, 181, 177, 190, 190, 174, 176, 188,
        192, 179, 186, 175, 190, 184, 190, 175,
        175, 183, 183, 190, 187, 186, 176, 181,
        183, 177, 182, 185, 183, 179, 178, 181,
        191, 183, 188, 181, 180, 193, 185, 180,
        177, 182, 177, 178, 179, 178, 191, 178,
    ];

    #[bench]
    fn measure_horizontal_macroblock_filter(b: &mut Bencher) {
        let hev_threshold = 5;
        let interior_limit = 15;
        let edge_limit = 15;

        let mut data = TEST_DATA.clone();
        let stride = 8;

        b.iter(|| {
            for y in 0..8 {
                black_box(macroblock_filter_horizontal(
                    hev_threshold,
                    interior_limit,
                    edge_limit,
                    &mut data[y * stride..][..8],
                ));
            }
        });
    }

    #[bench]
    fn measure_vertical_macroblock_filter(b: &mut Bencher) {
        let hev_threshold = 5;
        let interior_limit = 15;
        let edge_limit = 15;

        let mut data = TEST_DATA.clone();
        let stride = 8;

        b.iter(|| {
            for x in 0..8 {
                black_box(macroblock_filter_vertical(
                    hev_threshold,
                    interior_limit,
                    edge_limit,
                    &mut data,
                    4 * stride + x,
                    stride,
                ));
            }
        });
    }

    #[bench]
    fn measure_horizontal_subblock_filter(b: &mut Bencher) {
        let hev_threshold = 5;
        let interior_limit = 15;
        let edge_limit = 15;

        let mut data = TEST_DATA.clone();
        let stride = 8;

        b.iter(|| {
            for y in 0usize..8 {
                black_box(subblock_filter_horizontal(
                    hev_threshold,
                    interior_limit,
                    edge_limit,
                    &mut data[y * stride..][..8],
                ))
            }
        });
    }

    #[bench]
    fn measure_vertical_subblock_filter(b: &mut Bencher) {
        let hev_threshold = 5;
        let interior_limit = 15;
        let edge_limit = 15;

        let mut data = TEST_DATA.clone();
        let stride = 8;

        b.iter(|| {
            for x in 0..8 {
                black_box(subblock_filter_vertical(
                    hev_threshold,
                    interior_limit,
                    edge_limit,
                    &mut data,
                    4 * stride + x,
                    stride,
                ))
            }
        });
    }

    #[bench]
    fn measure_simple_segment_horizontal_filter(b: &mut Bencher) {
        let edge_limit = 15;

        let mut data = TEST_DATA.clone();
        let stride = 8;

        b.iter(|| {
            for y in 0usize..8 {
                black_box(simple_segment_horizontal(
                    edge_limit,
                    &mut data[y * stride..][..8],
                ))
            }
        });
    }

    #[bench]
    fn measure_simple_segment_vertical_filter(b: &mut Bencher) {
        let edge_limit = 15;

        let mut data = TEST_DATA.clone();
        let stride = 8;

        b.iter(|| {
            for x in 0usize..16 {
                black_box(simple_segment_vertical(
                    edge_limit,
                    &mut data,
                    4 * stride + x,
                    stride,
                ))
            }
        });
    }
}
