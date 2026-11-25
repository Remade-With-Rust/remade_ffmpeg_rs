//! Annex E arithmetic decoder and Annex A integer decoding.

use crate::{Error, Result};

/// Table E.1: (Qe, NMPS, NLPS, SWITCH).
const QE: [(u16, u8, u8, u8); 47] = [
    (0x5601, 1, 1, 1),
    (0x3401, 2, 6, 0),
    (0x1801, 3, 9, 0),
    (0x0AC1, 4, 12, 0),
    (0x0521, 5, 29, 0),
    (0x0221, 38, 33, 0),
    (0x5601, 7, 6, 1),
    (0x5401, 8, 14, 0),
    (0x4801, 9, 14, 0),
    (0x3801, 10, 14, 0),
    (0x3001, 11, 17, 0),
    (0x2401, 12, 18, 0),
    (0x1C01, 13, 20, 0),
    (0x1601, 29, 21, 0),
    (0x5601, 15, 14, 1),
    (0x5401, 16, 14, 0),
    (0x5101, 17, 15, 0),
    (0x4801, 18, 16, 0),
    (0x3801, 19, 17, 0),
    (0x3401, 20, 18, 0),
    (0x3001, 21, 19, 0),
    (0x2801, 22, 19, 0),
    (0x2401, 23, 20, 0),
    (0x2201, 24, 21, 0),
    (0x1C01, 25, 22, 0),
    (0x1801, 26, 23, 0),
    (0x1601, 27, 24, 0),
    (0x1401, 28, 25, 0),
    (0x1201, 29, 26, 0),
    (0x1101, 30, 27, 0),
    (0x0AC1, 31, 28, 0),
    (0x09C1, 32, 29, 0),
    (0x08A1, 33, 30, 0),
    (0x0521, 34, 31, 0),
    (0x0441, 35, 32, 0),
    (0x02A1, 36, 33, 0),
    (0x0221, 37, 34, 0),
    (0x0141, 38, 35, 0),
    (0x0111, 39, 36, 0),
    (0x0085, 40, 37, 0),
    (0x0049, 41, 38, 0),
    (0x0025, 42, 39, 0),
    (0x0015, 43, 40, 0),
    (0x0009, 44, 41, 0),
    (0x0005, 45, 42, 0),
    (0x0001, 45, 43, 0),
    (0x5601, 46, 46, 0),
];

/// A context byte is `index << 1 | mps`; this is its row of Table E.1 with
/// the next context bytes worked out, so a decision is one table load.
/// Entries past the 47 states are unreachable.
#[derive(Clone, Copy)]
struct State {
    /// Scaled as [`Arith::a`] is.
    qe: u32,
    nmps: u8,
    nlps: u8,
}

static STATES: [State; 256] = {
    let mut t = [State {
        qe: 0x5601 << 16,
        nmps: 0,
        nlps: 0,
    }; 256];
    let mut i = 0;
    while i < 47 {
        let (qe, nm, nl, sw) = QE[i];
        let mut mps = 0u8;
        while mps < 2 {
            t[i * 2 + mps as usize] = State {
                qe: (qe as u32) << 16,
                nmps: (nm << 1) | mps,
                nlps: (nl << 1) | (mps ^ sw),
            };
            mps += 1;
        }
        i += 1;
    }
    t
};

/// The software-conventions decoder of Annex G: the same decisions as Figure
/// E.14's, with the C register complemented so that the usual outcome (MPS,
/// no renormalisation) updates A alone.
#[derive(Clone, Copy)]
pub(crate) struct Arith<'a> {
    data: &'a [u8],
    pos: usize,
    /// The interval, 16 bits up so that it compares with `c` as it is.
    a: u32,
    c: u32,
    ct: u32,
}

impl<'a> Arith<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        let mut d = Self {
            data,
            pos: 0,
            a: 0x8000 << 16,
            c: 0,
            ct: 0,
        };
        d.c = (d.byte(0) ^ 0xFF) << 16;
        d.byte_in();
        d.c <<= 7;
        d.ct -= 7;
        d
    }

    /// Past the end the stream reads as 0xFF, which BYTEIN treats as an
    /// endless marker (1 bits): exactly the bytes an encoder may strip.
    #[inline]
    fn byte(&self, i: usize) -> u32 {
        self.data.get(i).map_or(0xFF, |&b| u32::from(b))
    }

    fn byte_in(&mut self) {
        let next = self.byte(self.pos + 1);
        if self.byte(self.pos) == 0xFF {
            if next > 0x8F {
                self.ct = 8;
            } else {
                self.pos += 1;
                self.c = self.c.wrapping_add(0xFE00).wrapping_sub(next << 9);
                self.ct = 7;
            }
        } else {
            self.pos += 1;
            self.c += 0xFF00 - (next << 8);
            self.ct = 8;
        }
    }

    /// RENORMD, with the shift that does not need a new byte inline.
    #[inline]
    fn renorm(&mut self) {
        let n = self.a.leading_zeros();
        self.a <<= n;
        if n <= self.ct {
            self.c <<= n;
            self.ct -= n;
        } else {
            // By value, so that the registers of a local coder need not live
            // in memory for the sake of this path.
            *self = self.shift_in(n);
        }
    }

    #[inline(never)]
    fn shift_in(mut self, mut n: u32) -> Self {
        while n > 0 {
            if self.ct == 0 {
                self.byte_in();
            }
            let s = n.min(self.ct);
            self.c <<= s;
            self.ct -= s;
            n -= s;
        }
        self
    }

    /// Decode `n` (at most 64) decisions at once if all are the more probable
    /// symbol `d` of one context and none renormalises; otherwise do nothing.
    #[inline]
    pub(crate) fn run(&mut self, cx: u8, d: u8, n: u32) -> bool {
        let qe = u64::from(STATES[usize::from(cx)].qe) * u64::from(n);
        let a = u64::from(self.a);
        if cx & 1 == d && a >= (0x8000 << 16) + qe && u64::from(self.c) < a - qe {
            self.a -= qe as u32;
            return true;
        }
        false
    }

    /// DECODE (E.3.2, G.1): one decision in the given context.
    #[inline]
    pub(crate) fn bit(&mut self, cx: &mut u8) -> u32 {
        let st = &STATES[usize::from(*cx)];
        let mps = u32::from(*cx & 1);
        let qe = st.qe;
        self.a -= qe;
        let lps;
        if self.c < self.a {
            if self.a & (0x8000 << 16) != 0 {
                return mps;
            }
            lps = self.a < qe;
        } else {
            self.c -= self.a;
            lps = self.a >= qe;
            self.a = qe;
        }
        *cx = if lps { st.nlps } else { st.nmps };
        self.renorm();
        mps ^ u32::from(lps)
    }

    /// Annex A.2; `None` is OOB.
    pub(crate) fn int(&mut self, cx: &mut IntCtx) -> Option<i64> {
        const ARMS: [(u32, i64); 6] = [(2, 0), (4, 4), (6, 20), (8, 84), (12, 340), (32, 4436)];
        let mut prev = 1usize;
        let mut get = |a: &mut Self| -> u32 {
            let d = a.bit(&mut cx.0[prev & 511]);
            prev = if prev < 256 {
                (prev << 1) | d as usize
            } else {
                (((prev << 1) | d as usize) & 511) | 256
            };
            d
        };
        let negative = get(self) == 1;
        let mut arm = 0;
        while arm < 5 && get(self) == 1 {
            arm += 1;
        }
        let (bits, base) = ARMS[arm];
        let mut v = 0i64;
        for _ in 0..bits {
            v = (v << 1) | i64::from(get(self));
        }
        v += base;
        match (negative, v) {
            (false, v) => Some(v),
            (true, 0) => None,
            (true, v) => Some(-v),
        }
    }

    /// [`Arith::int`] where OOB is not allowed.
    pub(crate) fn int_some(&mut self, cx: &mut IntCtx) -> Result<i64> {
        self.int(cx)
            .ok_or(Error::Invalid("unexpected out-of-band integer"))
    }

    /// Annex A.3. `cx` holds `1 << len` contexts.
    pub(crate) fn iaid(&mut self, cx: &mut [u8], len: u32) -> usize {
        let mut prev = 1usize;
        for _ in 0..len {
            prev = (prev << 1) | self.bit(&mut cx[prev]) as usize;
        }
        prev - (1 << len)
    }
}

/// One integer procedure's 512 contexts.
#[derive(Clone)]
pub(crate) struct IntCtx([u8; 512]);

impl IntCtx {
    pub(crate) fn new() -> Self {
        Self([0; 512])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T.88 H.2: the arithmetic coder's test sequence, decision by decision.
    #[test]
    fn annex_h2_sequence() {
        let coded = [
            0x84, 0xC7, 0x3B, 0xFC, 0xE1, 0xA1, 0x43, 0x04, 0x02, 0x20, 0x00, 0x00, 0x41, 0x0D,
            0xBB, 0x86, 0xF4, 0x31, 0x7F, 0xFF, 0x88, 0xFF, 0x37, 0x47, 0x1A, 0xDB, 0x6A, 0xDF,
            0xFF, 0xAC,
        ];
        let plain = [
            0x00, 0x02, 0x00, 0x51, 0x00, 0x00, 0x00, 0xC0, 0x03, 0x52, 0x87, 0x2A, 0xAA, 0xAA,
            0xAA, 0xAA, 0x82, 0xC0, 0x20, 0x00, 0xFC, 0xD7, 0x9E, 0xF6, 0xBF, 0x7F, 0xED, 0x90,
            0x4F, 0x46, 0xA3, 0xBF,
        ];
        let mut a = Arith::new(&coded);
        let mut cx = 0u8;
        for want in plain {
            let mut got = 0u32;
            for _ in 0..8 {
                got = (got << 1) | a.bit(&mut cx);
            }
            assert_eq!(got, want);
        }
    }
}
