//! Tier-1: code-block coefficient decoding (ISO/IEC 15444-1 Annex D).
//!
//! State is kept per *stripe column* (four vertically adjacent samples) in one
//! `u32`, so that the common "nothing to do here" test covers four samples:
//!
//! | bits  | meaning |
//! |-------|---------|
//! | 0-17  | significance of the 6 x 3 samples around the column: row `i` (0 is the row above, 1-4 the column's own), column `j` (0 left, 1 own, 2 right) at bit `3 * i + j` |
//! | 18-23 | sign of the own column's six rows |
//! | 24-27 | the sample has been refined before |
//! | 28-31 | the sample was coded in this bit-plane's significance pass |
//!
//! The 3 x 3 neighbourhood of sample `k` is then `(flags >> 3 * k) & 0x1FF`.

use crate::mq::{Contexts, Mq, Raw, CTX_MAG, CTX_RUN, CTX_UNI};

/// Code-block style bits (Table A.19).
pub(crate) const STYLE_BYPASS: u8 = 1;
pub(crate) const STYLE_RESET: u8 = 2;
pub(crate) const STYLE_TERMALL: u8 = 4;
pub(crate) const STYLE_CAUSAL: u8 = 8;
pub(crate) const STYLE_SEGSYM: u8 = 32;
pub(crate) const STYLE_HT: u8 = 64;

/// Most magnitude bit-planes a code-block may have: 30, plus one bit for the
/// half-step and one for the sign.
pub(crate) const MAX_PLANES: u32 = 30;

const SIG_ALL: u32 = (1 << 18) - 1;
const SIG_OWN: u32 = 0x10 | (0x10 << 3) | (0x10 << 6) | (0x10 << 9);
const PI_ALL: u32 = 0xF << 28;
const NEIGHBOURS: u32 = 0x1EF;

/// Zero-coding contexts (Table D.1) by neighbourhood, for each of the three
/// subband classes (LL/LH, HL, HH).
static ZC: [[u8; 512]; 3] = {
    let mut t = [[0u8; 512]; 3];
    let mut v = 0;
    while v < 512 {
        let h = ((v >> 3) & 1) + ((v >> 5) & 1);
        let w = ((v >> 1) & 1) + ((v >> 7) & 1);
        let d = (v & 1) + ((v >> 2) & 1) + ((v >> 6) & 1) + ((v >> 8) & 1);
        t[0][v] = zc_lh(h, w, d);
        t[1][v] = zc_lh(w, h, d);
        t[2][v] = zc_hh(h + w, d);
        v += 1;
    }
    t
};

const fn zc_lh(h: usize, v: usize, d: usize) -> u8 {
    match (h, v, d) {
        (2, _, _) => 8,
        (1, 1 | 2, _) => 7,
        (1, _, 0) => 5,
        (1, _, _) => 6,
        (_, 2, _) => 4,
        (_, 1, _) => 3,
        (_, _, 0) => 0,
        (_, _, 1) => 1,
        _ => 2,
    }
}

const fn zc_hh(hv: usize, d: usize) -> u8 {
    match (d, hv) {
        (0, 0) => 0,
        (0, 1) => 1,
        (0, _) => 2,
        (1, 0) => 3,
        (1, 1) => 4,
        (1, _) => 5,
        (2, 0) => 6,
        (2, _) => 7,
        _ => 8,
    }
}

/// Sign contexts (Tables D.2, D.3): index bits are (significant, negative)
/// for left, right, up, down; the value is `context | flip << 7`.
static SC: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut v = 0;
    while v < 256 {
        let mut s = [0i32; 4];
        let mut i = 0;
        while i < 4 {
            if (v >> (2 * i)) & 1 != 0 {
                s[i] = 1 - 2 * ((v as i32 >> (2 * i + 1)) & 1);
            }
            i += 1;
        }
        let h = clamp1(s[0] + s[1]);
        let w = clamp1(s[2] + s[3]);
        let (flip, h, w) = if h < 0 || (h == 0 && w < 0) {
            (0x80, -h, -w)
        } else {
            (0, h, w)
        };
        let ctx = if h == 0 { 9 + w } else { 12 + w };
        t[v] = ctx as u8 | flip;
        v += 1;
    }
    t
};

const fn clamp1(v: i32) -> i32 {
    if v > 1 {
        1
    } else if v < -1 {
        -1
    } else {
        v
    }
}

/// Where a pass gets its decisions from: the MQ coder or raw bits.
trait Source: Copy {
    fn decision(&mut self, cx: &mut Contexts, ctx: usize) -> u32;
    /// A sign, given its [`SC`] entry.
    fn sign(&mut self, cx: &mut Contexts, sc: u8) -> u32;
}

impl Source for Mq<'_> {
    #[inline(always)]
    fn decision(&mut self, cx: &mut Contexts, ctx: usize) -> u32 {
        self.decode(cx, ctx)
    }
    #[inline(always)]
    fn sign(&mut self, cx: &mut Contexts, sc: u8) -> u32 {
        self.decode(cx, usize::from(sc & 31)) ^ u32::from(sc >> 7)
    }
}

impl Source for Raw<'_> {
    #[inline(always)]
    fn decision(&mut self, _: &mut Contexts, _: usize) -> u32 {
        self.bit()
    }
    #[inline(always)]
    fn sign(&mut self, _: &mut Contexts, _: u8) -> u32 {
        self.bit()
    }
}

/// One codeword segment of a code-block.
pub(crate) struct Segment {
    /// Coding passes in the segment.
    pub passes: u32,
    /// Its bytes in the block's data buffer, the two `FF` sentinel bytes
    /// included.
    pub start: usize,
    pub end: usize,
}

/// Reusable code-block decoder.
#[derive(Default)]
pub(crate) struct T1 {
    /// Decoded coefficients, row-major: sign in bit 31, below it the magnitude
    /// times two with the reconstruction half-step in place.
    pub data: Vec<u32>,
    flags: Vec<u32>,
    w: usize,
    causal: bool,
}

/// One stripe: the flag words of the stripe above, its own and the stripe
/// below (each with a border column on both sides), and its samples (up to
/// four rows).
struct Stripe<'a> {
    up: &'a mut [u32],
    cur: &'a mut [u32],
    down: &'a mut [u32],
    rows: &'a mut [u32],
    w: usize,
    causal: bool,
}

/// The flag words of column `x` (counted without the border) and of its two
/// neighbours: left, own, right.
#[inline(always)]
fn columns(words: &mut [u32], x: usize) -> &mut [u32; 3] {
    match words[x..].first_chunk_mut() {
        Some(c) => c,
        None => unreachable!("border columns"),
    }
}

/// Sign-coding table entry of sample `k` of a column (`f` is the column's
/// current flag word).
#[inline(always)]
fn sign_entry(cur: &[u32; 3], f: u32, k: u32) -> u8 {
    let l = (cur[0] >> (19 + k)) & 1;
    let r = (cur[2] >> (19 + k)) & 1;
    let nb = f >> (3 * k);
    let idx = ((nb >> 3) & 1)
        | (l << 1)
        | (((nb >> 5) & 1) << 2)
        | (r << 3)
        | (((nb >> 1) & 1) << 4)
        | (((f >> (18 + k)) & 1) << 5)
        | (((nb >> 7) & 1) << 6)
        | (((f >> (20 + k)) & 1) << 7);
    SC[(idx & 255) as usize]
}

impl Stripe<'_> {
    fn height(&self) -> u32 {
        (self.rows.len() / self.w) as u32
    }

    /// Decode the sign of sample `k` of column `x` and mark it significant.
    #[inline(always)]
    fn newly<S: Source>(
        &mut self,
        src: &mut S,
        cx: &mut Contexts,
        f: &mut u32,
        x: usize,
        k: u32,
        value: u32,
    ) {
        let cur = columns(self.cur, x);
        let s = src.sign(cx, sign_entry(cur, *f, k));
        self.rows[k as usize * self.w + x] = value | (s << 31);
        *f |= (0x10 << (3 * k)) | (s << (19 + k));
        cur[0] |= 0x20 << (3 * k);
        cur[2] |= 0x08 << (3 * k);
        // Vertically causal contexts: the stripe above never sees this one.
        if k == 0 && !self.causal {
            let up = columns(self.up, x);
            up[0] |= 1 << 17;
            up[1] |= (1 << 16) | (s << 23);
            up[2] |= 1 << 15;
        }
        if k == 3 {
            let down = columns(self.down, x);
            down[0] |= 1 << 2;
            down[1] |= (1 << 1) | (s << 18);
            down[2] |= 1;
        }
    }

    /// Significance propagation (D.3.1). `FULL`: the stripe has four rows.
    #[inline(always)]
    fn significance<S: Source, const FULL: bool>(
        &mut self,
        src: &mut S,
        cx: &mut Contexts,
        bp: u32,
        zc: &[u8; 512],
    ) {
        let value = 3 << bp;
        for x in 0..self.w {
            let mut f = self.cur[x + 1];
            if f & SIG_ALL == 0 {
                continue;
            }
            if FULL {
                // Unrolled by hand: the compiler leaves it a loop.
                self.significance_at(src, cx, &mut f, x, 0, value, zc);
                self.significance_at(src, cx, &mut f, x, 1, value, zc);
                self.significance_at(src, cx, &mut f, x, 2, value, zc);
                self.significance_at(src, cx, &mut f, x, 3, value, zc);
            } else {
                for k in 0..self.height() {
                    self.significance_at(src, cx, &mut f, x, k, value, zc);
                }
            }
            self.cur[x + 1] = f;
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn significance_at<S: Source>(
        &mut self,
        src: &mut S,
        cx: &mut Contexts,
        f: &mut u32,
        x: usize,
        k: u32,
        value: u32,
        zc: &[u8; 512],
    ) {
        let nb = (*f >> (3 * k)) & 0x1FF;
        if nb & 0x10 != 0 || nb & NEIGHBOURS == 0 {
            return;
        }
        if src.decision(cx, usize::from(zc[nb as usize])) != 0 {
            self.newly(src, cx, f, x, k, value);
        }
        *f |= 1 << (28 + k);
    }

    /// Magnitude refinement (D.3.3).
    #[inline(always)]
    fn refinement<S: Source, const FULL: bool>(&mut self, src: &mut S, cx: &mut Contexts, bp: u32) {
        let half = 1u32 << bp;
        for x in 0..self.w {
            let mut f = self.cur[x + 1];
            if f & SIG_OWN == 0 {
                continue;
            }
            if FULL {
                self.refinement_at(src, cx, &mut f, x, 0, half);
                self.refinement_at(src, cx, &mut f, x, 1, half);
                self.refinement_at(src, cx, &mut f, x, 2, half);
                self.refinement_at(src, cx, &mut f, x, 3, half);
            } else {
                for k in 0..self.height() {
                    self.refinement_at(src, cx, &mut f, x, k, half);
                }
            }
            self.cur[x + 1] = f;
        }
    }

    #[inline(always)]
    fn refinement_at<S: Source>(
        &mut self,
        src: &mut S,
        cx: &mut Contexts,
        f: &mut u32,
        x: usize,
        k: u32,
        half: u32,
    ) {
        let nb = *f >> (3 * k);
        if (nb >> 4) & !(*f >> (28 + k)) & 1 == 0 {
            return;
        }
        let ctx = if (*f >> (24 + k)) & 1 != 0 {
            CTX_MAG + 2
        } else {
            CTX_MAG + usize::from(nb & NEIGHBOURS != 0)
        };
        let bit = src.decision(cx, ctx);
        // The sample holds the midpoint of the plane above: move it by half a
        // step of this plane, up for a 1 and down for a 0.
        let v = &mut self.rows[k as usize * self.w + x];
        *v = v
            .wrapping_add(half)
            .wrapping_sub((2 * half) & bit.wrapping_sub(1));
        *f |= 1 << (24 + k);
    }

    /// Cleanup (D.3.4).
    #[inline(always)]
    fn cleanup<const FULL: bool>(
        &mut self,
        src: &mut Mq,
        cx: &mut Contexts,
        bp: u32,
        zc: &[u8; 512],
    ) {
        let value = 3 << bp;
        for x in 0..self.w {
            let mut f = self.cur[x + 1];
            if !FULL {
                for k in 0..self.height() {
                    self.cleanup_at(src, cx, &mut f, x, k, value, zc);
                }
            } else if f & (SIG_ALL | PI_ALL) == 0 {
                if src.decode(cx, CTX_RUN) == 0 {
                    continue;
                }
                let mut k = src.decode(cx, CTX_UNI) << 1;
                k |= src.decode(cx, CTX_UNI);
                self.newly(src, cx, &mut f, x, k, value);
                while k < 3 {
                    k += 1;
                    self.cleanup_at(src, cx, &mut f, x, k, value, zc);
                }
            } else {
                self.cleanup_at(src, cx, &mut f, x, 0, value, zc);
                self.cleanup_at(src, cx, &mut f, x, 1, value, zc);
                self.cleanup_at(src, cx, &mut f, x, 2, value, zc);
                self.cleanup_at(src, cx, &mut f, x, 3, value, zc);
            }
            self.cur[x + 1] = f & !PI_ALL;
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn cleanup_at(
        &mut self,
        src: &mut Mq,
        cx: &mut Contexts,
        f: &mut u32,
        x: usize,
        k: u32,
        value: u32,
        zc: &[u8; 512],
    ) {
        let nb = (*f >> (3 * k)) & 0x1FF;
        if ((nb >> 4) | (*f >> (28 + k))) & 1 == 0
            && src.decode(cx, usize::from(zc[nb as usize])) != 0
        {
            self.newly(src, cx, f, x, k, value);
        }
    }
}

impl T1 {
    /// Decode a block of `w` x `h` samples with `planes` magnitude bit-planes
    /// (1 to [`MAX_PLANES`]). `class` is 0 for LL/LH, 1 for HL, 2 for HH.
    #[allow(clippy::too_many_arguments)]
    pub fn decode(
        &mut self,
        w: usize,
        h: usize,
        planes: u32,
        class: usize,
        style: u8,
        bytes: &[u8],
        segments: &[Segment],
    ) {
        self.w = w;
        self.causal = style & STYLE_CAUSAL != 0;
        self.data.clear();
        self.data.resize(w * h, 0);
        self.flags.clear();
        self.flags.resize((w + 2) * (h.div_ceil(4) + 2), 0);
        let zc = &ZC[class];
        let mut cx = Contexts::new();
        let mut pass = 0u32;
        let total = planes * 3 - 2;
        for seg in segments {
            let bytes = &bytes[seg.start..seg.end];
            let mut mq = Mq::new(bytes);
            let mut raw = Raw::new(bytes);
            for _ in 0..seg.passes {
                if pass >= total {
                    return;
                }
                // Pass 0 is the cleanup of the top plane; then three a plane.
                let bp = planes - 1 - pass.div_ceil(3);
                let is_raw = style & STYLE_BYPASS != 0 && pass >= 10;
                match (pass + 2) % 3 {
                    0 if is_raw => self.significance(&mut raw, &mut cx, bp, zc),
                    0 => self.significance(&mut mq, &mut cx, bp, zc),
                    1 if is_raw => self.refinement(&mut raw, &mut cx, bp),
                    1 => self.refinement(&mut mq, &mut cx, bp),
                    _ => {
                        self.cleanup(&mut mq, &mut cx, bp, zc);
                        if style & STYLE_SEGSYM != 0 {
                            for _ in 0..4 {
                                mq.decode(&mut cx, CTX_UNI);
                            }
                        }
                    }
                }
                if style & STYLE_RESET != 0 {
                    cx = Contexts::new();
                }
                pass += 1;
            }
        }
    }

    /// The block's stripes, top to bottom, and whether each has four rows.
    #[inline(always)]
    fn stripes(&mut self, mut body: impl FnMut(Stripe, bool)) {
        let fw = self.w + 2;
        for (i, rows) in self.data.chunks_mut(4 * self.w).enumerate() {
            let (up, rest) = self.flags[i * fw..(i + 3) * fw].split_at_mut(fw);
            let (cur, down) = rest.split_at_mut(fw);
            let full = rows.len() == 4 * self.w;
            let st = Stripe {
                up,
                cur,
                down,
                rows,
                w: self.w,
                causal: self.causal,
            };
            body(st, full);
        }
    }

    // The passes work on a local copy of the coder, so that its registers are
    // the machine's.

    fn significance<S: Source>(&mut self, src: &mut S, cx: &mut Contexts, bp: u32, zc: &[u8; 512]) {
        let mut s = *src;
        self.stripes(|mut st, full| {
            if full {
                st.significance::<S, true>(&mut s, cx, bp, zc);
            } else {
                st.significance::<S, false>(&mut s, cx, bp, zc);
            }
        });
        *src = s;
    }

    fn refinement<S: Source>(&mut self, src: &mut S, cx: &mut Contexts, bp: u32) {
        let mut s = *src;
        self.stripes(|mut st, full| {
            if full {
                st.refinement::<S, true>(&mut s, cx, bp);
            } else {
                st.refinement::<S, false>(&mut s, cx, bp);
            }
        });
        *src = s;
    }

    fn cleanup(&mut self, src: &mut Mq, cx: &mut Contexts, bp: u32, zc: &[u8; 512]) {
        let mut s = *src;
        self.stripes(|mut st, full| {
            if full {
                st.cleanup::<true>(&mut s, cx, bp, zc);
            } else {
                st.cleanup::<false>(&mut s, cx, bp, zc);
            }
        });
        *src = s;
    }
}

/// A sign-magnitude coefficient (times two) as a two's complement integer.
#[inline(always)]
pub(crate) fn signed2(v: u32) -> i32 {
    let m = (v & 0x7FFF_FFFF) as i32;
    let s = (v as i32) >> 31;
    (m ^ s) - s
}
