//! AAC-LC audio codec, backed by the pure-Rust
//! [`rusty_aac`](https://crates.io/crates/rusty_aac) decoder + encoder.
//!
//! This crate is a thin adapter: it maps the rff [`Decoder`]/[`Encoder`] traits
//! onto `rusty_aac`'s native API (which owns the whole AAC-LC engine — framing,
//! spectral reconstruction, filterbank, psychoacoustic encoder). No C, no FFI;
//! `rusty_aac`'s `simd` feature (on by default here) enables the runtime-detected
//! AVX2 quantize kernels, `--no-default-features` gives a 100%-safe scalar build.
//!
//! The MP4 `esds` extradata (the 2-byte `AudioSpecificConfig`) is available via
//! the re-exported [`rusty_aac::audio_specific_config_bytes`].

#![forbid(unsafe_code)]

/// The README's example, compiled as a doctest so it cannot go stale.
#[doc = include_str!("../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{
    AudioFrame, CodecId, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat,
};

pub use rusty_aac;
// Preserved re-exports for downstream users of the old rff-codec-aac API.
pub use rusty_aac::{
    audio_specific_config_bytes, is_adts, parse_adts, parse_audio_specific_config,
    sample_rate_for_index, sf_index_for_rate, write_adts_header, write_audio_specific_config,
    AdtsHeader, AudioSpecificConfig, BitReader, SAMPLE_RATES,
};

/// Register the AAC decoder + encoder into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Aac,
        name: "aac",
        long_name: "AAC (Advanced Audio Coding, Low Complexity)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(AacDecoder::default())),
        encoder: Some(|| Box::new(AacEncoder::new())),
    });
}

/// Map a `rusty_aac` error onto the equivalent rff [`Error`], preserving the
/// EAGAIN-style `Again`/`Eof` flow-control variants.
fn map_err(e: rusty_aac::Error) -> Error {
    match e {
        rusty_aac::Error::Again => Error::Again,
        rusty_aac::Error::Eof => Error::Eof,
        rusty_aac::Error::Unimplemented(what) => Error::Unimplemented(what),
        rusty_aac::Error::InvalidData(msg) => Error::InvalidData(msg),
        rusty_aac::Error::Unsupported(msg) => Error::Unsupported(msg),
        other => Error::InvalidData(format!("rusty_aac: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AacDecoder {
    inner: rusty_aac::AacDecoder,
    queue: VecDeque<Frame>,
    eof: bool,
}

/// Map decoded PCM to an rff interleaved-`f32` [`AudioFrame`].
fn pcm_to_frame(d: &rusty_aac::DecodedAudio) -> Frame {
    let samples = d.frames();
    let mut bytes = Vec::with_capacity(d.samples.len() * 4);
    for s in &d.samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    Frame::Audio(AudioFrame {
        sample_rate: d.sample_rate,
        channels: d.channels,
        format: SampleFormat::F32,
        planes: vec![bytes],
        samples,
        pts: d.pts,
    })
}

impl Decoder for AacDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        // Prefer the out-of-band AudioSpecificConfig (MP4 esds); otherwise fall
        // back to the stream's declared rate/channels (e.g. ADTS streams).
        if !params.extradata.is_empty() {
            // Use the RAW config bytes, not the parsed struct: HE-AAC signalling
            // (SBR / Parametric Stereo) lives in fields `AudioSpecificConfig`
            // does not carry. Through the parsed path an HE-AAC stream reports
            // its *core* rate — half the real one — and the pipeline resamples
            // or plays it at half speed.
            self.inner = rusty_aac::AacDecoder::with_config_bytes(&params.extradata)
                .or_else(|_| {
                    parse_audio_specific_config(&params.extradata)
                        .map(rusty_aac::AacDecoder::with_config)
                })
                .map_err(map_err)?;
        } else if params.sample_rate > 0 {
            self.inner = rusty_aac::AacDecoder::with_config(AudioSpecificConfig {
                object_type: 2,
                sample_rate: params.sample_rate,
                channels: params.channels,
            });
        }
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        match self.inner.decode(&packet.data, packet.pts) {
            Ok(pcm) => {
                self.queue.push_back(pcm_to_frame(&pcm));
                Ok(())
            }
            // An empty packet decodes to nothing — not an error at this layer.
            Err(rusty_aac::Error::Again) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
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

struct AacEncoder {
    config: rusty_aac::AacEncoderConfig,
    inner: Option<rusty_aac::AacEncoder>,
}

impl AacEncoder {
    fn new() -> Self {
        Self {
            config: rusty_aac::AacEncoderConfig::default(),
            inner: None,
        }
    }

    fn inner(&mut self) -> &mut rusty_aac::AacEncoder {
        self.inner
            .get_or_insert_with(|| rusty_aac::AacEncoder::new(self.config))
    }
}

impl Encoder for AacEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(b) = options.get_int("b") {
            if b > 0 {
                // Saturate rather than truncate a value beyond u32.
                self.config.bitrate_bps = u32::try_from(b).unwrap_or(u32::MAX);
            }
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("aac encode: expected an audio frame"));
        };
        let ch = a.channels.max(1) as usize;
        let (sr, channels) = (a.sample_rate, a.channels.max(1));
        // `a.samples` is a CLAIM made by whoever built the frame; the planes are
        // the authority. Only the whole sample frames every plane actually holds
        // are encoded. (Trusting the claim let one hostile frame force an
        // arbitrarily large allocation, index past a short plane, or - with no
        // plane at all - panic on `planes[0]`.)
        let enc = match a.format {
            SampleFormat::S16 | SampleFormat::F32 => {
                let Some(d) = a.planes.first() else {
                    return Err(Error::invalid(
                        "aac encode: audio frame has no sample plane",
                    ));
                };
                let bps = if a.format == SampleFormat::S16 { 2 } else { 4 };
                let frames = a.samples.min(d.len() / (bps * ch));
                let d = &d[..frames * ch * bps];
                // The same sample math the encoder used when it ingested rff
                // frames directly, so output stays byte-identical.
                let pcm: Vec<f32> = if bps == 2 {
                    d.chunks_exact(2)
                        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                        .collect()
                } else {
                    d.chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect()
                };
                self.inner().push_pcm(&pcm, channels, sr)
            }
            SampleFormat::F32Planar => {
                if a.planes.len() < ch {
                    return Err(Error::invalid(
                        "aac encode: planar frame has fewer planes than channels",
                    ));
                }
                let frames = a.planes[..ch]
                    .iter()
                    .map(|p| p.len() / 4)
                    .min()
                    .unwrap_or(0)
                    .min(a.samples);
                let planes: Vec<Vec<f32>> = a.planes[..ch]
                    .iter()
                    .map(|plane| {
                        plane[..frames * 4]
                            .chunks_exact(4)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .collect()
                    })
                    .collect();
                let refs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
                self.inner().push_pcm_planar(&refs, sr)
            }
            _ => return Err(Error::invalid("aac encode: unsupported sample format")),
        };
        enc.map_err(map_err)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(Error::Again); // nothing sent yet
        };
        let ep = inner.next_packet().map_err(map_err)?;
        let mut p = Packet::from_data(0, ep.data);
        p.pts = Some(ep.pts);
        Ok(p)
    }

    fn flush(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.finish();
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "test-only signal math and xorshift values: deliberate truncation, sample indices far below 2^52"
)]
mod tests {
    use super::*;

    fn audio(format: SampleFormat, channels: u16, planes: Vec<Vec<u8>>, samples: usize) -> Frame {
        Frame::Audio(AudioFrame {
            sample_rate: 44_100,
            channels,
            format,
            planes,
            samples,
            pts: None,
        })
    }

    fn encoded_bytes(frames: &[Frame]) -> usize {
        let mut enc = AacEncoder::new();
        for f in frames {
            enc.send_frame(f).unwrap();
        }
        enc.flush();
        let mut n = 0;
        while let Ok(p) = enc.receive_packet() {
            n += p.data.len();
        }
        n
    }

    /// A frame's `samples` is a CLAIM: a frame without a plane must be an
    /// error, not a panic, and an inflated count must neither allocate to it
    /// nor read past the plane (both used to happen).
    #[test]
    #[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
    fn hostile_frame_shapes_are_errors_or_bounded() {
        for format in [
            SampleFormat::S16,
            SampleFormat::F32,
            SampleFormat::F32Planar,
        ] {
            let mut enc = AacEncoder::new();
            assert!(enc.send_frame(&audio(format, 2, vec![], 1024)).is_err());
        }
        let tone: Vec<u8> = (0..4096u16)
            .flat_map(|i| ((f32::from(i) * 0.05).sin() * 0.4).to_le_bytes())
            .collect();
        for format in [SampleFormat::F32, SampleFormat::F32Planar] {
            // A claim of 2^40 samples over a 4096-sample plane encodes exactly
            // what the honest claim does.
            assert_eq!(
                encoded_bytes(&[audio(format, 1, vec![tone.clone()], 1 << 40)]),
                encoded_bytes(&[audio(format, 1, vec![tone.clone()], 4096)]),
                "{format:?}"
            );
        }
        // Planar with fewer planes than channels is an error.
        let mut enc = AacEncoder::new();
        assert!(enc
            .send_frame(&audio(SampleFormat::F32Planar, 2, vec![tone], 4096))
            .is_err());
    }

    /// Property: random frame shapes -- every sample format, 0..=8 channels,
    /// planes shorter or longer than claimed, missing planes -- never panic,
    /// whatever `send_frame` returns.
    #[test]
    #[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
    fn random_frame_shapes_never_panic() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let formats = [
            SampleFormat::S16,
            SampleFormat::F32,
            SampleFormat::F32Planar,
        ];
        let rates = [44_100u32, 22_050, 8_000, 48_000, 0, 7, 96_000];
        for _ in 0..200 {
            let mut enc = AacEncoder::new();
            let channels = (rnd() % 9) as u16;
            let nplanes = (rnd() % 4) as usize;
            let planes: Vec<Vec<u8>> = (0..nplanes)
                .map(|_| (0..(rnd() % 12_000)).map(|_| rnd() as u8).collect())
                .collect();
            let frame = Frame::Audio(AudioFrame {
                sample_rate: rates[(rnd() % 7) as usize],
                channels,
                format: formats[(rnd() % 3) as usize],
                planes,
                samples: if rnd() % 3 == 0 {
                    (rnd() % (1 << 40)) as usize
                } else {
                    (rnd() % 6000) as usize
                },
                pts: None,
            });
            let _ = enc.send_frame(&frame);
            enc.flush();
            while enc.receive_packet().is_ok() {}
        }
    }

    /// Option values beyond `u32` saturate instead of truncating.
    #[test]
    fn bitrate_option_saturates() {
        let mut enc = AacEncoder::new();
        let mut opts = Dictionary::new();
        opts.set("b", "99999999999");
        enc.configure(&opts).unwrap();
        assert_eq!(enc.config.bitrate_bps, u32::MAX);
    }

    /// Differential: the adapter's decoded plane is exactly `rusty_aac`'s PCM as
    /// little-endian f32 bytes -- on a real ADTS stream and on mutated copies.
    #[test]
    #[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
    fn adapter_decode_matches_rusty_aac() {
        let pcm: Vec<f32> = (0..44_100u16)
            .map(|i| (f32::from(i) * 0.07).sin() * 0.3)
            .collect();
        let mut enc = rusty_aac::AacEncoder::new(rusty_aac::AacEncoderConfig::default());
        enc.push_pcm(&pcm, 1, 44_100).unwrap();
        enc.finish();
        let mut base = Vec::new();
        while let Ok(p) = enc.next_packet() {
            let hdr = AdtsHeader {
                object_type: 2,
                sample_rate: 44_100,
                channels: 1,
                frame_length: p.data.len() + 7,
                header_len: 7,
            };
            base.push([write_adts_header(&hdr), p.data].concat());
        }
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for case in 0..20 {
            let mut packets = base.clone();
            for _ in 0..case {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let p = (seed % packets.len() as u64) as usize;
                let i = 7 + (seed >> 8) as usize % (packets[p].len() - 7);
                packets[p][i] ^= 1 << (seed % 8);
            }
            let mut ours = Vec::new();
            let mut dec = AacDecoder::default();
            let mut reference = rusty_aac::AacDecoder::new();
            let mut want = Vec::new();
            for p in &packets {
                if dec.send_packet(&Packet::from_data(0, p.clone())).is_ok() {
                    if let Ok(Frame::Audio(a)) = dec.receive_frame() {
                        ours.extend_from_slice(&a.planes[0]);
                    }
                }
                if let Ok(a) = reference.decode(p, None) {
                    for v in a.samples {
                        want.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }
            assert_eq!(ours, want, "case {case}");
        }
    }

    #[test]
    fn registers_aac_codec() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        assert!(reg.find_decoder(CodecId::Aac).is_ok());
        assert!(reg.find_encoder(CodecId::Aac).is_ok());
    }

    /// Thin end-to-end pass through the rff trait path: encode a tone via the
    /// `Encoder` trait, decode the packets via the `Decoder` trait, and confirm
    /// audible, sane PCM comes back (the deep gates live in `rusty_aac`).
    #[test]
    fn trait_path_encode_decode_roundtrip() {
        let sr = 44100u32;
        let n = 8192usize;
        let mut interleaved = Vec::with_capacity(n * 4);
        for i in 0..n {
            let s = ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / f64::from(sr)).sin() * 0.5)
                as f32;
            interleaved.extend_from_slice(&s.to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });

        let mut enc = AacEncoder::new();
        let mut opts = Dictionary::new();
        opts.set("b", "96000");
        enc.configure(&opts).unwrap();
        enc.send_frame(&frame).unwrap();
        assert!(matches!(enc.receive_packet(), Err(Error::Again)));
        enc.flush();

        let mut dec = AacDecoder::default();
        dec.configure(&CodecParams {
            sample_rate: sr,
            channels: 1,
            ..CodecParams::default()
        })
        .unwrap();

        let mut decoded = 0usize;
        let mut energy = 0f64;
        loop {
            match enc.receive_packet() {
                Ok(p) => {
                    assert!(!p.data.is_empty());
                    dec.send_packet(&p).unwrap();
                    let Frame::Audio(a) = dec.receive_frame().unwrap() else {
                        panic!("expected audio");
                    };
                    assert_eq!(a.sample_rate, sr);
                    assert_eq!(a.channels, 1);
                    assert_eq!(a.format, SampleFormat::F32);
                    decoded += a.samples;
                    for c in a.planes[0].chunks_exact(4) {
                        energy += f64::from(f32::from_le_bytes([c[0], c[1], c[2], c[3]])).powi(2);
                    }
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("unexpected encoder error: {e}"),
            }
        }
        assert!(decoded >= n, "decoded fewer samples than encoded");
        assert!(energy > 1.0, "decoded audio is silent");
    }
}
