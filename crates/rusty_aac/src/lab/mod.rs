//! The AAC quality lab — the **verdict instrument** for the Great Gate campaign
//! (`docs/codec-aac-great-gate.md`).
//!
//! Before this module existed there was no way to judge an AAC encoder change at
//! all: no corpus, no metric, no ladder. Per `codec-tune-quality`, that made every
//! quality claim unbankable, which is why P0 blocks every rung.
//!
//! Pieces:
//! * [`corpus`] — the deterministic 8-class content corpus, including the two
//!   synthesized **gap classes** without which arms A4/A5 cannot be judged.
//! * [`quality`] — NMR in the encoder's own MDCT/SWB domain. The fast iteration
//!   screen; PEAQ remains the verdict.
//! * [`ladder`] — the per-clip × per-bitrate runner, plus the **null arm**.
//! * [`signals`] — the per-frame content-signal vector every gate reads (P1).
//!
//! Driven by `cargo run -p rusty_aac --features lab --example aacquality`.
//!
//! # The measurement contract
//!
//! Anything measured here must satisfy `codec-measurement` before it is acted on:
//! the null arm runs first, quality is judged per clip at ≥4 operating points, and
//! any *timing* number must come from a pinned run under our `rusty_alloc` global
//! allocator — which is why the example and bench roots declare it and the library
//! deliberately does not.

pub mod corpus;
pub mod ladder;
pub mod quality;
pub mod signals;
pub mod wav;

pub use corpus::{corpus, Class, Signal};
pub use ladder::{null_arm, point, run, Point};
pub use quality::{track_nmr, NmrReport};
pub use wav::Wav;
pub use signals::{AacSignals, FrameSignals};
