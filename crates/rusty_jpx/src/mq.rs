//! The MQ arithmetic decoder (ISO/IEC 15444-1 Annex C) and the raw bit reader
//! of the selective bypass mode (D.6).

/// Probability states: `Qe`, next state after an MPS, after an LPS, and
/// whether an LPS swaps the sense of the MPS (Table C.2).
const STATES: [(u16, u8, u8, u8); 47] = [
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

/// A context, from `i = state * 2 + MPS`: `Qe << 48 | i << 4 | MPS`.
const fn context(i: usize) -> u64 {
    ((STATES[i / 2].0 as u64) << 48) | (i << 4 | i & 1) as u64
}

/// Contexts after an MPS (even index) and an LPS (odd), by `2 * i`.
static NEXT: [u64; 256] = {
    let mut t = [0; 256];
    let mut i = 0;
    while i < 94 {
        let (_, nm, nl, sw) = STATES[i / 2];
        t[2 * i] = context(nm as usize * 2 + (i & 1));
        t[2 * i + 1] = context(nl as usize * 2 + ((i & 1) ^ sw as usize));
        i += 1;
    }
    t
};

/// The contexts of a code-block (Table D.1 to D.4): 0 to 8 zero coding, 9 to
/// 13 signs, 14 to 16 magnitude refinement, 17 runs and 18 the uniform one.
/// An enumeration, so that indexing by one needs no check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
#[rustfmt::skip]
pub(crate) enum Ctx {
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14, C15, C16, Run, Uni,
}

impl Ctx {
    /// Context `n`, below 17.
    pub const fn new(n: usize) -> Self {
        use Ctx::*;
        [
            C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14, C15, C16,
        ][n]
    }
}

/// The context states of a code-block.
#[derive(Clone, Copy)]
pub(crate) struct Contexts([u64; 19]);

impl Contexts {
    /// Initial states (Table D.7).
    pub fn new() -> Self {
        let mut c = [context(0); 19];
        c[Ctx::C0 as usize] = context(4 * 2);
        c[Ctx::Run as usize] = context(3 * 2);
        c[Ctx::Uni as usize] = context(46 * 2);
        Self(c)
    }
}

/// MQ decoder registers (software conventions, C.3), at the top of 64 bits,
/// so that there is room for several bytes below `Chigh`.
///
/// Past its end the data reads as `FF FF`, which makes the decoder feed ones
/// from there on, as the standard asks for at a marker.
#[derive(Clone, Copy)]
pub(crate) struct Mq<'a> {
    data: &'a [u8],
    /// Of the last byte read.
    pos: usize,
    a: u64,
    c: u64,
    /// Valid bits below `Chigh`.
    ct: u32,
}

/// Shift `C` by `n` (at most 15) bits, more than the `ct` that are valid.
#[inline(never)]
fn shift_in(data: &[u8], pos: usize, c: u64, ct: u32, n: u32) -> (usize, u64, u32) {
    let (mut pos, mut c, mut n) = (pos, c << ct, n - ct);
    if let Some(&[last, b0, b1, b2, b3, b4]) = data.get(pos..).and_then(|d| d.first_chunk()) {
        // Five plain bytes.
        let v = u32::from_be_bytes([b0, b1, b2, b3]);
        if last != 0xFF && !has_ff(v) {
            c += (u64::from(v) << 16) | (u64::from(b4) << 8);
            return (pos + 5, c << n, 40 - n);
        }
    }
    let at = |i: usize| data.get(i).copied().unwrap_or(0xFF);
    loop {
        // A byte after FF has seven bits, and one that carries into the bits
        // before it.
        let next = u64::from(at(pos + 1));
        let ct;
        if at(pos) != 0xFF {
            pos += 1;
            c = c.wrapping_add(next << 40);
            ct = 8;
        } else if next > 0x8F {
            c = c.wrapping_add(0xFF << 40);
            ct = 8;
        } else {
            pos += 1;
            c = c.wrapping_add(next << 41);
            ct = 7;
        }
        let s = n.min(ct);
        c <<= s;
        n -= s;
        if n == 0 {
            return (pos, c, ct - s);
        }
    }
}

/// Whether one of four bytes is FF.
#[inline(always)]
fn has_ff(v: u32) -> bool {
    let z = !v;
    z.wrapping_sub(0x0101_0101) & v & 0x8080_8080 != 0
}

impl<'a> Mq<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let first = data.first().copied().unwrap_or(0xFF);
        let (pos, c, ct) = shift_in(data, 0, u64::from(first) << 48, 0, 7);
        Mq {
            data,
            pos,
            a: 0x8000 << 48,
            c,
            ct,
        }
    }

    /// Decode one decision in context `ctx`.
    #[inline(always)]
    pub fn decode(&mut self, cx: &mut Contexts, ctx: Ctx) -> u32 {
        let ctx = ctx as usize;
        let e = cx.0[ctx];
        let qe = e & !0xFFFF;
        let a;
        let (c, low) = self.c.overflowing_sub(qe);
        // The sub-interval sizes may be inverted (conditional exchange): then
        // the smaller one belongs to the MPS.
        let lps = if low {
            a = qe;
            self.a.wrapping_sub(qe) >= qe
        } else {
            self.c = c;
            a = self.a.wrapping_sub(qe);
            if a >> 63 != 0 {
                self.a = a;
                return e as u32 & 1;
            }
            a < qe
        };
        cx.0[ctx] = NEXT[((e as usize & 0x7F0) >> 3) | usize::from(lps)];
        let n = 63 - (a | 1).ilog2();
        self.a = a << n;
        if n <= self.ct {
            self.c <<= n;
            self.ct -= n;
        } else {
            (self.pos, self.c, self.ct) = shift_in(self.data, self.pos, self.c, self.ct, n);
        }
        (e as u32 ^ u32::from(lps)) & 1
    }
}

/// Raw bits of the bypass mode: MSB first, with a stuffed zero bit after each
/// `FF`. Past the end it reads ones.
#[derive(Clone, Copy)]
pub(crate) struct Raw<'a> {
    data: &'a [u8],
    pos: usize,
    byte: u8,
    ct: u32,
}

impl<'a> Raw<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Raw {
            data,
            pos: 0,
            byte: 0,
            ct: 0,
        }
    }

    #[inline(always)]
    pub fn bit(&mut self) -> u32 {
        if self.ct == 0 {
            let next = self.data.get(self.pos).copied().unwrap_or(0xFF);
            if self.byte == 0xFF {
                if next > 0x8F {
                    self.ct = 8;
                } else {
                    self.byte = next;
                    self.pos += 1;
                    self.ct = 7;
                }
            } else {
                self.byte = next;
                self.pos += 1;
                self.ct = 8;
            }
        }
        self.ct -= 1;
        u32::from(self.byte >> self.ct) & 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published MQ test sequence (ITU-T T.88 H.2; the same coder).
    #[test]
    fn published_test_sequence() {
        let coded = [
            0x84, 0xC7, 0x3B, 0xFC, 0xE1, 0xA1, 0x43, 0x04, 0x02, 0x20, 0x00, 0x00, 0x41, 0x0D,
            0xBB, 0x86, 0xF4, 0x31, 0x7F, 0xFF, 0x88, 0xFF, 0x37, 0x47, 0x1A, 0xDB, 0x6A, 0xDF,
            0xFF, 0xAC, 0xFF, 0xFF,
        ];
        let plain = [
            0x00, 0x02, 0x00, 0x51, 0x00, 0x00, 0x00, 0xC0, 0x03, 0x52, 0x87, 0x2A, 0xAA, 0xAA,
            0xAA, 0xAA, 0x82, 0xC0, 0x20, 0x00, 0xFC, 0xD7, 0x9E, 0xF6, 0xBF, 0x7F, 0xED, 0x90,
            0x4F, 0x46, 0xA3, 0xBF,
        ];
        let mut mq = Mq::new(&coded);
        let mut cx = Contexts::new();
        for want in plain {
            let got = (0..8).fold(0, |a, _| (a << 1) | mq.decode(&mut cx, Ctx::C1));
            assert_eq!(got, want);
        }
    }

    #[test]
    fn empty_segment_is_finite() {
        let mut mq = Mq::new(&[0xFF, 0xFF]);
        let mut cx = Contexts::new();
        for i in 0..100_000 {
            mq.decode(&mut cx, Ctx::new(i % 17));
        }
    }
}
