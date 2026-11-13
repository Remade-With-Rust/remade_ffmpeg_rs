//! Utilities for doing the YUV -> RGB conversion
//! The images are encoded in the Y'CbCr format as detailed here: <https://en.wikipedia.org/wiki/YCbCr>
//! so need to be converted to RGB to be displayed
//! To do the YUV -> RGB conversion we need to first decide how to map the yuv values to the pixels
//! The y buffer is the same size as the pixel buffer so that maps 1-1 but the
//! u and v buffers are half the size of the pixel buffer so we need to scale it up
//! The simple way to upscale is just to take each u/v value and associate it with the 4
//! pixels around it e.g. for a 4x4 image:
//!
//! ||||||
//! |yyyy|
//! |yyyy|
//! |yyyy|
//! |yyyy|
//! ||||||
//!
//! |||||||
//! |uu|vv|
//! |uu|vv|
//! |||||||
//!
//! Then each of the 2x2 pixels would match the u/v from the same quadrant
//!
//! However fancy upsampling is the default for libwebp which does a little more work to make the values smoother
//! It interpolates u and v so that for e.g. the pixel 1 down and 1 from the left the u value
//! would be (9*u0 + 3*u1 + 3*u2 + u3 + 8) / 16 and similar for the other pixels
//! The edges are mirrored, so for the pixel 1 down and 0 from the left it uses (9*u0 + 3*u2 + 3*u0 + u2 + 8) / 16

/// `_mm_mulhi_epu16` emulation
fn mulhi(v: u8, coeff: u16) -> i32 {
    ((u32::from(v) * u32::from(coeff)) >> 8) as i32
}

/// This function has been rewritten to encourage auto-vectorization.
///
/// Based on [src/dsp/yuv.h](https://github.com/webmproject/libwebp/blob/8534f53960befac04c9631e6e50d21dcb42dfeaf/src/dsp/yuv.h#L79)
/// from the libwebp source.
/// ```text
/// const YUV_FIX2: i32 = 6;
/// const YUV_MASK2: i32 = (256 << YUV_FIX2) - 1;
/// fn clip(v: i32) -> u8 {
///     if (v & !YUV_MASK2) == 0 {
///         (v >> YUV_FIX2) as u8
///     } else if v < 0 {
///         0
///     } else {
///         255
///     }
/// }
/// ```
// Clippy suggests the clamp method, but it seems to optimize worse as of rustc 1.82.0 nightly.
#[allow(clippy::manual_clamp)]
fn clip(v: i32) -> u8 {
    const YUV_FIX2: i32 = 6;
    (v >> YUV_FIX2).max(0).min(255) as u8
}

#[inline(always)]
fn yuv_to_r(y: u8, v: u8) -> u8 {
    clip(mulhi(y, 19077) + mulhi(v, 26149) - 14234)
}

#[inline(always)]
fn yuv_to_g(y: u8, u: u8, v: u8) -> u8 {
    clip(mulhi(y, 19077) - mulhi(u, 6419) - mulhi(v, 13320) + 8708)
}

#[inline(always)]
fn yuv_to_b(y: u8, u: u8) -> u8 {
    clip(mulhi(y, 19077) + mulhi(u, 33050) - 17685)
}

/// Per-image scratch rows, reused for every row: the full-width chroma of
/// the row being converted, and the column blend it is interpolated from.
struct Scratch {
    u: Vec<u8>,
    v: Vec<u8>,
    blend: Vec<u16>,
}

impl Scratch {
    fn new(width: usize) -> Self {
        Self {
            u: vec![0; width],
            v: vec![0; width],
            blend: vec![0; width.div_ceil(2)],
        }
    }
}

/// Fills an rgb buffer with the image from the yuv buffers
/// Size of the buffer is assumed to be correct
/// BPP is short for bytes per pixel, allows both rgb and rgba to be decoded
pub(crate) fn fill_rgb_buffer_fancy<const BPP: usize>(
    buffer: &mut [u8],
    y_buffer: &[u8],
    u_buffer: &[u8],
    v_buffer: &[u8],
    width: usize,
    height: usize,
    buffer_width: usize,
) {
    // buffer width is always even so don't need to do div_ceil
    let chroma_buffer_width = buffer_width / 2;
    let chroma_width = width.div_ceil(2);

    // Full-width chroma rows for the row being converted, reused for every row.
    let mut scratch = Scratch::new(width);

    // fill top row first since it only uses the top u/v row
    let top_row_y = &y_buffer[..width];
    let top_row_u = &u_buffer[..chroma_width];
    let top_row_v = &v_buffer[..chroma_width];
    let top_row_buffer = &mut buffer[..width * BPP];
    fill_row_fancy_with_1_uv_row::<BPP>(
        top_row_buffer,
        top_row_y,
        top_row_u,
        top_row_v,
        &mut scratch,
    );

    let mut main_row_chunks = buffer[width * BPP..].chunks_exact_mut(width * BPP * 2);
    // the y buffer iterator limits the end of the row iterator so we need this end index
    let end_y_index = height * buffer_width;
    let mut main_y_chunks = y_buffer[buffer_width..end_y_index].chunks_exact(buffer_width * 2);
    let mut main_u_windows = u_buffer
        .windows(chroma_buffer_width * 2)
        .step_by(chroma_buffer_width);
    let mut main_v_windows = v_buffer
        .windows(chroma_buffer_width * 2)
        .step_by(chroma_buffer_width);

    for (((row_buffer, y_rows), u_rows), v_rows) in (&mut main_row_chunks)
        .zip(&mut main_y_chunks)
        .zip(&mut main_u_windows)
        .zip(&mut main_v_windows)
    {
        let (u_row_1, u_row_2) = u_rows.split_at(chroma_buffer_width);
        let (v_row_1, v_row_2) = v_rows.split_at(chroma_buffer_width);
        let (row_buf_1, row_buf_2) = row_buffer.split_at_mut(width * BPP);
        let (y_row_1, y_row_2) = y_rows.split_at(buffer_width);
        fill_row_fancy_with_2_uv_rows::<BPP>(
            row_buf_1,
            &y_row_1[..width],
            &u_row_1[..chroma_width],
            &u_row_2[..chroma_width],
            &v_row_1[..chroma_width],
            &v_row_2[..chroma_width],
            &mut scratch,
        );
        fill_row_fancy_with_2_uv_rows::<BPP>(
            row_buf_2,
            &y_row_2[..width],
            &u_row_2[..chroma_width],
            &u_row_1[..chroma_width],
            &v_row_2[..chroma_width],
            &v_row_1[..chroma_width],
            &mut scratch,
        );
    }

    let final_row_buffer = main_row_chunks.into_remainder();

    // if the image has even height there will be one final row with only one u/v row matching it
    if !final_row_buffer.is_empty() {
        let final_y_row = main_y_chunks.remainder();

        let chroma_height = height.div_ceil(2);
        let start_chroma_index = (chroma_height - 1) * chroma_buffer_width;

        let final_u_row = &u_buffer[start_chroma_index..];
        let final_v_row = &v_buffer[start_chroma_index..];
        fill_row_fancy_with_1_uv_row::<BPP>(
            final_row_buffer,
            &final_y_row[..width],
            &final_u_row[..chroma_width],
            &final_v_row[..chroma_width],
            &mut scratch,
        );
    }
}

/// Fills a row with the fancy interpolation as detailed: each chroma row is
/// upsampled to full width (`u_row_1` / `v_row_1` are the rows nearest this
/// luma row), then the row is converted.
fn fill_row_fancy_with_2_uv_rows<const BPP: usize>(
    row_buffer: &mut [u8],
    y_row: &[u8],
    u_row_1: &[u8],
    u_row_2: &[u8],
    v_row_1: &[u8],
    v_row_2: &[u8],
    scratch: &mut Scratch,
) {
    let (u, v) = (&mut scratch.u[..y_row.len()], &mut scratch.v[..y_row.len()]);
    let blend = &mut scratch.blend[..u_row_1.len()];
    upsample_row(u_row_1, u_row_2, blend, u);
    upsample_row(v_row_1, v_row_2, blend, v);
    convert_row::<BPP>(row_buffer, y_row, u, v);
}

/// The first and (for even heights) last row have a single chroma row: the
/// same interpolation with that row standing in for both.
fn fill_row_fancy_with_1_uv_row<const BPP: usize>(
    row_buffer: &mut [u8],
    y_row: &[u8],
    u_row: &[u8],
    v_row: &[u8],
    scratch: &mut Scratch,
) {
    fill_row_fancy_with_2_uv_rows::<BPP>(row_buffer, y_row, u_row, u_row, v_row, v_row, scratch);
}

/// Fancy-upsample one chroma row to the full width `out.len()`. `near` is
/// the chroma row nearest the luma row being produced, `far` the other one
/// of the pair; both hold `ceil(width / 2)` samples. `blend` is scratch of
/// that length.
///
/// The four-tap weights (9, 3, 3, 1) / 16 are a 3:1 blend down the columns
/// followed by a 3:1 blend along the row, and the integer sums are the
/// same: `9a + 3a' + 3b + b' = 3(3a + b) + (3a' + b')`. So the column blend
/// is computed once per chroma sample, and each output is one more blend of
/// two of those. Pixel 0 and, for even widths, the last pixel sit outside
/// the chroma grid and mirror their single column. Plain slices throughout,
/// so both loops vectorize.
fn upsample_row(near: &[u8], far: &[u8], blend: &mut [u16], out: &mut [u8]) {
    for ((b, &n), &f) in blend.iter_mut().zip(near).zip(far) {
        *b = 3 * u16::from(n) + u16::from(f);
    }

    let width = out.len();
    out[0] = ((4 * blend[0] + 8) >> 4) as u8;

    let pairs = (width - 1) / 2;
    let (b0, b1) = (&blend[..pairs], &blend[1..=pairs]);
    for ((o, &x0), &x1) in out[1..].chunks_exact_mut(2).zip(b0).zip(b1) {
        o[0] = ((3 * x0 + x1 + 8) >> 4) as u8;
        o[1] = ((3 * x1 + x0 + 8) >> 4) as u8;
    }

    if width % 2 == 0 {
        out[width - 1] = ((4 * blend[pairs] + 8) >> 4) as u8;
    }
}

/// `(v * coeff) >> 8` without leaving 16 bits: with `coeff = hi * 256 + lo`,
/// the product is `v * hi * 256 + v * lo`, so the shifted value is exactly
/// `v * hi + ((v * lo) >> 8)`, and neither term can overflow `u16`. A 32-bit
/// product would be one multiply, but baseline x86-64 has no 32-bit vector
/// multiply and the loop would stay scalar.
#[inline(always)]
fn mulhi16(v: u8, coeff: u16) -> u16 {
    let v = u16::from(v);
    v * (coeff >> 8) + ((v * (coeff & 0xff)) >> 8)
}

/// Pixels converted per step of [`convert_row`].
const LANES: usize = 16;

/// [`yuv_to_r`], [`yuv_to_g`] and [`yuv_to_b`] for `LANES` pixels at once, in
/// 16-bit lanes (the way libwebp's SSE2 converter works):
///
/// * r and g: every partial sum fits `u16`, and the final value fits `i16`
///   (r in -14234..=30814, g in -10952..=27710), so wrapping 16-bit
///   arithmetic gives the 32-bit result exactly;
/// * b reaches 34238, past `i16`: its sum (at most 51923) stays unsigned and
///   the bias comes off with a saturating subtract, which is the same clamp
///   at zero that [`clip`] applies after the shift.
// As for `clip`: the clamp method optimizes worse than max/min here.
#[allow(clippy::manual_clamp)]
#[inline(always)]
fn convert_lanes(
    y: &[u8; LANES],
    u: &[u8; LANES],
    v: &[u8; LANES],
) -> ([u8; LANES], [u8; LANES], [u8; LANES]) {
    let (mut r, mut g, mut b) = ([0u8; LANES], [0u8; LANES], [0u8; LANES]);
    for i in 0..LANES {
        let luma = mulhi16(y[i], 19077);
        let red = luma.wrapping_add(mulhi16(v[i], 26149)).wrapping_sub(14234) as i16;
        let green = luma
            .wrapping_sub(mulhi16(u[i], 6419))
            .wrapping_sub(mulhi16(v[i], 13320))
            .wrapping_add(8708) as i16;
        let blue = (luma + mulhi16(u[i], 33050)).saturating_sub(17685);
        r[i] = (red >> 6).max(0).min(255) as u8;
        g[i] = (green >> 6).max(0).min(255) as u8;
        b[i] = (blue >> 6).min(255) as u8;
    }
    (r, g, b)
}

/// Convert one row of full-resolution y, u and v samples to rgb, writing the
/// first three bytes of every `BPP`-byte pixel.
#[inline(always)]
fn convert_row<const BPP: usize>(row_buffer: &mut [u8], y: &[u8], u: &[u8], v: &[u8]) {
    let mut out_chunks = row_buffer.chunks_exact_mut(LANES * BPP);
    let mut y_chunks = y.chunks_exact(LANES);
    let mut u_chunks = u.chunks_exact(LANES);
    let mut v_chunks = v.chunks_exact(LANES);

    for (((out, y), u), v) in (&mut out_chunks)
        .zip(&mut y_chunks)
        .zip(&mut u_chunks)
        .zip(&mut v_chunks)
    {
        let (r, g, b) = convert_lanes(
            y.try_into().unwrap(),
            u.try_into().unwrap(),
            v.try_into().unwrap(),
        );
        for (i, rgb) in out.chunks_exact_mut(BPP).enumerate() {
            rgb[0] = r[i];
            rgb[1] = g[i];
            rgb[2] = b[i];
        }
    }

    // the last few pixels of the row, one at a time
    for (((rgb, &y), &u), &v) in out_chunks
        .into_remainder()
        .chunks_exact_mut(BPP)
        .zip(y_chunks.remainder())
        .zip(u_chunks.remainder())
        .zip(v_chunks.remainder())
    {
        set_pixel(rgb, y, u, v);
    }
}

#[inline]
fn set_pixel(rgb: &mut [u8], y: u8, u: u8, v: u8) {
    rgb[0] = yuv_to_r(y, v);
    rgb[1] = yuv_to_g(y, u, v);
    rgb[2] = yuv_to_b(y, u);
}

/// Simple conversion, not currently used but could add a config to allow for using the simple
#[allow(unused)]
pub(crate) fn fill_rgb_buffer_simple<const BPP: usize>(
    buffer: &mut [u8],
    y_buffer: &[u8],
    u_buffer: &[u8],
    v_buffer: &[u8],
    width: usize,
    chroma_width: usize,
    buffer_width: usize,
) {
    // Full-width chroma rows for the row being converted, reused for every row.
    let mut scratch = Scratch::new(width);

    let u_row_twice_iter = u_buffer
        .chunks_exact(buffer_width / 2)
        .flat_map(|n| std::iter::repeat(n).take(2));
    let v_row_twice_iter = v_buffer
        .chunks_exact(buffer_width / 2)
        .flat_map(|n| std::iter::repeat(n).take(2));

    for (((row, y_row), u_row), v_row) in buffer
        .chunks_exact_mut(width * BPP)
        .zip(y_buffer.chunks_exact(buffer_width))
        .zip(u_row_twice_iter)
        .zip(v_row_twice_iter)
    {
        fill_rgba_row_simple::<BPP>(
            &y_row[..width],
            &u_row[..chroma_width],
            &v_row[..chroma_width],
            row,
            &mut scratch,
        );
    }
}

/// One row with simple (nearest) chroma: each chroma sample is repeated for
/// its two pixels into the scratch rows, then the row is converted like any
/// other (16 pixels at a time).
fn fill_rgba_row_simple<const BPP: usize>(
    y_vec: &[u8],
    u_vec: &[u8],
    v_vec: &[u8],
    rgba: &mut [u8],
    scratch: &mut Scratch,
) {
    let (u, v) = (&mut scratch.u[..y_vec.len()], &mut scratch.v[..y_vec.len()]);
    repeat_chroma(u_vec, u);
    repeat_chroma(v_vec, v);
    convert_row::<BPP>(rgba, y_vec, u, v);
}

/// Nearest-neighbour chroma: sample `k` goes to pixels `2k` and `2k + 1`
/// (an odd width ends on a single pixel). Fixed two-byte stores: `fill` on a
/// run-time-length pair is a `memset` call per pair.
fn repeat_chroma(chroma: &[u8], out: &mut [u8]) {
    let mut pairs = out.chunks_exact_mut(2);
    for (pair, &c) in (&mut pairs).zip(chroma) {
        pair[0] = c;
        pair[1] = c;
    }
    if let [last] = pairs.into_remainder() {
        *last = chroma[chroma.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fancy_grid() {
        #[rustfmt::skip]
        let y_buffer = [
            77, 162, 202, 185,
            28, 13, 199, 182,
            135, 147, 164, 135, 
            66, 27, 171, 130,
        ];

        #[rustfmt::skip]
        let u_buffer = [
            34, 101, 
            123, 163
        ];

        #[rustfmt::skip]
        let v_buffer = [
            97, 167,
            149, 23,
        ];

        let mut rgb_buffer = [0u8; 16 * 3];
        fill_rgb_buffer_fancy::<3>(&mut rgb_buffer, &y_buffer, &u_buffer, &v_buffer, 4, 4, 4);

        #[rustfmt::skip]
        let upsampled_u_buffer = [
            34, 51, 84, 101,
            56, 71, 101, 117,
            101, 112, 136, 148,
            123, 133, 153, 163,
        ];

        #[rustfmt::skip]
        let upsampled_v_buffer = [
            97, 115, 150, 167,
            110, 115, 126, 131,
            136, 117, 78, 59,
            149, 118, 55, 23,
        ];

        let mut upsampled_rgb_buffer = [0u8; 16 * 3];
        for (((rgb_val, y), u), v) in upsampled_rgb_buffer
            .chunks_exact_mut(3)
            .zip(y_buffer)
            .zip(upsampled_u_buffer)
            .zip(upsampled_v_buffer)
        {
            rgb_val[0] = yuv_to_r(y, v);
            rgb_val[1] = yuv_to_g(y, u, v);
            rgb_val[2] = yuv_to_b(y, u);
        }

        assert_eq!(rgb_buffer, upsampled_rgb_buffer);
    }

    /// The 16-bit lane conversion must equal the scalar formulas for every
    /// (y, u) and (y, v) pair, and for a spread of full triples.
    #[test]
    fn lanes_match_scalar() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let check = |y: [u8; LANES], u: [u8; LANES], v: [u8; LANES]| {
            let (r, g, b) = convert_lanes(&y, &u, &v);
            for i in 0..LANES {
                assert_eq!(r[i], yuv_to_r(y[i], v[i]), "r y {} v {}", y[i], v[i]);
                assert_eq!(
                    g[i],
                    yuv_to_g(y[i], u[i], v[i]),
                    "g {} {} {}",
                    y[i],
                    u[i],
                    v[i]
                );
                assert_eq!(b[i], yuv_to_b(y[i], u[i]), "b y {} u {}", y[i], u[i]);
            }
        };
        // r depends on (y, v), b on (y, u): cover both planes fully
        for y0 in (0..=255u8).step_by(LANES) {
            for c in 0..=255u8 {
                let y: [u8; LANES] = std::array::from_fn(|i| y0.wrapping_add(i as u8));
                check(y, [c; LANES], [c; LANES]);
                check(y, [c; LANES], [255 - c; LANES]);
            }
        }
        for _ in 0..20_000 {
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 24) as u8
            };
            let y: [u8; LANES] = std::array::from_fn(|_| next());
            let u: [u8; LANES] = std::array::from_fn(|_| next());
            let v: [u8; LANES] = std::array::from_fn(|_| next());
            check(y, u, v);
        }
    }

    /// Whole-buffer conversion at awkward sizes (odd and even, below and
    /// above one lane group) against a direct per-pixel reference.
    #[test]
    fn fancy_buffer_matches_reference() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        };
        for &(width, height) in &[
            (1usize, 1usize),
            (2, 2),
            (3, 5),
            (16, 4),
            (17, 3),
            (33, 7),
            (34, 8),
            (50, 2),
        ] {
            let buffer_width = width.div_ceil(16) * 16;
            let rows = height.div_ceil(16) * 16;
            let y_buffer: Vec<u8> = (0..buffer_width * rows).map(|_| next()).collect();
            let u_buffer: Vec<u8> = (0..buffer_width * rows / 4).map(|_| next()).collect();
            let v_buffer: Vec<u8> = (0..buffer_width * rows / 4).map(|_| next()).collect();
            let (cbw, cw, ch) = (buffer_width / 2, width.div_ceil(2), height.div_ceil(2));

            // reference: the interpolation weights written out per pixel
            let chroma = |plane: &[u8], x: usize, y: usize| -> u8 {
                let at = |cx: usize, cy: usize| u16::from(plane[cy * cbw + cx]);
                let (cx, cy) = (x / 2, y / 2);
                // the neighbour column / row on the side this pixel leans to
                let ox = if x % 2 == 0 {
                    cx.saturating_sub(1)
                } else {
                    (cx + 1).min(cw - 1)
                };
                let oy = if y % 2 == 0 {
                    cy.saturating_sub(1)
                } else {
                    (cy + 1).min(ch - 1)
                };
                ((9 * at(cx, cy) + 3 * at(ox, cy) + 3 * at(cx, oy) + at(ox, oy) + 8) / 16) as u8
            };
            for bpp in [3usize, 4] {
                let mut want = vec![7u8; width * height * bpp];
                for y in 0..height {
                    for x in 0..width {
                        let (l, u, v) = (
                            y_buffer[y * buffer_width + x],
                            chroma(&u_buffer, x, y),
                            chroma(&v_buffer, x, y),
                        );
                        let px = &mut want[(y * width + x) * bpp..][..3];
                        px[0] = yuv_to_r(l, v);
                        px[1] = yuv_to_g(l, u, v);
                        px[2] = yuv_to_b(l, u);
                    }
                }
                let mut got = vec![7u8; width * height * bpp];
                if bpp == 3 {
                    fill_rgb_buffer_fancy::<3>(
                        &mut got,
                        &y_buffer,
                        &u_buffer,
                        &v_buffer,
                        width,
                        height,
                        buffer_width,
                    );
                } else {
                    fill_rgb_buffer_fancy::<4>(
                        &mut got,
                        &y_buffer,
                        &u_buffer,
                        &v_buffer,
                        width,
                        height,
                        buffer_width,
                    );
                }
                assert_eq!(got, want, "{width}x{height} bpp {bpp}");
            }
        }
    }

    /// Simple (nearest) upsampling at awkward sizes against a per-pixel
    /// reference: pixel (x, y) takes chroma sample (x / 2, y / 2).
    #[test]
    fn simple_buffer_matches_reference() {
        let mut seed = 0xd1b5_4a32_d192_ed03u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        };
        for &(width, height) in &[
            (1usize, 1usize),
            (2, 2),
            (3, 5),
            (16, 4),
            (17, 3),
            (33, 7),
            (34, 8),
        ] {
            let buffer_width = width.div_ceil(16) * 16;
            let rows = height.div_ceil(16) * 16;
            let y_buffer: Vec<u8> = (0..buffer_width * rows).map(|_| next()).collect();
            let u_buffer: Vec<u8> = (0..buffer_width * rows / 4).map(|_| next()).collect();
            let v_buffer: Vec<u8> = (0..buffer_width * rows / 4).map(|_| next()).collect();
            let cbw = buffer_width / 2;
            for bpp in [3usize, 4] {
                let mut want = vec![7u8; width * height * bpp];
                for y in 0..height {
                    for x in 0..width {
                        let l = y_buffer[y * buffer_width + x];
                        let (u, v) = (
                            u_buffer[(y / 2) * cbw + x / 2],
                            v_buffer[(y / 2) * cbw + x / 2],
                        );
                        let px = &mut want[(y * width + x) * bpp..][..3];
                        px[0] = yuv_to_r(l, v);
                        px[1] = yuv_to_g(l, u, v);
                        px[2] = yuv_to_b(l, u);
                    }
                }
                let mut got = vec![7u8; width * height * bpp];
                let cw = width.div_ceil(2);
                if bpp == 3 {
                    fill_rgb_buffer_simple::<3>(
                        &mut got,
                        &y_buffer,
                        &u_buffer,
                        &v_buffer,
                        width,
                        cw,
                        buffer_width,
                    );
                } else {
                    fill_rgb_buffer_simple::<4>(
                        &mut got,
                        &y_buffer,
                        &u_buffer,
                        &v_buffer,
                        width,
                        cw,
                        buffer_width,
                    );
                }
                assert_eq!(got, want, "{width}x{height} bpp {bpp}");
            }
        }
    }

    #[test]
    fn test_yuv_conversions() {
        let (y, u, v) = (203, 40, 42);

        assert_eq!(yuv_to_r(y, v), 80);
        assert_eq!(yuv_to_g(y, u, v), 255);
        assert_eq!(yuv_to_b(y, u), 40);
    }
}
