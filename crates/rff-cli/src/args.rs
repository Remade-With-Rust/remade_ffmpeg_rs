//! FFmpeg-compatible argument parsing.
//!
//! FFmpeg's CLI is not a flat flag list — it's a small grammar:
//!
//! ```text
//!   ffmpeg [global opts] {[input opts] -i INPUT}... {[output opts] OUTPUT}...
//! ```
//!
//! Options can carry a *stream specifier* after a colon — `-c:v libx264` means
//! "codec, video stream". This module parses that grammar into a neutral
//! [`Cli`] and, for transcode invocations, builds an [`rff::transcode::TranscodeSpec`].
//!
//! This is a deliberately pragmatic subset: the common options people actually
//! type. Unknown options are skipped with a warning rather than aborting, and
//! the recognised set is easy to extend — add an arm to the match in [`parse`].

use std::path::PathBuf;

use rff::transcode::{InputSpec, MapSelector, MapSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff_core::{CodecId, Dictionary, MediaType};

/// Parse a `-map` value like `0`, `0:v`, `0:a`, or `0:2`.
fn parse_map(spec: &str) -> Option<MapSpec> {
    let mut parts = spec.split(':');
    let input: usize = parts.next()?.parse().ok()?;
    let selector = match parts.next() {
        None => MapSelector::All,
        Some("v") | Some("V") => MapSelector::Kind(MediaType::Video),
        Some("a") => MapSelector::Kind(MediaType::Audio),
        Some(idx) => MapSelector::Index(idx.parse().ok()?),
    };
    Some(MapSpec { input, selector })
}

/// What the user actually asked `ffmpeg` to do. Informational sub-commands
/// (`-version`, `-codecs`, ...) short-circuit a transcode.
pub enum Action {
    Version,
    Help,
    ListCodecs,
    ListFormats,
    Transcode(TranscodeSpec),
}

/// A fully parsed command line.
pub struct Cli {
    pub hide_banner: bool,
    pub loglevel: Option<String>,
    /// Non-fatal parse notes (unknown options, ignored values) to print to stderr.
    pub warnings: Vec<String>,
    pub action: Action,
}

/// Parse `args` (everything after the program name) into a [`Cli`].
///
/// Returns `Err` only for hard syntax errors (e.g. an option missing its
/// required value); soft problems become [`Cli::warnings`].
pub fn parse(args: &[String]) -> Result<Cli, String> {
    let mut hide_banner = false;
    let mut loglevel = None;
    let mut warnings = Vec::new();

    // Informational sub-commands win over a transcode if present.
    let mut action_override: Option<Action> = None;

    // --- transcode accumulators ---
    let mut inputs: Vec<InputSpec> = Vec::new();
    let mut pending_input_format: Option<String> = None;
    let mut out_format: Option<String> = None;
    let mut video_codec: Option<CodecId> = None;
    let mut audio_codec: Option<CodecId> = None;
    let mut video_opts = Dictionary::new();
    let mut audio_opts = Dictionary::new();
    let mut video_filters: Option<String> = None;
    let mut maps: Vec<MapSpec> = Vec::new();
    let mut overwrite = false;
    let mut output_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        let Some(opt) = arg.strip_prefix('-') else {
            // A bare token is a positional argument: the output file.
            if output_path.is_some() {
                warnings.push(format!(
                    "multiple output files given; using the last (`{arg}`)"
                ));
            }
            output_path = Some(PathBuf::from(arg));
            i += 1;
            continue;
        };

        // Split an option into its base name and optional stream specifier:
        // `c:v:0` -> ("c", Some("v:0")).
        let (base, spec) = match opt.split_once(':') {
            Some((b, s)) => (b, Some(s)),
            None => (opt, None),
        };

        match base {
            "version" => action_override = Some(Action::Version),
            "h" | "help" | "?" => action_override = Some(Action::Help),
            "codecs" | "encoders" | "decoders" => action_override = Some(Action::ListCodecs),
            "formats" | "muxers" | "demuxers" => action_override = Some(Action::ListFormats),
            "hide_banner" => hide_banner = true,
            "y" => overwrite = true,
            "n" => overwrite = false,
            "loglevel" | "v" => loglevel = Some(take_value(args, &mut i, arg)?),

            "i" => {
                let path = take_value(args, &mut i, arg)?;
                inputs.push(InputSpec {
                    path: PathBuf::from(path),
                    format: pending_input_format.take(),
                });
            }

            // `-f` applies to the next input if we haven't reached outputs yet,
            // otherwise to the output.
            "f" => {
                let fmt = take_value(args, &mut i, arg)?;
                if inputs.is_empty() {
                    pending_input_format = Some(fmt);
                } else {
                    out_format = Some(fmt);
                }
            }

            // Codec selection: -c / -codec (optionally :v / :a), and the legacy
            // -vcodec / -acodec aliases.
            "c" | "codec" => {
                let name = take_value(args, &mut i, arg)?;
                apply_codec(spec, &name, &mut video_codec, &mut audio_codec, &mut warnings);
            }
            "vcodec" => {
                let name = take_value(args, &mut i, arg)?;
                apply_codec(Some("v"), &name, &mut video_codec, &mut audio_codec, &mut warnings);
            }
            "acodec" => {
                let name = take_value(args, &mut i, arg)?;
                apply_codec(Some("a"), &name, &mut video_codec, &mut audio_codec, &mut warnings);
            }

            // Stream selection: -map INPUT[:v|:a|:N] (repeatable).
            "map" => {
                let value = take_value(args, &mut i, arg)?;
                match parse_map(&value) {
                    Some(m) => maps.push(m),
                    None => warnings.push(format!("ignoring invalid -map `{value}`")),
                }
            }

            // Video filter graph: -vf / -filter:v.
            "vf" => video_filters = Some(take_value(args, &mut i, arg)?),
            "filter" => {
                let value = take_value(args, &mut i, arg)?;
                match spec {
                    Some(s) if s.starts_with('v') => video_filters = Some(value),
                    Some(s) if s.starts_with('a') => {
                        warnings.push("audio filters (-filter:a) are not supported yet".into())
                    }
                    _ => warnings.push(format!("ignoring filter spec `{value}`")),
                }
            }

            // Bitrate: -b:v / -b:a (bare -b defaults to video).
            "b" => {
                let value = take_value(args, &mut i, arg)?;
                match spec {
                    Some(s) if s.starts_with('a') => audio_opts.set("b", value),
                    _ => video_opts.set("b", value),
                }
            }

            // Anything else: accept gracefully. Best-effort consume a trailing
            // value so we don't mistake it for the output path.
            _ => {
                warnings.push(format!("unrecognized option `-{opt}` (ignored)"));
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') {
                        i += 1;
                    }
                }
            }
        }

        i += 1;
    }

    if let Some(action) = action_override {
        return Ok(Cli {
            hide_banner,
            loglevel,
            warnings,
            action,
        });
    }

    // Warn about codec options that have no codec to attach to.
    if !video_opts.is_empty() && video_codec.is_none() {
        warnings.push("video options given without -c:v; ignored".into());
    }
    if !audio_opts.is_empty() && audio_codec.is_none() {
        warnings.push("audio options given without -c:a; ignored".into());
    }

    let output = output_path.map(|path| OutputSpec {
        path,
        format: out_format,
        video_codec: video_codec.map(|codec| StreamCodec {
            codec,
            options: video_opts,
        }),
        audio_codec: audio_codec.map(|codec| StreamCodec {
            codec,
            options: audio_opts,
        }),
        video_filters,
        maps,
        overwrite,
    });

    let spec = TranscodeSpec {
        inputs,
        outputs: output.into_iter().collect(),
    };

    Ok(Cli {
        hide_banner,
        loglevel,
        warnings,
        action: Action::Transcode(spec),
    })
}

/// Consume and return the value following an option, advancing the cursor.
fn take_value(args: &[String], i: &mut usize, opt: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("option `{opt}` requires an argument"))
}

/// Resolve a codec name to an id (treating `copy` as "no re-encode" → `None`)
/// and assign it to the slot selected by the stream specifier.
fn apply_codec(
    spec: Option<&str>,
    name: &str,
    video: &mut Option<CodecId>,
    audio: &mut Option<CodecId>,
    warnings: &mut Vec<String>,
) {
    if name == "copy" {
        // Stream copy: leave the slot unset; the pipeline will passthrough.
        return;
    }
    let Some(id) = CodecId::from_name(name) else {
        warnings.push(format!("unknown codec `{name}` (ignored)"));
        return;
    };
    match spec {
        Some(s) if s.starts_with('v') => *video = Some(id),
        Some(s) if s.starts_with('a') => *audio = Some(id),
        Some(s) if s.starts_with('s') => { /* subtitle codecs: not yet modeled */ }
        // No specifier: apply to whichever media type this codec is.
        None => match id.media_type() {
            rff_core::MediaType::Video => *video = Some(id),
            rff_core::MediaType::Audio => *audio = Some(id),
            _ => {}
        },
        _ => {}
    }
}
