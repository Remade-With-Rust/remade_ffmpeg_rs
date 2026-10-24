//! FLAC codec adapter over our in-house [`rusty_flac`] (pure Rust, no FFI):
//! lossless encoder + decoder, both ours end to end.
//!
//! The decoder takes two packet shapes:
//!
//! - **One FLAC frame per packet** (what the FLAC and Matroska demuxers emit):
//!   [`configure`](rff_codec::Decoder::configure) must supply the stream's
//!   STREAMINFO via `extradata` (`fLaC` + metadata blocks, or the bare
//!   34-byte block). Each packet decodes to one [`AudioFrame`].
//! - **A whole `.flac` stream** (starting `fLaC`, as the encoder emits it),
//!   which decodes to one [`AudioFrame`] and needs no configuration.
//!
//! Either way samples come out interleaved: native s16 for <=16-bit streams,
//! f32 normalized from the native depth for wider ones.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{AudioFrame, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat};

/// Register the FLAC codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Flac,
        name: "flac",
        long_name: "FLAC (Free Lossless Audio Codec)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(FlacDecoder::default())),
        encoder: Some(|| Box::new(FlacEncoder::new())),
    });
}

// ---------------------------------------------------------------------------
// Decoder adapter
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FlacDecoder {
    frames: VecDeque<Frame>,
    eof: bool,
    /// Frame decoder for the stream `configure` described (its STREAMINFO
    /// from `extradata`); it keeps its channel scratch across packets.
    core: Option<rusty_flac::FrameDecoder>,
    /// Interleaved PCM scratch, reused between packets.
    s16: Vec<i16>,
    f32: Vec<f32>,
}

/// Find STREAMINFO in codec extradata: `fLaC` + metadata blocks (what the
/// FLAC and Matroska demuxers hand over), or a bare 34-byte block.
fn streaminfo_block(extradata: &[u8]) -> Option<&[u8]> {
    if extradata.len() == 34 {
        return Some(extradata);
    }
    let mut rest = extradata.strip_prefix(b"fLaC")?;
    while rest.len() >= 4 {
        let (hdr, body) = rest.split_at(4);
        let len = ((hdr[1] as usize) << 16) | ((hdr[2] as usize) << 8) | hdr[3] as usize;
        if hdr[0] & 0x7F == 0 && len >= 34 && body.len() >= 34 {
            return Some(&body[..34]);
        }
        if hdr[0] & 0x80 != 0 || body.len() < len {
            return None;
        }
        rest = &body[len..];
    }
    None
}

/// Interleaved PCM in the frame format the adapter emits: native s16 for
/// <=16-bit streams (sub-16-bit up-scaled to the s16 grid), f32 normalised
/// from the native depth for wider ones (exact: 24-bit fits the mantissa).
enum Pcm<'a> {
    S16(&'a [i16]),
    F32(&'a [f32]),
}

/// One [`AudioFrame`] from interleaved PCM, its plane written in fixed-size
/// chunks (one 2- or 4-byte store per sample into a pre-sized plane).
fn audio_frame(info: &rusty_flac::StreamInfo, pcm: Pcm<'_>, pts: Option<i64>) -> AudioFrame {
    let channels = info.channels.max(1) as usize;
    let (format, plane, n) = match pcm {
        Pcm::S16(v) => {
            let mut b = vec![0u8; v.len() * 2];
            for (d, s) in b.chunks_exact_mut(2).zip(v) {
                d.copy_from_slice(&s.to_le_bytes());
            }
            (SampleFormat::S16, b, v.len())
        }
        Pcm::F32(v) => {
            let mut b = vec![0u8; v.len() * 4];
            for (d, s) in b.chunks_exact_mut(4).zip(v) {
                d.copy_from_slice(&s.to_le_bytes());
            }
            (SampleFormat::F32, b, v.len())
        }
    };
    AudioFrame {
        sample_rate: info.sample_rate,
        channels: channels as u16,
        format,
        planes: vec![plane],
        samples: n / channels,
        pts,
    }
}

fn map_err(e: rusty_flac::DecodeError) -> Error {
    Error::invalid(format!("flac decode: {e}"))
}

impl FlacDecoder {
    /// A whole `.flac` stream in one packet (the encoder's output).
    fn decode_stream(&mut self, data: &[u8], pts: Option<i64>) -> Result<()> {
        let mut dec = rusty_flac::Decoder::new(data).map_err(map_err)?;
        let info = dec.streaminfo().clone();
        let frame = if info.bits_per_sample <= 16 {
            self.s16.clear();
            while dec
                .next_frame_interleaved(&mut self.s16)
                .map_err(map_err)?
                .is_some()
            {}
            audio_frame(&info, Pcm::S16(&self.s16), pts)
        } else {
            self.f32.clear();
            while dec
                .next_frame_interleaved(&mut self.f32)
                .map_err(map_err)?
                .is_some()
            {}
            audio_frame(&info, Pcm::F32(&self.f32), pts)
        };
        self.frames.push_back(Frame::Audio(frame));
        Ok(())
    }

    /// Frame packet(s), as the FLAC / Matroska demuxers emit them. The first
    /// frame must decode; further frames in the same packet decode too, and
    /// bytes after the last one that don't parse as a frame (e.g. a
    /// trailing tag) are ignored. An empty packet yields nothing.
    fn decode_frames(&mut self, data: &[u8], pts: Option<i64>) -> Result<()> {
        let Some(core) = self.core.as_mut() else {
            return Err(Error::invalid(
                "flac decode: frame packet without STREAMINFO (extradata)",
            ));
        };
        if data.is_empty() {
            return Ok(());
        }
        let info = core.streaminfo().clone();
        fn run<S: rusty_flac::Sample>(
            core: &mut rusty_flac::FrameDecoder,
            data: &[u8],
            out: &mut Vec<S>,
        ) -> Result<()> {
            out.clear();
            let mut at = 0;
            while at < data.len() {
                match core.decode_frame_interleaved(&data[at..], out) {
                    Ok((_, used)) => at += used,
                    Err(e) if at == 0 => return Err(map_err(e)),
                    Err(_) => break,
                }
            }
            Ok(())
        }
        let frame = if info.bits_per_sample <= 16 {
            run(core, data, &mut self.s16)?;
            audio_frame(&info, Pcm::S16(&self.s16), pts)
        } else {
            run(core, data, &mut self.f32)?;
            audio_frame(&info, Pcm::F32(&self.f32), pts)
        };
        self.frames.push_back(Frame::Audio(frame));
        Ok(())
    }
}

impl Decoder for FlacDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        if let Some(si) = streaminfo_block(&params.extradata) {
            // A STREAMINFO with an impossible channel count leaves the
            // decoder unconfigured (frame packets are then refused).
            self.core = rusty_flac::StreamInfo::parse(si)
                .and_then(rusty_flac::FrameDecoder::new)
                .ok();
        }
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.starts_with(b"fLaC") {
            self.decode_stream(&packet.data, packet.pts)
        } else {
            self.decode_frames(&packet.data, packet.pts)
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frames.pop_front() {
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
// Encoder adapter
// ---------------------------------------------------------------------------

struct FlacEncoder {
    enc: Option<rusty_flac::Encoder>,
    /// `-compression_level`, stored until the first frame creates the encoder.
    level: Option<u32>,
    channels: usize,
    /// Interleave / quantize scratch, reused between frames.
    scratch: Vec<i32>,
    out: Option<Vec<u8>>,
    flushed: bool,
}

impl FlacEncoder {
    fn new() -> Self {
        FlacEncoder {
            enc: None,
            level: None,
            channels: 0,
            scratch: Vec::new(),
            out: None,
            flushed: false,
        }
    }
}

/// Round a float sample in [-1, 1) onto the encoder's integer grid.
#[inline]
fn quantize(s: f32, scale: f32) -> i32 {
    (s * scale).round().clamp(-scale, scale - 1.0) as i32
}

impl Encoder for FlacEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(level) = options.get_int("compression_level") {
            self.level = Some(level.clamp(0, 8) as u32);
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("flac encode: expected an audio frame"));
        };
        if self.enc.is_none() {
            // S16 is a native 16-bit grid; float carries ~24 bits of mantissa,
            // so map it to a 24-bit grid (lossless for int-derived floats).
            let bps = match a.format {
                SampleFormat::S16 => 16,
                _ => 24,
            };
            let channels = a.channels.max(1) as u32;
            let mut enc = rusty_flac::Encoder::new(a.sample_rate, channels, bps)
                .map_err(|e| Error::invalid(format!("flac encode: {e}")))?;
            if let Some(level) = self.level {
                enc.set_compression_level(level);
            }
            self.enc = Some(enc);
            self.channels = channels as usize;
        } else if a.channels.max(1) as usize != self.channels {
            return Err(Error::invalid(
                "flac encode: channel count changed mid-stream",
            ));
        }

        let ch = self.channels;
        let n = a.samples;
        let enc = self.enc.as_mut().expect("encoder initialized above");
        let map_err = |e: rusty_flac::EncodeError| Error::invalid(format!("flac encode: {e}"));
        match a.format {
            // The hot layouts stream straight from the packet bytes.
            SampleFormat::S16 => enc
                .push_s16le_bytes(&a.planes[0][..n * ch * 2])
                .map_err(map_err),
            SampleFormat::F32 => enc
                .push_f32le_bytes(&a.planes[0][..n * ch * 4])
                .map_err(map_err),
            SampleFormat::F32Planar => {
                let scale = (1i64 << 23) as f32;
                let scratch = &mut self.scratch;
                scratch.clear();
                scratch.reserve(n * ch);
                for i in 0..n {
                    for c in 0..ch {
                        let d = &a.planes[c];
                        let o = i * 4;
                        let s = f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
                        scratch.push(quantize(s, scale));
                    }
                }
                enc.push_interleaved(scratch).map_err(map_err)
            }
            _ => Err(Error::invalid(
                "flac encode: unsupported sample format (need S16/F32/F32Planar)",
            )),
        }
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(data) = self.out.take() {
            let mut p = Packet::from_data(0, data);
            p.pts = Some(0);
            return Ok(p);
        }
        if self.flushed {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        if let Some(enc) = self.enc.take() {
            self.out = Some(enc.finish());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_non_flac() {
        let mut dec = FlacDecoder::default();
        assert!(dec
            .send_packet(&Packet::from_data(0, b"not flac".to_vec()))
            .is_err());
    }

    /// Adapter end-to-end: S16 frame in → FLAC → decode → same samples out.
    #[test]
    fn adapter_roundtrip_s16() {
        let n = 6000usize;
        let mut interleaved = Vec::with_capacity(n * 2 * 2);
        for i in 0..n {
            let l = ((i as f64 * 0.07).sin() * 15000.0) as i16;
            let r = (i as i16).wrapping_mul(31);
            interleaved.extend_from_slice(&l.to_le_bytes());
            interleaved.extend_from_slice(&r.to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 44100,
            channels: 2,
            format: SampleFormat::S16,
            planes: vec![interleaved.clone()],
            samples: n,
            pts: Some(0),
        });

        let mut enc = FlacEncoder::new();
        enc.send_frame(&frame).unwrap();
        enc.flush();
        let packet = enc.receive_packet().unwrap();

        let (info, chans) = rusty_flac::decode(&packet.data).unwrap();
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.channels, 2);
        for i in 0..n {
            let l = i16::from_le_bytes([interleaved[i * 4], interleaved[i * 4 + 1]]) as i32;
            let r = i16::from_le_bytes([interleaved[i * 4 + 2], interleaved[i * 4 + 3]]) as i32;
            assert_eq!(chans[0][i], l);
            assert_eq!(chans[1][i], r);
        }
    }

    /// Split a `.flac` stream into (`fLaC` + metadata, frames) the way the
    /// demuxer does: a frame ends at a sync code where the CRC-16 residue of
    /// the bytes since the frame start is zero.
    fn split_frames(stream: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut pos = 4;
        loop {
            let last = stream[pos] & 0x80 != 0;
            let len = u32::from_be_bytes([0, stream[pos + 1], stream[pos + 2], stream[pos + 3]]);
            pos += 4 + len as usize;
            if last {
                break;
            }
        }
        let header = stream[..pos].to_vec();
        let crc16 = |data: &[u8]| {
            data.iter().fold(0u16, |mut c, &b| {
                c ^= (b as u16) << 8;
                for _ in 0..8 {
                    c = if c & 0x8000 != 0 {
                        (c << 1) ^ 0x8005
                    } else {
                        c << 1
                    };
                }
                c
            })
        };
        let mut frames = Vec::new();
        let mut start = pos;
        for p in pos + 10..stream.len() {
            if stream[p] == 0xFF
                && stream.get(p + 1).is_some_and(|&b| b & 0xFE == 0xF8)
                && crc16(&stream[start..p]) == 0
            {
                frames.push(stream[start..p].to_vec());
                start = p;
            }
        }
        frames.push(stream[start..].to_vec());
        (header, frames)
    }

    fn params(extradata: Vec<u8>) -> CodecParams {
        CodecParams {
            codec_id: rff_core::CodecId::Flac,
            width: 0,
            height: 0,
            pixel_format: None,
            sample_rate: 0,
            channels: 0,
            sample_format: None,
            extradata,
        }
    }

    fn audio(frame: Frame) -> AudioFrame {
        match frame {
            Frame::Audio(a) => a,
            Frame::Video(_) => panic!("expected audio"),
        }
    }

    /// Per-frame packets decode to exactly the PCM of the whole-stream decode.
    #[test]
    fn frame_packets_decode_identically_to_the_whole_stream() {
        let n = 23_456usize;
        let sig = |i: usize, k: i64| ((i as f64 * 0.031 * k as f64).sin() * 30000.0) as i64;
        for (rate, channels, bps) in [(44_100, 2u32, 16u32), (48_000, 1, 24), (22_050, 3, 8)] {
            let planes: Vec<Vec<i32>> = (0..channels as i64)
                .map(|c| {
                    (0..n)
                        .map(|i| {
                            (sig(i, c + 1) >> (16 - bps.min(16))) as i32 * (1 << (bps.max(16) - 16))
                        })
                        .collect()
                })
                .collect();
            let refs: Vec<&[i32]> = planes.iter().map(|p| p.as_slice()).collect();
            let mut enc = rusty_flac::Encoder::new(rate, channels, bps).unwrap();
            enc.push_planar(&refs).unwrap();
            let stream = enc.finish();

            let mut whole = FlacDecoder::default();
            whole
                .send_packet(&Packet::from_data(0, stream.clone()))
                .unwrap();
            let want = audio(whole.receive_frame().unwrap());

            let (header, frames) = split_frames(&stream);
            assert!(frames.len() > 3);
            for extradata in [header.clone(), header[8..42].to_vec()] {
                let mut dec = FlacDecoder::default();
                dec.configure(&params(extradata)).unwrap();
                let mut got = Vec::new();
                let mut samples = 0;
                for (i, f) in frames.iter().enumerate() {
                    let mut p = Packet::from_data(0, f.clone());
                    p.pts = Some(i as i64);
                    dec.send_packet(&p).unwrap();
                    let a = audio(dec.receive_frame().unwrap());
                    assert_eq!(a.pts, Some(i as i64));
                    assert_eq!(
                        (a.format, a.sample_rate, a.channels),
                        (want.format, want.sample_rate, want.channels)
                    );
                    samples += a.samples;
                    got.extend_from_slice(&a.planes[0]);
                    assert!(matches!(dec.receive_frame(), Err(Error::Again)));
                }
                assert_eq!(samples, want.samples);
                assert_eq!(got, want.planes[0], "{rate} Hz {channels} ch {bps}-bit");
            }
        }
    }

    #[test]
    fn frame_packets_need_streaminfo_and_reject_corruption() {
        let mut enc = rusty_flac::Encoder::new(44_100, 1, 16).unwrap();
        enc.push_planar(&[&(0..9000)
            .map(|i| (i * 37 % 2001) - 1000)
            .collect::<Vec<i32>>()])
            .unwrap();
        let stream = enc.finish();
        let (header, frames) = split_frames(&stream);
        let mut dec = FlacDecoder::default();
        assert!(dec
            .send_packet(&Packet::from_data(0, frames[0].clone()))
            .is_err());
        dec.configure(&params(header)).unwrap();
        let mut bad = frames[0].clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x40;
        assert!(dec.send_packet(&Packet::from_data(0, bad)).is_err());
        assert!(dec
            .send_packet(&Packet::from_data(0, b"junk".to_vec()))
            .is_err());
        // An empty packet yields nothing; a frame with a trailing tag decodes.
        dec.send_packet(&Packet::new(0)).unwrap();
        assert!(matches!(dec.receive_frame(), Err(Error::Again)));
        let mut tagged = frames.last().unwrap().clone();
        tagged.extend_from_slice(b"TAG trailing id3v1-ish bytes");
        dec.send_packet(&Packet::from_data(0, tagged)).unwrap();
        assert!(dec.receive_frame().is_ok());
        // Garbage extradata leaves the decoder unconfigured, not panicking.
        let mut dec = FlacDecoder::default();
        dec.configure(&params(b"fLaC\x01\xff\xff\xff".to_vec()))
            .unwrap();
        assert!(dec
            .send_packet(&Packet::from_data(0, frames[0].clone()))
            .is_err());
    }
}
