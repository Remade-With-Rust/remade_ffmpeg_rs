//! WAV (RIFF/WAVE) audio container.
//!
//! Reads the `fmt ` chunk for the PCM layout and streams the `data` chunk as
//! successive packets of whole sample frames (up to 4096 per packet) for
//! the [`pcm`](rff-codec-pcm) codec; writes both back. Supports interleaved
//! `s16` (WAVE format 1) and `f32` (format 3), plus u8/s24 repacked on read.
//! The codec parameters (sample rate, channels, sample format) ride on the
//! [`Stream`].

use std::io::{BufReader, Read, Seek, SeekFrom, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{
    Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, SeekOutput, Stream,
};

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
        mux_caps: MuxCaps::single(&[CodecId::Pcm]),
    });
    // Files stream the samples and patch the header's sizes at the end (#28);
    // pipes keep the buffered muxer above.
    registry.register_seekable_muxer("wav", |output| Box::new(WavMuxer::new_seekable(output)));
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

/// How the demuxer delivers a WAVE `(format_tag, bits_per_sample)` layout.
/// The engine's native sample formats are s16/f32; the other PCM layouts are
/// repacked losslessly on read (24-bit ints fit f32's mantissa exactly, u8
/// fits s16), so every standard PCM WAV decodes without an engine-wide
/// sample-format addition.
#[derive(Clone, Copy)]
enum Delivery {
    /// Pass the data chunk through untouched as this format.
    Native(SampleFormat),
    /// Unsigned 8-bit → s16 (exact, `(v-128) << 8`).
    U8ToS16,
    /// Signed 24-bit little-endian → f32 (exact, 24 bits < f32's mantissa).
    S24ToF32,
}

impl Delivery {
    /// Bytes per sample in the file.
    fn raw_bytes(self) -> usize {
        match self {
            Delivery::Native(f) => f.bytes_per_sample(),
            Delivery::U8ToS16 => 1,
            Delivery::S24ToF32 => 3,
        }
    }

    /// Repack one block of file samples into the delivered format. A block
    /// is a whole number of samples except possibly the last one, whose
    /// trailing partial sample is handled exactly as the whole chunk was.
    fn deliver(self, raw: Vec<u8>) -> (SampleFormat, Vec<u8>) {
        match self {
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
                const SCALE: f32 = 1.0 / 8_388_608.0; // 2^-23: exact for 24-bit ints
                for s in raw[..n * 3].chunks_exact(3) {
                    let v = i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8; // sign-extend
                    out.extend_from_slice(&(v as f32 * SCALE).to_le_bytes());
                }
                (SampleFormat::F32, out)
            }
        }
    }

    /// The sample format packets carry.
    fn format(self) -> SampleFormat {
        match self {
            Delivery::Native(f) => f,
            Delivery::U8ToS16 => SampleFormat::S16,
            Delivery::S24ToF32 => SampleFormat::F32,
        }
    }
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

/// Sample frames per packet (4096 — ~93 ms at 44.1 kHz; 16 KiB of stereo
/// s16). Every packet but the last holds exactly this many whole frames.
const FRAMES_PER_PACKET: usize = 4096;
/// Upper bound on one packet's file bytes, for absurd channel counts.
const MAX_PACKET_BYTES: usize = 1 << 20;
/// How much of a `fmt ` chunk is kept: the parser looks at its first 26 bytes;
/// the rest (whatever its declared size) is skipped, not buffered.
const FMT_KEEP: usize = 64;

/// Where the `data` chunk's bytes come from.
enum Source {
    /// The usual layout (`fmt ` before `data`): read straight from the input,
    /// `remaining` bytes left of the chunk (`None` = unknown length, to EOF).
    Stream {
        reader: BufReader<Input>,
        remaining: Option<u64>,
    },
    /// `data` before `fmt `: a non-seekable input cannot come back for the
    /// samples, so that (rare) layout buffers the chunk as before.
    Buffered { data: Vec<u8>, pos: usize },
}

/// Streaming WAV demuxer: `read_header` walks the RIFF chunks up to the start
/// of `data` (skipping LIST/JUNK/fact etc. without buffering them), then
/// `read_packet` returns successive blocks of the data chunk, so peak memory
/// is one packet, not the file (issue #26). Works on pipes.
struct WavDemuxer {
    input: Option<Input>,
    source: Option<Source>,
    delivery: Delivery,
    /// File bytes per sample frame (all channels), at least 1.
    raw_frame: usize,
    /// Frames per packet (≤ [`FRAMES_PER_PACKET`]).
    frames_per_packet: usize,
    /// PTS of the next packet, in samples (the stream time base is 1/rate).
    next_pts: i64,
    /// Whether a packet has been returned (an empty data chunk still yields
    /// one empty packet, as the whole-chunk demuxer did).
    emitted: bool,
}

impl WavDemuxer {
    fn new(input: Input) -> WavDemuxer {
        WavDemuxer {
            input: Some(input),
            source: None,
            delivery: Delivery::Native(SampleFormat::S16),
            raw_frame: 1,
            frames_per_packet: FRAMES_PER_PACKET,
            next_pts: 0,
            emitted: false,
        }
    }
}

/// The `fmt ` chunk fields the demuxer needs.
struct WavFmt {
    channels: u16,
    sample_rate: u32,
    delivery: Delivery,
}

/// Parse a `fmt ` chunk body (`f` = its bytes, clipped to the file).
fn parse_fmt(f: &[u8]) -> Result<WavFmt> {
    if f.len() < 16 {
        return Err(Error::invalid("wav demux: short `fmt ` chunk"));
    }
    let mut format_tag = rd_u16(f, 0);
    let channels = rd_u16(f, 2);
    let sample_rate = rd_u32(f, 4);
    let bits = rd_u16(f, 14);
    // WAVE_FORMAT_EXTENSIBLE: the real tag is the SubFormat GUID's first
    // two bytes (fmt chunk offset 24), after cbSize(2) + valid-bits(2) +
    // channel-mask(4).
    if format_tag == 0xFFFE {
        if f.len() < 26 {
            return Err(Error::invalid("wav demux: short extensible `fmt ` chunk"));
        }
        format_tag = rd_u16(f, 24);
    }
    let delivery = sample_format(format_tag, bits).ok_or_else(|| {
        Error::unsupported(format!(
            "wav demux: format tag {format_tag}, {bits}-bit (pcm u8/s16/s24 or f32)"
        ))
    })?;
    Ok(WavFmt {
        channels,
        sample_rate,
        delivery,
    })
}

/// Fill as much of `buf` as the source holds (short only at end of input).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Read up to `len` bytes, growing the buffer only as bytes actually arrive
/// (in bounded steps), so a lying size field cannot force a huge allocation.
fn read_up_to(r: &mut impl Read, len: u64) -> std::io::Result<Vec<u8>> {
    const STEP: u64 = 8 << 20;
    let mut v = Vec::new();
    let mut left = len;
    while left > 0 {
        let want = left.min(STEP);
        v.reserve_exact(want as usize);
        let got = r.by_ref().take(want).read_to_end(&mut v)? as u64;
        left -= got;
        if got < want {
            break; // end of input
        }
    }
    Ok(v)
}

/// Discard `n` bytes (or to end of input) without buffering them.
fn skip(r: &mut impl Read, n: u64) -> std::io::Result<()> {
    std::io::copy(&mut r.by_ref().take(n), &mut std::io::sink())?;
    Ok(())
}

/// Walk the RIFF chunks up to the samples: the FIRST `fmt ` chunk and the
/// FIRST `data` chunk, exactly as the whole-file walk chose them, with a data
/// chunk's bytes being `[start, min(start + size, EOF))`.
fn open_wav(input: Input) -> Result<(WavFmt, Source)> {
    let mut r = BufReader::new(input);
    let mut riff = [0u8; 12];
    if read_full(&mut r, &mut riff)? < 12 || probe_wav(&riff) == 0 {
        return Err(Error::invalid("wav demux: not a RIFF/WAVE file"));
    }
    let mut fmt: Option<WavFmt> = None;
    let mut early_data: Option<Vec<u8>> = None;
    loop {
        let mut ch = [0u8; 8];
        if read_full(&mut r, &mut ch)? < 8 {
            break; // no further complete chunk header
        }
        let size = rd_u32(&ch, 4) as u64;
        let pad = size & 1; // chunks are padded to even
        match &ch[0..4] {
            b"fmt " if fmt.is_none() => {
                let body = read_up_to(&mut r, size.min(FMT_KEEP as u64))?;
                fmt = Some(parse_fmt(&body)?);
                skip(&mut r, size - body.len() as u64 + pad)?;
            }
            b"data" if early_data.is_none() => {
                if let Some(fmt) = fmt.take() {
                    // 0 and 0xFFFFFFFF both mean "length unknown" (a streamed
                    // WAV whose writer could not seek back): read to EOF.
                    let remaining = (size != 0 && size != 0xFFFF_FFFF).then_some(size);
                    return Ok((
                        fmt,
                        Source::Stream {
                            reader: r,
                            remaining,
                        },
                    ));
                }
                early_data = Some(read_up_to(&mut r, size)?);
                skip(&mut r, pad)?;
            }
            _ => skip(&mut r, size + pad)?, // LIST, JUNK, fact, bext, ...
        }
    }
    let fmt = fmt.ok_or_else(|| Error::invalid("wav demux: no `fmt ` chunk"))?;
    let data = early_data.ok_or_else(|| Error::invalid("wav demux: no `data` chunk"))?;
    Ok((fmt, Source::Buffered { data, pos: 0 }))
}

impl Demuxer for WavDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("wav demux: header already read"))?;
        let (
            WavFmt {
                channels,
                sample_rate,
                delivery,
            },
            source,
        ) = open_wav(input)?;
        self.delivery = delivery;
        self.raw_frame = delivery.raw_bytes() * channels.max(1) as usize;
        self.frames_per_packet = (MAX_PACKET_BYTES / self.raw_frame).clamp(1, FRAMES_PER_PACKET);
        self.source = Some(source);

        let mut stream = Stream::new(0, CodecId::Pcm);
        stream.sample_rate = sample_rate;
        stream.channels = channels;
        stream.sample_format = Some(delivery.format());
        stream.time_base = Rational::new(1, sample_rate.max(1) as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let block = self.raw_frame * self.frames_per_packet;
        let raw = match self.source.as_mut() {
            None => return Err(Error::Eof),
            Some(Source::Stream { reader, remaining }) => {
                let want = remaining.map_or(block as u64, |left| left.min(block as u64));
                let raw = read_up_to(reader, want)?;
                if let Some(left) = remaining {
                    *left -= raw.len() as u64;
                }
                raw
            }
            Some(Source::Buffered { data, pos }) => {
                let end = (*pos + block).min(data.len());
                let raw = data[*pos..end].to_vec();
                *pos = end;
                raw
            }
        };
        if raw.is_empty() && self.emitted {
            self.source = None; // drop the input; the end is sticky
            return Err(Error::Eof);
        }
        self.emitted = true;
        let frames = (raw.len() / self.raw_frame) as i64;
        let (_, data) = self.delivery.deliver(raw);
        let mut packet = Packet::from_data(0, data);
        packet.pts = Some(self.next_pts);
        packet.duration = frames;
        self.next_pts += frames;
        Ok(packet)
    }
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

/// Bytes before the samples: `RIFF` + size + `WAVE`, a 16-byte `fmt ` chunk,
/// and the `data` chunk header.
const HEADER_LEN: usize = 44;

/// A RIFF size field. Sizes past 4 GiB cannot be stated in 32 bits, so they
/// are written as 0xFFFFFFFF -- the "length unknown" marker FFmpeg's reader
/// and ours take as "to the end of the file" -- rather than modulo 2^32,
/// which made readers stop early and drop the rest of the audio.
fn size32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Where the samples go.
enum Sink {
    /// A pipe or socket: the header's sizes are only known at the end and
    /// cannot be patched in, so the samples are held until the trailer.
    Buffered { out: Output, data: Vec<u8> },
    /// A seekable output (#28): the header goes out first with zero sizes,
    /// the samples stream straight through, and the trailer seeks back to
    /// `start` (where the header begins) to rewrite it with the real sizes.
    Seekable { out: SeekOutput, start: u64 },
}

/// WAV muxer. Both sinks write the same bytes; only the seekable one runs in
/// constant memory.
struct WavMuxer {
    sink: Sink,
    channels: u16,
    sample_rate: u32,
    /// WAVE format tag and bits per sample: `(1, 16)` s16, `(3, 32)` f32.
    tag: u16,
    bits: u16,
    /// Sample bytes written so far.
    data_len: u64,
}

impl WavMuxer {
    fn new(out: Output) -> WavMuxer {
        WavMuxer::with_sink(Sink::Buffered {
            out,
            data: Vec::new(),
        })
    }

    fn new_seekable(out: SeekOutput) -> WavMuxer {
        WavMuxer::with_sink(Sink::Seekable { out, start: 0 })
    }

    fn with_sink(sink: Sink) -> WavMuxer {
        WavMuxer {
            sink,
            channels: 0,
            sample_rate: 0,
            tag: 1,
            bits: 16,
            data_len: 0,
        }
    }

    /// The header for `data_len` sample bytes (the `data` chunk, and so the
    /// RIFF body, is padded to an even length).
    fn header(&self) -> [u8; HEADER_LEN] {
        let block_align = self.channels.wrapping_mul(self.bits / 8);
        let byte_rate = self.sample_rate.wrapping_mul(block_align as u32);
        let pad = self.data_len % 2;
        let mut h = [0u8; HEADER_LEN];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            h[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        put(b"RIFF");
        put(&size32(HEADER_LEN as u64 - 8 + self.data_len + pad).to_le_bytes());
        put(b"WAVE");
        put(b"fmt ");
        put(&16u32.to_le_bytes());
        put(&self.tag.to_le_bytes());
        put(&self.channels.to_le_bytes());
        put(&self.sample_rate.to_le_bytes());
        put(&byte_rate.to_le_bytes());
        put(&block_align.to_le_bytes());
        put(&self.bits.to_le_bytes());
        put(b"data");
        put(&size32(self.data_len).to_le_bytes());
        h
    }
}

impl Muxer for WavMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .first()
            .filter(|s| s.codec_id == CodecId::Pcm)
            .ok_or_else(|| Error::unsupported("wav mux: needs a single `pcm` stream"))?;
        let format = s
            .sample_format
            .ok_or_else(|| Error::invalid("wav mux: stream is missing a sample format"))?;
        (self.tag, self.bits) = match format {
            SampleFormat::S16 => (1, 16),
            SampleFormat::F32 => (3, 32),
            other => {
                return Err(Error::unsupported(format!(
                    "wav mux: sample format `{}` (only s16/f32)",
                    other.name()
                )))
            }
        };
        self.channels = s.channels.max(1);
        self.sample_rate = s.sample_rate.max(1);
        let header = self.header();
        if let Sink::Seekable { out, start } = &mut self.sink {
            *start = out.stream_position()?;
            out.write_all(&header)?;
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        match &mut self.sink {
            Sink::Buffered { data, .. } => data.extend_from_slice(&packet.data),
            Sink::Seekable { out, .. } => out.write_all(&packet.data)?,
        }
        self.data_len += packet.data.len() as u64;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let header = self.header();
        let pad: &[u8] = if self.data_len % 2 == 1 { &[0] } else { &[] };
        match &mut self.sink {
            Sink::Buffered { out, data } => {
                out.write_all(&header)?;
                out.write_all(data)?;
                out.write_all(pad)?;
                out.flush()?;
            }
            Sink::Seekable { out, start } => {
                out.write_all(pad)?;
                let end = out.stream_position()?;
                out.seek(SeekFrom::Start(*start))?;
                out.write_all(&header)?;
                out.seek(SeekFrom::Start(end))?;
                out.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
fn put_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

#[cfg(test)]
mod legacy;

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

    /// The prefix-read demuxer must return exactly what the old whole-buffer
    /// walk did -- first `fmt `, first `data`, `data = file[start..min(start +
    /// size, len)]` -- on every layout, including the ones that must take the
    /// fallback (fmt after data, chunks pushing `data` past the prefix, a
    /// header straddling it) and the ones where the declared size lies
    /// (truncated, streamed 0xFFFFFFFF, trailing chunks, two data chunks).
    #[test]
    fn demux_matches_whole_buffer_walk_on_every_layout() {
        /// The old algorithm, verbatim in effect: the oracle.
        fn reference(file: &[u8]) -> (u16, u32, Vec<u8>) {
            let top = legacy::chunks(file, 12);
            let fmt = top.iter().find(|(id, _)| id == b"fmt ").unwrap().1.clone();
            let data = top.iter().find(|(id, _)| id == b"data").unwrap().1.clone();
            let f = &file[fmt.start..];
            (rd_u16(f, 2), rd_u32(f, 4), file[data].to_vec())
        }
        /// RIFF/WAVE from `(id, payload, declared size override)`.
        fn riff(parts: &[(&[u8; 4], Vec<u8>, Option<u32>)]) -> Vec<u8> {
            let mut body = b"WAVE".to_vec();
            for (id, payload, declared) in parts {
                body.extend_from_slice(*id);
                let size = declared.unwrap_or(payload.len() as u32);
                body.extend_from_slice(&size.to_le_bytes());
                body.extend_from_slice(payload);
                if payload.len() % 2 == 1 {
                    body.push(0);
                }
            }
            let mut file = b"RIFF".to_vec();
            file.extend_from_slice(&(body.len() as u32).to_le_bytes());
            file.extend_from_slice(&body);
            file
        }
        let fmt = fmt_chunk(1, 2, 44_100, 16);
        let pcm = |n: usize| (0..n).map(|i| (i * 7 + 3) as u8).collect::<Vec<u8>>();
        let mut layouts: Vec<(String, Vec<u8>)> = vec![
            (
                "plain".into(),
                riff(&[(b"fmt ", fmt.clone(), None), (b"data", pcm(1000), None)]),
            ),
            (
                "fmt after data".into(),
                riff(&[(b"data", pcm(1000), None), (b"fmt ", fmt.clone(), None)]),
            ),
            (
                "big LIST first".into(),
                riff(&[
                    (b"LIST", pcm(70_000), None),
                    (b"fmt ", fmt.clone(), None),
                    (b"data", pcm(4000), None),
                ]),
            ),
            (
                "odd junk".into(),
                riff(&[
                    (b"fmt ", fmt.clone(), None),
                    (b"junk", pcm(3), None),
                    (b"data", pcm(1000), None),
                ]),
            ),
            (
                "truncated".into(),
                riff(&[
                    (b"fmt ", fmt.clone(), None),
                    (b"data", pcm(1200), Some(5000)),
                ]),
            ),
            (
                "streamed size".into(),
                riff(&[
                    (b"fmt ", fmt.clone(), None),
                    (b"data", pcm(1200), Some(u32::MAX)),
                ]),
            ),
            (
                "trailing LIST".into(),
                riff(&[
                    (b"fmt ", fmt.clone(), None),
                    (b"data", pcm(800), None),
                    (b"LIST", pcm(100), None),
                ]),
            ),
            (
                "two data".into(),
                riff(&[
                    (b"fmt ", fmt.clone(), None),
                    (b"data", pcm(400), None),
                    (b"data", pcm(900), None),
                ]),
            ),
            (
                "data past prefix".into(),
                riff(&[(b"fmt ", fmt.clone(), None), (b"data", pcm(300_000), None)]),
            ),
        ];
        // A data header landing on, across and just after the 64 KiB prefix edge.
        let head = legacy::HEAD_BYTES as usize;
        for pad in (head - 60)..(head - 30) {
            layouts.push((
                format!("edge pad {pad}"),
                riff(&[
                    (b"fmt ", fmt.clone(), None),
                    (b"LIST", pcm(pad), None),
                    (b"data", pcm(500), None),
                ]),
            ));
        }
        for (name, file) in layouts {
            let (ch, rate, want) = reference(&file);
            let mut dem = WavDemuxer::new(Box::new(Cursor::new(file)));
            let streams = dem.read_header().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(streams[0].channels, ch, "{name}");
            assert_eq!(streams[0].sample_rate, rate, "{name}");
            let mut got = Vec::new();
            loop {
                match dem.read_packet() {
                    Ok(p) => got.extend_from_slice(&p.data),
                    Err(Error::Eof) => break,
                    Err(e) => panic!("{name}: {e}"),
                }
            }
            assert_eq!(got, want, "{name}");
        }
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

    // ---- streaming (issue #26): equivalence, header cost, robustness ----

    use std::sync::atomic::{AtomicU64, Ordering};

    /// A reader that counts the bytes pulled from it and hands out at most
    /// `chunk` bytes per `read` (a pipe-like source).
    struct Counting {
        inner: Cursor<Vec<u8>>,
        read: Arc<AtomicU64>,
        chunk: usize,
    }
    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.chunk);
            let got = self.inner.read(&mut buf[..n])?;
            self.read.fetch_add(got as u64, Ordering::Relaxed);
            Ok(got)
        }
    }
    fn counting(data: Vec<u8>, chunk: usize) -> (Input, Arc<AtomicU64>) {
        let read = Arc::new(AtomicU64::new(0));
        let r = Counting {
            inner: Cursor::new(data),
            read: read.clone(),
            chunk,
        };
        (Box::new(r), read)
    }

    /// A demuxer's whole output, with the packets' payloads concatenated:
    /// `(header, payload, per-packet (pts, duration, len), end)`.
    type Run = (String, Vec<u8>, Vec<(Option<i64>, i64, usize)>, String);
    fn drive(mut d: Box<dyn Demuxer>) -> Run {
        let head = match d.read_header() {
            Ok(s) => format!("{s:?}"),
            Err(e) => return (format!("Err({e})"), Vec::new(), Vec::new(), String::new()),
        };
        let (mut payload, mut packets) = (Vec::new(), Vec::new());
        for _ in 0..1_000_000 {
            match d.read_packet() {
                Ok(p) => {
                    assert_eq!(p.stream_index, 0);
                    packets.push((p.pts, p.duration, p.data.len()));
                    payload.extend_from_slice(&p.data);
                }
                Err(e) => {
                    let again = d.read_packet().err().map(|e| e.to_string());
                    assert_eq!(again, Some(e.to_string()), "end not sticky");
                    return (head, payload, packets, e.to_string());
                }
            }
        }
        panic!("demuxer did not terminate");
    }

    /// The streaming demuxer must report the same stream, the same samples
    /// (concatenated) and the same end as the whole-chunk one, in packets of
    /// whole frames whose pts/duration tile the timeline from 0.
    fn assert_same_as_legacy(name: &str, file: &[u8], chunks: &[usize]) {
        let old = drive(Box::new(legacy::LegacyWavDemuxer::new(Box::new(
            Cursor::new(file.to_vec()),
        ))));
        for &chunk in chunks {
            let (input, _) = counting(file.to_vec(), chunk);
            let new = drive(Box::new(WavDemuxer::new(input)));
            assert_eq!(new.0, old.0, "{name}: header (chunk {chunk})");
            assert!(new.1 == old.1, "{name}: payload differs (chunk {chunk})");
            assert_eq!(new.3, old.3, "{name}: end (chunk {chunk})");
            if old.2.is_empty() {
                assert!(new.2.is_empty(), "{name}");
                continue;
            }
            // Timing: contiguous, in samples, whole frames per packet.
            let mut dem = WavDemuxer::new(Box::new(Cursor::new(file.to_vec())));
            let s = &dem.read_header().unwrap()[0];
            let out_frame =
                s.sample_format.unwrap().bytes_per_sample() * s.channels.max(1) as usize;
            let mut t = 0;
            for (i, &(pts, dur, len)) in new.2.iter().enumerate() {
                assert_eq!(pts, Some(t), "{name}: packet {i} pts");
                assert_eq!(dur, (len / out_frame) as i64, "{name}: packet {i} duration");
                if i + 1 < new.2.len() {
                    assert_eq!(len % out_frame, 0, "{name}: packet {i} splits a frame");
                    assert!(len > 0, "{name}: empty packet {i} mid-stream");
                }
                t += dur;
            }
            assert_eq!(
                t,
                (old.1.len() / out_frame) as i64,
                "{name}: total duration"
            );
        }
    }

    /// RIFF/WAVE from `(id, payload, declared size override)`.
    fn riff(parts: &[(&[u8; 4], Vec<u8>, Option<u32>)]) -> Vec<u8> {
        let mut body = b"WAVE".to_vec();
        for (id, payload, declared) in parts {
            body.extend_from_slice(*id);
            let size = declared.unwrap_or(payload.len() as u32);
            body.extend_from_slice(&size.to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);
        file
    }
    fn pcm(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn streaming_matches_whole_chunk_demuxer() {
        let s16 = fmt_chunk(1, 2, 44_100, 16);
        let mut ext = fmt_chunk(0xFFFE, 2, 48_000, 24);
        ext.extend_from_slice(&22u16.to_le_bytes());
        ext.extend_from_slice(&24u16.to_le_bytes());
        ext.extend_from_slice(&3u32.to_le_bytes());
        ext.extend_from_slice(&1u16.to_le_bytes());
        ext.extend_from_slice(&[0u8; 14]);
        let f = |fmt: &Vec<u8>, n: usize| {
            riff(&[(b"fmt ", fmt.clone(), None), (b"data", pcm(n), None)])
        };
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("s16 many packets", f(&s16, 4 * 10_000)),
            ("s16 partial frame", f(&s16, 4 * 9_000 + 3)),
            ("s16 exact packets", f(&s16, 4 * 4096 * 3)),
            ("u8 mono odd", f(&fmt_chunk(1, 1, 8_000, 8), 12_345)),
            (
                "s24 stereo partial",
                f(&fmt_chunk(1, 2, 48_000, 24), 6 * 5000 + 4),
            ),
            ("f32 6ch", f(&fmt_chunk(3, 6, 48_000, 32), 24 * 7000)),
            ("extensible s24", f(&ext, 6 * 4500)),
            ("zero channels", f(&fmt_chunk(1, 0, 8_000, 16), 9000)),
            ("empty data", f(&s16, 0)),
            (
                "fmt after data",
                riff(&[(b"data", pcm(30_000), None), (b"fmt ", s16.clone(), None)]),
            ),
            (
                "empty data before fmt",
                riff(&[(b"data", Vec::new(), None), (b"fmt ", s16.clone(), None)]),
            ),
            (
                "big LIST first",
                riff(&[
                    (b"LIST", pcm(70_000), None),
                    (b"fmt ", s16.clone(), None),
                    (b"data", pcm(40_000), None),
                ]),
            ),
            (
                "odd junk",
                riff(&[
                    (b"fmt ", s16.clone(), None),
                    (b"junk", pcm(3), None),
                    (b"data", pcm(1000), None),
                ]),
            ),
            (
                "truncated",
                riff(&[
                    (b"fmt ", s16.clone(), None),
                    (b"data", pcm(1200), Some(50_000)),
                ]),
            ),
            (
                "streamed size",
                riff(&[
                    (b"fmt ", s16.clone(), None),
                    (b"data", pcm(30_000), Some(u32::MAX)),
                ]),
            ),
            (
                "trailing LIST",
                riff(&[
                    (b"fmt ", s16.clone(), None),
                    (b"data", pcm(20_000), None),
                    (b"LIST", pcm(100), None),
                ]),
            ),
            (
                "two data",
                riff(&[
                    (b"fmt ", s16.clone(), None),
                    (b"data", pcm(400), None),
                    (b"data", pcm(900), None),
                ]),
            ),
            (
                "two fmt",
                riff(&[
                    (b"fmt ", s16.clone(), None),
                    (b"fmt ", fmt_chunk(3, 1, 8000, 32), None),
                    (b"data", pcm(400), None),
                ]),
            ),
            (
                "huge fmt",
                riff(&[
                    (b"fmt ", s16.clone(), Some(0xFFFF_FF00)),
                    (b"data", pcm(400), None),
                ]),
            ),
            ("no fmt", riff(&[(b"data", pcm(400), None)])),
            ("no data", riff(&[(b"fmt ", s16.clone(), None)])),
            (
                "short fmt",
                riff(&[(b"fmt ", vec![1, 0, 2, 0], None), (b"data", pcm(8), None)]),
            ),
            ("bad tag", f(&fmt_chunk(2, 2, 44_100, 4), 100)),
            ("not riff", b"RIFX\0\0\0\0WAVEfmt ".to_vec()),
            ("empty", Vec::new()),
        ];
        for (name, file) in &cases {
            assert_same_as_legacy(name, file, &[1, 7, usize::MAX]);
        }
        // Real files, when the checkout has them.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for rel in [
            "corpus/mus_piano.wav",
            "corpus/corp_st_mus_vocal.wav",
            "corpus/corp_long_mus_guitar.wav",
            "PEAQ_python/test_clean.wav",
        ] {
            if let Ok(file) = std::fs::read(root.join(rel)) {
                assert_same_as_legacy(rel, &file, &[4093, usize::MAX]);
            }
        }
    }

    /// A `data` size of 0 means "unknown length" (streamed WAV), like
    /// 0xFFFFFFFF: the samples run to the end of the input (FFmpeg does the
    /// same). The whole-chunk demuxer returned nothing here.
    #[test]
    fn zero_data_size_reads_to_eof() {
        let file = riff(&[
            (b"fmt ", fmt_chunk(1, 2, 44_100, 16), None),
            (b"data", pcm(20_000), Some(0)),
        ]);
        let (_, payload, packets, end) =
            drive(Box::new(WavDemuxer::new(Box::new(Cursor::new(file)))));
        assert_eq!(payload, pcm(20_000));
        assert_eq!(packets.len(), 2);
        assert_eq!(end, "end of stream");
    }

    #[test]
    fn read_header_reads_only_the_header() {
        // ~4 MB of stereo s16 behind a LIST chunk.
        let file = riff(&[
            (b"fmt ", fmt_chunk(1, 2, 44_100, 16), None),
            (b"LIST", pcm(500), None),
            (b"data", pcm(4 << 20), None),
        ]);
        let (input, read) = counting(file, usize::MAX);
        let mut dem = WavDemuxer::new(input);
        dem.read_header().unwrap();
        assert!(
            read.load(Ordering::Relaxed) < 64 * 1024,
            "header read too much"
        );
        // A packet pulls about one packet's worth (4096 frames = 16 KiB).
        assert_eq!(dem.read_packet().unwrap().data.len(), 4096 * 4);
        assert!(read.load(Ordering::Relaxed) < 64 * 1024);
    }

    #[test]
    fn truncated_and_garbage_inputs_fail_cleanly() {
        // Chunks declaring 4 GB over a tiny input: no 4 GB allocation, an end.
        for parts in [
            vec![
                (b"fmt ", fmt_chunk(1, 2, 44_100, 16), None),
                (b"data", pcm(10), Some(u32::MAX - 1)),
            ],
            vec![
                (b"data", pcm(10), Some(u32::MAX - 1)),
                (b"fmt ", fmt_chunk(1, 2, 44_100, 16), None),
            ],
            vec![
                (b"LIST", pcm(10), Some(u32::MAX - 1)),
                (b"fmt ", fmt_chunk(1, 2, 44_100, 16), None),
            ],
        ] {
            let _ = drive(Box::new(WavDemuxer::new(Box::new(Cursor::new(riff(
                &parts,
            ))))));
        }
        // Every truncation of a valid file, and garbage behind a RIFF header:
        // never a panic, always an end.
        let good = riff(&[
            (b"fmt ", fmt_chunk(1, 2, 44_100, 16), None),
            (b"LIST", pcm(5), None),
            (b"data", pcm(64), None),
        ]);
        for cut in 0..good.len() {
            let _ = drive(Box::new(WavDemuxer::new(Box::new(Cursor::new(
                good[..cut].to_vec(),
            )))));
        }
        let mut x = 0x2545_F491_u32;
        for _ in 0..64 {
            let mut g = good[..12].to_vec();
            for _ in 0..300 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                g.push(x as u8);
            }
            let _ = drive(Box::new(WavDemuxer::new(Box::new(Cursor::new(g)))));
        }
        // Reading packets before the header is just end-of-stream.
        let mut dem = WavDemuxer::new(Box::new(Cursor::new(good)));
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    // ---- streaming mux (issue #28): byte identity, sizes past 4 GiB ----

    use std::collections::BTreeMap;
    use std::io::{Seek, SeekFrom};

    /// A seekable `Cursor<Vec<u8>>` sink whose bytes outlive the muxer.
    #[derive(Clone)]
    struct SharedCursor(Arc<Mutex<Cursor<Vec<u8>>>>);
    impl Write for SharedCursor {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for SharedCursor {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(pos)
        }
    }

    fn pcm_stream(format: SampleFormat, channels: u16, rate: u32) -> Stream {
        let mut s = Stream::new(0, CodecId::Pcm);
        s.channels = channels;
        s.sample_rate = rate;
        s.sample_format = Some(format);
        s
    }

    fn drive_mux(mut m: Box<dyn Muxer>, s: &Stream, packets: &[Packet]) -> Result<()> {
        m.write_header(std::slice::from_ref(s))?;
        for p in packets {
            m.write_packet(p)?;
        }
        m.write_trailer()
    }

    fn mux_legacy(s: &Stream, packets: &[Packet]) -> Result<Vec<u8>> {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let m = legacy::LegacyWavMuxer::new(Box::new(sink.clone()));
        drive_mux(Box::new(m), s, packets)?;
        let out = sink.0.lock().unwrap().clone();
        Ok(out)
    }

    fn registry() -> FormatRegistry {
        let mut reg = FormatRegistry::new();
        register(&mut reg);
        reg
    }

    /// The registered plain factory, over a non-seekable sink (a pipe).
    fn mux_plain(s: &Stream, packets: &[Packet]) -> Result<Vec<u8>> {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let m = registry().open_muxer("wav", Box::new(sink.clone()))?;
        drive_mux(m, s, packets)?;
        let out = sink.0.lock().unwrap().clone();
        Ok(out)
    }

    /// The registered seekable factory, over a `Cursor<Vec<u8>>` already
    /// holding `prefix` (the header patch must land where the muxer began).
    /// Returns the bytes after the prefix.
    fn mux_seekable(s: &Stream, packets: &[Packet], prefix: &[u8]) -> Result<Vec<u8>> {
        let mut cur = Cursor::new(prefix.to_vec());
        cur.seek(SeekFrom::End(0)).unwrap();
        let sink = SharedCursor(Arc::new(Mutex::new(cur)));
        let m = registry().open_muxer_seekable("wav", Box::new(sink.clone()))?;
        drive_mux(m, s, packets)?;
        let cur = sink.0.lock().unwrap();
        assert_eq!(cur.position(), cur.get_ref().len() as u64, "not at the end");
        assert_eq!(&cur.get_ref()[..prefix.len()], prefix, "prefix clobbered");
        Ok(cur.get_ref()[prefix.len()..].to_vec())
    }

    /// Both muxer paths write exactly the legacy (whole-file) muxer's bytes:
    /// s16/f32, mono to 8 channels, odd and empty packets, no packets.
    #[test]
    fn both_mux_paths_are_byte_identical_to_the_legacy_muxer() {
        let streams = [
            pcm_stream(SampleFormat::S16, 1, 22_050),
            pcm_stream(SampleFormat::S16, 2, 44_100),
            pcm_stream(SampleFormat::F32, 2, 48_000),
            pcm_stream(SampleFormat::F32, 6, 96_000),
            pcm_stream(SampleFormat::S16, 8, 48_000),
            pcm_stream(SampleFormat::S16, 0, 0), // clamped to 1 ch / 1 Hz
        ];
        let sizes: [&[usize]; 5] = [
            &[],
            &[0],
            &[4096 * 4, 4096 * 4, 1000],
            &[3, 0, 8, 5, 1],
            &[100_001, 77, 65_536],
        ];
        for s in &streams {
            for sz in sizes {
                let packets: Vec<Packet> = sz
                    .iter()
                    .enumerate()
                    .map(|(i, &n)| Packet::from_data(0, pcm(n + i)[i..].to_vec()))
                    .collect();
                let name = format!("{:?} {}ch {:?}", s.sample_format, s.channels, sz);
                let want = mux_legacy(s, &packets).unwrap();
                assert!(mux_plain(s, &packets).unwrap() == want, "{name}: plain");
                assert!(
                    mux_seekable(s, &packets, &[]).unwrap() == want,
                    "{name}: seekable"
                );
                assert!(
                    mux_seekable(s, &packets, b"xyz").unwrap() == want,
                    "{name}: seekable after a prefix"
                );
            }
        }
    }

    #[test]
    fn unsupported_sample_format_fails_at_header() {
        let s = pcm_stream(SampleFormat::F32Planar, 2, 44_100);
        assert!(mux_legacy(&s, &[]).is_err()); // the legacy muxer: at the trailer
        let reg = registry();
        let mut m = reg.open_muxer("wav", Box::new(Vec::new())).unwrap();
        assert!(m.write_header(std::slice::from_ref(&s)).is_err());
        let cur: SeekOutput = Box::new(Cursor::new(Vec::new()));
        let mut m = reg.open_muxer_seekable("wav", cur).unwrap();
        assert!(m.write_header(std::slice::from_ref(&s)).is_err());
        let mut no_format = s.clone();
        no_format.sample_format = None;
        assert!(m.write_header(&[no_format]).is_err());
    }

    /// `(position, length, small writes by offset)` of a [`Sparse`] sink.
    type SparseState = (u64, u64, BTreeMap<u64, Vec<u8>>);

    /// A seekable sink that keeps the bytes of small writes only and just
    /// advances over big ones, so a >4 GiB file can be "written" in a test.
    #[derive(Clone, Default)]
    struct Sparse(Arc<Mutex<SparseState>>);
    impl Write for Sparse {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut g = self.0.lock().unwrap();
            let (pos, len, kept) = &mut *g;
            if buf.len() <= 64 {
                kept.insert(*pos, buf.to_vec());
            }
            *pos += buf.len() as u64;
            *len = (*len).max(*pos);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Sparse {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            let mut g = self.0.lock().unwrap();
            g.0 = match to {
                SeekFrom::Start(p) => p,
                SeekFrom::End(d) => (g.1 as i64 + d) as u64,
                SeekFrom::Current(d) => (g.0 as i64 + d) as u64,
            };
            Ok(g.0)
        }
    }

    /// Stream `data_len` sample bytes through the seekable muxer into a
    /// sparse sink; returns `(final header, file length, final position)`.
    fn mux_sparse(data_len: u64) -> (Vec<u8>, u64, u64) {
        let s = pcm_stream(SampleFormat::S16, 2, 48_000);
        let sink = Sparse::default();
        let mut m = registry()
            .open_muxer_seekable("wav", Box::new(sink.clone()))
            .unwrap();
        m.write_header(std::slice::from_ref(&s)).unwrap();
        let big = Packet::from_data(0, vec![0; 64 << 20]);
        let mut left = data_len;
        while left >= big.data.len() as u64 {
            m.write_packet(&big).unwrap();
            left -= big.data.len() as u64;
        }
        m.write_packet(&Packet::from_data(0, vec![0; left as usize]))
            .unwrap();
        m.write_trailer().unwrap();
        let g = sink.0.lock().unwrap();
        // The last write at offset 0 is the rewritten header.
        (g.2[&0].clone(), g.1, g.0)
    }

    /// Up to 4 GiB the sizes are exact; past it, a size that does not fit in
    /// 32 bits is written as 0xFFFFFFFF ("to end of file"), not modulo 2^32.
    #[test]
    fn sizes_near_and_past_4_gib() {
        // Everything fits: RIFF = 36 + data (even).
        let data = u32::MAX as u64 - 37;
        let (h, len, pos) = mux_sparse(data);
        assert_eq!((len, pos), (44 + data, 44 + data));
        assert_eq!(rd_u32(&h, 4), (36 + data) as u32);
        assert_eq!(rd_u32(&h, 40), data as u32);
        // Odd data that fits; the padded RIFF body does not.
        let data = u32::MAX as u64 - 36;
        let (h, len, _) = mux_sparse(data);
        assert_eq!(len, 44 + data + 1);
        assert_eq!(rd_u32(&h, 4), u32::MAX);
        assert_eq!(rd_u32(&h, 40), data as u32);
        // Past 4 GiB of samples: both sizes unknown.
        let data = (1u64 << 32) + 4;
        let (h, len, pos) = mux_sparse(data);
        assert_eq!((len, pos), (44 + data, 44 + data));
        assert_eq!(rd_u32(&h, 4), u32::MAX);
        assert_eq!(rd_u32(&h, 40), u32::MAX);
        assert_eq!(&h[36..40], b"data");
    }
}
