//! FLAC codec adapter over our in-house [`rusty_flac`] (pure Rust, no FFI):
//! lossless encoder + decoder, both ours end to end.
//!
//! FLAC is self-describing (its `STREAMINFO` carries sample rate / channels /
//! bit depth), so — like the image codecs — a packet is the whole `.flac`
//! stream and no [`configure`](rff_codec::Decoder::configure) is needed. The
//! whole stream decodes to one interleaved `f32` [`AudioFrame`] (samples
//! normalized from FLAC's native bit depth).

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{
    AudioFrame, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat,
};

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
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for FlacDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let (info, chans) = rusty_flac::decode(&packet.data)
            .map_err(|e| Error::invalid(format!("flac decode: {e}")))?;
        let channels = info.channels.max(1) as usize;
        let total = chans.first().map_or(0, |c| c.len());

        // ≤16-bit streams interleave straight to native s16 (no float detour);
        // wider depths go to f32 (exact — 24-bit fits the mantissa).
        let frame = if info.bits_per_sample <= 16 {
            let shift = 16 - info.bits_per_sample; // sub-16-bit up-scales to the s16 grid
            let mut bytes: Vec<u8> = Vec::with_capacity(total * channels * 2);
            match channels {
                1 => {
                    for &v in &chans[0] {
                        bytes.extend_from_slice(&(((v << shift) as i16).to_le_bytes()));
                    }
                }
                2 => {
                    let (l, r) = (&chans[0], &chans[1]);
                    for i in 0..total {
                        bytes.extend_from_slice(&(((l[i] << shift) as i16).to_le_bytes()));
                        bytes.extend_from_slice(&(((r[i] << shift) as i16).to_le_bytes()));
                    }
                }
                _ => {
                    for i in 0..total {
                        for chan in &chans {
                            bytes.extend_from_slice(&(((chan[i] << shift) as i16).to_le_bytes()));
                        }
                    }
                }
            }
            AudioFrame {
                sample_rate: info.sample_rate,
                channels: channels as u16,
                format: SampleFormat::S16,
                planes: vec![bytes],
                samples: total,
                pts: packet.pts,
            }
        } else {
            // Normalize native integer samples to f32 in [-1, 1).
            let scale = (1u64 << (info.bits_per_sample - 1)) as f32;
            let inv = 1.0 / scale;
            let mut bytes: Vec<u8> = Vec::with_capacity(total * channels * 4);
            for i in 0..total {
                for chan in &chans {
                    bytes.extend_from_slice(&(chan[i] as f32 * inv).to_le_bytes());
                }
            }
            AudioFrame {
                sample_rate: info.sample_rate,
                channels: channels as u16,
                format: SampleFormat::F32,
                planes: vec![bytes],
                samples: total,
                pts: packet.pts,
            }
        };
        self.frame = Some(Frame::Audio(frame));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frame.take() {
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
        let scratch = &mut self.scratch;
        scratch.clear();
        scratch.reserve(n * ch);
        match a.format {
            SampleFormat::S16 => {
                let d = &a.planes[0];
                for i in 0..n * ch {
                    let o = i * 2;
                    scratch.push(i16::from_le_bytes([d[o], d[o + 1]]) as i32);
                }
            }
            SampleFormat::F32 => {
                let d = &a.planes[0];
                let scale = (1i64 << 23) as f32;
                for i in 0..n * ch {
                    let o = i * 4;
                    let s = f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
                    scratch.push(quantize(s, scale));
                }
            }
            SampleFormat::F32Planar => {
                let scale = (1i64 << 23) as f32;
                for i in 0..n {
                    for c in 0..ch {
                        let d = &a.planes[c];
                        let o = i * 4;
                        let s = f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
                        scratch.push(quantize(s, scale));
                    }
                }
            }
            _ => {
                return Err(Error::invalid(
                    "flac encode: unsupported sample format (need S16/F32/F32Planar)",
                ))
            }
        }
        self.enc
            .as_mut()
            .expect("encoder initialized above")
            .push_interleaved(scratch)
            .map_err(|e| Error::invalid(format!("flac encode: {e}")))
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
}
