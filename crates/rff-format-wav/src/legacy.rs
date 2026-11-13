//! The pre-#26 WAV demuxer (data chunk read whole, delivered as one packet),
//! kept test-only as the equivalence oracle for the streaming one: the
//! concatenated payload of the streaming demuxer's packets must equal this
//! one's single packet on every layout.

use std::io::Read;
use std::ops::Range;

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Input, Stream};

use super::{parse_fmt, probe_wav, rd_u32, Delivery, WavFmt};

/// Walk RIFF sub-chunks in `buf[start..]`, returning `(id, data_range)` pairs.
pub(crate) fn chunks(buf: &[u8], mut p: usize) -> Vec<([u8; 4], Range<usize>)> {
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

/// How much of the file was read before the `data` chunk was located.
pub(crate) const HEAD_BYTES: u64 = 64 * 1024;

fn read_wav(input: &mut Input) -> Result<(WavFmt, Vec<u8>)> {
    let mut head = Vec::new();
    input.by_ref().take(HEAD_BYTES).read_to_end(&mut head)?;
    if probe_wav(&head) != 0 {
        let top = chunks(&head, 12);
        let fmt = top.iter().position(|(id, _)| id == b"fmt ");
        let data = top.iter().position(|(id, _)| id == b"data");
        if let (Some(f), Some(d)) = (fmt, data) {
            if f < d {
                let fmt = parse_fmt(&head[top[f].1.clone()])?;
                let start = top[d].1.start;
                let declared = rd_u32(&head, start - 4) as usize;
                let mut samples = Vec::with_capacity(declared.min(1 << 28));
                samples.extend_from_slice(&head[start..]);
                input.read_to_end(&mut samples)?;
                samples.truncate(declared);
                return Ok((fmt, samples));
            }
        }
    }

    let mut buf = head;
    input.read_to_end(&mut buf)?;
    if probe_wav(&buf) == 0 {
        return Err(Error::invalid("wav demux: not a RIFF/WAVE file"));
    }
    let top = chunks(&buf, 12);
    let fmt = top
        .iter()
        .find(|(id, _)| id == b"fmt ")
        .map(|(_, r)| r.clone())
        .ok_or_else(|| Error::invalid("wav demux: no `fmt ` chunk"))?;
    let fmt = parse_fmt(&buf[fmt])?;
    let data = top
        .iter()
        .find(|(id, _)| id == b"data")
        .map(|(_, r)| r.clone())
        .ok_or_else(|| Error::invalid("wav demux: no `data` chunk"))?;
    buf.truncate(data.end);
    buf.drain(..data.start);
    Ok((fmt, buf))
}

pub(crate) struct LegacyWavDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl LegacyWavDemuxer {
    pub(crate) fn new(input: Input) -> LegacyWavDemuxer {
        LegacyWavDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for LegacyWavDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("wav demux: header already read"))?;
        let (
            WavFmt {
                channels,
                sample_rate,
                delivery,
            },
            raw,
        ) = read_wav(&mut input)?;
        let (format, data) = match delivery {
            Delivery::Native(fmt) => (fmt, raw),
            Delivery::U8ToS16 => {
                let mut out = Vec::with_capacity(raw.len() * 2);
                for &b in &raw {
                    let v = ((b as i16) - 128) << 8;
                    out.extend_from_slice(&v.to_le_bytes());
                }
                (SampleFormat::S16, out)
            }
            Delivery::S24ToF32 => {
                let n = raw.len() / 3;
                let mut out = Vec::with_capacity(n * 4);
                const SCALE: f32 = 1.0 / 8_388_608.0;
                for s in raw[..n * 3].chunks_exact(3) {
                    let v = i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8;
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

/// The pre-#28 WAV muxer (every sample buffered, the file assembled in
/// `write_trailer`), kept test-only as the byte-identity oracle for both
/// muxer paths: the buffered one (pipes) and the streaming one (seekable
/// outputs) must write exactly these bytes.
pub(crate) struct LegacyWavMuxer {
    out: rff_format::Output,
    channels: u16,
    sample_rate: u32,
    format: SampleFormat,
    data: Vec<u8>,
}

impl LegacyWavMuxer {
    pub(crate) fn new(out: rff_format::Output) -> LegacyWavMuxer {
        LegacyWavMuxer {
            out,
            channels: 0,
            sample_rate: 0,
            format: SampleFormat::S16,
            data: Vec::new(),
        }
    }
}

impl rff_format::Muxer for LegacyWavMuxer {
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
        use std::io::Write;
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

        let mut head = Vec::with_capacity(4 + 8 + fmt.len() + 8);
        head.extend_from_slice(b"WAVE");
        super::put_chunk(&mut head, b"fmt ", &fmt);
        let pad = self.data.len() % 2;
        let body_len = head.len() + 8 + self.data.len() + pad;
        head.extend_from_slice(b"data");
        head.extend_from_slice(&(self.data.len() as u32).to_le_bytes());

        self.out.write_all(b"RIFF")?;
        self.out.write_all(&(body_len as u32).to_le_bytes())?;
        self.out.write_all(&head)?;
        self.out.write_all(&self.data)?;
        if pad == 1 {
            self.out.write_all(&[0])?;
        }
        self.out.flush()?;
        Ok(())
    }
}
