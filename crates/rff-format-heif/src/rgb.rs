//! YCbCr -> 8-bit RGBA, for callers who want display pixels.
//!
//! The matrix and range come from `nclx` (BT.601, full range when the file
//! has none: libheif's default profile). Chroma is upsampled
//! nearest-neighbour. This is a convenience, not a reference: compare
//! decoders in YCbCr, where there is no conversion to disagree about.

use crate::image::{Chroma, Image};
use crate::{Alpha, Nclx};

/// `(Kr, Kb)` for an H.273 `matrix_coefficients` code; `None` = identity (GBR).
fn kr_kb(matrix: u16) -> Option<(f32, f32)> {
    Some(match matrix {
        0 => return None,
        1 => (0.2126, 0.0722),
        4 => (0.30, 0.11),
        7 => (0.212, 0.087),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.299, 0.114), // 5, 6, unspecified, and anything unknown
    })
}

pub(crate) fn to_rgba8(img: &Image, alpha: Option<&Alpha>, nclx: Option<Nclx>) -> Vec<u8> {
    let nclx = nclx.unwrap_or(Nclx {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: true,
    });
    let (w, h) = (img.width as usize, img.height as usize);
    let s = (1u32 << (img.bit_depth.saturating_sub(8))) as f32;
    let max = ((1u32 << img.bit_depth) - 1) as f32;
    let (y_off, y_scale, c_scale) = if nclx.full_range {
        (0.0, 1.0 / max, 1.0 / max)
    } else {
        (16.0 * s, 1.0 / (219.0 * s), 1.0 / (224.0 * s))
    };
    let mid = (1u32 << (img.bit_depth - 1)) as f32;
    let k = kr_kb(nclx.matrix);
    let (sx, sy) = img.chroma.shift();
    let mono = img.chroma == Chroma::Monochrome;
    let to8 = |v: f32| (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    let amax = alpha.map_or(1, |a| (1u32 << a.bit_depth) - 1);

    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let yr = img.planes[0].row(y as u32);
        let (cbr, crr) = if mono {
            (yr, yr)
        } else {
            (
                img.planes[1].row((y >> sy) as u32),
                img.planes[2].row((y >> sy) as u32),
            )
        };
        let ar = alpha.map(|a| a.plane.row(y as u32));
        for x in 0..w {
            let yv = (yr[x] as f32 - y_off) * y_scale;
            let (r, g, b) = if mono {
                (yv, yv, yv)
            } else {
                let cx = x >> sx;
                let cb = (cbr[cx] as f32 - mid) * c_scale;
                let cr = (crr[cx] as f32 - mid) * c_scale;
                match k {
                    None => (cr + 0.5, yv, cb + 0.5),
                    Some((kr, kb)) => {
                        let r = yv + 2.0 * (1.0 - kr) * cr;
                        let b = yv + 2.0 * (1.0 - kb) * cb;
                        let g = (yv - kr * r - kb * b) / (1.0 - kr - kb);
                        (r, g, b)
                    }
                }
            };
            let o = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
            let a8 = ar.map_or(255u32, |r| (r[x] as u32 * 255 + amax / 2) / amax);
            let (mut r8, mut g8, mut b8) = (to8(r), to8(g), to8(b));
            if alpha.is_some_and(|a| a.premultiplied) {
                let un = |c: u8| -> u8 {
                    (c as u32 * 255 + a8 / 2)
                        .checked_div(a8)
                        .map_or(0, |v| v.min(255) as u8)
                };
                (r8, g8, b8) = (un(r8), un(g8), un(b8));
            }
            o.copy_from_slice(&[r8, g8, b8, a8 as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Plane;

    #[test]
    fn grey_and_primaries() {
        let mut img = Image::blank(2, 2, Chroma::Yuv420, 8);
        img.planes[0] = Plane::filled(2, 2, 128);
        let px = to_rgba8(&img, None, None);
        assert_eq!(&px[..4], &[128, 128, 128, 255]);
        // Limited-range white.
        img.planes[0] = Plane::filled(2, 2, 235);
        let lim = Nclx {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        };
        assert_eq!(&to_rgba8(&img, None, Some(lim))[..4], &[255, 255, 255, 255]);
    }
}
