//! FLAC lossless audio **decoder**, backed by the pure-Rust [`claxon`]
//! (Apache-2.0, no FFI).
//!
//! FLAC is self-describing (its `STREAMINFO` carries sample rate / channels /
//! bit depth), so — like the image codecs — a packet is the whole `.flac`
//! stream and no [`configure`](rff_codec::Decoder::configure) is needed. The
//! whole stream decodes to one interleaved `f32` [`AudioFrame`] (samples
//! normalized from FLAC's native bit depth).
//!
//! Decode only — there is no permissive pure-Rust FLAC *encoder* (registered
//! with `encoder: None`).

use std::io::Cursor;

use claxon::FlacReader;
use rff_codec::{Codec, CodecRegistry, Decoder};
use rff_core::{AudioFrame, Error, Frame, MediaType, Packet, Result, SampleFormat};

/// Register the FLAC codec (decode only) into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Flac,
        name: "flac",
        long_name: "FLAC (Free Lossless Audio Codec)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(FlacDecoder::default())),
        encoder: None,
    });
}

#[derive(Default)]
struct FlacDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for FlacDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut reader = FlacReader::new(Cursor::new(&packet.data))
            .map_err(|e| Error::invalid(format!("flac decode: {e}")))?;
        let info = reader.streaminfo();
        let channels = info.channels.max(1) as usize;
        // Normalize native integer samples to f32 in [-1, 1).
        let scale = (1u64 << info.bits_per_sample.saturating_sub(1).max(1)) as f32;

        let mut floats = Vec::new();
        for sample in reader.samples() {
            let v = sample.map_err(|e| Error::invalid(format!("flac decode: {e}")))?;
            floats.push(v as f32 / scale);
        }
        let total = floats.len();
        let bytes: Vec<u8> = floats.iter().flat_map(|s| s.to_le_bytes()).collect();

        self.frame = Some(Frame::Audio(AudioFrame {
            sample_rate: info.sample_rate,
            channels: channels as u16,
            format: SampleFormat::F32,
            planes: vec![bytes],
            samples: total / channels,
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
}
