//! Generic region decoding (6.2), arithmetic variant.
//!
//! A context is three windows of pixels: row -1's, this row's and row -2's,
//! from the low bits up, each with its newest pixel in its lowest bit and the
//! AT pixels at their nominal places. With this row's in the middle, the bits
//! that enter the other two are far enough apart to come from one word, eight
//! pixels' worth at a time.

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
const BITS0: [u32; 4] = [0, 6, 11, 15];
const BITS_EXT: [u32; 12] = [8, 13, 5, 14, 12, 1, 9, 10, 11, 0, 15, 6];

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
            _ => &[0],
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

    /// Length of the table they are kept in.
    pub(crate) fn table(&self) -> usize {
        match self.id {
            0 => T0::N,
            1 => T1::N,
            2 => T2::N,
            _ => T3::N,
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

/// One template's context layout.
trait Shape {
    /// Length of the context table. Template 2 leaves half of its unused, so
    /// that its windows are as far apart as the others'.
    const N: usize;
    /// Pixels of row -1, from bit 0, and how far right the newest is.
    const R1: (u32, u32);
    /// Pixels of this row and the bit they start at.
    const H: (u32, u32);
    /// Pixels of row -2 (if any), the bit they start at, and how far right
    /// the newest is.
    const R2: (u32, u32, u32);
    /// The SLTP context: its windows as above.
    const SLTP: (usize, usize, usize);

    const SLTP_CONTEXT: usize =
        Self::SLTP.0 | Self::SLTP.1 << Self::H.1 | Self::SLTP.2 << Self::R2.1;
    /// The context of a pixel amid black ones.
    const BLACK: usize = ((1 << Self::R1.0) - 1)
        | ((1 << Self::H.0) - 1) << Self::H.1
        | ((1 << Self::R2.0) - 1) << Self::R2.1;
    /// The bits where a window's newest pixel enters.
    const NEWEST: u32 = 1 | 1 << Self::H.1 | (Self::R2.0 != 0) as u32 * (1 << Self::R2.1);
    /// [`entering`] eight black pixels on each row.
    const ENTER_BLACK: u32 = 0xFF | (Self::R2.0 != 0) as u32 * (0xFF << Self::R2.1);
    /// The bits that are another pixel's, shifted.
    const SHIFTED: u32 = Self::BLACK as u32 & !Self::NEWEST;
}

struct T0;
struct T1;
struct T2;
struct T3;

// Row -2: x-2..=x+2, row -1: x-3..=x+3, then x-4..=x-1.
impl Shape for T0 {
    const N: usize = 1 << 16;
    const R1: (u32, u32) = (7, 3);
    const H: (u32, u32) = (4, 7);
    const R2: (u32, u32, u32) = (5, 11, 2);
    const SLTP: (usize, usize, usize) = (0b011_0010, 0b0101, 0b1_0011);
}
// Row -2: x-1..=x+2, row -1: x-2..=x+3, then x-3..=x-1.
impl Shape for T1 {
    const N: usize = 1 << 13;
    const R1: (u32, u32) = (6, 3);
    const H: (u32, u32) = (3, 6);
    const R2: (u32, u32, u32) = (4, 9, 2);
    const SLTP: (usize, usize, usize) = (0b11_0010, 0b101, 0b0011);
}
// Row -2: x-1..=x+1, row -1: x-2..=x+2, then x-2..=x-1.
impl Shape for T2 {
    const N: usize = 1 << 11;
    const R1: (u32, u32) = (5, 2);
    const H: (u32, u32) = (2, 5);
    const R2: (u32, u32, u32) = (3, 8, 1);
    const SLTP: (usize, usize, usize) = (0b1_1001, 0b01, 0b001);
}
// Row -1: x-3..=x+2, then x-4..=x-1.
impl Shape for T3 {
    const N: usize = 1 << 10;
    const R1: (u32, u32) = (6, 2);
    const H: (u32, u32) = (4, 6);
    const R2: (u32, u32, u32) = (0, 0, 0);
    const SLTP: (usize, usize, usize) = (0b01_1001, 0b0101, 0);
}

/// The context of a row's first pixel, from the first bytes of rows -2 and -1.
#[inline]
fn first<S: Shape>(r2: &[u8], r1: &[u8]) -> u32 {
    let byte = |r: &[u8]| u32::from(r.first().copied().unwrap_or(0));
    let c = byte(r1) >> (7 - S::R1.1);
    if S::R2.0 == 0 {
        c
    } else {
        c | byte(r2) >> (7 - S::R2.2) << S::R2.1
    }
}

/// The pixels that enter the windows over rows -2 and -1 for the second to
/// the ninth pixel from `x0`, both rows' in step: bit `7 - j` (row -1) and
/// that plus `R2.1` (row -2). `r2` and `r1` end with the bytes at `x0` and
/// `x0 + 8`.
#[inline]
fn entering<S: Shape>(r2: u32, r1: u32) -> u32 {
    let next = (r1 >> (7 - S::R1.1)) & 0xFF;
    if S::R2.0 == 0 {
        next
    } else {
        next | ((r2 >> (7 - S::R2.2)) & 0xFF) << S::R2.1
    }
}

/// The next pixel's context: shift in the pixel `d` decoded and the pixels
/// entering on the rows above, the top ones of `next`.
#[inline]
fn advance<S: Shape>(c: u32, d: u32, next: u32) -> u32 {
    let enter = (next >> 7) & S::NEWEST & !(1 << S::H.1);
    ((c << 1) & S::SHIFTED) | d << S::H.1 | enter
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
        0 => r.pick::<T0, { T0::N }>(a, cx),
        1 => r.pick::<T1, { T1::N }>(a, cx),
        2 => r.pick::<T2, { T2::N }>(a, cx),
        _ => r.pick::<T3, { T3::N }>(a, cx),
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
    fn pick<S: Shape, const N: usize>(&self, coder: &mut Arith<'_>, cx: &mut [u8]) -> Bitmap {
        if self.moved.is_empty() && self.skip.is_none() {
            self.run::<S, N, true>(coder, cx)
        } else {
            self.run::<S, N, false>(coder, cx)
        }
    }

    /// `PLAIN`: no moved AT pixels, no skipping. Each a function of its own,
    /// so that the plain one's registers are not shared out to the other.
    #[inline(never)]
    fn run<S: Shape, const N: usize, const PLAIN: bool>(
        &self,
        coder: &mut Arith<'_>,
        cx: &mut [u8],
    ) -> Bitmap {
        let (w, h) = (self.w, self.h);
        let mut out = Bitmap::new(w, h, false);
        let stride = out.stride;
        let wide;
        let white = if let Some(z) = WHITE_ROW.get(..stride) {
            z
        } else {
            wide = vec![0; stride];
            &wide
        };
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
                ltp ^= a.bit(&mut cx[S::SLTP_CONTEXT & (N - 1)]);
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
            } else if PLAIN {
                let up = |n: usize| if n > y { white } else { up(n) };
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
    /// of the nominal context and gathered eight at a time from where they
    /// point.
    fn row_general<'r, 'd, S: Shape, const N: usize>(
        &self,
        mut a: Arith<'d>,
        cx: &mut [u8; N],
        skip: &[u8],
        up: &dyn Fn(usize) -> &'r [u8],
        cur: &mut [u8],
    ) -> Arith<'d> {
        // Pixels of this row, newest in bit 0.
        let mut hist = 0u32;
        let keep = !self.moved.iter().fold(0u32, |m, &(bit, _, _)| m | 1 << bit);
        // Those at most seven back on this row are not stored yet, but in the
        // history. The others, each with its row (none: this one).
        let mut near = [(0u32, 0u32); 12];
        let mut far = [(0u32, 0i64, None); 12];
        let (mut nears, mut fars) = (0, 0);
        for &(bit, ax, ay) in self.moved {
            if ay == 0 && ax > -8 {
                near[nears] = (bit, u32::from(ax.unsigned_abs()) - 1);
                nears += 1;
            } else {
                let row = (ay < 0).then(|| up(usize::from(ay.unsigned_abs())));
                far[fars] = (bit, i64::from(ax), row);
                fars += 1;
            }
        }
        let (near, far) = (&near[..nears], &far[..fars]);
        let (r2, r1) = (up(2), up(1));
        let last = cur.len() - 1;
        let mut c = first::<S>(r2, r1);
        let byte = |r: &[u8]| u32::from(r.first().copied().unwrap_or(0));
        let (mut w2, mut w1) = (byte(r2), byte(r1));
        for i in 0..cur.len() {
            let sk = skip.get(i).copied().unwrap_or(0);
            let x0 = i as i64 * 8;
            let after = |r: &[u8]| u32::from(r.get(i + 1).copied().unwrap_or(0));
            w2 = (w2 << 8) & 0xFFFF | after(r2);
            w1 = (w1 << 8) & 0xFFFF | after(r1);
            let mut next = entering::<S>(w2, w1);
            let mut views = [0u8; 12];
            // Whether any of the moved pixels is black, or white.
            let (mut any, mut all) = (0u8, 0xFFu8);
            for (v, &(_, ax, row)) in views.iter_mut().zip(far) {
                *v = (fetch32(row.unwrap_or(&cur[..i]), x0 + ax) >> 24) as u8;
                any |= *v;
                all &= *v;
            }
            if i != last && sk == 0 {
                if c | next | u32::from(any) | (hist & 0xFF) == 0 && a.run(cx[0], 0, 8) {
                    hist <<= 8;
                    continue;
                }
                if c == S::BLACK as u32
                    && next == S::ENTER_BLACK
                    && hist & u32::from(all) == 0xFF
                    && a.run(cx[S::BLACK & (N - 1)], 1, 8)
                {
                    cur[i] = 0xFF;
                    continue;
                }
            }
            let mut moved = [0u16; 8];
            for (&v, &(bit, _, _)) in views.iter().zip(far) {
                spread(&mut moved, v, 1 << bit);
            }
            let n = if i == last { (self.w - 1) % 8 + 1 } else { 8 };
            if sk == 0 && near.is_empty() {
                for &moved in &moved[..n as usize] {
                    let d = a.bit(&mut cx[((c & keep) | u32::from(moved)) as usize & (N - 1)]);
                    hist = (hist << 1) | d;
                    c = advance::<S>(c, d, next);
                    next <<= 1;
                }
                cur[i] = (hist << (8 - n)) as u8;
                continue;
            }
            for (j, moved) in (0..n).zip(moved) {
                let d = if (sk >> (7 - j)) & 1 == 1 {
                    0
                } else {
                    let mut c = (c & keep) | u32::from(moved);
                    for &(bit, back) in near {
                        c |= ((hist >> back) & 1) << bit;
                    }
                    a.bit(&mut cx[c as usize & (N - 1)])
                };
                hist = (hist << 1) | d;
                c = advance::<S>(c, d, next);
                next <<= 1;
            }
            cur[i] = (hist << (8 - n)) as u8;
        }
        a
    }
}

/// Set `bit` in those of `out` whose bit of `v` is set, most significant
/// first.
#[inline]
fn spread(out: &mut [u16; 8], v: u8, bit: u16) {
    for (o, m) in out.iter_mut().zip([0x80u16, 0x40, 0x20, 0x10, 8, 4, 2, 1]) {
        *o |= if u16::from(v) & m != 0 { bit } else { 0 };
    }
}

/// A row of white pixels, for all but the widest bitmaps.
static WHITE_ROW: [u8; 1024] = [0; 1024];

/// The rows above, `r2` and `r1`, are as long as `cur`.
fn row_nominal<'d, S: Shape, const N: usize>(
    mut a: Arith<'d>,
    cx: &mut [u8; N],
    r2: &[u8],
    r1: &[u8],
    cur: &mut [u8],
    w: u32,
) -> Arith<'d> {
    let len = cur.len();
    let (r2, r1) = (&r2[..len], &r1[..len]);
    let mut c = first::<S>(r2, r1);
    let (mut w2, mut w1) = (u32::from(r2[0]), u32::from(r1[0]));
    let mut i = 0;
    while i < len {
        let last = i + 1 == len;
        let mut n = (w - 1) % 8 + 1;
        w2 = (w2 << 8) & 0xFFFF;
        w1 = (w1 << 8) & 0xFFFF;
        if !last {
            n = 8;
            w2 |= u32::from(r2[i + 1]);
            w1 |= u32::from(r1[i + 1]);
        }
        let mut next = entering::<S>(w2, w1);
        // Pixels deep inside white or black share one context, usually a very
        // skewed one: take 64 or 8 at once.
        if !last && c | next == 0 {
            let from = i;
            while words(r2, r1, i + 1).is_some_and(|(p, q)| p | q == 0) && a.run(cx[0], 0, 64) {
                i += 8;
            }
            if i != from {
                (w2, w1) = (0, 0);
                continue;
            }
            if a.run(cx[0], 0, 8) {
                i += 1;
                continue;
            }
        }
        if !last && c == S::BLACK as u32 && next == S::ENTER_BLACK {
            let from = i;
            while words(r2, r1, i + 1).is_some_and(|(p, q)| p & q == u64::MAX)
                && a.run(cx[S::BLACK & (N - 1)], 1, 64)
            {
                cur[i..i + 8].fill(0xFF);
                i += 8;
            }
            if i != from {
                (w2, w1) = (0xFF, 0xFF);
                continue;
            }
            if a.run(cx[S::BLACK & (N - 1)], 1, 8) {
                cur[i] = 0xFF;
                i += 1;
                continue;
            }
        }
        let mut byte = 0;
        for _ in 0..n {
            let d = a.bit(&mut cx[c as usize & (N - 1)]);
            byte = (byte << 1) | d;
            c = advance::<S>(c, d, next);
            next <<= 1;
        }
        cur[i] = (byte << (8 - n)) as u8;
        i += 1;
    }
    a
}

/// Eight bytes of both rows, if there are that many from `i` on.
#[inline]
fn words(r2: &[u8], r1: &[u8], i: usize) -> Option<(u64, u64)> {
    let p = r2.get(i..)?.first_chunk::<8>()?;
    let q = r1.get(i..)?.first_chunk::<8>()?;
    Some((u64::from_ne_bytes(*p), u64::from_ne_bytes(*q)))
}
