//! Vorbis audio **decoder**, backed by the pure-Rust [`lewton`] (MIT/Apache-2.0,
//! no FFI).
//!
//! Vorbis carries three setup headers (identification, comment, setup) before
//! any audio. The Ogg demuxer hands those to us packed (length-prefixed) as the
//! stream's `extradata`; [`configure`](rff_codec::Decoder::configure) parses the
//! identification + setup headers, and each audio packet then decodes to an
//! interleaved `s16` [`AudioFrame`].
//!
//! Decode is backed by lewton. Encode is the in-house pure-Rust Vorbis **encoder**
//! ([`rusty_vorbis`], see `docs/codec-vorbis-encoder.md`) — masking-driven floor,
//! rate-distortion residue, stereo coupling, `-q` control; this crate is a thin
//! adapter mapping the rff [`Encoder`] trait onto `rusty_vorbis`'s native API.
//! The encoder emits its three setup headers as its first packets (also via
//! [`VorbisEncoder::headers`]); the Ogg muxer pages them ahead of the audio packets.

use std::collections::VecDeque;

use lewton::audio::{read_audio_packet, PreviousWindowRight};
use lewton::header::{read_header_ident, read_header_setup, IdentHeader, SetupHeader};
use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{AudioFrame, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat};

pub use rusty_vorbis;

/// Register the Vorbis codec (decode + encode) into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Vorbis,
        name: "vorbis",
        long_name: "Vorbis (Ogg Vorbis)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(VorbisDecoder::default())),
        encoder: Some(|| Box::new(VorbisEncoder::new())),
    });
}

/// Map a `rusty_vorbis` error onto the equivalent rff [`Error`], preserving the
/// EAGAIN-style `Again`/`Eof` flow-control variants.
fn map_err(e: rusty_vorbis::Error) -> Error {
    match e {
        rusty_vorbis::Error::Again => Error::Again,
        rusty_vorbis::Error::Eof => Error::Eof,
        rusty_vorbis::Error::Unimplemented(what) => Error::Unimplemented(what),
        rusty_vorbis::Error::InvalidData(msg) => Error::InvalidData(msg),
        rusty_vorbis::Error::Unsupported(msg) => Error::Unsupported(msg),
        other => Error::InvalidData(format!("rusty_vorbis: {other}")),
    }
}

/// Unpack length-prefixed (`u32 LE` + bytes) header blobs from `extradata`.
fn unpack_headers(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if i + len > data.len() {
            break;
        }
        out.push(data[i..i + len].to_vec());
        i += len;
    }
    out
}

struct State {
    ident: IdentHeader,
    setup: SetupHeader,
    pwr: PreviousWindowRight,
    channels: u16,
    sample_rate: u32,
}

#[derive(Default)]
struct VorbisDecoder {
    state: Option<State>,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for VorbisDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        let headers = unpack_headers(&params.extradata);
        if headers.len() < 3 {
            return Err(Error::invalid(
                "vorbis: expected 3 setup headers in extradata",
            ));
        }
        let ident = read_header_ident(&headers[0])
            .map_err(|e| Error::invalid(format!("vorbis ident header: {e:?}")))?;
        let setup = read_header_setup(
            &headers[2],
            ident.audio_channels,
            (ident.blocksize_0, ident.blocksize_1),
        )
        .map_err(|e| Error::invalid(format!("vorbis setup header: {e:?}")))?;

        self.state = Some(State {
            channels: ident.audio_channels as u16,
            sample_rate: ident.audio_sample_rate,
            ident,
            setup,
            pwr: PreviousWindowRight::new(),
        });
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let st = self
            .state
            .as_mut()
            .ok_or_else(|| Error::invalid("vorbis decode: not configured"))?;

        let pcm: Vec<Vec<i16>> = read_audio_packet(&st.ident, &st.setup, &packet.data, &mut st.pwr)
            .map_err(|e| Error::invalid(format!("vorbis decode: {e:?}")))?;
        if pcm.is_empty() || pcm[0].is_empty() {
            return Ok(()); // some packets carry no output samples
        }

        // lewton returns planar per-channel i16; interleave to s16.
        let channels = pcm.len();
        let samples = pcm[0].len();
        let mut interleaved = Vec::with_capacity(samples * channels * 2);
        for i in 0..samples {
            for ch in &pcm {
                interleaved.extend_from_slice(&ch[i].to_le_bytes());
            }
        }
        self.queue.push_back(Frame::Audio(AudioFrame {
            sample_rate: st.sample_rate,
            channels: st.channels,
            format: SampleFormat::S16,
            planes: vec![interleaved],
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
// Encoder — a thin wrapper over rusty_vorbis's native streaming encoder.
// ---------------------------------------------------------------------------

/// The rff-trait Vorbis encoder, delegating to [`rusty_vorbis::VorbisEncoder`].
/// The header/audio packet stream (three setup headers first, then audio with
/// granule pts) is the inner encoder's, unchanged.
pub struct VorbisEncoder {
    inner: rusty_vorbis::VorbisEncoder,
}

impl VorbisEncoder {
    pub fn new() -> Self {
        VorbisEncoder {
            inner: rusty_vorbis::VorbisEncoder::default(),
        }
    }

    /// The three Vorbis setup headers for the configured stream.
    pub fn headers(&self) -> Vec<Vec<u8>> {
        self.inner.headers()
    }

    /// The three setup headers packed as length-prefixed `extradata` (`u32 LE len + bytes`
    /// each) — the format the Ogg muxer and the Vorbis decoder both use.
    pub fn extradata(&self) -> Vec<u8> {
        self.inner.extradata()
    }
}

impl Default for VorbisEncoder {
    fn default() -> Self {
        VorbisEncoder::new()
    }
}

impl Encoder for VorbisEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(b) = options.get_int("b") {
            if b > 0 {
                self.inner.set_bitrate_bps(b as i32);
            }
        }
        // `-q:a` / `-qscale:a` (Vorbis quality, −1..=10) takes precedence when present.
        if let Some(q) = options.get_int("q").or_else(|| options.get_int("qscale")) {
            self.inner
                .set_quality(rusty_vorbis::quality01_from_vorbis_q(q as f64));
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("vorbis encode: expected an audio frame"));
        };
        let ch = a.channels.max(1) as usize;
        let n = a.samples;
        // Convert to the typed interleaved slice — the same sample math the encoder
        // used when it ingested rff frames directly, so output stays byte-identical.
        let enc = match a.format {
            SampleFormat::S16 => {
                let d = &a.planes[0];
                let mut pcm = Vec::with_capacity(n * ch);
                for i in 0..n * ch {
                    let o = i * 2;
                    pcm.push(i16::from_le_bytes([d[o], d[o + 1]]));
                }
                self.inner.push_pcm_s16(&pcm, a.channels, a.sample_rate)
            }
            SampleFormat::F32 => {
                let d = &a.planes[0];
                let mut pcm = Vec::with_capacity(n * ch);
                for i in 0..n * ch {
                    let o = i * 4;
                    pcm.push(f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]));
                }
                self.inner.push_pcm_f32(&pcm, a.channels, a.sample_rate)
            }
            _ => return Err(Error::invalid("vorbis encode: unsupported sample format")),
        };
        enc.map_err(map_err)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        let ep = self.inner.next_packet().map_err(map_err)?;
        let mut pkt = Packet::from_data(0, ep.data);
        pkt.pts = Some(ep.pts);
        pkt.duration = ep.duration;
        Ok(pkt)
    }

    fn flush(&mut self) {
        self.inner.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_vorbis_codec() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        assert!(reg.find_decoder(rff_core::CodecId::Vorbis).is_ok());
        assert!(reg.find_encoder(rff_core::CodecId::Vorbis).is_ok());
    }

    #[test]
    fn unpack_headers_roundtrips() {
        let mut packed = Vec::new();
        for h in [b"abc".as_slice(), b"", b"defgh"] {
            packed.extend_from_slice(&(h.len() as u32).to_le_bytes());
            packed.extend_from_slice(h);
        }
        let out = unpack_headers(&packed);
        assert_eq!(out, vec![b"abc".to_vec(), Vec::new(), b"defgh".to_vec()]);
    }

    #[test]
    fn configure_rejects_missing_headers() {
        let mut dec = VorbisDecoder::default();
        assert!(dec.configure(&CodecParams::default()).is_err());
    }

    #[test]
    fn decode_before_configure_errors() {
        let mut dec = VorbisDecoder::default();
        assert!(dec.send_packet(&Packet::from_data(0, vec![0; 4])).is_err());
    }
}
