//! `rff-format` — the container (muxer/demuxer) abstraction layer (FFmpeg's
//! `libavformat` core, minus the formats themselves).
//!
//! A container is the file wrapper (AVI, MP4, Matroska, ...) that interleaves
//! one or more elementary streams. This crate defines:
//! * [`Stream`] — the description of one elementary stream inside a container,
//! * [`Demuxer`] — reads a container, exposing streams and [`Packet`]s,
//! * [`Muxer`] — writes streams + packets back into a container,
//! * [`FormatRegistry`] — what individual format crates register into.
//!
//! Concrete formats live in their own crates (`rff-format-avi`, ...).

pub mod aac;
pub mod av1;
pub mod avc;
pub mod chapter;
pub mod flac;
mod group;
pub mod hvc;
pub mod nal;
pub mod time;

pub use chapter::Chapter;
pub use group::{SampleGroupRun, SampleGrouping, TrackReference};

use std::collections::HashMap;
use std::io::{Read, Seek, Write};

use rff_core::{
    CleanAperture, CodecId, ColorInfo, ColorRange, ContentLightLevel, Dictionary, Error,
    MasteringDisplay, MediaType, Packet, PixelFormat, Rational, Result, SampleFormat, Timecode,
};

/// Description of one elementary stream within a container.
#[derive(Debug, Clone)]
pub struct Stream {
    /// Position of this stream within its container.
    pub index: usize,
    pub codec_id: CodecId,
    pub media_type: MediaType,
    /// Unit of all timestamps on packets of this stream.
    pub time_base: Rational,
    // --- Video parameters (zero/ignored for non-video) ---
    pub width: u32,
    pub height: u32,
    /// Raw pixel layout, when known (raw video). Self-describing codecs leave this `None`.
    pub pixel_format: Option<PixelFormat>,
    /// Luma/chroma value range. JPEG and PNG sources are always
    /// [`ColorRange::Full`]; leaving it [`Unspecified`](ColorRange::Unspecified)
    /// makes containers label the data limited-range, which silently corrupts it.
    pub color_range: ColorRange,
    // --- Audio parameters (zero/ignored for non-audio) ---
    pub sample_rate: u32,
    pub channels: u16,
    /// Raw sample layout, when known (PCM). Self-describing codecs leave this `None`.
    pub sample_format: Option<SampleFormat>,
    /// Codec-private initialization data (e.g. H.264 SPS/PPS, OpusHead). Empty
    /// for codecs whose bitstream is fully self-describing.
    pub extradata: Vec<u8>,
    /// Total stream duration in [`time_base`](Stream::time_base) units, when
    /// the container declares it (MP4 `mdhd`, Matroska `Duration`). `None` when
    /// unknown (live streams, TS).
    pub duration: Option<i64>,
    /// Total packet count (frames for video), when the container's index
    /// declares it. `None` when unknown — never a guess.
    pub nb_frames: Option<u64>,
    /// The container's own code for this codec, when it has one worth
    /// keeping: the MP4/QuickTime sample-entry fourcc (`apch` vs `apcn` names
    /// the ProRes profile, `twos` vs `sowt` the PCM byte order on disk). A
    /// muxer of the same family writes it back when it is valid for the
    /// codec, so a remux keeps the original entry; `None` lets the muxer pick.
    pub codec_tag: Option<[u8; 4]>,
    // --- Video description (containers label; they do not convert) ---
    /// Colour primaries / transfer / matrix (MP4 `colr`, Matroska `Colour`).
    pub color: ColorInfo,
    /// HDR mastering display (MP4 `mdcv`, Matroska `MasteringMetadata`).
    pub mastering_display: Option<MasteringDisplay>,
    /// HDR content light level (MP4 `clli`, Matroska `MaxCLL`/`MaxFALL`).
    pub content_light: Option<ContentLightLevel>,
    /// Pixel aspect ratio as `(horizontal, vertical)` spacing (MP4 `pasp`).
    /// `None` when not recorded, which means square.
    pub pixel_aspect: Option<(u32, u32)>,
    /// Clean aperture (MP4 `clap`).
    pub clean_aperture: Option<CleanAperture>,
    /// Start timecode (a QuickTime `tmcd` track referenced by this video track).
    pub timecode: Option<Timecode>,
    /// The frames carry an alpha plane as a block addition (Matroska
    /// `AlphaMode`; WebM VP8/VP9 alpha).
    pub alpha_mode: bool,
    // --- Audio timing the container records beside the packets ---
    /// Samples the decoder must drop from the start, in nanoseconds
    /// (Matroska `CodecDelay`; Opus pre-skip). The demuxer has already taken
    /// it off packet timestamps; it is here so a remux can write it back.
    pub codec_delay_ns: u64,
    /// How far before a seek target decoding must start to converge, in
    /// nanoseconds (Matroska `SeekPreRoll`; 80 ms for Opus).
    pub seek_preroll_ns: u64,
    // --- Presentation (#114) ---
    /// How the decoded picture is transformed for display: the 3×3 matrix of
    /// an MP4 `tkhd` (`a b u / c d v / x y w`, with `a b c d x y` in 16.16
    /// and `u v w` in 2.30 fixed point), kept verbatim so a remux writes it
    /// back unchanged. `None` means identity. Matroska carries a pure rotation
    /// of it as `Projection` / `ProjectionPoseRoll`. See [`Stream::rotation`]
    /// and [`Stream::set_rotation`].
    pub display_matrix: Option<[i32; 9]>,
    /// The track language: an ISO 639-2 code (`eng`, `und`) from MP4 `mdhd`
    /// or Matroska `Language`, or a BCP 47 tag from Matroska
    /// `LanguageBCP47`. `None` when the container says nothing.
    pub language: Option<String>,
    /// The track's human-readable name (Matroska `Name`).
    pub name: Option<String>,
    /// Track flags (Matroska `FlagDefault`, `FlagForced`, ...).
    pub disposition: Disposition,
    /// Interlacing, as the QuickTime `fiel` detail / Matroska `FieldOrder`
    /// code (the two share their values): 0 progressive, 1 top field first,
    /// 6 bottom field first, 9 bottom field first stored first, 14 top field
    /// first stored first, 2 interlaced but undetermined. `None` when the
    /// container says nothing.
    pub field_order: Option<u8>,
    /// Matroska `StereoMode` (0 mono, 1 side by side left first, ...), when
    /// the container records it.
    pub stereo_mode: Option<u8>,
    /// The container's own codec identifier when it is a string: the
    /// Matroska `CodecID` (`V_MPEG4/ISO/AVC`, `A_DTS`, `V_MS/VFW/FOURCC`).
    /// Set for every track a demuxer reads one for, so a caller can tell
    /// apart codecs rff has no [`CodecId`] for ([`CodecId::None`]).
    pub codec_string: Option<String>,
}

/// A stream's track flags, as Matroska records them (`FlagDefault`,
/// `FlagEnabled`, `FlagForced`, `FlagHearingImpaired`, `FlagVisualImpaired`,
/// `FlagOriginal`, `FlagCommentary`). `default` and `enabled` are `None` when
/// the container leaves them at its default (in Matroska both default to
/// set), so a fresh stream changes nothing in the output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Disposition {
    pub default: Option<bool>,
    pub enabled: Option<bool>,
    pub forced: bool,
    pub hearing_impaired: bool,
    pub visual_impaired: bool,
    pub original: bool,
    pub commentary: bool,
}

/// The display matrix that rotates a `width`×`height` picture clockwise by
/// `degrees` (any angle; quarter turns are exact). Quarter turns carry the
/// translation QuickTime writers put in `tkhd` so the picture lands at the
/// origin (`0 1 0 / -1 0 0 / h 0 1` for 90°, as an iPhone writes it).
pub fn rotation_matrix(degrees: f64, width: u32, height: u32) -> [i32; 9] {
    const ONE: i32 = 1 << 16;
    const W: i32 = 1 << 30;
    let fixed = |v: u32| i32::try_from(i64::from(v) << 16).unwrap_or(i32::MAX);
    let (w, h) = (fixed(width), fixed(height));
    let turn = degrees.rem_euclid(360.0);
    match turn {
        0.0 => [ONE, 0, 0, 0, ONE, 0, 0, 0, W],
        90.0 => [0, ONE, 0, -ONE, 0, 0, h, 0, W],
        180.0 => [-ONE, 0, 0, 0, -ONE, 0, w, h, W],
        270.0 => [0, -ONE, 0, ONE, 0, 0, 0, w, W],
        t => {
            let (s, c) = t.to_radians().sin_cos();
            let q = |v: f64| (v * f64::from(ONE)).round() as i32;
            [q(c), q(s), 0, q(-s), q(c), 0, 0, 0, W]
        }
    }
}

/// The clockwise rotation (degrees in `[0, 360)`) a display matrix applies,
/// or `None` when it is not a pure rotation (a flip, a scale, a shear).
pub fn matrix_rotation(m: &[i32; 9]) -> Option<f64> {
    let f = |v: i32| f64::from(v) / 65536.0;
    let (a, b, c, d) = (f(m[0]), f(m[1]), f(m[3]), f(m[4]));
    let tol = 1e-3;
    if (a - d).abs() > tol || (b + c).abs() > tol || (a * a + b * b - 1.0).abs() > 2.0 * tol {
        return None;
    }
    let deg = b.atan2(a).to_degrees().rem_euclid(360.0);
    // Snap what is within rounding of a whole degree (16.16 cos/sin).
    let snapped = deg.round();
    let deg = if (deg - snapped).abs() < 1e-2 {
        snapped.rem_euclid(360.0)
    } else {
        deg
    };
    Some(deg)
}

impl Stream {
    /// The clockwise rotation to apply for display, in degrees: 0 without a
    /// display matrix, `None` when the matrix is not a pure rotation.
    pub fn rotation(&self) -> Option<f64> {
        match &self.display_matrix {
            None => Some(0.0),
            Some(m) => matrix_rotation(m),
        }
    }

    /// Ask for the picture to be shown rotated `degrees` clockwise (set
    /// [`width`](Stream::width)/[`height`](Stream::height) first: quarter
    /// turns record a translation by them). 0 clears the matrix.
    pub fn set_rotation(&mut self, degrees: f64) {
        self.display_matrix = if degrees.rem_euclid(360.0) == 0.0 {
            None
        } else {
            Some(rotation_matrix(degrees, self.width, self.height))
        };
    }
}

impl Stream {
    /// Create a stream with sane defaults for the given codec.
    pub fn new(index: usize, codec_id: CodecId) -> Stream {
        Stream {
            index,
            codec_id,
            media_type: codec_id.media_type(),
            time_base: Rational::new(1, 1000),
            width: 0,
            height: 0,
            pixel_format: None,
            color_range: ColorRange::Unspecified,
            sample_rate: 0,
            channels: 0,
            sample_format: None,
            extradata: Vec::new(),
            duration: None,
            nb_frames: None,
            codec_tag: None,
            color: ColorInfo::UNSPECIFIED,
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            clean_aperture: None,
            timecode: None,
            alpha_mode: false,
            codec_delay_ns: 0,
            seek_preroll_ns: 0,
            display_matrix: None,
            language: None,
            name: None,
            disposition: Disposition::default(),
            field_order: None,
            stereo_mode: None,
            codec_string: None,
        }
    }
}

/// A byte source a demuxer reads from. (`Box<dyn Read + Send>` keeps the trait
/// object-safe and lets inputs be files, network streams, or in-memory buffers.)
pub type Input = Box<dyn Read + Send>;

/// A byte source that can also seek — what a local file or an in-memory buffer
/// offers and a pipe or socket does not. Blanket-implemented for every
/// `Read + Seek` type.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// A seekable [`Input`]. Formats whose index can sit anywhere in the file (MP4's
/// `moov`) use it to read only that index and fetch each payload on demand,
/// instead of buffering the whole file. It upcasts to [`Input`] for formats
/// that only stream.
pub type SeekInput = Box<dyn ReadSeek + Send>;
/// A byte sink a muxer writes to.
pub type Output = Box<dyn Write + Send>;

/// A byte sink that can also seek -- a local file, an in-memory buffer; not a
/// pipe or socket. Blanket-implemented for every `Write + Seek` type.
pub trait WriteSeek: Write + Seek {}
impl<T: Write + Seek> WriteSeek for T {}

/// A seekable [`Output`]. Containers whose header records sizes or an index
/// that is only known at the end (MP4 `mdat`/`moov`, Matroska Segment/Cues,
/// AVI `movi`/`idx1`, WAV `RIFF`/`data`) use it to stream the payload as it
/// arrives and patch the header afterwards, instead of holding the whole
/// output in memory until the trailer (#28). It upcasts to [`Output`] for
/// muxers that only stream.
pub type SeekOutput = Box<dyn WriteSeek + Send>;

/// Reads a container format: parses the header into [`Stream`]s, then yields
/// [`Packet`]s until the input is exhausted.
pub trait Demuxer: Send {
    /// Parse the container header and return its streams.
    fn read_header(&mut self) -> Result<Vec<Stream>>;

    /// Read the next packet. Returns [`Error::Eof`] at end of input.
    fn read_packet(&mut self) -> Result<Packet>;

    /// Name the streams the caller will drop: `discard[i]` is stream `i`.
    /// Called after [`read_header`](Demuxer::read_header); a demuxer may then
    /// skip those streams' packets without reading or converting them. It is
    /// a hint: a demuxer that ignores it (the default) still returns them and
    /// the caller drops them, so either way the kept packets are identical.
    fn set_discard(&mut self, _discard: &[bool]) {}

    /// Random access to the container's index, for an editor that jumps to a
    /// frame instead of reading the file in order: per-stream sample tables,
    /// reading a sample by number, and the sync sample at or before a time.
    /// Available after [`read_header`](Demuxer::read_header) from demuxers
    /// whose container has an index (MP4 sample tables, Matroska clusters and
    /// Cues); `None` from the rest (the default). Reading through the index
    /// does not move [`read_packet`](Demuxer::read_packet)'s position.
    fn sample_index(&mut self) -> Option<&mut dyn SampleIndex> {
        None
    }

    /// Hand out samples as the container stores them instead of in the form
    /// rff's decoders take: H.264 / HEVC length-prefixed (AVCC / HVCC)
    /// rather than Annex-B with the parameter sets put in front of each
    /// keyframe, PCM in the stored byte order, ASS events in Matroska as
    /// stored. Storage transforms of the container itself (Matroska
    /// ContentEncodings and header stripping, the ProRes frame header MKV
    /// drops) are still undone, so a sample is always a whole codec frame.
    /// Call it after
    /// [`read_header`](Demuxer::read_header); it applies to
    /// [`read_packet`](Demuxer::read_packet) and to [`SampleIndex::read_sample`].
    /// Returns whether the demuxer honours it (the default does not: its
    /// samples are already stored as they are handed out).
    fn set_raw_samples(&mut self, _raw: bool) -> bool {
        false
    }

    /// The file's global metadata, in FFmpeg's key names (`title`,
    /// `artist`, `comment`, `encoder`, `creation_time`, ...): MP4 `udta`
    /// (`meta/ilst` and QuickTime `©xxx` atoms), Matroska `Info` and global
    /// `Tags`. Available after [`read_header`](Demuxer::read_header); `None`
    /// from demuxers that read none (the default).
    fn metadata(&self) -> Option<&Dictionary> {
        None
    }

    /// Stream `index`'s own metadata, in FFmpeg-style key names where FFmpeg
    /// has one and the format's own otherwise (the JPEG MPF demuxer's
    /// `mpf_type`). Available after [`read_header`](Demuxer::read_header);
    /// `None` when the stream has none (the default).
    fn stream_metadata(&self, _index: usize) -> Option<&Dictionary> {
        None
    }

    /// The file's chapters, in stored order: the QuickTime chapter text track
    /// or Nero `chpl` of MP4/MOV, the default edition of Matroska
    /// `Chapters`. Available after [`read_header`](Demuxer::read_header);
    /// empty when the file has none, and from demuxers that read none (the
    /// default).
    fn chapters(&self) -> &[Chapter] {
        &[]
    }

    /// Also hand out the container's timecode tracks (MP4/MOV `tmcd`) as
    /// data streams of their own: codec [`CodecId::None`], `codec_tag`
    /// `tmcd`, the sample description as `extradata`, the stored timecode
    /// samples as packets, and the streams they time naming them in
    /// [`track_references`](Demuxer::track_references). They come **after**
    /// every other stream, so no other stream's index changes, and
    /// [`Stream::timecode`] stays as it is without this. Call it before
    /// [`read_header`](Demuxer::read_header). Off unless asked for; returns
    /// whether the demuxer can (the default cannot).
    fn set_timecode_streams(&mut self, _on: bool) -> bool {
        false
    }

    /// What stream `index` refers to (MP4 `tref`), as far as the targets are
    /// streams this demuxer gives. Available after
    /// [`read_header`](Demuxer::read_header); empty by default.
    fn track_references(&self, _index: usize) -> &[TrackReference] {
        &[]
    }
}

/// One sample (packet) of a stream as the container's index records it.
///
/// The timestamps are the ones [`Demuxer::read_packet`] gives the sample, in
/// the stream's time base, with the edit list applied; the edit list itself is
/// [`SampleIndex::edit_list`]. `offset` and `size` locate the stored bytes, so
/// a caller can also read them itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleInfo {
    /// File offset of the stored sample.
    pub offset: u64,
    /// Stored size in bytes.
    pub size: u32,
    /// Decode timestamp.
    pub dts: i64,
    /// Presentation timestamp.
    pub pts: i64,
    /// Duration (0 when the container does not say).
    pub duration: i64,
    /// A sync sample (keyframe): decoding can start here.
    pub sync: bool,
}

/// One entry of a stream's edit list (MP4 `elst`): `segment_duration` of
/// presentation, in the movie time scale, showing the media from
/// `media_time` (stream time base); `media_time` -1 is an empty edit (a
/// delay). `media_rate` is 16.16 fixed point (0x10000 = normal speed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditEntry {
    pub segment_duration: i64,
    pub media_time: i64,
    pub media_rate: i32,
}

/// Random access over a demuxer's index; see [`Demuxer::sample_index`].
pub trait SampleIndex {
    /// Samples in stream `stream` (0 for an unknown stream).
    fn sample_count(&self, stream: usize) -> u64;

    /// The index entry of sample `index` of `stream`, in decode order.
    fn sample_info(&self, stream: usize, index: u64) -> Option<SampleInfo>;

    /// Read sample `index` of `stream` as a packet, in the same form as
    /// [`Demuxer::read_packet`] would give it (see
    /// [`Demuxer::set_raw_samples`]).
    fn read_sample(&mut self, stream: usize, index: u64) -> Result<Packet>;

    /// The last sync sample of `stream` whose presentation time is at or
    /// before `pts`: where decoding starts to show the frame at `pts`.
    /// `None` if no sync sample is that early.
    fn sync_at_or_before(&self, stream: usize, pts: i64) -> Option<u64>;

    /// The stream's edit list (empty when it has none, or the container has
    /// no such thing).
    fn edit_list(&self, _stream: usize) -> &[EditEntry] {
        &[]
    }

    /// The movie time scale [`EditEntry::segment_duration`] counts in (MP4
    /// `mvhd`), when the container has one.
    fn movie_timescale(&self) -> Option<u32> {
        None
    }

    /// Samples in the stream's stored sample table: what
    /// [`raw_sample_info`](SampleIndex::raw_sample_info) numbers. Without an
    /// edit list this is [`sample_count`](SampleIndex::sample_count); an MP4
    /// edit list can replay samples or leave some out of the edited index.
    fn raw_sample_count(&self, stream: usize) -> u64 {
        self.sample_count(stream)
    }

    /// Sample `index` of the stored sample table, in decode order, with the
    /// edit list **not** applied: for MP4 the `stbl` row verbatim, `dts` the
    /// sum of the `stts` deltas before it (0 for the first sample), `pts` =
    /// `dts` + its `ctts` offset, `duration` its `stts` delta, all in the
    /// media time scale (the stream's time base). An editor applies
    /// [`edit_list`](SampleIndex::edit_list) itself on this — for example
    /// AAC encoder priming (the first edit's `media_time`) and padding (where
    /// its `segment_duration` ends) — and a muxer given these samples and the
    /// same edit list ([`Muxer::set_edit_list`]) writes the same table back.
    /// Defaults to [`sample_info`](SampleIndex::sample_info) for containers
    /// without edit lists.
    fn raw_sample_info(&self, stream: usize, index: u64) -> Option<SampleInfo> {
        self.sample_info(stream, index)
    }

    /// Read stored sample `index` (numbered as
    /// [`raw_sample_info`](SampleIndex::raw_sample_info) numbers them) as a
    /// packet timed by that entry, shaped as
    /// [`read_sample`](SampleIndex::read_sample) shapes it.
    fn read_raw_sample(&mut self, stream: usize, index: u64) -> Result<Packet> {
        self.read_sample(stream, index)
    }

    /// The stream's sample groups (MP4 `sbgp`/`sgpd`, in the movie and in
    /// fragments), numbered as
    /// [`raw_sample_info`](SampleIndex::raw_sample_info) numbers samples.
    /// Empty when it has none, or the container has no such thing.
    fn sample_groups(&self, _stream: usize) -> &[SampleGrouping] {
        &[]
    }

    /// A shareable, `&self` view of this index for reading samples from
    /// several places at once (an editor's media pool): cheap to clone (an
    /// `Arc`), `Send + Sync`, and independent of this demuxer's
    /// [`read_packet`](Demuxer::read_packet) position and of every other
    /// source. Each call gives a source with its own read position; they
    /// share the index tables and the input (reads take turns on it). Packets
    /// are shaped as [`read_sample`](SampleIndex::read_sample) shapes them
    /// at the time of the call (see [`Demuxer::set_raw_samples`]). `None`
    /// from demuxers that cannot share their input (the default).
    fn sample_source(&mut self) -> Option<std::sync::Arc<dyn SampleSource>> {
        None
    }
}

/// `&self` random access to a demuxed file's samples: see
/// [`SampleIndex::sample_source`]. The methods mirror [`SampleIndex`]'s.
pub trait SampleSource: Send + Sync {
    fn sample_count(&self, stream: usize) -> u64;
    fn sample_info(&self, stream: usize, index: u64) -> Option<SampleInfo>;
    fn read_sample(&self, stream: usize, index: u64) -> Result<Packet>;
    fn sync_at_or_before(&self, stream: usize, pts: i64) -> Option<u64>;
    fn edit_list(&self, _stream: usize) -> &[EditEntry] {
        &[]
    }
    fn movie_timescale(&self) -> Option<u32> {
        None
    }
    fn raw_sample_count(&self, stream: usize) -> u64 {
        self.sample_count(stream)
    }
    fn raw_sample_info(&self, stream: usize, index: u64) -> Option<SampleInfo> {
        self.sample_info(stream, index)
    }
    fn read_raw_sample(&self, stream: usize, index: u64) -> Result<Packet> {
        self.read_sample(stream, index)
    }
    fn sample_groups(&self, _stream: usize) -> &[SampleGrouping] {
        &[]
    }
}

/// One input shared by several readers, each with its own position: a
/// `Read + Seek` handle over a [`SeekInput`] behind a mutex. Every read
/// seeks the shared input to this handle's position first (skipped when it
/// is already there), so handles never disturb each other. Cloning gives a
/// handle at the same position. What demuxers use to hand out
/// [`SampleSource`]s without reopening the file.
#[derive(Clone)]
pub struct SharedInput {
    /// The input and where it currently stands (`None`: unknown).
    inner: std::sync::Arc<std::sync::Mutex<(SeekInput, Option<u64>)>>,
    pos: u64,
}

impl SharedInput {
    /// Share `input`; the handle starts at the input's current position.
    pub fn new(mut input: SeekInput) -> SharedInput {
        let pos = input.stream_position().unwrap_or(0);
        SharedInput {
            inner: std::sync::Arc::new(std::sync::Mutex::new((input, Some(pos)))),
            pos,
        }
    }
}

type SharedState = std::sync::Mutex<(SeekInput, Option<u64>)>;

fn lock_shared(
    inner: &SharedState,
) -> std::io::Result<std::sync::MutexGuard<'_, (SeekInput, Option<u64>)>> {
    inner
        .lock()
        .map_err(|_| std::io::Error::other("shared input poisoned"))
}

impl Read for SharedInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut g = lock_shared(&self.inner)?;
        let (input, at) = &mut *g;
        if *at != Some(self.pos) {
            *at = None;
            input.seek(std::io::SeekFrom::Start(self.pos))?;
        }
        let n = input.read(buf)?;
        self.pos += n as u64;
        *at = Some(self.pos);
        Ok(n)
    }
}

impl Seek for SharedInput {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start");
        self.pos = match pos {
            std::io::SeekFrom::Start(n) => n,
            std::io::SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(invalid)?,
            std::io::SeekFrom::End(d) => {
                let mut g = lock_shared(&self.inner)?;
                let (input, at) = &mut *g;
                let end = input.seek(std::io::SeekFrom::End(0))?;
                *at = Some(end);
                end.checked_add_signed(d).ok_or_else(invalid)?
            }
        };
        Ok(self.pos)
    }
}

/// Writes a container format: declare streams, write packets, finalize.
pub trait Muxer: Send {
    /// Hand the muxer output metadata (`-metadata title=...`). Called before
    /// [`write_header`](Muxer::write_header); formats without a metadata slot
    /// simply ignore it (the default).
    fn set_metadata(&mut self, _metadata: &Dictionary) {}

    /// Hand the muxer output stream `index`'s metadata (what the demuxer's
    /// [`Demuxer::stream_metadata`] gave for the stream it came from). Called
    /// before [`write_header`](Muxer::write_header); formats without
    /// per-stream metadata ignore it (the default).
    fn set_stream_metadata(&mut self, _index: usize, _metadata: &Dictionary) {}

    /// A muxer option, FFmpeg-style (`movflags`, `+faststart`). Called before
    /// [`write_header`](Muxer::write_header). Returns whether the muxer knows
    /// the option; the default knows none.
    fn set_option(&mut self, _key: &str, _value: &str) -> bool {
        false
    }

    /// Write `edits` as stream `stream`'s edit list instead of the one the
    /// muxer would derive (encoder delay, trims, a start delay). Segment
    /// durations are in the muxer's movie time scale (1/1000 s for MP4 unless
    /// `set_option("movie_timescale", ..)` picks another), media
    /// times in the stream's time base, -1 for an empty edit. Called before
    /// [`write_trailer`](Muxer::write_trailer). Returns whether the container
    /// has edit lists; the default has none.
    fn set_edit_list(&mut self, _stream: usize, _edits: &[EditEntry]) -> bool {
        false
    }

    /// Write `chapters` into the container. Called before
    /// [`write_header`](Muxer::write_header). Returns whether the container
    /// carries chapters; the default does not.
    fn set_chapters(&mut self, _chapters: &[Chapter]) -> bool {
        false
    }

    /// Write `references` as what output stream `stream` refers to (MP4
    /// `tref`), replacing any set before. A `tmcd` reference to a timecode
    /// stream is written instead of the timecode track the muxer would make
    /// from [`Stream::timecode`]. Called before
    /// [`write_trailer`](Muxer::write_trailer). Returns whether the container
    /// has track references; the default does not.
    fn set_track_references(&mut self, _stream: usize, _references: &[TrackReference]) -> bool {
        false
    }

    /// Write the container header for the given set of streams.
    fn write_header(&mut self, streams: &[Stream]) -> Result<()>;

    /// Write one packet (its `stream_index` selects the target stream).
    fn write_packet(&mut self, packet: &Packet) -> Result<()>;

    /// Finalize the container (indexes, trailers, fixups).
    fn write_trailer(&mut self) -> Result<()>;
}

/// Factory opening a demuxer over an input byte source.
pub type DemuxerFactory = fn(Input) -> Box<dyn Demuxer>;
/// Factory opening a demuxer over a [`SeekInput`] — registered alongside the
/// format's [`DemuxerFactory`] by formats that read less when they can seek.
pub type SeekableDemuxerFactory = fn(SeekInput) -> Box<dyn Demuxer>;
/// Factory opening a muxer over an output byte sink.
pub type MuxerFactory = fn(Output) -> Box<dyn Muxer>;
/// Factory opening a muxer over a [`SeekOutput`] -- registered alongside the
/// format's [`MuxerFactory`] by formats that can stream when they can seek.
pub type SeekableMuxerFactory = fn(SeekOutput) -> Box<dyn Muxer>;
/// Factory opening a muxer that owns its own file set (HLS/DASH write a
/// playlist/manifest plus many segment files, so a single byte sink cannot
/// model them). Receives the output path and the format options
/// (`-hls_time 6` → `{"hls_time": "6"}`).
pub type PathMuxerFactory = fn(&std::path::Path, &Dictionary) -> Result<Box<dyn Muxer>>;
/// Content sniffer: scores how strongly a byte prefix looks like this format,
/// `0` (no match) to `100` (certain) — mirrors FFmpeg's `AVInputFormat::read_probe`.
pub type ProbeFn = fn(&[u8]) -> i32;

/// What a container's **muxer** will accept: the machine-readable half of the
/// rules each muxer already enforces when it writes a header.
///
/// Muxers validate at run time, and a few historically wrote a zero codec tag
/// rather than refusing an unmappable codec. Declaring the same rules here lets
/// a caller answer "what can this file become?" *before* opening an encoder —
/// which is what `rff-targets` is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuxCaps {
    /// Every codec this muxer can write. Empty for demux-only formats.
    pub codecs: &'static [CodecId],
    /// Most streams the container holds; `None` when there is no fixed limit.
    pub max_streams: Option<usize>,
    /// True when the container holds exactly ONE picture, so extra frames are
    /// refused or dropped (png, jpeg, gif, webp, avif, jxl, av2f).
    pub still_image: bool,
}

impl MuxCaps {
    /// A format that cannot be written at all (demux-only).
    pub const NONE: MuxCaps = MuxCaps {
        codecs: &[],
        max_streams: Some(0),
        still_image: false,
    };

    /// A container interleaving any number of streams (mp4, matroska, mpegts).
    pub const fn container(codecs: &'static [CodecId]) -> MuxCaps {
        MuxCaps {
            codecs,
            max_streams: None,
            still_image: false,
        }
    }

    /// A file holding exactly one elementary stream (wav, mp3, ivf, ogg).
    pub const fn single(codecs: &'static [CodecId]) -> MuxCaps {
        MuxCaps {
            codecs,
            max_streams: Some(1),
            still_image: false,
        }
    }

    /// Narrow a [`single`](MuxCaps::single) file to one picture (png, jpeg).
    pub const fn image(self) -> MuxCaps {
        MuxCaps {
            codecs: self.codecs,
            max_streams: self.max_streams,
            still_image: true,
        }
    }

    /// Can this muxer write `id`?
    pub fn accepts(&self, id: CodecId) -> bool {
        self.codecs.contains(&id)
    }

    /// Can this muxer carry any stream of `media` at all?
    pub fn accepts_media(&self, media: MediaType) -> bool {
        self.codecs.iter().any(|c| c.media_type() == media)
    }

    /// The codecs this muxer accepts for one media category.
    pub fn codecs_for(&self, media: MediaType) -> impl Iterator<Item = CodecId> + '_ {
        self.codecs
            .iter()
            .copied()
            .filter(move |c| c.media_type() == media)
    }
}

/// Static description of a container format and its read/write support.
pub struct Format {
    /// Short name (`avi`, `mp4`, ...), as used by `ffmpeg -f <name>`.
    pub name: &'static str,
    pub long_name: &'static str,
    /// File extensions that imply this format (without the dot).
    pub extensions: &'static [&'static str],
    /// Present if this format can be demuxed (read).
    pub demuxer: Option<DemuxerFactory>,
    /// Present if this format can be muxed (written).
    pub muxer: Option<MuxerFactory>,
    /// Present for formats that mux into a *path* (playlist + segment files)
    /// rather than one byte sink — HLS, DASH. Mutually exclusive with `muxer`.
    pub muxer_path: Option<PathMuxerFactory>,
    /// Content sniffer for magic-byte detection (independent of the filename).
    pub probe: Option<ProbeFn>,
    /// What this format's muxer accepts. [`MuxCaps::NONE`] for demux-only
    /// formats. Keep it in step with the muxer — `tests/mux_caps.rs` gates it.
    pub mux_caps: MuxCaps,
}

impl Format {
    pub fn can_demux(&self) -> bool {
        self.demuxer.is_some()
    }
    pub fn can_mux(&self) -> bool {
        self.muxer.is_some() || self.muxer_path.is_some()
    }
}

/// Holds every container format known to a running engine.
#[derive(Default)]
pub struct FormatRegistry {
    by_name: HashMap<&'static str, Format>,
    /// Seek-aware demuxer factories, keyed by format name (see
    /// [`register_seekable_demuxer`](Self::register_seekable_demuxer)).
    seekable: HashMap<&'static str, SeekableDemuxerFactory>,
    /// Seek-aware muxer factories, keyed by format name (see
    /// [`register_seekable_muxer`](Self::register_seekable_muxer)).
    seekable_muxers: HashMap<&'static str, SeekableMuxerFactory>,
}

impl FormatRegistry {
    pub fn new() -> FormatRegistry {
        FormatRegistry::default()
    }

    pub fn register(&mut self, format: Format) {
        self.by_name.insert(format.name, format);
    }

    /// Give the format named `name` a demuxer that takes a [`SeekInput`]. Used
    /// by [`open_demuxer_seekable`](Self::open_demuxer_seekable) when the caller
    /// has a seekable source; the plain [`Format::demuxer`] still serves pipes
    /// and sockets.
    pub fn register_seekable_demuxer(
        &mut self,
        name: &'static str,
        factory: SeekableDemuxerFactory,
    ) {
        self.seekable.insert(name, factory);
    }

    pub fn by_name(&self, name: &str) -> Option<&Format> {
        self.by_name.get(name)
    }

    /// Find a format whose extension list contains `ext` (case-insensitive,
    /// no leading dot). The cheap "guess by filename" path; [`probe`](Self::probe)
    /// inspects content instead.
    pub fn by_extension(&self, ext: &str) -> Option<&Format> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        self.by_name
            .values()
            .find(|f| f.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)))
    }

    /// Identify a format by sniffing a byte prefix: the registered format whose
    /// [`probe`](Format::probe) returns the highest positive score wins. Returns
    /// `None` if nothing recognizes the data.
    pub fn probe(&self, data: &[u8]) -> Option<&Format> {
        self.by_name
            .values()
            .filter_map(|f| f.probe.map(|p| (p(data), f)))
            .filter(|(score, _)| *score > 0)
            .max_by_key(|(score, _)| *score)
            .map(|(_, f)| f)
    }

    /// Open a demuxer for the format named `name`.
    pub fn open_demuxer(&self, name: &str, input: Input) -> Result<Box<dyn Demuxer>> {
        self.by_name
            .get(name)
            .and_then(|f| f.demuxer)
            .map(|factory| factory(input))
            .ok_or_else(|| Error::DemuxerNotFound(name.to_string()))
    }

    /// Open a demuxer over a seekable input: the format's seek-aware demuxer if
    /// it registered one, otherwise its plain streaming demuxer (a seekable
    /// source is still a byte stream). Callers that hold a file should prefer
    /// this — it is what lets MP4 read only `moov` instead of the whole file.
    pub fn open_demuxer_seekable(&self, name: &str, input: SeekInput) -> Result<Box<dyn Demuxer>> {
        if !self.by_name.get(name).is_some_and(Format::can_demux) {
            return Err(Error::DemuxerNotFound(name.to_string()));
        }
        match self.seekable.get(name) {
            Some(factory) => Ok(factory(input)),
            None => self.open_demuxer(name, input),
        }
    }

    /// Give the format named `name` a muxer that takes a [`SeekOutput`]. Used
    /// by [`open_muxer_seekable`](Self::open_muxer_seekable) when the caller
    /// writes to something seekable; the plain [`Format::muxer`] still serves
    /// pipes and sockets.
    pub fn register_seekable_muxer(&mut self, name: &'static str, factory: SeekableMuxerFactory) {
        self.seekable_muxers.insert(name, factory);
    }

    /// Open a muxer over a seekable output: the format's seek-aware muxer if it
    /// registered one, otherwise its plain muxer (a seekable sink is still a
    /// byte sink). Callers writing a file should prefer this -- it is what lets
    /// MP4/Matroska/AVI/WAV stream instead of buffering the whole output.
    pub fn open_muxer_seekable(&self, name: &str, output: SeekOutput) -> Result<Box<dyn Muxer>> {
        if self.by_name.get(name).is_none_or(|f| f.muxer.is_none()) {
            return Err(Error::MuxerNotFound(name.to_string()));
        }
        match self.seekable_muxers.get(name) {
            Some(factory) => Ok(factory(output)),
            None => self.open_muxer(name, output),
        }
    }

    /// Open a muxer for the format named `name`.
    pub fn open_muxer(&self, name: &str, output: Output) -> Result<Box<dyn Muxer>> {
        self.by_name
            .get(name)
            .and_then(|f| f.muxer)
            .map(|factory| factory(output))
            .ok_or_else(|| Error::MuxerNotFound(name.to_string()))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Format> {
        self.by_name.values()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Reports which factory built it via the stream count.
    struct Tagged(usize);
    impl Demuxer for Tagged {
        fn read_header(&mut self) -> Result<Vec<Stream>> {
            Ok((0..self.0).map(|i| Stream::new(i, CodecId::None)).collect())
        }
        fn read_packet(&mut self) -> Result<rff_core::Packet> {
            Err(Error::Eof)
        }
    }

    fn format(name: &'static str) -> Format {
        Format {
            name,
            long_name: name,
            extensions: &[],
            demuxer: Some(|_| Box::new(Tagged(1))),
            muxer: None,
            muxer_path: None,
            probe: None,
            mux_caps: MuxCaps::NONE,
        }
    }

    fn streams(mut d: Box<dyn Demuxer>) -> usize {
        d.read_header().unwrap().len()
    }

    #[test]
    fn shared_input_handles_keep_their_own_positions() {
        let data: Vec<u8> = (0..=255).collect();
        let mut a = SharedInput::new(Box::new(Cursor::new(data)));
        let mut b = a.clone();
        b.seek(std::io::SeekFrom::Start(200)).unwrap();
        let mut x = [0u8; 3];
        a.read_exact(&mut x).unwrap();
        assert_eq!(x, [0, 1, 2]);
        b.read_exact(&mut x).unwrap();
        assert_eq!(x, [200, 201, 202]);
        a.read_exact(&mut x).unwrap();
        assert_eq!(x, [3, 4, 5]);
        assert_eq!(b.seek(std::io::SeekFrom::End(-1)).unwrap(), 255);
        let t = std::thread::spawn(move || {
            let mut y = [0u8; 1];
            b.read_exact(&mut y).unwrap();
            y[0]
        });
        assert_eq!(t.join().unwrap(), 255);
    }

    #[test]
    fn rotation_matrices_round_trip() {
        for deg in [0.0, 90.0, 180.0, 270.0, 30.0, 315.0] {
            let m = rotation_matrix(deg, 1920, 1080);
            assert_eq!(matrix_rotation(&m), Some(deg), "{deg}");
        }
        // An iPhone portrait clip: rotate 90 degrees clockwise, translated by the height.
        let m = rotation_matrix(-270.0, 1920, 1080);
        assert_eq!(m, [0, 0x10000, 0, -0x10000, 0, 0, 1080 << 16, 0, 1 << 30]);
        // A horizontal flip is not a rotation.
        let flip = [-0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 1 << 30];
        assert_eq!(matrix_rotation(&flip), None);
        let mut s = Stream::new(0, CodecId::H264);
        (s.width, s.height) = (640, 480);
        assert_eq!(s.rotation(), Some(0.0));
        s.set_rotation(90.0);
        assert_eq!(s.rotation(), Some(90.0));
        s.set_rotation(360.0);
        assert_eq!(s.display_matrix, None);
    }

    #[test]
    fn seekable_open_prefers_the_seekable_factory_and_falls_back() {
        let mut r = FormatRegistry::new();
        r.register(format("plain"));
        r.register(format("seeky"));
        r.register_seekable_demuxer("seeky", |_| Box::new(Tagged(2)));
        let input = || -> SeekInput { Box::new(Cursor::new(vec![0u8; 4])) };
        assert_eq!(
            streams(r.open_demuxer_seekable("seeky", input()).unwrap()),
            2
        );
        // No seekable factory: the streaming one serves the seekable input.
        assert_eq!(
            streams(r.open_demuxer_seekable("plain", input()).unwrap()),
            1
        );
        // A plain stream open never takes the seekable path.
        assert_eq!(
            streams(
                r.open_demuxer("seeky", Box::new(Cursor::new(vec![])))
                    .unwrap()
            ),
            1
        );
        assert!(matches!(
            r.open_demuxer_seekable("missing", input()),
            Err(Error::DemuxerNotFound(_))
        ));
    }

    /// Reports which factory built it through the error `write_trailer` returns.
    struct TaggedMux(&'static str);
    impl Muxer for TaggedMux {
        fn write_header(&mut self, _: &[Stream]) -> Result<()> {
            Ok(())
        }
        fn write_packet(&mut self, _: &rff_core::Packet) -> Result<()> {
            Ok(())
        }
        fn write_trailer(&mut self) -> Result<()> {
            Err(Error::invalid(self.0))
        }
    }

    fn tag(mut m: Box<dyn Muxer>) -> String {
        m.write_trailer().unwrap_err().to_string()
    }

    #[test]
    fn seekable_muxer_open_prefers_the_seekable_factory_and_falls_back() {
        let mut r = FormatRegistry::new();
        let with_muxer = |name| Format {
            muxer: Some(|_| Box::new(TaggedMux("plain"))),
            ..format(name)
        };
        r.register(with_muxer("plain"));
        r.register(with_muxer("seeky"));
        r.register(format("demux_only"));
        r.register_seekable_muxer("seeky", |_| Box::new(TaggedMux("seekable")));
        let output = || -> SeekOutput { Box::new(Cursor::new(Vec::new())) };
        assert!(tag(r.open_muxer_seekable("seeky", output()).unwrap()).contains("seekable"));
        // No seekable factory: the plain one serves the seekable output.
        assert!(tag(r.open_muxer_seekable("plain", output()).unwrap()).contains("plain"));
        // A plain open never takes the seekable path.
        assert!(tag(r.open_muxer("seeky", Box::new(Vec::new())).unwrap()).contains("plain"));
        assert!(matches!(
            r.open_muxer_seekable("demux_only", output()),
            Err(Error::MuxerNotFound(_))
        ));
        assert!(matches!(
            r.open_muxer_seekable("missing", output()),
            Err(Error::MuxerNotFound(_))
        ));
    }
}
