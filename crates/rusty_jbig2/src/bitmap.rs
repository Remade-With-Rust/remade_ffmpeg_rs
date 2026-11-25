use crate::{Error, Result};

/// A 1-bit image: packed rows, most significant bit first, each row padded
/// with zero bits to a whole byte.
///
/// **Polarity: 1 is black**, as JBIG2 (T.88) defines it. PDF's `JBIG2Decode`
/// filter output is the opposite (0 is black); [`Bitmap1::into_pdf_samples`]
/// gives that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap1 {
    /// Pixels per row.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// Bytes per row: `width.div_ceil(8)`.
    pub stride: usize,
    /// `stride * height` bytes.
    pub data: Vec<u8>,
}

impl Bitmap1 {
    /// Whether the pixel is black (1). Coordinates outside are white.
    #[must_use]
    pub fn is_black(&self, x: u32, y: u32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let i = y as usize * self.stride + (x >> 3) as usize;
        self.data
            .get(i)
            .is_some_and(|b| (b >> (7 - (x & 7))) & 1 == 1)
    }

    /// The samples PDF's `JBIG2Decode` filter produces: same packing, black
    /// is 0 and white is 1 (`DeviceGray` with the default `Decode`). Padding
    /// bits stay zero.
    #[must_use]
    pub fn into_pdf_samples(mut self) -> Vec<u8> {
        let mask = tail_mask(self.width);
        for row in self.data.chunks_exact_mut(self.stride.max(1)) {
            for b in row.iter_mut() {
                *b = !*b;
            }
            if let Some(l) = row.last_mut() {
                *l &= mask;
            }
        }
        self.data
    }
}

/// Valid bits of the last byte of a row of `width` pixels.
pub(crate) fn tail_mask(width: u32) -> u8 {
    if width % 8 == 0 {
        0xFF
    } else {
        0xFFu8 << (8 - width % 8)
    }
}

/// How a bitmap is combined with what it is drawn onto (7.4.1.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Or,
    And,
    Xor,
    Xnor,
    Replace,
}

impl Op {
    pub(crate) fn from_bits(v: u32) -> Result<Self> {
        Ok(match v {
            0 => Op::Or,
            1 => Op::And,
            2 => Op::Xor,
            3 => Op::Xnor,
            4 => Op::Replace,
            _ => return Err(Error::Invalid("reserved combination operator")),
        })
    }
}

/// The decoder's working bitmap. Invariant: padding bits are zero.
#[derive(Clone, Debug)]
pub(crate) struct Bitmap {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) stride: usize,
    pub(crate) data: Vec<u8>,
}

/// 32 bits of `row` starting at bit `pos`; bits outside the row are 0.
#[inline]
pub(crate) fn fetch32(row: &[u8], pos: i64) -> u32 {
    let byte = pos >> 3;
    let sh = (pos & 7) as u32;
    let chunk = usize::try_from(byte)
        .ok()
        .and_then(|b| row.get(b..)?.first_chunk::<5>());
    let w = if let Some(s) = chunk {
        (u64::from(u32::from_be_bytes([s[0], s[1], s[2], s[3]])) << 8) | u64::from(s[4])
    } else {
        let mut w = 0u64;
        for i in 0..5 {
            let b = usize::try_from(byte + i).ok().and_then(|b| row.get(b));
            w = (w << 8) | u64::from(b.copied().unwrap_or(0));
        }
        w
    };
    (w >> (8 - sh)) as u32
}

impl Bitmap {
    /// The caller has charged the budget for this size.
    pub(crate) fn new(w: u32, h: u32, fill: bool) -> Self {
        let stride = w.div_ceil(8) as usize;
        let mut b = Self {
            w,
            h,
            stride,
            data: vec![if fill { 0xFF } else { 0 }; stride * h as usize],
        };
        if fill {
            b.clear_padding(0);
        }
        b
    }

    /// Zero the padding bits of every row stored from byte `from` on.
    fn clear_padding(&mut self, from: usize) {
        let m = tail_mask(self.w);
        if m != 0xFF && self.stride > 0 {
            for r in self.data[from..].chunks_exact_mut(self.stride) {
                r[self.stride - 1] &= m;
            }
        }
    }

    /// `data` must hold `w.div_ceil(8) * h` bytes.
    pub(crate) fn from_packed(w: u32, h: u32, data: Vec<u8>) -> Self {
        let stride = w.div_ceil(8) as usize;
        debug_assert_eq!(data.len(), stride * h as usize);
        let mut b = Self { w, h, stride, data };
        b.clear_padding(0);
        b
    }

    /// Row `y`, empty when outside.
    #[inline]
    pub(crate) fn row(&self, y: i64) -> &[u8] {
        if y < 0 || y >= i64::from(self.h) {
            return &[];
        }
        let s = y as usize * self.stride;
        &self.data[s..s + self.stride]
    }

    #[inline]
    pub(crate) fn get(&self, x: u32, y: u32) -> u32 {
        self.row(i64::from(y))
            .get((x >> 3) as usize)
            .map_or(0, |b| u32::from(b >> (7 - (x & 7))) & 1)
    }

    /// Copy of the `w` x `h` area at (`x`, `y`); outside pixels are 0. The
    /// caller has charged the budget.
    pub(crate) fn extract(&self, x: i64, y: i64, w: u32, h: u32) -> Bitmap {
        let mut out = Bitmap::new(w, h, false);
        out.draw(self, -x, -y, Op::Or);
        out
    }

    /// Grow or crop to `h` rows, new rows filled with `fill`.
    pub(crate) fn set_height(&mut self, h: u32, fill: bool) {
        let old = self.data.len();
        let new = self.stride * h as usize;
        self.data.resize(new, if fill { 0xFF } else { 0 });
        self.h = h;
        if fill && new > old {
            self.clear_padding(old);
        }
    }

    /// Combine `src` into `self` with its top left corner at (`x`, `y`);
    /// parts that fall outside are ignored. `|x|` and `|y|` are below 2^48.
    pub(crate) fn draw(&mut self, src: &Bitmap, x: i64, y: i64, op: Op) {
        let x0 = x.max(0);
        let x1 = (x + i64::from(src.w)).min(i64::from(self.w));
        let y0 = y.max(0);
        let y1 = (y + i64::from(src.h)).min(i64::from(self.h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (b0, b1) = ((x0 >> 3) as usize, ((x1 + 7) >> 3) as usize);
        let head = 0xFFu8 >> (x0 & 7);
        let tail = if x1 & 7 == 0 {
            0xFF
        } else {
            0xFFu8 << (8 - (x1 & 7))
        };
        for yy in y0..y1 {
            let s = src.row(yy - y);
            let start = yy as usize * self.stride;
            let d = &mut self.data[start + b0..start + b1];
            let p = b0 as i64 * 8 - x;
            match op {
                Op::Or => blend(d, s, p, head, tail, |d, s, m| d | (s & m)),
                Op::And => blend(d, s, p, head, tail, |d, s, m| d & (s | !m)),
                Op::Xor => blend(d, s, p, head, tail, |d, s, m| d ^ (s & m)),
                Op::Xnor => blend(d, s, p, head, tail, |d, s, m| (d & !m) | (!(d ^ s) & m)),
                Op::Replace => blend(d, s, p, head, tail, |d, s, m| (d & !m) | (s & m)),
            }
        }
    }
}

/// One row: byte `i` of `d` is combined with the 8 bits of `s` at `p + 8i`.
#[inline]
fn blend(d: &mut [u8], s: &[u8], p: i64, head: u8, tail: u8, f: impl Fn(u8, u8, u8) -> u8) {
    let n = d.len();
    if p >= 0 && p & 7 == 0 && n > 2 {
        // Byte-aligned source: no shifting between the end bytes.
        let s0 = (p >> 3) as usize;
        if let Some(sm) = s.get(s0..s0 + n) {
            d[0] = f(d[0], sm[0], head);
            for (d, s) in d[1..n - 1].iter_mut().zip(&sm[1..n - 1]) {
                *d = f(*d, *s, 0xFF);
            }
            d[n - 1] = f(d[n - 1], sm[n - 1], tail);
            return;
        }
    }
    let sh = (p & 7) as u32;
    let mut idx = p >> 3;
    let at = |k: i64| -> u32 {
        usize::try_from(k)
            .ok()
            .and_then(|k| s.get(k))
            .map_or(0, |&v| u32::from(v))
    };
    let mut hi = at(idx);
    for (i, b) in d.iter_mut().enumerate() {
        let mut m = 0xFF;
        if i == 0 {
            m &= head;
        }
        if i + 1 == n {
            m &= tail;
        }
        idx += 1;
        let lo = at(idx);
        *b = f(*b, (((hi << 8) | lo) >> (8 - sh)) as u8, m);
        hi = lo;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(b: &Bitmap, x: i64, y: i64) -> u32 {
        if x < 0 || y < 0 || x >= i64::from(b.w) || y >= i64::from(b.h) {
            0
        } else {
            b.get(x as u32, y as u32)
        }
    }

    fn noise(w: u32, h: u32, mut seed: u32) -> Bitmap {
        let mut b = Bitmap::new(w, h, false);
        for v in &mut b.data {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = (seed >> 24) as u8;
        }
        b.clear_padding(0);
        b
    }

    #[test]
    fn draw_matches_per_pixel_definition() {
        let base = noise(37, 8, 99);
        for op in [Op::Or, Op::And, Op::Xor, Op::Xnor, Op::Replace] {
            for (sw, sh) in [(1, 1), (7, 3), (8, 4), (9, 2), (31, 5), (64, 3)] {
                let src = noise(sw, sh, 7);
                for x in -70..50 {
                    for y in [-4i64, -1, 0, 2, 9] {
                        let mut d = base.clone();
                        d.draw(&src, x, y, op);
                        for py in 0..8i64 {
                            for pxx in 0..37i64 {
                                let (sx, sy) = (pxx - x, py - y);
                                let b = px(&base, pxx, py);
                                let inside = (0..i64::from(sw)).contains(&sx)
                                    && (0..i64::from(sh)).contains(&sy);
                                let s = px(&src, sx, sy);
                                let want = if inside {
                                    match op {
                                        Op::Or => b | s,
                                        Op::And => b & s,
                                        Op::Xor => b ^ s,
                                        Op::Xnor => 1 ^ b ^ s,
                                        Op::Replace => s,
                                    }
                                } else {
                                    b
                                };
                                assert_eq!(px(&d, pxx, py), want, "{op:?} {sw}x{sh} {x},{y}");
                            }
                        }
                        assert!(d.data.chunks(d.stride).all(|r| r[4] & 0x07 == 0));
                    }
                }
            }
        }
    }

    #[test]
    fn fetch32_matches_bits() {
        let b = noise(77, 1, 3);
        for pos in -50..100i64 {
            let v = fetch32(b.row(0), pos);
            for i in 0..32 {
                assert_eq!((v >> (31 - i)) & 1, px(&b, pos + i, 0));
            }
        }
    }

    #[test]
    fn pdf_samples_invert_and_keep_padding_zero() {
        let b = Bitmap1 {
            width: 10,
            height: 2,
            stride: 2,
            data: vec![0xFF, 0xC0, 0x00, 0x40],
        };
        assert!(b.is_black(9, 1) && !b.is_black(8, 1) && !b.is_black(10, 0));
        assert_eq!(b.into_pdf_samples(), [0x00, 0x00, 0xFF, 0x80]);
    }
}
