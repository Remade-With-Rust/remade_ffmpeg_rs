//! Feature-gated stage profiler. Zero-cost when `profile` is off: every scope
//! compiles to nothing and no state exists.
//!
//! Priced before it was placed, per the measurement discipline: the scopes here
//! are **per row**, not per pixel or per symbol. A 1920x4320 encode takes ~4,320
//! rows x 2 scopes = ~8,600 `Instant::now()` pairs at ~25 ns, i.e. ~0.4 ms
//! against a ~600 ms encode — under 0.1%. A per-pixel tap would have been
//! ~25 million calls and would have dwarfed the thing being measured.
//!
//! Read the residue: `total - sum(stages)` is printed by the harness. If it is
//! large, there is a stage that has no scope, not free work.

#[cfg(feature = "profile")]
mod imp {
    use std::cell::RefCell;
    use std::time::Instant;

    /// Stage buckets. Kept as a fixed list so lookup is an index, not a hash.
    pub const STAGES: &[&str] = &[
        "enc.filter",
        "enc.deflate",
        "enc.chunk",
        "dec.inflate",
        "dec.unfilter",
        "dec.transform",
    ];

    thread_local! {
        static ACC: RefCell<[(f64, u64); 6]> = const { RefCell::new([(0.0, 0); 6]) };
    }

    pub struct Scope {
        idx: usize,
        start: Instant,
    }

    impl Scope {
        #[inline]
        pub fn new(idx: usize) -> Self {
            Scope {
                idx,
                start: Instant::now(),
            }
        }
    }

    impl Drop for Scope {
        #[inline]
        fn drop(&mut self) {
            let ns = self.start.elapsed().as_nanos() as f64;
            let i = self.idx;
            ACC.with(|a| {
                let mut a = a.borrow_mut();
                a[i].0 += ns;
                a[i].1 += 1;
            });
        }
    }

    /// Clear all buckets. Call between timed repetitions.
    pub fn reset() {
        ACC.with(|a| *a.borrow_mut() = [(0.0, 0); 6]);
    }

    /// `(stage, milliseconds, call count)` for every non-empty bucket.
    pub fn dump() -> Vec<(&'static str, f64, u64)> {
        ACC.with(|a| {
            let a = a.borrow();
            STAGES
                .iter()
                .enumerate()
                .filter(|(i, _)| a[*i].1 > 0)
                .map(|(i, name)| (*name, a[i].0 / 1e6, a[i].1))
                .collect()
        })
    }
}

#[cfg(not(feature = "profile"))]
mod imp {
    pub const STAGES: &[&str] = &[];
    pub struct Scope;
    impl Scope {
        #[inline(always)]
        pub fn new(_idx: usize) -> Self {
            Scope
        }
    }
    pub fn reset() {}
    pub fn dump() -> Vec<(&'static str, f64, u64)> {
        Vec::new()
    }
}

pub use imp::{dump, reset, Scope, STAGES};

pub const ENC_FILTER: usize = 0;
pub const ENC_DEFLATE: usize = 1;
pub const ENC_CHUNK: usize = 2;
pub const DEC_INFLATE: usize = 3;
pub const DEC_UNFILTER: usize = 4;
pub const DEC_TRANSFORM: usize = 5;

/// Open a stage scope for the enclosing block. Compiles away entirely when the
/// `profile` feature is off.
#[macro_export]
macro_rules! prof_scope {
    ($idx:expr) => {
        let _rusty_png_scope = $crate::prof::Scope::new($idx);
    };
}
