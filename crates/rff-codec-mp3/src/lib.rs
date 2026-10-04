//! MP3 (MPEG-1/2/2.5 Audio Layer III) codec, backed by the pure-Rust
//! [`rusty_mp3`] decoder + encoder (formerly in-tree here, now a standalone
//! publishable crate — the same adapter pattern as `rff-codec-h264` over
//! `rusty_h264`).
//!
//! This crate is the thin rff-facing layer: `register()` wires the codec into
//! the registry, the wrapper structs implement the rff [`Decoder`]/[`Encoder`]
//! traits by delegating to [`rusty_mp3::Mp3Decoder`]/[`rusty_mp3::Mp3Encoder`],
//! and everything ffmpeg-CLI-shaped stays here: `Dictionary` option parsing
//! (`-b:a`, `-q:a`), `AudioFrame` ↔ PCM conversion honoring `af.format`, and
//! `rusty_mp3` → rff error mapping.

#![forbid(unsafe_code)]

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{
    AudioFrame, CodecId, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat,
};

/// The underlying pure-Rust MP3 engine, re-exported for direct use.
pub use rusty_mp3;

/// Re-export the Prometheus telemetry hooks.
///
/// This crate FORWARDS the `prometheus-telemetry` feature to `rusty_mp3`
/// (see Cargo.toml) but stopped re-exporting the module when mp3 was
/// extracted into a standalone crate — so `rff_codec_mp3::prometheus_telemetry`
/// vanished while the feature that implies it kept working. Prometheus's
/// harvester imports exactly that path and no longer compiles.
///
/// Forwarding a feature without re-exporting what the feature provides is a
/// silent break: the manifest still says yes and the module is gone.
#[cfg(feature = "prometheus-telemetry")]
pub use rusty_mp3::prometheus_telemetry;

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

/// Map a [`rusty_mp3::Error`] onto the rff error space, variant for variant
/// (the drain-protocol variants `Again`/`Eof` must survive the mapping — the
/// pipeline drives codecs by them).
fn map_err(e: rusty_mp3::Error) -> Error {
    match e {
        rusty_mp3::Error::Unimplemented(what) => Error::Unimplemented(what),
        rusty_mp3::Error::Eof => Error::Eof,
        rusty_mp3::Error::Again => Error::Again,
        rusty_mp3::Error::InvalidData(msg) => Error::InvalidData(msg),
        rusty_mp3::Error::Unsupported(msg) => Error::Unsupported(msg),
        // `rusty_mp3::Error` is #[non_exhaustive]; anything future-added is a
        // decode/encode failure from rff's point of view.
        other => Error::InvalidData(format!("rusty_mp3: {other}")),
    }
}

#[derive(Default)]
struct Mp3Decoder {
    inner: rusty_mp3::Mp3Decoder,
}

impl Decoder for Mp3Decoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.inner.push(&packet.data);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let audio = self.inner.next_frame().map_err(map_err)?;
        let channels = audio.channels.max(1);
        // One sized pass into an exact-length plane. The per-sample
        // `extend_from_slice` this replaces re-checked capacity on every
        // sample, which kept the loop scalar; with the length fixed up front it
        // is a plain lane-wise copy (on little-endian, byte-for-byte a memcpy).
        let mut bytes = vec![0u8; audio.samples.len() * 4];
        for (d, s) in bytes.chunks_exact_mut(4).zip(&audio.samples) {
            d.copy_from_slice(&s.to_le_bytes());
        }
        Ok(Frame::Audio(AudioFrame {
            sample_rate: audio.sample_rate,
            channels,
            format: SampleFormat::F32,
            samples: audio.samples.len() / channels as usize,
            planes: vec![bytes],
            pts: None,
        }))
    }

    fn flush(&mut self) {
        self.inner.flush();
    }
}

/// Parse an FFmpeg-style bitrate string ("128k", "192000") into kbps.
///
/// The option string is user input. Rust's float-to-int `as` SATURATES (NaN and
/// negatives give 0, huge values `u32::MAX`), so no string can produce undefined
/// behaviour, and 0 means "default" downstream; the encoder snaps every other
/// value to the nearest legal bitrate.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

/// MP3 encoder adapter. Options are collected in [`Encoder::configure`]; the
/// inner [`rusty_mp3::Mp3Encoder`] is built lazily on the first audio frame
/// (its header locks on the first PCM push).
#[derive(Default)]
struct Mp3Encoder {
    inner: Option<rusty_mp3::Mp3Encoder>,
    /// CBR target (kbps); 0 ⇒ default 128.
    cbr_kbps: u32,
    /// VBR quality target (peak NMR). `Some` ⇒ VBR, `None` ⇒ CBR.
    quality: Option<f32>,
    eof: bool,
}

impl Mp3Encoder {
    fn inner(&mut self) -> &mut rusty_mp3::Mp3Encoder {
        if self.inner.is_none() {
            self.inner = Some(rusty_mp3::Mp3Encoder::new(rusty_mp3::Mp3EncoderConfig {
                bitrate_kbps: self.cbr_kbps,
                vbr_quality: self.quality,
            }));
        }
        // Cannot fail: the branch above assigned `Some` when it was `None`.
        self.inner.as_mut().unwrap()
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
                self.quality = Some(rusty_mp3::vbr_quality_index(q));
                break;
            }
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(af) = frame else {
            return Ok(());
        };
        let in_ch = af.channels.max(1);
        // The PCM/WAV demuxer path delivers S16 (not F32), so we MUST honor
        // `af.format` here — reading s16 bytes with an f32 stride yields
        // denormal garbage (a silent encode). The unit tests only ever fed F32,
        // which is why this once slipped; `s16_input_encodes_to_audible_output`
        // below guards it.
        let bps = match af.format {
            SampleFormat::F32 => 4,
            SampleFormat::S16 => 2,
            other => {
                return Err(Error::unsupported(format!(
                    "mp3 encode: sample format `{}` (need interleaved s16/f32)",
                    other.name()
                )))
            }
        };
        // The plane goes to the encoder AS BYTES: it converts and deinterleaves
        // in one pass into its own frame buffer. This used to decode the whole
        // plane into a `Vec<f32>` / `Vec<i16>` first (and the s16 entry point
        // then made a second, `f32`, copy) -- on a WAV, which arrives as ONE
        // frame, that was two or three file-sized temporaries.
        //
        // `af.samples` is a CLAIM made by whoever built the frame; the plane is
        // the authority. Only the whole sample frames the plane actually holds
        // are encoded. (A short plane used to be padded with silence up to the
        // claim, so one frame with an inflated `samples` could force an
        // arbitrarily large allocation -- and an empty `planes` panicked.)
        let Some(data) = af.planes.first() else {
            return Err(Error::invalid(
                "mp3 encode: audio frame has no sample plane",
            ));
        };
        let frame_bytes = usize::from(in_ch) * bps;
        let frames = af.samples.min(data.len() / frame_bytes);
        let plane = &data[..frames * frame_bytes];
        let sr = af.sample_rate;
        let inner = self.inner();
        match bps {
            4 => inner.push_pcm_f32le(plane, in_ch, sr),
            _ => inner.push_pcm_s16le(plane, in_ch, sr),
        }
        .map_err(map_err)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        // The inner encoder owns the packet queue (including the Xing/Info
        // frame push-fronted at finish) — pull straight through, mapping the
        // Again/Eof drain protocol variant for variant.
        match self.inner.as_mut() {
            Some(inner) => inner
                .next_packet()
                .map(|bytes| Packet::from_data(0, bytes))
                .map_err(map_err),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::Again),
        }
    }

    fn flush(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.finish();
        }
        self.eof = true;
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)] // test signal synthesis
mod tests {
    use super::*;

    /// Encode interleaved **S16** mono — the sample format the WAV/PCM demuxer
    /// actually delivers (the `rusty_mp3` native tests otherwise feed f32 slices).
    fn encode_mono_s16(input: &[i16], sample_rate: u32) -> Vec<u8> {
        let bytes: Vec<u8> = input.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut enc = Mp3Encoder::default();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate,
            channels: 1,
            format: SampleFormat::S16,
            planes: vec![bytes],
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

    /// REGRESSION (silent-encode bug): `send_frame` must honor `af.format`. The
    /// WAV/PCM demuxer delivers S16; reading those bytes with an F32 (4-byte)
    /// stride over 2-byte samples yields denormal garbage → a full-size but
    /// SILENT MP3 that every decoder reproduces as silence. Feed S16, require
    /// audible output. (Brick tests all fed F32, so this class of bug slipped.)
    #[test]
    #[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
    fn s16_input_encodes_to_audible_output() {
        let sr = 44100u32;
        let n = sr as usize; // 1 s
        let s16: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 32767.0) as i16
            })
            .collect();
        let out = decode_mono(encode_mono_s16(&s16, sr));
        let rms = (out.iter().map(|x| x * x).sum::<f32>() / out.len().max(1) as f32).sqrt();
        assert!(
            rms > 0.1,
            "S16 input produced near-silent output (rms={rms}); encoder ignored af.format"
        );
    }

    /// A frame's `samples` is a claim: an empty `planes` must be an error, not a
    /// panic, and an inflated count must neither allocate to it nor encode more
    /// than the plane holds (it used to pad with silence up to the claim).
    #[test]
    #[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
    fn hostile_frame_shapes_are_errors_or_bounded() {
        let mut enc = Mp3Encoder::default();
        let empty = Frame::Audio(AudioFrame {
            sample_rate: 44_100,
            channels: 2,
            format: SampleFormat::S16,
            planes: vec![],
            samples: 1152,
            pts: None,
        });
        assert!(enc.send_frame(&empty).is_err());

        let claim = |samples: usize| {
            let mut enc = Mp3Encoder::default();
            enc.send_frame(&Frame::Audio(AudioFrame {
                sample_rate: 44_100,
                channels: 1,
                format: SampleFormat::S16,
                planes: vec![vec![0x11; 2 * 4608]],
                samples,
                pts: None,
            }))
            .unwrap();
            enc.flush();
            let mut n = 0;
            while let Ok(p) = enc.receive_packet() {
                n += p.data.len();
            }
            n
        };
        // A claim of 2^40 samples over a 4608-sample plane encodes exactly what
        // the honest claim does.
        assert_eq!(claim(1 << 40), claim(4608));
    }

    /// Property: random frame shapes -- every sample format, 0..=8 channels,
    /// valid and invalid rates, planes shorter or longer than claimed -- never
    /// panic, whatever `send_frame` returns.
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
        for _ in 0..300 {
            let mut enc = Mp3Encoder::default();
            let plane: Vec<u8> = (0..(rnd() % 20_000)).map(|_| rnd() as u8).collect();
            let frame = Frame::Audio(AudioFrame {
                sample_rate: rates[(rnd() % 7) as usize],
                channels: (rnd() % 9) as u16,
                format: formats[(rnd() % 3) as usize],
                planes: if rnd() % 10 == 0 { vec![] } else { vec![plane] },
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

    /// Option strings are user input: known forms parse, and hostile ones
    /// saturate instead of panicking (the documented float-to-int behaviour).
    #[test]
    fn bitrate_options_parse_or_saturate() {
        assert_eq!(parse_bitrate("128k"), Some(128));
        assert_eq!(parse_bitrate("192000"), Some(192));
        assert_eq!(parse_bitrate("0.32M"), Some(320));
        assert_eq!(parse_bitrate("-5k"), Some(0));
        assert_eq!(parse_bitrate("nank"), Some(0));
        assert_eq!(parse_bitrate("1e30k"), Some(u32::MAX));
        for s in ["", "k", "M", "abc", "  12 K ", "99999999999999999999", "∞k"] {
            let _ = parse_bitrate(s);
        }
    }

    /// Differential: the adapter's decoded plane is exactly `rusty_mp3`'s PCM
    /// as little-endian f32 bytes -- on a real stream and on mutated copies.
    #[test]
    #[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
    fn adapter_decode_matches_rusty_mp3() {
        let sr = 44_100u32;
        let s16: Vec<i16> = (0..sr as usize)
            .map(|i| ((i as f32 * 0.07).sin() * 9000.0) as i16)
            .collect();
        let base = encode_mono_s16(&s16, sr);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for case in 0..20 {
            let mut mp3 = base.clone();
            for _ in 0..case {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed % mp3.len() as u64) as usize;
                mp3[i] ^= 1 << (seed % 8);
            }
            let ours = decode_mono(mp3.clone());
            let mut reference = rusty_mp3::Mp3Decoder::new();
            reference.push(&mp3);
            reference.flush();
            let mut want = Vec::new();
            while let Ok(f) = reference.next_frame() {
                want.extend_from_slice(&f.samples);
            }
            assert_eq!(ours.len(), want.len(), "case {case}");
            assert!(
                ours.iter()
                    .zip(&want)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "case {case}"
            );
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
}
