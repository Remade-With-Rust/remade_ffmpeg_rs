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

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use rff_codec::{CodecParams, Decoder, Encoder};
use rff_core::{
    AudioFrame, CodecId, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat,
    VideoFrame,
};
use rff_filter::{FilterChain, FilterComplex};
use rff_format::{Muxer, Stream};
use rff_resample::Resampler;

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

/// What to do with one input stream on its way to the output.
enum StreamOp {
    /// Media type we don't handle (e.g. subtitles): drop its packets.
    Skip,
    /// Remux packets unchanged into output stream `out_index`.
    Copy { out_index: usize },
    /// Decode, run the video filter graph, then re-encode into `out_index`.
    Transcode {
        decoder: Box<dyn Decoder>,
        encoder: Box<dyn Encoder>,
        filters: FilterChain,
        /// `-filter_complex` overlay: a pre-decoded frame composited onto each
        /// of this stream's frames at `(x, y)` (after `filters`). Video only.
        overlay: Option<(VideoFrame, u32, u32)>,
        /// Sample rate the encoder needs (0 = no audio resampling required).
        target_rate: u32,
        /// Lazily built once the first audio frame reveals the input rate.
        resampler: Option<Resampler>,
        out_index: usize,
    },
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
    let rs = resampler
        .get_or_insert_with(|| Resampler::new(af.sample_rate, target_rate, af.channels));
    let out = rs.process(&audio_to_f32(&af)?);
    let pts = af.pts.map(|p| p * target_rate as i64 / af.sample_rate.max(1) as i64);
    Ok(f32_frame(out, target_rate, af.channels, pts))
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
            StreamOp::Transcode { overlay, .. } => *overlay = Some((over, x, y)),
            _ => {
                return Err(Error::unsupported(
                    "filter_complex overlay needs input #0's video re-encoded — pass -c:v",
                ))
            }
        }
    }

    // --- confirm the muxer exists before touching disk ---
    let out_format = resolve_output_format(engine, output)?;
    engine
        .formats
        .by_name(&out_format)
        .filter(|f| f.can_mux())
        .ok_or_else(|| Error::MuxerNotFound(out_format.clone()))?;

    // --- refuse to clobber an existing file unless -y was given ---
    if !output.overwrite && output.path.exists() {
        return Err(Error::Option(format!(
            "{} already exists (pass -y to overwrite)",
            output.path.display()
        )));
    }
    let out_file = File::create(&output.path)?;
    let mut muxer = engine.formats.open_muxer(&out_format, Box::new(out_file))?;
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
fn select_streams(
    inputs: &[Vec<Stream>],
    output: &OutputSpec,
) -> Result<Vec<(usize, usize)>> {
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
            let filters = if stream.media_type == MediaType::Video {
                FilterChain::parse(output.video_filters.as_deref().unwrap_or(""))?
            } else {
                FilterChain::default()
            };
            let (out_w, out_h) = filters.output_dims(stream.width, stream.height);

            // If the encoder only accepts certain sample rates and the input
            // isn't one of them, resample to the nearest accepted rate. The
            // output stream then carries that target rate.
            let mut target_rate = 0;
            let mut out_rate = stream.sample_rate;
            if stream.media_type == MediaType::Audio && stream.sample_rate > 0 {
                if let Some(rates) = encoder.accepted_sample_rates() {
                    if !rates.contains(&stream.sample_rate) {
                        target_rate = nearest_rate(&rates, stream.sample_rate);
                        out_rate = target_rate;
                    }
                }
            }

            let mut os = Stream::new(out_index, target.codec);
            os.media_type = stream.media_type;
            os.time_base = stream.time_base;
            os.width = out_w;
            os.height = out_h;
            os.pixel_format = stream.pixel_format;
            os.sample_rate = out_rate;
            os.channels = stream.channels;
            // Compressed-audio decoders (AAC/Opus/Vorbis/FLAC) emit f32; default
            // the output stream to that when the input doesn't declare a format.
            os.sample_format = stream.sample_format.or(Some(SampleFormat::F32));
            // Audio encoders timestamp packets in per-channel samples, so the
            // output stream's time base is 1/sample_rate.
            if stream.media_type == MediaType::Audio && out_rate > 0 {
                os.time_base = rff_core::Rational::new(1, out_rate as i32);
            }
            Ok((
                StreamOp::Transcode {
                    decoder,
                    encoder,
                    filters,
                    overlay: None,
                    target_rate,
                    resampler: None,
                    out_index,
                },
                os,
            ))
        }
        // Stream copy: carry the same codec/packets through unchanged.
        None => {
            let mut os = stream.clone();
            os.index = out_index;
            Ok((StreamOp::Copy { out_index }, os))
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
        .ok_or_else(|| Error::Option("filter_complex overlay: overlay input has no video".into()))?;
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
        StreamOp::Copy { out_index } => {
            let mut packet = packet;
            packet.stream_index = *out_index;
            muxer.write_packet(&packet)?;
            report.packets_written += 1;
            Ok(())
        }
        StreamOp::Transcode {
            decoder,
            encoder,
            filters,
            overlay,
            target_rate,
            resampler,
            out_index,
        } => {
            decoder.send_packet(&packet)?;
            loop {
                match decoder.receive_frame() {
                    Ok(frame) => {
                        report.frames_decoded += 1;
                        let frame = apply_filters(filters, frame)?;
                        let frame = apply_overlay(overlay, frame)?;
                        let frame = conform_audio(resampler, *target_rate, frame)?;
                        encoder.send_frame(&frame)?;
                        drain_encoder(&mut **encoder, *out_index, muxer, report)?;
                    }
                    Err(Error::Again) | Err(Error::Eof) => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }
}

/// At end of input, flush each transcoded stream's decoder, then its encoder,
/// writing out any frames/packets they were still buffering.
fn flush_streams(
    ops: &mut [StreamOp],
    muxer: &mut dyn Muxer,
    report: &mut TranscodeReport,
) -> Result<()> {
    for op in ops.iter_mut() {
        let StreamOp::Transcode {
            decoder,
            encoder,
            filters,
            overlay,
            target_rate,
            resampler,
            out_index,
        } = op
        else {
            continue;
        };

        decoder.flush();
        loop {
            match decoder.receive_frame() {
                Ok(frame) => {
                    report.frames_decoded += 1;
                    let frame = apply_filters(filters, frame)?;
                    let frame = apply_overlay(overlay, frame)?;
                    let frame = conform_audio(resampler, *target_rate, frame)?;
                    encoder.send_frame(&frame)?;
                    drain_encoder(&mut **encoder, *out_index, muxer, report)?;
                }
                Err(Error::Again) | Err(Error::Eof) => break,
                Err(e) => return Err(e),
            }
        }

        // Flush the resampler's FIR tail so no samples are lost at end of stream.
        if let Some(rs) = resampler {
            let tail = rs.finish();
            if !tail.is_empty() {
                let frame = f32_frame(tail, rs.out_rate(), rs.channels(), None);
                encoder.send_frame(&frame)?;
                drain_encoder(&mut **encoder, *out_index, muxer, report)?;
            }
        }

        encoder.flush();
        drain_encoder(&mut **encoder, *out_index, muxer, report)?;
    }
    Ok(())
}

/// Pull every ready packet out of `encoder` and mux it into `out_index`.
fn drain_encoder(
    encoder: &mut dyn Encoder,
    out_index: usize,
    muxer: &mut dyn Muxer,
    report: &mut TranscodeReport,
) -> Result<()> {
    loop {
        match encoder.receive_packet() {
            Ok(mut packet) => {
                packet.stream_index = out_index;
                muxer.write_packet(&packet)?;
                report.packets_written += 1;
            }
            Err(Error::Again) | Err(Error::Eof) => break,
            Err(e) => return Err(e),
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
        .ok_or_else(|| Error::MuxerNotFound(output.path.display().to_string()))
}
