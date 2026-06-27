//! WAV (RIFF/WAVE) audio container.
//!
//! Reads the `fmt ` chunk for the PCM layout and yields the `data` chunk as one
//! packet for the [`pcm`](rff-codec-pcm) codec; writes both back. Supports
//! interleaved `s16` (WAVE format 1) and `f32` (format 3). The codec parameters
//! (sample rate, channels, sample format) ride on the [`Stream`].

use std::io::{Read, Write};
use std::ops::Range;

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the WAV format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "wav",
        long_name: "WAV / WAVE (RIFF audio)",
        extensions: &["wav"],
        demuxer: Some(|input| Box::new(WavDemuxer::new(input))),
        muxer: Some(|output| Box::new(WavMuxer::new(output))),
        probe: Some(probe_wav),
    });
}

/// Sniff WAV: a RIFF file whose form type is `WAVE`.
fn probe_wav(data: &[u8]) -> i32 {
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        100
    } else {
        0
    }
}

fn rd_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Walk RIFF sub-chunks in `buf[start..]`, returning `(id, data_range)` pairs.
fn chunks(buf: &[u8], mut p: usize) -> Vec<([u8; 4], Range<usize>)> {
    let mut out = Vec::new();
    while p + 8 <= buf.len() {
        let id = [buf[p], buf[p + 1], buf[p + 2], buf[p + 3]];
        let size = rd_u32(buf, p + 4) as usize;
        let start = p + 8;
        let end = (start + size).min(buf.len());
        out.push((id, start..end));
        p = end + (size & 1); // pad to even
    }
    out
}

/// Map a WAVE `(format_tag, bits_per_sample)` to a [`SampleFormat`].
fn sample_format(tag: u16, bits: u16) -> Option<SampleFormat> {
    match (tag, bits) {
        (1, 16) => Some(SampleFormat::S16),
        (3, 32) => Some(SampleFormat::F32),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct WavDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl WavDemuxer {
    fn new(input: Input) -> WavDemuxer {
        WavDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for WavDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("wav demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        if probe_wav(&buf) == 0 {
            return Err(Error::invalid("wav demux: not a RIFF/WAVE file"));
        }

        let top = chunks(&buf, 12); // skip "RIFF" + size + "WAVE"
        let fmt = top
            .iter()
            .find(|(id, _)| id == b"fmt ")
            .map(|(_, r)| r.clone())
            .ok_or_else(|| Error::invalid("wav demux: no `fmt ` chunk"))?;
        if fmt.len() < 16 {
            return Err(Error::invalid("wav demux: short `fmt ` chunk"));
        }
        let f = &buf[fmt.start..];
        let format_tag = rd_u16(f, 0);
        let channels = rd_u16(f, 2);
        let sample_rate = rd_u32(f, 4);
        let bits = rd_u16(f, 14);
        let format = sample_format(format_tag, bits).ok_or_else(|| {
            Error::unsupported(format!(
                "wav demux: format tag {format_tag}, {bits}-bit (only s16/f32)"
            ))
        })?;

        let data = top
            .iter()
            .find(|(id, _)| id == b"data")
            .map(|(_, r)| buf[r.clone()].to_vec())
            .ok_or_else(|| Error::invalid("wav demux: no `data` chunk"))?;
        self.sample = Some(data);

        let mut stream = Stream::new(0, CodecId::Pcm);
        stream.sample_rate = sample_rate;
        stream.channels = channels;
        stream.sample_format = Some(format);
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

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct WavMuxer {
    out: Output,
    channels: u16,
    sample_rate: u32,
    format: SampleFormat,
    data: Vec<u8>,
}

impl WavMuxer {
    fn new(out: Output) -> WavMuxer {
        WavMuxer {
            out,
            channels: 0,
            sample_rate: 0,
            format: SampleFormat::S16,
            data: Vec::new(),
        }
    }
}

impl Muxer for WavMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .first()
            .filter(|s| s.codec_id == CodecId::Pcm)
            .ok_or_else(|| Error::unsupported("wav mux: needs a single `pcm` stream"))?;
        self.channels = s.channels.max(1);
        self.sample_rate = s.sample_rate.max(1);
        self.format = s
            .sample_format
            .ok_or_else(|| Error::invalid("wav mux: stream is missing a sample format"))?;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.data.extend_from_slice(&packet.data);
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let (tag, bits): (u16, u16) = match self.format {
            SampleFormat::S16 => (1, 16),
            SampleFormat::F32 => (3, 32),
            other => {
                return Err(Error::unsupported(format!(
                    "wav mux: sample format `{}` (only s16/f32)",
                    other.name()
                )))
            }
        };
        let block_align = self.channels * (bits / 8);
        let byte_rate = self.sample_rate * block_align as u32;

        let mut fmt = Vec::new();
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&self.channels.to_le_bytes());
        fmt.extend_from_slice(&self.sample_rate.to_le_bytes());
        fmt.extend_from_slice(&byte_rate.to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());

        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        put_chunk(&mut body, b"fmt ", &fmt);
        put_chunk(&mut body, b"data", &self.data);

        self.out.write_all(b"RIFF")?;
        self.out.write_all(&(body.len() as u32).to_le_bytes())?;
        self.out.write_all(&body)?;
        self.out.flush()?;
        Ok(())
    }
}

fn put_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
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

    #[test]
    fn wav_mux_then_demux_roundtrips() {
        let pcm: Vec<u8> = (0..32).collect(); // 8 stereo s16 samples

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = WavMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Pcm);
            s.channels = 2;
            s.sample_rate = 44_100;
            s.sample_format = Some(SampleFormat::S16);
            mux.write_header(&[s]).unwrap();
            mux.write_packet(&Packet::from_data(0, pcm.clone())).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[0..4], b"RIFF");
        assert_eq!(&file[8..12], b"WAVE");
        assert_eq!(probe_wav(&file), 100);

        let mut dem = WavDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Pcm);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(streams[0].sample_rate, 44_100);
        assert_eq!(streams[0].sample_format, Some(SampleFormat::S16));
        assert_eq!(dem.read_packet().unwrap().data, pcm);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
