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
    fn rotate_ccw(&self, quarters: u8) -> Plane {
        let (w, h) = (self.width as usize, self.height as usize);
        let src = &self.data;
        match quarters & 3 {
            0 => self.clone(),
            2 => {
                let mut data = src.clone();
                data.reverse();
                Plane {
                    width: self.width,
                    height: self.height,
                    data,
                }
            }
            q => {
                // 90 ccw: new[y'][x'] = old[x'][w-1-y'];  270 ccw: new[y'][x'] = old[h-1-x'][y'].
                let (nw, nh) = (h, w);
                let mut data = vec![0u16; w * h];
                for yn in 0..nh {
                    let out = &mut data[yn * nw..(yn + 1) * nw];
                    if q == 1 {
                        let xo = w - 1 - yn;
                        for (xn, d) in out.iter_mut().enumerate() {
                            *d = src[xn * w + xo];
                        }
                    } else {
                        let xo = yn;
                        for (xn, d) in out.iter_mut().enumerate() {
                            *d = src[(h - 1 - xn) * w + xo];
                        }
                    }
                }
                Plane {
                    width: nw as u32,
                    height: nh as u32,
                    data,
                }
            }
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
            let mut out = Vec::with_capacity(n * 2);
            for p in &self.planes {
                for &v in &p.data {
                    out.extend_from_slice(&v.to_le_bytes());
                }
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
        if quarters & 3 == 0 {
            return self.clone();
        }
        // Rotating 4:2:2 by a quarter turn would need 4:4:0; go via 4:4:4.
        if self.chroma == Chroma::Yuv422 && quarters & 1 == 1 {
            return self.to_444().rotate_ccw(quarters);
        }
        let planes: Vec<Plane> = self.planes.iter().map(|p| p.rotate_ccw(quarters)).collect();
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
        assert_eq!(p.rotate_ccw(1).rotate_ccw(3), p);
        assert_eq!(p.rotate_ccw(2).rotate_ccw(2), p);
        assert_eq!(p.rotate_ccw(1).rotate_ccw(1), p.rotate_ccw(2));
        // 90 ccw moves the top-right corner to the top-left.
        let r = p.rotate_ccw(1);
        assert_eq!((r.width, r.height), (3, 5));
        assert_eq!(r.data[0], p.data[4]);
        // 270 ccw (90 cw) moves the bottom-left corner to the top-left.
        let r = p.rotate_ccw(3);
        assert_eq!(r.data[0], p.data[10]);
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
