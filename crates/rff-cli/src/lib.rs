//! `rff-cli` — the command-line front-ends.
//!
//! Two thin binaries, [`ffmpeg`](crate::ffmpeg) and [`ffprobe`](crate::ffprobe),
//! that parse FFmpeg-compatible arguments and call straight into the `rff`
//! engine API. All the real work lives in `rff`; this crate is just argument
//! grammar + terminal output.

pub mod args;
pub mod ffmpeg;
pub mod ffprobe;

// Primary allocator for every rff-cli binary (rff/rffprobe/ffmpeg/ffprobe):
// our rusty_alloc, the pure-Rust mimalloc remake. Set here in the lib root so
// all four [[bin]] shims inherit it from one place. This crate is the binary
// front-end only — never a dependency of other libraries — so declaring the
// global allocator here does not impose it on external consumers.
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;
