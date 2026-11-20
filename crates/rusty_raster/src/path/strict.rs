// Copyright 2022 Yevhenii Reizner (strict-num)
// Copyright 2026 Mata Network
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

//! Range-checked float newtypes (the subset of `strict-num` 0.1 that the
//! rasteriser uses).
//!
//! Unlike `strict-num`, the `new_unchecked` constructors here are safe: they
//! still enforce the invariant (by clamping, or by mapping a non-finite value
//! to zero) so a caller bug can produce a wrong pixel but never an invalid
//! value.

use core::cmp::Ordering;

#[inline]
fn clamp_f32(min: f32, val: f32, max: f32) -> f32 {
    max.min(val).max(min)
}

/// An immutable, finite `f32`.
#[derive(Copy, Clone, Default, Debug)]
#[repr(transparent)]
pub struct FiniteF32(f32);

impl FiniteF32 {
    /// Creates a finite `f32`. Returns `None` for NaN and infinity.
    #[inline]
    pub fn new(n: f32) -> Option<Self> {
        if n.is_finite() {
            Some(FiniteF32(n))
        } else {
            None
        }
    }

    /// Creates a finite `f32` from a value the caller knows is finite.
    ///
    /// Safe: a non-finite value becomes zero.
    #[inline]
    pub const fn new_unchecked(n: f32) -> Self {
        if n.is_finite() {
            FiniteF32(n)
        } else {
            FiniteF32(0.0)
        }
    }

    /// Returns the value as a primitive type.
    #[inline]
    pub const fn get(&self) -> f32 {
        self.0
    }
}

impl Eq for FiniteF32 {}

impl PartialEq for FiniteF32 {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Ord for FiniteF32 {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        if self.0 < other.0 {
            Ordering::Less
        } else if self.0 > other.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

impl PartialOrd for FiniteF32 {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl core::hash::Hash for FiniteF32 {
    #[inline]
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl PartialEq<f32> for FiniteF32 {
    #[inline]
    fn eq(&self, other: &f32) -> bool {
        self.get() == *other
    }
}

/// An immutable, finite `f32` that is known to be > 0.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
#[repr(transparent)]
pub struct NonZeroPositiveF32(FiniteF32);

impl NonZeroPositiveF32 {
    /// Creates a new `NonZeroPositiveF32` if the given value is finite and > 0.
    #[inline]
    pub fn new(n: f32) -> Option<Self> {
        if n.is_finite() && n > 0.0 {
            Some(NonZeroPositiveF32(FiniteF32(n)))
        } else {
            None
        }
    }

    /// Returns the value as a primitive type.
    #[inline]
    pub const fn get(&self) -> f32 {
        self.0.get()
    }

    /// Returns the value as a `FiniteF32`.
    #[inline]
    pub const fn get_finite(&self) -> FiniteF32 {
        self.0
    }
}

impl PartialEq<f32> for NonZeroPositiveF32 {
    #[inline]
    fn eq(&self, other: &f32) -> bool {
        self.get() == *other
    }
}

/// An immutable, finite `f32` in a 0..=1 range.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
#[repr(transparent)]
pub struct NormalizedF32(FiniteF32);

impl NormalizedF32 {
    /// A `NormalizedF32` value initialized with zero.
    pub const ZERO: Self = NormalizedF32(FiniteF32(0.0));
    /// A `NormalizedF32` value initialized with one.
    pub const ONE: Self = NormalizedF32(FiniteF32(1.0));

    /// Creates a `NormalizedF32` if the given value is in a 0..=1 range.
    #[inline]
    pub fn new(n: f32) -> Option<Self> {
        if n.is_finite() && (0.0..=1.0).contains(&n) {
            Some(NormalizedF32(FiniteF32(n)))
        } else {
            None
        }
    }

    /// Creates a `NormalizedF32` from a value the caller knows is in 0..=1.
    ///
    /// Safe: the value is clamped, and NaN or infinity becomes zero.
    #[inline]
    pub fn new_unchecked(n: f32) -> Self {
        Self::new_clamped(n)
    }

    /// Creates a `NormalizedF32` clamping the given value to a 0..=1 range.
    ///
    /// Returns zero in case of NaN or infinity.
    #[inline]
    pub fn new_clamped(n: f32) -> Self {
        if n.is_finite() {
            NormalizedF32(FiniteF32(clamp_f32(0.0, n, 1.0)))
        } else {
            Self::ZERO
        }
    }

    /// Creates a `NormalizedF32` by dividing the given value by 255.
    #[inline]
    pub fn new_u8(n: u8) -> Self {
        NormalizedF32(FiniteF32(f32::from(n) / 255.0))
    }

    /// Creates a `NormalizedF32` by dividing the given value by 65535.
    #[inline]
    pub fn new_u16(n: u16) -> Self {
        NormalizedF32(FiniteF32(f32::from(n) / 65535.0))
    }

    /// Returns the value as a primitive type.
    #[inline]
    pub const fn get(self) -> f32 {
        self.0.get()
    }

    /// Returns the value as a `FiniteF32`.
    #[inline]
    pub const fn get_finite(&self) -> FiniteF32 {
        self.0
    }

    /// Returns the value as a `u8`.
    #[inline]
    pub fn to_u8(&self) -> u8 {
        ((self.0).0 * 255.0 + 0.5) as u8
    }

    /// Returns the value as a `u16`.
    #[inline]
    pub fn to_u16(&self) -> u16 {
        ((self.0).0 * 65535.0 + 0.5) as u16
    }
}

impl core::ops::Mul<NormalizedF32> for NormalizedF32 {
    type Output = Self;

    #[inline]
    fn mul(self, rhs: Self) -> Self::Output {
        Self::new_clamped((self.0).0 * (rhs.0).0)
    }
}

impl PartialEq<f32> for NormalizedF32 {
    #[inline]
    fn eq(&self, other: &f32) -> bool {
        self.get() == *other
    }
}
