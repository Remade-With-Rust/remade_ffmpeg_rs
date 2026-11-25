//! Generic region decoding (6.2), arithmetic variant.
//!
//! Contexts are gathered in reading order with the AT pixels at their nominal
//! places, so Figures 8 to 11 (the SLTP contexts) read off directly.

use crate::arith::Arith;
use crate::bitmap::{fetch32, Bitmap};
use crate::huffman::Bits;
use crate::{Error, Result};

/// Template, typical prediction and adaptive pixels of a generic region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Template {
    /// GBTEMPLATE, 0 to 3.
    pub(crate) id: u8,
    /// The 12-AT-pixel form of template 0 (EXTTEMPLATE).
    pub(crate) ext: bool,
    /// Adaptive pixel offsets, 4 (template 0), 12 (extended) or 1.
    pub(crate) at: Vec<(i16, i8)>,
}

const NOMINAL0: [(i16, i8); 4] = [(3, -1), (-3, -1), (2, -2), (-2, -2)];
const NOMINAL_EXT: [(i16, i8); 12] = [
    (-2, 0),
    (0, -2),
    (-2, -1),
    (-1, -2),
    (1, -2),
    (2, -1),
    (-3, 0),
    (-4, 0),
    (2, -2),
    (3, -1),
    (-2, -2),
    (-3, -1),
];

/// Context bits of the adaptive pixels.
const BITS0: [u32; 4] = [4, 10, 11, 15];
const BITS_EXT: [u32; 12] = [1, 13, 9, 14, 12, 5, 2, 3, 11, 4, 15, 10];

impl Template {
    fn nominal(id: u8, ext: bool) -> &'static [(i16, i8)] {
        match (id, ext) {
            (0, true) => &NOMINAL_EXT,
            (0, false) => &NOMINAL0,
            (1, _) => &[(3, -1)],
            _ => &[(2, -1)],
        }
    }

    fn bits(&self) -> &'static [u32] {
        match (self.id, self.ext) {
            (0, true) => &BITS_EXT,
            (0, false) => &BITS0,
            (1, _) => &[3],
            (2, _) => &[2],
            _ => &[4],
        }
    }

    /// Number of contexts.
    pub(crate) fn contexts(&self) -> usize {
        1 << match self.id {
            0 => 16,
            1 => 13,
            _ => 10,
        }
    }

    /// Reads the AT flags that follow the region or dictionary flags.
    pub(crate) fn parse(id: u8, ext: bool, b: &mut Bits<'_>) -> Result<Self> {
        let n = Self::nominal(id, ext).len();
        let mut at = Vec::with_capacity(n);
        for _ in 0..n {
            let x = i16::from(b.bits(8)? as u8 as i8);
            let y = b.bits(8)? as u8 as i8;
            if y > 0 || (y == 0 && x >= 0) {
                return Err(Error::Invalid("adaptive pixel not before the current one"));
            }
            at.push((x, y));
        }
        Ok(Self { id, ext, at })
    }

    /// The template of a halftone bit plane or (with `first`) of a pattern
    /// dictionary: nominal but for A1.
    pub(crate) fn nominal_but(id: u8, first: Option<(i16, i8)>) -> Self {
        let mut at = Self::nominal(id, false).to_vec();
        if let Some(f) = first {
            at[0] = f;
        }
        Self { id, ext: false, at }
    }
}

/// One template's context layout: where the windows over the rows above sit.
trait Shape {
    /// The SLTP context.
    const SLTP: usize;
    /// Context bit where row -1's newest pixel enters, and how far right of
    /// the current pixel that is. Likewise row -2 (no pixels if bit is 0).
    const R1: (u32, u32);
    const R2: (u32, u32);
    /// Number of contexts.
    const N: usize;
    /// The context bits that shift: all but the last of each window.
    const SHIFT: u32 = {
        let m = (Self::N as u32 - 1) & !1 & !(1 << Self::R1.0);
        if Self::R2.0 == 0 {
            m
        } else {
            m & !(1 << Self::R2.0)
        }
    };
}

struct T0;
struct T1;
struct T2;
struct T3;

// Row -2: x-2..=x+2, row -1: x-3..=x+3, then x-4..=x-1.
impl Shape for T0 {
    const SLTP: usize = 0b1001_1011_0010_0101;
    const R1: (u32, u32) = (4, 3);
    const R2: (u32, u32) = (11, 2);
    const N: usize = 1 << 16;
}
// Row -2: x-1..=x+2, row -1: x-2..=x+3, then x-3..=x-1.
impl Shape for T1 {
    const SLTP: usize = 0b0_0111_1001_0101;
    const R1: (u32, u32) = (3, 3);
    const R2: (u32, u32) = (9, 2);
    const N: usize = 1 << 13;
}
// Row -2: x-1..=x+1, row -1: x-2..=x+2, then x-2..=x-1.
impl Shape for T2 {
    const SLTP: usize = 0b00_1110_0101;
    const R1: (u32, u32) = (2, 2);
    const R2: (u32, u32) = (7, 1);
    const N: usize = 1 << 10;
}
// Row -1: x-3..=x+2, then x-4..=x-1.
impl Shape for T3 {
    const SLTP: usize = 0b01_1001_0101;
    const R1: (u32, u32) = (4, 2);
    const R2: (u32, u32) = (0, 0);
    const N: usize = 1 << 10;
}

/// The context of the first pixel of a byte but for its history: the pixels
/// of the rows above, in windows whose bit 31 is pixel `x0 - 8`.
#[inline]
fn window<S: Shape>(r2: u32, r1: u32) -> u32 {
    let part = |w: u32, (bit, right): (u32, u32), width: u32| {
        ((w >> (23 - right)) & ((1 << width) - 1)) << bit
    };
    let top = S::N.trailing_zeros();
    if S::R2.0 == 0 {
        part(r1, S::R1, top - S::R1.0)
    } else {
        part(r1, S::R1, S::R2.0 - S::R1.0) | part(r2, S::R2, top - S::R2.0)
    }
}

/// The context of pixel `j + 1` from pixel `j`'s: shift in the pixel decoded
/// and the pixels entering on the rows above.
#[inline]
fn advance<S: Shape>(c: u32, d: u32, r2: u32, r1: u32, j: u32) -> u32 {
    let mut n = ((c << 1) & S::SHIFT) | d;
    n |= ((r1 >> (22 - S::R1.1 - j)) & 1) << S::R1.0;
    if S::R2.0 != 0 {
        n |= ((r2 >> (22 - S::R2.1 - j)) & 1) << S::R2.0;
    }
    n
}

/// Decode `w` x `h` pixels with contexts `cx` (the caller has charged the
/// budget). Pixels set in `skip` are 0 and not coded.
pub(crate) fn decode(
    a: &mut Arith<'_>,
    cx: &mut [u8],
    t: &Template,
    tpgdon: bool,
    skip: Option<&Bitmap>,
    w: u32,
    h: u32,
) -> Bitmap {
    let moved: Vec<(u32, i16, i8)> = t
        .bits()
        .iter()
        .zip(&t.at)
        .zip(Template::nominal(t.id, t.ext))
        .filter(|((_, at), nominal)| at != nominal)
        .map(|((&bit, &(x, y)), _)| (bit, x, y))
        .collect();
    let r = Region {
        moved: &moved,
        tpgdon,
        skip,
        w,
        h,
    };
    match t.id {
        0 => r.run::<T0, { T0::N }>(a, cx),
        1 => r.run::<T1, { T1::N }>(a, cx),
        2 => r.run::<T2, { T2::N }>(a, cx),
        _ => r.run::<T3, { T3::N }>(a, cx),
    }
}

struct Region<'r> {
    /// AT pixels away from their nominal places: context bit and offset.
    moved: &'r [(u32, i16, i8)],
    tpgdon: bool,
    skip: Option<&'r Bitmap>,
    w: u32,
    h: u32,
}

impl Region<'_> {
    fn run<S: Shape, const N: usize>(&self, coder: &mut Arith<'_>, cx: &mut [u8]) -> Bitmap {
        let (w, h) = (self.w, self.h);
        let mut out = Bitmap::new(w, h, false);
        let stride = out.stride;
        let Some(cx) = cx.first_chunk_mut::<N>() else {
            debug_assert!(false, "context array size");
            return out;
        };
        if w == 0 {
            return out;
        }
        // The coder's registers stay local to the row loop.
        let mut a = *coder;
        let mut ltp = 0;
        for y in 0..h as usize {
            if self.tpgdon {
                ltp ^= a.bit(&mut cx[S::SLTP & (N - 1)]);
            }
            let (above, cur) = out.data.split_at_mut(y * stride);
            let cur = &mut cur[..stride];
            // The row `n` up, empty above the bitmap.
            let up = |n: usize| -> &[u8] {
                if n > y {
                    &[]
                } else {
                    &above[(y - n) * stride..(y - n + 1) * stride]
                }
            };
            if ltp == 1 {
                if y > 0 {
                    cur.copy_from_slice(up(1));
                }
            } else if self.moved.is_empty() && self.skip.is_none() {
                a = row_nominal::<S, N>(a, cx, up(2), up(1), cur, w);
            } else {
                let skip = self.skip.map_or(&[][..], |s| s.row(y as i64));
                a = self.row_general::<S, N>(a, cx, skip, &up, cur);
            }
        }
        *coder = a;
        out
    }

    /// Moved AT pixels and/or skipping: the moved pixels' bits are masked out
    /// of the nominal context and fetched eight at a time from where they
    /// point.
    fn row_general<'r, 'd, S: Shape, const N: usize>(
        &self,
        mut a: Arith<'d>,
        cx: &mut [u8; N],
        skip: &[u8],
        up: &dyn Fn(usize) -> &'r [u8],
        cur: &mut [u8],
    ) -> Arith<'d> {
        let moved = self.moved;
        let keep = !moved.iter().fold(0u32, |m, &(bit, _, _)| m | 1 << bit);
        let (r2, r1) = (up(2), up(1));
        // Pixels of this row, newest in bit 0.
        let mut hist = 0u32;
        let last = cur.len() - 1;
        let mut ahead = [0u32; 12];
        for i in 0..cur.len() {
            let x0 = i as i64 * 8;
            let (w2, w1) = (fetch32(r2, x0 - 8), fetch32(r1, x0 - 8));
            for (s, &(_, ax, ay)) in ahead.iter_mut().zip(moved) {
                let x = x0 + i64::from(ax);
                if ay < 0 {
                    *s = fetch32(up(usize::from(ay.unsigned_abs())), x);
                } else if ax <= -8 {
                    // Same row, far enough back that its bytes are stored.
                    *s = fetch32(&cur[..i], x);
                }
            }
            let sk = skip.get(i).copied().unwrap_or(0);
            let n = if i == last { (self.w - 1) % 8 + 1 } else { 8 };
            let mut c = window::<S>(w2, w1) | (hist & ((1 << S::R1.0) - 1));
            for j in 0..n {
                let d = if (sk >> (7 - j)) & 1 == 1 {
                    0
                } else {
                    let mut c = c & keep;
                    for (&(bit, ax, ay), s) in moved.iter().zip(&ahead) {
                        let v = if ay < 0 || ax <= -8 {
                            s >> (31 - j)
                        } else {
                            // ax is -1 to -7 here.
                            hist >> (u32::from(ax.unsigned_abs()) - 1)
                        };
                        c |= (v & 1) << bit;
                    }
                    a.bit(&mut cx[c as usize & (N - 1)])
                };
                hist = (hist << 1) | d;
                c = advance::<S>(c, d, w2, w1, j);
            }
            cur[i] = (hist << (8 - n)) as u8;
        }
        a
    }
}

fn row_nominal<'d, S: Shape, const N: usize>(
    mut a: Arith<'d>,
    cx: &mut [u8; N],
    r2: &[u8],
    r1: &[u8],
    cur: &mut [u8],
    w: u32,
) -> Arith<'d> {
    const BYTES: u32 = 0x00FF_FFFF;
    let mut hist = 0u32;
    // The bytes at x0 - 8, x0 and x0 + 8 of the rows above.
    let first = |r: &[u8]| u32::from(r.first().copied().unwrap_or(0));
    let (mut w2, mut w1) = (first(r2), first(r1));
    let last = cur.len() - 1;
    for (i, byte) in cur.iter_mut().enumerate() {
        let next = |r: &[u8]| u32::from(r.get(i + 1).copied().unwrap_or(0));
        w2 = ((w2 << 8) | next(r2)) & BYTES;
        w1 = ((w1 << 8) | next(r1)) & BYTES;
        // Eight pixels deep inside white or black share one context, usually
        // a very skewed one.
        if i != last {
            if w2 | w1 | (hist & 0xFF) == 0 && a.run8(cx[0], 0) {
                continue;
            }
            if w2 & w1 == BYTES && hist & 0xFF == 0xFF && a.run8(cx[N - 1], 1) {
                *byte = 0xFF;
                continue;
            }
        }
        let n = if i == last { (w - 1) % 8 + 1 } else { 8 };
        let (b2, b1) = (w2 << 8, w1 << 8);
        let mut c = window::<S>(b2, b1) | (hist & ((1 << S::R1.0) - 1));
        for j in 0..n {
            let d = a.bit(&mut cx[c as usize & (N - 1)]);
            hist = (hist << 1) | d;
            c = advance::<S>(c, d, b2, b1, j);
        }
        *byte = (hist << (8 - n)) as u8;
    }
    a
}
