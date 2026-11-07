//! Opus audio codec, backed by our pure-Rust [`rusty_opus`] (BSD-3-Clause, our
//! performance fork of `opus-rs`, a port of libopus 1.6 — no FFI).
//!
//! Opus works in `f32` samples at one of 8/12/16/24/48 kHz, in fixed frame
//! durations; we use 20 ms frames. The encoder buffers incoming audio and emits
//! one Opus packet per frame; the decoder turns each packet back into an
//! [`AudioFrame`]. Sample rate + channel count reach the decoder via
//! [`configure`](rff_codec::Decoder::configure) (typically from an `OpusHead`).
//!
//! Frames may arrive as interleaved `s16` or `f32`; both are accepted (decoded
//! output is `f32`).
//!
//! When the stream's `OpusHead` (RFC 7845) arrives as extradata — as from the
//! Ogg demuxer — the decoder honours it the way FFmpeg does: the pre-skip is
//! dropped from the start, the output gain is applied, the last packet is
//! trimmed to the final granule position ([`Packet::end_position`]) — or, from
//! Matroska/WebM (whose `CodecPrivate` is the same `OpusHead`), by the last
//! block's `DiscardPadding` ([`Packet::discard_padding`]).
//! Multichannel (mapping family ≠ 0) streams are refused as unsupported.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{AudioFrame, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat};
use rusty_opus::parallel::{encode_parallel, ParallelConfig};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};

/// Opus frame duration in milliseconds (samples/frame = rate/1000 * this).
const FRAME_MS: usize = 20;
/// Opus supports only these sample rates.
const RATES: [u32; 5] = [8000, 12000, 16000, 24000, 48000];
/// The longest Opus packet: 120 ms (RFC 6716 §3.2.5).
const MAX_PACKET_MS: usize = 120;

/// Register the Opus codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Opus,
        name: "opus",
        long_name: "Opus (Opus Interactive Audio Codec)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(OpusDec::default())),
        encoder: Some(|| Box::new(OpusEnc::default())),
    });
}

fn check_rate(rate: u32) -> Result<()> {
    if RATES.contains(&rate) {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "opus: sample rate {rate} (must be 8000/12000/16000/24000/48000)"
        )))
    }
}

/// Read interleaved `s16`/`f32` plane 0 into `f32` samples.
fn frame_to_f32(af: &AudioFrame) -> Result<Vec<f32>> {
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
            "opus encode: sample format `{}` (need interleaved s16/f32)",
            other.name()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// The fields of an `OpusHead` identification header (RFC 7845 §5.1) that
/// change what the decoder outputs.
#[derive(Debug, Clone, PartialEq)]
struct OpusHead {
    channels: u16,
    /// Samples (at 48 kHz) to drop from the start of the decoded stream.
    pre_skip: u16,
    /// Output gain, Q7.8 dB.
    gain_q8: i16,
    mapping_family: u8,
}

impl OpusHead {
    /// Parse an `OpusHead`; `None` if `data` is not one.
    fn parse(data: &[u8]) -> Option<OpusHead> {
        if data.len() < 19 || &data[0..8] != b"OpusHead" {
            return None;
        }
        Some(OpusHead {
            channels: data[9] as u16,
            pre_skip: u16::from_le_bytes([data[10], data[11]]),
            gain_q8: i16::from_le_bytes([data[16], data[17]]),
            mapping_family: data[18],
        })
    }
}

#[derive(Default)]
struct OpusDec {
    dec: Option<OpusDecoder>,
    sample_rate: u32,
    channels: u16,
    /// Linear output gain from `OpusHead` (1.0 = none).
    gain: f32,
    /// Pre-skip samples still to drop from the start of the output.
    skip: usize,
    /// Stream position (granule units) after the last packet decoded: the
    /// last page granule seen plus the samples since — FFmpeg's view, which
    /// decides how much of the final packet it keeps.
    position: i64,
    queue: VecDeque<Frame>,
    eof: bool,
}

/// `(start, end)`: the slice of a packet's `decoded` samples FFmpeg keeps,
/// given the pre-skip still pending and the stream `position` before the
/// packet. Mirrors `oggparseopus.c` (end trimming: the overrun past the final
/// granule, capped at the packet; else the packet's Matroska discard padding)
/// and `decode.c` (the start skip first, which swallows any frame it covers
/// whole; the end padding only when it fits in what is left). Advances `skip`
/// past what this packet consumed.
fn keep_range(skip: &mut usize, position: i64, packet: &Packet, decoded: usize) -> (usize, usize) {
    let pad = match packet.end_position {
        Some(end) if packet.flags.end_of_stream => position
            .saturating_add(decoded as i64)
            .saturating_sub(end)
            .clamp(0, decoded as i64) as usize,
        // Matroska's `DiscardPadding` says the same thing directly.
        _ => packet.discard_padding as usize,
    };
    if *skip >= decoded {
        *skip -= decoded;
        return (0, 0);
    }
    let start = std::mem::take(skip);
    let end = if pad > 0 && pad <= decoded - start {
        decoded - pad
    } else {
        decoded
    };
    (start, end)
}

impl Decoder for OpusDec {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        // Opus defaults to 48 kHz when the stream doesn't say otherwise.
        let rate = if params.sample_rate == 0 {
            48_000
        } else {
            params.sample_rate
        };
        check_rate(rate)?;
        let head = OpusHead::parse(&params.extradata);
        let channels = match &head {
            Some(h) if h.channels > 0 => h.channels,
            _ => params.channels.max(1),
        };
        // Mapping family 1+ (surround, ambisonics) packs several Opus streams
        // per packet; only the single-stream family 0 is decoded here.
        if let Some(family @ 1..) = head.as_ref().map(|h| h.mapping_family) {
            return Err(Error::unsupported(format!(
                "opus decode: channel mapping family {family} (multichannel Opus)"
            )));
        }
        let dec = OpusDecoder::new(rate as i32, channels as usize)
            .map_err(|e| Error::invalid(format!("opus decode init: {e}")))?;
        // Pre-skip (like granules) counts 48 kHz samples.
        self.skip = head
            .as_ref()
            .map_or(0, |h| (h.pre_skip as u64 * rate as u64 / 48_000) as usize);
        // FFmpeg: gain = 10^(Q7.8 dB / 20).
        self.gain = head.as_ref().map_or(1.0, |h| {
            10f64.powf(h.gain_q8 as f64 / (20.0 * 256.0)) as f32
        });
        self.position = 0;
        self.dec = Some(dec);
        self.sample_rate = rate;
        self.channels = channels;
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let channels = self.channels as usize;
        // The decode size is a capacity: room for the longest packet.
        let frame_size = self.sample_rate as usize / 1000 * MAX_PACKET_MS;
        let dec = self
            .dec
            .as_mut()
            .ok_or_else(|| Error::invalid("opus decode: not configured"))?;

        let mut out = vec![0f32; frame_size * channels];
        let decoded = dec
            .decode(&packet.data, frame_size, &mut out)
            .map_err(|e| Error::invalid(format!("opus decode: {e}")))?;
        let (start, end) = keep_range(&mut self.skip, self.position, packet, decoded);
        self.position = packet
            .end_position
            .unwrap_or(self.position.saturating_add(decoded as i64));
        if start == end {
            return Ok(());
        }

        let mut bytes = Vec::with_capacity((end - start) * channels * 4);
        for &s in &out[start * channels..end * channels] {
            bytes.extend_from_slice(&(s * self.gain).to_le_bytes());
        }
        self.queue.push_back(Frame::Audio(AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            format: SampleFormat::F32,
            planes: vec![bytes],
            samples: end - start,
            pts: packet.pts,
        }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.queue.pop_front() {
            return Ok(frame);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct OpusEnc {
    enc: Option<OpusEncoder>,
    sample_rate: u32,
    channels: u16,
    /// Samples per channel in one Opus frame (20 ms).
    frame_size: usize,
    /// Interleaved f32 samples awaiting a full frame.
    buffer: Vec<f32>,
    /// PTS (in per-channel samples) for the next emitted frame.
    next_pts: i64,
    queue: VecDeque<Packet>,
    eof: bool,
    // --- rate-control / tuning (from `configure`, FFmpeg-style opts) ---
    /// Target bitrate (bits/s), `-b:a`. Default 64 kbps (matches libopus's default).
    bitrate: i32,
    /// Encoder complexity 0–10 (`-compression_level`). Higher = better/slower.
    complexity: i32,
    /// Constant bitrate when set (`-vbr off` / `-b:a` with CBR). Default VBR.
    use_cbr: bool,
    /// `voip` vs `audio` application (`-application`). Default audio.
    voip: bool,
    /// Frame/chunk-parallel encoding (R1): buffer the whole stream and encode it
    /// across cores at `flush`, beating single-threaded libopus wall-clock.
    /// Default on (batch/file semantics); `-opus_parallel 0` forces serial for
    /// low-latency/streaming. Chunk seams are PEAQ-neutral (see `warmup`).
    parallel: bool,
    /// Look-back frames each worker re-encodes to prime its state (PEAQ-neutral
    /// at ≥4; default 8). `-opus_warmup N`.
    warmup: usize,
    /// Worker thread count (`0` = all cores). `-threads N`.
    threads: usize,
}

impl Default for OpusEnc {
    fn default() -> Self {
        OpusEnc {
            enc: None,
            sample_rate: 0,
            channels: 0,
            frame_size: 0,
            buffer: Vec::new(),
            next_pts: 0,
            queue: VecDeque::new(),
            eof: false,
            bitrate: 64_000,
            complexity: 9,
            use_cbr: false,
            voip: false,
            parallel: true,
            warmup: 8,
            threads: 0,
        }
    }
}

impl OpusEnc {
    fn init(&mut self, af: &AudioFrame) -> Result<()> {
        check_rate(af.sample_rate)?;
        let channels = af.channels.max(1);
        let app = if self.voip {
            Application::Voip
        } else {
            Application::Audio
        };
        let mut enc = OpusEncoder::new(af.sample_rate as i32, channels as usize, app)
            .map_err(|e| Error::invalid(format!("opus encode init: {e}")))?;
        enc.bitrate_bps = self.bitrate;
        enc.complexity = self.complexity.clamp(0, 10);
        enc.use_cbr = self.use_cbr;
        self.enc = Some(enc);
        self.sample_rate = af.sample_rate;
        self.channels = channels;
        self.frame_size = af.sample_rate as usize / 1000 * FRAME_MS;
        Ok(())
    }

    /// Encode as many whole frames as `buffer` currently holds.
    fn drain_frames(&mut self) -> Result<()> {
        let frame_samples = self.frame_size * self.channels as usize;
        if frame_samples == 0 || self.buffer.len() < frame_samples {
            return Ok(());
        }
        // Iterate whole frames in place with a cursor and drain the consumed
        // prefix ONCE at the end. Draining a frame off the FRONT each iteration
        // (`buffer.drain(..frame_samples)`) shifts the whole remaining buffer down
        // per frame — O(n²) over a fully-buffered stream, and it dominated encode
        // wall time (~5× the codec itself). Destructure `self` so the input slice
        // (`buffer`) and the encoder (`enc`) borrow as distinct fields.
        let OpusEnc {
            enc,
            buffer,
            queue,
            next_pts,
            frame_size,
            ..
        } = self;
        let enc = enc.as_mut().expect("encoder initialized");
        let frame_size = *frame_size;
        let n_full = buffer.len() / frame_samples;
        for i in 0..n_full {
            let chunk = &buffer[i * frame_samples..(i + 1) * frame_samples];
            let mut out = vec![0u8; 4000]; // max Opus packet size
            let n = enc
                .encode(chunk, frame_size, &mut out)
                .map_err(|e| Error::invalid(format!("opus encode: {e}")))?;
            out.truncate(n);
            // PTS in per-channel samples; each 20 ms frame advances by frame_size.
            let mut packet = Packet::from_data(0, out);
            packet.pts = Some(*next_pts);
            *next_pts += frame_size as i64;
            queue.push_back(packet);
        }
        buffer.drain(..n_full * frame_samples);
        Ok(())
    }

    /// Encode the whole buffered stream in parallel (R1) at flush, emitting one
    /// PTS-stamped packet per 20 ms frame in order.
    fn drain_parallel(&mut self) {
        let frame_samples = self.frame_size * self.channels as usize;
        if frame_samples == 0 || self.buffer.is_empty() {
            return;
        }
        let rem = self.buffer.len() % frame_samples;
        if rem != 0 {
            self.buffer
                .extend(std::iter::repeat_n(0.0, frame_samples - rem));
        }
        let app = if self.voip {
            Application::Voip
        } else {
            Application::Audio
        };
        let mut cfg = ParallelConfig::new(self.sample_rate as i32, self.channels as usize, app);
        cfg.bitrate_bps = self.bitrate;
        cfg.complexity = self.complexity;
        cfg.use_cbr = self.use_cbr;
        cfg.warmup = self.warmup;
        cfg.threads = self.threads;
        let packets = encode_parallel(&cfg, &self.buffer, self.frame_size);
        self.buffer.clear();
        for data in packets {
            let mut packet = Packet::from_data(0, data);
            packet.pts = Some(self.next_pts);
            self.next_pts += self.frame_size as i64;
            self.queue.push_back(packet);
        }
    }
}

impl Encoder for OpusEnc {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(b) = options.get_bitrate("b") {
            if b > 0 {
                self.bitrate = b as i32;
            }
        }
        // `-compression_level 0..10` maps to Opus complexity (FFmpeg's mapping).
        if let Some(c) = options.get_int("compression_level") {
            self.complexity = (c as i32).clamp(0, 10);
        }
        if let Some(app) = options.get("application") {
            self.voip = app.eq_ignore_ascii_case("voip") || app.eq_ignore_ascii_case("lowdelay");
        }
        // `-vbr off` / `-vbr 0` → CBR (FFmpeg's libopus `vbr` option).
        if let Some(v) = options.get("vbr") {
            self.use_cbr = v == "off" || v == "0" || v.eq_ignore_ascii_case("false");
        }
        // R1 frame-parallel controls.
        if let Some(p) = options.get("opus_parallel") {
            self.parallel = !(p == "0" || p == "off" || p.eq_ignore_ascii_case("false"));
        }
        if let Some(w) = options.get_int("opus_warmup") {
            self.warmup = w.max(0) as usize;
        }
        if let Some(t) = options.get_int("threads") {
            self.threads = t.max(0) as usize;
        }
        Ok(())
    }

    fn accepted_sample_rates(&self) -> Option<Vec<u32>> {
        Some(RATES.to_vec())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let af = match frame {
            Frame::Audio(a) => a,
            Frame::Video(_) => {
                return Err(Error::unsupported(
                    "opus encode: video frame on an audio codec",
                ))
            }
        };
        if self.enc.is_none() {
            self.init(af)?;
            // Seed output PTS from the first frame's timestamp (per-channel samples).
            self.next_pts = af.pts.unwrap_or(0);
        } else if af.sample_rate != self.sample_rate || af.channels != self.channels {
            return Err(Error::unsupported(
                "opus encode: stream layout changed mid-stream",
            ));
        }
        self.buffer.extend(frame_to_f32(af)?);
        // Parallel mode accumulates the whole stream and encodes it at `flush`;
        // serial mode drains whole frames incrementally as they arrive.
        if self.parallel {
            Ok(())
        } else {
            self.drain_frames()
        }
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.queue.pop_front() {
            return Ok(packet);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        // `init` runs on the first frame, so `frame_size` is set iff we saw input.
        if self.frame_size != 0 && !self.buffer.is_empty() {
            if self.parallel {
                self.drain_parallel();
            } else {
                // Pad a trailing partial frame with silence so it still encodes.
                let frame_samples = self.frame_size * self.channels as usize;
                let rem = self.buffer.len() % frame_samples;
                if rem != 0 {
                    self.buffer
                        .extend(std::iter::repeat_n(0.0, frame_samples - rem));
                }
                let _ = self.drain_frames();
            }
        }
        self.eof = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tone to Opus and decode it back; check the pipeline runs and a
    /// plausible number of samples come out (Opus is lossy — no exact compare).
    #[test]
    fn opus_encode_then_decode() {
        let (rate, channels) = (48_000u32, 1u16);
        let frame = rate as usize / 1000 * FRAME_MS; // 960
        let mut samples = Vec::new();
        for i in 0..frame * 10 {
            samples.push((i as f32 * 0.05).sin() * 0.3);
        }
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();

        let mut enc = OpusEnc::default();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: rate,
            channels,
            format: SampleFormat::F32,
            planes: vec![bytes],
            samples: samples.len(),
            pts: Some(0),
        }))
        .unwrap();
        enc.flush();

        let mut packets = Vec::new();
        loop {
            match enc.receive_packet() {
                Ok(p) => {
                    assert!(!p.data.is_empty());
                    packets.push(p);
                }
                Err(Error::Eof) | Err(Error::Again) => break,
                Err(e) => panic!("encode: {e}"),
            }
        }
        assert!(
            packets.len() >= 8,
            "expected ~10 frames, got {}",
            packets.len()
        );

        let mut dec = OpusDec::default();
        dec.configure(&CodecParams {
            sample_rate: rate,
            channels,
            ..Default::default()
        })
        .unwrap();
        let mut decoded = 0usize;
        let mut energy = 0f64;
        for p in &packets {
            dec.send_packet(p).unwrap();
            if let Ok(Frame::Audio(af)) = dec.receive_frame() {
                assert_eq!(af.format, SampleFormat::F32);
                decoded += af.samples;
                for b in af.planes[0].chunks_exact(4) {
                    let s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64;
                    energy += s * s;
                }
            }
        }
        assert!(decoded >= frame * 8, "decoded too few samples: {decoded}");
        let rms = (energy / decoded.max(1) as f64).sqrt();
        assert!(rms > 0.02, "decoded audio is silent: rms {rms:.4}");
    }

    /// An `OpusHead` with the given channel count, pre-skip, gain and family.
    fn head(channels: u8, pre_skip: u16, gain_q8: i16, family: u8) -> Vec<u8> {
        let mut v = b"OpusHead".to_vec();
        v.push(1);
        v.push(channels);
        v.extend_from_slice(&pre_skip.to_le_bytes());
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&gain_q8.to_le_bytes());
        v.push(family);
        if family != 0 {
            v.extend_from_slice(&[4, 2, 0, 4, 1, 2, 3, 5]); // 5.1 tables
        }
        v
    }

    /// `n` 20 ms stereo packets of a tone, from the serial encoder.
    fn encoded(n: usize) -> Vec<Packet> {
        let mut enc = OpusEnc {
            parallel: false,
            ..Default::default()
        };
        let samples: Vec<f32> = (0..960 * n * 2)
            .map(|i| ((i / 2) as f32 * 0.05).sin() * 0.3)
            .collect();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![samples.iter().flat_map(|s| s.to_le_bytes()).collect()],
            samples: 960 * n,
            pts: Some(0),
        }))
        .unwrap();
        enc.flush();
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
        assert_eq!(packets.len(), n);
        packets
    }

    /// Decode `packets` with `extradata`; returns the interleaved output.
    fn decode(extradata: Vec<u8>, packets: &[Packet]) -> Vec<f32> {
        let mut dec = OpusDec::default();
        dec.configure(&CodecParams {
            sample_rate: 48_000,
            channels: 2,
            extradata,
            ..Default::default()
        })
        .unwrap();
        let mut out = Vec::new();
        for p in packets {
            dec.send_packet(p).unwrap();
            while let Ok(Frame::Audio(af)) = dec.receive_frame() {
                assert_eq!(af.planes[0].len(), af.samples * 2 * 4);
                out.extend(
                    af.planes[0]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
            }
        }
        out
    }

    #[test]
    fn opus_head_parses() {
        let h = OpusHead::parse(&head(2, 312, -256, 0)).unwrap();
        assert_eq!(
            h,
            OpusHead {
                channels: 2,
                pre_skip: 312,
                gain_q8: -256,
                mapping_family: 0,
            }
        );
        assert_eq!(OpusHead::parse(b"OpusTags........."), None);
        assert_eq!(OpusHead::parse(&head(2, 312, 0, 0)[..18]), None);
    }

    #[test]
    fn keep_range_mirrors_ffmpeg() {
        let mut p = Packet::from_data(0, Vec::new());
        // Pre-skip spanning packets: whole frames swallowed, then a cut.
        let mut skip = 1000;
        assert_eq!(keep_range(&mut skip, 0, &p, 960), (0, 0));
        assert_eq!(skip, 40);
        assert_eq!(keep_range(&mut skip, 960, &p, 960), (40, 960));
        assert_eq!(skip, 0);
        // End trim: only the end-of-stream packet, by its overrun.
        p.end_position = Some(5000);
        assert_eq!(keep_range(&mut skip, 4800, &p, 960), (0, 960));
        p.flags.end_of_stream = true;
        assert_eq!(keep_range(&mut skip, 4800, &p, 960), (0, 200));
        assert_eq!(keep_range(&mut skip, 3000, &p, 960), (0, 960), "no overrun");
        assert_eq!(keep_range(&mut skip, 6000, &p, 960), (0, 0), "capped: all");
        // Skip and padding on one packet: padding must fit what's left.
        let mut skip = 100;
        assert_eq!(keep_range(&mut skip, 4800, &p, 960), (100, 200));
        let mut skip = 900;
        assert_eq!(keep_range(&mut skip, 4800, &p, 960), (900, 960));
    }

    /// Matroska/WebM: the last block's DiscardPadding, in samples, cuts its
    /// end under the same `decode.c` rules (it must fit after the skip).
    #[test]
    fn discard_padding_trims_like_the_final_granule() {
        let mut p = Packet::from_data(0, Vec::new());
        p.discard_padding = 648;
        let mut skip = 0;
        assert_eq!(keep_range(&mut skip, 0, &p, 960), (0, 312));
        assert_eq!(keep_range(&mut skip, 0, &p, 648), (0, 0), "all of it");
        assert_eq!(
            keep_range(&mut skip, 0, &p, 600),
            (0, 600),
            "too long: ignored"
        );
        let mut skip = 312;
        assert_eq!(
            keep_range(&mut skip, 0, &p, 960),
            (312, 312),
            "all that's left"
        );
        let mut skip = 312;
        p.discard_padding = 649;
        assert_eq!(
            keep_range(&mut skip, 0, &p, 960),
            (312, 960),
            "no room after skip"
        );
        let mut skip = 312;
        p.discard_padding = 48;
        assert_eq!(keep_range(&mut skip, 0, &p, 960), (312, 912));
    }

    /// Pre-skip comes off the front, the final granule trims the back: a
    /// 2 s stream with libopus's 312-sample pre-skip decodes to exactly 2 s.
    #[test]
    fn pre_skip_and_final_granule_trim_the_stream() {
        let mut packets = encoded(10);
        let raw = decode(head(2, 0, 0, 0), &packets);
        assert_eq!(raw.len(), 960 * 10 * 2);
        // Pages of 4 packets; the last page's granule ends 500 samples early.
        let granule_end = 960 * 10 - 500;
        for (i, p) in packets.iter_mut().enumerate() {
            if i == 9 {
                p.end_position = Some(granule_end);
                p.flags.end_of_stream = true;
            } else if i % 4 == 3 {
                p.end_position = Some(960 * (i as i64 + 1));
            }
        }
        let got = decode(head(2, 312, 0, 0), &packets);
        let want = &raw[312 * 2..granule_end as usize * 2];
        assert_eq!(got.len(), want.len());
        assert_eq!(got, want);
    }

    /// OpusHead output gain is applied as FFmpeg does: 10^(Q7.8 dB / 20).
    #[test]
    fn output_gain_is_applied() {
        let packets = encoded(3);
        let raw = decode(head(2, 0, 0, 0), &packets);
        let loud = decode(head(2, 0, 6 * 256, 0), &packets); // +6 dB
        let g = 10f64.powf(6.0 / 20.0) as f32;
        assert_eq!(loud.len(), raw.len());
        for (a, b) in raw.iter().zip(&loud) {
            assert_eq!(a * g, *b);
        }
    }

    /// Multistream (surround) packets are not decoded: refuse them clearly
    /// rather than misread them as one stream.
    #[test]
    fn multichannel_mapping_is_refused() {
        let mut dec = OpusDec::default();
        let err = dec
            .configure(&CodecParams {
                sample_rate: 48_000,
                channels: 6,
                extradata: head(6, 312, 0, 1),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
    }
}
