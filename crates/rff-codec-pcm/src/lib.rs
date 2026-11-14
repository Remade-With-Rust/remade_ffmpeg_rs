//! Linear PCM audio codec — the audio analog of "raw video".
//!
//! PCM packets carry uncompressed interleaved samples, so they are *not*
//! self-describing: the decoder learns the sample rate, channel count, and
//! sample layout from [`CodecParams`] via [`configure`](rff_codec::Decoder::configure)
//! (the new codec-parameters plumbing). Encodes interleaved `s16` and `f32`;
//! decodes those plus `u8`, `s24`, `s32` and `f64` (camera and DAW files in
//! QuickTime), which it widens to the `s16` / `f32` every encoder takes.

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

/// The layout a decoded frame of `format` is delivered in: the two layouts
/// every encoder here accepts. 8-bit widens to s16 exactly; 24/32-bit integer
/// and 64-bit float go to f32 (24-bit exactly, the others to f32 precision).
fn decoded_format(format: SampleFormat) -> Result<SampleFormat> {
    match format {
        SampleFormat::S16 | SampleFormat::U8 => Ok(SampleFormat::S16),
        SampleFormat::F32 | SampleFormat::S24 | SampleFormat::S32 | SampleFormat::F64 => {
            Ok(SampleFormat::F32)
        }
        other => Err(Error::unsupported(format!(
            "pcm decode: sample format `{}` (interleaved u8/s16/s24/s32/f32/f64 only)",
            other.name()
        ))),
    }
}

/// Convert interleaved `format` samples to [`decoded_format`]'s layout.
/// `S16` and `F32` pass through untouched (no copy beyond the packet's).
fn widen(format: SampleFormat, data: &[u8]) -> Vec<u8> {
    match format {
        SampleFormat::U8 => data
            .iter()
            .flat_map(|&b| (((b as i16) - 128) << 8).to_le_bytes())
            .collect(),
        SampleFormat::S24 => data
            .chunks_exact(3)
            .flat_map(|c| {
                let v = i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8;
                (v as f32 / 8_388_608.0).to_le_bytes()
            })
            .collect(),
        SampleFormat::S32 => data
            .chunks_exact(4)
            .map(|c| {
                let v = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                (v as f64 / 2_147_483_648.0) as f32
            })
            .flat_map(f32::to_le_bytes)
            .collect(),
        SampleFormat::F64 => data
            .chunks_exact(8)
            .flat_map(|c| {
                let v = f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
                (v as f32).to_le_bytes()
            })
            .collect(),
        _ => data.to_vec(),
    }
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
        decoded_format(format)?;
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
            format: decoded_format(format)?,
            planes: vec![widen(format, &packet.data[..samples * frame_bytes])],
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

    fn decode_one(format: SampleFormat, channels: u16, data: Vec<u8>) -> AudioFrame {
        let mut dec = PcmDecoder::default();
        dec.configure(&CodecParams {
            sample_rate: 48_000,
            channels,
            sample_format: Some(format),
            ..Default::default()
        })
        .unwrap();
        dec.send_packet(&Packet::from_data(0, data)).unwrap();
        let Frame::Audio(af) = dec.receive_frame().unwrap() else {
            unreachable!()
        };
        af
    }

    fn f32s(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn camera_layouts_widen_to_what_encoders_take() {
        // u8: 0x80 is silence, 0x00 full negative, 0xFF just under full positive.
        let af = decode_one(SampleFormat::U8, 1, vec![0x80, 0x00, 0xFF]);
        assert_eq!(af.format, SampleFormat::S16);
        assert_eq!(af.samples, 3);
        assert_eq!(
            af.planes[0],
            [0i16, -32768, 32512]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>()
        );

        // s24: +half scale and -full scale, stereo, one frame.
        let af = decode_one(
            SampleFormat::S24,
            2,
            vec![0x00, 0x00, 0x40, 0x00, 0x00, 0x80],
        );
        assert_eq!(af.format, SampleFormat::F32);
        assert_eq!(af.samples, 1);
        assert_eq!(f32s(&af.planes[0]), [0.5, -1.0]);

        // s32 and f64 land on the same values.
        let af = decode_one(
            SampleFormat::S32,
            1,
            [i32::MIN, 1 << 30]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        );
        assert_eq!(f32s(&af.planes[0]), [-1.0, 0.5]);
        let af = decode_one(
            SampleFormat::F64,
            1,
            [0.25f64, -0.75]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        );
        assert_eq!(f32s(&af.planes[0]), [0.25, -0.75]);

        // A trailing partial frame is not decoded as a sample.
        let af = decode_one(SampleFormat::S24, 2, vec![0; 7]);
        assert_eq!((af.samples, af.planes[0].len()), (1, 8));
    }

    #[test]
    fn pcm_decode_requires_a_sample_format() {
        let mut dec = PcmDecoder::default();
        assert!(dec.configure(&CodecParams::default()).is_err());
    }
}
