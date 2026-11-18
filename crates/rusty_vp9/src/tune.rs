//! Experiment and diagnostic switches read from the environment (`VP9_*`).
//!
//! The encoder and decoder carry many of these: A/B toggles for kernels,
//! RD tuning knobs, probes and dumps. They are how the campaigns measured
//! each change, but a stray variable on a user's machine must not change the
//! files an application exports (#119). So they are read only when the crate
//! is built with the `env-tuning` feature; without it every switch reads as
//! unset, which is each one's default, and the output depends on the API
//! alone. Settings an application needs are on
//! [`Vp9EncoderConfig`](crate::Vp9EncoderConfig) instead.

use std::env::VarError;
use std::ffi::OsString;

/// [`std::env::var`] with the `env-tuning` feature, else always unset.
#[inline]
pub(crate) fn var(key: &str) -> Result<String, VarError> {
    #[cfg(feature = "env-tuning")]
    {
        std::env::var(key)
    }
    #[cfg(not(feature = "env-tuning"))]
    {
        let _ = key;
        Err(VarError::NotPresent)
    }
}

/// [`std::env::var_os`] with the `env-tuning` feature, else always unset.
#[inline]
pub(crate) fn var_os(key: &str) -> Option<OsString> {
    #[cfg(feature = "env-tuning")]
    {
        std::env::var_os(key)
    }
    #[cfg(not(feature = "env-tuning"))]
    {
        let _ = key;
        None
    }
}
