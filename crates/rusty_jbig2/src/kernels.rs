//! The bitmap combination kernels, and the only `unsafe` of the crate.
//!
//! [`Narrow::run`] checks once that its rows are inside both buffers and then
//! goes over them with raw pointers: a bounds check per row and buffer is two
//! thirds of the work of a symbol's row. The safe scalar form of the same
//! thing is [`Narrow::row_at_end`], which also takes the rows that the check
//! leaves out; the tests compare both with single pixels.
#![allow(unsafe_code)]

/// The word a combination operator works on.
pub(crate) trait Word:
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
pub(crate) trait Combine {
    /// Whether clear padding bits combine to set ones.
    const SETS_PADDING: bool = false;

    fn f<T: Word>(d: T, s: T, m: T) -> T;
}

pub(crate) struct Or;
pub(crate) struct And;
pub(crate) struct Xor;
pub(crate) struct Xnor;
pub(crate) struct Replace;

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
pub(crate) struct Narrow {
    pub(crate) rows: usize,
    /// From the source's bit position to the destination's, to the left
    /// (below 64). Any bit this takes around the end is outside the mask.
    pub(crate) rotate: u32,
    /// The destination bits drawn to.
    pub(crate) mask: u64,
}

impl Narrow {
    /// Rows start at byte `d0` of `dst` (at least 8 bytes) and `s0` of `src`
    /// and follow in steps of their strides.
    #[inline(never)]
    pub(crate) fn run<C: Combine>(
        &self,
        (dst, dstride, mut d0): (&mut [u8], usize, usize),
        (src, sstride, mut s0): (&[u8], usize, usize),
    ) {
        let mut pad = [0u8; 72];
        let src = if src.len() <= 64 {
            // Every row of a small source gets a whole word to load.
            // A pattern's usual size without a call.
            if let Some(w) = src.first_chunk::<8>().filter(|_| src.len() == 8) {
                pad[..8].copy_from_slice(w);
            } else {
                pad[..src.len()].copy_from_slice(src);
            }
            &pad
        } else {
            src
        };
        // Rows with a whole word to load and store come first.
        let n = whole_words(self.rows, dst.len(), d0, dstride);
        let n = whole_words(n, src.len(), s0, sstride);
        let rotate = self.rotate;
        // The operators are bitwise, so only the source needs swapping.
        let m = self.mask.to_be();
        for _ in 0..n {
            // SAFETY: row `i < n` of each buffer is the 8 bytes at its start
            // plus `i` strides. `whole_words` has checked, without overflow,
            // that those of row `n - 1` end inside the buffer, and the
            // offsets of the rows before it are no larger. `dst` is borrowed
            // mutably and `src` is not part of it. `[u8; 8]` is unaligned.
            let (d, s) = unsafe {
                (
                    &mut *dst.as_mut_ptr().add(d0).cast::<[u8; 8]>(),
                    &*src.as_ptr().add(s0).cast::<[u8; 8]>(),
                )
            };
            let v = u64::from_be_bytes(*s).rotate_left(rotate).to_be();
            *d = C::f(u64::from_ne_bytes(*d), v, m).to_ne_bytes();
            d0 = d0.wrapping_add(dstride);
            s0 = s0.wrapping_add(sstride);
        }
        for _ in n..self.rows {
            self.row_at_end::<C>(dst, d0, src, s0);
            d0 += dstride;
            s0 += sstride;
        }
    }

    /// Any row, less than a word from the end of the buffers included: the
    /// word there or the last one, shifted.
    fn row_at_end<C: Combine>(&self, dst: &mut [u8], d0: usize, src: &[u8], s0: usize) {
        let (Some(dend), Some(send)) = (dst.len().checked_sub(8), src.len().checked_sub(8)) else {
            return;
        };
        let (dat, sat) = (d0.min(dend), s0.min(send));
        let d = dst.get_mut(dat..).and_then(<[u8]>::first_chunk_mut::<8>);
        let s = src.get(sat..).and_then(<[u8]>::first_chunk::<8>);
        if let (Some(d), Some(s)) = (d, s) {
            let k = (d0 - dat) * 8;
            let v = (u64::from_be_bytes(*s) << ((s0 - sat) * 8)).rotate_left(self.rotate);
            *d = C::f(u64::from_be_bytes(*d), v >> k, self.mask >> k).to_be_bytes();
        }
    }
}

/// How many of `rows` starting at `at` in steps of `stride` begin with 8
/// bytes of a buffer of `len`. All but the last few do.
#[inline]
fn whole_words(rows: usize, len: usize, at: usize, stride: usize) -> usize {
    let fits = |n: usize| {
        (n - 1)
            .checked_mul(stride)
            .and_then(|v| v.checked_add(at))
            .and_then(|v| v.checked_add(8))
            .is_some_and(|end| end <= len)
    };
    let mut n = rows;
    while n > 0 && !fits(n) {
        n -= 1;
    }
    n
}

/// A draw of rows of several 64-bit words.
pub(crate) struct Words {
    count: usize,
    head: u64,
    tail: u64,
    sh: u32,
}

impl Words {
    /// Rows from bit `lead` (below 8) to bit `end` of the destination, the
    /// source beginning at bit `sh` (below 8).
    pub(crate) fn new(lead: u32, end: i64, sh: u32) -> Self {
        Self {
            count: (end as u64).div_ceil(64) as usize,
            head: !0 >> lead,
            tail: !0 << ((end as u64).wrapping_neg() & 63),
            sh,
        }
    }

    /// Draw a row, unless less than its words and a byte are left.
    #[inline]
    pub(crate) fn row<C: Combine>(&self, d: &mut [u8], s: &[u8]) -> bool {
        let bytes = self.count * 8;
        let (Some(d), Some(s)) = (d.get_mut(..bytes), s.get(..=bytes)) else {
            return false;
        };
        let after = s[8..].iter().step_by(8);
        let mut m = self.head;
        let mut left = self.count;
        for ((d, s), a) in d.chunks_exact_mut(8).zip(s.chunks_exact(8)).zip(after) {
            if let (Some(d), Some(s)) = (d.first_chunk_mut::<8>(), s.first_chunk::<8>()) {
                left -= 1;
                if left == 0 {
                    m &= self.tail;
                }
                let v = (u64::from_be_bytes(*s) << self.sh) | (u64::from(*a) >> (8 - self.sh));
                *d = C::f(u64::from_be_bytes(*d), v, m).to_be_bytes();
                m = !0;
            }
        }
        true
    }
}

/// Any other draw, in bytes: the source's shifted ones between two masked
/// ones of the destination.
pub(crate) struct Wide {
    /// The source byte the first destination byte begins in, -1 if that is
    /// before the row, and the bit it begins at.
    pub(crate) first: i64,
    pub(crate) sh: u32,
    pub(crate) head: u8,
    pub(crate) tail: u8,
}

impl Wide {
    pub(crate) fn row<C: Combine>(&self, d: &mut [u8], s: &[u8]) {
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

    /// The pointer loop against its safe form, on buffers that end anywhere.
    #[test]
    fn narrow_matches_its_safe_rows() {
        let mut seed = 17u32;
        let mut byte = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };
        for (rows, dstride, sstride, d0, s0, short) in [
            (9usize, 1usize, 1usize, 0usize, 0usize, 0usize),
            (9, 8, 8, 0, 0, 0),
            (20, 9, 5, 3, 1, 0),
            (20, 2, 9, 1, 1, 0),
            (30, 40, 3, 35, 0, 0),
            (30, 40, 3, 35, 0, 5),
            (1, 8, 1, 0, 0, 0),
            (0, 8, 8, 0, 0, 0),
        ] {
            let dlen = (d0 + rows * dstride).max(8);
            let slen = (s0 + rows * sstride).saturating_sub(short);
            let dst: Vec<u8> = (0..dlen).map(|_| byte()).collect();
            let src: Vec<u8> = (0..slen).map(|_| byte()).collect();
            for rotate in [0, 1, 7, 33, 63] {
                let w = Narrow {
                    rows,
                    rotate,
                    // What a byte-aligned row of one byte has.
                    mask: 0xFF << 56,
                };
                let mut fast = dst.clone();
                w.run::<Xor>((&mut fast, dstride, d0), (&src, sstride, s0));
                let mut safe = dst.clone();
                let mut padded = src.clone();
                padded.resize(padded.len() + 8, 0);
                if src.len() > 64 {
                    padded.truncate(src.len());
                }
                for i in 0..rows {
                    w.row_at_end::<Xor>(&mut safe, d0 + i * dstride, &padded, s0 + i * sstride);
                }
                assert_eq!(fast, safe, "{rows} {dstride} {sstride} {rotate}");
            }
        }
    }

    #[test]
    fn whole_words_is_exact() {
        for len in 0..60 {
            for at in 0..20 {
                for stride in 0..12 {
                    for rows in 0..9usize {
                        let want = (0..rows).take_while(|i| at + i * stride + 8 <= len).count();
                        assert_eq!(whole_words(rows, len, at, stride), want);
                    }
                }
            }
        }
        assert_eq!(whole_words(3, 100, 0, usize::MAX / 2 + 1), 1);
        assert_eq!(whole_words(1, 100, usize::MAX - 2, 1), 0);
    }
}
