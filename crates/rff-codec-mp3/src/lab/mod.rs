//! MP3 encoder experiment harness — "the lab".
//!
//! A repeatable framework to track every encoder brick and tune the experimental
//! ones on the fly. The encoder splits into two regimes and this harness serves
//! both:
//!
//! * **Conformance bricks** (Foundation, analysis, coding) — one correct answer,
//!   proven by a bit-exact round-trip through our own decoder. Metric:
//!   `max_abs_err == 0`.
//! * **Quality bricks** (the psychoacoustic model + distortion loop) — no single
//!   right answer. Tuned by sweeping variants and tracking PSNR / noise-to-mask.
//!
//! Pieces:
//! * [`bricks`] — the canonical manifest of every brick + its status (mirrors
//!   `docs/mp3-encoder-plan.md`, but in typed code so it can't drift).
//! * [`signals`] — a deterministic corpus (no fixtures, no `rand`).
//! * [`metrics`] — how output is scored against a reference.
//! * [`variant`] — named parameter presets per brick; the "modify on the fly" knob.
//! * [`experiment`] — run a brick+variant over the corpus → a repeatable report.
//!
//! Driven by `cargo run -p rff-codec-mp3 --features lab --example mp3lab`.
//! See `docs/mp3-lab.md`.

pub mod bricks;
pub mod experiment;
pub mod metrics;
pub mod quantizer;
pub mod signals;
pub mod variant;

pub use bricks::{Brick, Class, Phase, Status, Verify, BRICKS};
pub use experiment::{run, Overrides, Report};
pub use metrics::Metrics;
pub use signals::{corpus, Signal};
