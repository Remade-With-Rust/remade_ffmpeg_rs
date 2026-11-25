//! The pixel operations of filter primitives.
//!
//! Each gives the bytes resvg 0.48.1's does (`filter/*.rs`, Copyright 2020
//! the Resvg Authors, Apache-2.0 OR MIT; see `NOTICE.md` for what resvg's
//! own are based on), computed differently where that is cheaper: the blurs
//! run along rows, the morphology window is built by doubling, the transfer
//! functions are tables and the turbulence lattice is looked up once for all
//! four channels.

use rusty_raster::Pixmap;

use crate::filter::*;
use crate::tree::Color;

/// An RGBA pixel.
pub(crate) type Px = [u8; 4];

pub(crate) fn px(p: &Pixmap) -> &[Px] {
    p.data().as_chunks().0
}

pub(crate) fn px_mut(p: &mut Pixmap) -> &mut [Px] {
    p.data_mut().as_chunks_mut().0
}

/// A zeroed buffer, or `None` when it cannot be had.
fn zeroed<T: Clone + Default>(len: usize) -> Option<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, T::default());
    Some(v)
}

/// `val` clamped to `min..=max`; NaN becomes `min`.
#[inline]
fn bound(min: f32, val: f32, max: f32) -> f32 {
    if val > max {
        max
    } else if val >= min {
        val
    } else {
        min
    }
}

// --------------------------------------------------------------- colour --

/// sRGB to linear RGB: `c / 12.92` up to 0.04045, else
/// `((c + 0.055) / 1.055) ^ 2.4`.
#[rustfmt::skip]
const TO_LINEAR: [u8; 256] = [
    0,   0,   0,   0,   0,   0,  0,    1,   1,   1,   1,   1,   1,   1,   1,   1,
    1,   1,   2,   2,   2,   2,  2,    2,   2,   2,   3,   3,   3,   3,   3,   3,
    4,   4,   4,   4,   4,   5,  5,    5,   5,   6,   6,   6,   6,   7,   7,   7,
    8,   8,   8,   8,   9,   9,  9,   10,  10,  10,  11,  11,  12,  12,  12,  13,
    13,  13,  14,  14,  15,  15,  16,  16,  17,  17,  17,  18,  18,  19,  19,  20,
    20,  21,  22,  22,  23,  23,  24,  24,  25,  25,  26,  27,  27,  28,  29,  29,
    30,  30,  31,  32,  32,  33,  34,  35,  35,  36,  37,  37,  38,  39,  40,  41,
    41,  42,  43,  44,  45,  45,  46,  47,  48,  49,  50,  51,  51,  52,  53,  54,
    55,  56,  57,  58,  59,  60,  61,  62,  63,  64,  65,  66,  67,  68,  69,  70,
    71,  72,  73,  74,  76,  77,  78,  79,  80,  81,  82,  84,  85,  86,  87,  88,
    90,  91,  92,  93,  95,  96,  97,  99, 100, 101, 103, 104, 105, 107, 108, 109,
    111, 112, 114, 115, 116, 118, 119, 121, 122, 124, 125, 127, 128, 130, 131, 133,
    134, 136, 138, 139, 141, 142, 144, 146, 147, 149, 151, 152, 154, 156, 157, 159,
    161, 163, 164, 166, 168, 170, 171, 173, 175, 177, 179, 181, 183, 184, 186, 188,
    190, 192, 194, 196, 198, 200, 202, 204, 206, 208, 210, 212, 214, 216, 218, 220,
    222, 224, 226, 229, 231, 233, 235, 237, 239, 242, 244, 246, 248, 250, 253, 255,
];

/// Linear RGB to sRGB: `c * 12.92` up to 0.0031308, else
/// `1.055 * c ^ (1 / 2.4) - 0.055`.
#[rustfmt::skip]
const TO_SRGB: [u8; 256] = [
    0,  13,  22,  28,  34,  38,  42,  46,  50,  53,  56,  59,  61,  64,  66,  69,
    71,  73,  75,  77,  79,  81,  83,  85,  86,  88,  90,  92,  93,  95,  96,  98,
    99, 101, 102, 104, 105, 106, 108, 109, 110, 112, 113, 114, 115, 117, 118, 119,
    120, 121, 122, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136,
    137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 148, 149, 150, 151,
    152, 153, 154, 155, 155, 156, 157, 158, 159, 159, 160, 161, 162, 163, 163, 164,
    165, 166, 167, 167, 168, 169, 170, 170, 171, 172, 173, 173, 174, 175, 175, 176,
    177, 178, 178, 179, 180, 180, 181, 182, 182, 183, 184, 185, 185, 186, 187, 187,
    188, 189, 189, 190, 190, 191, 192, 192, 193, 194, 194, 195, 196, 196, 197, 197,
    198, 199, 199, 200, 200, 201, 202, 202, 203, 203, 204, 205, 205, 206, 206, 207,
    208, 208, 209, 209, 210, 210, 211, 212, 212, 213, 213, 214, 214, 215, 215, 216,
    216, 217, 218, 218, 219, 219, 220, 220, 221, 221, 222, 222, 223, 223, 224, 224,
    225, 226, 226, 227, 227, 228, 228, 229, 229, 230, 230, 231, 231, 232, 232, 233,
    233, 234, 234, 235, 235, 236, 236, 237, 237, 238, 238, 238, 239, 239, 240, 240,
    241, 241, 242, 242, 243, 243, 244, 244, 245, 245, 246, 246, 246, 247, 247, 248,
    248, 249, 249, 250, 250, 251, 251, 251, 252, 252, 253, 253, 254, 254, 255, 255,
];

#[inline]
fn demultiply(c: u8, a: f32) -> u8 {
    (f32::from(c) / a + 0.5) as u8
}

#[inline]
fn multiply(c: u8, a: f32) -> u8 {
    (f32::from(c) * a + 0.5) as u8
}

/// Runs `f` on every pixel without its alpha multiplied in, and multiplies
/// the alpha `f` leaves back in. `f` must map transparent black to some
/// pixel the same way for equal inputs (it is a pure function); opaque and
/// fully transparent pixels skip the arithmetic that would not change them.
#[inline(always)]
fn map_straight(data: &mut [Px], mut f: impl FnMut(Px) -> Px) {
    for p in data {
        let a = p[3];
        let straight = match a {
            255 => *p,
            // `c / 0`: 0 is NaN, which casts to 0; anything else saturates.
            0 => [
                if p[0] == 0 { 0 } else { 255 },
                if p[1] == 0 { 0 } else { 255 },
                if p[2] == 0 { 0 } else { 255 },
                0,
            ],
            _ => {
                let af = f32::from(a) / 255.0;
                [
                    demultiply(p[0], af),
                    demultiply(p[1], af),
                    demultiply(p[2], af),
                    a,
                ]
            }
        };
        let out = f(straight);
        *p = match out[3] {
            255 => out,
            0 => [0; 4],
            a => {
                let af = f32::from(a) / 255.0;
                [
                    multiply(out[0], af),
                    multiply(out[1], af),
                    multiply(out[2], af),
                    a,
                ]
            }
        };
    }
}

/// Converts premultiplied pixels of the other colour space to `to`.
pub(crate) fn convert_space(data: &mut [Px], to: ColorInterpolation) {
    let t = match to {
        ColorInterpolation::SRgb => &TO_SRGB,
        ColorInterpolation::LinearRgb => &TO_LINEAR,
    };
    map_straight(data, |p| {
        [
            t[usize::from(p[0])],
            t[usize::from(p[1])],
            t[usize::from(p[2])],
            p[3],
        ]
    });
}

fn transfer_table(func: &TransferFunction) -> [u8; 256] {
    std::array::from_fn(|i| {
        let c = i as f32 / 255.0;
        let c = match func {
            TransferFunction::Identity => return i as u8,
            TransferFunction::Table(v) | TransferFunction::Discrete(v) if v.is_empty() => {
                return i as u8
            }
            TransferFunction::Table(v) => {
                let n = v.len() - 1;
                let k = ((c * n as f32).floor() as usize).min(n);
                if k == n {
                    v[k]
                } else {
                    let (vk, vk1) = (v[k], v[k + 1]);
                    let (k, n) = (k as f32, n as f32);
                    vk + (c - k / n) * n * (vk1 - vk)
                }
            }
            TransferFunction::Discrete(v) => {
                let k = (c * v.len() as f32).floor() as usize;
                v[k.min(v.len() - 1)]
            }
            TransferFunction::Linear { slope, intercept } => slope * c + intercept,
            TransferFunction::Gamma {
                amplitude,
                exponent,
                offset,
            } => amplitude * c.powf(*exponent) + offset,
        };
        (bound(0.0, c, 1.0) * 255.0) as u8
    })
}

pub(crate) fn component_transfer(fe: &ComponentTransfer, data: &mut [Px]) {
    let [r, g, b, a] = [&fe.func_r, &fe.func_g, &fe.func_b, &fe.func_a].map(transfer_table);
    map_straight(data, |p| {
        [
            r[usize::from(p[0])],
            g[usize::from(p[1])],
            b[usize::from(p[2])],
            a[usize::from(p[3])],
        ]
    });
}

#[inline]
fn from_normalized(c: f32) -> u8 {
    (bound(0.0, c, 1.0) * 255.0) as u8
}

pub(crate) fn color_matrix(kind: &ColorMatrixKind, data: &mut [Px]) {
    let norm = |p: Px| p.map(|c| f32::from(c) / 255.0);
    let rgb_matrix = |data: &mut [Px], m: [f32; 9]| {
        map_straight(data, |p| {
            let [r, g, b, _] = norm(p);
            [
                from_normalized(r * m[0] + g * m[1] + b * m[2]),
                from_normalized(r * m[3] + g * m[4] + b * m[5]),
                from_normalized(r * m[6] + g * m[7] + b * m[8]),
                p[3],
            ]
        });
    };
    match *kind {
        ColorMatrixKind::Matrix(m) => map_straight(data, |p| {
            let [r, g, b, a] = norm(p);
            let row = |i: usize| {
                from_normalized(r * m[i] + g * m[i + 1] + b * m[i + 2] + a * m[i + 3] + m[i + 4])
            };
            [row(0), row(5), row(10), row(15)]
        }),
        ColorMatrixKind::Saturate(v) => {
            let v = v.max(0.0);
            rgb_matrix(
                data,
                [
                    0.213 + 0.787 * v,
                    0.715 - 0.715 * v,
                    0.072 - 0.072 * v,
                    0.213 - 0.213 * v,
                    0.715 + 0.285 * v,
                    0.072 - 0.072 * v,
                    0.213 - 0.213 * v,
                    0.715 - 0.715 * v,
                    0.072 + 0.928 * v,
                ],
            );
        }
        ColorMatrixKind::HueRotate(angle) => {
            let (a2, a1) = (angle.to_radians().sin(), angle.to_radians().cos());
            rgb_matrix(
                data,
                [
                    0.213 + 0.787 * a1 - 0.213 * a2,
                    0.715 - 0.715 * a1 - 0.715 * a2,
                    0.072 - 0.072 * a1 + 0.928 * a2,
                    0.213 - 0.213 * a1 + 0.143 * a2,
                    0.715 + 0.285 * a1 + 0.140 * a2,
                    0.072 - 0.072 * a1 - 0.283 * a2,
                    0.213 - 0.213 * a1 - 0.787 * a2,
                    0.715 - 0.715 * a1 + 0.715 * a2,
                    0.072 + 0.928 * a1 + 0.072 * a2,
                ],
            );
        }
        ColorMatrixKind::LuminanceToAlpha => map_straight(data, |p| {
            let [r, g, b, _] = norm(p);
            [
                0,
                0,
                0,
                from_normalized(r * 0.2125 + g * 0.7154 + b * 0.0721),
            ]
        }),
    }
}

/// `k1*a*b + k2*a + k3*b + k4` into `b`, premultiplied.
pub(crate) fn arithmetic([k1, k2, k3, k4]: [f32; 4], a: &[Px], b: &mut [Px]) {
    let calc = |i1: u8, i2: u8, max: f32| {
        let i1 = f32::from(i1) / 255.0;
        let i2 = f32::from(i2) / 255.0;
        bound(0.0, k1 * i1 * i2 + k2 * i1 + k3 * i2 + k4, max)
    };
    for (p, q) in a.iter().zip(b) {
        let alpha = calc(p[3], q[3], 1.0);
        *q = if crate::convert::approx_zero(alpha) {
            [0; 4]
        } else {
            let c = |i: usize| (calc(p[i], q[i], alpha) * 255.0) as u8;
            [c(0), c(1), c(2), (alpha * 255.0) as u8]
        };
    }
}

// ----------------------------------------------------------------- blur --

const BOXES: usize = 5;

/// The radii of the five box blurs that approximate a Gaussian.
fn box_radii(sigma: f32) -> [usize; BOXES] {
    if sigma <= 0.0 || sigma.is_nan() {
        return [0; BOXES];
    }
    let n = BOXES as f32;
    // The ideal averaging width, and the odd widths around it.
    let ideal = (12.0 * sigma * sigma / n).sqrt() + 1.0;
    let mut lower = (ideal.floor() as i32).min(i32::MAX - 4);
    if lower % 2 == 0 {
        lower -= 1;
    }
    let l = lower as f32;
    let m = (12.0 * sigma * sigma - n * l * l - 4.0 * n * l - 3.0 * n) / (-4.0 * l - 4.0);
    let m = m.round() as usize;
    std::array::from_fn(|i| {
        let size = if i < m { lower } else { lower + 2 };
        ((size - 1) / 2) as usize
    })
}

/// Rounds a non-negative value under 2^22 to the nearest integer (ties to
/// even), as a byte.
#[inline(always)]
fn round_byte(v: f32) -> u8 {
    // The integer ends up in the low mantissa bits.
    (v + 12_582_912.0).to_bits() as u8
}

/// Five box blurs along each axis. Outside the image counts as transparent.
pub(crate) fn box_blur(sigma_x: f32, sigma_y: f32, w: usize, data: &mut [Px]) -> Option<()> {
    if w == 0 || data.is_empty() {
        return Some(());
    }
    let mut back: Vec<Px> = zeroed(data.len())?;
    let mut sums: Vec<i32> = zeroed(w * 4)?;
    for (rx, ry) in box_radii(sigma_x).into_iter().zip(box_radii(sigma_y)) {
        if ry == 0 {
            // The vertical blur does nothing.
            back.copy_from_slice(data);
        } else {
            box_vertical(ry, w, data, &mut back, &mut sums);
        }
        if rx == 0 {
            data.copy_from_slice(&back);
        } else {
            for (src, dst) in back.chunks_exact(w).zip(data.chunks_exact_mut(w)) {
                box_row(rx, src, dst);
            }
        }
    }
    Some(())
}

/// One row: a sum sliding over `x - r ..= x + r`.
fn box_row(r: usize, src: &[Px], dst: &mut [Px]) {
    let w = src.len().min(dst.len());
    let scale = 1.0 / (r + r + 1) as f32;
    let mut sum = [0i32; 4];
    let add = |sum: &mut [i32; 4], p: Px| {
        for (s, c) in sum.iter_mut().zip(p) {
            *s += i32::from(c);
        }
    };
    let sub = |sum: &mut [i32; 4], p: Px| {
        for (s, c) in sum.iter_mut().zip(p) {
            *s -= i32::from(c);
        }
    };
    for p in &src[..r.min(w)] {
        add(&mut sum, *p);
    }
    // Where the window enters the row on the right, and leaves on the left.
    let enter = src.get(r..).unwrap_or(&[]);
    let put = |sum: &[i32; 4]| sum.map(|s| round_byte(s as f32 * scale));
    let head = w.min(r + 1);
    for (x, d) in dst[..head].iter_mut().enumerate() {
        if let Some(p) = enter.get(x) {
            add(&mut sum, *p);
        }
        *d = put(&sum);
    }
    if w <= r + 1 {
        return;
    }
    let leave = &src[..w - r - 1];
    let enter = enter.get(r + 1..).unwrap_or(&[]);
    let tail = &mut dst[r + 1..w];
    let mid = enter.len().min(tail.len());
    let (tail_mid, tail_end) = tail.split_at_mut(mid);
    let (leave_mid, leave_end) = leave.split_at(mid);
    for ((d, p), q) in tail_mid.iter_mut().zip(&enter[..mid]).zip(leave_mid) {
        add(&mut sum, *p);
        sub(&mut sum, *q);
        *d = put(&sum);
    }
    for (d, q) in tail_end.iter_mut().zip(leave_end) {
        sub(&mut sum, *q);
        *d = put(&sum);
    }
}

/// Every column at once: one sum per byte of a row, updated row by row.
fn box_vertical(r: usize, w: usize, src: &[Px], dst: &mut [Px], sums: &mut [i32]) {
    let h = src.len() / w;
    let scale = 1.0 / (r + r + 1) as f32;
    let row = |y: usize| src[y * w..(y + 1) * w].as_flattened();
    sums.fill(0);
    for y in 0..r.min(h) {
        for (s, c) in sums.iter_mut().zip(row(y)) {
            *s += i32::from(*c);
        }
    }
    for (y, out) in dst.chunks_exact_mut(w).enumerate() {
        let out = out.as_flattened_mut();
        let enter = (y + r < h).then(|| row(y + r));
        let leave = (y > r).then(|| row(y - r - 1));
        match (enter, leave) {
            (Some(e), Some(l)) => {
                for (((d, s), e), l) in out.iter_mut().zip(sums.iter_mut()).zip(e).zip(l) {
                    *s += i32::from(*e) - i32::from(*l);
                    *d = round_byte(*s as f32 * scale);
                }
            }
            (Some(e), None) => {
                for ((d, s), e) in out.iter_mut().zip(sums.iter_mut()).zip(e) {
                    *s += i32::from(*e);
                    *d = round_byte(*s as f32 * scale);
                }
            }
            (None, Some(l)) => {
                for ((d, s), l) in out.iter_mut().zip(sums.iter_mut()).zip(l) {
                    *s -= i32::from(*l);
                    *d = round_byte(*s as f32 * scale);
                }
            }
            (None, None) => {
                for (d, s) in out.iter_mut().zip(sums.iter()) {
                    *d = round_byte(*s as f32 * scale);
                }
            }
        }
    }
}

const IIR_STEPS: usize = 4;

/// `(lambda, nu)` of the recursive filter (Alvarez and Mazorra).
fn iir_coefficients(sigma: f64) -> (f64, f64) {
    let lambda = (sigma * sigma) / (2.0 * IIR_STEPS as f64);
    let nu = (1.0 + 2.0 * lambda - (1.0 + 4.0 * lambda).sqrt()) / (2.0 * lambda);
    (lambda, nu)
}

/// A Gaussian as a cascade of first-order recursive filters, each run
/// forwards and backwards along each axis.
pub(crate) fn iir_blur(sigma_x: f64, sigma_y: f64, w: usize, data: &mut [Px]) -> Option<()> {
    if w == 0 || data.is_empty() {
        return Some(());
    }
    let mut buf: Vec<f64> = zeroed(data.len())?;
    let mut lanes: Vec<[f64; 4]> = zeroed(w)?;
    let x = (sigma_x > 0.0).then(|| iir_coefficients(sigma_x));
    let y = (sigma_y > 0.0).then(|| iir_coefficients(sigma_y));
    let (lambda_x, nu_x) = x.unwrap_or((1.0, 1.0));
    let (lambda_y, nu_y) = y.unwrap_or((1.0, 1.0));
    // An exponent the compiler cannot see: the run-time `powi` is what resvg
    // ends up with, and on some platforms it differs from the multiplications
    // a constant one becomes in the last bit (which truncation makes visible
    // on flat areas).
    let steps = std::hint::black_box(2 * IIR_STEPS as i32);
    let scale = ((nu_x * nu_y).sqrt() / (lambda_x * lambda_y).sqrt()).powi(steps);
    for channel in 0..4 {
        for (b, p) in buf.iter_mut().zip(data.iter()) {
            *b = f64::from(p[channel]) / 255.0;
        }
        if x.is_some() {
            let mut rows = buf.chunks_exact_mut(w * 4);
            for four in rows.by_ref() {
                iir_rows::<4>(nu_x, w, four, &mut lanes);
            }
            for one in rows.into_remainder().chunks_exact_mut(w) {
                iir_rows::<1>(nu_x, w, one, &mut lanes);
            }
        }
        if y.is_some() {
            for _ in 0..IIR_STEPS {
                let mut at = w;
                while at < buf.len() {
                    let (prev, cur) = buf[at - w..at + w].split_at_mut(w);
                    for (c, p) in cur.iter_mut().zip(prev) {
                        *c += nu_y * *p;
                    }
                    at += w;
                }
                while at > w {
                    at -= w;
                    let (cur, next) = buf[at - w..at + w].split_at_mut(w);
                    for (c, n) in cur.iter_mut().zip(next) {
                        *c += nu_y * *n;
                    }
                }
            }
        }
        for (p, b) in data.iter_mut().zip(&buf) {
            p[channel] = (b * scale * 255.0) as u8;
        }
    }
    Some(())
}

/// The recursive filter along `N` rows at once (they are independent, so
/// they share the wait on each other's arithmetic).
fn iir_rows<const N: usize>(nu: f64, w: usize, rows: &mut [f64], lanes: &mut [[f64; 4]]) {
    for (x, lane) in lanes.iter_mut().enumerate() {
        for (n, v) in lane[..N].iter_mut().enumerate() {
            *v = rows[n * w + x];
        }
    }
    let step = |cur: &mut [f64; 4], prev: &[f64; 4]| {
        for (c, p) in cur[..N].iter_mut().zip(prev) {
            *c += nu * p;
        }
    };
    for _ in 0..IIR_STEPS {
        for x in 1..w {
            let (a, b) = lanes.split_at_mut(x);
            step(&mut b[0], &a[x - 1]);
        }
        for x in (1..w).rev() {
            let (a, b) = lanes.split_at_mut(x);
            step(&mut a[x - 1], &b[0]);
        }
    }
    for (x, lane) in lanes.iter().enumerate() {
        for (n, v) in lane[..N].iter().enumerate() {
            rows[n * w + x] = *v;
        }
    }
}

// ----------------------------------------------------------- morphology --

/// The minimum (`erode`) or maximum over a window; pixels outside the image
/// do not take part.
pub(crate) fn morphology(erode: bool, rx: f32, ry: f32, w: usize, data: &mut [Px]) -> Option<()> {
    if w == 0 || data.is_empty() {
        return Some(());
    }
    let h = data.len() / w;
    // resvg's window: an even number of pixels (`2 * ceil(r)`), no larger
    // than the image, reaching `len / 2` back.
    let window = |r: f32, max: usize| (r.ceil() as usize).saturating_mul(2).min(max);
    if erode {
        morph_axes::<true>(window(rx, w), window(ry, h), w, data)
    } else {
        morph_axes::<false>(window(rx, w), window(ry, h), w, data)
    }
}

#[inline(always)]
fn pick<const MIN: bool>(a: u8, b: u8) -> u8 {
    if MIN {
        a.min(b)
    } else {
        a.max(b)
    }
}

fn morph_axes<const MIN: bool>(cols: usize, rows: usize, w: usize, data: &mut [Px]) -> Option<()> {
    let identity = if MIN { 255u8 } else { 0 };
    let h = data.len() / w;
    // Rows: each is padded on both sides, then every window starting at `i`
    // is folded into position `i` by doubling.
    let back = cols / 2;
    let len = back + w + cols;
    let mut line: Vec<u8> = zeroed(len * 4)?;
    for row in data.chunks_exact_mut(w) {
        line.fill(identity);
        line[back * 4..(back + w) * 4].copy_from_slice(row.as_flattened());
        fold::<MIN>(&mut line, cols, 4);
        row.as_flattened_mut().copy_from_slice(&line[..w * 4]);
    }
    // Columns: the same on whole rows.
    let back = rows / 2;
    let stride = w * 4;
    let len = back.checked_add(h)?.checked_add(rows)?;
    let mut plane: Vec<u8> = zeroed(len.checked_mul(stride)?)?;
    plane.fill(identity);
    plane[back * stride..(back + h) * stride].copy_from_slice(data.as_flattened());
    fold::<MIN>(&mut plane, rows, stride);
    data.as_flattened_mut()
        .copy_from_slice(&plane[..h * stride]);
    Some(())
}

/// Replaces every item `i` (of `size` bytes) by the fold of the `window`
/// items starting at it, for all windows that end inside the buffer.
fn fold<const MIN: bool>(buf: &mut [u8], window: usize, size: usize) {
    let mut have = 1;
    while have < window {
        // `have` items are folded in; the next `have` (or what is missing)
        // are at that distance, still unread by this pass.
        let shift = have.min(window - have) * size;
        let n = buf.len().saturating_sub(shift);
        let mut at = 0;
        // Blocks no longer than the distance: source and target are apart.
        while at < n {
            let len = shift.min(n - at);
            let (a, b) = buf[at..].split_at_mut(shift);
            for (x, y) in a[..len].iter_mut().zip(&b[..len]) {
                *x = pick::<MIN>(*x, *y);
            }
            at += len;
        }
        have += have.min(window - have);
    }
}

// --------------------------------------------------------- convolution --

pub(crate) fn convolve(fe: &ConvolveMatrix, w: usize, data: &mut [Px]) -> Option<()> {
    if w == 0 || data.is_empty() {
        return Some(());
    }
    if fe.preserve_alpha {
        for p in data.iter_mut() {
            let a = f32::from(p[3]) / 255.0;
            *p = [
                demultiply(p[0], a),
                demultiply(p[1], a),
                demultiply(p[2], a),
                p[3],
            ];
        }
    }
    let mut out: Vec<Px> = zeroed(data.len())?;
    let h = data.len() / w;
    let (cols, rows) = (fe.columns as usize, fe.rows as usize);
    // The kernel is applied rotated by 180 degrees.
    let kernel: Vec<f32> = fe.kernel.iter().rev().copied().collect();
    let edge = |v: i64, max: usize| -> Option<usize> {
        let max = max as i64;
        let v = match fe.edge_mode {
            EdgeMode::None if !(0..max).contains(&v) => return None,
            EdgeMode::None => v,
            EdgeMode::Duplicate => v.clamp(0, max - 1),
            EdgeMode::Wrap => v.rem_euclid(max),
        };
        Some(v as usize)
    };
    // Column positions for every `x + ox`, so the edge rule is looked up.
    let xs: Vec<Option<usize>> = (0..w + cols)
        .map(|i| edge(i as i64 - i64::from(fe.target_x), w))
        .collect();
    let mut src_rows: Vec<Option<usize>> = vec![None; rows];
    for (y, out_row) in out.chunks_exact_mut(w).enumerate() {
        for (oy, r) in src_rows.iter_mut().enumerate() {
            *r = edge(y as i64 - i64::from(fe.target_y) + oy as i64, h);
        }
        for (x, out) in out_row.iter_mut().enumerate() {
            let mut sum = [0.0f32; 4];
            for (r, k) in src_rows.iter().zip(kernel.chunks_exact(cols)) {
                let Some(r) = r else {
                    continue;
                };
                let row = &data[r * w..(r + 1) * w];
                for (tx, k) in xs[x..x + cols].iter().zip(k) {
                    let Some(tx) = tx else {
                        continue;
                    };
                    for (s, c) in sum.iter_mut().zip(row[*tx]) {
                        *s += f32::from(c) / 255.0 * k;
                    }
                }
            }
            let alpha = if fe.preserve_alpha {
                f32::from(data[y * w + x][3]) / 255.0
            } else {
                sum[3] / fe.divisor + fe.bias
            };
            let bounded = bound(0.0, alpha, 1.0);
            let calc = |v: f32| {
                let v = v / fe.divisor + fe.bias * alpha;
                let v = if fe.preserve_alpha {
                    bound(0.0, v, 1.0) * bounded
                } else {
                    bound(0.0, v, bounded)
                };
                (v * 255.0 + 0.5) as u8
            };
            *out = [
                calc(sum[0]),
                calc(sum[1]),
                calc(sum[2]),
                (bounded * 255.0 + 0.5) as u8,
            ];
        }
    }
    data.copy_from_slice(&out);
    Some(())
}

// -------------------------------------------------------- displacement --

pub(crate) fn displacement(
    fe: &DisplacementMap,
    (sx, sy): (f32, f32),
    w: usize,
    src: &[Px],
    map: &[Px],
    dst: &mut [Px],
) {
    if w == 0 {
        return;
    }
    let h = src.len() / w;
    let channel = |c: ColorChannel| match c {
        ColorChannel::R => 0,
        ColorChannel::G => 1,
        ColorChannel::B => 2,
        ColorChannel::A => 3,
    };
    let (cx, cy) = (
        channel(fe.x_channel_selector),
        channel(fe.y_channel_selector),
    );
    for (i, (d, m)) in dst.iter_mut().zip(map).enumerate() {
        let (x, y) = (i % w, i / w);
        let value = |c: usize| {
            let c = match (c, m[3]) {
                (3, a) => a,
                // Transparent: unpremultiplied black.
                (_, 0) => 0,
                (c, a) => demultiply(m[c], f32::from(a) / 255.0),
            };
            f32::from(c) / 255.0 - 0.5
        };
        let (dx, dy) = (value(cx), value(cy));
        let ox = (x as f32 + dx * sx).round();
        let oy = (y as f32 + dy * sy).round();
        // Out of range, or NaN: left transparent.
        if ox >= 0.0 && oy >= 0.0 && ox < w as f32 && oy < h as f32 {
            if let Some(p) = src.get(oy as usize * w + ox as usize) {
                *d = *p;
            }
        }
    }
}

// ---------------------------------------------------------- turbulence --

/// Octaves past this change nothing visible (each adds half of the last),
/// and past 63 the lattice position would leave the integer range.
pub(crate) const MAX_OCTAVES: u32 = 48;

const LATTICE: usize = 256;
const LATTICE_LEN: usize = LATTICE + LATTICE + 2;
const PERLIN_N: i32 = 0x1000;

/// The tables of the noise function (the algorithm of the SVG
/// specification's `feTurbulence` reference code).
struct Noise {
    selector: Vec<usize>,
    /// The four channels' gradients at each lattice index.
    gradient: Vec<[[f64; 2]; 4]>,
}

fn random(seed: i32) -> i32 {
    const M: i32 = i32::MAX;
    const A: i32 = 16807;
    const Q: i32 = 127_773;
    const R: i32 = 2836;
    let mut v = A * (seed % Q) - R * (seed / Q);
    if v <= 0 {
        v += M;
    }
    v
}

impl Noise {
    fn new(seed: i32) -> Noise {
        let mut seed = if seed <= 0 {
            (-i64::from(seed) % i64::from(i32::MAX - 1) + 1) as i32
        } else {
            seed.min(i32::MAX - 1)
        };
        let mut selector = vec![0usize; LATTICE_LEN];
        let mut gradient = vec![[[0.0f64; 2]; 4]; LATTICE_LEN];
        for k in 0..4 {
            for (i, g) in gradient[..LATTICE].iter_mut().enumerate() {
                selector[i] = i;
                let g = &mut g[k];
                for v in g.iter_mut() {
                    seed = random(seed);
                    *v = f64::from((seed % 512) - 256) / 256.0;
                }
                let s = (g[0] * g[0] + g[1] * g[1]).sqrt();
                g[0] /= s;
                g[1] /= s;
            }
        }
        for i in (1..LATTICE).rev() {
            seed = random(seed);
            selector.swap(i, (seed % 256) as usize);
        }
        for i in 0..LATTICE + 2 {
            selector[LATTICE + i] = selector[i];
            gradient[LATTICE + i] = gradient[i];
        }
        Noise { selector, gradient }
    }
}

#[derive(Clone, Copy)]
struct Stitch {
    width: i32,
    height: i32,
    wrap_x: i32,
    wrap_y: i32,
}

/// The frequency adjusted so that the tile's borders are continuous.
fn stitch_frequency(freq: f64, size: f64) -> f64 {
    if freq.abs() <= f64::MIN_POSITIVE {
        return freq;
    }
    let lo = (size * freq).floor() / size;
    let hi = (size * freq).ceil() / size;
    if freq / lo < hi / freq {
        lo
    } else {
        hi
    }
}

/// Fills `dst` (`w` pixels wide) with noise, premultiplied.
pub(crate) fn turbulence(
    fe: &Turbulence,
    offset: (f64, f64),
    scale: (f64, f64),
    w: usize,
    dst: &mut [Px],
) {
    if w == 0 {
        return;
    }
    let noise = Noise::new(fe.seed);
    let h = dst.len() / w;
    let (mut fx, mut fy) = (
        f64::from(fe.base_frequency_x),
        f64::from(fe.base_frequency_y),
    );
    let octaves = fe.num_octaves.min(MAX_OCTAVES);
    let tile = fe.stitch_tiles.then(|| {
        fx = stitch_frequency(fx, w as f64);
        fy = stitch_frequency(fy, h as f64);
        ((w as f64 * fx + 0.5) as i32, (h as f64 * fy + 0.5) as i32)
    });
    for (y, row) in dst.chunks_exact_mut(w).enumerate() {
        for (x, p) in row.iter_mut().enumerate() {
            let pos = (
                (x as f64 + offset.0) / scale.0 * fx,
                (y as f64 + offset.1) / scale.1 * fy,
            );
            let stitch = tile.map(|(width, height)| Stitch {
                width,
                height,
                wrap_x: (x as f64 * fx + f64::from(PERLIN_N) + f64::from(width)) as i32,
                wrap_y: (y as f64 * fy + f64::from(PERLIN_N) + f64::from(height)) as i32,
            });
            let sum = noise.sum(pos, octaves, fe.fractal_noise, stitch);
            let c = sum.map(|n| {
                let n = if fe.fractal_noise {
                    (n * 255.0 + 255.0) / 2.0
                } else {
                    n * 255.0
                };
                (bound(0.0, n as f32, 255.0) + 0.5) as u8
            });
            let a = f32::from(c[3]) / 255.0;
            *p = [
                multiply(c[0], a),
                multiply(c[1], a),
                multiply(c[2], a),
                c[3],
            ];
        }
    }
}

impl Noise {
    /// The four channels at `(x, y)`, summed over the octaves.
    fn sum(
        &self,
        (mut x, mut y): (f64, f64),
        octaves: u32,
        fractal: bool,
        mut stitch: Option<Stitch>,
    ) -> [f64; 4] {
        let mut sum = [0.0f64; 4];
        let mut ratio = 1.0;
        for _ in 0..octaves {
            let n = self.noise(x, y, stitch);
            for (s, n) in sum.iter_mut().zip(n) {
                *s += if fractal { n } else { n.abs() } / ratio;
            }
            x *= 2.0;
            y *= 2.0;
            ratio *= 2.0;
            if let Some(s) = &mut stitch {
                s.width = s.width.wrapping_mul(2);
                s.wrap_x = s.wrap_x.wrapping_mul(2).wrapping_sub(PERLIN_N);
                s.height = s.height.wrapping_mul(2);
                s.wrap_y = s.wrap_y.wrapping_mul(2).wrapping_sub(PERLIN_N);
            }
        }
        sum
    }

    /// One octave of the four channels (which share the lattice cell).
    fn noise(&self, x: f64, y: f64, stitch: Option<Stitch>) -> [f64; 4] {
        let split = |v: f64| {
            let t = v + f64::from(PERLIN_N);
            let b0 = t as i32;
            (b0, b0.wrapping_add(1), t - t as i64 as f64)
        };
        let (mut bx0, mut bx1, rx0) = split(x);
        let (mut by0, mut by1, ry0) = split(y);
        let (rx1, ry1) = (rx0 - 1.0, ry0 - 1.0);
        if let Some(s) = stitch {
            for b in [&mut bx0, &mut bx1] {
                if *b >= s.wrap_x {
                    *b = b.wrapping_sub(s.width);
                }
            }
            for b in [&mut by0, &mut by1] {
                if *b >= s.wrap_y {
                    *b = b.wrapping_sub(s.height);
                }
            }
        }
        let at = |v: i32| (v & 0xff) as usize;
        let i = self.selector[at(bx0)];
        let j = self.selector[at(bx1)];
        let g = |base: usize, b: i32| &self.gradient[self.selector[base + at(b)]];
        let (g00, g10) = (g(i, by0), g(j, by0));
        let (g01, g11) = (g(i, by1), g(j, by1));
        let curve = |t: f64| t * t * (3.0 - 2.0 * t);
        let lerp = |t: f64, a: f64, b: f64| a + t * (b - a);
        let (sx, sy) = (curve(rx0), curve(ry0));
        std::array::from_fn(|c| {
            let u = rx0 * g00[c][0] + ry0 * g00[c][1];
            let v = rx1 * g10[c][0] + ry0 * g10[c][1];
            let a = lerp(sx, u, v);
            let u = rx0 * g01[c][0] + ry1 * g01[c][1];
            let v = rx1 * g11[c][0] + ry1 * g11[c][1];
            let b = lerp(sx, u, v);
            lerp(sy, a, b)
        })
    }
}

// ------------------------------------------------------------- lighting --

type Vec3 = [f32; 3];

fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn length(a: Vec3) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

fn minus(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// `a` with length 1, or as it is when it has none.
fn normalized(a: Vec3) -> Vec3 {
    let l = length(a);
    if crate::convert::approx_zero(l) {
        a
    } else {
        a.map(|v| v / l)
    }
}

pub(crate) fn diffuse_lighting(
    fe: &DiffuseLighting,
    light: LightSource,
    w: usize,
    src: &[Px],
    dst: &mut [Px],
) {
    let factor = |normal: Option<Vec3>, l: Vec3| {
        let k = match normal {
            None => l[2],
            Some(n) => dot(n, l) / length(n),
        };
        fe.diffuse_constant * k
    };
    lighting(
        light,
        fe.surface_scale,
        fe.lighting_color,
        factor,
        |_| 255,
        w,
        src,
        dst,
    );
}

pub(crate) fn specular_lighting(
    fe: &SpecularLighting,
    light: LightSource,
    w: usize,
    src: &[Px],
    dst: &mut [Px],
) {
    let linear = crate::convert::approx_zero(fe.specular_exponent - 1.0);
    let factor = |normal: Option<Vec3>, l: Vec3| {
        let h = [l[0] + 0.0, l[1] + 0.0, l[2] + 1.0];
        let hl = length(h);
        if crate::convert::approx_zero(hl) {
            return 0.0;
        }
        let k = match normal {
            None => h[2] / hl,
            Some(n) => dot(n, h) / length(n) / hl,
        };
        let k = if linear {
            k
        } else {
            k.powf(fe.specular_exponent)
        };
        fe.specular_constant * k
    };
    lighting(
        light,
        fe.surface_scale,
        fe.lighting_color,
        factor,
        |c| c[0].max(c[1]).max(c[2]),
        w,
        src,
        dst,
    );
}

/// Lights the surface whose height is `src`'s alpha. Nothing is drawn for an
/// image under 3x3. `factor` gets the surface normal, `None` for a level
/// spot.
#[allow(clippy::too_many_arguments)]
fn lighting(
    light: LightSource,
    surface_scale: f32,
    color: Color,
    factor: impl Fn(Option<Vec3>, Vec3) -> f32,
    alpha: impl Fn([u8; 3]) -> u8,
    w: usize,
    src: &[Px],
    dst: &mut [Px],
) {
    if w < 3 || src.len() / w < 3 || dst.len() < src.len() {
        return;
    }
    let h = src.len() / w;
    let rgb = [color.red, color.green, color.blue];
    let scaled = |c: u8, f: f32| (bound(0.0, f32::from(c) * f, 255.0) + 0.5) as u8;
    let fixed = match light {
        LightSource::Distant { azimuth, elevation } => {
            let (a, e) = (azimuth.to_radians(), elevation.to_radians());
            [a.cos() * e.cos(), a.sin() * e.cos(), e.sin()]
        }
        _ => [1.0; 3],
    };
    // A spot light's direction and cone.
    let spot = match light {
        LightSource::Spot(s) => Some((
            s,
            normalized(minus(
                [s.points_at_x, s.points_at_y, s.points_at_z],
                [s.x, s.y, s.z],
            )),
            s.limiting_cone_angle.map(|a| a.to_radians().cos()),
        )),
        _ => None,
    };
    let k = surface_scale / 255.0;
    let at = |x: usize, y: usize| i32::from(src[y * w + x][3]);
    for y in 0..h {
        // The rows the normal is taken over, each with its weight; at the
        // edges the missing ones are left out and the factors make up.
        let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(1), (x + 1).min(w - 1));
            // Sobel differences, weight 2 on the pixel's own row / column.
            let mut nx = 2 * (at(x1, y) - at(x0, y));
            let mut ny = 2 * (at(x, y1) - at(x, y0));
            let (mut wy, mut wx) = (2.0f32, 2.0f32);
            for yy in [y0, y1] {
                if yy != y {
                    nx += at(x1, yy) - at(x0, yy);
                    wy += 1.0;
                }
            }
            for xx in [x0, x1] {
                if xx != x {
                    ny += at(xx, y1) - at(xx, y0);
                    wx += 1.0;
                }
            }
            let normal = (nx != 0 || ny != 0).then(|| {
                let fx = 2.0 / (wy * (x1 - x0) as f32);
                let fy = 2.0 / (wx * (y1 - y0) as f32);
                [-nx as f32 * k * fx, -ny as f32 * k * fy, 1.0]
            });
            let toward = |px: f32, py: f32, pz: f32| {
                let z = f32::from(src[y * w + x][3]) / 255.0 * surface_scale;
                normalized(minus([px, py, pz], [x as f32, y as f32, z]))
            };
            let (l, c) = match (light, &spot) {
                (LightSource::Point { x, y, z }, _) => (toward(x, y, z), rgb),
                (_, Some((s, direction, cone))) => {
                    let l = toward(s.x, s.y, s.z);
                    let d = -dot(l, *direction);
                    let c = if d <= 0.0 || cone.is_some_and(|c| d < c) {
                        [0; 3]
                    } else {
                        let f = d.powf(s.specular_exponent);
                        rgb.map(|c| scaled(c, f))
                    };
                    (l, c)
                }
                _ => (fixed, rgb),
            };
            let f = factor(normal, l);
            let c = c.map(|c| scaled(c, f));
            dst[y * w + x] = [c[0], c[1], c[2], alpha(c)];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn image(&mut self, w: usize, h: usize) -> Vec<Px> {
            (0..w * h)
                .map(|_| {
                    let a = match self.next() % 4 {
                        0 => 0,
                        1 => 255,
                        _ => self.next() % 256,
                    };
                    let c = [0; 3].map(|_: u8| (self.next() % 256 * a / 255) as u8);
                    [c[0], c[1], c[2], a as u8]
                })
                .collect()
        }
    }

    /// The straightforward form of `map_straight`, with no special cases.
    fn map_plain(data: &mut [Px], f: impl Fn(Px) -> Px) {
        for p in data {
            let a = f32::from(p[3]) / 255.0;
            let o = f([
                demultiply(p[0], a),
                demultiply(p[1], a),
                demultiply(p[2], a),
                p[3],
            ]);
            let a = f32::from(o[3]) / 255.0;
            *p = [
                multiply(o[0], a),
                multiply(o[1], a),
                multiply(o[2], a),
                o[3],
            ];
        }
    }

    /// Every (colour, alpha) pair, valid or not.
    #[test]
    fn colour_spaces_match_the_plain_form() {
        let all: Vec<Px> = (0..=255u8)
            .flat_map(|a| (0..=255u8).map(move |c| [c, c / 2, 255 - c, a]))
            .collect();
        for (space, t) in [
            (ColorInterpolation::SRgb, &TO_SRGB),
            (ColorInterpolation::LinearRgb, &TO_LINEAR),
        ] {
            let (mut a, mut b) = (all.clone(), all.clone());
            convert_space(&mut a, space);
            map_plain(&mut b, |p| {
                [
                    t[usize::from(p[0])],
                    t[usize::from(p[1])],
                    t[usize::from(p[2])],
                    p[3],
                ]
            });
            assert!(a == b);
        }
    }

    /// Box blurs against summing every window afresh.
    #[test]
    fn box_blur_matches_window_sums() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for (w, h) in [(1, 1), (1, 9), (9, 1), (5, 7), (33, 18), (64, 3)] {
            for r in [1usize, 2, 3, 7, 20, 100] {
                let src = rng.image(w, h);
                let scale = 1.0 / (r + r + 1) as f32;
                let naive = |at: &dyn Fn(i64) -> Option<Px>, c: i64| -> Px {
                    let mut sum = [0i64; 4];
                    for i in c - r as i64..=c + r as i64 {
                        if let Some(p) = at(i) {
                            for k in 0..4 {
                                sum[k] += i64::from(p[k]);
                            }
                        }
                    }
                    // The rounding resvg uses.
                    sum.map(|s| (s as f32 * scale + 12_582_912.0 - 12_582_912.0) as u8)
                };
                let (mut vert, mut sums) = (vec![[0; 4]; w * h], vec![0; w * 4]);
                box_vertical(r, w, &src, &mut vert, &mut sums);
                let mut horz = vec![[0; 4]; w * h];
                for (s, d) in src.chunks_exact(w).zip(horz.chunks_exact_mut(w)) {
                    box_row(r, s, d);
                }
                for y in 0..h {
                    for x in 0..w {
                        let col =
                            |i: i64| (0..h as i64).contains(&i).then(|| src[i as usize * w + x]);
                        let row =
                            |i: i64| (0..w as i64).contains(&i).then(|| src[y * w + i as usize]);
                        assert_eq!(vert[y * w + x], naive(&col, y as i64), "v {w}x{h} r{r}");
                        assert_eq!(horz[y * w + x], naive(&row, x as i64), "h {w}x{h} r{r}");
                    }
                }
            }
        }
    }

    /// The recursive blur against the one-value-at-a-time recurrence.
    #[test]
    fn iir_blur_matches_the_plain_recurrence() {
        let mut rng = Rng(7);
        for (w, h) in [(1, 1), (1, 6), (7, 1), (9, 11), (30, 8)] {
            for (sx, sy) in [(1.0, 1.5), (0.0, 0.7), (1.9, 0.0)] {
                let src = rng.image(w, h);
                let mut fast = src.clone();
                iir_blur(sx, sy, w, &mut fast).unwrap();
                let mut scale = (1.0f64, 1.0f64);
                let mut out = src.clone();
                for c in 0..4 {
                    let mut buf: Vec<f64> = src.iter().map(|p| f64::from(p[c]) / 255.0).collect();
                    for (sigma, vertical) in [(sx, false), (sy, true)] {
                        if sigma == 0.0 {
                            continue;
                        }
                        let (lambda, nu) = iir_coefficients(sigma);
                        if c == 0 {
                            scale = (scale.0 * nu, scale.1 * lambda);
                        }
                        let (lines, len) = if vertical { (w, h) } else { (h, w) };
                        for line in 0..lines {
                            let at = |i: usize| {
                                if vertical {
                                    i * w + line
                                } else {
                                    line * w + i
                                }
                            };
                            for _ in 0..IIR_STEPS {
                                for i in 1..len {
                                    buf[at(i)] += nu * buf[at(i - 1)];
                                }
                                for i in (1..len).rev() {
                                    buf[at(i - 1)] += nu * buf[at(i)];
                                }
                            }
                        }
                    }
                    let eight = std::hint::black_box(8);
                    let k = (scale.0.sqrt() / scale.1.sqrt()).powi(eight);
                    for (o, b) in out.iter_mut().zip(&buf) {
                        o[c] = (b * k * 255.0) as u8;
                    }
                }
                assert_eq!(fast, out, "{w}x{h} {sx} {sy}");
            }
        }
    }

    /// Morphology against taking every window afresh.
    #[test]
    fn morphology_matches_windows() {
        let mut rng = Rng(99);
        for (w, h) in [(1, 1), (1, 8), (8, 1), (13, 9), (40, 25)] {
            for (rx, ry) in [(0.4, 1.0), (1.0, 1.0), (2.5, 3.0), (7.0, 0.1), (1e9, 4.0)] {
                for erode in [true, false] {
                    let src = rng.image(w, h);
                    let mut fast = src.clone();
                    morphology(erode, rx, ry, w, &mut fast).unwrap();
                    let cols = ((f32::ceil(rx) as usize).saturating_mul(2)).min(w);
                    let rows = ((f32::ceil(ry) as usize).saturating_mul(2)).min(h);
                    for y in 0..h {
                        for x in 0..w {
                            let mut want = [if erode { 255u8 } else { 0 }; 4];
                            for oy in 0..rows {
                                for ox in 0..cols {
                                    let tx = (x + ox).wrapping_sub(cols / 2);
                                    let ty = (y + oy).wrapping_sub(rows / 2);
                                    if tx < w && ty < h {
                                        let p = src[ty * w + tx];
                                        for (v, c) in want.iter_mut().zip(p) {
                                            *v = if erode { c.min(*v) } else { c.max(*v) };
                                        }
                                    }
                                }
                            }
                            assert_eq!(fast[y * w + x], want, "{w}x{h} {rx},{ry} {erode}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn displacement_moves_by_scale() {
        // Opaque: red moves in x, alpha in y; half transparent: red is read
        // unpremultiplied.
        let fe = DisplacementMap {
            input1: Input::SourceGraphic,
            input2: Input::SourceGraphic,
            scale: 4.0,
            x_channel_selector: ColorChannel::R,
            y_channel_selector: ColorChannel::G,
        };
        let w = 8;
        let src: Vec<Px> = (0..64).map(|i| [i as u8, 0, 0, 255]).collect();
        let mut map = vec![[128, 128, 0, 255]; 64];
        map[27] = [255, 128, 0, 255]; // x + 2
        map[28] = [128, 0, 0, 255]; // y - 2
        map[35] = [128, 64, 0, 128]; // x + 2: red is 255 of 128
        map[0] = [0, 0, 0, 255]; // outside: transparent
        let mut dst = vec![[0; 4]; 64];
        displacement(&fe, (4.0, 4.0), w, &src, &map, &mut dst);
        assert_eq!(dst[27][0], 29);
        assert_eq!(dst[28][0], 12);
        assert_eq!(dst[35][0], 37);
        assert_eq!(dst[0], [0; 4]);
        assert_eq!(dst[20][0], 20);
    }

    /// Kernels on images of no, one and a few pixels, with every parameter
    /// at its extremes.
    #[test]
    fn extremes_do_not_panic() {
        for (w, h) in [(1usize, 1usize), (2, 1), (1, 3), (3, 3)] {
            for s in [0.0f32, 1e-30, 1.0, 1e9, 1e30, f32::INFINITY, f32::NAN] {
                let mut d = vec![[9u8; 4]; w * h];
                box_blur(s, s, w, &mut d).unwrap();
                iir_blur(f64::from(s), f64::from(s), w, &mut d).unwrap();
                morphology(true, s, s, w, &mut d).unwrap();
            }
            for seed in [i32::MIN, -1, 0, 1, i32::MAX] {
                let fe = Turbulence {
                    base_frequency_x: 1e30,
                    base_frequency_y: 0.1,
                    num_octaves: u32::MAX,
                    seed,
                    stitch_tiles: seed & 1 == 0,
                    fractal_noise: seed > 0,
                };
                let mut d = vec![[0u8; 4]; w * h];
                turbulence(&fe, (1e30, -1e30), (1e-30, 1.0), w, &mut d);
            }
        }
    }
}
