use crate::{DecodeOptions, Error, Limits, Result};

/// Running account of the work and memory one decode call has used (and
/// whether the call is lenient, which the same functions need).
///
/// Every loop whose length the input controls charges this before it runs, so
/// a declared size can never buy more than [`Limits::max_work`] allows.
pub(crate) struct Budget {
    pub(crate) limits: Limits,
    pub(crate) lenient: bool,
    left: u64,
}

impl Budget {
    pub(crate) fn new(opts: &DecodeOptions) -> Self {
        Self {
            limits: opts.limits.clone(),
            lenient: opts.lenient,
            left: opts.limits.max_work,
        }
    }

    pub(crate) fn charge(&mut self, units: u64) -> Result<()> {
        match self.left.checked_sub(units) {
            Some(l) => {
                self.left = l;
                Ok(())
            }
            None => Err(Error::LimitExceeded("max_work")),
        }
    }

    /// Check a `w` x `h` bitmap against `max_pixels`. Rows are whole bytes,
    /// so a column of one-pixel rows counts as what it allocates.
    pub(crate) fn size(&self, w: u64, h: u64) -> Result<u64> {
        let px = w
            .div_ceil(8)
            .checked_mul(8)
            .and_then(|w| w.checked_mul(h))
            .ok_or(Error::LimitExceeded("max_pixels"))?;
        if w > u64::from(u32::MAX) || h > u64::from(u32::MAX) || px > self.limits.max_pixels {
            return Err(Error::LimitExceeded("max_pixels"));
        }
        Ok(px)
    }

    /// Account for decoding a `w` x `h` bitmap.
    pub(crate) fn bitmap(&mut self, w: u64, h: u64) -> Result<()> {
        // A zero-width bitmap still costs a loop over its rows.
        let px = self.size(w, h)?;
        // and every bitmap a small allocation.
        self.charge(px.max(h) + 1024)
    }

    pub(crate) fn symbols(&mut self, n: u64) -> Result<()> {
        if n > u64::from(self.limits.max_symbols) {
            return Err(Error::LimitExceeded("max_symbols"));
        }
        // Eight bytes each in a list.
        self.charge(n * 64)
    }
}
