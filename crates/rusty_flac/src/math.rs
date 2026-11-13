//! The few transcendentals the encoder uses, behind one seam.
//!
//! With the `libm` feature these come from the pure-Rust `libm` crate, which
//! gives the same bits on every target — so an encoder on a chip and an
//! encoder on the host make identical decisions. Without it (the `std`
//! default) they are the platform's. The decoder uses none of them.

#[cfg(feature = "libm")]
mod imp {
    #[inline]
    pub fn cos(x: f64) -> f64 {
        libm::cos(x)
    }
    #[inline]
    pub fn log2(x: f64) -> f64 {
        libm::log2(x)
    }
    #[inline]
    pub fn floor(x: f64) -> f64 {
        libm::floor(x)
    }
    #[inline]
    pub fn round(x: f64) -> f64 {
        libm::round(x)
    }
    #[inline]
    pub fn exp2(x: f64) -> f64 {
        libm::exp2(x)
    }
    #[inline]
    pub fn roundf(x: f32) -> f32 {
        libm::roundf(x)
    }
}

#[cfg(not(feature = "libm"))]
mod imp {
    #[inline]
    pub fn cos(x: f64) -> f64 {
        x.cos()
    }
    #[inline]
    pub fn log2(x: f64) -> f64 {
        x.log2()
    }
    #[inline]
    pub fn floor(x: f64) -> f64 {
        x.floor()
    }
    #[inline]
    pub fn round(x: f64) -> f64 {
        x.round()
    }
    #[inline]
    pub fn exp2(x: f64) -> f64 {
        x.exp2()
    }
    #[inline]
    pub fn roundf(x: f32) -> f32 {
        x.round()
    }
}

pub(crate) use imp::{cos, exp2, floor, log2, round, roundf};
