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
fn pcm_to_frame(d: rusty_aac::DecodedAudio) -> Frame {
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
            let cfg = parse_audio_specific_config(&params.extradata).map_err(map_err)?;
            self.inner = rusty_aac::AacDecoder::with_config(cfg);
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
                self.queue.push_back(pcm_to_frame(pcm));
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
    fn new() -> AacEncoder {
        AacEncoder {
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
                self.config.bitrate_bps = b as u32;
            }
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("aac encode: expected an audio frame"));
        };
        let ch = a.channels.max(1) as usize;
        let n = a.samples;
        let (sr, channels) = (a.sample_rate, a.channels.max(1));
        // Convert to interleaved f32 — the same sample math the encoder used
        // when it ingested rff frames directly, so output stays byte-identical.
        let enc = match a.format {
            SampleFormat::S16 => {
                let d = &a.planes[0];
                let mut pcm = Vec::with_capacity(n * ch);
                for i in 0..n * ch {
                    let o = i * 2;
                    pcm.push(i16::from_le_bytes([d[o], d[o + 1]]) as f32 / 32768.0);
                }
                self.inner().push_pcm(&pcm, channels, sr)
            }
            SampleFormat::F32 => {
                let d = &a.planes[0];
                let mut pcm = Vec::with_capacity(n * ch);
                for i in 0..n * ch {
                    let o = i * 4;
                    pcm.push(f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]));
                }
                self.inner().push_pcm(&pcm, channels, sr)
            }
            SampleFormat::F32Planar => {
                let planes: Vec<Vec<f32>> = a
                    .planes
                    .iter()
                    .take(ch)
                    .map(|plane| {
                        plane
                            .chunks_exact(4)
                            .take(n)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .collect()
                    })
                    .collect();
                let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
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
mod tests {
    use super::*;

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
            let s =
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / sr as f64).sin() * 0.5) as f32;
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
                        energy +=
                            (f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64).powi(2);
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
