//! Transcoding — the engine side of `ffmpeg`.
//!
//! The pipeline is the classic FFmpeg shape:
//!
//! ```text
//!   demuxer ─▶ decoder ─▶ [filters] ─▶ encoder ─▶ muxer
//!   (input)    (raw frames pass through the middle)      (output)
//! ```
//!
//! [`TranscodeSpec`] is the declarative description of a job — inputs, an
//! output, and the codecs to use. [`run`] resolves that spec against an
//! [`Engine`] (finding the demuxer, decoders, encoders, and muxer) and then
//! drives the loop.
//!
//! The *resolution* step is real and useful today: it validates that every
//! requested codec/container actually exists and is registered, returning a
//! precise error if not. The *drive* step bottoms out in the scaffolded
//! codecs, so an end-to-end run currently stops at the first
//! [`Unimplemented`](rff_core::Error::Unimplemented) — by design, until the
//! codec bodies land.

use std::path::PathBuf;

use rff_core::{CodecId, Dictionary, Error, Result};

use crate::Engine;

/// One input file for a job.
#[derive(Debug, Clone)]
pub struct InputSpec {
    pub path: PathBuf,
    /// Force a container format instead of guessing from the extension
    /// (`ffmpeg -f <fmt> -i ...`).
    pub format: Option<String>,
}

/// Per-stream codec selection for the output.
#[derive(Debug, Clone)]
pub struct StreamCodec {
    pub codec: CodecId,
    /// Codec options (`-b:v 2M`, `-crf 23`, ...).
    pub options: Dictionary,
}

/// The output file for a job.
#[derive(Debug, Clone)]
pub struct OutputSpec {
    pub path: PathBuf,
    /// Force a container format instead of guessing from the extension
    /// (`ffmpeg -f <fmt> ...`).
    pub format: Option<String>,
    /// Video codec for the output, if a video stream is produced.
    pub video_codec: Option<StreamCodec>,
    /// Audio codec for the output, if an audio stream is produced.
    pub audio_codec: Option<StreamCodec>,
    /// Overwrite the output if it exists (`-y`); otherwise fail (`-n`).
    pub overwrite: bool,
}

/// A complete, declarative transcoding job.
#[derive(Debug, Clone, Default)]
pub struct TranscodeSpec {
    pub inputs: Vec<InputSpec>,
    pub outputs: Vec<OutputSpec>,
}

/// A successful run's summary (frames/packets moved, etc.). Fields will grow as
/// the pipeline does.
#[derive(Debug, Clone, Default)]
pub struct TranscodeReport {
    pub packets_written: u64,
    pub frames_decoded: u64,
}

/// Resolve and run a transcode job against `engine`.
///
/// Resolution validates the whole graph up front so failures are reported
/// before any work begins. See the module docs for what "run" currently does.
pub fn run(engine: &Engine, spec: &TranscodeSpec) -> Result<TranscodeReport> {
    if spec.inputs.is_empty() {
        return Err(Error::Option("no input files specified".into()));
    }
    if spec.outputs.is_empty() {
        return Err(Error::Option("no output file specified".into()));
    }

    // --- Resolve inputs: pick a demuxer for each. ---
    for input in &spec.inputs {
        let format_name = resolve_input_format(engine, input)?;
        // Confirm the format actually offers a demuxer.
        engine
            .formats
            .by_name(&format_name)
            .filter(|f| f.can_demux())
            .ok_or_else(|| Error::DemuxerNotFound(format_name.clone()))?;
    }

    // --- Resolve outputs: pick a muxer and validate every requested codec. ---
    for output in &spec.outputs {
        let format_name = resolve_output_format(engine, output)?;
        engine
            .formats
            .by_name(&format_name)
            .filter(|f| f.can_mux())
            .ok_or_else(|| Error::MuxerNotFound(format_name.clone()))?;

        if let Some(v) = &output.video_codec {
            // Will return EncoderNotFound if unregistered.
            let _ = engine.codecs.find_encoder(v.codec)?;
        }
        if let Some(a) = &output.audio_codec {
            let _ = engine.codecs.find_encoder(a.codec)?;
        }
    }

    // Graph is valid. Driving it bottoms out in scaffolded codecs for now.
    Err(Error::Unimplemented(
        "transcode pipeline: demux→decode→encode→mux drive loop",
    ))
}

/// Decide which container to demux an input as: explicit `-f`, else by extension.
fn resolve_input_format(engine: &Engine, input: &InputSpec) -> Result<String> {
    if let Some(forced) = &input.format {
        return Ok(forced.clone());
    }
    let ext = input
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    engine
        .formats
        .by_extension(ext)
        .map(|f| f.name.to_string())
        .ok_or_else(|| Error::DemuxerNotFound(input.path.display().to_string()))
}

/// Decide which container to mux an output as: explicit `-f`, else by extension.
fn resolve_output_format(engine: &Engine, output: &OutputSpec) -> Result<String> {
    if let Some(forced) = &output.format {
        return Ok(forced.clone());
    }
    let ext = output
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    engine
        .formats
        .by_extension(ext)
        .map(|f| f.name.to_string())
        .ok_or_else(|| Error::MuxerNotFound(output.path.display().to_string()))
}
