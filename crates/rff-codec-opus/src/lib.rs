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

use std::collections::VecDeque;

use rusty_opus::parallel::{encode_parallel, ParallelConfig};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};
use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{AudioFrame, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat};

/// Opus frame duration in milliseconds (samples/frame = rate/1000 * this).
const FRAME_MS: usize = 20;
/// Opus supports only these sample rates.
const RATES: [u32; 5] = [8000, 12000, 16000, 24000, 48000];

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

#[derive(Default)]
struct OpusDec {
    dec: Option<OpusDecoder>,
    sample_rate: u32,
    channels: u16,
    queue: VecDeque<Frame>,
    eof: bool,
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
        let channels = params.channels.max(1);
        self.dec = Some(
            OpusDecoder::new(rate as i32, channels as usize)
                .map_err(|e| Error::invalid(format!("opus decode init: {e}")))?,
        );
        self.sample_rate = rate;
        self.channels = channels;
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let channels = self.channels as usize;
        // opus-rs wants the exact per-channel frame size and a buffer to match.
        // Our encoder emits 20 ms frames, so decode at that size.
        let frame_size = self.sample_rate as usize / 1000 * FRAME_MS;
        let dec = self
            .dec
            .as_mut()
            .ok_or_else(|| Error::invalid("opus decode: not configured"))?;

        let mut out = vec![0f32; frame_size * channels];
        let samples = dec
            .decode(&packet.data, frame_size, &mut out)
            .map_err(|e| Error::invalid(format!("opus decode: {e}")))?;
        out.truncate(samples * channels);

        let bytes: Vec<u8> = out.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.queue.push_back(Frame::Audio(AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            format: SampleFormat::F32,
            planes: vec![bytes],
            samples,
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
                .extend(std::iter::repeat(0.0).take(frame_samples - rem));
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
                        .extend(std::iter::repeat(0.0).take(frame_samples - rem));
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
            samples.push(((i as f32 * 0.05).sin() * 0.3) as f32);
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
}
