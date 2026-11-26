//! Planar decoded images and the HEIF transforms (`clap`, `irot`, `imir`).
//!
//! Samples are `u16` at every bit depth, as the HEVC decoder produces them;
//! [`Image::bit_depth`] says how many bits are significant. Planes are
//! tightly packed, row-major.

use rff_core::{Error, Result};

/// Chroma layout of an [`Image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chroma {
    /// Luma only.
    Monochrome,
    /// Chroma at half width and half height.
    Yuv420,
    /// Chroma at half width, full height.
    Yuv422,
    /// Chroma at full resolution.
    Yuv444,
}

impl Chroma {
    /// From an HEVC `chroma_format_idc`.
    pub fn from_idc(idc: u8) -> Option<Chroma> {
        Some(match idc {
            0 => Chroma::Monochrome,
            1 => Chroma::Yuv420,
            2 => Chroma::Yuv422,
            3 => Chroma::Yuv444,
            _ => return None,
        })
    }

    /// Horizontal and vertical chroma subsampling shifts.
    pub fn shift(self) -> (u32, u32) {
        match self {
            Chroma::Yuv420 => (1, 1),
            Chroma::Yuv422 => (1, 0),
            Chroma::Monochrome | Chroma::Yuv444 => (0, 0),
        }
    }

    /// Number of planes (1 or 3).
    pub fn planes(self) -> usize {
        if self == Chroma::Monochrome {
            1
        } else {
            3
        }
    }

    /// Chroma plane size for a `w`×`h` luma plane (rounded up).
    pub fn chroma_size(self, w: u32, h: u32) -> (u32, u32) {
        let (sx, sy) = self.shift();
        (w.div_ceil(1 << sx), h.div_ceil(1 << sy))
    }
}

/// One sample plane: `width * height` samples, row-major, no padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u16>,
}

impl Plane {
    /// A plane filled with `value`.
    pub fn filled(width: u32, height: u32, value: u16) -> Plane {
        Plane {
            width,
            height,
            data: vec![value; width as usize * height as usize],
        }
    }

    /// Row `y`.
    pub fn row(&self, y: u32) -> &[u16] {
        let w = self.width as usize;
        &self.data[y as usize * w..(y as usize + 1) * w]
    }

    fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Plane {
        let mut data = Vec::with_capacity(w as usize * h as usize);
        for r in y..y + h {
            let row = self.row(r);
            data.extend_from_slice(&row[x as usize..(x + w) as usize]);
        }
        Plane {
            width: w,
            height: h,
            data,
        }
    }

    /// Rotate anti-clockwise by `quarters` × 90 degrees.
    fn rotate_ccw(&self, quarters: u8, threads: usize) -> Plane {
        let (w, h) = (self.width as usize, self.height as usize);
        let src = &self.data;
        match quarters & 3 {
            0 => self.clone(),
            // A half turn is the samples in reverse order: one pass, not a
            // clone followed by an in-place reverse.
            2 => Plane {
                width: self.width,
                height: self.height,
                data: src.iter().rev().copied().collect(),
            },
            q => Plane {
                width: self.height,
                height: self.width,
                data: rotate_quarter(src, w, h, q, threads),
            },
        }
    }

    /// Exchange left and right.
    fn flip_horizontal(&mut self) {
        let w = self.width as usize;
        if w == 0 {
            return;
        }
        for row in self.data.chunks_exact_mut(w) {
            row.reverse();
        }
    }

    /// Exchange top and bottom.
    fn flip_vertical(&mut self) {
        let (w, h) = (self.width as usize, self.height as usize);
        for y in 0..h / 2 {
            let (top, bottom) = self.data.split_at_mut((h - 1 - y) * w);
            top[y * w..(y + 1) * w].swap_with_slice(&mut bottom[..w]);
        }
    }
}

/// Rotate a `w`x`h` plane a quarter turn anti-clockwise (`q` = 1) or
/// clockwise (`q` = 3), in 64x64 blocks.
///
/// A quarter turn reads the source down its columns. Walking a whole output
/// row at a time touches a new cache line for every sample (a 12 MP photo's
/// plane is 8 KB a row), so the naive loop is bound by memory latency and
/// ran ~70 ms, serially after the parallel tile decode. In 64x64 blocks the
/// 64 source lines a block reads stay in L1 for all 64 of its output rows.
/// Same samples in the same places: [`rotate_quarter_naive`] is the oracle.
///
/// With `threads` > 1 the output is split into bands of whole block rows, one
/// thread per band: each thread writes only its own rows.
fn rotate_quarter(src: &[u16], w: usize, h: usize, q: u8, threads: usize) -> Vec<u16> {
    const B: usize = 64;
    let (nw, nh) = (h, w);
    let mut dst = vec![0u16; w * h];
    if nw == 0 || nh == 0 {
        return dst;
    }
    // Output rows `rows` x columns `cols` the sample at a time, into `out`
    // (whose first row is output row `y0`).
    let scalar = |y0: usize, out: &mut [u16], rows: (usize, usize), cols: (usize, usize)| {
        // Most blocks have no ragged strip: an empty column range must not
        // still walk (and bounds-check) every row.
        if cols.0 == cols.1 {
            return;
        }
        for r in rows.0..rows.1 {
            let yn = y0 + r;
            let o = &mut out[r * nw + cols.0..r * nw + cols.1];
            if q == 1 {
                // new[yn][xn] = old[xn][w-1-yn]
                let xo = w - 1 - yn;
                for (d, xn) in o.iter_mut().zip(cols.0..) {
                    *d = src[xn * w + xo];
                }
            } else {
                // new[yn][xn] = old[h-1-xn][yn]
                for (d, xn) in o.iter_mut().zip(cols.0..) {
                    *d = src[(h - 1 - xn) * w + yn];
                }
            }
        }
    };
    // `rows` output rows starting at row `y0`, into `out` (their samples).
    // Each block's whole 8x8 tiles go through the SIMD transpose; the ragged
    // right and bottom strips (only at the plane's edges) go sample by sample.
    let band = |y0: usize, out: &mut [u16]| {
        let rows = out.len() / nw;
        for by in (0..rows).step_by(B) {
            let ye = (by + B).min(rows);
            let ty = by + (ye - by) / 8 * 8;
            for bx in (0..nw).step_by(B) {
                let xe = (bx + B).min(nw);
                let tx = bx + (xe - bx) / 8 * 8;
                if ty > by && tx > bx {
                    crate::simd::rotate_tiles(src, w, h, q, out, nw, y0, (by, ty), (bx, tx));
                }
                scalar(y0, out, (by, ty), (tx, xe));
                scalar(y0, out, (ty, ye), (bx, xe));
            }
        }
    };
    let blocks = nh.div_ceil(B);
    let threads = threads.clamp(1, blocks);
    if threads == 1 {
        band(0, &mut dst);
    } else {
        let rows_per = blocks.div_ceil(threads) * B;
        std::thread::scope(|s| {
            for (i, chunk) in dst.chunks_mut(rows_per * nw).enumerate() {
                let band = &band;
                s.spawn(move || band(i * rows_per, chunk));
            }
        });
    }
    dst
}

/// The row-at-a-time quarter turn: the oracle for [`rotate_quarter`].
#[cfg(test)]
fn rotate_quarter_naive(src: &[u16], w: usize, h: usize, q: u8) -> Vec<u16> {
    let (nw, nh) = (h, w);
    let mut data = vec![0u16; w * h];
    for yn in 0..nh {
        for xn in 0..nw {
            data[yn * nw + xn] = if q == 1 {
                src[xn * w + (w - 1 - yn)]
            } else {
                src[(h - 1 - xn) * w + yn]
            };
        }
    }
    data
}

/// A decoded image: luma plus zero or two chroma planes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
    /// Significant bits per sample (8 or 10 for HEVC Main / Main 10).
    pub bit_depth: u8,
    /// Y, then Cb and Cr unless [`Chroma::Monochrome`].
    pub planes: Vec<Plane>,
}

impl Image {
    /// A blank image: luma `0`, chroma at the neutral midpoint.
    pub fn blank(width: u32, height: u32, chroma: Chroma, bit_depth: u8) -> Image {
        let mut planes = vec![Plane::filled(width, height, 0)];
        if chroma != Chroma::Monochrome {
            let (cw, ch) = chroma.chroma_size(width, height);
            let mid = 1u16 << (bit_depth - 1);
            planes.push(Plane::filled(cw, ch, mid));
            planes.push(Plane::filled(cw, ch, mid));
        }
        Image {
            width,
            height,
            chroma,
            bit_depth,
            planes,
        }
    }

    /// A zeroed image (chroma too): a canvas for callers that overwrite
    /// every sample. Unlike [`blank`](Self::blank) it writes nothing: a large
    /// zeroed allocation is fresh pages, where filling chroma with the
    /// midpoint is a pass over memory the caller is about to overwrite.
    pub(crate) fn zeroed(width: u32, height: u32, chroma: Chroma, bit_depth: u8) -> Image {
        let mut planes = vec![Plane::filled(width, height, 0)];
        if chroma != Chroma::Monochrome {
            let (cw, ch) = chroma.chroma_size(width, height);
            planes.push(Plane::filled(cw, ch, 0));
            planes.push(Plane::filled(cw, ch, 0));
        }
        Image {
            width,
            height,
            chroma,
            bit_depth,
            planes,
        }
    }

    /// The planes back to back, tightly packed: one byte per sample at up to
    /// 8 bits, little-endian `u16` above (the `yuv420p` / `yuv420p10le`
    /// layouts).
    pub fn to_planar_bytes(&self) -> Vec<u8> {
        let n: usize = self.planes.iter().map(|p| p.data.len()).sum();
        if self.bit_depth <= 8 {
            let mut out = Vec::with_capacity(n);
            for p in &self.planes {
                out.extend(p.data.iter().map(|&v| v as u8));
            }
            out
        } else {
            // Sized once and written in place: a two-byte `extend` per sample
            // re-checked the capacity every time and never vectorised.
            let mut out = vec![0u8; n * 2];
            let mut rest = out.as_mut_slice();
            for p in &self.planes {
                let (dst, tail) = rest.split_at_mut(p.data.len() * 2);
                for (o, &v) in dst.chunks_exact_mut(2).zip(&p.data) {
                    o.copy_from_slice(&v.to_le_bytes());
                }
                rest = tail;
            }
            out
        }
    }

    /// Crop to the luma rectangle `(x, y, w, h)`, which must lie inside the
    /// image. As in libheif, a 4:2:0 crop at an odd offset (or a 4:2:2 crop
    /// at an odd left edge) first converts to 4:4:4, so no chroma sample is
    /// shifted by half a pixel.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Image> {
        if w == 0 || h == 0 || x + w > self.width || y + h > self.height {
            return Err(Error::invalid("heif: crop rectangle outside the image"));
        }
        let odd = match self.chroma {
            Chroma::Yuv420 => x & 1 == 1 || y & 1 == 1,
            Chroma::Yuv422 => x & 1 == 1,
            _ => false,
        };
        if odd {
            return self.to_444().crop(x, y, w, h);
        }
        let (sx, sy) = self.chroma.shift();
        let planes = self
            .planes
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if i == 0 {
                    p.crop(x, y, w, h)
                } else {
                    // `right` and `bottom` are coordinates, so rounding them
                    // down keeps every chroma sample a kept luma sample uses.
                    let (cl, cr) = (x >> sx, (x + w - 1) >> sx);
                    let (ct, cb) = (y >> sy, (y + h - 1) >> sy);
                    p.crop(cl, ct, cr - cl + 1, cb - ct + 1)
                }
            })
            .collect();
        Ok(Image {
            width: w,
            height: h,
            chroma: self.chroma,
            bit_depth: self.bit_depth,
            planes,
        })
    }

    /// Upsample chroma to 4:4:4 (nearest neighbour).
    pub fn to_444(&self) -> Image {
        if matches!(self.chroma, Chroma::Yuv444 | Chroma::Monochrome) {
            return self.clone();
        }
        let (sx, sy) = self.chroma.shift();
        let mut planes = vec![self.planes[0].clone()];
        for p in &self.planes[1..] {
            let mut data = Vec::with_capacity(self.width as usize * self.height as usize);
            for y in 0..self.height {
                let row = p.row(y >> sy);
                data.extend((0..self.width).map(|x| row[(x >> sx) as usize]));
            }
            planes.push(Plane {
                width: self.width,
                height: self.height,
                data,
            });
        }
        Image {
            width: self.width,
            height: self.height,
            chroma: Chroma::Yuv444,
            bit_depth: self.bit_depth,
            planes,
        }
    }

    /// Rotate anti-clockwise by `quarters` × 90 degrees (`irot`).
    pub fn rotate_ccw(&self, quarters: u8) -> Image {
        self.rotate_ccw_threads(quarters, 1)
    }

    /// [`rotate_ccw`](Self::rotate_ccw) on up to `threads` threads.
    pub(crate) fn rotate_ccw_threads(&self, quarters: u8, threads: usize) -> Image {
        if quarters & 3 == 0 {
            return self.clone();
        }
        // Rotating 4:2:2 by a quarter turn would need 4:4:0; go via 4:4:4.
        if self.chroma == Chroma::Yuv422 && quarters & 1 == 1 {
            return self.to_444().rotate_ccw_threads(quarters, threads);
        }
        let planes: Vec<Plane> = self
            .planes
            .iter()
            .map(|p| p.rotate_ccw(quarters, threads))
            .collect();
        Image {
            width: planes[0].width,
            height: planes[0].height,
            chroma: self.chroma,
            bit_depth: self.bit_depth,
            planes,
        }
    }

    /// Mirror (`imir`), with libheif's reading of the axis bit: `0` exchanges
    /// top and bottom, `1` exchanges left and right.
    pub fn mirror(&mut self, axis: u8) {
        for p in &mut self.planes {
            if axis & 1 == 1 {
                p.flip_horizontal();
            } else {
                p.flip_vertical();
            }
        }
    }

    /// Copy `src` into this image with its top-left corner at luma `(x, y)`,
    /// clipped to this image. Both must share chroma layout and bit depth,
    /// and `x`, `y` must be multiples of the chroma subsampling.
    pub fn blit(&mut self, src: &Image, x: u32, y: u32) {
        for (i, (dp, sp)) in self.planes.iter_mut().zip(&src.planes).enumerate() {
            let (sx, sy) = if i == 0 { (0, 0) } else { self.chroma.shift() };
            blit_plane(
                dp,
                sp.width,
                sp.height,
                &sp.data,
                sp.width as usize,
                x >> sx,
                y >> sy,
            );
        }
    }
}

/// Copy a `w`×`h` block (rows `stride` apart in `src`) into `dst` at
/// `(x, y)`, clipped to `dst`.
pub(crate) fn blit_plane(
    dst: &mut Plane,
    w: u32,
    h: u32,
    src: &[u16],
    stride: usize,
    x: u32,
    y: u32,
) {
    if x >= dst.width || y >= dst.height {
        return;
    }
    let cw = w.min(dst.width - x) as usize;
    let ch = h.min(dst.height - y) as usize;
    let dw = dst.width as usize;
    for r in 0..ch {
        let d = (y as usize + r) * dw + x as usize;
        dst.data[d..d + cw].copy_from_slice(&src[r * stride..r * stride + cw]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(w: u32, h: u32) -> Plane {
        Plane {
            width: w,
            height: h,
            data: (0..w * h).map(|v| v as u16).collect(),
        }
    }

    #[test]
    fn rotations_compose_to_identity() {
        let p = ramp(5, 3);
        assert_eq!(p.rotate_ccw(1, 1).rotate_ccw(3, 1), p);
        assert_eq!(p.rotate_ccw(2, 1).rotate_ccw(2, 1), p);
        assert_eq!(p.rotate_ccw(1, 1).rotate_ccw(1, 1), p.rotate_ccw(2, 1));
        // 90 ccw moves the top-right corner to the top-left.
        let r = p.rotate_ccw(1, 1);
        assert_eq!((r.width, r.height), (3, 5));
        assert_eq!(r.data[0], p.data[4]);
        // 270 ccw (90 cw) moves the bottom-left corner to the top-left.
        let r = p.rotate_ccw(3, 1);
        assert_eq!(r.data[0], p.data[10]);
    }

    #[test]
    fn blocked_rotation_matches_naive() {
        for (w, h) in [
            (1, 1),
            (5, 3),
            (64, 64),
            (65, 63),
            (130, 7),
            (7, 200),
            (257, 129),
            // Whole 8x8 tiles only, tiles plus one ragged strip, both strips,
            // and blocks whose tile count differs per band.
            (8, 8),
            (16, 9),
            (9, 16),
            (67, 131),
            (136, 72),
            (200, 70),
            (4032 / 16, 3024 / 16 + 3),
        ] {
            let mut p = ramp(w, h);
            // Full-range values (not just a ramp): the extremes and noise.
            let mut s = 0x9e37_79b9u32;
            for (i, v) in p.data.iter_mut().enumerate() {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *v = match i % 5 {
                    0 => 0xffff,
                    1 => 0,
                    2 => 0x3ff,
                    _ => (s >> 16) as u16,
                };
            }
            for q in [1, 3] {
                for threads in [1, 2, 3, 8] {
                    assert_eq!(
                        rotate_quarter(&p.data, w as usize, h as usize, q, threads),
                        rotate_quarter_naive(&p.data, w as usize, h as usize, q),
                        "{w}x{h} q{q} threads {threads}"
                    );
                }
            }
        }
    }

    /// The packing against the per-sample loop it replaced, at every depth
    /// it distinguishes, odd sizes, and full-range sample values.
    #[test]
    fn planar_bytes_match_per_sample_packing() {
        let mut s = 0x7f4a_7c15u32;
        for (w, h) in [(1, 1), (3, 5), (17, 9), (64, 33)] {
            for chroma in [Chroma::Yuv420, Chroma::Yuv444, Chroma::Monochrome] {
                for bd in [8u8, 10, 12] {
                    let mut img = Image::blank(w, h, chroma, bd);
                    for p in &mut img.planes {
                        for v in &mut p.data {
                            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            *v = (s >> 16) as u16;
                        }
                    }
                    let mut want = Vec::new();
                    for p in &img.planes {
                        for &v in &p.data {
                            if bd <= 8 {
                                want.push(v as u8);
                            } else {
                                want.extend_from_slice(&v.to_le_bytes());
                            }
                        }
                    }
                    assert_eq!(img.to_planar_bytes(), want, "{w}x{h} {chroma:?} {bd}");
                }
            }
        }
    }

    #[test]
    fn flips() {
        let mut p = ramp(3, 2);
        p.flip_horizontal();
        assert_eq!(p.data, vec![2, 1, 0, 5, 4, 3]);
        let mut p = ramp(3, 3);
        p.flip_vertical();
        assert_eq!(p.data, vec![6, 7, 8, 3, 4, 5, 0, 1, 2]);
    }

    #[test]
    fn odd_420_crop_goes_through_444() {
        let img = Image::blank(8, 8, Chroma::Yuv420, 8);
        assert_eq!(img.crop(1, 0, 4, 4).unwrap().chroma, Chroma::Yuv444);
        let c = img.crop(2, 2, 5, 3).unwrap();
        assert_eq!(c.chroma, Chroma::Yuv420);
        assert_eq!((c.planes[1].width, c.planes[1].height), (3, 2));
        assert!(img.crop(6, 0, 4, 4).is_err());
    }
}
