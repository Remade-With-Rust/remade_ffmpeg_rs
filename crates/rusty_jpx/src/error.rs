//! Error type and bounded readers.

use std::fmt;

/// Everything that can go wrong decoding a JPEG 2000 image.
///
/// Malformed input always surfaces as one of these; it never panics.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum JpxError {
    /// The file is malformed.
    Format(&'static str),
    /// The data ends before the image is complete. With
    /// [`DecodeOptions::allow_truncated`](crate::DecodeOptions::allow_truncated)
    /// the decoder returns what it has instead.
    Truncated,
    /// Well formed, but uses something this crate does not decode.
    Unsupported(&'static str),
    /// Decoding would exceed a [`Limits`](crate::Limits) value.
    Limit(&'static str),
    /// The options do not fit the image (a `reduce` larger than the number of
    /// decomposition levels).
    Usage(&'static str),
}

/// `Result` alias used throughout the crate.
pub type JpxResult<T> = Result<T, JpxError>;

impl fmt::Display for JpxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(m) => write!(f, "malformed JPEG 2000: {m}"),
            Self::Truncated => f.write_str("truncated JPEG 2000 data"),
            Self::Unsupported(m) => write!(f, "unsupported JPEG 2000 feature: {m}"),
            Self::Limit(m) => write!(f, "JPEG 2000 limit exceeded: {m}"),
            Self::Usage(m) => write!(f, "invalid decode option: {m}"),
        }
    }
}

impl std::error::Error for JpxError {}

/// A vector of `n` default values, or a `Limit` error when the allocator
/// refuses it.
pub(crate) fn try_vec<T: Clone + Default>(n: usize) -> JpxResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| JpxError::Limit("out of memory"))?;
    v.resize(n, T::default());
    Ok(v)
}

/// `v` with room for `n` values, whatever they are: a buffer for reuse.
pub(crate) fn try_grow(v: &mut Vec<i32>, n: usize) -> JpxResult<&mut [i32]> {
    if v.len() < n {
        *v = Vec::new();
        *v = try_vec(n)?;
    }
    Ok(v)
}

/// Big-endian field reader over a marker segment or box.
pub(crate) struct Fields<'a>(pub &'a [u8]);

impl<'a> Fields<'a> {
    pub fn take(&mut self, n: usize) -> JpxResult<&'a [u8]> {
        if n > self.0.len() {
            return Err(JpxError::Truncated);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    pub fn uint(&mut self, n: usize) -> JpxResult<u64> {
        let bytes = self.take(n)?;
        Ok(bytes.iter().fold(0, |a, &b| (a << 8) | u64::from(b)))
    }

    pub fn u8(&mut self) -> JpxResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> JpxResult<u16> {
        Ok(self.uint(2)? as u16)
    }

    pub fn u32(&mut self) -> JpxResult<u32> {
        Ok(self.uint(4)? as u32)
    }

    /// A component index: one byte below 257 components, two otherwise.
    pub fn component(&mut self, count: usize) -> JpxResult<usize> {
        Ok(if count < 257 {
            usize::from(self.u8()?)
        } else {
            usize::from(self.u16()?)
        })
    }
}
