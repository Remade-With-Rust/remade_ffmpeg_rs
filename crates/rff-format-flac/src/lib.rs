//! Native FLAC audio container (`.flac`).
//!
//! A `.flac` file is `fLaC` + metadata blocks + audio frames; the whole file is
//! the [`flac`](rff-codec-flac) codec's input, so the demuxer hands it over as
//! one packet. Sample rate + channels for `ffprobe` come from the `STREAMINFO`
//! metadata block (the first block, bit-packed right after the magic).

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the FLAC format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "flac",
        long_name: "FLAC (Free Lossless Audio Codec)",
        extensions: &["flac"],
        demuxer: Some(|input| Box::new(FlacDemuxer::new(input))),
        muxer: Some(|output| Box::new(FlacMuxer::new(output))),
        probe: Some(probe_flac),
    });
}

/// Sniff FLAC by its `fLaC` stream marker.
fn probe_flac(data: &[u8]) -> i32 {
    if data.len() >= 4 && &data[0..4] == b"fLaC" {
        100
    } else {
        0
    }
}

/// Read sample rate / channels from the bit-packed `STREAMINFO` block.
///
/// Layout: `fLaC`(4) + block header(4) + STREAMINFO. The 64-bit field at byte
/// 18 packs sample_rate(20) | channels-1(3) | bits-1(5) | total_samples(36).
fn streaminfo(buf: &[u8]) -> Option<(u32, u16)> {
    if buf.len() < 22 {
        return None;
    }
    let b = &buf[18..];
    let sample_rate = ((b[0] as u32) << 12) | ((b[1] as u32) << 4) | ((b[2] as u32) >> 4);
    let channels = ((b[2] >> 1) & 0x07) as u16 + 1;
    Some((sample_rate, channels))
}

struct FlacDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl FlacDemuxer {
    fn new(input: Input) -> FlacDemuxer {
        FlacDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for FlacDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("flac demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        if probe_flac(&buf) == 0 {
            return Err(Error::invalid("flac demux: not a FLAC file"));
        }
        let (sample_rate, channels) =
            streaminfo(&buf).ok_or_else(|| Error::invalid("flac demux: bad STREAMINFO"))?;

        self.sample = Some(buf);
        let mut stream = Stream::new(0, CodecId::Flac);
        stream.sample_rate = sample_rate;
        stream.channels = channels;
        stream.sample_format = Some(rff_core::SampleFormat::F32);
        stream.time_base = Rational::new(1, sample_rate.max(1) as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        match self.sample.take() {
            Some(data) => {
                let mut packet = Packet::from_data(0, data);
                packet.pts = Some(0);
                Ok(packet)
            }
            None => Err(Error::Eof),
        }
    }
}

struct FlacMuxer {
    out: Output,
}

impl FlacMuxer {
    fn new(out: Output) -> FlacMuxer {
        FlacMuxer { out }
    }
}

impl Muxer for FlacMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Flac => Ok(()),
            Some(_) => Err(Error::unsupported(
                "flac mux: only the `flac` codec is supported",
            )),
            None => Err(Error::invalid("flac mux: no streams")),
        }
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        // A FLAC packet is the whole stream; pass it through (stream copy).
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// `fLaC` + a STREAMINFO block declaring 44100 Hz, 2 channels.
    fn flac_header() -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x22]); // block header: STREAMINFO, len 34
        v.extend_from_slice(&[0u8; 10]); // min/max block + min/max frame sizes
                                         // 64-bit packed field: sample_rate=44100 (0x0AC44), channels=2, bits=16.
                                         // bytes: [0x0A, 0xC4, 0x42, ...] → sr top20, then chan(3)=001, bits(5)=01111
        v.push(0x0A);
        v.push(0xC4);
        // byte 20: sr low4 = 0x4, channels-1=1 (bits), bits-1 high...
        // sr low nibble (0x4)<<4 | (chan-1)<<1 | (bits-1 top bit)
        v.push((0x4 << 4) | ((2 - 1) << 1) | 0); // = 0x42
        v.push((15) << 4); // bits-1 = 15 (16-bit) low 4 bits in top nibble
        v.extend_from_slice(&[0u8; 100]); // remaining STREAMINFO + slack
        v
    }

    #[test]
    fn sniffs_and_reads_flac_streaminfo() {
        let flac = flac_header();
        assert_eq!(probe_flac(&flac), 100);
        assert_eq!(probe_flac(b"OggS....."), 0);

        let mut dem = FlacDemuxer::new(Box::new(Cursor::new(flac.clone())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Flac);
        assert_eq!(streams[0].sample_rate, 44_100);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(dem.read_packet().unwrap().data, flac);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
