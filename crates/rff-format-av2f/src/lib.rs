//! **AV2F** still-image container, backed by [`rusty_av2f`].
//!
//! One AV2 still picture in an ISOBMFF/HEIF file, in the shape AVIF uses for
//! AV1. The coded payload is a plain AV2 bitstream, so decoding goes through
//! the existing [`CodecId::Av2`] decoder — this crate is the container only.
//!
//! # Experimental — not an AOM standard
//!
//! AVIF's brand, item type and configuration record are fixed by a published
//! AOM specification. **No equivalent document exists for AV2**, so AV2F's
//! four-character codes are chosen rather than specified. They live in
//! `rusty_av2f::fourcc` so a real specification can be adopted by editing one
//! file.
//!
//! Files written here are readable here and nowhere else. Useful for pipeline
//! work and for measuring AV2 against AVIF on stills; not for interchange.
//!
//! # Header forms
//!
//! Both AV2 still-picture header forms mux and demux: the full form and the
//! compact `single_picture_header_flag` form (the natural choice for an image
//! format — it is what AVIF does for AV1). The historical full-only
//! restriction was lifted with `rusty_av2f` 0.2.0, once `rusty_av2d` 0.2.5
//! decoded the compact form byte-identically to the reference.

use std::io::Read;

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};
use rusty_av2f::{decode, encode, Config, Params, Subsampling};

/// Register the AV2F format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "av2f",
        long_name: "AV2F (AV2 still image, experimental)",
        extensions: &[rusty_av2f::fourcc::EXTENSION],
        demuxer: Some(|input| Box::new(Av2fDemuxer::new(input))),
        muxer: Some(|output| Box::new(Av2fMuxer::new(output))),
        muxer_path: None,
        probe: Some(rusty_av2f::probe),
        mux_caps: MuxCaps::single(&[CodecId::Av2]).image(),
    });
}

fn map_err(e: rusty_av2f::Error) -> Error {
    match e {
        rusty_av2f::Error::NotAv2f => Error::invalid("av2f: not an AV2F file"),
        rusty_av2f::Error::Malformed(w) => Error::invalid(format!("av2f: {w}")),
        rusty_av2f::Error::Invalid(w) => Error::invalid(format!("av2f: {w}")),
        rusty_av2f::Error::Unsupported(w) => Error::unsupported(format!("av2f: {w}")),
    }
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct Av2fDemuxer {
    input: Option<Input>,
    /// The coded payload, handed out once by `read_packet`.
    payload: Option<Vec<u8>>,
}

impl Av2fDemuxer {
    fn new(input: Input) -> Av2fDemuxer {
        Av2fDemuxer {
            input: Some(input),
            payload: None,
        }
    }
}

impl Demuxer for Av2fDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("av2f demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;

        let img = decode(&buf).map_err(map_err)?;
        self.payload = Some(img.payload.to_vec());

        let mut stream = Stream::new(0, CodecId::Av2);
        stream.width = img.width;
        stream.height = img.height;
        // A still image: one frame, timebase is a formality.
        stream.time_base = Rational::new(1, 1);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let data = self.payload.take().ok_or(Error::Eof)?;
        let mut packet = Packet::from_data(0, data);
        packet.pts = Some(0);
        packet.flags.keyframe = true;
        Ok(packet)
    }
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct Av2fMuxer {
    out: Output,
    width: u32,
    height: u32,
    payload: Vec<u8>,
}

impl Av2fMuxer {
    fn new(out: Output) -> Av2fMuxer {
        Av2fMuxer {
            out,
            width: 0,
            height: 0,
            payload: Vec::new(),
        }
    }
}

impl Muxer for Av2fMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .iter()
            .find(|s| s.media_type == MediaType::Video)
            .ok_or_else(|| Error::invalid("av2f mux: no video stream"))?;
        if s.codec_id != CodecId::Av2 {
            return Err(Error::unsupported(format!(
                "av2f mux: needs an `av2` stream, got `{}`",
                s.codec_id
            )));
        }
        if s.width == 0 || s.height == 0 {
            return Err(Error::invalid("av2f mux: stream is missing image dimensions"));
        }
        self.width = s.width;
        self.height = s.height;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        // A still image is one picture. Accepting a second would silently
        // produce a file holding only the first, so refuse instead.
        if !self.payload.is_empty() {
            return Err(Error::unsupported(
                "av2f mux: AV2F holds a single still picture; got more than one packet",
            ));
        }
        self.payload.extend_from_slice(&packet.data);
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.payload.is_empty() {
            return Err(Error::invalid("av2f mux: no image data was written"));
        }
        let params = Params {
            width: self.width,
            height: self.height,
            config: Config {
                // The container records what it was told; the authoritative
                // description is the AV2 sequence header inside the payload.
                bit_depth: 8,
                subsampling: Subsampling::Yuv420,
                full_still_picture_header: true,
            },
        };
        let file = encode(&params, &self.payload).map_err(map_err)?;
        self.out.write_all(&file)?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mux_one(payload: &[u8]) -> Vec<u8> {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = Av2fMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Av2);
            s.width = 432;
            s.height = 240;
            mux.write_header(&[s]).unwrap();
            mux.write_packet(&Packet::from_data(0, payload.to_vec()))
                .unwrap();
            mux.write_trailer().unwrap();
        }
        let v = sink.0.lock().unwrap().clone();
        v
    }

    #[test]
    fn mux_then_demux_preserves_the_payload() {
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 253) as u8).collect();
        let file = mux_one(&payload);
        assert_eq!(rusty_av2f::probe(&file), 100);

        let mut dem = Av2fDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Av2);
        assert_eq!((streams[0].width, streams[0].height), (432, 240));

        let pkt = dem.read_packet().unwrap();
        assert_eq!(pkt.data, payload, "payload must survive the container");
        assert!(pkt.flags.keyframe);
        // A still image yields exactly one packet.
        assert!(dem.read_packet().is_err());
    }

    #[test]
    fn refuses_a_second_picture() {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let mut mux = Av2fMuxer::new(Box::new(sink));
        let mut s = Stream::new(0, CodecId::Av2);
        s.width = 16;
        s.height = 16;
        mux.write_header(&[s]).unwrap();
        mux.write_packet(&Packet::from_data(0, vec![1, 2, 3])).unwrap();
        assert!(
            mux.write_packet(&Packet::from_data(0, vec![4, 5])).is_err(),
            "a second packet must fail loudly, not be dropped"
        );
    }

    #[test]
    fn refuses_a_non_av2_stream() {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let mut mux = Av2fMuxer::new(Box::new(sink));
        let mut s = Stream::new(0, CodecId::Vp9);
        s.width = 16;
        s.height = 16;
        assert!(mux.write_header(&[s]).is_err());
    }

    #[test]
    fn does_not_claim_avif_files() {
        let mut avif = vec![0, 0, 0, 32];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0; 20]);
        assert!(rusty_av2f::probe(&avif) < 100);
    }
}
