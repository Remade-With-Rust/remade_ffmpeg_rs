//! Vorbis audio **decoder**, backed by the pure-Rust [`lewton`] (MIT/Apache-2.0,
//! no FFI).
//!
//! Vorbis carries three setup headers (identification, comment, setup) before
//! any audio. The Ogg demuxer hands those to us packed (length-prefixed) as the
//! stream's `extradata`; [`configure`](rff_codec::Decoder::configure) parses the
//! identification + setup headers, and each audio packet then decodes to an
//! interleaved `s16` [`AudioFrame`].
//!
//! Decode only — there is no permissive pure-Rust Vorbis *encoder* (the codec
//! is registered with `encoder: None`); use Opus for encoding.

use std::collections::VecDeque;

use lewton::audio::{read_audio_packet, PreviousWindowRight};
use lewton::header::{read_header_ident, read_header_setup, IdentHeader, SetupHeader};
use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder};
use rff_core::{AudioFrame, Error, Frame, MediaType, Packet, Result, SampleFormat};

/// Register the Vorbis codec (decode only) into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Vorbis,
        name: "vorbis",
        long_name: "Vorbis (Ogg Vorbis)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(VorbisDecoder::default())),
        encoder: None,
    });
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

        let pcm: Vec<Vec<i16>> =
            read_audio_packet(&st.ident, &st.setup, &packet.data, &mut st.pwr)
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

#[cfg(test)]
mod tests {
    use super::*;

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
