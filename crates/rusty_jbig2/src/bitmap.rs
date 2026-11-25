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
        match op {
            Op::Or => self.rows::<Or>(src, (x, y), (x0, x1), (y0, y1)),
            Op::And => self.rows::<And>(src, (x, y), (x0, x1), (y0, y1)),
            Op::Xor => self.rows::<Xor>(src, (x, y), (x0, x1), (y0, y1)),
            Op::Xnor => self.rows::<Xnor>(src, (x, y), (x0, x1), (y0, y1)),
            Op::Replace => self.rows::<Replace>(src, (x, y), (x0, x1), (y0, y1)),
        }
    }

    /// [`Bitmap::draw`] clipped to columns `x0..x1` and rows `y0..y1`, not
    /// empty.
    fn rows<C: Combine>(
        &mut self,
        src: &Bitmap,
        (x, y): (i64, i64),
        (x0, x1): (i64, i64),
        (y0, y1): (i64, i64),
    ) {
        // Full rows: one run of bytes.
        if x == 0 && src.w == self.w {
            let (y0, y1) = (y0 as usize, y1 as usize);
            let from = (y0 as i64 - y) as usize * src.stride;
            let d = &mut self.data[y0 * self.stride..y1 * self.stride];
            for (d, s) in d.iter_mut().zip(&src.data[from..]) {
                *d = C::f(*d, *s, 0xFF);
            }
            if C::SETS_PADDING {
                self.clear_padding(y0 * self.stride);
            }
            return;
        }
        let (b0, b1) = ((x0 >> 3) as usize, ((x1 + 7) >> 3) as usize);
        let d0 = y0 as usize * self.stride + b0;
        let lead = (x0 & 7) as u32;
        let (sx, bits) = (x0 - x, (x1 - x0) as u64);
        let skip = (sx & 7) as u32;
        // Symbols and patterns: each row within one 64-bit word of both.
        if bits <= u64::from(64 - lead.max(skip)) && self.data.len() >= 8 {
            let s0 = (y0 - y) as usize * src.stride + (sx >> 3) as usize;
            let w = Narrow {
                rows: (y1 - y0) as usize,
                skip,
                lead,
                mask: (!0u64 << (64 - bits)) >> lead,
            };
            w.run::<C>(
                (&mut self.data, self.stride, d0),
                (&src.data, src.stride, s0),
            );
            return;
        }
        let tail = if x1 & 7 == 0 {
            0xFF
        } else {
            0xFFu8 << (8 - (x1 & 7))
        };
        let w = Wide {
            first: b0 as i64 - (x >> 3) - i64::from(x & 7 != 0),
            sh: (x.wrapping_neg() & 7) as u32,
            head: 0xFF >> lead,
            tail,
        };
        for yy in y0..y1 {
            let start = yy as usize * self.stride;
            w.row::<C>(&mut self.data[start + b0..start + b1], src.row(yy - y));
        }
    }
}

/// The word a combination operator works on.
trait Word:
    Copy
    + std::ops::BitAnd<Output = Self>
    + std::ops::BitOr<Output = Self>
    + std::ops::BitXor<Output = Self>
    + std::ops::Not<Output = Self>
{
}
impl Word for u8 {}
impl Word for u64 {}

/// A combination operator: `s` onto `d` where `m` is set.
trait Combine {
    /// Whether clear padding bits combine to set ones.
    const SETS_PADDING: bool = false;
    fn f<T: Word>(d: T, s: T, m: T) -> T;
}

struct Or;
struct And;
struct Xor;
struct Xnor;
struct Replace;

impl Combine for Or {
    #[inline]
    fn f<T: Word>(d: T, s: T, m: T) -> T {
        d | (s & m)
    }
}
impl Combine for And {
    #[inline]
    fn f<T: Word>(d: T, s: T, m: T) -> T {
        d & (s | !m)
    }
}
impl Combine for Xor {
    #[inline]
    fn f<T: Word>(d: T, s: T, m: T) -> T {
        d ^ (s & m)
    }
}
impl Combine for Xnor {
    const SETS_PADDING: bool = true;
    #[inline]
    fn f<T: Word>(d: T, s: T, m: T) -> T {
        d ^ (!s & m)
    }
}
impl Combine for Replace {
    #[inline]
    fn f<T: Word>(d: T, s: T, m: T) -> T {
        (d & !m) | (s & m)
    }
}

/// A draw whose rows each lie within one 64-bit word of the source and of the
/// destination. Words are big-endian, so the leftmost pixel is the top bit.
struct Narrow {
    rows: usize,
    /// Bit of the source's first byte the rows start at.
    skip: u32,
    /// Bit of the destination's first byte they start at.
    lead: u32,
    /// The destination bits drawn to.
    mask: u64,
}

impl Narrow {
    /// Rows start at byte `d0` of `dst` (at least 8 bytes) and `s0` of `src`
    /// and follow in steps of their strides.
    fn run<C: Combine>(
        &self,
        (dst, dstride, mut d0): (&mut [u8], usize, usize),
        (src, sstride, mut s0): (&[u8], usize, usize),
    ) {
        let mut pad = [0u8; 72];
        let src = if src.len() <= 64 {
            // Every row of a small source gets a whole word to load.
            pad[..src.len()].copy_from_slice(src);
            &pad
        } else {
            src
        };
        let (skip, lead) = (self.skip, self.lead);
        // The operators are bitwise, so only the source needs swapping.
        let m = self.mask.to_be();
        let mut left = self.rows;
        while left > 0 {
            let d = dst.get_mut(d0..).and_then(<[u8]>::first_chunk_mut::<8>);
            let s = src.get(s0..).and_then(<[u8]>::first_chunk::<8>);
            let (Some(d), Some(s)) = (d, s) else { break };
            let v = ((u64::from_be_bytes(*s) << skip) >> lead).to_be();
            *d = C::f(u64::from_ne_bytes(*d), v, m).to_ne_bytes();
            d0 += dstride;
            s0 += sstride;
            left -= 1;
        }
        // Rows with less than a word left in one of the buffers: the last
        // word there, shifted.
        let (dend, send) = (dst.len() - 8, src.len() - 8);
        while left > 0 {
            let (dat, sat) = (d0.min(dend), s0.min(send));
            let d = dst.get_mut(dat..).and_then(<[u8]>::first_chunk_mut::<8>);
            let s = src.get(sat..).and_then(<[u8]>::first_chunk::<8>);
            let (Some(d), Some(s)) = (d, s) else { break };
            let k = (d0 - dat) * 8;
            let v = (u64::from_be_bytes(*s) << ((s0 - sat) * 8) << skip) >> lead;
            *d = C::f(u64::from_be_bytes(*d), v >> k, self.mask >> k).to_be_bytes();
            d0 += dstride;
            s0 += sstride;
            left -= 1;
        }
    }
}

/// Any other draw, in bytes: the source's shifted ones between two masked
/// ones of the destination.
struct Wide {
    /// The source byte the first destination byte begins in, -1 if that is
    /// before the row, and the bit it begins at.
    first: i64,
    sh: u32,
    head: u8,
    tail: u8,
}

impl Wide {
    fn row<C: Combine>(&self, d: &mut [u8], s: &[u8]) {
        let sh = self.sh;
        let at = |k: i64| -> u32 {
            usize::try_from(k)
                .ok()
                .and_then(|k| s.get(k))
                .map_or(0, |&v| u32::from(v))
        };
        let end = |i: usize| -> u8 {
            let k = self.first + i as i64;
            (((at(k) << 8) | at(k + 1)) >> (8 - sh)) as u8
        };
        let n = d.len();
        if n == 1 {
            d[0] = C::f(d[0], end(0), self.head & self.tail);
            return;
        }
        d[0] = C::f(d[0], end(0), self.head);
        d[n - 1] = C::f(d[n - 1], end(n - 1), self.tail);
        // Source bits under whole destination bytes are all in the row.
        let mid = &mut d[1..n - 1];
        let from = (self.first + 1) as usize;
        let Some(hi) = s.get(from..from + mid.len()) else {
            return;
        };
        if sh == 0 {
            for (d, a) in mid.iter_mut().zip(hi) {
                *d = C::f(*d, *a, 0xFF);
            }
        } else if let Some(lo) = s.get(from + 1..from + 1 + mid.len()) {
            for ((d, a), b) in mid.iter_mut().zip(hi).zip(lo) {
                *d = C::f(*d, (a << sh) | (b >> (8 - sh)), 0xFF);
            }
        }
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

    /// The definition, one pixel at a time.
    fn draw_oracle(base: &Bitmap, src: &Bitmap, x: i64, y: i64, op: Op) -> Bitmap {
        let mut out = Bitmap::new(base.w, base.h, false);
        for py in 0..i64::from(base.h) {
            for pxx in 0..i64::from(base.w) {
                let (sx, sy) = (pxx - x, py - y);
                let b = px(base, pxx, py);
                let inside =
                    (0..i64::from(src.w)).contains(&sx) && (0..i64::from(src.h)).contains(&sy);
                let s = px(src, sx, sy);
                let v = if inside {
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
                if v == 1 {
                    out.data[py as usize * out.stride + (pxx >> 3) as usize] |= 0x80 >> (pxx & 7);
                }
            }
        }
        out
    }

    const OPS: [Op; 5] = [Op::Or, Op::And, Op::Xor, Op::Xnor, Op::Replace];

    /// Both kernels (one word, and bytes) against the definition: every shift
    /// and clip of sources from a pixel to beyond two words, onto destinations
    /// down to less than a word in all.
    #[test]
    fn draw_matches_per_pixel_definition() {
        let sizes = [
            (1u32, 1u32),
            (7, 3),
            (8, 4),
            (9, 2),
            (31, 5),
            (37, 3),
            (56, 2),
            (57, 3),
            (58, 2),
            (63, 2),
            (64, 3),
            (65, 2),
            (71, 2),
            (72, 2),
            (130, 2),
        ];
        let bases = [
            (37u32, 8u32),
            (5, 2),
            (8, 7),
            (8, 8),
            (64, 1),
            (71, 3),
            (150, 3),
            (300, 2),
        ];
        for (dw, dh) in bases {
            let base = noise(dw, dh, 99);
            for (sw, sh) in sizes {
                let src = noise(sw, sh, 7);
                for x in -i64::from(sw) - 2..i64::from(dw) + 3 {
                    for y in [-4i64, -1, 0, 1, i64::from(dh) - 1, 9] {
                        for op in OPS {
                            let mut d = base.clone();
                            d.draw(&src, x, y, op);
                            let want = draw_oracle(&base, &src, x, y, op);
                            assert_eq!(d.data, want.data, "{op:?} {dw}x{dh} {sw}x{sh} {x},{y}");
                        }
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
