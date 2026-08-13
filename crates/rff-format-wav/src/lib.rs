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
        muxer_path: None,
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

/// How the demuxer delivers a WAVE `(format_tag, bits_per_sample)` layout.
/// The engine's native sample formats are s16/f32; the other PCM layouts are
/// repacked losslessly on read (24-bit ints fit f32's mantissa exactly, u8
/// fits s16), so every standard PCM WAV decodes without an engine-wide
/// sample-format addition.
enum Delivery {
    /// Pass the data chunk through untouched as this format.
    Native(SampleFormat),
    /// Unsigned 8-bit → s16 (exact, `(v-128) << 8`).
    U8ToS16,
    /// Signed 24-bit little-endian → f32 (exact, 24 bits < f32's mantissa).
    S24ToF32,
}

/// Map a WAVE `(format_tag, bits_per_sample)` to a delivery plan.
fn sample_format(tag: u16, bits: u16) -> Option<Delivery> {
    match (tag, bits) {
        (1, 16) => Some(Delivery::Native(SampleFormat::S16)),
        (3, 32) => Some(Delivery::Native(SampleFormat::F32)),
        (1, 8) => Some(Delivery::U8ToS16),
        (1, 24) => Some(Delivery::S24ToF32),
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
        let mut format_tag = rd_u16(f, 0);
        let channels = rd_u16(f, 2);
        let sample_rate = rd_u32(f, 4);
        let bits = rd_u16(f, 14);
        // WAVE_FORMAT_EXTENSIBLE: the real tag is the SubFormat GUID's first
        // two bytes (fmt chunk offset 24), after cbSize(2) + valid-bits(2) +
        // channel-mask(4).
        if format_tag == 0xFFFE {
            if fmt.len() < 26 {
                return Err(Error::invalid("wav demux: short extensible `fmt ` chunk"));
            }
            format_tag = rd_u16(f, 24);
        }
        let delivery = sample_format(format_tag, bits).ok_or_else(|| {
            Error::unsupported(format!(
                "wav demux: format tag {format_tag}, {bits}-bit (pcm u8/s16/s24 or f32)"
            ))
        })?;

        let raw = top
            .iter()
            .find(|(id, _)| id == b"data")
            .map(|(_, r)| &buf[r.clone()])
            .ok_or_else(|| Error::invalid("wav demux: no `data` chunk"))?;
        let (format, data) = match delivery {
            Delivery::Native(fmt) => (fmt, raw.to_vec()),
            Delivery::U8ToS16 => {
                let mut out = Vec::with_capacity(raw.len() * 2);
                for &b in raw {
                    let v = ((b as i16) - 128) << 8;
                    out.extend_from_slice(&v.to_le_bytes());
                }
                (SampleFormat::S16, out)
            }
            Delivery::S24ToF32 => {
                let n = raw.len() / 3;
                let mut out = Vec::with_capacity(n * 4);
                const SCALE: f32 = 1.0 / 8_388_608.0; // 2^-23: exact for 24-bit ints
                for s in raw[..n * 3].chunks_exact(3) {
                    let v = i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8; // sign-extend
                    out.extend_from_slice(&(v as f32 * SCALE).to_le_bytes());
                }
                (SampleFormat::F32, out)
            }
        };
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

    /// Build a minimal WAV file around a fmt chunk + data chunk.
    fn wav_file(fmt: &[u8], data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        put_chunk(&mut body, b"fmt ", fmt);
        put_chunk(&mut body, b"data", data);
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);
        file
    }

    fn fmt_chunk(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let block_align = channels * (bits / 8);
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        fmt
    }

    #[test]
    fn s24_wav_delivers_exact_f32() {
        // Extremes + sign cases: the f32 delivery must be exact.
        let samples: [i32; 6] = [0, 1, -1, 8_388_607, -8_388_608, -4_242_424];
        let mut data = Vec::new();
        for &v in &samples {
            data.extend_from_slice(&v.to_le_bytes()[..3]);
        }
        let file = wav_file(&fmt_chunk(1, 1, 48_000, 24), &data);

        let mut dem = WavDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].sample_format, Some(SampleFormat::F32));
        let pkt = dem.read_packet().unwrap();
        for (i, &v) in samples.iter().enumerate() {
            let f = f32::from_le_bytes(pkt.data[i * 4..i * 4 + 4].try_into().unwrap());
            let back = (f * 8_388_608.0).round() as i32;
            assert_eq!(back, v, "sample {i} not exact");
        }
    }

    #[test]
    fn extensible_24bit_wav_parses() {
        // WAVE_FORMAT_EXTENSIBLE (0xFFFE) with a PCM SubFormat GUID.
        let mut fmt = fmt_chunk(0xFFFE, 2, 44_100, 24);
        fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&24u16.to_le_bytes()); // valid bits
        fmt.extend_from_slice(&3u32.to_le_bytes()); // channel mask
        fmt.extend_from_slice(&1u16.to_le_bytes()); // SubFormat: PCM
        fmt.extend_from_slice(&[0u8; 14]); // rest of GUID
        let file = wav_file(&fmt, &[0u8; 6]);

        let mut dem = WavDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].sample_format, Some(SampleFormat::F32));
        assert_eq!(streams[0].channels, 2);
    }

    #[test]
    fn u8_wav_delivers_s16() {
        let file = wav_file(&fmt_chunk(1, 1, 8_000, 8), &[0u8, 128, 255]);
        let mut dem = WavDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].sample_format, Some(SampleFormat::S16));
        let pkt = dem.read_packet().unwrap();
        let vals: Vec<i16> = pkt
            .data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(vals, vec![-32768, 0, 32512]);
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
            mux.write_packet(&Packet::from_data(0, pcm.clone()))
                .unwrap();
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
