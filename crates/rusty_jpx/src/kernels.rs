//! Sample kernels: wavelet lifting, colour transforms, dequantization and the
//! final level shift.
//!
//! Each kernel is written once over [`Isa`], a handful of 32-bit lane
//! operations, and instantiated for AVX2 (detected at run time), the
//! architecture's baseline vectors (SSE2, NEON) and one scalar lane. The
//! scalar instance is the portable fallback and the oracle of the tests: all
//! instances perform the same operations in the same order (no fused
//! multiply-add), so their results are identical to the bit.
//!
//! Reals travel as the bits of an `f32` in `i32` storage, which lets a tile's
//! coefficient buffer become the output buffer.
//!
//! The only `unsafe` code of the crate is here. The public functions are safe:
//! they check every length that the pointer arithmetic relies on.
#![allow(unsafe_code)]

use std::marker::PhantomData;

/// 1.5 * 2^23: adding it leaves an integer of up to 22 bits in the low
/// mantissa bits, rounded to nearest, halves to even.
const ROUND: f32 = 12_582_912.0;

const ALPHA: f32 = -1.586_134_3;
const BETA: f32 = -0.052_980_12;
const GAMMA: f32 = 0.882_911_1;
const DELTA: f32 = 0.443_506_85;
const K: f32 = 1.230_174_1;

/// 32-bit lanes; reals are the same lanes, reinterpreted.
///
/// # Safety
///
/// The operations may only be called where the instruction set they stand for
/// is available; `load` and `store` access `N` lanes, without alignment.
trait Isa: Copy {
    type R: Copy;
    const N: usize;
    unsafe fn load(p: *const i32) -> Self::R;
    unsafe fn store(p: *mut i32, v: Self::R);
    unsafe fn splat(v: i32) -> Self::R;
    unsafe fn add(a: Self::R, b: Self::R) -> Self::R;
    unsafe fn sub(a: Self::R, b: Self::R) -> Self::R;
    /// Arithmetic shift right.
    unsafe fn sra<const S: i32>(a: Self::R) -> Self::R;
    /// Signed.
    unsafe fn min(a: Self::R, b: Self::R) -> Self::R;
    unsafe fn max(a: Self::R, b: Self::R) -> Self::R;
    /// Sign and magnitude (bit 31, bits 0-30) to two's complement.
    unsafe fn signed(v: Self::R) -> Self::R;
    /// The same with the magnitude halved, rounding towards zero.
    unsafe fn signed_half(v: Self::R) -> Self::R;
    unsafe fn addf(a: Self::R, b: Self::R) -> Self::R;
    unsafe fn subf(a: Self::R, b: Self::R) -> Self::R;
    unsafe fn mulf(a: Self::R, b: Self::R) -> Self::R;
    /// `a` limited to `lo..=hi`; `lo` when `a` is not a number.
    unsafe fn clampf(a: Self::R, lo: Self::R, hi: Self::R) -> Self::R;
    /// Integer to real.
    unsafe fn to_real(a: Self::R) -> Self::R;
    /// `N` consecutive columns of four samples, as four rows.
    unsafe fn load_columns(p: *const i32) -> [Self::R; 4];
    /// `a[0] b[0] a[1] b[1] ..`, low and high half.
    unsafe fn zip(a: Self::R, b: Self::R) -> (Self::R, Self::R);
    #[inline(always)]
    unsafe fn splatf(v: f32) -> Self::R {
        Self::splat(real(v))
    }
}

#[inline(always)]
fn real(v: f32) -> i32 {
    v.to_bits() as i32
}

#[inline(always)]
fn float(bits: i32) -> f32 {
    f32::from_bits(bits as u32)
}

#[derive(Clone, Copy)]
struct Scalar;

impl Isa for Scalar {
    type R = i32;
    const N: usize = 1;
    #[inline(always)]
    unsafe fn load(p: *const i32) -> i32 {
        *p
    }
    #[inline(always)]
    unsafe fn store(p: *mut i32, v: i32) {
        *p = v;
    }
    #[inline(always)]
    unsafe fn splat(v: i32) -> i32 {
        v
    }
    #[inline(always)]
    unsafe fn add(a: i32, b: i32) -> i32 {
        a.wrapping_add(b)
    }
    #[inline(always)]
    unsafe fn sub(a: i32, b: i32) -> i32 {
        a.wrapping_sub(b)
    }
    #[inline(always)]
    unsafe fn sra<const S: i32>(a: i32) -> i32 {
        a >> S
    }
    #[inline(always)]
    unsafe fn min(a: i32, b: i32) -> i32 {
        a.min(b)
    }
    #[inline(always)]
    unsafe fn max(a: i32, b: i32) -> i32 {
        a.max(b)
    }
    #[inline(always)]
    unsafe fn signed(v: i32) -> i32 {
        let s = v >> 31;
        ((v & 0x7FFF_FFFF) ^ s).wrapping_sub(s)
    }
    #[inline(always)]
    unsafe fn signed_half(v: i32) -> i32 {
        let s = v >> 31;
        (((v & 0x7FFF_FFFF) >> 1) ^ s).wrapping_sub(s)
    }
    #[inline(always)]
    unsafe fn addf(a: i32, b: i32) -> i32 {
        real(float(a) + float(b))
    }
    #[inline(always)]
    unsafe fn subf(a: i32, b: i32) -> i32 {
        real(float(a) - float(b))
    }
    #[inline(always)]
    unsafe fn mulf(a: i32, b: i32) -> i32 {
        real(float(a) * float(b))
    }
    #[inline(always)]
    unsafe fn clampf(a: i32, lo: i32, hi: i32) -> i32 {
        real(float(a).max(float(lo)).min(float(hi)))
    }
    #[inline(always)]
    unsafe fn to_real(a: i32) -> i32 {
        real(a as f32)
    }
    #[inline(always)]
    unsafe fn load_columns(p: *const i32) -> [i32; 4] {
        [*p, *p.add(1), *p.add(2), *p.add(3)]
    }
    #[inline(always)]
    unsafe fn zip(a: i32, b: i32) -> (i32, i32) {
        (a, b)
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::Isa;
    use std::arch::x86_64::*;

    #[derive(Clone, Copy)]
    pub(super) struct Sse2;

    impl Isa for Sse2 {
        type R = __m128i;
        const N: usize = 4;
        #[inline(always)]
        unsafe fn load(p: *const i32) -> __m128i {
            _mm_loadu_si128(p.cast())
        }
        #[inline(always)]
        unsafe fn store(p: *mut i32, v: __m128i) {
            _mm_storeu_si128(p.cast(), v);
        }
        #[inline(always)]
        unsafe fn splat(v: i32) -> __m128i {
            _mm_set1_epi32(v)
        }
        #[inline(always)]
        unsafe fn add(a: __m128i, b: __m128i) -> __m128i {
            _mm_add_epi32(a, b)
        }
        #[inline(always)]
        unsafe fn sub(a: __m128i, b: __m128i) -> __m128i {
            _mm_sub_epi32(a, b)
        }
        #[inline(always)]
        unsafe fn sra<const S: i32>(a: __m128i) -> __m128i {
            _mm_srai_epi32::<S>(a)
        }
        #[inline(always)]
        unsafe fn min(a: __m128i, b: __m128i) -> __m128i {
            let gt = _mm_cmpgt_epi32(a, b);
            _mm_or_si128(_mm_and_si128(gt, b), _mm_andnot_si128(gt, a))
        }
        #[inline(always)]
        unsafe fn max(a: __m128i, b: __m128i) -> __m128i {
            let gt = _mm_cmpgt_epi32(a, b);
            _mm_or_si128(_mm_and_si128(gt, a), _mm_andnot_si128(gt, b))
        }
        #[inline(always)]
        unsafe fn signed(v: __m128i) -> __m128i {
            let s = _mm_srai_epi32::<31>(v);
            let m = _mm_srli_epi32::<1>(_mm_slli_epi32::<1>(v));
            _mm_sub_epi32(_mm_xor_si128(m, s), s)
        }
        #[inline(always)]
        unsafe fn signed_half(v: __m128i) -> __m128i {
            let s = _mm_srai_epi32::<31>(v);
            let m = _mm_srli_epi32::<2>(_mm_slli_epi32::<1>(v));
            _mm_sub_epi32(_mm_xor_si128(m, s), s)
        }
        #[inline(always)]
        unsafe fn addf(a: __m128i, b: __m128i) -> __m128i {
            _mm_castps_si128(_mm_add_ps(_mm_castsi128_ps(a), _mm_castsi128_ps(b)))
        }
        #[inline(always)]
        unsafe fn subf(a: __m128i, b: __m128i) -> __m128i {
            _mm_castps_si128(_mm_sub_ps(_mm_castsi128_ps(a), _mm_castsi128_ps(b)))
        }
        #[inline(always)]
        unsafe fn mulf(a: __m128i, b: __m128i) -> __m128i {
            _mm_castps_si128(_mm_mul_ps(_mm_castsi128_ps(a), _mm_castsi128_ps(b)))
        }
        #[inline(always)]
        unsafe fn clampf(a: __m128i, lo: __m128i, hi: __m128i) -> __m128i {
            // `maxps` returns its second operand when one is not a number.
            let v = _mm_max_ps(_mm_castsi128_ps(a), _mm_castsi128_ps(lo));
            _mm_castps_si128(_mm_min_ps(v, _mm_castsi128_ps(hi)))
        }
        #[inline(always)]
        unsafe fn to_real(a: __m128i) -> __m128i {
            _mm_castps_si128(_mm_cvtepi32_ps(a))
        }
        #[inline(always)]
        unsafe fn load_columns(p: *const i32) -> [__m128i; 4] {
            let c = [0, 4, 8, 12].map(|i| Self::load(p.add(i)));
            let (a, b) = (
                _mm_unpacklo_epi32(c[0], c[1]),
                _mm_unpackhi_epi32(c[0], c[1]),
            );
            let (d, e) = (
                _mm_unpacklo_epi32(c[2], c[3]),
                _mm_unpackhi_epi32(c[2], c[3]),
            );
            [
                _mm_unpacklo_epi64(a, d),
                _mm_unpackhi_epi64(a, d),
                _mm_unpacklo_epi64(b, e),
                _mm_unpackhi_epi64(b, e),
            ]
        }
        #[inline(always)]
        unsafe fn zip(a: __m128i, b: __m128i) -> (__m128i, __m128i) {
            (_mm_unpacklo_epi32(a, b), _mm_unpackhi_epi32(a, b))
        }
    }

    #[derive(Clone, Copy)]
    pub(super) struct Avx2;

    impl Isa for Avx2 {
        type R = __m256i;
        const N: usize = 8;
        #[inline(always)]
        unsafe fn load(p: *const i32) -> __m256i {
            _mm256_loadu_si256(p.cast())
        }
        #[inline(always)]
        unsafe fn store(p: *mut i32, v: __m256i) {
            _mm256_storeu_si256(p.cast(), v);
        }
        #[inline(always)]
        unsafe fn splat(v: i32) -> __m256i {
            _mm256_set1_epi32(v)
        }
        #[inline(always)]
        unsafe fn add(a: __m256i, b: __m256i) -> __m256i {
            _mm256_add_epi32(a, b)
        }
        #[inline(always)]
        unsafe fn sub(a: __m256i, b: __m256i) -> __m256i {
            _mm256_sub_epi32(a, b)
        }
        #[inline(always)]
        unsafe fn sra<const S: i32>(a: __m256i) -> __m256i {
            _mm256_srai_epi32::<S>(a)
        }
        #[inline(always)]
        unsafe fn min(a: __m256i, b: __m256i) -> __m256i {
            _mm256_min_epi32(a, b)
        }
        #[inline(always)]
        unsafe fn max(a: __m256i, b: __m256i) -> __m256i {
            _mm256_max_epi32(a, b)
        }
        #[inline(always)]
        unsafe fn signed(v: __m256i) -> __m256i {
            _mm256_sign_epi32(_mm256_srli_epi32::<1>(_mm256_slli_epi32::<1>(v)), v)
        }
        #[inline(always)]
        unsafe fn signed_half(v: __m256i) -> __m256i {
            _mm256_sign_epi32(_mm256_srli_epi32::<2>(_mm256_slli_epi32::<1>(v)), v)
        }
        #[inline(always)]
        unsafe fn addf(a: __m256i, b: __m256i) -> __m256i {
            let v = _mm256_add_ps(_mm256_castsi256_ps(a), _mm256_castsi256_ps(b));
            _mm256_castps_si256(v)
        }
        #[inline(always)]
        unsafe fn subf(a: __m256i, b: __m256i) -> __m256i {
            let v = _mm256_sub_ps(_mm256_castsi256_ps(a), _mm256_castsi256_ps(b));
            _mm256_castps_si256(v)
        }
        #[inline(always)]
        unsafe fn mulf(a: __m256i, b: __m256i) -> __m256i {
            let v = _mm256_mul_ps(_mm256_castsi256_ps(a), _mm256_castsi256_ps(b));
            _mm256_castps_si256(v)
        }
        #[inline(always)]
        unsafe fn clampf(a: __m256i, lo: __m256i, hi: __m256i) -> __m256i {
            let v = _mm256_max_ps(_mm256_castsi256_ps(a), _mm256_castsi256_ps(lo));
            _mm256_castps_si256(_mm256_min_ps(v, _mm256_castsi256_ps(hi)))
        }
        #[inline(always)]
        unsafe fn to_real(a: __m256i) -> __m256i {
            _mm256_castps_si256(_mm256_cvtepi32_ps(a))
        }
        #[inline(always)]
        unsafe fn load_columns(p: *const i32) -> [__m256i; 4] {
            // Columns i and i + 4 share a register.
            let c = [0, 4, 8, 12].map(|i| {
                let (l, h) = (_mm_loadu_si128(p.add(i).cast()), p.add(i + 16).cast());
                _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(l), _mm_loadu_si128(h))
            });
            let (a, b) = (
                _mm256_unpacklo_epi32(c[0], c[1]),
                _mm256_unpackhi_epi32(c[0], c[1]),
            );
            let (d, e) = (
                _mm256_unpacklo_epi32(c[2], c[3]),
                _mm256_unpackhi_epi32(c[2], c[3]),
            );
            [
                _mm256_unpacklo_epi64(a, d),
                _mm256_unpackhi_epi64(a, d),
                _mm256_unpacklo_epi64(b, e),
                _mm256_unpackhi_epi64(b, e),
            ]
        }
        #[inline(always)]
        unsafe fn zip(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
            let (l, h) = (_mm256_unpacklo_epi32(a, b), _mm256_unpackhi_epi32(a, b));
            (
                _mm256_permute2x128_si256::<0x20>(l, h),
                _mm256_permute2x128_si256::<0x31>(l, h),
            )
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::Isa;
    use std::arch::aarch64::*;

    #[derive(Clone, Copy)]
    pub(super) struct Neon;

    #[inline(always)]
    unsafe fn ps(v: int32x4_t) -> float32x4_t {
        vreinterpretq_f32_s32(v)
    }

    #[inline(always)]
    unsafe fn si(v: float32x4_t) -> int32x4_t {
        vreinterpretq_s32_f32(v)
    }

    impl Isa for Neon {
        type R = int32x4_t;
        const N: usize = 4;
        #[inline(always)]
        unsafe fn load(p: *const i32) -> int32x4_t {
            vld1q_s32(p)
        }
        #[inline(always)]
        unsafe fn store(p: *mut i32, v: int32x4_t) {
            vst1q_s32(p, v);
        }
        #[inline(always)]
        unsafe fn splat(v: i32) -> int32x4_t {
            vdupq_n_s32(v)
        }
        #[inline(always)]
        unsafe fn add(a: int32x4_t, b: int32x4_t) -> int32x4_t {
            vaddq_s32(a, b)
        }
        #[inline(always)]
        unsafe fn sub(a: int32x4_t, b: int32x4_t) -> int32x4_t {
            vsubq_s32(a, b)
        }
        #[inline(always)]
        unsafe fn sra<const S: i32>(a: int32x4_t) -> int32x4_t {
            vshrq_n_s32::<S>(a)
        }
        #[inline(always)]
        unsafe fn min(a: int32x4_t, b: int32x4_t) -> int32x4_t {
            vminq_s32(a, b)
        }
        #[inline(always)]
        unsafe fn max(a: int32x4_t, b: int32x4_t) -> int32x4_t {
            vmaxq_s32(a, b)
        }
        #[inline(always)]
        unsafe fn signed(v: int32x4_t) -> int32x4_t {
            let s = vshrq_n_s32::<31>(v);
            let m = vandq_s32(v, vdupq_n_s32(0x7FFF_FFFF));
            vsubq_s32(veorq_s32(m, s), s)
        }
        #[inline(always)]
        unsafe fn signed_half(v: int32x4_t) -> int32x4_t {
            let s = vshrq_n_s32::<31>(v);
            let m = vshrq_n_s32::<1>(vandq_s32(v, vdupq_n_s32(0x7FFF_FFFF)));
            vsubq_s32(veorq_s32(m, s), s)
        }
        #[inline(always)]
        unsafe fn addf(a: int32x4_t, b: int32x4_t) -> int32x4_t {
            si(vaddq_f32(ps(a), ps(b)))
        }
        #[inline(always)]
        unsafe fn subf(a: int32x4_t, b: int32x4_t) -> int32x4_t {
            si(vsubq_f32(ps(a), ps(b)))
        }
        #[inline(always)]
        unsafe fn mulf(a: int32x4_t, b: int32x4_t) -> int32x4_t {
            si(vmulq_f32(ps(a), ps(b)))
        }
        #[inline(always)]
        unsafe fn clampf(a: int32x4_t, lo: int32x4_t, hi: int32x4_t) -> int32x4_t {
            // `fmaxnm` returns the number when the other one is not.
            si(vminq_f32(vmaxnmq_f32(ps(a), ps(lo)), ps(hi)))
        }
        #[inline(always)]
        unsafe fn to_real(a: int32x4_t) -> int32x4_t {
            si(vcvtq_f32_s32(a))
        }
        #[inline(always)]
        unsafe fn load_columns(p: *const i32) -> [int32x4_t; 4] {
            let int32x4x4_t(a, b, c, d) = vld4q_s32(p);
            [a, b, c, d]
        }
        #[inline(always)]
        unsafe fn zip(a: int32x4_t, b: int32x4_t) -> (int32x4_t, int32x4_t) {
            (vzip1q_s32(a, b), vzip2q_s32(a, b))
        }
    }
}

// The vectors every processor of this architecture has.
#[cfg(target_arch = "aarch64")]
use neon::Neon as Baseline;
#[cfg(target_arch = "x86_64")]
use x86::Sse2 as Baseline;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
use Scalar as Baseline;

/// Define `$name` to run `$body`, generic over [`Isa`] and always inlined,
/// with the best instruction set at hand. The safety contract is `$body`'s.
macro_rules! dispatch {
    ($name:ident = $body:ident::<$g:ident>($($a:ident: $t:ty),* $(,)?)) => {
        #[allow(clippy::too_many_arguments)]
        unsafe fn $name($($a: $t),*) {
            #[cfg(target_arch = "x86_64")]
            {
                #[target_feature(enable = "avx2")]
                #[allow(clippy::too_many_arguments)]
                unsafe fn avx2($($a: $t),*) {
                    $body::<x86::Avx2, $g>($($a),*);
                }
                if std::is_x86_feature_detected!("avx2") {
                    return avx2($($a),*);
                }
            }
            $body::<Baseline, $g>($($a),*);
        }
    };
}

/// `n` zeros from the allocator's zeroed memory, which for a large buffer is
/// usually fresh pages that need no clearing; `None` when it refuses.
pub(crate) fn zeros(n: usize) -> Option<Vec<i32>> {
    if n == 0 {
        return Some(Vec::new());
    }
    let layout = std::alloc::Layout::array::<i32>(n).ok()?;
    // SAFETY: the layout is not empty; it is the one a vector of `n` values
    // gives back, and they are all initialized.
    unsafe {
        let p = std::alloc::alloc_zeroed(layout).cast::<i32>();
        (!p.is_null()).then(|| Vec::from_raw_parts(p, n, n))
    }
}

// ---------------------------------------------------------------- lifting

/// One lifting step: a sample and its two neighbours.
trait Lift {
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R;
}

struct Update53;
impl Lift for Update53 {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        I::sub(v, I::sra::<2>(I::add(I::add(a, b), I::splat(2))))
    }
}

struct Predict53;
impl Lift for Predict53 {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        I::add(v, I::sra::<1>(I::add(a, b)))
    }
}

/// `v - c * (a + b)`.
#[inline(always)]
unsafe fn minus<I: Isa>(v: I::R, c: f32, a: I::R, b: I::R) -> I::R {
    I::subf(v, I::mulf(I::splatf(c), I::addf(a, b)))
}

/// The first step also scales the low-pass samples.
struct ScaleDelta;
impl Lift for ScaleDelta {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        minus::<I>(I::mulf(v, I::splatf(K)), DELTA, a, b)
    }
}

/// [`ScaleDelta`], scaling the high-pass neighbours as well, and the step
/// after it, scaling its high-pass sample: a row needs no pass for that alone.
struct ScaleAllDelta;
impl Lift for ScaleAllDelta {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        let k = I::splatf(1.0 / K);
        ScaleDelta::apply::<I>(v, I::mulf(a, k), I::mulf(b, k))
    }
}

struct ScaleGamma;
impl Lift for ScaleGamma {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        Gamma::apply::<I>(I::mulf(v, I::splatf(1.0 / K)), a, b)
    }
}

struct Gamma;
impl Lift for Gamma {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        minus::<I>(v, GAMMA, a, b)
    }
}

struct Beta;
impl Lift for Beta {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        minus::<I>(v, BETA, a, b)
    }
}

struct Alpha;
impl Lift for Alpha {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, a: I::R, b: I::R) -> I::R {
        minus::<I>(v, ALPHA, a, b)
    }
}

// ------------------------------------------------------------------- rows

/// An operation on sample `i`, or on the `N` samples from it on.
trait Each {
    unsafe fn at<I: Isa>(&self, i: usize);
}

/// # Safety
///
/// `e` may be applied to every index from `first` up to `end`, which is not
/// less.
#[inline(always)]
unsafe fn each<I: Isa, E: Each>(e: &E, first: usize, end: usize) {
    let n = end - first;
    let mut i = first;
    for _ in 0..n / (2 * I::N) {
        e.at::<I>(i);
        e.at::<I>(i + I::N);
        i += 2 * I::N;
    }
    if n & I::N != 0 {
        e.at::<I>(i);
        i += I::N;
    }
    // Written as a remainder, so that the compiler sees how short it is.
    for i in i..i + (n & (I::N - 1)) {
        e.at::<Scalar>(i);
    }
}

/// `dst[i] = f(t[i], left[i], left[i + 1])`.
struct Step<L>(*mut i32, *const i32, *const i32, PhantomData<L>);
impl<L: Lift> Each for Step<L> {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, i: usize) {
        let (a, b) = (self.2.wrapping_add(i), self.2.wrapping_add(i + 1));
        let v = L::apply::<I>(I::load(self.1.add(i)), I::load(a), I::load(b));
        I::store(self.0.add(i), v);
    }
}

/// Row, high and low: `row[2 i] = f(high[i], low[i - 1], low[i])`, followed by
/// `low[i]`.
struct HighLow<L>(*mut i32, *const i32, *const i32, PhantomData<L>);
impl<L: Lift> Each for HighLow<L> {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, i: usize) {
        let b = I::load(self.2.add(i));
        let v = L::apply::<I>(I::load(self.1.add(i)), I::load(self.2.add(i - 1)), b);
        store_pairs::<I>(self.0.add(2 * i), v, b);
    }
}

/// Row, high and low: `row[2 i] = low[i]`, followed by
/// `f(high[i], low[i], low[i + 1])`.
struct LowHigh<L>(*mut i32, *const i32, *const i32, PhantomData<L>);
impl<L: Lift> Each for LowHigh<L> {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, i: usize) {
        let a = I::load(self.2.add(i));
        let v = L::apply::<I>(I::load(self.1.add(i)), a, I::load(self.2.add(i + 1)));
        store_pairs::<I>(self.0.add(2 * i), a, v);
    }
}

/// # Safety
///
/// `p` has room for `2 * N` samples.
#[inline(always)]
unsafe fn store_pairs<I: Isa>(p: *mut i32, a: I::R, b: I::R) {
    let (x, y) = I::zip(a, b);
    I::store(p, x);
    I::store(p.add(I::N), y);
}

/// `dst[i] = f(t[i], s[i - 1], s[i])` when `lead` (`t`'s first sample comes
/// before `s`'s first), else `f(t[i], s[i], s[i + 1])`; missing neighbours are
/// mirrored.
///
/// # Safety
///
/// `t` and `dst` have `n` samples (and are the same or disjoint), `s` has `m`,
/// both at least 1; the leading one is as long as the other or one longer.
#[inline(always)]
unsafe fn step<I: Isa, L: Lift>(
    dst: *mut i32,
    t: *const i32,
    n: usize,
    s: *const i32,
    m: usize,
    lead: bool,
) {
    let (first, end) = if lead {
        *dst = L::apply::<Scalar>(*t, *s, *s);
        (1, n.min(m))
    } else {
        (0, n.min(m - 1))
    };
    // The left neighbour of t[i] is s[i - first].
    let left = s.wrapping_sub(first);
    each::<I, _>(&Step(dst, t, left, PhantomData::<L>), first, end.max(first));
    let i = end.max(first);
    if i < n {
        let a = *left.wrapping_add(i);
        *dst.add(i) = L::apply::<Scalar>(*t.add(i), a, a);
    }
}

/// The last lifting step, of the high-pass half of a row, with the
/// interleaving: `low` has the finished low-pass samples, `row` the high-pass
/// ones from `lw` on, and receives the result.
///
/// # Safety
///
/// `low` has `lw` samples, `row` `lw + hw`, both at least 1, the first one in
/// output order (high if `odd`) as long as the other or one longer; they do
/// not overlap.
#[inline(always)]
unsafe fn predict_row<I: Isa, L: Lift>(
    row: *mut i32,
    lw: usize,
    hw: usize,
    low: *const i32,
    odd: bool,
) {
    // The output never overtakes the high-pass samples still to be read:
    // 2 (i + N) <= lw + i + N as long as i + N <= lw.
    let high = row.add(lw).cast_const();
    if odd {
        // The neighbours of high[i] are low[i - 1] and low[i].
        *row = L::apply::<Scalar>(*high, *low, *low);
        *row.add(1) = *low;
        each::<I, _>(&HighLow(row, high, low, PhantomData::<L>), 1, lw);
        if hw > lw {
            let a = *low.add(lw - 1);
            *row.add(2 * lw) = L::apply::<Scalar>(*high.add(lw), a, a);
        }
    } else {
        // The neighbours of high[i] are low[i] and low[i + 1].
        each::<I, _>(&LowHigh(row, high, low, PhantomData::<L>), 0, lw - 1);
        let i = lw - 1;
        let a = *low.add(i);
        if hw == lw {
            *row.add(2 * i + 1) = L::apply::<Scalar>(*high.add(i), a, a);
        }
        *row.add(2 * i) = a;
    }
}

trait Row {
    /// # Safety
    ///
    /// `row` has `lw + hw` samples and `tmp` `lw`, not overlapping; see
    /// [`predict_row`] for the lengths.
    unsafe fn run<I: Isa>(row: *mut i32, lw: usize, hw: usize, odd: bool, tmp: *mut i32);
}

struct Row53;
impl Row for Row53 {
    #[inline(always)]
    unsafe fn run<I: Isa>(row: *mut i32, lw: usize, hw: usize, odd: bool, tmp: *mut i32) {
        step::<I, Update53>(tmp, row, lw, row.add(lw), hw, !odd);
        predict_row::<I, Predict53>(row, lw, hw, tmp, odd);
    }
}

struct Row97;
impl Row for Row97 {
    #[inline(always)]
    unsafe fn run<I: Isa>(row: *mut i32, lw: usize, hw: usize, odd: bool, tmp: *mut i32) {
        let high = row.add(lw);
        step::<I, ScaleAllDelta>(tmp, row, lw, high, hw, !odd);
        step::<I, ScaleGamma>(high, high, hw, tmp, lw, odd);
        step::<I, Beta>(tmp, tmp, lw, high, hw, !odd);
        predict_row::<I, Alpha>(row, lw, hw, tmp, odd);
    }
}

/// # Safety
///
/// As for [`Row::run`], for the `h` rows `stride` apart from `p` on.
#[inline(always)]
unsafe fn row_loop<I: Isa, W: Row>(
    p: *mut i32,
    stride: usize,
    h: usize,
    (lw, hw): (usize, usize),
    odd: bool,
    tmp: *mut i32,
) {
    for y in 0..h {
        W::run::<I>(p.add(y * stride), lw, hw, odd, tmp);
    }
}

dispatch!(rows53 = row_loop::<Row53>(p: *mut i32, stride: usize, h: usize, n: (usize, usize), odd: bool, tmp: *mut i32));
dispatch!(rows97 = row_loop::<Row97>(p: *mut i32, stride: usize, h: usize, n: (usize, usize), odd: bool, tmp: *mut i32));

/// Horizontal synthesis, in place, of `h` rows: the low-pass half of `lw`
/// samples followed by the high-pass half of `hw`, both at least 1; the 9/7 on
/// reals or else the 5/3. `odd`: the first output sample has an odd position.
/// `tmp` holds at least `lw`.
pub(crate) fn synthesize_rows(
    p: &mut [i32],
    stride: usize,
    (lw, hw): (usize, usize),
    h: usize,
    odd: bool,
    reals: bool,
    tmp: &mut [i32],
) {
    let w = lw + hw;
    assert!(w <= stride && h >= 1 && p.len() >= (h - 1) * stride + w);
    let (long, short) = if odd { (hw, lw) } else { (lw, hw) };
    assert!(short >= 1 && (long == short || long == short + 1));
    assert!(tmp.len() >= lw);
    let (p, tmp) = (p.as_mut_ptr(), tmp.as_mut_ptr());
    // SAFETY: the lengths were just checked; the slices are distinct.
    unsafe {
        if reals {
            rows97(p, stride, h, (lw, hw), odd, tmp);
        } else {
            rows53(p, stride, h, (lw, hw), odd, tmp);
        }
    }
}

// ---------------------------------------------------------------- columns

/// Most columns the vertical transform handles together.
const STRIP: usize = 16;

/// Scratch samples per row that [`synthesize_columns`] needs.
pub(crate) const COLUMN_SCRATCH: usize = 2 * STRIP;

/// Two registers side by side.
#[derive(Clone, Copy)]
struct Pair<I: Isa>(I::R, I::R);

impl<I: Isa> Pair<I> {
    #[inline(always)]
    unsafe fn load(p: *const i32) -> Self {
        Pair(I::load(p), I::load(p.add(I::N)))
    }
    #[inline(always)]
    unsafe fn store(self, p: *mut i32) {
        I::store(p, self.0);
        I::store(p.add(I::N), self.1);
    }
    #[inline(always)]
    unsafe fn lift<L: Lift>(self, a: Self, b: Self) -> Self {
        Pair(
            L::apply::<I>(self.0, a.0, b.0),
            L::apply::<I>(self.1, a.1, b.1),
        )
    }
    #[inline(always)]
    unsafe fn scale(self, c: f32) -> Self {
        Pair(I::mulf(self.0, I::splatf(c)), I::mulf(self.1, I::splatf(c)))
    }
}

/// Rows of one strip, by output position, mirrored at both ends.
#[derive(Clone, Copy)]
struct Column {
    base: *const i32,
    stride: usize,
    /// Rows, at least 2, and low-pass ones among them.
    h: usize,
    lh: usize,
    /// The first output row has an odd position.
    odd: bool,
}

impl Column {
    /// Input row of output position `i`, mirrored into range.
    #[inline(always)]
    fn at(&self, i: isize) -> *const i32 {
        let mut i = i as usize;
        if i >= self.h {
            i = mirror(i as isize, self.h);
        }
        let mut row = i / 2;
        if (i & 1 != 0) != self.odd {
            row += self.lh;
        }
        self.base.wrapping_add(row * self.stride)
    }
}

#[cold]
#[inline(never)]
fn mirror(i: isize, n: usize) -> usize {
    let period = 2 * (n as isize - 1);
    let j = i.rem_euclid(period);
    (if j < n as isize { j } else { period - j }) as usize
}

/// A vertical transform, in one sweep over the rows: symmetric extension and
/// lifting commute, so the edges need no cases of their own.
trait Sweep {
    /// How far the output lags: a step at low-pass position `i` finishes the
    /// rows `i - LAG` (low-pass) and `i - LAG - 1` (high-pass).
    const LAG: isize;
    /// What a step leaves for the next one.
    type State<I: Isa>: Copy;
    /// From the high-pass row before the first low-pass one.
    unsafe fn start<I: Isa>(high: *const i32) -> Self::State<I>;
    /// Take the low-pass row at `i` and the high-pass row at `i + 1`; return
    /// the two finished rows, high-pass first.
    unsafe fn step<I: Isa>(
        state: &mut Self::State<I>,
        low: *const i32,
        high: *const i32,
    ) -> (Pair<I>, Pair<I>);
}

struct Sweep53;
impl Sweep for Sweep53 {
    const LAG: isize = 0;
    /// High-pass input, low-pass output.
    type State<I: Isa> = (Pair<I>, Pair<I>);
    #[inline(always)]
    unsafe fn start<I: Isa>(high: *const i32) -> Self::State<I> {
        let d = Pair::load(high);
        (d, d)
    }
    #[inline(always)]
    unsafe fn step<I: Isa>(
        state: &mut Self::State<I>,
        low: *const i32,
        high: *const i32,
    ) -> (Pair<I>, Pair<I>) {
        let (d, e) = *state;
        let d1 = Pair::<I>::load(high);
        let e1 = Pair::<I>::load(low).lift::<Update53>(d, d1);
        *state = (d1, e1);
        (d.lift::<Predict53>(e, e1), e1)
    }
}

struct Sweep97;
impl Sweep for Sweep97 {
    const LAG: isize = 2;
    type State<I: Isa> = [Pair<I>; 4];
    #[inline(always)]
    unsafe fn start<I: Isa>(high: *const i32) -> Self::State<I> {
        [Pair::load(high).scale(1.0 / K); 4]
    }
    #[inline(always)]
    unsafe fn step<I: Isa>(
        state: &mut Self::State<I>,
        low: *const i32,
        high: *const i32,
    ) -> (Pair<I>, Pair<I>) {
        let [d0, s1, d1, s2] = *state;
        let d0n = Pair::<I>::load(high).scale(1.0 / K);
        let s1n = Pair::<I>::load(low).lift::<ScaleDelta>(d0, d0n);
        let d1n = d0.lift::<Gamma>(s1, s1n);
        let s2n = s1.lift::<Beta>(d1, d1n);
        *state = [d0n, s1n, d1n, s2n];
        (d1.lift::<Alpha>(s2, s2n), s2n)
    }
}

/// # Safety
///
/// `c` describes readable rows of `2 * N` samples and `out` has `2 * N`
/// samples for each of them; the two do not overlap.
#[inline(always)]
unsafe fn sweep<I: Isa, S: Sweep>(c: Column, out: *mut i32) {
    let n = 2 * I::N;
    let h = c.h as isize;
    // `i` runs over the low-pass positions, from where the first complete
    // output needs it.
    let odd = isize::from(c.odd);
    let mut i = -S::LAG - odd;
    let mut state = S::start::<I>(c.at(i - 1));
    let row = |i: isize| out.wrapping_add((i as usize).wrapping_mul(n));
    while i <= h + S::LAG {
        if i > S::LAG && i + 1 < h {
            // Away from the edges.
            let mut low = c.at(i);
            let mut high = c.at(i + 1);
            let mut to = row(i - S::LAG - 1);
            let count = (h - 1 - i + 1) / 2;
            for _ in 0..count {
                let (a, b) = S::step::<I>(&mut state, low, high);
                a.store(to);
                b.store(to.add(n));
                low = low.wrapping_add(c.stride);
                high = high.wrapping_add(c.stride);
                to = to.wrapping_add(2 * n);
            }
            i += 2 * count;
        } else {
            let (a, b) = S::step::<I>(&mut state, c.at(i), c.at(i + 1));
            let done = i - S::LAG;
            if done > 0 {
                a.store(row(done - 1));
            }
            if done >= 0 && done < h {
                b.store(row(done));
            }
            i += 2;
        }
    }
}

/// # Safety
///
/// `p` has `h` rows (at least 2, the `lh` low-pass ones first) of `w`
/// samples, `stride` apart; `col` has `COLUMN_SCRATCH * h` samples and does
/// not overlap.
#[inline(always)]
unsafe fn column_loop<I: Isa, S: Sweep>(
    p: *mut i32,
    stride: usize,
    w: usize,
    (h, lh): (usize, usize),
    odd: bool,
    col: *mut i32,
) {
    let n = 2 * I::N;
    let mut c = Column {
        base: p,
        stride,
        h,
        lh,
        odd,
    };
    let mut x = 0;
    while x + n <= w {
        c.base = p.add(x);
        sweep::<I, S>(c, col);
        for y in 0..h {
            Pair::<I>::load(col.add(y * n)).store(p.add(y * stride + x));
        }
        x += n;
    }
    if x < w {
        // The last columns go through a strip of their own.
        let strip = col.add(STRIP * h);
        for y in 0..h {
            std::ptr::copy_nonoverlapping(p.add(y * stride + x), strip.add(y * n), w - x);
        }
        c.base = strip;
        c.stride = n;
        sweep::<I, S>(c, col);
        for y in 0..h {
            std::ptr::copy_nonoverlapping(col.add(y * n), p.add(y * stride + x), w - x);
        }
    }
}

dispatch!(columns53 = column_loop::<Sweep53>(p: *mut i32, stride: usize, w: usize, n: (usize, usize), odd: bool, col: *mut i32));
dispatch!(columns97 = column_loop::<Sweep97>(p: *mut i32, stride: usize, w: usize, n: (usize, usize), odd: bool, col: *mut i32));

/// Vertical synthesis, in place, of `h` rows (at least 2) of `w` samples: `lh`
/// low-pass rows, then the high-pass ones; the 9/7 on reals or else the 5/3.
/// `odd`: the first output row has an odd position. `col` is scratch of
/// `COLUMN_SCRATCH * h`.
pub(crate) fn synthesize_columns(
    p: &mut [i32],
    stride: usize,
    w: usize,
    (h, lh): (usize, usize),
    odd: bool,
    reals: bool,
    col: &mut [i32],
) {
    assert!(w <= stride && h >= 2 && p.len() >= (h - 1) * stride + w);
    // The low-pass rows are those at even positions.
    assert!(lh == (h + usize::from(!odd)) / 2);
    assert!(col.len() >= COLUMN_SCRATCH * h);
    let (p, col) = (p.as_mut_ptr(), col.as_mut_ptr());
    // SAFETY: the lengths were just checked; the slices are distinct.
    unsafe {
        if reals {
            columns97(p, stride, w, (h, lh), odd, col);
        } else {
            columns53(p, stride, w, (h, lh), odd, col);
        }
    }
}

// ----------------------------------------------------------- sample by sample

dispatch!(each_rct = each::<Rct>(e: &Rct, first: usize, end: usize));
dispatch!(each_ict = each::<Ict>(e: &Ict, first: usize, end: usize));

/// Inverse RCT (G.2).
struct Rct([*mut i32; 3]);
impl Each for Rct {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, i: usize) {
        let [a, b, c] = self.0.map(|p| p.add(i));
        let (y, u, v) = (I::load(a), I::load(b), I::load(c));
        let g = I::sub(y, I::sra::<2>(I::add(u, v)));
        I::store(a, I::add(v, g));
        I::store(b, g);
        I::store(c, I::add(u, g));
    }
}

/// Inverse ICT (G.3).
struct Ict([*mut i32; 3]);
impl Each for Ict {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, i: usize) {
        let [a, b, c] = self.0.map(|p| p.add(i));
        let (y, u, v) = (I::load(a), I::load(b), I::load(c));
        let times = |x: I::R, c: f32| I::mulf(x, I::splatf(c));
        I::store(a, I::addf(y, times(v, 1.402)));
        let g = I::subf(y, times(u, 0.344_13));
        I::store(b, I::subf(g, times(v, 0.714_14)));
        I::store(c, I::addf(y, times(u, 1.772)));
    }
}

/// Inverse colour transform of three planes of equal length: the ICT on reals
/// or else the RCT.
pub(crate) fn inverse_mct(planes: [&mut [i32]; 3], reals: bool) {
    let n = planes[0].len();
    assert!(planes.iter().all(|p| p.len() == n));
    let planes = planes.map(<[i32]>::as_mut_ptr);
    // SAFETY: each plane has `n` samples and they are distinct.
    unsafe {
        if reals {
            each_ict(&Ict(planes), 0, n);
        } else {
            each_rct(&Rct(planes), 0, n);
        }
    }
}

trait Dequant {
    unsafe fn apply<I: Isa>(v: I::R, step: f32) -> I::R;
}

struct Halve;
impl Dequant for Halve {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, _: f32) -> I::R {
        I::signed_half(v)
    }
}

struct ToReal;
impl Dequant for ToReal {
    #[inline(always)]
    unsafe fn apply<I: Isa>(v: I::R, step: f32) -> I::R {
        I::mulf(I::to_real(I::signed(v)), I::splatf(step))
    }
}

/// # Safety
///
/// `src` has stripes of `4 * w` samples for `h` rows, `dst` has `h` rows of
/// `w`, `stride` apart.
#[inline(always)]
unsafe fn dequant_loop<I: Isa, D: Dequant>(
    src: *const i32,
    dst: *mut i32,
    stride: usize,
    (w, h): (usize, usize),
    step: f32,
) {
    let mut y = 0;
    while y < h {
        let (src, dst) = (src.add(y * w), dst.add(y * stride));
        if y + 4 <= h {
            dequant_stripe::<I, D>(src, dst, stride, w, 4, step);
        } else {
            dequant_stripe::<I, D>(src, dst, stride, w, h - y, step);
        }
        y += 4;
    }
}

/// # Safety
///
/// `src` has `4 * w` samples, `dst` has `rows` (up to four) rows of `w`,
/// `stride` apart.
#[inline(always)]
unsafe fn dequant_stripe<I: Isa, D: Dequant>(
    src: *const i32,
    dst: *mut i32,
    stride: usize,
    w: usize,
    rows: usize,
    step: f32,
) {
    let e = Stripe {
        src,
        dst,
        stride,
        rows,
        step,
        kind: PhantomData::<D>,
    };
    each::<I, _>(&e, 0, w);
}

struct Stripe<D> {
    src: *const i32,
    dst: *mut i32,
    stride: usize,
    rows: usize,
    step: f32,
    kind: PhantomData<D>,
}
impl<D: Dequant> Each for Stripe<D> {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, x: usize) {
        let v = I::load_columns(self.src.add(4 * x));
        for (i, &v) in v.iter().enumerate().take(self.rows) {
            let p = self.dst.add(i * self.stride + x);
            I::store(p, D::apply::<I>(v, self.step));
        }
    }
}

dispatch!(dequant_int = dequant_loop::<Halve>(src: *const i32, dst: *mut i32, stride: usize, n: (usize, usize), step: f32));
dispatch!(dequant_real = dequant_loop::<ToReal>(src: *const i32, dst: *mut i32, stride: usize, n: (usize, usize), step: f32));

/// Coefficients of a `w` x `h` code-block (bit 31 the sign, below it twice the
/// magnitude; stripes of four rows, in them column by column) into a plane of
/// rows `stride` apart: times half of `step` as reals, or halved towards zero
/// as integers when there is no step.
pub(crate) fn dequantize(
    src: &[u32],
    (w, h): (usize, usize),
    dst: &mut [i32],
    stride: usize,
    step: Option<f32>,
) {
    if w == 0 || h == 0 {
        return;
    }
    assert!(src.len() >= w * h.next_multiple_of(4));
    assert!(w <= stride && dst.len() >= (h - 1) * stride + w);
    let (src, dst) = (src.as_ptr().cast::<i32>(), dst.as_mut_ptr());
    // SAFETY: both lengths were just checked; `u32` and `i32` have the same
    // layout.
    unsafe {
        match step {
            None => dequant_int(src, dst, stride, (w, h), 0.0),
            Some(step) => dequant_real(src, dst, stride, (w, h), step * 0.5),
        }
    }
}

/// Level shift and range of a component's samples.
#[derive(Clone, Copy)]
pub(crate) struct Level {
    shift: i32,
    min: i32,
    max: i32,
    bits: u8,
}

impl Level {
    /// For samples of 1 to 31 bits.
    pub fn new(bits: u8, signed: bool) -> Self {
        let half = 1i64 << (bits - 1);
        let (shift, min, max) = if signed {
            (0, -half, half - 1)
        } else {
            (half, 0, 2 * half - 1)
        };
        Level {
            shift: shift as i32,
            min: min as i32,
            max: max as i32,
            bits,
        }
    }
}

struct LevelInt {
    src: *const i32,
    dst: *mut i32,
    level: Level,
}
impl Each for LevelInt {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, i: usize) {
        // The limits less the shift are in range, and limiting first keeps
        // the sum in range too.
        let l = self.level;
        let (lo, hi) = (I::splat(l.min - l.shift), I::splat(l.max - l.shift));
        let v = I::min(I::max(I::load(self.src.add(i)), lo), hi);
        I::store(self.dst.add(i), I::add(v, I::splat(l.shift)));
    }
}

struct LevelReal {
    src: *const i32,
    dst: *mut i32,
    level: Level,
}
impl Each for LevelReal {
    #[inline(always)]
    unsafe fn at<I: Isa>(&self, i: usize) {
        // Up to 20 bits the sum with ROUND does the rounding.
        let l = self.level;
        let lo = I::splatf((l.min - l.shift) as f32);
        let hi = I::splatf((l.max - l.shift) as f32);
        let v = I::clampf(I::load(self.src.add(i)), lo, hi);
        let v = I::addf(v, I::splatf(ROUND));
        let bias = I::splat(l.shift.wrapping_sub(real(ROUND)));
        I::store(self.dst.add(i), I::add(v, bias));
    }
}

dispatch!(each_level_int = each::<LevelInt>(e: &LevelInt, first: usize, end: usize));
dispatch!(each_level_real = each::<LevelReal>(e: &LevelReal, first: usize, end: usize));

/// # Safety
///
/// `src` and `dst` have `n` samples and are the same or disjoint.
unsafe fn level(src: *const i32, dst: *mut i32, n: usize, level: Level, reals: bool) {
    if !reals {
        each_level_int(&LevelInt { src, dst, level }, 0, n);
    } else if level.bits <= 20 {
        each_level_real(&LevelReal { src, dst, level }, 0, n);
    } else {
        for i in 0..n {
            *dst.add(i) = (float(*src.add(i)).round_ties_even() as i32)
                .saturating_add(level.shift)
                .clamp(level.min, level.max);
        }
    }
}

/// Round (reals), level shift and limit samples in place.
pub(crate) fn level_in_place(p: &mut [i32], l: Level, reals: bool) {
    let q = p.as_mut_ptr();
    // SAFETY: one slice, its own length.
    unsafe { level(q, q, p.len(), l, reals) }
}

/// Round (reals), level shift and limit the samples of `src` into `dst`.
pub(crate) fn level_into(dst: &mut [i32], src: &[i32], l: Level, reals: bool) {
    assert!(dst.len() == src.len());
    // SAFETY: equal lengths, and the slices are distinct.
    unsafe { level(src.as_ptr(), dst.as_mut_ptr(), src.len(), l, reals) }
}
