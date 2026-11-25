//! Generic refinement region decoding (6.3).

use crate::arith::Arith;
use crate::bitmap::{fetch32, Bitmap};
use crate::generic::spread;
use crate::huffman::Bits;
use crate::{Error, Result};

/// GRTEMPLATE and its adaptive pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RefTemplate {
    /// GRTEMPLATE 1 (10 pixels, no AT) rather than 0 (13 pixels).
    pub(crate) one: bool,
    /// RA1 in the bitmap being decoded, RA2 in the reference.
    pub(crate) at: [(i8, i8); 2],
}

const NOMINAL: (i8, i8) = (-1, -1);

impl RefTemplate {
    pub(crate) fn contexts(self) -> usize {
        if self.one {
            1 << 10
        } else {
            1 << 13
        }
    }

    /// Reads the refinement AT flags (present for template 0 only).
    pub(crate) fn parse(one: bool, b: &mut Bits<'_>) -> Result<Self> {
        let mut at = [NOMINAL; 2];
        if !one {
            for p in &mut at {
                *p = (b.bits(8)? as u8 as i8, b.bits(8)? as u8 as i8);
            }
            let (x, y) = at[0];
            if y > 0 || (y == 0 && x >= 0) {
                return Err(Error::Invalid("adaptive pixel not before the current one"));
            }
        }
        Ok(Self { one, at })
    }
}

/// The reference bitmap and its offset (GRREFERENCEDX, GRREFERENCEDY).
pub(crate) struct Reference<'a> {
    pub(crate) bitmap: &'a Bitmap,
    pub(crate) dx: i64,
    pub(crate) dy: i64,
}

/// Decode `w` x `h` pixels with contexts `cx` (the caller has charged the
/// budget).
pub(crate) fn decode(
    a: &mut Arith<'_>,
    cx: &mut [u8],
    t: RefTemplate,
    tpgron: bool,
    r: &Reference<'_>,
    w: u32,
    h: u32,
) -> Bitmap {
    if t.one {
        run::<true, false, { 1 << 10 }>(a, cx, t, tpgron, r, w, h)
    } else if t.at == [NOMINAL; 2] {
        run::<false, false, { 1 << 13 }>(a, cx, t, tpgron, r, w, h)
    } else {
        run::<false, true, { 1 << 13 }>(a, cx, t, tpgron, r, w, h)
    }
}

/// A row seen through a window that moves right by bytes: the byte that a
/// given position falls in and the two after it.
struct Window<'r> {
    row: &'r [u8],
    /// The next byte to come in; far out of range when left of the row.
    next: usize,
    bytes: u32,
    sh: u32,
}

impl<'r> Window<'r> {
    /// Ready to [`Window::advance`] to bit `pos`.
    fn new(row: &'r [u8], pos: i64) -> Self {
        let mut w = Self {
            row,
            next: (pos >> 3) as usize,
            bytes: 0,
            sh: (pos & 7) as u32,
        };
        w.load();
        w.load();
        w
    }

    #[inline]
    fn load(&mut self) {
        let b = self.row.get(self.next).map_or(0, |&b| u32::from(b));
        self.bytes = (self.bytes << 8) | b;
        self.next = self.next.wrapping_add(1);
    }

    /// The 16 pixels from here on, and eight further the next time.
    #[inline]
    fn advance(&mut self) -> u16 {
        self.load();
        (self.bytes >> (8 - self.sh)) as u16
    }

    /// Whether this and the next eight views are white.
    #[inline]
    fn white(&self) -> bool {
        let after = match self.row.get(self.next..).map(<[u8]>::first_chunk::<8>) {
            Some(Some(v)) => *v == [0; 8],
            // Partly outside: not worth finding out. All of it: white.
            Some(None) => self.next == self.row.len(),
            None => self.next < self.next.wrapping_add(8),
        };
        after && self.bytes & 0x00FF_FFFF == 0
    }

    /// Skip eight views that are white.
    #[inline]
    fn skip(&mut self) {
        self.bytes = 0;
        self.next = self.next.wrapping_add(8);
    }
}

/// The contexts of eight pixels but for the one to their left. The rows begin
/// with the pixel left of the first one's: `p` above in this bitmap, `q` the
/// reference's.
#[inline]
fn contexts8<const ONE: bool>(p: u16, q: [u16; 3]) -> [u16; 8] {
    /// Four lanes of 16 bits.
    const LANES: u64 = 0x0001_0001_0001_0001;
    let mut out = [0; 8];
    for (half, out) in out.chunks_exact_mut(4).enumerate() {
        // The three pixels around pixel `j` of the half at the top of lane
        // `j`: ten pixels' worth of bits leave room for what is shifted out of
        // the lane below.
        let lanes = |w: u16| u64::from((w & 0xFFC0) << (4 * half)) * 0x0008_0004_0002_0001;
        // From there to bit `to`, those of `three`.
        let put = |w: u16, three: u64, to: u32| (lanes(w) >> (13 - to)) & (LANES * (three << to));
        let x = if ONE {
            put(p, 7, 7) | (lanes(q[0]) >> 9) & (LANES << 5) | put(q[1], 7, 2) | put(q[2], 3, 0)
        } else {
            put(p, 7, 10) | put(q[0], 7, 6) | put(q[1], 7, 3) | put(q[2], 7, 0)
        };
        for (j, o) in out.iter_mut().enumerate() {
            *o = (x >> (16 * j)) as u16;
        }
    }
    out
}

/// For eight pixels: which have a uniform 3x3 neighbourhood in the reference,
/// and which a black one.
#[inline]
fn typical(q: [u16; 3]) -> (u8, u8) {
    let and = q[0] & q[1] & q[2];
    let or = q[0] | q[1] | q[2];
    let black = and & and << 1 & and >> 1;
    let white = !(or | or << 1 | or >> 1);
    (((black | white) >> 7) as u8, (black >> 7) as u8)
}

#[inline(never)]
fn run<const ONE: bool, const CUSTOM: bool, const N: usize>(
    coder: &mut Arith<'_>,
    cx: &mut [u8],
    t: RefTemplate,
    tpgron: bool,
    r: &Reference<'_>,
    w: u32,
    h: u32,
) -> Bitmap {
    let mut out = Bitmap::new(w, h, false);
    let stride = out.stride;
    let Some(cx) = cx.first_chunk_mut::<N>() else {
        debug_assert!(false, "context array size");
        return out;
    };
    if w == 0 {
        return out;
    }
    let mut a = *coder;
    let (ra1, ra2) = (t.at[0], t.at[1]);
    // RA1 at most seven back on this row is in the history.
    let near = ra1.1 == 0 && ra1.0 > -8;
    let left = if ONE { 6 } else { 9 };
    let sltp = if ONE { 0x08 } else { 0x10 };
    let mut ltp = 0;
    for y in 0..i64::from(h) {
        if tpgron {
            ltp ^= a.bit(&mut cx[sltp]);
        }
        let (above, cur) = out.data.split_at_mut(y as usize * stride);
        let cur = &mut cur[..stride];
        let up = |n: i64| -> &[u8] {
            if n > y || n < 1 {
                &[]
            } else {
                let s = (y - n) as usize * stride;
                &above[s..s + stride]
            }
        };
        let ry = y - r.dy;
        let row = |k: i64| Window::new(r.bitmap.row(ry + k), -r.dx - 1);
        let (mut p, mut q0, mut q1, mut q2) = (Window::new(up(1), -1), row(-1), row(0), row(1));
        let ra1_row = up(-i64::from(ra1.1));
        let ra2_row = r.bitmap.row(ry + i64::from(ra2.1));
        let mut hist = 0u32;
        let last = stride - 1;
        let mut at = 0;
        while at < stride {
            let i = at;
            at += 1;
            let above = p.advance();
            let q = [q0.advance(), q1.advance(), q2.advance()];
            let n = if i == last { (w - 1) % 8 + 1 } else { 8 };
            let (mut v1, mut v2) = (0, 0);
            if CUSTOM {
                let x0 = i as i64 * 8;
                let x1 = x0 + i64::from(ra1.0);
                let row = if ra1.1 < 0 { ra1_row } else { &cur[..i] };
                v1 = (fetch32(row, x1) >> 24) as u8;
                v2 = (fetch32(ra2_row, x0 - r.dx + i64::from(ra2.0)) >> 24) as u8;
            }
            let (mut sure, mut value) = (0, 0);
            if ltp == 1 {
                (sure, value) = typical(q);
                if sure == 0xFF && i != last {
                    cur[i] = value;
                    hist = u32::from(value);
                    if !CUSTOM && value == 0 {
                        while at + 8 < stride && [&p, &q0, &q1, &q2].iter().all(|w| w.white()) {
                            for w in [&mut p, &mut q0, &mut q1, &mut q2] {
                                w.skip();
                            }
                            at += 8;
                        }
                    }
                    continue;
                }
            }
            if sure == 0 && i != last {
                // The ten pixels of each row that the eight contexts have.
                let any = above | q[0] | q[1] | q[2];
                let all = above & q[0] & q[1] & q[2];
                if u32::from(any >> 6) | (hist & 0xFF) | u32::from(v1 | v2) == 0 {
                    if a.run(cx[0], 0, 8) {
                        if !CUSTOM {
                            while at + 8 < stride
                                && [&p, &q0, &q1, &q2].iter().all(|w| w.white())
                                && a.run(cx[0], 0, 64)
                            {
                                for w in [&mut p, &mut q0, &mut q1, &mut q2] {
                                    w.skip();
                                }
                                at += 8;
                            }
                        }
                        continue;
                    }
                } else if all >> 6 == 0x3FF
                    && hist & 0xFF == 0xFF
                    && (!CUSTOM || v2 == 0xFF && (near || v1 == 0xFF))
                    && a.run(cx[N - 1], 1, 8)
                {
                    cur[i] = 0xFF;
                    continue;
                }
            }
            let mut contexts = contexts8::<ONE>(above, q);
            if CUSTOM {
                for c in &mut contexts {
                    *c &= !(1 << 12 | 1 << 8);
                }
                if !near {
                    spread(&mut contexts, v1, 1 << 12);
                }
                spread(&mut contexts, v2, 1 << 8);
            }
            if sure == 0 && !(CUSTOM && near) {
                for &c in &contexts[..n as usize] {
                    let c = u32::from(c) | (hist & 1) << left;
                    hist = (hist << 1) | a.bit(&mut cx[c as usize & (N - 1)]);
                }
                cur[i] = (hist << (8 - n)) as u8;
                continue;
            }
            for (j, &c) in (0..n).zip(&contexts) {
                if (sure >> (7 - j)) & 1 == 1 {
                    hist = (hist << 1) | u32::from((value >> (7 - j)) & 1);
                    continue;
                }
                let mut c = u32::from(c) | (hist & 1) << left;
                if CUSTOM && near {
                    c |= ((hist >> (u32::from(ra1.0.unsigned_abs()) - 1)) & 1) << 12;
                }
                hist = (hist << 1) | a.bit(&mut cx[c as usize & (N - 1)]);
            }
            cur[i] = (hist << (8 - n)) as u8;
        }
    }
    *coder = a;
    out
}
