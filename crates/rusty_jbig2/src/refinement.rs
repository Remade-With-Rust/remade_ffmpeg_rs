//! Generic refinement region decoding (6.3).

use crate::arith::Arith;
use crate::bitmap::{fetch32, Bitmap};
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
    let mut out = Bitmap::new(w, h, false);
    let stride = out.stride;
    if w == 0 {
        return out;
    }
    let (ra1, ra2) = (t.at[0], t.at[1]);
    let custom = !t.one && t.at != [NOMINAL; 2];
    let mask = t.contexts() - 1;
    let sltp = if t.one { 0x08 } else { 0x10 };
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
        let prev = up(1);
        let ry = y - r.dy;
        let refs = [r.bitmap.row(ry - 1), r.bitmap.row(ry), r.bitmap.row(ry + 1)];
        let ra2_row = r.bitmap.row(ry + i64::from(ra2.1));
        let mut hist = 0u32;
        let last = stride - 1;
        for i in 0..stride {
            let x0 = i as i64 * 8;
            // Each window starts one pixel left of x0.
            let p = fetch32(prev, x0 - 1);
            let rx = x0 - r.dx - 1;
            let q = [
                fetch32(refs[0], rx),
                fetch32(refs[1], rx),
                fetch32(refs[2], rx),
            ];
            let (a1, a2) = if custom {
                let x1 = x0 + i64::from(ra1.0);
                (
                    if ra1.1 < 0 {
                        fetch32(up(-i64::from(ra1.1)), x1)
                    } else {
                        // Same row: only used far enough back to be stored.
                        fetch32(&cur[..i], x1)
                    },
                    fetch32(ra2_row, rx + 1 + i64::from(ra2.0)),
                )
            } else {
                (0, 0)
            };
            let n = if i == last { (w - 1) % 8 + 1 } else { 8 };
            for j in 0..n {
                let s = 29 - j;
                let (q0, q1, q2) = ((q[0] >> s) & 7, (q[1] >> s) & 7, (q[2] >> s) & 7);
                if ltp == 1 && q0 == q1 && q1 == q2 && (q0 == 0 || q0 == 7) {
                    hist = (hist << 1) | (q0 & 1);
                    continue;
                }
                let h1 = hist & 1;
                let p3 = (p >> s) & 7;
                let mut c = if t.one {
                    p3 << 7 | h1 << 6 | ((q0 >> 1) & 1) << 5 | q1 << 2 | (q2 & 3)
                } else {
                    p3 << 10 | h1 << 9 | q0 << 6 | q1 << 3 | q2
                };
                if custom {
                    let v1 = if ra1.1 < 0 || ra1.0 <= -8 {
                        a1 >> (31 - j)
                    } else {
                        // Same row: ra1.0 is -1 to -7.
                        hist >> (u32::from(ra1.0.unsigned_abs()) - 1)
                    };
                    let v2 = a2 >> (31 - j);
                    c = (c & !(1 << 12 | 1 << 8)) | (v1 & 1) << 12 | (v2 & 1) << 8;
                }
                hist = (hist << 1) | a.bit(&mut cx[c as usize & mask]);
            }
            cur[i] = (hist << (8 - n)) as u8;
        }
    }
    out
}
