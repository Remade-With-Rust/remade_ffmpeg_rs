//! Tier-1: code-block coefficient decoding (ISO/IEC 15444-1 Annex D).
//!
//! State is kept per *stripe column* (four vertically adjacent samples) in one
//! `u32`, so that the common "nothing to do here" test covers four samples:
//!
//! | bits  | meaning |
//! |-------|---------|
//! | 0-17  | significance of the 6 x 3 samples around the column: row `i` (0 is the row above, 1-4 the column's own), column `j` (0 left, 1 own, 2 right) at bit `3 * i + j` |
//! | 18 + 3 i | sign of row `i` of the own column (row 5 at bit 31) |
//! | 19 + 3 k | sample `k` was coded in this bit-plane's significance pass |
//! | 20 + 3 k | sample `k` has been refined before |
//!
//! The 3 x 3 neighbourhood of sample `k` is then `(flags >> 3 * k) & 0x1FF`,
//! and a property of all four samples is one mask ([`EACH`]).

use std::cell::Cell;

use crate::mq::{Contexts, Ctx, Mq, Raw};

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
/// One bit for each sample of a column.
const EACH: u32 = 0o1111;
const PI: u32 = 19;
const PI_ALL: u32 = EACH << PI;
const MU: u32 = 20;
const NEIGHBOURS: u32 = 0x1EF;

/// Zero-coding contexts (Table D.1) by neighbourhood, for each of the three
/// subband classes (LL/LH, HL, HH).
static ZC: [[Ctx; 512]; 3] = {
    let mut t = [[Ctx::C0; 512]; 3];
    let mut v = 0;
    while v < 512 {
        let h = ((v >> 3) & 1) + ((v >> 5) & 1);
        let w = ((v >> 1) & 1) + ((v >> 7) & 1);
        let d = (v & 1) + ((v >> 2) & 1) + ((v >> 6) & 1) + ((v >> 8) & 1);
        t[0][v] = Ctx::new(zc_lh(h, w, d) as usize);
        t[1][v] = Ctx::new(zc_lh(w, h, d) as usize);
        t[2][v] = Ctx::new(zc_hh(h + w, d) as usize);
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

/// Sign contexts (Tables D.2, D.3): index bits are (negative, significant)
/// for up, left, right, down; the value is the context and
/// whether the decision is inverted.
static SC: [(Ctx, u8); 256] = {
    let mut t = [(Ctx::C0, 0); 256];
    let mut v = 0;
    while v < 256 {
        let mut s = [0i32; 4];
        let mut i = 0;
        while i < 4 {
            if (v >> (2 * i + 1)) & 1 != 0 {
                s[i] = 1 - 2 * ((v as i32 >> (2 * i)) & 1);
            }
            i += 1;
        }
        let [up, left, right, down] = s;
        let h = clamp1(left + right);
        let w = clamp1(up + down);
        let (flip, h, w) = if h < 0 || (h == 0 && w < 0) {
            (1, -h, -w)
        } else {
            (0, h, w)
        };
        let ctx = if h == 0 { 9 + w } else { 12 + w };
        t[v] = (Ctx::new(ctx as usize), flip);
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
    fn decision(&mut self, cx: &mut Contexts, ctx: Ctx) -> u32;
    /// A sign, given its [`SC`] entry.
    fn sign(&mut self, cx: &mut Contexts, sc: (Ctx, u8)) -> u32;
}

impl Source for Mq<'_> {
    #[inline(always)]
    fn decision(&mut self, cx: &mut Contexts, ctx: Ctx) -> u32 {
        self.decode(cx, ctx)
    }
    #[inline(always)]
    fn sign(&mut self, cx: &mut Contexts, sc: (Ctx, u8)) -> u32 {
        self.decode(cx, sc.0) ^ u32::from(sc.1)
    }
}

impl Source for Raw<'_> {
    #[inline(always)]
    fn decision(&mut self, _: &mut Contexts, _: Ctx) -> u32 {
        self.bit()
    }
    #[inline(always)]
    fn sign(&mut self, _: &mut Contexts, _: (Ctx, u8)) -> u32 {
        self.bit()
    }
}

/// One codeword segment of a code-block.
pub(crate) struct Segment {
    /// Coding passes in the segment.
    pub passes: u32,
    /// Its bytes in the data.
    pub start: usize,
    pub end: usize,
}

/// Reusable code-block decoder.
#[derive(Default)]
pub(crate) struct T1 {
    /// Decoded coefficients, sign in bit 31, below it the magnitude times two
    /// with the reconstruction half-step in place. The order is that of the
    /// scan: stripes of four rows, in them column by column (the last stripe
    /// is filled up with zeros).
    pub data: Vec<u32>,
    flags: Vec<u32>,
    w: usize,
    h: usize,
    causal: bool,
}

/// What is around one stripe column: the flag words of it and its left and
/// right neighbours, in its own stripe and the ones above and below, and its
/// samples.
struct Place<'a> {
    up: &'a [Cell<u32>; 3],
    cur: &'a [Cell<u32>; 3],
    down: &'a [Cell<u32>; 3],
    samples: &'a mut [u32; 4],
    causal: bool,
}

#[inline(always)]
fn set(word: &Cell<u32>, bits: u32) {
    word.set(word.get() | bits);
}

impl Place<'_> {
    /// Sign-coding table entry of sample `k` (`f` is the flag word).
    #[inline(always)]
    fn sign_entry(&self, f: u32, k: u32) -> (Ctx, u8) {
        let down = if k == 3 {
            (f >> 25) & 0x40
        } else {
            (f >> (18 + 3 * k)) & 0x40
        };
        let idx = ((f >> (3 * k)) & 0xAA)
            | ((f >> (18 + 3 * k)) & 1)
            | ((self.cur[0].get() >> (19 + 3 * k)) & 4)
            | ((self.cur[2].get() >> (17 + 3 * k)) & 0x10)
            | down;
        SC[(idx & 255) as usize]
    }

    /// Decode the sign of sample `k` and mark it significant.
    #[inline(always)]
    fn newly<S: Source>(
        &mut self,
        src: &mut S,
        cx: &mut Contexts,
        f: &mut u32,
        k: u32,
        value: u32,
    ) {
        let s = src.sign(cx, self.sign_entry(*f, k));
        self.samples[k as usize & 3] = value | (s << 31);
        *f |= (0x10 << (3 * k)) | (s << (21 + 3 * k));
        set(&self.cur[0], 0x20 << (3 * k));
        set(&self.cur[2], 0x08 << (3 * k));
        // Vertically causal contexts: the stripe above never sees this one.
        if k == 0 && !self.causal {
            set(&self.up[0], 1 << 17);
            set(&self.up[1], (1 << 16) | (s << 31));
            set(&self.up[2], 1 << 15);
        }
        if k == 3 {
            set(&self.down[0], 1 << 2);
            set(&self.down[1], (1 << 1) | (s << 18));
            set(&self.down[2], 1);
        }
    }

    /// Significance propagation (D.3.1) of the column's first `h` samples.
    #[inline(always)]
    fn significance<S: Source>(
        &mut self,
        src: &mut S,
        cx: &mut Contexts,
        bp: u32,
        zc: &[Ctx; 512],
        h: u32,
    ) {
        let mut f = self.cur[1].get();
        if f & SIG_ALL == 0 {
            return;
        }
        // Samples with a significant neighbour, not significant themselves.
        let side = f | (f >> 2);
        let row = side | (f >> 1);
        let fresh = !(f >> 4) & EACH;
        let mut m = (row | (side >> 3) | (row >> 6)) & fresh;
        if m == 0 {
            return;
        }
        let value = 3 << bp;
        macro_rules! at {
            ($k:expr) => {
                if m & (1 << (3 * $k)) != 0 {
                    let nb = (f >> (3 * $k)) & 0x1FF;
                    if src.decision(cx, zc[nb as usize]) != 0 {
                        self.newly(src, cx, &mut f, $k, value);
                        m |= (8 << (3 * $k)) & fresh;
                    }
                    f |= 1 << (PI + 3 * $k);
                }
            };
        }
        // Unrolled by hand: the compiler leaves it a loop.
        at!(0);
        if h > 1 {
            at!(1);
        }
        if h > 2 {
            at!(2);
        }
        if h > 3 {
            at!(3);
        }
        // The last sample has no successor to update it for.
        let _ = m;
        self.cur[1].set(f);
    }

    /// Magnitude refinement (D.3.3).
    #[inline(always)]
    fn refinement<S: Source>(&mut self, src: &mut S, cx: &mut Contexts, bp: u32) {
        let mut f = self.cur[1].get();
        let m = (f >> 4) & !(f >> PI) & EACH;
        if m == 0 {
            return;
        }
        let half = 1u32 << bp;
        macro_rules! at {
            ($k:expr) => {
                if m & (1 << (3 * $k)) != 0 {
                    let ctx = if (f >> (MU + 3 * $k)) & 1 != 0 {
                        Ctx::C16
                    } else if (f >> (3 * $k)) & NEIGHBOURS != 0 {
                        Ctx::C15
                    } else {
                        Ctx::C14
                    };
                    let bit = src.decision(cx, ctx);
                    // The sample holds the midpoint of the plane above: move it by
                    // half a step of this plane, up for a 1 and down for a 0.
                    let v = &mut self.samples[$k as usize];
                    *v = v
                        .wrapping_add(half)
                        .wrapping_sub((2 * half) & bit.wrapping_sub(1));
                    f |= 1 << (MU + 3 * $k);
                }
            };
        }
        at!(0);
        at!(1);
        at!(2);
        at!(3);
        self.cur[1].set(f);
    }

    /// Cleanup (D.3.4) of the column's first `h` samples.
    #[inline(always)]
    fn cleanup(&mut self, src: &mut Mq, cx: &mut Contexts, bp: u32, zc: &[Ctx; 512], h: u32) {
        let mut f = self.cur[1].get();
        let value = 3 << bp;
        let m = if f & (SIG_ALL | PI_ALL) == 0 && h == 4 {
            if src.decode(cx, Ctx::Run) == 0 {
                return;
            }
            let mut k = src.decode(cx, Ctx::Uni) << 1;
            k |= src.decode(cx, Ctx::Uni);
            self.newly(src, cx, &mut f, k, value);
            (0o1110 << (3 * k)) & EACH
        } else {
            !((f >> 4) | (f >> PI)) & EACH
        };
        if m != 0 {
            macro_rules! at {
                ($k:expr) => {
                    if m & (1 << (3 * $k)) != 0 {
                        let nb = (f >> (3 * $k)) & 0x1FF;
                        if src.decode(cx, zc[nb as usize]) != 0 {
                            self.newly(src, cx, &mut f, $k, value);
                        }
                    }
                };
            }
            at!(0);
            if h > 1 {
                at!(1);
            }
            if h > 2 {
                at!(2);
            }
            if h > 3 {
                at!(3);
            }
        }
        self.cur[1].set(f & !PI_ALL);
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
        self.h = h;
        self.causal = style & STYLE_CAUSAL != 0;
        self.data.clear();
        self.data.resize(w * h.next_multiple_of(4), 0);
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
                                mq.decode(&mut cx, Ctx::Uni);
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

    /// Visit the block's stripe columns, in scan order, with the number of
    /// their rows.
    #[inline(always)]
    fn columns(&mut self, mut body: impl FnMut(Place, u32)) {
        let fw = self.w + 2;
        let causal = self.causal;
        let flags = Cell::from_mut(&mut self.flags[..]).as_slice_of_cells();
        for (i, stripe) in self.data.chunks_exact_mut(4 * self.w).enumerate() {
            let [up, cur, down] = [0, 1, 2].map(|j| flags[(i + j) * fw..][..fw].windows(3));
            let rows = self.h - 4 * i;
            let places = stripe.chunks_exact_mut(4).zip(cur).zip(up).zip(down);
            let places = places.map(|(((samples, cur), up), down)| Place {
                up: three(up),
                cur: three(cur),
                down: three(down),
                samples: match samples.first_chunk_mut() {
                    Some(s) => s,
                    None => unreachable!("chunks of four"),
                },
                causal,
            });
            if rows >= 4 {
                places.for_each(|p| body(p, 4));
            } else {
                places.for_each(|p| body(p, rows as u32));
            }
        }
    }

    // The passes work on a local copy of the coder, so that its registers are
    // the machine's.

    fn significance<S: Source>(
        &mut self,
        src: &mut S,
        cx: &mut Contexts,
        bp: u32,
        zc: &[Ctx; 512],
    ) {
        let mut s = *src;
        self.columns(
            #[inline(always)]
            |mut p, h| p.significance(&mut s, cx, bp, zc, h),
        );
        *src = s;
    }

    fn refinement<S: Source>(&mut self, src: &mut S, cx: &mut Contexts, bp: u32) {
        let mut s = *src;
        self.columns(
            #[inline(always)]
            |mut p, _| p.refinement(&mut s, cx, bp),
        );
        *src = s;
    }

    fn cleanup(&mut self, src: &mut Mq, cx: &mut Contexts, bp: u32, zc: &[Ctx; 512]) {
        let mut s = *src;
        self.columns(
            #[inline(always)]
            |mut p, h| p.cleanup(&mut s, cx, bp, zc, h),
        );
        *src = s;
    }
}

#[inline(always)]
fn three(window: &[Cell<u32>]) -> &[Cell<u32>; 3] {
    match window.first_chunk() {
        Some(w) => w,
        None => unreachable!("windows of three"),
    }
}
