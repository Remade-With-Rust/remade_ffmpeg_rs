//! YCbCr -> 8-bit RGBA, for callers who want display pixels.
//!
//! The matrix and range come from `nclx` (BT.601, full range when the file
//! has none: libheif's default profile). Chroma is upsampled
//! nearest-neighbour. This is a convenience, not a reference: compare
//! decoders in YCbCr, where there is no conversion to disagree about.
//!
//! [`to_rgba8_scalar`] is the definition: one pixel at a time, in f32. On a
//! CPU with AVX2, [`to_rgba8`] runs the same operations eight pixels at a
//! time (`simd::rgba_row`; for 4:2:0, the chroma terms once per chroma row
//! with `simd::chroma_terms`, shared by its two luma rows) and finishes each
//! row's last `width % 8` pixels with the scalar [`Conv::pixel`]; the output
//! is byte-identical.

use crate::image::{Chroma, Image};
use crate::simd::{self, RgbConsts, RgbMode};
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

/// The conversion of one picture: its constants and its per-pixel rule.
struct Conv {
    y_off: f32,
    y_scale: f32,
    c_scale: f32,
    mid: f32,
    k: Option<(f32, f32)>,
    mono: bool,
    amax: u32,
    premultiplied: bool,
}

impl Conv {
    fn new(img: &Image, alpha: Option<&Alpha>, nclx: Option<Nclx>) -> Conv {
        let nclx = nclx.unwrap_or(Nclx {
            primaries: 1,
            transfer: 13,
            matrix: 6,
            full_range: true,
        });
        let s = (1u32 << (img.bit_depth.saturating_sub(8))) as f32;
        let max = ((1u32 << img.bit_depth) - 1) as f32;
        let (y_off, y_scale, c_scale) = if nclx.full_range {
            (0.0, 1.0 / max, 1.0 / max)
        } else {
            (16.0 * s, 1.0 / (219.0 * s), 1.0 / (224.0 * s))
        };
        Conv {
            y_off,
            y_scale,
            c_scale,
            mid: (1u32 << (img.bit_depth - 1)) as f32,
            k: kr_kb(nclx.matrix),
            mono: img.chroma == Chroma::Monochrome,
            amax: alpha.map_or(1, |a| (1u32 << a.bit_depth) - 1),
            premultiplied: alpha.is_some_and(|a| a.premultiplied),
        }
    }

    /// One pixel: luma, the chroma samples it uses (ignored when mono) and
    /// its alpha sample, if any.
    #[inline(always)]
    fn pixel(&self, y: u16, cb: u16, cr: u16, a: Option<u16>) -> [u8; 4] {
        let to8 = |v: f32| (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        let yv = (y as f32 - self.y_off) * self.y_scale;
        let (r, g, b) = if self.mono {
            (yv, yv, yv)
        } else {
            let cb = (cb as f32 - self.mid) * self.c_scale;
            let cr = (cr as f32 - self.mid) * self.c_scale;
            match self.k {
                None => (cr + 0.5, yv, cb + 0.5),
                Some((kr, kb)) => {
                    let r = yv + 2.0 * (1.0 - kr) * cr;
                    let b = yv + 2.0 * (1.0 - kb) * cb;
                    let g = (yv - kr * r - kb * b) / (1.0 - kr - kb);
                    (r, g, b)
                }
            }
        };
        let amax = self.amax;
        let a8 = a.map_or(255u32, |a| (a as u32 * 255 + amax / 2) / amax);
        let (mut r8, mut g8, mut b8) = (to8(r), to8(g), to8(b));
        if self.premultiplied {
            let un = |c: u8| -> u8 {
                (c as u32 * 255 + a8 / 2)
                    .checked_div(a8)
                    .map_or(0, |v| v.min(255) as u8)
            };
            (r8, g8, b8) = (un(r8), un(g8), un(b8));
        }
        [r8, g8, b8, a8 as u8]
    }

    /// The constants [`simd::rgba_row`] needs, formed as [`pixel`](Self::pixel)
    /// forms them.
    fn simd(&self) -> (RgbConsts, RgbMode) {
        let (kr, kb) = self.k.unwrap_or((0.0, 0.0));
        let mode = match (self.mono, self.k) {
            (true, _) => RgbMode::Mono,
            (false, None) => RgbMode::Identity,
            (false, Some(_)) => RgbMode::Matrix,
        };
        (
            RgbConsts {
                y_off: self.y_off,
                y_scale: self.y_scale,
                c_scale: self.c_scale,
                mid: self.mid,
                kr,
                kb,
                kcr: 2.0 * (1.0 - kr),
                kcb: 2.0 * (1.0 - kb),
                kd: 1.0 - kr - kb,
                amax: self.amax,
            },
            mode,
        )
    }
}

pub(crate) fn to_rgba8(img: &Image, alpha: Option<&Alpha>, nclx: Option<Nclx>) -> Vec<u8> {
    let conv = Conv::new(img, alpha, nclx);
    if conv.premultiplied || img.width < 8 || !simd::rgba_simd_available() {
        return to_rgba8_scalar(img, alpha, nclx);
    }
    let (consts, mode) = conv.simd();
    let (w, h) = (img.width as usize, img.height as usize);
    let (sx, sy) = img.chroma.shift();
    let n = w / 8 * 8;
    let mut out = vec![0u8; w * h * 4];
    // When rows share a chroma row (4:2:0), its terms are computed once.
    let share = sy == 1 && !conv.mono;
    let (mut tr, mut tb) = if share {
        (vec![0f32; n], vec![0f32; n])
    } else {
        (Vec::new(), Vec::new())
    };
    let mut terms_row = None;
    for (y, o) in out.chunks_exact_mut(w * 4).enumerate() {
        let yr = img.planes[0].row(y as u32);
        let (cbr, crr) = if conv.mono {
            (yr, yr)
        } else {
            (
                img.planes[1].row((y >> sy) as u32),
                img.planes[2].row((y >> sy) as u32),
            )
        };
        let ar = alpha.map(|a| a.plane.row(y as u32));
        if share {
            if terms_row != Some(y >> sy) {
                simd::chroma_terms(&consts, mode, sx, cbr, crr, &mut tr, &mut tb, n);
                terms_row = Some(y >> sy);
            }
            simd::rgba_row_terms(&consts, mode, yr, &tr, &tb, ar, o, n);
        } else {
            simd::rgba_row(&consts, mode, sx, yr, cbr, crr, ar, o, n);
        }
        for x in n..w {
            let cx = x >> sx;
            o[x * 4..x * 4 + 4].copy_from_slice(&conv.pixel(
                yr[x],
                cbr[cx],
                crr[cx],
                ar.map(|r| r[x]),
            ));
        }
    }
    out
}

/// The scalar conversion: the oracle for [`to_rgba8`] and its fallback.
pub(crate) fn to_rgba8_scalar(img: &Image, alpha: Option<&Alpha>, nclx: Option<Nclx>) -> Vec<u8> {
    let conv = Conv::new(img, alpha, nclx);
    let (w, h) = (img.width as usize, img.height as usize);
    let (sx, sy) = img.chroma.shift();
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let yr = img.planes[0].row(y as u32);
        let (cbr, crr) = if conv.mono {
            (yr, yr)
        } else {
            (
                img.planes[1].row((y >> sy) as u32),
                img.planes[2].row((y >> sy) as u32),
            )
        };
        let ar = alpha.map(|a| a.plane.row(y as u32));
        for x in 0..w {
            let cx = x >> sx;
            let o = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
            o.copy_from_slice(&conv.pixel(yr[x], cbr[cx], crr[cx], ar.map(|r| r[x])));
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

    fn noise(s: &mut u32, max: u16) -> u16 {
        *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // A third of samples at the extremes (0, max, and past max: a
        // decoder never produces those, but the twin must agree anyway).
        match (*s >> 8) % 9 {
            0 => 0,
            1 => max,
            2 => u16::MAX,
            _ => ((*s >> 16) as u16) & max,
        }
    }

    fn random_image(w: u32, h: u32, chroma: Chroma, bd: u8, s: &mut u32) -> Image {
        let mut img = Image::blank(w, h, chroma, bd);
        let max = ((1u32 << bd) - 1) as u16;
        for p in &mut img.planes {
            for v in &mut p.data {
                *v = noise(s, max);
            }
        }
        img
    }

    /// The AVX2 path against the scalar oracle: every matrix code the
    /// conversion distinguishes, both ranges, 8/10/12 bits, every chroma
    /// layout, odd widths, and alpha at several depths.
    #[test]
    fn simd_matches_scalar() {
        if !simd::rgba_simd_available() {
            eprintln!("no AVX2 here: the SIMD path cannot run, nothing to compare");
            return;
        }
        let mut s = 0x2545_f491u32;
        let mut cases = 0;
        for &(w, h) in &[
            (8u32, 2u32),
            (9, 3),
            (15, 4),
            (16, 1),
            (33, 5),
            (64, 3),
            (127, 2),
        ] {
            for chroma in [
                Chroma::Yuv420,
                Chroma::Yuv422,
                Chroma::Yuv444,
                Chroma::Monochrome,
            ] {
                for bd in [8u8, 10, 12] {
                    let img = random_image(w, h, chroma, bd, &mut s);
                    for matrix in [0u16, 1, 4, 6, 7, 9, 2] {
                        for full_range in [true, false] {
                            let nclx = Some(Nclx {
                                primaries: 1,
                                transfer: 1,
                                matrix,
                                full_range,
                            });
                            for abd in [0u8, 8, 10, 16] {
                                let alpha = (abd > 0).then(|| {
                                    let max = ((1u32 << abd) - 1) as u16;
                                    let mut p = Plane::filled(w, h, 0);
                                    for v in &mut p.data {
                                        *v = noise(&mut s, max);
                                    }
                                    Alpha {
                                        plane: p,
                                        bit_depth: abd,
                                        premultiplied: false,
                                    }
                                });
                                assert_eq!(
                                    to_rgba8(&img, alpha.as_ref(), nclx),
                                    to_rgba8_scalar(&img, alpha.as_ref(), nclx),
                                    "{w}x{h} {chroma:?} {bd}-bit matrix {matrix} \
                                     full {full_range} alpha {abd}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(cases > 1000);
        // No nclx at all (libheif's default profile).
        let img = random_image(40, 3, Chroma::Yuv420, 8, &mut s);
        assert_eq!(
            to_rgba8(&img, None, None),
            to_rgba8_scalar(&img, None, None)
        );
    }

    /// Every 8-bit `(Y, Cb, Cr)` triple, through four matrix/range pairs.
    /// Random samples are not enough to pin the operation ORDER: a kernel
    /// that reassociated `g = (y - kr r) - kb b` as `(y - kb b) - kr r`
    /// passed 20,000 random pixels, because a one-ulp difference rarely
    /// crosses a rounding boundary of `to8`. Over all 2^24 triples it does.
    #[test]
    fn simd_matches_scalar_every_8bit_triple() {
        if !simd::rgba_simd_available() {
            return;
        }
        let (w, h) = (4096u32, 4096u32);
        let mut img = Image::blank(w, h, Chroma::Yuv444, 8);
        for (k, p) in img.planes.iter_mut().enumerate() {
            for (i, v) in p.data.iter_mut().enumerate() {
                *v = ((i >> (8 * k)) & 255) as u16;
            }
        }
        // The default profile, the two commonest coded matrices at limited
        // range, and identity (GBR). 2^24 pixels each.
        for (matrix, full_range) in [(6u16, true), (1, false), (9, false), (0, true)] {
            let nclx = Some(Nclx {
                primaries: 1,
                transfer: 1,
                matrix,
                full_range,
            });
            assert!(
                to_rgba8(&img, None, nclx) == to_rgba8_scalar(&img, None, nclx),
                "matrix {matrix} full {full_range}"
            );
        }
    }

    /// Every 8-bit triple again, through the 4:2:0 path (chroma terms
    /// computed once per chroma row and shared by two luma rows), which the
    /// 4:4:4 test above never enters. Chroma `(x, y)` holds `(Cb, Cr) =
    /// (x & 255, y & 255)`, repeated in 64 tiles; the 2x2 luma quad over a
    /// chroma sample in tile `t` holds `4t .. 4t + 3`, so the 64 tiles cover
    /// all 256 luma values for every chroma pair.
    #[test]
    fn simd_matches_scalar_every_8bit_triple_420() {
        if !simd::rgba_simd_available() {
            return;
        }
        let mut img = Image::blank(4096, 4096, Chroma::Yuv420, 8);
        let tile = |cx: usize, cy: usize| (cx >> 8) + 8 * (cy >> 8);
        for (i, v) in img.planes[0].data.iter_mut().enumerate() {
            let (x, y) = (i % 4096, i / 4096);
            *v = (4 * tile(x >> 1, y >> 1) + (x & 1) + 2 * (y & 1)) as u16;
        }
        let (lo, hi) = img.planes.split_at_mut(2);
        for (i, (cb, cr)) in lo[1].data.iter_mut().zip(hi[0].data.iter_mut()).enumerate() {
            (*cb, *cr) = ((i % 2048 & 255) as u16, (i / 2048 & 255) as u16);
        }
        for (matrix, full_range) in [(6u16, true), (1, false), (0, true)] {
            let nclx = Some(Nclx {
                primaries: 1,
                transfer: 1,
                matrix,
                full_range,
            });
            assert!(
                to_rgba8(&img, None, nclx) == to_rgba8_scalar(&img, None, nclx),
                "matrix {matrix} full {full_range}"
            );
        }
    }

    /// The kernel's f32 alpha division against the integer one, for every
    /// sample value at every alpha bit depth.
    #[test]
    fn simd_alpha_every_value() {
        if !simd::rgba_simd_available() {
            return;
        }
        let img = Image::blank(65536 / 256, 256, Chroma::Monochrome, 8);
        for abd in 1u8..=16 {
            let plane = Plane {
                width: 256,
                height: 256,
                data: (0..=u16::MAX).collect(),
            };
            let alpha = Alpha {
                plane,
                bit_depth: abd,
                premultiplied: false,
            };
            assert_eq!(
                to_rgba8(&img, Some(&alpha), None),
                to_rgba8_scalar(&img, Some(&alpha), None),
                "alpha {abd}-bit"
            );
        }
    }
}
