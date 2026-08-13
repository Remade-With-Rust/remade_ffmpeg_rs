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
    // Sample format pinned by the codec NAME (`-c:a pcm_s16le`) or by an
    // explicit `-sample_fmt`. Kept separate from the codec id, which is
    // format-agnostic by design.
    let mut audio_sample_format: Option<rff_core::SampleFormat> = None;
    let mut video_opts = Dictionary::new();
    let mut audio_opts = Dictionary::new();
    // Options given WITHOUT a `:v`/`:a` stream specifier that more than one
    // codec kind understands (FFmpeg treats `-compression_level` this way).
    // Held aside and attached after parsing, once we know which codecs actually
    // exist — attaching eagerly to both made every `-compression_level` on an
    // image encode emit a spurious "audio options given without -c:a" warning.
    let mut shared_opts = Dictionary::new();
    let mut max_video_frames: Option<u64> = None;
    let mut video_filters: Option<String> = None;
    let mut audio_filters: Option<String> = None;
    let mut filter_complex: Option<String> = None;
    let mut maps: Vec<MapSpec> = Vec::new();
    let mut overwrite = false;
    let mut output_path: Option<PathBuf> = None;
    // Trim window: -ss (start), -t (duration), -to (absolute end).
    let mut trim_start: Option<f64> = None;
    let mut trim_duration: Option<f64> = None;
    let mut trim_to: Option<f64> = None;
    let mut frame_rate: Option<(u32, u32)> = None;
    let mut audio_rate: Option<u32> = None;
    let mut audio_channels: Option<u16> = None;
    let mut metadata = Dictionary::new();
    let mut format_options = Dictionary::new();

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
                apply_codec(
                    spec,
                    &name,
                    &mut video_codec,
                    &mut audio_codec,
                    &mut audio_sample_format,
                    &mut warnings,
                );
            }
            "vcodec" => {
                let name = take_value(args, &mut i, arg)?;
                apply_codec(
                    Some("v"),
                    &name,
                    &mut video_codec,
                    &mut audio_codec,
                    &mut audio_sample_format,
                    &mut warnings,
                );
            }
            "acodec" => {
                let name = take_value(args, &mut i, arg)?;
                apply_codec(
                    Some("a"),
                    &name,
                    &mut video_codec,
                    &mut audio_codec,
                    &mut audio_sample_format,
                    &mut warnings,
                );
            }

            // Stream selection: -map INPUT[:v|:a|:N] (repeatable).
            "map" => {
                let value = take_value(args, &mut i, arg)?;
                match parse_map(&value) {
                    Some(m) => maps.push(m),
                    None => warnings.push(format!("ignoring invalid -map `{value}`")),
                }
            }

            // Video filter graph: -vf / -filter:v. Audio: -af / -filter:a.
            "vf" => video_filters = Some(take_value(args, &mut i, arg)?),
            "af" => audio_filters = Some(take_value(args, &mut i, arg)?),
            // Multi-input filter graph: -filter_complex / -lavfi.
            "filter_complex" | "lavfi" => filter_complex = Some(take_value(args, &mut i, arg)?),
            "filter" => {
                let value = take_value(args, &mut i, arg)?;
                match spec {
                    Some(s) if s.starts_with('v') => video_filters = Some(value),
                    Some(s) if s.starts_with('a') => audio_filters = Some(value),
                    _ => warnings.push(format!("ignoring filter spec `{value}`")),
                }
            }

            // Trim window: -ss START, -t DURATION, -to END (`HH:MM:SS.mmm` or
            // seconds). Applied to the output: decode-and-discard, so it is
            // frame-accurate for transcodes and keyframe-cut for -c copy.
            "ss" => trim_start = Some(parse_time(&take_value(args, &mut i, arg)?)?),
            "t" => trim_duration = Some(parse_time(&take_value(args, &mut i, arg)?)?),
            "to" => trim_to = Some(parse_time(&take_value(args, &mut i, arg)?)?),

            // -r RATE: constant output frame rate (25, 29.97, 30000/1001).
            "r" | "framerate" => frame_rate = Some(parse_rate(&take_value(args, &mut i, arg)?)?),

            // -s WxH: shorthand for a trailing scale filter.
            "s" | "video_size" => {
                let value = take_value(args, &mut i, arg)?;
                let (w, h) = value
                    .split_once(['x', 'X'])
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                    .ok_or_else(|| format!("-s: expected WxH, got `{value}`"))?;
                let scale = format!("scale={w}:{h}");
                video_filters = Some(match video_filters.take() {
                    Some(chain) => format!("{chain},{scale}"),
                    None => scale,
                });
            }

            // -ar RATE / -ac CHANNELS: audio output rate and channel count.
            "ar" => {
                let value = take_value(args, &mut i, arg)?;
                audio_rate = Some(
                    value
                        .parse()
                        .map_err(|_| format!("-ar: bad sample rate `{value}`"))?,
                );
            }
            "ac" => {
                let value = take_value(args, &mut i, arg)?;
                audio_channels = Some(
                    value
                        .parse()
                        .map_err(|_| format!("-ac: bad channel count `{value}`"))?,
                );
            }

            // HLS segmenting: -hls_time SECONDS. (-hls_list_size shapes LIVE
            // sliding-window playlists; our HLS output is VOD, so say so.)
            "hls_time" => {
                let value = take_value(args, &mut i, arg)?;
                format_options.set("hls_time", value);
            }
            "hls_list_size" => {
                let _ = take_value(args, &mut i, arg)?;
                warnings.push("-hls_list_size: HLS output is a VOD playlist; ignored".into());
            }

            // -metadata key=value (repeatable).
            "metadata" => {
                let value = take_value(args, &mut i, arg)?;
                match value.split_once('=') {
                    Some((k, v)) => metadata.set(k.trim(), v),
                    None => warnings.push(format!("-metadata: expected key=value, got `{value}`")),
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

            // Rate control / tuning (video by default; `:a` targets audio):
            // -crf / -qp / -q / -qscale (quality), -preset (speed<->quality), -pass (1|2),
            // -cpu-used / -speed (VP9 speed preset 0 best..4 fastest).
            // `-frames:v N` / `-frames N` — stop after N video frames. Needed for
            // any rate-vs-quality measurement over a clip prefix: without it a
            // harness silently encodes the whole input while scoring a prefix.
            "frames" | "vframes" => {
                let value = take_value(args, &mut i, arg)?;
                match value.parse::<u64>() {
                    Ok(n) => max_video_frames = Some(n),
                    Err(_) => warnings.push(format!("ignoring non-numeric -{arg} {value}")),
                }
            }

            // JPEG/MJPEG private options default to video (an image codec has no
            // audio side): -jpeg_quality 1..100, -sampling 444|440|422|420|411,
            // -progressive, -optimize_huffman, -restart_interval, -trellis.
            // `-pred` is PNG's filter/prediction knob (none/sub/up/avg/paeth/
            // mixed), spelled as FFmpeg's PNG encoder spells it. It is video-only
            // by nature — there is no audio codec that takes it.
            "crf" | "qp" | "preset" | "pass" | "q" | "qscale" | "cpu-used" | "speed" | "lag"
            | "lag-in-frames" | "arnr-strength" | "dispatch-budget" | "jpeg_quality"
            | "sampling" | "jpeg_sampling" | "progressive" | "optimize_huffman"
            | "restart_interval" | "trellis" | "pred" | "png_auto_type" | "png_auto_config" => {
                let value = take_value(args, &mut i, arg)?;
                if base == "pass" && value != "1" {
                    warnings
                        .push("two-pass (-pass 2) is parsed but runs single-pass for now".into());
                }
                match spec {
                    Some(s) if s.starts_with('a') => audio_opts.set(base, value),
                    _ => video_opts.set(base, value),
                }
            }

            // `-compression_level` is not audio-only: FFmpeg treats it as a
            // generic codec option, and PNG uses it (0..9) exactly as Opus does
            // (0..10). Unscoped it therefore has to reach BOTH dictionaries, or
            // `rff -i in.png -c:v png -compression_level 9 out.png` silently
            // encodes at the default — which is what it used to do. `:v` / `:a`
            // still target one side.
            "compression_level" | "threads" => {
                let value = take_value(args, &mut i, arg)?;
                match spec {
                    Some(s) if s.starts_with('v') => video_opts.set(base, value),
                    Some(s) if s.starts_with('a') => audio_opts.set(base, value),
                    _ => shared_opts.set(base, value),
                }
            }

            // Codec private / tuning options forwarded to the encoder's
            // `configure` Dictionary (audio by default; `:v` targets video).
            // Includes Opus: -application, -vbr, and the R1 frame-parallel
            // controls -opus_parallel / -opus_warmup / -threads.
            "application" | "vbr" | "opus_parallel" | "opus_warmup" | "frame_duration" => {
                let value = take_value(args, &mut i, arg)?;
                match spec {
                    Some(s) if s.starts_with('v') => video_opts.set(base, value),
                    _ => audio_opts.set(base, value),
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

    // Attach the unscoped shared options to whichever codecs are actually
    // present. An explicit `:v`/`:a` value already set above wins over these.
    for (key, value) in shared_opts.iter() {
        if video_codec.is_some() && video_opts.get(key).is_none() {
            video_opts.set(key, value);
        }
        if audio_codec.is_some() && audio_opts.get(key).is_none() {
            audio_opts.set(key, value);
        }
    }

    // Warn about codec options that have no codec to attach to.
    if !video_opts.is_empty() && video_codec.is_none() {
        warnings.push("video options given without -c:v; ignored".into());
    }
    if !audio_opts.is_empty() && audio_codec.is_none() {
        warnings.push("audio options given without -c:a; ignored".into());
    }
    // Re-encode-only options on a stream-copy: say so instead of silence.
    if audio_codec.is_none()
        && (audio_filters.is_some() || audio_rate.is_some() || audio_channels.is_some())
    {
        warnings.push("-af/-ar/-ac need a re-encode; pass -c:a (ignored on stream copy)".into());
    }

    // `-vf fps=N` is the same CFR stage as `-r`: lift it out of the chain.
    if let Some(vf) = &video_filters {
        let mut kept: Vec<&str> = Vec::new();
        for token in vf.split(',') {
            match token.trim().strip_prefix("fps=") {
                Some(rate) => match parse_rate(rate) {
                    Ok(r) => frame_rate = Some(r),
                    Err(e) => warnings.push(e),
                },
                None => kept.push(token),
            }
        }
        if kept.len() != vf.split(',').count() {
            let kept = kept.join(",");
            video_filters = (!kept.trim().is_empty()).then_some(kept);
        }
    }
    if frame_rate.is_some() && video_codec.is_none() {
        warnings.push("-r/-vf fps= need a re-encode; pass -c:v (ignored on stream copy)".into());
    }

    // -ss/-t/-to resolve to an absolute [start, end) window; -to wins over -t
    // only when both name the same bound (FFmpeg takes the earlier end).
    let trim_end = match (trim_to, trim_duration) {
        (Some(to), Some(t)) => Some(to.min(trim_start.unwrap_or(0.0) + t)),
        (Some(to), None) => Some(to),
        (None, Some(t)) => Some(trim_start.unwrap_or(0.0) + t),
        (None, None) => None,
    };

    let output = output_path.map(|path| OutputSpec {
        path,
        format: out_format,
        video_codec: video_codec.map(|codec| StreamCodec {
            codec,
            options: video_opts,
            sample_format: None,
        }),
        audio_codec: audio_codec.map(|codec| StreamCodec {
            codec,
            options: audio_opts,
            sample_format: audio_sample_format,
        }),
        video_filters,
        filter_complex,
        max_video_frames,
        maps,
        overwrite,
        trim_start,
        trim_end,
        frame_rate,
        audio_rate,
        audio_channels,
        audio_filters,
        metadata,
        format_options,
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

/// Parse an FFmpeg time value: plain seconds (`12.5`) or `[HH:]MM:SS[.mmm]`.
fn parse_time(value: &str) -> Result<f64, String> {
    let value = value.trim();
    let bad = || format!("bad time `{value}` (want seconds or [HH:]MM:SS[.mmm])");
    if !value.contains(':') {
        return value.parse::<f64>().map_err(|_| bad());
    }
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() > 3 || parts.is_empty() {
        return Err(bad());
    }
    let mut seconds = 0.0;
    for p in &parts {
        seconds = seconds * 60.0 + p.parse::<f64>().map_err(|_| bad())?;
    }
    Ok(seconds)
}

/// Parse a frame rate: integer (`25`), decimal (`29.97`), or fraction
/// (`30000/1001`). Decimals become exact fractions over a power of ten.
fn parse_rate(value: &str) -> Result<(u32, u32), String> {
    let value = value.trim();
    let bad = || format!("bad frame rate `{value}`");
    if let Some((n, d)) = value.split_once('/') {
        let (n, d) = (
            n.trim().parse::<u32>().map_err(|_| bad())?,
            d.trim().parse::<u32>().map_err(|_| bad())?,
        );
        if n == 0 || d == 0 {
            return Err(bad());
        }
        return Ok((n, d));
    }
    if let Some((int, frac)) = value.split_once('.') {
        let digits = frac.len().min(3) as u32;
        let den = 10u32.pow(digits);
        let int: u32 = int.parse().map_err(|_| bad())?;
        let frac: u32 = frac[..digits as usize].parse().map_err(|_| bad())?;
        let num = int * den + frac;
        if num == 0 {
            return Err(bad());
        }
        return Ok((num, den));
    }
    let n: u32 = value.parse().map_err(|_| bad())?;
    if n == 0 {
        return Err(bad());
    }
    Ok((n, 1))
}

/// Resolve a codec name to an id (treating `copy` as "no re-encode" → `None`)
/// and assign it to the slot selected by the stream specifier.
fn apply_codec(
    spec: Option<&str>,
    name: &str,
    video: &mut Option<CodecId>,
    audio: &mut Option<CodecId>,
    audio_format: &mut Option<rff_core::SampleFormat>,
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
    // `pcm_s16le` vs `pcm_f32le` differ only in the name; keep what it pins.
    let pinned = CodecId::sample_format_from_name(name);
    match spec {
        Some(s) if s.starts_with('v') => *video = Some(id),
        Some(s) if s.starts_with('a') => {
            *audio = Some(id);
            if pinned.is_some() {
                *audio_format = pinned;
            }
        }
        Some(s) if s.starts_with('s') => { /* subtitle codecs: not yet modeled */ }
        // No specifier: apply to whichever media type this codec is.
        None => match id.media_type() {
            rff_core::MediaType::Video => *video = Some(id),
            rff_core::MediaType::Audio => {
                *audio = Some(id);
                if pinned.is_some() {
                    *audio_format = pinned;
                }
            }
            _ => {}
        },
        _ => {}
    }
}
