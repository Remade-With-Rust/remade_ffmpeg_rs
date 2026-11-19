//! Core Audio Format (CAF) container (demux).
//!
//! A CAF file is `caff`, a version and flags, then chunks of a four-character
//! type and a signed 64-bit size (big-endian throughout). `desc` describes the
//! audio, `kuki` carries the codec's magic cookie, `pakt` lists the size (and
//! for some codecs the duration) of each variable-size packet, and `data`
//! holds the packets after a 4-byte edit count. A `data` size of -1 means "to
//! the end of the file".
//!
//! Formats read:
//!
//! | `desc` format | delivered as |
//! |---|---|
//! | `lpcm` (int 8-32 bit or float 32/64, either byte order) | `pcm`, byte-swapped to little-endian (8-bit widened to s16) |
//! | `alac` | `alac`, one packet per ALAC frame, the cookie as extradata |
//! | `aac ` | `aac`, one packet per AAC frame, the AudioSpecificConfig (from the cookie's ES descriptor) as extradata |
//! | `ima4` | `adpcm_ima_qt`, whole 34-byte-per-channel frames |
//! | `alaw` / `ulaw` | `pcm_alaw` / `pcm_mulaw` |
//!
//! Like the WAV and AIFF demuxers this streams when it can: with the usual
//! layout (`pakt` before `data`, or a constant packet size) `read_header`
//! stops at the samples and `read_packet` reads one packet at a time. A
//! variable-size `data` chunk that precedes its `pakt` is buffered, since a
//! pipe cannot come back for it.
//!
//! Damaged files read as far as they go: a packet table that runs past the
//! data stops at the data's end, a short final packet is dropped, and an
//! unknown chunk is skipped.

use std::io::{BufReader, Read};

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Stream};

/// Register the CAF format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "caf",
        long_name: "CAF (Apple Core Audio Format)",
        extensions: &["caf"],
        demuxer: Some(|input| Box::new(CafDemuxer::new(input))),
        muxer: None,
        muxer_path: None,
        probe: Some(probe_caf),
        mux_caps: MuxCaps::NONE,
    });
}

/// Sniff CAF: `caff` and version 1.
fn probe_caf(data: &[u8]) -> i32 {
    if data.len() >= 8 && &data[0..4] == b"caff" && data[4..6] == [0, 1] {
        100
    } else {
        0
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The `desc` chunk.
#[derive(Clone, Copy, Debug)]
struct Desc {
    sample_rate: f64,
    format: [u8; 4],
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    channels: u32,
    bits: u32,
}

fn parse_desc(b: &[u8]) -> Result<Desc> {
    if b.len() < 32 {
        return Err(Error::invalid("caf demux: short desc chunk"));
    }
    let d = Desc {
        sample_rate: f64::from_be_bytes(b[0..8].try_into().unwrap()),
        format: b[8..12].try_into().unwrap(),
        flags: be32(b, 12),
        bytes_per_packet: be32(b, 16),
        frames_per_packet: be32(b, 20),
        channels: be32(b, 24),
        bits: be32(b, 28),
    };
    if !(d.sample_rate >= 1.0 && d.sample_rate <= i32::MAX as f64) {
        return Err(Error::invalid(format!(
            "caf demux: sample rate {}",
            d.sample_rate
        )));
    }
    if d.channels == 0 || d.channels > 64 {
        return Err(Error::invalid(format!(
            "caf demux: {} channels",
            d.channels
        )));
    }
    Ok(d)
}

/// How packets are cut and delivered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plan {
    /// Linear PCM: `bytes` per sample; delivered little-endian as `format`
    /// (8-bit widened to s16).
    Pcm {
        bytes: usize,
        little_endian: bool,
        format: SampleFormat,
    },
    /// Constant-size packets passed through (G.711, ima4).
    Constant(CodecId),
    /// Variable-size packets from the packet table (ALAC, AAC).
    Variable(CodecId),
}

fn plan_for(d: &Desc) -> Result<Plan> {
    let ch = d.channels as usize;
    match &d.format {
        b"lpcm" => {
            let float = d.flags & 1 != 0;
            let little_endian = d.flags & 2 != 0;
            let bytes = d.bits.div_ceil(8) as usize;
            let format = match (float, bytes) {
                (false, 1 | 2) => SampleFormat::S16,
                (false, 3) => SampleFormat::S24,
                (false, 4) => SampleFormat::S32,
                (true, 4) => SampleFormat::F32,
                (true, 8) => SampleFormat::F64,
                _ => {
                    return Err(Error::unsupported(format!(
                        "caf demux: lpcm, {} bits{}",
                        d.bits,
                        if float { " float" } else { "" }
                    )))
                }
            };
            if d.bytes_per_packet != 0 && d.bytes_per_packet as usize != bytes * ch {
                return Err(Error::unsupported(
                    "caf demux: lpcm with padded or non-interleaved packets",
                ));
            }
            Ok(Plan::Pcm {
                bytes,
                little_endian,
                format,
            })
        }
        b"alaw" => Ok(Plan::Constant(CodecId::PcmAlaw)),
        b"ulaw" => Ok(Plan::Constant(CodecId::PcmMulaw)),
        b"ima4" => Ok(Plan::Constant(CodecId::AdpcmImaQt)),
        b"alac" => Ok(Plan::Variable(CodecId::Alac)),
        b"aac " => Ok(Plan::Variable(CodecId::Aac)),
        other => Err(Error::unsupported(format!(
            "caf demux: format `{}`",
            String::from_utf8_lossy(other)
        ))),
    }
}

impl Plan {
    fn codec(self) -> CodecId {
        match self {
            Plan::Pcm { .. } => CodecId::Pcm,
            Plan::Constant(c) | Plan::Variable(c) => c,
        }
    }

    fn sample_format(self) -> SampleFormat {
        match self {
            Plan::Pcm { format, .. } => format,
            // What the decoders produce for these (ALAC deeper than 16 bits
            // and AAC produce f32; the stream reports the common case).
            Plan::Constant(_) => SampleFormat::S16,
            Plan::Variable(CodecId::Alac) => SampleFormat::S16,
            Plan::Variable(_) => SampleFormat::F32,
        }
    }

    fn deliver(self, raw: Vec<u8>) -> Vec<u8> {
        match self {
            Plan::Pcm { bytes: 1, .. } => raw
                .iter()
                .flat_map(|&b| ((b as i8 as i16) << 8).to_le_bytes())
                .collect(),
            Plan::Pcm {
                bytes,
                little_endian: false,
                ..
            } => {
                let mut out = raw;
                for s in out.chunks_exact_mut(bytes) {
                    s.reverse();
                }
                out
            }
            _ => raw,
        }
    }
}

/// Read one BER-style variable-length integer (7 bits per byte, high bit =
/// more follow) from `b` at `*at`.
fn varint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    for _ in 0..10 {
        let byte = *b.get(*at)?;
        *at += 1;
        v = (v << 7) | (byte & 0x7F) as u64;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The packet table: each packet's byte size and sample frames.
#[derive(Default, Debug)]
struct PacketTable {
    entries: Vec<(u64, u64)>,
    valid_frames: Option<u64>,
    priming: u32,
}

fn parse_pakt(b: &[u8], d: &Desc) -> Result<PacketTable> {
    if b.len() < 24 {
        return Err(Error::invalid("caf demux: short pakt chunk"));
    }
    let count = i64::from_be_bytes(b[0..8].try_into().unwrap()).max(0) as u64;
    let valid = i64::from_be_bytes(b[8..16].try_into().unwrap());
    let priming = i32::from_be_bytes(b[16..20].try_into().unwrap()).max(0) as u32;
    let mut at = 24;
    let mut entries = Vec::new();
    // With both sizes constant the table holds no per-packet data, and its
    // count must not drive a loop (a corrupt count would allocate without
    // bound): the packets are cut at the constant size instead.
    let count = if d.bytes_per_packet != 0 && d.frames_per_packet != 0 {
        0
    } else {
        count
    };
    entries.reserve(count.min((b.len() as u64 - 24).max(1)) as usize);
    for _ in 0..count {
        let size = if d.bytes_per_packet == 0 {
            match varint(b, &mut at) {
                Some(v) => v,
                None => break,
            }
        } else {
            d.bytes_per_packet as u64
        };
        let frames = if d.frames_per_packet == 0 {
            match varint(b, &mut at) {
                Some(v) => v,
                None => break,
            }
        } else {
            d.frames_per_packet as u64
        };
        entries.push((size, frames));
    }
    Ok(PacketTable {
        entries,
        valid_frames: (valid >= 0).then_some(valid as u64),
        priming,
    })
}

/// The AudioSpecificConfig inside an AAC magic cookie: an MPEG-4 ES_Descriptor
/// (tag 3) holding a DecoderConfigDescriptor (tag 4) holding the
/// DecoderSpecificInfo (tag 5). Some writers wrap it in an `esds`-style
/// version/flags word first; a cookie that is not a descriptor is taken as
/// the config itself.
fn aac_config(cookie: &[u8]) -> Vec<u8> {
    fn descriptor(b: &[u8], at: &mut usize) -> Option<(u8, usize)> {
        let tag = *b.get(*at)?;
        *at += 1;
        let mut len = 0usize;
        for _ in 0..4 {
            let byte = *b.get(*at)?;
            *at += 1;
            len = (len << 7) | (byte & 0x7F) as usize;
            if byte & 0x80 == 0 {
                break;
            }
        }
        Some((tag, len))
    }
    let find = |start: usize| -> Option<Vec<u8>> {
        let mut at = start;
        let (tag, _) = descriptor(cookie, &mut at)?;
        if tag != 3 {
            return None;
        }
        at += 2; // ES_ID
        let flags = *cookie.get(at)?;
        at += 1;
        if flags & 0x80 != 0 {
            at += 2; // dependsOn_ES_ID
        }
        if flags & 0x40 != 0 {
            let n = *cookie.get(at)? as usize;
            at += 1 + n; // URL
        }
        if flags & 0x20 != 0 {
            at += 2; // OCR_ES_ID
        }
        let (tag, _) = descriptor(cookie, &mut at)?;
        if tag != 4 {
            return None;
        }
        at += 13; // objectType, streamType, bufferSize, max/avg bitrate
        let (tag, len) = descriptor(cookie, &mut at)?;
        if tag != 5 {
            return None;
        }
        cookie.get(at..at + len).map(<[u8]>::to_vec)
    };
    find(0)
        .or_else(|| find(4))
        .unwrap_or_else(|| cookie.to_vec())
}

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

/// Read up to `len` bytes, growing only as bytes arrive.
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
            break;
        }
    }
    Ok(v)
}

fn skip(r: &mut impl Read, n: u64) -> std::io::Result<()> {
    std::io::copy(&mut r.by_ref().take(n), &mut std::io::sink())?;
    Ok(())
}

/// Where packet bytes come from.
enum Source {
    Stream {
        reader: BufReader<Input>,
        remaining: u64,
    },
    Buffered {
        data: Vec<u8>,
        pos: usize,
    },
}

impl Source {
    /// Up to `n` bytes (short only at the end of the data).
    fn take(&mut self, n: usize) -> Result<Vec<u8>> {
        Ok(match self {
            Source::Stream { reader, remaining } => {
                let got = read_up_to(reader, (*remaining).min(n as u64))?;
                *remaining -= got.len() as u64;
                got
            }
            Source::Buffered { data, pos } => {
                let end = (*pos + n).min(data.len());
                let v = data[*pos..end].to_vec();
                *pos = end;
                v
            }
        })
    }
}

/// Small chunks are kept whole; this caps `desc`/`kuki` reads.
const META_KEEP: u64 = 1 << 20;
/// A packet table larger than this is refused (64 M packets).
const PAKT_MAX: u64 = 1 << 30;

struct Header {
    desc: Desc,
    plan: Plan,
    cookie: Vec<u8>,
    table: Option<PacketTable>,
}

fn open_caf(input: Input) -> Result<(Header, Source)> {
    let mut r = BufReader::new(input);
    let mut head = [0u8; 8];
    if read_full(&mut r, &mut head)? < 8 || probe_caf(&head) == 0 {
        return Err(Error::invalid("caf demux: not a CAF file"));
    }
    let mut desc: Option<Desc> = None;
    let mut cookie = Vec::new();
    let mut table: Option<PacketTable> = None;
    let mut early: Option<Vec<u8>> = None;
    loop {
        let mut ch = [0u8; 12];
        if read_full(&mut r, &mut ch)? < 12 {
            break;
        }
        let size = i64::from_be_bytes(ch[4..12].try_into().unwrap());
        match &ch[0..4] {
            b"desc" if desc.is_none() => {
                let n = size.max(0) as u64;
                let body = read_up_to(&mut r, n.min(META_KEEP))?;
                desc = Some(parse_desc(&body)?);
                skip(&mut r, n - body.len() as u64)?;
            }
            b"kuki" => {
                let n = size.max(0) as u64;
                cookie = read_up_to(&mut r, n.min(META_KEEP))?;
                skip(&mut r, n - cookie.len() as u64)?;
            }
            b"pakt" if table.is_none() => {
                let n = size.max(0) as u64;
                if n > PAKT_MAX {
                    return Err(Error::invalid("caf demux: packet table too large"));
                }
                let d = desc.ok_or_else(|| Error::invalid("caf demux: pakt before desc"))?;
                let body = read_up_to(&mut r, n)?;
                table = Some(parse_pakt(&body, &d)?);
            }
            b"data" if early.is_none() => {
                // -1: to the end of the file. The 4-byte edit count is not audio.
                let n = if size < 0 { u64::MAX } else { size as u64 };
                let mut edit = [0u8; 4];
                if n < 4 || read_full(&mut r, &mut edit)? < 4 {
                    break;
                }
                let remaining = n.saturating_sub(4);
                let d = desc.ok_or_else(|| Error::invalid("caf demux: data before desc"))?;
                let plan = plan_for(&d)?;
                let can_stream = !matches!(plan, Plan::Variable(_)) || table.is_some();
                if can_stream {
                    return Ok((
                        Header {
                            desc: d,
                            plan,
                            cookie,
                            table,
                        },
                        Source::Stream {
                            reader: r,
                            remaining,
                        },
                    ));
                }
                early = Some(read_up_to(&mut r, remaining)?);
                if size < 0 {
                    break; // "to the end": nothing can follow
                }
            }
            _ => {
                if size < 0 {
                    break;
                }
                skip(&mut r, size as u64)?;
            }
        }
    }
    let desc = desc.ok_or_else(|| Error::invalid("caf demux: no desc chunk"))?;
    let data = early.ok_or_else(|| Error::invalid("caf demux: no data chunk"))?;
    let plan = plan_for(&desc)?;
    if matches!(plan, Plan::Variable(_)) && table.is_none() {
        return Err(Error::invalid(
            "caf demux: variable-size packets without a pakt chunk",
        ));
    }
    Ok((
        Header {
            desc,
            plan,
            cookie,
            table,
        },
        Source::Buffered { data, pos: 0 },
    ))
}

/// PCM / G.711 sample frames per packet, and `ima4` frames per packet.
const FRAMES_PER_PACKET: usize = 4096;
const IMA4_PER_PACKET: usize = 64;
const MAX_PACKET_BYTES: usize = 1 << 20;

struct CafDemuxer {
    input: Option<Input>,
    source: Option<Source>,
    plan: Plan,
    /// Constant layouts: file bytes per unit and samples per unit, units per packet.
    unit_bytes: usize,
    unit_samples: usize,
    units_per_packet: usize,
    /// Variable layouts: the packet table and the next entry.
    table: Vec<(u64, u64)>,
    next: usize,
    /// A variable-format stream whose `desc` gives constant sizes after all:
    /// every packet is this (bytes, frames).
    fixed_packet: Option<(u64, u64)>,
    next_pts: i64,
}

impl CafDemuxer {
    fn new(input: Input) -> CafDemuxer {
        CafDemuxer {
            input: Some(input),
            source: None,
            plan: Plan::Constant(CodecId::None),
            unit_bytes: 1,
            unit_samples: 1,
            units_per_packet: 1,
            table: Vec::new(),
            next: 0,
            fixed_packet: None,
            next_pts: 0,
        }
    }
}

impl Demuxer for CafDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("caf demux: header already read"))?;
        let (h, source) = open_caf(input)?;
        let ch = h.desc.channels as usize;
        self.plan = h.plan;
        let rate = h.desc.sample_rate.round() as u32;
        let mut stream = Stream::new(0, h.plan.codec());
        stream.sample_rate = rate;
        stream.channels = h.desc.channels as u16;
        // ALAC deeper than 16 bits decodes to f32 (`desc` flags 1..=4 are
        // 16/20/24/32-bit).
        stream.sample_format = Some(match h.plan {
            Plan::Variable(CodecId::Alac) if h.desc.flags != 1 => SampleFormat::F32,
            plan => plan.sample_format(),
        });
        stream.time_base = Rational::new(1, rate.max(1) as i32);
        match h.plan {
            Plan::Pcm { bytes, .. } => {
                self.unit_bytes = bytes * ch;
                self.unit_samples = 1;
                self.units_per_packet = FRAMES_PER_PACKET;
            }
            Plan::Constant(codec) => {
                let (bytes, samples, per) = if codec == CodecId::AdpcmImaQt {
                    (34 * ch, 64, IMA4_PER_PACKET)
                } else {
                    (ch, 1, FRAMES_PER_PACKET)
                };
                self.unit_bytes = bytes;
                self.unit_samples = samples;
                self.units_per_packet = per;
            }
            Plan::Variable(codec) => {
                let table = h.table.unwrap_or_default();
                stream.extradata = if codec == CodecId::Aac {
                    aac_config(&h.cookie)
                } else {
                    h.cookie.clone()
                };
                if let Some(v) = table.valid_frames {
                    stream.duration = Some(v as i64);
                    stream.nb_frames = Some(table.entries.len() as u64);
                }
                if codec == CodecId::Aac && table.priming > 0 && rate > 0 {
                    stream.codec_delay_ns = table.priming as u64 * 1_000_000_000 / rate as u64;
                }
                self.table = table.entries;
                if h.desc.bytes_per_packet != 0 && h.desc.frames_per_packet != 0 {
                    self.fixed_packet = Some((
                        h.desc.bytes_per_packet as u64,
                        h.desc.frames_per_packet as u64,
                    ));
                }
            }
        }
        self.units_per_packet = self
            .units_per_packet
            .min((MAX_PACKET_BYTES / self.unit_bytes.max(1)).max(1));
        self.source = Some(source);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let Some(source) = self.source.as_mut() else {
            return Err(Error::Eof);
        };
        let (data, frames) = if let Plan::Variable(_) = self.plan {
            let Some((size, frames)) = self.table.get(self.next).copied().or(self.fixed_packet)
            else {
                self.source = None;
                return Err(Error::Eof);
            };
            self.next += 1;
            if size > MAX_PACKET_BYTES as u64 * 16 {
                self.source = None;
                return Err(Error::invalid(
                    "caf demux: packet size past any codec's frame",
                ));
            }
            let raw = source.take(size as usize)?;
            if (raw.len() as u64) < size {
                self.source = None; // a short final packet is dropped
                return Err(Error::Eof);
            }
            (raw, frames as i64)
        } else {
            let want = self.unit_bytes * self.units_per_packet;
            let mut raw = source.take(want)?;
            let whole = raw.len() / self.unit_bytes * self.unit_bytes;
            if whole == 0 {
                self.source = None;
                return Err(Error::Eof);
            }
            raw.truncate(whole);
            let frames = (whole / self.unit_bytes * self.unit_samples) as i64;
            (self.plan.deliver(raw), frames)
        };
        let mut packet = Packet::from_data(0, data);
        packet.pts = Some(self.next_pts);
        packet.duration = frames;
        self.next_pts += frames;
        Ok(packet)
    }
}

#[cfg(test)]
mod tests;
