//! Vorbis audio **decoder**, backed by the pure-Rust [`lewton`] (MIT/Apache-2.0,
//! no FFI).
//!
//! Vorbis carries three setup headers (identification, comment, setup) before
//! any audio. The Ogg demuxer hands those to us packed (length-prefixed) as the
//! stream's `extradata`; [`configure`](rff_codec::Decoder::configure) parses the
//! identification + setup headers, and each audio packet then decodes to an
//! interleaved `f32` [`AudioFrame`] — float, like FFmpeg's native Vorbis
//! decoder (`fltp`), not quantised to 16 bits. Channels come out in FFmpeg's
//! (WAV) order, not Vorbis's: see [`ffmpeg_channel_map`].
//!
//! Decode is backed by lewton. Encode is the in-house pure-Rust Vorbis **encoder**
//! ([`rusty_vorbis`], see `docs/codec-vorbis-encoder.md`) — masking-driven floor,
//! rate-distortion residue, stereo coupling, `-q` control; this crate is a thin
//! adapter mapping the rff [`Encoder`] trait onto `rusty_vorbis`'s native API.
//! The encoder emits its three setup headers as its first packets (also via
//! [`VorbisEncoder::headers`]); the Ogg muxer pages them ahead of the audio packets.

use std::collections::VecDeque;

use lewton::audio::{read_audio_packet_generic, PreviousWindowRight};
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

/// Vorbis → FFmpeg channel order: output channel `i` takes Vorbis channel
/// `map[i]`. Vorbis orders 3–8 channels with the centre second and the LFE
/// last (spec §4.3.9); FFmpeg/WAV put the centre third and the LFE fourth.
/// This is libavcodec's `ff_vorbis_channel_layout_offsets`, applied the way
/// `vorbisdec.c` does (`channel_ptrs[offsets[i]] = plane i`). 1, 2 and 4
/// channels are identical in both orders; beyond 8 Vorbis leaves the order
/// application-defined and FFmpeg keeps it as is.
fn ffmpeg_channel_map(channels: usize) -> Vec<usize> {
    let map: &[usize] = match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 5, 3, 4],
        7 => &[0, 2, 1, 6, 5, 3, 4],
        8 => &[0, 2, 1, 7, 5, 6, 3, 4],
        _ => return (0..channels).collect(),
    };
    map.to_vec()
}

/// Interleave lewton's planar output into little-endian f32 bytes, output
/// channel `i` taking plane `map[i]`.
fn interleave(pcm: &[Vec<f32>], map: &[usize], samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * map.len() * 4);
    for i in 0..samples {
        for &c in map {
            out.extend_from_slice(&pcm[c][i].to_le_bytes());
        }
    }
    out
}

struct State {
    ident: IdentHeader,
    setup: SetupHeader,
    pwr: PreviousWindowRight,
    /// Output channel order (see [`ffmpeg_channel_map`]).
    map: Vec<usize>,
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
            map: ffmpeg_channel_map(ident.audio_channels as usize),
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

        let pcm: Vec<Vec<f32>> =
            read_audio_packet_generic(&st.ident, &st.setup, &packet.data, &mut st.pwr)
                .map_err(|e| Error::invalid(format!("vorbis decode: {e:?}")))?;
        if pcm.is_empty() || pcm[0].is_empty() {
            return Ok(()); // some packets carry no output samples
        }

        // lewton returns planar per-channel f32 in Vorbis channel order;
        // interleave in FFmpeg's.
        let samples = pcm[0].len();
        let interleaved = interleave(&pcm, &st.map, samples);
        self.queue.push_back(Frame::Audio(AudioFrame {
            sample_rate: st.sample_rate,
            channels: st.channels,
            format: SampleFormat::F32,
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

    /// Encode `n` frames of a stereo tone with the in-house encoder; returns the
    /// packed extradata and the audio packets.
    fn encoded_tone(n: usize) -> (Vec<u8>, Vec<Packet>) {
        let mut enc = VorbisEncoder::new();
        let mut plane = Vec::with_capacity(n * 2 * 4);
        for i in 0..n {
            for f in [0.02f32, 0.031] {
                plane.extend_from_slice(&(0.4 * (f * i as f32).sin()).to_le_bytes());
            }
        }
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: 44_100,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![plane],
            samples: n,
            pts: Some(0),
        }))
        .unwrap();
        enc.flush();
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
        // The first three packets are the setup headers.
        let audio = packets.split_off(3);
        (enc.extradata(), audio)
    }

    fn decode_all(extradata: &[u8], packets: &[Packet]) -> Vec<AudioFrame> {
        let mut dec = VorbisDecoder::default();
        dec.configure(&CodecParams {
            extradata: extradata.to_vec(),
            ..Default::default()
        })
        .unwrap();
        let mut out = Vec::new();
        for p in packets {
            dec.send_packet(p).unwrap();
            while let Ok(Frame::Audio(af)) = dec.receive_frame() {
                out.push(af);
            }
        }
        dec.flush();
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            out.push(af);
        }
        out
    }

    /// Output is float, as FFmpeg's decoder's is: not quantised to 16 bits.
    #[test]
    fn decodes_to_unquantised_f32() {
        let (extradata, packets) = encoded_tone(8192);
        let frames = decode_all(&extradata, &packets);
        assert!(!frames.is_empty());
        let mut off_grid = 0usize;
        for af in &frames {
            assert_eq!(af.format, SampleFormat::F32);
            assert_eq!(af.planes[0].len(), af.samples * af.channels as usize * 4);
            for b in af.planes[0].chunks_exact(4) {
                let s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                assert!(s.is_finite() && s.abs() <= 1.5, "{s}");
                if (s * 32768.0).fract() != 0.0 {
                    off_grid += 1;
                }
            }
        }
        assert!(off_grid > 1000, "only {off_grid} samples off the s16 grid");
    }

    /// FFmpeg order for every channel count Vorbis defines (checked against
    /// libavcodec's `ff_vorbis_channel_layout_offsets` and an FFmpeg 8.1 decode
    /// of a 5.1 file with a distinct tone per channel).
    #[test]
    fn channel_map_matches_ffmpeg() {
        assert_eq!(ffmpeg_channel_map(1), [0]);
        assert_eq!(ffmpeg_channel_map(2), [0, 1]);
        assert_eq!(ffmpeg_channel_map(3), [0, 2, 1]);
        assert_eq!(ffmpeg_channel_map(4), [0, 1, 2, 3]);
        assert_eq!(ffmpeg_channel_map(5), [0, 2, 1, 3, 4]);
        assert_eq!(ffmpeg_channel_map(6), [0, 2, 1, 5, 3, 4]);
        assert_eq!(ffmpeg_channel_map(7), [0, 2, 1, 6, 5, 3, 4]);
        assert_eq!(ffmpeg_channel_map(8), [0, 2, 1, 7, 5, 6, 3, 4]);
        assert_eq!(ffmpeg_channel_map(9), (0..9).collect::<Vec<_>>());
        for n in 1..=8 {
            let mut m = ffmpeg_channel_map(n);
            m.sort_unstable();
            assert_eq!(m, (0..n).collect::<Vec<_>>(), "{n}ch map is a permutation");
        }
    }

    /// 5.1: Vorbis FL FC FR RL RR LFE → FFmpeg FL FR FC LFE BL BR.
    #[test]
    fn interleave_reorders_5_1_to_ffmpeg_order() {
        // Plane k holds the constant k (FL=0, FC=1, FR=2, RL=3, RR=4, LFE=5).
        let pcm: Vec<Vec<f32>> = (0..6).map(|k| vec![k as f32; 2]).collect();
        let bytes = interleave(&pcm, &ffmpeg_channel_map(6), 2);
        let got: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let frame = [0.0, 2.0, 1.0, 5.0, 3.0, 4.0]; // FL FR FC LFE BL BR
        assert_eq!(got, [frame, frame].concat());
    }

    #[test]
    fn decode_before_configure_errors() {
        let mut dec = VorbisDecoder::default();
        assert!(dec.send_packet(&Packet::from_data(0, vec![0; 4])).is_err());
    }
}
