// Copyright 2006 The Android Open Source Project
// Copyright 2020 Yevhenii Reizner
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::path::floating_point::f32_as_2s_compliment;

#[allow(missing_docs)]
pub const SCALAR_MAX: f32 = 3.402823466e+38;
#[allow(missing_docs)]
pub const SCALAR_NEARLY_ZERO: f32 = 1.0 / (1 << 12) as f32;
#[allow(missing_docs)]
pub const SCALAR_ROOT_2_OVER_2: f32 = 0.707106781;

/// Float number extension methods.
///
/// Mainly for internal use. Do not rely on it!
#[allow(missing_docs)]
pub trait Scalar {
    fn half(self) -> Self;
    fn ave(self, other: Self) -> Self;
    fn sqr(self) -> Self;
    fn invert(self) -> Self;
    fn bound(self, min: Self, max: Self) -> Self;
    fn is_nearly_equal(self, other: Self) -> bool;
    fn is_nearly_equal_within_tolerance(self, other: Self, tolerance: Self) -> bool;
    fn is_nearly_zero(self) -> bool;
    fn is_nearly_zero_within_tolerance(self, tolerance: Self) -> bool;
    fn almost_dequal_ulps(self, other: Self) -> bool;
}

impl Scalar for f32 {
    fn half(self) -> f32 {
        self * 0.5
    }

    fn ave(self, other: Self) -> f32 {
        (self + other) * 0.5
    }

    fn sqr(self) -> f32 {
        self * self
    }

    fn invert(self) -> f32 {
        1.0 / self
    }

    // Works just like SkTPin, returning `max` for NaN/inf
    /// A non-panicking clamp.
    fn bound(self, min: Self, max: Self) -> Self {
        max.min(self).max(min)
    }

    fn is_nearly_equal(self, other: Self) -> bool {
        (self - other).abs() <= SCALAR_NEARLY_ZERO
    }

    fn is_nearly_equal_within_tolerance(self, other: Self, tolerance: Self) -> bool {
        (self - other).abs() <= tolerance
    }

    fn is_nearly_zero(self) -> bool {
        self.is_nearly_zero_within_tolerance(SCALAR_NEARLY_ZERO)
    }

    fn is_nearly_zero_within_tolerance(self, tolerance: Self) -> bool {
        debug_assert!(tolerance >= 0.0);
        self.abs() <= tolerance
    }

    // From SkPathOpsTypes.
    fn almost_dequal_ulps(self, other: Self) -> bool {
        const ULPS_EPSILON: i32 = 16;
        let a_bits = f32_as_2s_compliment(self);
        let b_bits = f32_as_2s_compliment(other);
        // Find the difference in ULPs.
        a_bits < b_bits + ULPS_EPSILON && b_bits < a_bits + ULPS_EPSILON
    }
}
