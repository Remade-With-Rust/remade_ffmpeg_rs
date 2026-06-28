//! Linear PCM audio codec — the audio analog of "raw video".
//!
//! PCM packets carry uncompressed interleaved samples, so they are *not*
//! self-describing: the decoder learns the sample rate, channel count, and
//! sample layout from [`CodecParams`] via [`configure`](rff_codec::Decoder::configure)
//! (the new codec-parameters plumbing). Supports interleaved `s16` and `f32`.

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{AudioFrame, Error, Frame, MediaType, Packet, Result, SampleFormat};

/// Register the PCM codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Pcm,
        name: "pcm",
        long_name: "PCM (uncompressed linear audio)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(PcmDecoder::default())),
        encoder: Some(|| Box::new(PcmEncoder::default())),
    });
}

/// Reject sample layouts this codec can't handle (planar is unsupported).
fn check_interleaved(format: SampleFormat, op: &str) -> Result<()> {
    match format {
        SampleFormat::S16 | SampleFormat::F32 => Ok(()),
        other => Err(Error::unsupported(format!(
            "{op}: sample format `{}` (only interleaved s16/f32)",
            other.name()
        ))),
    }
}

#[derive(Default)]
struct PcmDecoder {
    sample_rate: u32,
    channels: u16,
    format: Option<SampleFormat>,
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for PcmDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        let format = params
            .sample_format
            .ok_or_else(|| Error::invalid("pcm decode: stream is missing a sample format"))?;
        check_interleaved(format, "pcm decode")?;
        self.sample_rate = params.sample_rate;
        self.channels = params.channels.max(1);
        self.format = Some(format);
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let format = self
            .format
            .ok_or_else(|| Error::invalid("pcm decode: not configured"))?;
        let frame_bytes = format.bytes_per_sample() * self.channels as usize;
        let samples = if frame_bytes == 0 {
            0
        } else {
            packet.data.len() / frame_bytes
        };
        self.frame = Some(Frame::Audio(AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            format,
            planes: vec![packet.data.clone()],
            samples,
            pts: packet.pts,
        }));
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

#[derive(Default)]
struct PcmEncoder {
    packet: Option<Packet>,
    eof: bool,
}

impl Encoder for PcmEncoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let af = match frame {
            Frame::Audio(a) => a,
            Frame::Video(_) => {
                return Err(Error::unsupported(
                    "pcm encode: video frame on an audio codec",
                ))
            }
        };
        check_interleaved(af.format, "pcm encode")?;
        // Interleaved PCM is already the wire format — emit the raw samples.
        let mut packet = Packet::from_data(0, af.planes[0].clone());
        packet.pts = af.pts;
        self.packet = Some(packet);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.packet.take() {
            return Ok(packet);
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
    fn pcm_decode_uses_configured_params() {
        // 4 stereo s16 samples = 16 bytes.
        let data: Vec<u8> = (0..16).collect();
        let mut dec = PcmDecoder::default();
        dec.configure(&CodecParams {
            sample_rate: 48_000,
            channels: 2,
            sample_format: Some(SampleFormat::S16),
            ..Default::default()
        })
        .unwrap();
        dec.send_packet(&Packet::from_data(0, data.clone()))
            .unwrap();
        let Frame::Audio(af) = dec.receive_frame().unwrap() else {
            unreachable!()
        };
        assert_eq!(af.sample_rate, 48_000);
        assert_eq!(af.channels, 2);
        assert_eq!(af.samples, 4); // 16 bytes / (2 ch * 2 bytes)
        assert_eq!(af.planes[0], data);

        // Re-encoding yields the same bytes back.
        let mut enc = PcmEncoder::default();
        enc.send_frame(&Frame::Audio(af)).unwrap();
        assert_eq!(enc.receive_packet().unwrap().data, data);
    }

    #[test]
    fn pcm_decode_requires_a_sample_format() {
        let mut dec = PcmDecoder::default();
        assert!(dec.configure(&CodecParams::default()).is_err());
    }
}
