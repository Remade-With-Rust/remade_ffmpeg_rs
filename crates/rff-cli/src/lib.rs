//! `rff-cli` — the command-line front-ends.
//!
//! Two thin binaries, [`ffmpeg`](crate::ffmpeg) and [`ffprobe`](crate::ffprobe),
//! that parse FFmpeg-compatible arguments and call straight into the `rff`
//! engine API. All the real work lives in `rff`; this crate is just argument
//! grammar + terminal output.

pub mod args;
pub mod ffmpeg;
pub mod ffprobe;
