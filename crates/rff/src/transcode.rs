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
//! drives the loop to completion.
//!
//! Each input stream is either **transcoded** (decode → re-encode) when an
//! output codec is requested for its media type, or **stream-copied**
//! (remuxed packet-for-packet) when none is — the same `-c:v copy` distinction
//! FFmpeg draws. Video runs through the `-vf` filter graph (scale/crop); audio
//! is automatically resampled to a rate the target encoder accepts (FFmpeg's
//! implicit `aresample`).

use std::io::Read;
use std::path::PathBuf;

use rff_codec::{CodecParams, Decoder, Encoder};
use rff_core::{
    AudioFrame, CodecId, ColorRange, Dictionary, Error, Frame, MediaType, Packet, PixelFormat,
    Result, SampleFormat, VideoFrame,
};
use rff_filter::{FilterChain, FilterComplex};

use rff_format::{Muxer, Stream};
use rff_resample::Resampler;

use crate::Engine;

/// Codecs whose decoded output is inherently full-range (they decode to RGB, or
/// are defined on 0-255 samples). JPEG is the canonical case: the standard has
/// no limited-range mode, which is why FFmpeg calls its pixel formats `yuvj*`.
fn is_full_range_codec(codec: CodecId) -> bool {
    matches!(
        codec,
        CodecId::Jpeg | CodecId::Png | CodecId::Gif | CodecId::Webp
    )
}

/// What range this stream's decoded frames are actually in.
///
/// An `Unspecified` stream is not "unknown, do nothing" — every consumer has to
/// pick something, so pick it here, once, explicitly:
/// full-range for codecs that are defined that way, limited-range otherwise
/// (the video convention, and what an untagged y4m means).
fn effective_color_range(stream: &Stream) -> ColorRange {
    match stream.color_range {
        ColorRange::Unspecified if is_full_range_codec(stream.codec_id) => ColorRange::Full,
        ColorRange::Unspecified => ColorRange::Limited,
        explicit => explicit,
    }
}

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
    /// Output sample format pinned by the codec NAME (`-c:a pcm_s16le`) or by
    /// `-sample_fmt`. `None` = take whatever the decoder emits. Without this,
    /// raw-PCM output silently ignored the format half of its own codec name.
    pub sample_format: Option<SampleFormat>,
}

/// Which input stream(s) a `-map` entry selects.
#[derive(Debug, Clone)]
pub enum MapSelector {
    /// All streams of the input (`-map 0`).
    All,
    /// All streams of a media kind (`-map 0:v` / `0:a`).
    Kind(MediaType),
    /// One stream by index within the input (`-map 0:2`).
    Index(usize),
}

/// One `-map` entry: pick stream(s) from input `input`.
#[derive(Debug, Clone)]
pub struct MapSpec {
    pub input: usize,
    pub selector: MapSelector,
}

/// The output file for a job.
#[derive(Debug, Clone, Default)]
pub struct OutputSpec {
    pub path: PathBuf,
    /// Force a container format instead of guessing from the extension
    /// (`ffmpeg -f <fmt> ...`).
    pub format: Option<String>,
    /// Video codec for the output, if a video stream is produced.
    pub video_codec: Option<StreamCodec>,
    /// Audio codec for the output, if an audio stream is produced.
    pub audio_codec: Option<StreamCodec>,
    /// Subtitle codec (`-c:s subrip|webvtt`). Text-subtitle packets share one
    /// contract (plain text + ms timing), so this relabels the stream for the
    /// muxer rather than re-encoding anything.
    pub subtitle_codec: Option<CodecId>,
    /// Video filter graph (`-vf`), e.g. `scale=320:240,crop=...`. Applied to
    /// decoded video frames before re-encoding (transcode streams only).
    pub video_filters: Option<String>,
    /// Multi-input filter graph (`-filter_complex`). Currently models `overlay`:
    /// the last input is composited over input #0's video.
    pub filter_complex: Option<String>,
    /// Explicit stream selection (`-map`). Empty = default (all video + audio,
    /// in input/stream order).
    pub maps: Vec<MapSpec>,
    /// Overwrite the output if it exists (`-y`); otherwise fail (`-n`).
    pub overwrite: bool,
    /// Stop after this many VIDEO frames (`-frames:v N`). `None` = whole input.
    ///
    /// This is a measurement-critical option: without it a harness that thinks
    /// it is encoding a 50-frame prefix silently encodes the whole clip, and any
    /// rate-vs-quality pairing computed against a prefix is then wrong.
    pub max_video_frames: Option<u64>,
    /// Output trim window start, in seconds (`-ss`). Decoded frames (or copied
    /// packets) before this point are dropped; timestamps are shifted so the
    /// output starts at zero.
    pub trim_start: Option<f64>,
    /// Output trim window end, in seconds (`-to`, or `-ss` + `-t`). Exclusive.
    pub trim_end: Option<f64>,
    /// Constant output frame rate (`-r`), as `(num, den)` — e.g. `(30000, 1001)`.
    /// Frames are duplicated/dropped to hit it (FFmpeg's `fps` filter).
    pub frame_rate: Option<(u32, u32)>,
    /// Target audio sample rate (`-ar`). The encoder's accepted-rate list still
    /// wins if it cannot take this exact rate (nearest accepted is used).
    pub audio_rate: Option<u32>,
    /// Target audio channel count (`-ac`): 1↔2 downmix/upmix.
    pub audio_channels: Option<u16>,
    /// Audio filter chain (`-af` / `-filter:a`), e.g. `volume=-6dB,atrim=...`.
    pub audio_filters: Option<String>,
    /// Output metadata (`-metadata title=...`), handed to muxers that carry it.
    pub metadata: Dictionary,
    /// Muxer options (`-hls_time 6` → `{"hls_time": "6"}`), for formats that
    /// take them (HLS/DASH segmenting).
    pub format_options: Dictionary,
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

/// Output trim window (`-ss` / `-t` / `-to`), in seconds.
#[derive(Clone, Copy)]
struct Trim {
    start: f64,
    end: Option<f64>,
}

impl Trim {
    fn from_spec(o: &OutputSpec) -> Option<Trim> {
        if o.trim_start.is_none() && o.trim_end.is_none() {
            return None;
        }
        Some(Trim {
            start: o.trim_start.unwrap_or(0.0).max(0.0),
            end: o.trim_end,
        })
    }

    fn keeps(&self, t: f64) -> bool {
        t >= self.start - 1e-9 && self.end.map_or(true, |e| t < e - 1e-9)
    }
}

/// CFR conversion state (`-r`): input frames are snapped onto a fixed
/// `num/den` fps grid, duplicating the previous frame across gaps and dropping
/// frames that land on an already-filled slot. Emitted frames carry their slot
/// index as pts (time base `den/num`).
struct FpsConv {
    num: u32,
    den: u32,
    next_slot: i64,
    last: Option<VideoFrame>,
}

impl FpsConv {
    fn new(num: u32, den: u32) -> FpsConv {
        FpsConv {
            num: num.max(1),
            den: den.max(1),
            next_slot: 0,
            last: None,
        }
    }

    fn slot_time(&self, slot: i64) -> f64 {
        slot as f64 * self.den as f64 / self.num as f64
    }

    /// Feed one input frame at time `t` (seconds); returns the frames due
    /// strictly before `t`, each stamped with its slot pts.
    fn push(&mut self, frame: VideoFrame, t: f64) -> Vec<VideoFrame> {
        let mut out = Vec::new();
        while let Some(last) = &self.last {
            if self.slot_time(self.next_slot) < t - 1e-9 {
                let mut f = last.clone();
                f.pts = Some(self.next_slot);
                out.push(f);
                self.next_slot += 1;
            } else {
                break;
            }
        }
        self.last = Some(frame);
        out
    }

    /// End of stream: emit the held frame in its slot.
    fn finish(&mut self) -> Option<VideoFrame> {
        self.last.take().map(|mut f| {
            f.pts = Some(self.next_slot);
            self.next_slot += 1;
            f
        })
    }
}

/// What to do with one input stream on its way to the output.
enum StreamOp {
    /// Not selected (or a media type we can't route): drop its packets.
    Skip,
    /// Remux packets unchanged into the output.
    Copy(CopyOp),
    /// Decode, filter, re-encode.
    Transcode(Box<TranscodeOp>),
}

/// Stream-copy: forward packets, applying the trim window at packet level.
/// Video cuts on the first keyframe inside the window (a copy can't split a
/// GOP); timestamps shift so the output starts at zero.
struct CopyOp {
    out_index: usize,
    is_video: bool,
    /// Trim window in stream ticks (`None` = no trimming).
    start_ticks: i64,
    end_ticks: Option<i64>,
    trimming: bool,
    /// Video: set once the first in-window keyframe has passed.
    started: bool,
}

impl CopyOp {
    /// Apply trim/shift; `None` = drop the packet.
    fn process(&mut self, mut packet: Packet) -> Option<Packet> {
        if self.trimming {
            let ts = packet.pts.or(packet.dts).unwrap_or(0);
            if ts < self.start_ticks {
                return None;
            }
            if let Some(end) = self.end_ticks {
                if ts >= end {
                    return None;
                }
            }
            if self.is_video && !self.started {
                if !packet.flags.keyframe {
                    return None; // wait for a clean random-access point
                }
                self.started = true;
            }
            packet.pts = packet.pts.map(|p| p - self.start_ticks);
            packet.dts = packet.dts.map(|d| d - self.start_ticks);
        }
        packet.stream_index = self.out_index;
        Some(packet)
    }
}

/// Decode → trim → fps → filters/overlay → audio conform → encode → mux.
struct TranscodeOp {
    decoder: Box<dyn Decoder>,
    encoder: Box<dyn Encoder>,
    filters: FilterChain,
    /// `-filter_complex` overlay: a pre-decoded frame composited onto each
    /// of this stream's frames at `(x, y)` (after `filters`). Video only.
    overlay: Option<(VideoFrame, u32, u32)>,
    /// The input stream's time base (frame pts are in these ticks).
    in_time_base: rff_core::Rational,
    is_video: bool,
    /// Output trim window; pts are shifted by `shift_ticks` after it.
    trim: Option<Trim>,
    shift_ticks: i64,
    /// Fallback clock for trimming audio whose frames carry no pts.
    audio_clock_samples: u64,
    /// `-r`: CFR duplicate/drop stage.
    fps: Option<FpsConv>,
    /// `-af`: the audio filter chain (volume/atrim/...).
    audio_chain: rff_filter::AudioFilterChain,
    /// `-ac`: mix to this many channels before everything else (0 = keep).
    target_channels: u16,
    /// Sample rate the encoder needs (0 = no audio resampling required).
    target_rate: u32,
    /// Sample format the output pins (`-c:a pcm_s16le`), if any.
    target_sample_format: Option<SampleFormat>,
    /// Lazily built once the first audio frame reveals the input rate.
    resampler: Option<Resampler>,
    /// Pixel formats the encoder accepts (`None` = anything).
    accepted_formats: Option<Vec<PixelFormat>>,
    /// Lazily built conversion into an accepted format.
    pixel_converter: Option<FilterChain>,
    /// Range of the frames reaching the encoder, for that conversion.
    source_range: ColorRange,
    out_index: usize,
    /// `-frames:v` limit for this output, and how many we have sent.
    max_video_frames: Option<u64>,
    video_frames_sent: u64,
    /// pts of video frames handed to the encoder, FIFO. Video encoders that
    /// don't timestamp their packets (VP9, H.264) get these stamped on in
    /// [`TranscodeOp::drain`] — without it every Matroska block lands at t=0.
    pending_pts: std::collections::VecDeque<Option<i64>>,
}

/// Composite the `-filter_complex` overlay onto a video frame, if one is set.
/// Audio frames and the no-overlay case pass through untouched.
fn apply_overlay(overlay: &Option<(VideoFrame, u32, u32)>, frame: Frame) -> Result<Frame> {
    match (overlay, frame) {
        (Some((over, x, y)), Frame::Video(v)) => {
            Ok(Frame::Video(rff_filter::overlay(v, over, *x, *y)?))
        }
        (_, frame) => Ok(frame),
    }
}

/// Apply a video filter chain to a frame. Filters are video-only; audio passes
/// through untouched, and an empty chain is a no-op.
fn apply_filters(filters: &mut FilterChain, frame: Frame) -> Result<Frame> {
    if filters.is_empty() {
        return Ok(frame);
    }
    match frame {
        Frame::Video(v) => Ok(Frame::Video(filters.apply(v)?)),
        other => Ok(other),
    }
}

/// Pick the accepted rate closest to `target`.
fn nearest_rate(rates: &[u32], target: u32) -> u32 {
    rates
        .iter()
        .copied()
        .min_by_key(|r| (*r as i64 - target as i64).abs())
        .unwrap_or(target)
}

/// Read interleaved `s16`/`f32` plane 0 of an audio frame into `f32` samples.
fn audio_to_f32(af: &AudioFrame) -> Result<Vec<f32>> {
    match af.format {
        SampleFormat::F32 => Ok(af.planes[0]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()),
        SampleFormat::S16 => Ok(af.planes[0]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect()),
        other => Err(Error::unsupported(format!(
            "resample: sample format `{}` (need interleaved s16/f32)",
            other.name()
        ))),
    }
}

/// Wrap interleaved `f32` samples as an `f32` [`AudioFrame`].
fn f32_frame(samples: Vec<f32>, rate: u32, channels: u16, pts: Option<i64>) -> Frame {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    Frame::Audio(AudioFrame {
        sample_rate: rate,
        channels,
        format: SampleFormat::F32,
        samples: samples.len() / channels.max(1) as usize,
        planes: vec![bytes],
        pts,
    })
}

/// Convert an audio frame to `target` sample format, if one is pinned and the
/// frame is not already in it. The audio analogue of `conform_video`: raw PCM
/// output declares its format in the container header, so the frames handed to
/// the muxer must actually BE that format, not merely be labelled it.
fn conform_sample_format(target: Option<SampleFormat>, frame: Frame) -> Result<Frame> {
    let Some(target) = target else {
        return Ok(frame);
    };
    let Frame::Audio(af) = frame else {
        return Ok(frame);
    };
    if af.format == target {
        return Ok(Frame::Audio(af));
    }
    let samples = audio_to_f32(&af)?;
    let planes = match target {
        SampleFormat::F32 => vec![samples.iter().flat_map(|s| s.to_le_bytes()).collect()],
        SampleFormat::S16 => vec![samples
            .iter()
            .flat_map(|s| {
                // Round-to-nearest and clamp, matching the usual f32->s16 rule:
                // truncation would bias every sample toward zero.
                let v = (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
                v.to_le_bytes()
            })
            .collect()],
        other => {
            return Err(Error::unsupported(format!(
                "sample format conversion to `{}` (only interleaved s16/f32)",
                other.name()
            )))
        }
    };
    Ok(Frame::Audio(AudioFrame {
        sample_rate: af.sample_rate,
        channels: af.channels,
        format: target,
        samples: af.samples,
        planes,
        pts: af.pts,
    }))
}

/// Resample an audio frame to `target_rate` if needed (the resampler is built
/// lazily from the first frame's rate/channels). `target_rate == 0` is a no-op.
fn conform_audio(
    resampler: &mut Option<Resampler>,
    target_rate: u32,
    frame: Frame,
) -> Result<Frame> {
    if target_rate == 0 {
        return Ok(frame);
    }
    let Frame::Audio(af) = frame else {
        return Ok(frame);
    };
    if af.sample_rate == target_rate {
        return Ok(Frame::Audio(af));
    }
    let rs =
        resampler.get_or_insert_with(|| Resampler::new(af.sample_rate, target_rate, af.channels));
    let out = rs.process(&audio_to_f32(&af)?);
    let pts = af
        .pts
        .map(|p| p * target_rate as i64 / af.sample_rate.max(1) as i64);
    Ok(f32_frame(out, target_rate, af.channels, pts))
}

/// Convert a video frame into a pixel format the encoder accepts, if it isn't
/// already in one. The converting chain is built lazily from the first frame
/// that needs it, mirroring the audio resampler above.
///
/// This is what lets a decoder emit its cheapest native layout: the JPEG decoder
/// hands out planar Y'CbCr, and a PNG encoder downstream still gets the RGB it
/// requires because this inserts the conversion.
fn conform_video(
    converter: &mut Option<FilterChain>,
    accepted: &Option<Vec<PixelFormat>>,
    source_range: ColorRange,
    frame: Frame,
) -> Result<Frame> {
    let Some(accepted) = accepted else {
        return Ok(frame);
    };
    let Frame::Video(vf) = frame else {
        return Ok(frame);
    };
    if accepted.is_empty() || accepted.contains(&vf.format) {
        return Ok(Frame::Video(vf));
    }
    if converter.is_none() {
        // First choice the encoder listed that we can actually convert to.
        let target = accepted
            .iter()
            .find(|f| matches!(f, PixelFormat::Rgb24 | PixelFormat::Yuv420p))
            .ok_or_else(|| {
                Error::unsupported(format!(
                    "no conversion from `{}` to any format this encoder accepts",
                    vf.format.name()
                ))
            })?;
        // Pin the range: `spec.limited` describes the Y'CbCr side either way,
        // and guessing it wrong is a silent several-dB error.
        let range = match source_range {
            ColorRange::Limited => "limited",
            _ => "full",
        };
        *converter = Some(FilterChain::parse(&format!(
            "format={}:bt601:{}",
            target.name(),
            range
        ))?);
    }
    let chain = converter.as_mut().expect("just built");
    Ok(Frame::Video(chain.apply(vf)?))
}

/// Mix an audio frame to `target` channels (0 = keep). Stereo↔mono only: a
/// downmix averages pairs, an upmix duplicates the channel.
fn conform_channels(target: u16, frame: Frame) -> Result<Frame> {
    if target == 0 {
        return Ok(frame);
    }
    let Frame::Audio(af) = frame else {
        return Ok(frame);
    };
    if af.channels == target {
        return Ok(Frame::Audio(af));
    }
    let samples = audio_to_f32(&af)?;
    let mixed: Vec<f32> = match (af.channels, target) {
        (2, 1) => samples.chunks_exact(2).map(|p| 0.5 * (p[0] + p[1])).collect(),
        (1, 2) => samples.iter().flat_map(|s| [*s, *s]).collect(),
        (from, to) => {
            return Err(Error::unsupported(format!(
                "-ac: only 1<->2 channel mixing supported (input {from} ch, requested {to})"
            )))
        }
    };
    Ok(f32_frame(mixed, af.sample_rate, target, af.pts))
}

impl TranscodeOp {
    /// A frame's start time in seconds (input timeline). Audio without pts
    /// falls back to a running sample clock; video without pts cannot be
    /// placed and errors — only called when trim/fps actually need the time.
    fn frame_time(&self, frame: &Frame) -> Result<f64> {
        let tb = self.in_time_base;
        let tick = if tb.num > 0 && tb.den > 0 {
            tb.num as f64 / tb.den as f64
        } else {
            0.001
        };
        match frame.pts() {
            Some(p) => Ok(p as f64 * tick),
            None => match frame {
                Frame::Audio(af) => {
                    Ok(self.audio_clock_samples as f64 / af.sample_rate.max(1) as f64)
                }
                Frame::Video(_) => Err(Error::unsupported(
                    "-ss/-t/-to/-r need timestamps and this video stream has none",
                )),
            },
        }
    }

    /// Decoded-frame entry point: trim window, timestamp shift, CFR stage,
    /// then the per-frame pipeline in [`TranscodeOp::emit`].
    fn handle_frame(
        &mut self,
        frame: Frame,
        muxer: &mut dyn Muxer,
        report: &mut TranscodeReport,
    ) -> Result<()> {
        report.frames_decoded += 1;
        let needs_time = self.trim.is_some() || (self.is_video && self.fps.is_some());
        let t = if needs_time {
            Some(self.frame_time(&frame)?)
        } else {
            None
        };
        // The sample clock tracks ALL input audio, kept or dropped.
        if let Frame::Audio(af) = &frame {
            self.audio_clock_samples += af.samples as u64;
        }
        let mut frame = frame;
        if let (Some(trim), Some(t)) = (self.trim, t) {
            match frame {
                // Audio frames can be arbitrarily long (a WAV decodes as one
                // frame), so the window slices samples instead of keep/drop.
                Frame::Audio(af) => {
                    match rff_filter::trim_audio_frame(af, Some(t), Some(trim.start), trim.end)? {
                        Some(af) => frame = Frame::Audio(af),
                        None => return Ok(()),
                    }
                }
                Frame::Video(_) => {
                    if !trim.keeps(t) {
                        return Ok(());
                    }
                }
            }
        }
        // Shift timestamps so the output timeline starts at zero.
        if self.shift_ticks != 0 {
            match &mut frame {
                Frame::Video(v) => v.pts = v.pts.map(|p| p - self.shift_ticks),
                Frame::Audio(a) => a.pts = a.pts.map(|p| p - self.shift_ticks),
            }
        }
        match frame {
            Frame::Video(v) if self.fps.is_some() => {
                let t_out = t.expect("fps requires time") - self.trim.map_or(0.0, |w| w.start);
                let mut fps = self.fps.take().expect("checked");
                let due = fps.push(v, t_out);
                self.fps = Some(fps);
                for f in due {
                    self.emit(Frame::Video(f), muxer, report)?;
                }
                Ok(())
            }
            other => self.emit(other, muxer, report),
        }
    }

    /// Post-fps pipeline: `-frames:v` gate, filters, overlay, audio conforms,
    /// encode, drain.
    fn emit(
        &mut self,
        frame: Frame,
        muxer: &mut dyn Muxer,
        report: &mut TranscodeReport,
    ) -> Result<()> {
        if let Some(limit) = self.max_video_frames {
            if matches!(frame, Frame::Video(_)) {
                if self.video_frames_sent >= limit {
                    return Ok(());
                }
                self.video_frames_sent += 1;
            }
        }
        let frame = apply_filters(&mut self.filters, frame)?;
        let frame = apply_overlay(&self.overlay, frame)?;
        let frame = conform_channels(self.target_channels, frame)?;
        let frame = match frame {
            Frame::Audio(af) if !self.audio_chain.is_empty() => {
                let tb = self.in_time_base;
                let t = af
                    .pts
                    .map(|p| p as f64 * tb.num as f64 / tb.den.max(1) as f64);
                match self.audio_chain.apply(af, t)? {
                    Some(af) => Frame::Audio(af),
                    None => return Ok(()), // atrim consumed the whole frame
                }
            }
            other => other,
        };
        let frame = conform_audio(&mut self.resampler, self.target_rate, frame)?;
        let frame = conform_sample_format(self.target_sample_format, frame)?;
        let frame = conform_video(
            &mut self.pixel_converter,
            &self.accepted_formats,
            self.source_range,
            frame,
        )?;
        if self.is_video {
            self.pending_pts.push_back(frame.pts());
        }
        self.encoder.send_frame(&frame)?;
        self.drain(muxer, report)
    }

    /// Pull every ready packet out of the encoder and mux it. Video encoders
    /// that don't timestamp their packets (VP9, H.264 emit `pts: None`) get the
    /// corresponding source frame's pts stamped on, FIFO — without this every
    /// Matroska block would land at t=0.
    fn drain(&mut self, muxer: &mut dyn Muxer, report: &mut TranscodeReport) -> Result<()> {
        loop {
            match self.encoder.receive_packet() {
                Ok(mut packet) => {
                    if self.is_video {
                        let queued = self.pending_pts.pop_front().flatten();
                        if packet.pts.is_none() {
                            packet.pts = queued;
                        }
                    }
                    packet.stream_index = self.out_index;
                    muxer.write_packet(&packet)?;
                    report.packets_written += 1;
                }
                Err(Error::Again) | Err(Error::Eof) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// End of input: flush the decoder, the CFR stage's held frame, the
    /// resampler's FIR tail, then the encoder.
    fn finish(&mut self, muxer: &mut dyn Muxer, report: &mut TranscodeReport) -> Result<()> {
        self.decoder.flush();
        loop {
            match self.decoder.receive_frame() {
                Ok(frame) => self.handle_frame(frame, muxer, report)?,
                Err(Error::Again) | Err(Error::Eof) => break,
                Err(e) => return Err(e),
            }
        }
        if let Some(mut fps) = self.fps.take() {
            let held = fps.finish();
            self.fps = Some(fps);
            if let Some(f) = held {
                self.emit(Frame::Video(f), muxer, report)?;
            }
        }
        if let Some(rs) = &mut self.resampler {
            let tail = rs.finish();
            if !tail.is_empty() {
                let frame = f32_frame(tail, rs.out_rate(), rs.channels(), None);
                let frame = conform_sample_format(self.target_sample_format, frame)?;
                self.encoder.send_frame(&frame)?;
                self.drain(muxer, report)?;
            }
        }
        self.encoder.flush();
        self.drain(muxer, report)
    }
}

/// Resolve and run a transcode job against `engine`.
///
/// Resolution (formats, decoders, encoders) happens up front so failures are
/// reported before any output file is touched; then the demux→decode→encode→mux
/// loop runs to completion.
pub fn run(engine: &Engine, spec: &TranscodeSpec) -> Result<TranscodeReport> {
    if spec.inputs.is_empty() {
        return Err(Error::Option("no input files specified".into()));
    }
    if spec.outputs.is_empty() {
        return Err(Error::Option("no output file specified".into()));
    }
    if spec.outputs.len() > 1 {
        return Err(Error::unsupported("multiple outputs are not supported yet"));
    }
    let output = &spec.outputs[0];

    // -filter_complex overlay: the last input is composited over input #0's
    // video. Resolve it up front so stream selection can exclude that input.
    let overlay_xy = output
        .filter_complex
        .as_deref()
        .map(FilterComplex::parse)
        .transpose()?
        .and_then(|fc| fc.overlay);
    let overlay_input = match overlay_xy {
        Some(_) if spec.inputs.len() >= 2 => Some(spec.inputs.len() - 1),
        Some(_) => {
            return Err(Error::Option(
                "filter_complex overlay needs a second input (the overlay image/video)".into(),
            ))
        }
        None => None,
    };

    // --- open every input demuxer and read its streams ---
    let mut demuxers: Vec<Box<dyn rff_format::Demuxer>> = Vec::new();
    let mut input_streams: Vec<Vec<Stream>> = Vec::new();
    for input in &spec.inputs {
        let (in_format, reader) = open_input(engine, input)?;
        let mut demuxer = engine.formats.open_demuxer(&in_format, reader)?;
        input_streams.push(demuxer.read_header()?);
        demuxers.push(demuxer);
    }

    // --- select which (input, stream) pairs go to the output, in order ---
    let mut selection = select_streams(&input_streams, output)?;
    // The overlay input is consumed by the filter, not muxed as its own stream.
    if let Some(oin) = overlay_input {
        selection.retain(|(inp, _)| *inp != oin);
    }
    if selection.is_empty() {
        return Err(Error::unsupported("no streams selected for the output"));
    }

    // Single-codec containers can't stream-copy a foreign codec, so an unset
    // `-c:v` must mean TRANSCODE to the container's one codec, not copy (which
    // the muxer would reject). y4m additionally can't carry anything compressed.
    // Mirrors ffmpeg picking the muxer's default codec (`out.webp` → webp).
    // When the source already IS that codec, `None` stays: stream-copy wins.
    let mut output_owned;
    let output = if output.video_codec.is_none() {
        let format_name = resolve_output_format(engine, output).ok();
        // y4m keeps its unconditional default: it can't carry anything
        // compressed, so even rawvideo input goes through the decode path.
        let required = match format_name.as_deref() {
            Some("yuv4mpegpipe") => Some((CodecId::RawVideo, true)),
            Some("webp") => Some((CodecId::Webp, false)),
            Some("png") => Some((CodecId::Png, false)),
            Some("jpeg") => Some((CodecId::Jpeg, false)),
            Some("gif") => Some((CodecId::Gif, false)),
            Some("avif") => Some((CodecId::Avif, false)),
            Some("jpegxl") => Some((CodecId::Jxl, false)),
            _ => None,
        };
        let apply = match required {
            Some((_, true)) => true,
            // Image containers: only when the source codec differs — a matching
            // source keeps `None` and takes the stream-copy fast path.
            Some((codec, false)) => !selection.iter().any(|&(inp, local)| {
                let s = &input_streams[inp][local];
                s.media_type == MediaType::Video && s.codec_id == codec
            }),
            None => false,
        };
        if apply {
            output_owned = output.clone();
            output_owned.video_codec = Some(StreamCodec {
                codec: required.expect("apply implies required").0,
                options: Dictionary::default(),
                sample_format: None, // video: no audio sample format to pin
            });
            &output_owned
        } else {
            output
        }
    } else {
        output
    };

    // Per-input op tables (Skip for unselected streams) + ordered output streams.
    let mut per_input_ops: Vec<Vec<StreamOp>> = input_streams
        .iter()
        .map(|s| (0..s.len()).map(|_| StreamOp::Skip).collect())
        .collect();
    let mut out_streams: Vec<Stream> = Vec::new();
    for (out_index, &(inp, local)) in selection.iter().enumerate() {
        let (op, os) = build_op(engine, &input_streams[inp][local], output, out_index)?;
        per_input_ops[inp][local] = op;
        out_streams.push(os);
    }

    // --- filter_complex overlay: pre-decode the overlay frame and hand it to
    // input #0's video transcode op (which composites it onto every frame) ---
    if let (Some((x, y)), Some(oin)) = (overlay_xy, overlay_input) {
        let over = decode_overlay_frame(engine, &mut *demuxers[oin], &input_streams[oin])?;
        let vidx = input_streams[0]
            .iter()
            .position(|s| s.media_type == MediaType::Video)
            .ok_or_else(|| Error::Option("filter_complex overlay: input #0 has no video".into()))?;
        match &mut per_input_ops[0][vidx] {
            StreamOp::Transcode(op) => op.overlay = Some((over, x, y)),
            _ => {
                return Err(Error::unsupported(
                    "filter_complex overlay needs input #0's video re-encoded — pass -c:v",
                ))
            }
        }
    }

    // --- pick the muxer from the registry. Path-based formats (HLS/DASH: a
    // playlist plus many segment files) are built with the output path; every
    // other format writes one byte sink through rff-io (file, pipe, udp).
    let out_format = resolve_output_format(engine, output)?;
    let muxer_path_factory = engine
        .formats
        .by_name(&out_format)
        .filter(|fmt| fmt.can_mux())
        .ok_or_else(|| Error::MuxerNotFound(out_format.clone()))?
        .muxer_path;

    // --- refuse to clobber an existing file unless -y was given (pipes and
    // sockets have nothing to clobber) ---
    let out_path_str = output.path.to_str().unwrap_or_default();
    let is_sink_stream = rff_io::is_pipe(out_path_str) || rff_io::is_udp(out_path_str);
    if !is_sink_stream && !output.overwrite && output.path.exists() {
        return Err(Error::Option(format!(
            "{} already exists (pass -y to overwrite)",
            output.path.display()
        )));
    }
    let mut muxer: Box<dyn Muxer> = match muxer_path_factory {
        Some(factory) => factory(&output.path, &output.format_options)?,
        None => engine
            .formats
            .open_muxer(&out_format, rff_io::create(out_path_str)?)?,
    };
    if !output.metadata.is_empty() {
        muxer.set_metadata(&output.metadata);
    }
    muxer.write_header(&out_streams)?;

    let mut report = TranscodeReport::default();

    // --- drive each input through its plan into the shared muxer ---
    for (demuxer, ops) in demuxers.iter_mut().zip(per_input_ops.iter_mut()) {
        loop {
            match demuxer.read_packet() {
                Ok(packet) => process_packet(packet, ops, &mut *muxer, &mut report)?,
                Err(Error::Eof) => break,
                Err(e) => return Err(e),
            }
        }
        flush_streams(ops, &mut *muxer, &mut report)?;
    }
    muxer.write_trailer()?;

    Ok(report)
}

/// Resolve which `(input_index, stream_index)` pairs go to the output, in
/// output order. With no `-map`, defaults to every video + audio stream across
/// all inputs (input order, then stream order).
fn select_streams(inputs: &[Vec<Stream>], output: &OutputSpec) -> Result<Vec<(usize, usize)>> {
    let mut selection = Vec::new();
    if output.maps.is_empty() {
        for (ii, streams) in inputs.iter().enumerate() {
            for (i, s) in streams.iter().enumerate() {
                if matches!(
                    s.media_type,
                    MediaType::Video | MediaType::Audio | MediaType::Subtitle
                ) {
                    selection.push((ii, i));
                }
            }
        }
    } else {
        for map in &output.maps {
            let streams = inputs
                .get(map.input)
                .ok_or_else(|| Error::Option(format!("-map: no input #{}", map.input)))?;
            for (i, s) in streams.iter().enumerate() {
                let hit = match &map.selector {
                    MapSelector::All => true,
                    MapSelector::Kind(k) => s.media_type == *k,
                    MapSelector::Index(idx) => i == *idx,
                };
                if hit {
                    selection.push((map.input, i));
                }
            }
        }
    }
    Ok(selection)
}

/// Build the [`StreamOp`] (transcode or copy) and matching output [`Stream`] for
/// one selected input stream at output position `out_index`.
fn build_op(
    engine: &Engine,
    stream: &Stream,
    output: &OutputSpec,
    out_index: usize,
) -> Result<(StreamOp, Stream)> {
    let requested = match stream.media_type {
        MediaType::Video => output.video_codec.as_ref(),
        MediaType::Audio => output.audio_codec.as_ref(),
        _ => None,
    };
    match requested {
        // Transcode: decode the input codec, re-encode to the requested one.
        Some(target) => {
            let mut decoder = engine.codecs.find_decoder(stream.codec_id)?;
            decoder.configure(&codec_params(stream))?;
            let mut encoder = engine.codecs.find_encoder(target.codec)?;
            encoder.configure(&target.options)?; // rate control: -crf / -preset / -b
                                                 // Video filter graph (`-vf`); applies to video streams only.
            let mut filters = if stream.media_type == MediaType::Video {
                FilterChain::parse(output.video_filters.as_deref().unwrap_or(""))?
            } else {
                FilterChain::default()
            };
            // Colour range has to be reconciled before any filter runs. JPEG (and
            // the other image codecs) are defined on full-range samples, so
            // handing them limited-range video encodes the wrong signal — it
            // measured 28.35 dB where it should have been 36.15 dB, a deficit
            // that looked exactly like poor encoder quality. Convert once, up
            // front, then tell the rest of the chain what it is now looking at.
            let mut source_range = ColorRange::Unspecified;
            if stream.media_type == MediaType::Video {
                source_range = effective_color_range(stream);
                if is_full_range_codec(target.codec) && source_range == ColorRange::Limited {
                    filters.prepend_range_conversion(true);
                    source_range = ColorRange::Full;
                }
                filters.set_source_color_range(source_range);
            }
            let (out_w, out_h) = filters.output_dims(stream.width, stream.height);
            let is_video = stream.media_type == MediaType::Video;
            let is_audio = stream.media_type == MediaType::Audio;

            // Audio filter chain (`-af`); audio streams only.
            let audio_chain = if is_audio {
                rff_filter::AudioFilterChain::parse(output.audio_filters.as_deref().unwrap_or(""))?
            } else {
                rff_filter::AudioFilterChain::default()
            };

            // `-ac`: validate up front — only mono/stereo mixing exists.
            let target_channels = match output.audio_channels {
                Some(c) if is_audio => {
                    if !(1..=2).contains(&c) {
                        return Err(Error::unsupported(format!(
                            "-ac {c}: only 1 or 2 channels supported"
                        )));
                    }
                    c
                }
                _ => 0,
            };

            // Output sample rate: `-ar` / `aresample=` wins, else the input
            // rate; either way the encoder's accepted-rate list has the last
            // word (nearest accepted).
            let mut target_rate = 0;
            let mut out_rate = stream.sample_rate;
            if is_audio && stream.sample_rate > 0 {
                let desired = output
                    .audio_rate
                    .or(audio_chain.resample_target())
                    .unwrap_or(stream.sample_rate);
                let desired = match encoder.accepted_sample_rates() {
                    Some(rates) if !rates.contains(&desired) => nearest_rate(&rates, desired),
                    _ => desired,
                };
                if desired != stream.sample_rate {
                    target_rate = desired;
                }
                out_rate = desired;
            }

            // Trim window and the tick shift that re-zeroes the output timeline.
            let trim = Trim::from_spec(output);
            let tb = stream.time_base;
            let shift_ticks = trim
                .map(|t| (t.start * tb.den as f64 / tb.num.max(1) as f64).round() as i64)
                .unwrap_or(0);

            // `-r`: CFR stage; the output stream ticks in 1/fps units.
            let fps = match output.frame_rate {
                Some((num, den)) if is_video => Some(FpsConv::new(num, den)),
                _ => None,
            };

            let mut accepted_formats = encoder.accepted_pixel_formats();

            let mut os = Stream::new(out_index, target.codec);
            os.media_type = stream.media_type;
            os.time_base = stream.time_base;
            os.width = out_w;
            os.height = out_h;
            os.pixel_format = stream.pixel_format;
            // Raw output declares its pixel format in the container header,
            // which the muxer writes before a single frame has been seen. So the
            // format cannot be discovered later — pin it now and force frames to
            // match, or the header ends up describing data it does not have.
            // (A 4:4:4 JPEG decoded straight to planes under a `C420mpeg2`
            // header is silent corruption, not a mislabel.)
            if target.codec == CodecId::RawVideo && stream.media_type == MediaType::Video {
                let pinned = os.pixel_format.unwrap_or(PixelFormat::Yuv420p);
                os.pixel_format = Some(pinned);
                accepted_formats = Some(vec![pinned]);
            }
            // Colour range: a `format=` filter that converts to Y'CbCr decides
            // it; otherwise it rides through from the input. Decoders whose
            // native output is RGB (JPEG, PNG, GIF, WebP) are full-range by
            // definition, so an untagged input from one of those is Full, not
            // "unknown" — leaving it unspecified makes the muxer label the
            // samples limited-range and every reader then rescales them.
            // A colour conversion in the chain decides the output range;
            // otherwise it is whatever we reconciled the source to above.
            os.color_range = filters.output_color_range().unwrap_or(source_range);
            os.sample_rate = out_rate;
            os.channels = if target_channels > 0 {
                target_channels
            } else {
                stream.channels
            };
            // A CFR conversion re-times the stream: pts become slot indices.
            if let Some((num, den)) = output.frame_rate {
                if is_video {
                    os.time_base = rff_core::Rational::new(den as i32, num as i32);
                }
            }
            // A format pinned by the output codec NAME (`-c:a pcm_s16le`) wins:
            // it is an explicit request, and the muxer writes it into the
            // container header before any frame is seen. Otherwise compressed
            // decoders (AAC/Opus/Vorbis/FLAC/MP3) emit f32, so default to that
            // when the input doesn't declare a format.
            let target_sample_format = target.sample_format;
            os.sample_format = target_sample_format
                .or(stream.sample_format)
                .or(Some(SampleFormat::F32));
            // Audio encoders timestamp packets in per-channel samples, so the
            // output stream's time base is 1/sample_rate.
            if stream.media_type == MediaType::Audio && out_rate > 0 {
                os.time_base = rff_core::Rational::new(1, out_rate as i32);
            }
            Ok((
                StreamOp::Transcode(Box::new(TranscodeOp {
                    decoder,
                    encoder,
                    filters,
                    overlay: None,
                    in_time_base: stream.time_base,
                    is_video,
                    trim,
                    shift_ticks,
                    audio_clock_samples: 0,
                    fps,
                    audio_chain,
                    target_channels,
                    target_rate,
                    target_sample_format,
                    resampler: None,
                    accepted_formats,
                    pixel_converter: None,
                    source_range,
                    out_index,
                    max_video_frames: output.max_video_frames,
                    video_frames_sent: 0,
                    pending_pts: std::collections::VecDeque::new(),
                })),
                os,
            ))
        }
        // Stream copy: carry the same codec/packets through unchanged (the trim
        // window still applies, cutting video on keyframes).
        None => {
            let mut os = stream.clone();
            os.index = out_index;
            // `-c:s`: text subtitles share one packet contract, so choosing a
            // subtitle codec relabels the stream (SubRip ↔ WebVTT) in place.
            if stream.media_type == MediaType::Subtitle {
                if let Some(target) = output.subtitle_codec {
                    if target.media_type() != MediaType::Subtitle {
                        return Err(Error::unsupported(format!(
                            "-c:s: `{}` is not a subtitle codec",
                            target.name()
                        )));
                    }
                    os.codec_id = target;
                }
            }
            let trim = Trim::from_spec(output);
            let tb = stream.time_base;
            let to_ticks = |s: f64| (s * tb.den as f64 / tb.num.max(1) as f64).round() as i64;
            Ok((
                StreamOp::Copy(CopyOp {
                    out_index,
                    is_video: stream.media_type == MediaType::Video,
                    start_ticks: trim.map(|t| to_ticks(t.start)).unwrap_or(0),
                    end_ticks: trim.and_then(|t| t.end).map(to_ticks),
                    trimming: trim.is_some(),
                    started: false,
                }),
                os,
            ))
        }
    }
}

/// Decode the first video frame of the overlay input and convert it to 4:2:0,
/// ready to composite onto the base's YUV frames.
fn decode_overlay_frame(
    engine: &Engine,
    demuxer: &mut dyn rff_format::Demuxer,
    streams: &[Stream],
) -> Result<VideoFrame> {
    let vidx = streams
        .iter()
        .position(|s| s.media_type == MediaType::Video)
        .ok_or_else(|| {
            Error::Option("filter_complex overlay: overlay input has no video".into())
        })?;
    let mut decoder = engine.codecs.find_decoder(streams[vidx].codec_id)?;
    decoder.configure(&codec_params(&streams[vidx]))?;
    let mut to_yuv = FilterChain::parse("format=yuv420p")?;
    let mut got_eof = false;
    loop {
        let frame = match demuxer.read_packet() {
            Ok(pkt) if pkt.stream_index as usize != vidx => continue,
            Ok(pkt) => {
                decoder.send_packet(&pkt)?;
                decoder.receive_frame()
            }
            Err(Error::Eof) if !got_eof => {
                got_eof = true;
                decoder.flush();
                decoder.receive_frame()
            }
            Err(e) => return Err(e),
        };
        match frame {
            Ok(Frame::Video(v)) => return to_yuv.apply(v),
            Ok(_) => continue,
            Err(Error::Again) if !got_eof => continue,
            Err(Error::Again) | Err(Error::Eof) => {
                return Err(Error::Option(
                    "filter_complex overlay: no decodable frame in the overlay input".into(),
                ))
            }
            Err(e) => return Err(e),
        }
    }
}

/// Build the decoder configuration from a demuxed input stream.
fn codec_params(s: &Stream) -> CodecParams {
    CodecParams {
        codec_id: s.codec_id,
        width: s.width,
        height: s.height,
        pixel_format: s.pixel_format,
        sample_rate: s.sample_rate,
        channels: s.channels,
        sample_format: s.sample_format,
        extradata: s.extradata.clone(),
    }
}

/// Route one demuxed packet through its stream's plan.
fn process_packet(
    packet: Packet,
    ops: &mut [StreamOp],
    muxer: &mut dyn Muxer,
    report: &mut TranscodeReport,
) -> Result<()> {
    let Some(op) = ops.get_mut(packet.stream_index) else {
        return Ok(()); // packet for a stream we didn't plan — drop it
    };
    match op {
        StreamOp::Skip => Ok(()),
        StreamOp::Copy(copy) => {
            if let Some(packet) = copy.process(packet) {
                muxer.write_packet(&packet)?;
                report.packets_written += 1;
            }
            Ok(())
        }
        StreamOp::Transcode(op) => {
            op.decoder.send_packet(&packet)?;
            loop {
                match op.decoder.receive_frame() {
                    Ok(frame) => op.handle_frame(frame, muxer, report)?,
                    Err(Error::Again) | Err(Error::Eof) => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }
}

/// At end of input, flush each transcoded stream's decoder, filter state, and
/// encoder, writing out anything still buffered.
fn flush_streams(
    ops: &mut [StreamOp],
    muxer: &mut dyn Muxer,
    report: &mut TranscodeReport,
) -> Result<()> {
    for op in ops.iter_mut() {
        if let StreamOp::Transcode(op) = op {
            op.finish(muxer, report)?;
        }
    }
    Ok(())
}

/// Open an input as a streaming reader and decide its container format —
/// local file or `http://` URL. Delegates to the shared [`crate::probe`] opener
/// so ffmpeg and ffprobe resolve inputs identically.
fn open_input(engine: &Engine, input: &InputSpec) -> Result<(String, Box<dyn Read + Send>)> {
    let path = input
        .path
        .to_str()
        .ok_or_else(|| Error::Option("input path is not valid UTF-8".into()))?;
    crate::probe::open_source(engine, path, input.format.as_deref())
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
        .ok_or_else(|| {
            Error::MuxerNotFound(format!(
                "{} (no format for this extension — pass -f)",
                output.path.display()
            ))
        })
}

#[cfg(test)]
mod sample_format_tests {
    use super::*;

    fn f32_af(samples: &[f32]) -> Frame {
        f32_frame(samples.to_vec(), 44_100, 1, Some(0))
    }

    /// `-c:a pcm_s16le` used to be silently ignored on decode: the codec NAME
    /// carries the format but `CodecId` does not, so raw output kept whatever
    /// the decoder emitted (f32), writing double the bytes under an s16 request.
    #[test]
    fn pinned_format_converts_f32_to_s16() {
        let src = [0.0f32, 0.5, -0.5, 1.0, -1.0];
        let out = conform_sample_format(Some(SampleFormat::S16), f32_af(&src)).unwrap();
        let Frame::Audio(af) = out else { unreachable!() };
        assert_eq!(af.format, SampleFormat::S16);
        assert_eq!(af.planes[0].len(), src.len() * 2, "s16 is 2 bytes/sample");
        let got: Vec<i16> = af.planes[0]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        // Round-to-nearest, and full scale must CLAMP rather than wrap: 1.0 maps
        // to 32767, not to -32768 via overflow.
        assert_eq!(got, vec![0, 16384, -16384, 32767, -32768]);
    }

    /// No pin, or a frame already in the target format, must pass through
    /// untouched — the conversion is not allowed to cost anything when idle.
    #[test]
    fn unpinned_or_matching_format_is_a_passthrough() {
        let src = [0.25f32, -0.25];
        let Frame::Audio(a) = conform_sample_format(None, f32_af(&src)).unwrap() else {
            unreachable!()
        };
        assert_eq!(a.format, SampleFormat::F32);
        let Frame::Audio(b) =
            conform_sample_format(Some(SampleFormat::F32), f32_af(&src)).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(b.format, SampleFormat::F32);
        assert_eq!(b.planes[0], a.planes[0]);
    }

    /// The codec name is the only place the format lives for raw PCM.
    #[test]
    fn codec_name_pins_the_sample_format() {
        assert_eq!(
            CodecId::sample_format_from_name("pcm_s16le"),
            Some(SampleFormat::S16)
        );
        assert_eq!(
            CodecId::sample_format_from_name("pcm_f32le"),
            Some(SampleFormat::F32)
        );
        // Bare `pcm` pins nothing, and compressed codecs never do.
        assert_eq!(CodecId::sample_format_from_name("pcm"), None);
        assert_eq!(CodecId::sample_format_from_name("mp3"), None);
    }
}
