//! `rff-codec-mp3` — an in-house, pure-Rust MP3 (MPEG-1/2 Audio Layer III)
//! decoder **and** encoder.
//!
//! Why in-house: the robust Rust MP3 crate (Symphonia) is MPL-2.0, which trips
//! our no-copyleft-in-core license gate; the permissive one (puremp3) is
//! incomplete. MP3's patents expired in 2017 and Layer III is exhaustively
//! documented, so we build our own — the same path as AAC and VP9. See
//! `docs/ffmpeg-parity.md`.
//!
//! ## Layout (the framework, built brick by brick)
//!
//! * shared: [`header`] (frame header), [`frame`] (side-info + types),
//!   [`bitio`] (MSB-first bit I/O), [`tables`] (ISO constant tables)
//! * [`decode`]: side-info → reservoir → Huffman → scalefactors → requantize →
//!   stereo → antialias → IMDCT → synthesis filterbank → PCM
//! * [`encode`]: analysis filterbank → MDCT → psychoacoustic model → two-loop
//!   quantizer → Huffman → bitstream
//!
//! The pipeline wiring is in place; each DSP stage is a `todo!()`/`Unimplemented`
//! brick. The public [`Decoder`]/[`Encoder`] return a labelled `Unimplemented`
//! until the bricks are laid, so a transcode resolves the whole graph and stops
//! cleanly at MP3.
#![allow(dead_code)] // Scaffold: stage bricks are wired but not yet implemented.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{
    AudioFrame, CodecId, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat,
};

mod bitio;
mod decode;
mod encode;
mod frame;
mod header;
mod tables;

/// MP3 encoder experiment harness — brick tracking, corpus, metrics, variant
/// sweeps. Opt-in behind the `lab` feature; see docs/mp3-lab.md.
#[cfg(feature = "lab")]
pub mod lab;

pub use decode::Mp3Decode;
pub use encode::Mp3Encode;
use header::FrameHeader;

/// Register the MP3 codec (decoder + encoder) into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Mp3,
        name: "mp3",
        long_name: "MP3 (MPEG-1/2 Audio Layer III)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(Mp3Decoder::default())),
        encoder: Some(|| Box::new(Mp3Encoder::default())),
    });
}

#[derive(Default)]
struct Mp3Decoder {
    state: Mp3Decode,
    /// Accumulated bytes awaiting frame-sync (packets may split/join frames).
    buf: Vec<u8>,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl Mp3Decoder {
    /// Frame-sync the buffer: for each complete frame, split header / side-info /
    /// main-data, decode it, and queue an `AudioFrame`. Leaves a trailing partial
    /// frame in `buf` for the next packet.
    fn parse_frames(&mut self) {
        let mut pos = 0;
        while pos + 4 <= self.buf.len() {
            // Sync = 11 set bits: 0xFF then top 3 bits of the next byte.
            if self.buf[pos] != 0xFF || self.buf[pos + 1] & 0xE0 != 0xE0 {
                pos += 1;
                continue;
            }
            let hb = [
                self.buf[pos],
                self.buf[pos + 1],
                self.buf[pos + 2],
                self.buf[pos + 3],
            ];
            let header = match FrameHeader::parse(hb) {
                Ok(h) => h,
                Err(_) => {
                    pos += 1;
                    continue;
                }
            };
            let frame_size = header.frame_size();
            if frame_size < 4 {
                pos += 1;
                continue;
            }
            if pos + frame_size > self.buf.len() {
                break; // incomplete frame — wait for more data
            }

            let crc = if header.crc_protected { 2 } else { 0 };
            let si_start = pos + 4 + crc;
            let si_len = header.side_info_len();
            let main_start = si_start + si_len;
            if main_start > pos + frame_size {
                pos += 1;
                continue;
            }
            let side_info = self.buf[si_start..main_start].to_vec();
            let main_data = self.buf[main_start..pos + frame_size].to_vec();

            if let Ok(pcm) = self.state.decode_frame(&header, &side_info, &main_data) {
                let channels = header.channel_mode.channels().max(1);
                let mut bytes = Vec::with_capacity(pcm.len() * 4);
                for s in &pcm {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                self.queue.push_back(Frame::Audio(AudioFrame {
                    sample_rate: header.sample_rate,
                    channels: channels as u16,
                    format: SampleFormat::F32,
                    planes: vec![bytes],
                    samples: pcm.len() / channels,
                    pts: None,
                }));
            }
            pos += frame_size;
        }
        self.buf.drain(0..pos);
    }
}

impl Decoder for Mp3Decoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.buf.extend_from_slice(&packet.data);
        self.parse_frames();
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

/// MP3 encoder: accumulates per-channel PCM, emits one MP3 frame per 1152 samples
/// per channel. MPEG-1, CBR or VBR, psychoacoustic noise shaping; mono, stereo,
/// or per-frame mid/side joint stereo. Configured via `-b:a` (CBR) / `-q:a` (VBR).
#[derive(Default)]
struct Mp3Encoder {
    state: Mp3Encode,
    header: Option<FrameHeader>,
    /// Accumulated samples per channel awaiting a full frame.
    pcm: [Vec<f32>; 2],
    queue: VecDeque<rff_core::Packet>,
    /// Audio frames emitted and their total byte length (for the Info header).
    total_frames: u32,
    total_bytes: usize,
    /// CBR target (kbps); 0 ⇒ default 128.
    cbr_kbps: u32,
    /// VBR quality target (peak NMR). `Some` ⇒ VBR, `None` ⇒ CBR.
    quality: Option<f32>,
    eof: bool,
}

/// Snap a requested bitrate (kbps) to the nearest valid Layer III value for the
/// MPEG version (the V1 and V2/2.5 bitrate tables differ).
fn snap_bitrate(version: header::MpegVersion, kbps: u32) -> u32 {
    let v1 = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    let v2 = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let valid: &[u32] = if version == header::MpegVersion::V1 {
        &v1
    } else {
        &v2
    };
    *valid
        .iter()
        .min_by_key(|&&b| b.abs_diff(kbps))
        .unwrap_or(&128)
}

/// Parse an FFmpeg-style bitrate string ("128k", "192000") into kbps.
fn parse_bitrate(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix(['k', 'K']) {
        n.trim().parse::<f32>().ok().map(|x| x as u32)
    } else if let Some(n) = s.strip_suffix(['m', 'M']) {
        n.trim().parse::<f32>().ok().map(|x| (x * 1000.0) as u32)
    } else {
        s.parse::<u32>()
            .ok()
            .map(|x| if x >= 1000 { x / 1000 } else { x })
    }
}

/// Build the frame header from the input's sample rate, channel count, and the
/// configured CBR bitrate (VBR overrides the bitrate per frame later).
fn encoder_header(sample_rate: u32, channels: u16, cbr_kbps: u32) -> Result<FrameHeader> {
    let version = match sample_rate {
        32000 | 44100 | 48000 => header::MpegVersion::V1,
        16000 | 22050 | 24000 => header::MpegVersion::V2,
        8000 | 11025 | 12000 => header::MpegVersion::V2_5,
        _ => {
            return Err(Error::unsupported(
                "mp3 encode: unsupported sample rate (need 8–48 kHz MPEG-1/2/2.5)",
            ))
        }
    };
    let channel_mode = if channels >= 2 {
        frame::ChannelMode::Stereo
    } else {
        frame::ChannelMode::Mono
    };
    // V2/2.5 default to a lower nominal bitrate to fit the smaller frame.
    let default_kbps = if version == header::MpegVersion::V1 {
        128
    } else {
        64
    };
    let bitrate_kbps = snap_bitrate(
        version,
        if cbr_kbps == 0 {
            default_kbps
        } else {
            cbr_kbps
        },
    );
    Ok(FrameHeader {
        version,
        crc_protected: false,
        bitrate_kbps,
        sample_rate,
        padding: false,
        channel_mode,
        copyright: false,
        original: true,
        emphasis: 0,
    })
}

impl Mp3Encoder {
    /// Emit a frame for each full 1152-sample-per-channel block accumulated.
    fn drain_frames(&mut self) {
        let Some(header) = self.header.clone() else {
            return;
        };
        let nch = header.channel_mode.channels();
        let spf = header.version.samples_per_frame();
        while self.pcm[0].len() >= spf && (nch == 1 || self.pcm[1].len() >= spf) {
            let block: Vec<Vec<f32>> = (0..nch)
                .map(|c| self.pcm[c].drain(0..spf).collect())
                .collect();
            if let Ok(bytes) = self.state.encode_frame(&header, &block, self.quality) {
                self.total_frames += 1;
                self.total_bytes += bytes.len();
                self.queue.push_back(Packet::from_data(0, bytes));
            }
        }
    }
}

impl Encoder for Mp3Encoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(b) = options.get("b").and_then(parse_bitrate) {
            self.cbr_kbps = b;
        }
        // VBR quality via -q:a / -qp:a / -crf:a (0 = best … 9 = smallest). Its
        // presence switches to VBR; map the index to a peak-NMR target.
        for key in ["q", "qp", "crf", "qscale"] {
            if let Some(q) = options.get(key).and_then(|v| v.trim().parse::<f32>().ok()) {
                self.quality = Some(10f32.powf((q.clamp(0.0, 9.0) * 2.0) / 10.0));
                break;
            }
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(af) = frame else {
            return Ok(());
        };
        if self.header.is_none() {
            self.header = Some(encoder_header(af.sample_rate, af.channels, self.cbr_kbps)?);
        }
        let nch = self.header.as_ref().unwrap().channel_mode.channels();
        let in_ch = (af.channels as usize).max(1);
        let data = &af.planes[0];
        // Deinterleave; if the input has fewer channels than the output, replicate.
        for s in 0..af.samples {
            for c in 0..nch {
                let ic = c.min(in_ch - 1);
                let off = (s * in_ch + ic) * 4;
                let v = if off + 4 <= data.len() {
                    f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                } else {
                    0.0
                };
                self.pcm[c].push(v);
            }
        }
        self.drain_frames();
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.queue.pop_front() {
            return Ok(p);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        // Pad each channel's tail to a whole frame and encode it.
        if let Some(header) = self.header.clone() {
            let nch = header.channel_mode.channels();
            let spf = header.version.samples_per_frame();
            if (0..nch).any(|c| !self.pcm[c].is_empty()) {
                for c in 0..nch {
                    let padded = self.pcm[c].len().div_ceil(spf) * spf;
                    self.pcm[c].resize(padded, 0.0);
                }
                self.drain_frames();
            }
            // Prepend the Xing/Info header now that the totals are known (counts
            // include the Info frame itself). Streaming consumers that drain before
            // flush won't get it first — that case wants two-pass.
            if self.total_frames > 0 {
                let fsize = header.frame_size() as u32;
                let info = encode::bitstream::info_frame(
                    &header,
                    self.total_frames + 1,
                    self.total_bytes as u32 + fsize,
                    self.quality.is_some(),
                );
                self.queue.push_front(Packet::from_data(0, info));
            }
        }
        self.eof = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helpers for the encode→decode pipeline gate.
    fn pcm_to_bytes(pcm: &[f32]) -> Vec<u8> {
        let mut b = Vec::with_capacity(pcm.len() * 4);
        for &s in pcm {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    fn encode_mono(input: &[f32], sample_rate: u32) -> Vec<u8> {
        let mut enc = Mp3Encoder::default();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![pcm_to_bytes(input)],
            samples: input.len(),
            pts: None,
        }))
        .unwrap();
        enc.flush();
        let mut mp3 = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            mp3.extend_from_slice(&p.data);
        }
        mp3
    }

    fn decode_mono(mp3: Vec<u8>) -> Vec<f32> {
        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, mp3)).unwrap();
        dec.flush();
        let mut out = Vec::new();
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            for c in af.planes[0].chunks_exact(4) {
                out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        out
    }

    /// Best-aligned reconstruction SNR (dB) of `out` vs `reference`, searching the
    /// codec delay and skipping warm-up at both ends.
    fn best_snr(reference: &[f32], out: &[f32]) -> f64 {
        let (mut best, skip) = (f64::NEG_INFINITY, 2304usize);
        for delay in 0..3000 {
            let mut sig = 0f64;
            let mut err = 0f64;
            let mut n = 0;
            let mut i = skip;
            while i + delay < out.len() && i < reference.len() {
                let r = reference[i] as f64;
                sig += r * r;
                err += (r - out[i + delay] as f64).powi(2);
                n += 1;
                i += 1;
            }
            if n > 5000 && err > 0.0 {
                best = best.max(10.0 * (sig / err).log10());
            }
        }
        best
    }

    /// **R1 — stereo.** Two different tones in L and R survive independently.
    #[test]
    fn encode_decode_stereo() {
        let sr = 44100u32;
        let frames = 16;
        let n = frames * 1152;
        let pi2 = 2.0 * std::f32::consts::PI;
        // Interleaved L/R: L = 700 Hz, R = 3000 Hz.
        let mut interleaved = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            interleaved.push(0.35 * (pi2 * 700.0 * t).sin()); // L
            interleaved.push(0.30 * (pi2 * 3000.0 * t).sin()); // R
        }

        let mut enc = Mp3Encoder::default();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![pcm_to_bytes(&interleaved)],
            samples: n,
            pts: None,
        }))
        .unwrap();
        enc.flush();
        let mut mp3 = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            mp3.extend_from_slice(&p.data);
        }
        assert!(!mp3.is_empty());
        if let Ok(path) = std::env::var("MP3_ENC_OUT") {
            std::fs::write(path, &mp3).expect("write MP3_ENC_OUT");
        }

        // Decode and split the interleaved stereo output back into L and R.
        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, mp3)).unwrap();
        dec.flush();
        let (mut left, mut right) = (Vec::new(), Vec::new());
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            assert_eq!(af.channels, 2, "decoded stream must be stereo");
            for fr in af.planes[0].chunks_exact(8) {
                left.push(f32::from_le_bytes([fr[0], fr[1], fr[2], fr[3]]));
                right.push(f32::from_le_bytes([fr[4], fr[5], fr[6], fr[7]]));
            }
        }

        let ref_l: Vec<f32> = (0..n)
            .map(|i| 0.35 * (pi2 * 700.0 * i as f32 / sr as f32).sin())
            .collect();
        let ref_r: Vec<f32> = (0..n)
            .map(|i| 0.30 * (pi2 * 3000.0 * i as f32 / sr as f32).sin())
            .collect();
        let snr_l = best_snr(&ref_l, &left);
        let snr_r = best_snr(&ref_r, &right);
        eprintln!("[R1] stereo SNR L {snr_l:.1} dB  R {snr_r:.1} dB");
        assert!(
            snr_l > 20.0 && snr_r > 20.0,
            "stereo channels too noisy: L {snr_l} R {snr_r}"
        );
    }

    /// **R5 — MPEG-2 / 2.5 (LSF).** Each low sample rate encodes as MPEG-2 (≥16 kHz)
    /// or MPEG-2.5 (<16 kHz) — 1 granule/frame, V2 bitrate + side-info — and
    /// round-trips through our decoder. FFmpeg validates the band tables out of band
    /// (`MP3_ENC_DIR`); all six rates decode to >69 dB there.
    #[test]
    fn encode_decode_mpeg2() {
        let dump_dir = std::env::var("MP3_ENC_DIR").ok();
        for &sr in &[22050u32, 24000, 16000, 12000, 11025, 8000] {
            let n = 24 * 576; // MPEG-2: 576 samples/frame
            let input: Vec<f32> = (0..n)
                .map(|i| 0.4 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr as f32).sin())
                .collect();

            let mp3 = encode_mono(&input, sr);
            assert!(!mp3.is_empty());
            if let Some(dir) = &dump_dir {
                std::fs::write(format!("{dir}/v2_{sr}.mp3"), &mp3).expect("dump");
            }
            let h = header::FrameHeader::parse([mp3[0], mp3[1], mp3[2], mp3[3]]).unwrap();
            assert_ne!(
                h.version,
                header::MpegVersion::V1,
                "{sr}: must be MPEG-2/2.5"
            );
            assert_eq!(h.sample_rate, sr);

            let out = decode_mono(mp3);
            assert!(out.len() > n / 2);
            let snr = best_snr(&input, &out);
            eprintln!("[R5] MPEG-2 {sr} Hz round-trip SNR {snr:.1} dB");
            assert!(
                snr > 30.0,
                "{sr}: MPEG-2 round-trip SNR too low: {snr:.1} dB"
            );
        }
    }

    /// **Q5 — block switching.** A castanet-like transient (silence then a sharp
    /// burst, repeating) drives the encoder into short blocks; the stream must
    /// carry window-switched frames and still reconstruct.
    #[test]
    fn block_switching_on_transients() {
        let sr = 44100u32;
        let frames = 20;
        let n = frames * 1152;
        let mut s = 0xBEEF_1234u32;
        // Repeating impulse-train: a sharp burst every ~1500 samples on silence.
        let input: Vec<f32> = (0..n)
            .map(|i| {
                if i % 1500 < 80 {
                    s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    0.7 * ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
                } else {
                    0.0
                }
            })
            .collect();

        let mp3 = encode_mono(&input, sr);
        assert!(!mp3.is_empty());
        if let Ok(path) = std::env::var("MP3_ENC_OUT") {
            std::fs::write(path, &mp3).expect("write MP3_ENC_OUT");
        }

        // Count window-switched frames: in the side info, the first granule's
        // `window_switching` bit. For MPEG-1 mono it's bit 8 of the per-granule
        // fields — simplest to detect by decoding and checking the stream carries
        // short blocks via the side-info parser.
        let si_len = header::FrameHeader::parse([mp3[0], mp3[1], mp3[2], mp3[3]])
            .map(|h| h.side_info_len())
            .unwrap_or(17);
        let mut switched = 0;
        let mut pos = 0;
        while pos + 4 <= mp3.len() {
            if mp3[pos] == 0xFF && mp3[pos + 1] & 0xE0 == 0xE0 {
                if let Ok(h) =
                    header::FrameHeader::parse([mp3[pos], mp3[pos + 1], mp3[pos + 2], mp3[pos + 3]])
                {
                    let si = &mp3[pos + 4..pos + 4 + si_len];
                    if let Ok(parsed) = decode::sideinfo::parse(&h, si) {
                        if (0..h.version.granules())
                            .any(|gr| parsed.granules[gr][0].window_switching)
                        {
                            switched += 1;
                        }
                    }
                    pos += h.frame_size();
                    continue;
                }
            }
            pos += 1;
        }
        eprintln!("[Q5] window-switched frames: {switched}/{frames}");
        assert!(
            switched >= 3,
            "transients must trigger block switching, got {switched}"
        );

        // And the switched stream still decodes frame-for-frame.
        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, mp3)).unwrap();
        dec.flush();
        let mut decoded = 0;
        while let Ok(Frame::Audio(_)) = dec.receive_frame() {
            decoded += 1;
        }
        assert!(decoded >= frames, "all frames must decode, got {decoded}");
    }

    /// **R4 — conformance corpus.** A broad set of signals each round-trips through
    /// encode → our decoder above a per-signal floor. With `MP3_ENC_DIR` set, the
    /// `.mp3`s are dumped for the out-of-band FFmpeg/LAME cross-check.
    #[test]
    fn conformance_corpus_round_trips() {
        let sr = 44100u32;
        let frames = 14;
        let n = frames * 1152;
        let pi2 = std::f32::consts::TAU;
        let tone = |f: f32| -> Vec<f32> {
            (0..n)
                .map(|i| 0.4 * (pi2 * f * i as f32 / sr as f32).sin())
                .collect()
        };
        let noise = |seed: u32, amp: f32| -> Vec<f32> {
            let mut s = seed;
            (0..n)
                .map(|_| {
                    s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    amp * ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
                })
                .collect()
        };
        let sweep: Vec<f32> = {
            let mut ph = 0f32;
            (0..n)
                .map(|i| {
                    let f = 80.0 + (16000.0 - 80.0) * (i as f32 / n as f32);
                    ph += pi2 * f / sr as f32;
                    0.4 * ph.sin()
                })
                .collect()
        };
        // (name, mono signal, SNR floor dB) — tones reconstruct cleanly, noise
        // is lossy at 128k so the floor is lower.
        let corpus: Vec<(&str, Vec<f32>, f64)> = vec![
            ("tone-250", tone(250.0), 40.0),
            ("tone-1k", tone(1000.0), 50.0),
            ("tone-4k", tone(4000.0), 45.0),
            ("tone-12k", tone(12000.0), 30.0),
            ("sweep", sweep, 15.0),
            ("noise", noise(0xC0FFEE, 0.4), 5.0),
            ("quiet-noise", noise(0x1234, 0.02), 3.0),
            (
                "two-tone",
                {
                    let a = tone(600.0);
                    let b = tone(5200.0);
                    a.iter().zip(&b).map(|(x, y)| 0.5 * (x + y)).collect()
                },
                25.0,
            ),
        ];

        let dump_dir = std::env::var("MP3_ENC_DIR").ok();
        for (name, sig, floor) in &corpus {
            let mp3 = encode_mono(sig, sr);
            assert!(!mp3.is_empty(), "{name}: no output");
            if let Some(dir) = &dump_dir {
                std::fs::write(format!("{dir}/{name}.mp3"), &mp3).expect("dump");
            }
            let out = decode_mono(mp3);
            assert!(out.len() > n / 2, "{name}: too few samples");
            let snr = best_snr(sig, &out);
            eprintln!("[R4] {name:<12} SNR {snr:.1} dB (floor {floor})");
            assert!(snr >= *floor, "{name}: SNR {snr:.1} below floor {floor}");
        }
    }

    /// **R2 — VBR.** Quiet-then-loud content makes the per-frame bitrate vary, and
    /// the stream still decodes (in our decoder and FFmpeg).
    #[test]
    fn vbr_varies_bitrate_and_round_trips() {
        let sr = 44100u32;
        let pi2 = 2.0 * std::f32::consts::PI;
        // 10 quiet simple frames, then 10 loud broadband-noise frames.
        let mut input = Vec::new();
        let mut s = 0x1234_5678u32;
        for f in 0..20 {
            for i in 0..1152 {
                let t = (f * 1152 + i) as f32 / sr as f32;
                if f < 10 {
                    input.push(0.05 * (pi2 * 500.0 * t).sin()); // quiet tone → few bits
                } else {
                    s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    input.push(0.6 * ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5));
                    // loud noise
                }
            }
        }

        let mut enc = Mp3Encoder::default();
        let mut opts = Dictionary::new();
        opts.set("q", "3"); // request VBR
        enc.configure(&opts).unwrap();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![pcm_to_bytes(&input)],
            samples: input.len(),
            pts: None,
        }))
        .unwrap();
        enc.flush();
        let mut mp3 = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            mp3.extend_from_slice(&p.data);
        }
        if let Ok(path) = std::env::var("MP3_ENC_OUT") {
            std::fs::write(path, &mp3).expect("write MP3_ENC_OUT");
        }

        // Walk the frames and collect their coded bitrates — VBR must use ≥2.
        let mut bitrates = std::collections::BTreeSet::new();
        let mut pos = 0;
        while pos + 4 <= mp3.len() {
            if mp3[pos] == 0xFF && mp3[pos + 1] & 0xE0 == 0xE0 {
                if let Ok(h) =
                    header::FrameHeader::parse([mp3[pos], mp3[pos + 1], mp3[pos + 2], mp3[pos + 3]])
                {
                    bitrates.insert(h.bitrate_kbps);
                    pos += h.frame_size();
                    continue;
                }
            }
            pos += 1;
        }
        eprintln!("[R2] VBR bitrates used: {bitrates:?}");
        assert!(
            bitrates.len() >= 2,
            "VBR must vary the bitrate, got {bitrates:?}"
        );

        // And it still decodes frame-for-frame.
        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, mp3)).unwrap();
        dec.flush();
        let mut frames = 0;
        while let Ok(Frame::Audio(_)) = dec.receive_frame() {
            frames += 1;
        }
        assert!(frames >= 20, "expected all frames to decode, got {frames}");
    }

    /// **R1+ — mid/side joint stereo.** Correlated channels (L ≈ R, slightly
    /// panned) trigger M/S; the stream must still reconstruct L and R.
    #[test]
    fn encode_decode_joint_stereo() {
        let sr = 44100u32;
        let n = 16 * 1152;
        let pi2 = 2.0 * std::f32::consts::PI;
        // Near-mono content with a small inter-channel difference → M/S wins.
        let mut interleaved = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let base = 0.4 * (pi2 * 900.0 * t).sin();
            interleaved.push(base); // L
            interleaved.push(base * 0.95 + 0.02 * (pi2 * 1500.0 * t).sin()); // R ≈ L
        }

        let mut enc = Mp3Encoder::default();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![pcm_to_bytes(&interleaved)],
            samples: n,
            pts: None,
        }))
        .unwrap();
        enc.flush();
        let mut mp3 = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            mp3.extend_from_slice(&p.data);
        }
        if let Ok(path) = std::env::var("MP3_ENC_OUT") {
            std::fs::write(path, &mp3).expect("write MP3_ENC_OUT");
        }

        // At least one frame must have chosen joint stereo (header mode 0b01).
        let joint = mp3
            .windows(2)
            .step_by(1)
            .any(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0 && (w[1] & 0x06) == 0x02);
        assert!(joint, "no joint-stereo frame emitted");

        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, mp3)).unwrap();
        dec.flush();
        let (mut left, mut right) = (Vec::new(), Vec::new());
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            for fr in af.planes[0].chunks_exact(8) {
                left.push(f32::from_le_bytes([fr[0], fr[1], fr[2], fr[3]]));
                right.push(f32::from_le_bytes([fr[4], fr[5], fr[6], fr[7]]));
            }
        }
        let ref_l: Vec<f32> = (0..n)
            .map(|i| 0.4 * (pi2 * 900.0 * i as f32 / sr as f32).sin())
            .collect();
        let ref_r: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.4 * (pi2 * 900.0 * t).sin() * 0.95 + 0.02 * (pi2 * 1500.0 * t).sin()
            })
            .collect();
        let snr_l = best_snr(&ref_l, &left);
        let snr_r = best_snr(&ref_r, &right);
        eprintln!("[R1+] joint-stereo SNR L {snr_l:.1} dB  R {snr_r:.1} dB");
        assert!(
            snr_l > 20.0 && snr_r > 20.0,
            "joint stereo too noisy: L {snr_l} R {snr_r}"
        );
    }

    /// **C4 — the pipeline gate.** A multi-tone signal (which exercises Q6's
    /// non-flat scalefactors) round-trips PCM → encoder → decoder → PCM well above
    /// the noise floor, and the `.mp3` decodes in FFmpeg (checked out-of-band; see
    /// docs/mp3-encoder-plan.md).
    #[test]
    fn encode_decode_pipeline_multitone() {
        let sr = 44100u32;
        let n = 16 * 1152;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let pi2 = 2.0 * std::f32::consts::PI;
                0.3 * (pi2 * 600.0 * t).sin()
                    + 0.2 * (pi2 * 2300.0 * t).sin()
                    + 0.12 * (pi2 * 9000.0 * t).sin()
            })
            .collect();

        let mp3 = encode_mono(&input, sr);
        assert!(!mp3.is_empty(), "encoder produced no data");
        assert_eq!(mp3[0], 0xFF);
        assert_eq!(mp3[1] & 0xE0, 0xE0);
        if let Ok(path) = std::env::var("MP3_ENC_OUT") {
            std::fs::write(path, &mp3).expect("write MP3_ENC_OUT");
        }

        let out = decode_mono(mp3);
        assert!(out.len() > n / 2, "decoder produced too few samples");

        let snr = best_snr(&input, &out);
        eprintln!("[C4] encode→decode multitone SNR {snr:.1} dB");
        assert!(snr > 20.0, "round-trip SNR too low: {snr:.1} dB");
    }

    /// Decode a real MP3 file (path in `MP3_REF`) and report structure. Validates
    /// frame-sync (skips ID3), header/side-info parsing, and main-data extraction
    /// on real data. Output is silent until D[]/codebooks are laid; this checks
    /// the *structure* (frame count, sample count, no panics, finite samples).
    #[test]
    fn decode_real_mp3_structure() {
        let Ok(path) = std::env::var("MP3_REF") else {
            return; // self-skip when not running the reference harness
        };
        let data = std::fs::read(&path).expect("read MP3_REF");
        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, data)).unwrap();
        dec.flush();

        let mut frames = 0usize;
        let mut samples = 0usize;
        let mut pcm: Vec<u8> = Vec::new();
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            assert_eq!(af.sample_rate, 44100);
            assert_eq!(af.channels, 1);
            assert_eq!(af.planes[0].len(), af.samples * af.channels as usize * 4);
            pcm.extend_from_slice(&af.planes[0]);
            frames += 1;
            samples += af.samples;
        }
        eprintln!("[MP3] decoded frames={frames} samples={samples}");
        assert!(frames > 0, "must decode at least one frame from real data");
        if let Ok(out) = std::env::var("MP3_OUT") {
            std::fs::write(out, &pcm).expect("write MP3_OUT");
        }
    }

    #[test]
    fn registers_as_audio_codec() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        let codec = reg.by_name("mp3").expect("mp3 registered");
        assert_eq!(codec.id, CodecId::Mp3);
        assert_eq!(codec.media_type, MediaType::Audio);
        assert!(codec.can_decode() && codec.can_encode());
    }

    #[test]
    fn header_parse_roundtrip() {
        // MPEG-1 Layer III, 128 kbps, 44.1 kHz, stereo, no CRC, no padding.
        let bytes = [0xFF, 0xFB, 0x90, 0x00];
        let h = header::FrameHeader::parse(bytes).unwrap();
        assert_eq!(h.version, header::MpegVersion::V1);
        assert_eq!(h.bitrate_kbps, 128);
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(h.channel_mode, frame::ChannelMode::Stereo);
        assert!(!h.crc_protected && !h.padding);
        assert_eq!(h.frame_size(), 417);
        assert_eq!(h.to_bytes(), bytes, "header must round-trip bit-exactly");
    }

    #[test]
    fn header_rejects_non_layer3_and_bad_sync() {
        // Layer II (0b10 in the layer field): byte1 = 1111_1101.
        assert!(header::FrameHeader::parse([0xFF, 0xFD, 0x90, 0x00]).is_err());
        // Broken sync.
        assert!(header::FrameHeader::parse([0x00, 0x00, 0x00, 0x00]).is_err());
    }

    fn hdr(version: header::MpegVersion, mode: frame::ChannelMode) -> header::FrameHeader {
        header::FrameHeader {
            version,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: mode,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn sideinfo_bit_accounting_all_layouts() {
        use frame::ChannelMode::{Mono, Stereo};
        use header::MpegVersion::{V1, V2};
        // (version, channel mode, expected side-info length in bytes)
        for (v, m, len) in [
            (V1, Stereo, 32),
            (V1, Mono, 17),
            (V2, Stereo, 17),
            (V2, Mono, 9),
        ] {
            let h = hdr(v, m);
            assert_eq!(h.side_info_len(), len);
            // An all-zero block parses cleanly; parse()'s debug_assert verifies
            // the field widths sum to exactly len*8 bits for this layout.
            let si = decode::sideinfo::parse(&h, &vec![0u8; len]).unwrap();
            assert_eq!(si.main_data_begin, 0);
        }
    }

    #[test]
    fn frame_size_matches_spec_example() {
        // MPEG-1 L3, 128 kbps, 44100 Hz, no padding → 417 bytes.
        let h = header::FrameHeader {
            version: header::MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: frame::ChannelMode::Stereo,
            copyright: false,
            original: true,
            emphasis: 0,
        };
        assert_eq!(h.frame_size(), 417);
        assert_eq!(h.side_info_len(), 32);
    }
}
