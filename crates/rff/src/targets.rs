//! Conversion targets — "what can this file be turned into?"
//!
//! [`probe`](crate::probe::probe) says what an input *is*; this says what the
//! engine can write from it. The whole answer comes out of the same registries
//! the transcoder uses, so it cannot drift away from what a conversion would
//! actually do.
//!
//! ```no_run
//! let engine = rff::Engine::new();
//! let plan = rff::targets::targets(&engine, "clip.mp4")?;
//! for t in &plan.targets {
//!     println!("{:<6} {}", t.extension, t.summary());
//! }
//! # Ok::<(), rff_core::Error>(())
//! ```

use std::path::Path;

use rff_core::Result;

pub use rff_targets::{
    format_matrix, is_lossless, matrix_to_json, readable_extensions, Action, DropReason, Fidelity,
    FormatSupport, Plan, SourceStream, StreamPlan, Target, TargetKind,
};

use crate::Engine;

/// Probe `path`, then enumerate every output container the engine can write
/// for it, best first.
///
/// Fails only when the input cannot be read at all — an input we *can* read
/// always has at least one target, since it can be remuxed into a container
/// that accepts its own codecs.
pub fn targets(engine: &Engine, path: impl AsRef<Path>) -> Result<Plan> {
    let info = crate::probe::probe(engine, path)?;
    let mut plan = for_streams(
        engine,
        &info
            .streams
            .iter()
            .map(|s| SourceStream::new(s.index, s.media_type, s.codec_id))
            .collect::<Vec<_>>(),
    );
    plan.source_format = Some(info.format_name);
    Ok(plan)
}

/// Enumerate targets for streams a caller already has — from a previous probe,
/// a database row, or a UI form. No file is opened.
pub fn for_streams(engine: &Engine, source: &[SourceStream]) -> Plan {
    rff_targets::plan(&engine.codecs, &engine.formats, source)
}

/// Every container this build can read or write, independent of any input.
pub fn matrix(engine: &Engine) -> Vec<FormatSupport> {
    format_matrix(&engine.formats)
}
