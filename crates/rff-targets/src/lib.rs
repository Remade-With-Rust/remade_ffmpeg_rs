//! `rff-targets` — "what can this file be converted into?"
//!
//! Given the streams of an input, this crate enumerates every container the
//! engine can actually *write* for it, and says for each one what happens to
//! each stream: a byte-exact stream copy, a re-encode (and whether that
//! re-encode is lossy), or a drop the container forces.
//!
//! The answer is derived from the engine's own registries — [`MuxCaps`] on each
//! registered [`Format`](rff_format::Format), and the encoder/decoder factories
//! on each registered [`Codec`](rff_codec::Codec). Nothing here is a hand-kept
//! list of file extensions, so a build with fewer codecs compiled in reports
//! fewer targets, automatically and correctly.
//!
//! ```no_run
//! # use rff_codec::CodecRegistry;
//! # use rff_format::FormatRegistry;
//! # let (codecs, formats) = (CodecRegistry::new(), FormatRegistry::new());
//! # let streams: Vec<rff_format::Stream> = Vec::new();
//! let source: Vec<rff_targets::SourceStream> = streams.iter().map(Into::into).collect();
//! for target in rff_targets::plan(&codecs, &formats, &source).targets {
//!     println!("{:<10} {}", target.extension, target.summary());
//! }
//! ```
//!
//! The result is plain data plus a [`to_json`](Plan::to_json) rendering, so a
//! CLI, an HTTP handler and a browser UI all consume the same answer instead of
//! each keeping its own conversion table.

use std::fmt;

use rff_codec::CodecRegistry;
use rff_core::{CodecId, MediaType};
use rff_format::{FormatRegistry, MuxCaps};

mod json;
pub use json::matrix_to_json;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// One stream of the input, reduced to what target selection depends on.
///
/// Deliberately smaller than [`rff_format::Stream`] so callers can build it
/// from a probe result, a database row, or a UI form — anything that knows a
/// media type and a codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceStream {
    pub index: usize,
    pub media_type: MediaType,
    pub codec_id: CodecId,
}

impl SourceStream {
    pub fn new(index: usize, media_type: MediaType, codec_id: CodecId) -> SourceStream {
        SourceStream {
            index,
            media_type,
            codec_id,
        }
    }
}

impl From<&rff_format::Stream> for SourceStream {
    fn from(s: &rff_format::Stream) -> SourceStream {
        SourceStream {
            index: s.index,
            media_type: s.media_type,
            codec_id: s.codec_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// What the engine can produce from one input: every writable container, best
/// first.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The input streams the plan was computed for.
    pub source: Vec<SourceStream>,
    /// Short name of the input container, when the caller knew it.
    pub source_format: Option<String>,
    /// Every container that can be written, ordered by [`plan`]'s ranking.
    pub targets: Vec<Target>,
}

impl Plan {
    /// The targets that need no re-encoding at all.
    pub fn stream_copies(&self) -> impl Iterator<Item = &Target> {
        self.targets.iter().filter(|t| t.fidelity == Fidelity::Copy)
    }

    /// Targets of one kind (all the image outputs, all the audio outputs, ...).
    pub fn of_kind(&self, kind: TargetKind) -> impl Iterator<Item = &Target> + '_ {
        self.targets.iter().filter(move |t| t.kind == kind)
    }

    /// Look one target up by its format name (`"webm"`).
    pub fn target(&self, format: &str) -> Option<&Target> {
        self.targets.iter().find(|t| t.format == format)
    }
}

/// One output container the engine can write for this input.
#[derive(Debug, Clone)]
pub struct Target {
    /// Format name as `-f` takes it (`mp4`, `matroska`, `yuv4mpegpipe`).
    pub format: &'static str,
    pub long_name: &'static str,
    /// The extension to offer by default (the format's first).
    pub extension: &'static str,
    /// Every extension that selects this format.
    pub extensions: &'static [&'static str],
    pub kind: TargetKind,
    /// What happens to each input stream, in input order.
    pub streams: Vec<StreamPlan>,
    pub fidelity: Fidelity,
    /// Anything a caller should say out loud before writing this: dropped
    /// streams, still-image truncation, generation loss, multi-file output.
    pub notes: Vec<String>,
    /// A ready-to-run argument list, minus the input and output paths:
    /// `["-c:v", "copy", "-c:a", "opus"]`. Goes between `-i <input>` and the
    /// output path.
    pub args: Vec<String>,
}

impl Target {
    /// Streams that survive into the output.
    pub fn kept(&self) -> impl Iterator<Item = &StreamPlan> {
        self.streams.iter().filter(|s| !s.action.is_drop())
    }

    /// Streams the container forces us to leave behind.
    pub fn dropped(&self) -> impl Iterator<Item = &StreamPlan> {
        self.streams.iter().filter(|s| s.action.is_drop())
    }

    /// What happens to the streams: `"h264 copy + aac->opus"`.
    pub fn stream_summary(&self) -> String {
        let mut parts: Vec<String> = self.kept().map(|s| s.summary()).collect();
        if parts.is_empty() {
            parts.push("nothing".into());
        }
        let mut s = parts.join(" + ");
        let dropped = self.dropped().count();
        if dropped > 0 {
            s.push_str(&format!(", {dropped} dropped"));
        }
        s
    }

    /// [`stream_summary`](Target::stream_summary) plus the fidelity verdict:
    /// `"h264 copy + aac->opus (lossy)"`.
    pub fn summary(&self) -> String {
        format!("{} ({})", self.stream_summary(), self.fidelity)
    }

    /// The full command a user would run, given concrete paths.
    pub fn command(&self, input: &str, output_stem: &str) -> String {
        let mut s = format!("rff -i {input}");
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        format!("{s} {output_stem}.{}", self.extension)
    }
}

/// The broad category an output belongs to — what a UI groups it under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    /// Moving pictures, with or without sound.
    Video,
    /// Sound only.
    Audio,
    /// A single picture.
    Image,
    /// Timed text.
    Subtitle,
}

impl TargetKind {
    pub fn name(self) -> &'static str {
        match self {
            TargetKind::Video => "video",
            TargetKind::Audio => "audio",
            TargetKind::Image => "image",
            TargetKind::Subtitle => "subtitle",
        }
    }
}

impl fmt::Display for TargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.name())
    }
}

/// How much of the source survives the conversion.
///
/// Ordered worst-last, so a target's fidelity is the `max` over its streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fidelity {
    /// Every kept stream is copied through: the payload bytes are unchanged.
    Copy,
    /// Something is re-encoded, but every re-encode is mathematically lossless.
    Lossless,
    /// At least one stream is re-encoded through a lossy codec.
    Lossy,
}

impl Fidelity {
    pub fn name(self) -> &'static str {
        match self {
            Fidelity::Copy => "copy",
            Fidelity::Lossless => "lossless",
            Fidelity::Lossy => "lossy",
        }
    }
}

impl fmt::Display for Fidelity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.name())
    }
}

/// What one input stream becomes in one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPlan {
    pub input_index: usize,
    pub media_type: MediaType,
    pub from: CodecId,
    pub action: Action,
}

impl StreamPlan {
    /// `"h264 copy"`, `"aac->opus"`, `"h264 dropped (...)"`.
    pub fn summary(&self) -> String {
        match self.action {
            Action::Copy => format!("{} copy", self.from),
            Action::Transcode { to, .. } => format!("{}->{}", self.from, to),
            Action::Drop(reason) => format!("{} dropped ({reason})", self.from),
        }
    }
}

/// The fate of one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Muxed straight through — no decode, no encode, bytes preserved.
    Copy,
    /// Decoded and re-encoded to `to`.
    Transcode { to: CodecId, lossy: bool },
    /// Left out of the output.
    Drop(DropReason),
}

impl Action {
    pub fn is_drop(&self) -> bool {
        matches!(self, Action::Drop(_))
    }
}

/// Why a stream cannot make it into a given container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The container carries nothing of this media type at all.
    UnsupportedMedia,
    /// The container's codecs for this media type all lack an encoder here.
    NoEncoder,
    /// The source codec has no decoder here, so it cannot be re-encoded.
    NoDecoder,
    /// The container holds fewer streams than the input has.
    ContainerFull,
}

impl DropReason {
    /// Stable machine-readable slug (used in the JSON rendering).
    pub fn name(self) -> &'static str {
        match self {
            DropReason::UnsupportedMedia => "unsupported-media",
            DropReason::NoEncoder => "no-encoder",
            DropReason::NoDecoder => "no-decoder",
            DropReason::ContainerFull => "container-full",
        }
    }

    fn explain(self) -> &'static str {
        match self {
            DropReason::UnsupportedMedia => "container carries no stream of this type",
            DropReason::NoEncoder => "no encoder for any codec this container accepts",
            DropReason::NoDecoder => "no decoder for the source codec",
            DropReason::ContainerFull => "container is full",
        }
    }
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.explain())
    }
}

// ---------------------------------------------------------------------------
// Codec facts
// ---------------------------------------------------------------------------

/// Is re-encoding *into* `id` mathematically lossless, as this engine writes it?
///
/// WebP counts as lossless because our encoder emits VP8L (the lossy VP8 encode
/// path is a decode-only gap here); GIF does **not**, because writing one
/// quantizes to a 256-colour palette even though the LZW step itself is exact.
pub fn is_lossless(id: CodecId) -> bool {
    matches!(
        id,
        CodecId::Png
            | CodecId::Flac
            | CodecId::Pcm
            | CodecId::RawVideo
            | CodecId::Webp
            | CodecId::Subrip
            | CodecId::WebVtt
    )
}

/// Codecs whose encoder only produces a single picture, so they cannot supply
/// the video track of a moving-picture container. Our AVIF encoder drives
/// rav1e in `still_picture` mode; video-mode AV1 encode is a known gap.
fn encodes_stills_only(id: CodecId) -> bool {
    matches!(id, CodecId::Avif)
}

/// Text-cue codecs, which convert between each other by relabelling the cue
/// packets rather than through the codec registry.
fn is_text_subtitle(id: CodecId) -> bool {
    matches!(id, CodecId::Subrip | CodecId::WebVtt)
}

/// Preference order when a container accepts several codecs for one media type.
/// The first entry the container accepts *and* we can encode wins.
fn preference(media: MediaType) -> &'static [CodecId] {
    match media {
        MediaType::Video => &[
            CodecId::H264,
            CodecId::Vp9,
            CodecId::Av2,
            CodecId::Avif,
            CodecId::Jpeg,
            CodecId::Png,
            CodecId::Webp,
            CodecId::Gif,
            CodecId::Jxl,
            CodecId::RawVideo,
        ],
        MediaType::Audio => &[
            CodecId::Opus,
            CodecId::Aac,
            CodecId::Flac,
            CodecId::Mp3,
            CodecId::Vorbis,
            CodecId::Pcm,
        ],
        MediaType::Subtitle => &[CodecId::Subrip, CodecId::WebVtt],
        _ => &[],
    }
}

/// Per-container overrides where the globally-preferred codec is legal but not
/// what the file is expected to carry. MP4 accepts Opus, but an `.mp4` should
/// default to AAC.
fn preferred_override(format: &str, media: MediaType) -> Option<CodecId> {
    match (format, media) {
        ("mp4" | "dash", MediaType::Audio) => Some(CodecId::Aac),
        _ => None,
    }
}

/// The order a person expects containers offered in, **per kind**: a `.flac`
/// input should lead with `.mp3`/`.wav`, not with the `.mkv` that also happens
/// to accept its audio. One global list cannot do that — Matroska is the second
/// video container and the sixth audio one. Unlisted formats sort last.
const VIDEO_ORDER: &[&str] = &[
    "mp4",
    "matroska",
    "webm",
    "mpegts",
    "flv",
    "avi",
    "hls",
    "dash",
    "ivf",
    "av2f",
    "yuv4mpegpipe",
];
const AUDIO_ORDER: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "mp4", "matroska", "webm", "mpegts", "flv", "avi", "hls", "dash",
];
const IMAGE_ORDER: &[&str] = &["jpeg", "png", "webp", "avif", "gif", "jpegxl", "av2f"];
const SUBTITLE_ORDER: &[&str] = &["srt", "webvtt", "matroska", "mp4"];

/// Where `name` sits in its kind's running order.
fn rank(kind: TargetKind, name: &str) -> usize {
    let order = match kind {
        TargetKind::Video => VIDEO_ORDER,
        TargetKind::Audio => AUDIO_ORDER,
        TargetKind::Image => IMAGE_ORDER,
        TargetKind::Subtitle => SUBTITLE_ORDER,
    };
    order.iter().position(|n| *n == name).unwrap_or(order.len())
}

/// Media types in the order a container spends its stream budget.
const MEDIA_PRIORITY: [MediaType; 3] = [MediaType::Video, MediaType::Audio, MediaType::Subtitle];

fn media_rank(m: MediaType) -> usize {
    MEDIA_PRIORITY.iter().position(|p| *p == m).unwrap_or(9)
}

// ---------------------------------------------------------------------------
// The planner
// ---------------------------------------------------------------------------

/// Enumerate every container the engine can write for `source`, best first.
///
/// "Best" is: the kind that matches the input first (a video in leads with
/// video containers), then stream-copy before lossless re-encode before lossy,
/// then a fixed popularity order. A container where every stream would be
/// dropped is not offered at all.
pub fn plan(codecs: &CodecRegistry, formats: &FormatRegistry, source: &[SourceStream]) -> Plan {
    let mut targets: Vec<Target> = formats
        .iter()
        .filter(|f| f.can_mux())
        .filter_map(|f| {
            build_target(
                codecs,
                f.name,
                f.long_name,
                f.extensions,
                &f.mux_caps,
                f.muxer_path.is_some(),
                // The extension alone selects the muxer unless it resolves to a
                // different format (asking the registry keeps this correct as
                // formats are added, where a hand-kept list would drift).
                |ext| formats.by_extension(ext).map(|r| r.name) != Some(f.name),
                source,
            )
        })
        .collect();

    let source_kind = source_kind(source);
    targets.sort_by_key(|t| {
        (
            (t.kind != source_kind) as u8,
            t.fidelity,
            rank(t.kind, t.format),
            t.format,
        )
    });

    Plan {
        source: source.to_vec(),
        source_format: None,
        targets,
    }
}

/// What the input mostly *is*, which decides which targets lead the list.
fn source_kind(source: &[SourceStream]) -> TargetKind {
    if source.iter().any(|s| s.media_type == MediaType::Video) {
        TargetKind::Video
    } else if source.iter().any(|s| s.media_type == MediaType::Audio) {
        TargetKind::Audio
    } else {
        TargetKind::Subtitle
    }
}

#[allow(clippy::too_many_arguments)]
fn build_target(
    codecs: &CodecRegistry,
    format: &'static str,
    long_name: &'static str,
    extensions: &'static [&'static str],
    caps: &MuxCaps,
    multi_file: bool,
    // True when the suggested extension does not resolve back to this format,
    // so the command must say `-f <format>` explicitly.
    needs_explicit_f: impl Fn(&str) -> bool,
    source: &[SourceStream],
) -> Option<Target> {
    // 1. Decide each stream's fate on codec grounds alone.
    let mut streams: Vec<StreamPlan> = source
        .iter()
        .map(|s| StreamPlan {
            input_index: s.index,
            media_type: s.media_type,
            from: s.codec_id,
            action: decide(codecs, format, caps, s),
        })
        .collect();

    // 2. Apply the container's stream budget: keep the highest-priority
    //    survivors, drop the overflow.
    if let Some(max) = caps.max_streams {
        let mut order: Vec<usize> = (0..streams.len())
            .filter(|&i| !streams[i].action.is_drop())
            .collect();
        order.sort_by_key(|&i| (media_rank(streams[i].media_type), streams[i].input_index));
        for &i in order.iter().skip(max) {
            streams[i].action = Action::Drop(DropReason::ContainerFull);
        }
    }

    if streams.iter().all(|s| s.action.is_drop()) {
        return None;
    }

    // 3. A target is only as faithful as its worst kept stream.
    let fidelity = streams
        .iter()
        .filter_map(|s| match s.action {
            Action::Copy => Some(Fidelity::Copy),
            Action::Transcode { lossy: false, .. } => Some(Fidelity::Lossless),
            Action::Transcode { lossy: true, .. } => Some(Fidelity::Lossy),
            Action::Drop(_) => None,
        })
        .max()
        .unwrap_or(Fidelity::Copy);

    let kind = if caps.still_image {
        TargetKind::Image
    } else {
        match streams
            .iter()
            .filter(|s| !s.action.is_drop())
            .map(|s| s.media_type)
            .min_by_key(|m| media_rank(*m))
        {
            Some(MediaType::Video) => TargetKind::Video,
            Some(MediaType::Audio) => TargetKind::Audio,
            _ => TargetKind::Subtitle,
        }
    };

    let extension = primary_extension(extensions, &streams, format);
    Some(Target {
        format,
        long_name,
        extension,
        extensions,
        kind,
        notes: notes(format, caps, multi_file, &streams, kind),
        args: args(format, needs_explicit_f(extension), &streams),
        streams,
        fidelity,
    })
}

/// The extension to suggest for this target.
///
/// A container with a codec-specific extension should use it when that is the
/// codec we are writing — Ogg is `.opus` when it carries Opus and `.ogg` when it
/// carries Vorbis. Otherwise the format's first extension wins (`.jpg`, not
/// `.jpeg`).
fn primary_extension(
    extensions: &'static [&'static str],
    streams: &[StreamPlan],
    format: &'static str,
) -> &'static str {
    for s in streams {
        let codec = match s.action {
            Action::Copy => s.from,
            Action::Transcode { to, .. } => to,
            Action::Drop(_) => continue,
        };
        if let Some(ext) = extensions.iter().find(|e| **e == codec.name()) {
            return ext;
        }
    }
    extensions.first().copied().unwrap_or(format)
}

/// Copy, transcode, or drop — for one stream against one container.
fn decide(codecs: &CodecRegistry, format: &str, caps: &MuxCaps, s: &SourceStream) -> Action {
    if !caps.accepts_media(s.media_type) {
        return Action::Drop(DropReason::UnsupportedMedia);
    }
    // A stream copy touches no codec at all, so it needs neither encoder nor
    // decoder — only a muxer that accepts the codec.
    if caps.accepts(s.codec_id) {
        return Action::Copy;
    }
    // Text cues convert by relabelling, not through the codec registry.
    if s.media_type == MediaType::Subtitle && is_text_subtitle(s.codec_id) {
        return match caps
            .codecs_for(MediaType::Subtitle)
            .find(|id| is_text_subtitle(*id))
        {
            Some(to) => Action::Transcode { to, lossy: false },
            None => Action::Drop(DropReason::NoEncoder),
        };
    }
    // Re-encoding means decoding the source first.
    if !codecs.by_id(s.codec_id).is_some_and(|c| c.can_decode()) {
        return Action::Drop(DropReason::NoDecoder);
    }
    match pick_encoder(codecs, format, caps, s.media_type) {
        Some(to) => Action::Transcode {
            to,
            lossy: !is_lossless(to),
        },
        None => Action::Drop(DropReason::NoEncoder),
    }
}

/// The codec to re-encode into for `media` in this container: the override if
/// there is one and we can encode it, else the first preference the container
/// accepts and we can encode.
fn pick_encoder(
    codecs: &CodecRegistry,
    format: &str,
    caps: &MuxCaps,
    media: MediaType,
) -> Option<CodecId> {
    let usable = |id: CodecId| {
        // A still-only encoder cannot supply a moving-picture container.
        let shape_fits = caps.still_image || !encodes_stills_only(id);
        shape_fits && caps.accepts(id) && codecs.by_id(id).is_some_and(|c| c.can_encode())
    };
    preferred_override(format, media)
        .filter(|id| usable(*id))
        .or_else(|| preference(media).iter().copied().find(|id| usable(*id)))
}

/// Everything a caller should be told before writing this target.
fn notes(
    format: &str,
    caps: &MuxCaps,
    multi_file: bool,
    streams: &[StreamPlan],
    kind: TargetKind,
) -> Vec<String> {
    let mut notes = Vec::new();

    if caps.still_image
        && kind == TargetKind::Image
        && streams
            .iter()
            .any(|s| !s.action.is_drop() && s.media_type == MediaType::Video)
    {
        notes.push("holds one picture — only the first frame is written".into());
    }
    if multi_file {
        notes.push("writes a playlist plus segment files, not a single file".into());
    }
    // Generation loss: already-lossy content pushed through another lossy codec.
    for s in streams {
        if let Action::Transcode { to, lossy: true } = s.action {
            if !is_lossless(s.from) {
                notes.push(format!(
                    "{} -> {to} re-encodes already-lossy data (generation loss)",
                    s.from
                ));
            }
        }
    }
    for s in streams {
        if let Action::Drop(reason) = s.action {
            notes.push(format!(
                "stream {} ({} {}) is dropped: {reason}",
                s.input_index, s.media_type, s.from
            ));
        }
    }
    if format == "av2f" {
        notes.push(
            "experimental: the four-character codes are ours, so these files read back \
             here and nowhere else"
                .into(),
        );
    }
    notes
}

/// The `-c:*` arguments that pin this plan, so a caller does not have to trust
/// that the engine's defaults match what we just reported.
fn args(format: &str, needs_f: bool, streams: &[StreamPlan]) -> Vec<String> {
    let mut args = Vec::new();
    let mut seen = [false; 3]; // video, audio, subtitle
    for s in streams {
        let (slot, flag) = match s.media_type {
            MediaType::Video => (0usize, "-c:v"),
            MediaType::Audio => (1, "-c:a"),
            MediaType::Subtitle => (2, "-c:s"),
            _ => continue,
        };
        if seen[slot] {
            continue;
        }
        let codec = match s.action {
            Action::Copy => "copy".to_string(),
            Action::Transcode { to, .. } => to.name().to_string(),
            Action::Drop(_) => continue,
        };
        seen[slot] = true;
        args.push(flag.to_string());
        args.push(codec);
    }
    if needs_f {
        args.push("-f".into());
        args.push(format.to_string());
    }
    args
}

// ---------------------------------------------------------------------------
// Static matrix (no input file needed)
// ---------------------------------------------------------------------------

/// One row of the "what can this build read and write?" matrix, independent of
/// any input. A UI populates its format picker from this.
#[derive(Debug, Clone)]
pub struct FormatSupport {
    pub format: &'static str,
    pub long_name: &'static str,
    pub extensions: &'static [&'static str],
    pub can_read: bool,
    pub can_write: bool,
    /// Codecs the muxer accepts, split by media type.
    pub video: Vec<CodecId>,
    pub audio: Vec<CodecId>,
    pub subtitle: Vec<CodecId>,
    pub still_image: bool,
    pub max_streams: Option<usize>,
}

/// The full container matrix for this build, container-first then alphabetical.
pub fn format_matrix(formats: &FormatRegistry) -> Vec<FormatSupport> {
    let mut rows: Vec<FormatSupport> = formats
        .iter()
        .map(|f| FormatSupport {
            format: f.name,
            long_name: f.long_name,
            extensions: f.extensions,
            can_read: f.can_demux(),
            can_write: f.can_mux(),
            video: f.mux_caps.codecs_for(MediaType::Video).collect(),
            audio: f.mux_caps.codecs_for(MediaType::Audio).collect(),
            subtitle: f.mux_caps.codecs_for(MediaType::Subtitle).collect(),
            still_image: f.mux_caps.still_image,
            max_streams: f.mux_caps.max_streams,
        })
        .collect();
    rows.sort_by_key(|r| (rank(TargetKind::Video, r.format), r.format));
    rows
}

/// Every extension this build can *read*, deduplicated and sorted — the accept
/// list for a file picker.
pub fn readable_extensions(formats: &FormatRegistry) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = formats
        .iter()
        .filter(|f| f.can_demux())
        .flat_map(|f| f.extensions.iter().copied())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}
