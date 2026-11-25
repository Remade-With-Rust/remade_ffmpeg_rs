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

/// One context, packed so that a decision needs a single load:
/// `Qe << 16 | MPS-successor << 9 | LPS-successor << 2 | MPS`.
/// Successors index this table (state * 2 + MPS).
static TABLE: [u32; 128] = {
    let mut t = [0u32; 128];
    let mut i = 0;
    while i < 94 {
        let (qe, nm, nl, sw) = STATES[i / 2];
        let mps = (i & 1) as u32;
        let nm = nm as u32 * 2 + mps;
        let nl = nl as u32 * 2 + (mps ^ sw as u32);
        t[i] = ((qe as u32) << 16) | (nm << 9) | (nl << 2) | mps;
        i += 1;
    }
    t
};

pub(crate) const CTX_MAG: usize = 14;
pub(crate) const CTX_RUN: usize = 17;
pub(crate) const CTX_UNI: usize = 18;

/// The 19 context states of a code-block (rounded up so that a masked index
/// is always in range).
#[derive(Clone, Copy)]
pub(crate) struct Contexts([u32; 32]);

impl Contexts {
    /// Initial states (Table D.7).
    pub fn new() -> Self {
        let mut c = [TABLE[0]; 32];
        c[0] = TABLE[4 * 2];
        c[CTX_RUN] = TABLE[3 * 2];
        c[CTX_UNI] = TABLE[46 * 2];
        Self(c)
    }
}

/// MQ decoder registers (software conventions, C.3).
///
/// `data` must end with the two bytes `FF FF`: the decoder stops at them and
/// feeds ones from there on, as the standard asks for at a marker.
#[derive(Clone, Copy)]
pub(crate) struct Mq<'a> {
    data: &'a [u8],
    pos: usize,
    a: u32,
    c: u32,
    ct: u32,
}

impl<'a> Mq<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut m = Mq {
            data,
            pos: 0,
            a: 0x8000,
            c: u32::from(data[0]) << 16,
            ct: 0,
        };
        m.byte_in();
        m.c <<= 7;
        m.ct -= 7;
        m
    }

    #[inline]
    fn byte_in(&mut self) {
        let next = self.data.get(self.pos + 1).copied().unwrap_or(0xFF);
        if self.data.get(self.pos).copied().unwrap_or(0xFF) == 0xFF {
            if next > 0x8F {
                self.c += 0xFF00;
                self.ct = 8;
            } else {
                self.pos += 1;
                self.c += u32::from(next) << 9;
                self.ct = 7;
            }
        } else {
            self.pos += 1;
            self.c += u32::from(next) << 8;
            self.ct = 8;
        }
    }

    #[inline(always)]
    fn renorm(&mut self) {
        let mut n = self.a.leading_zeros() - 16;
        loop {
            if self.ct == 0 {
                self.byte_in();
            }
            let s = n.min(self.ct);
            self.a <<= s;
            self.c <<= s;
            self.ct -= s;
            n -= s;
            if n == 0 {
                break;
            }
        }
    }

    /// Decode one decision in context `ctx`.
    #[inline(always)]
    pub fn decode(&mut self, cx: &mut Contexts, ctx: usize) -> u32 {
        let e = cx.0[ctx & 31];
        let qe = e >> 16;
        self.a -= qe;
        // `swapped`: the sub-interval sizes are inverted (conditional
        // exchange), so the smaller one belongs to the MPS.
        let swapped = self.a < qe;
        let lps = if (self.c >> 16) < qe {
            self.a = qe;
            !swapped
        } else {
            self.c -= qe << 16;
            if self.a & 0x8000 != 0 {
                return e & 1;
            }
            swapped
        };
        let next = if lps { e >> 2 } else { e >> 9 };
        cx.0[ctx & 31] = TABLE[(next & 127) as usize];
        self.renorm();
        (e & 1) ^ u32::from(lps)
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
            let got = (0..8).fold(0, |a, _| (a << 1) | mq.decode(&mut cx, 1));
            assert_eq!(got, want);
        }
    }

    #[test]
    fn empty_segment_is_finite() {
        let mut mq = Mq::new(&[0xFF, 0xFF]);
        let mut cx = Contexts::new();
        for i in 0..100_000 {
            mq.decode(&mut cx, i % 19);
        }
    }
}
