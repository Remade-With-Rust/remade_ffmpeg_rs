//! AIFF and AIFF-C audio container (demux).
//!
//! An AIFF file is an IFF `FORM` of type `AIFF` or `AIFC`: big-endian chunks,
//! each padded to an even length. The `COMM` chunk gives the channel count,
//! the sample size and the sample rate (an 80-bit IEEE extended float); in
//! AIFF-C it also names a compression type. `SSND` holds the samples, after an
//! `offset` of its own.
//!
//! Compression types read:
//!
//! | type | delivered as |
//! |---|---|
//! | `NONE`, `twos` (big-endian), `sowt` (little-endian) signed PCM | `pcm`: 8-bit widened to s16, 16-bit s16, 24-bit s24, 32-bit s32 |
//! | `raw ` (unsigned 8-bit) | `pcm` s16 |
//! | `fl32` / `FL32`, `fl64` / `FL64` | `pcm` f32 / f64 |
//! | `in24` / `in32` (big-endian) | `pcm` s24 / s32 |
//! | `alaw` / `ALAW`, `ulaw` / `ULAW` | `pcm_alaw` / `pcm_mulaw` |
//! | `ima4` | `adpcm_ima_qt`, whole 34-byte-per-channel frames |
//!
//! PCM is byte-swapped to the engine's little-endian layouts on read, so the
//! [`pcm`](rff-codec-pcm) decoder takes it unchanged. Sample sizes that are
//! not a whole number of bytes occupy the next whole byte size (AIFF
//! left-justifies them), so a 20-bit file reads as 24-bit.
//!
//! Like the WAV demuxer this streams: `read_header` walks the chunks up to the
//! samples (skipping `FVER`, `MARK`, `INST`, `COMT`, `NAME`, `ID3 ` and the rest
//! without buffering them) and `read_packet` returns successive blocks of
//! whole frames, so memory is one packet, not the file. An `SSND` before
//! `COMM` is buffered, since a pipe cannot come back for it.
//!
//! Damaged files read as far as they go: an `SSND` size past the end of the
//! file stops at the end, a trailing partial frame is dropped, and a `COMM`
//! frame count that disagrees with the data is ignored in favour of the data.

use std::io::{BufReader, Read};

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Stream};

/// Register the AIFF format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "aiff",
        long_name: "AIFF / AIFF-C (Audio Interchange File Format)",
        extensions: &["aif", "aiff", "aifc", "afc"],
        demuxer: Some(|input| Box::new(AiffDemuxer::new(input))),
        muxer: None,
        muxer_path: None,
        probe: Some(probe_aiff),
        mux_caps: MuxCaps::NONE,
    });
}

/// Sniff AIFF: an IFF `FORM` whose type is `AIFF` or `AIFC`.
fn probe_aiff(data: &[u8]) -> i32 {
    if data.len() >= 12 && &data[0..4] == b"FORM" && matches!(&data[8..12], b"AIFF" | b"AIFC") {
        100
    } else {
        0
    }
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// An 80-bit IEEE 754 extended-precision value (the `COMM` sample rate), as
/// f64. Denormals, infinities and NaNs come back as 0, which the caller
/// treats as "no rate".
pub(crate) fn extended_to_f64(b: &[u8; 10]) -> f64 {
    let sign = if b[0] & 0x80 != 0 { -1.0 } else { 1.0 };
    let exp = (((b[0] & 0x7F) as i32) << 8) | b[1] as i32;
    let mantissa = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if exp == 0 || exp == 0x7FFF || mantissa == 0 {
        return 0.0;
    }
    // value = mantissa * 2^(exp - 16383 - 63); the explicit integer bit means
    // no implied leading one.
    sign * mantissa as f64 * 2f64.powi(exp - 16383 - 63)
}

/// How samples are delivered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plan {
    /// Signed PCM, `bytes` per sample, big- or little-endian in the file,
    /// delivered little-endian as `format` (8-bit is widened to s16).
    Pcm {
        bytes: usize,
        big_endian: bool,
        format: SampleFormat,
    },
    /// Unsigned 8-bit PCM (`raw `), widened to s16.
    U8,
    /// IEEE float, `bytes` (4 or 8) per sample, big-endian.
    Float { bytes: usize },
    /// One byte per sample, passed through to a G.711 decoder.
    G711(CodecId),
    /// `ima4`: 34 bytes per channel per 64 samples.
    Ima4,
}

impl Plan {
    /// File bytes per sample frame of `channels`, or per `ima4` frame.
    fn frame_bytes(self, channels: usize) -> usize {
        channels
            * match self {
                Plan::Pcm { bytes, .. } | Plan::Float { bytes } => bytes,
                Plan::U8 | Plan::G711(_) => 1,
                Plan::Ima4 => 34,
            }
    }

    /// Samples per channel one file frame holds.
    fn frame_samples(self) -> usize {
        if self == Plan::Ima4 {
            64
        } else {
            1
        }
    }

    fn codec(self) -> CodecId {
        match self {
            Plan::G711(c) => c,
            Plan::Ima4 => CodecId::AdpcmImaQt,
            _ => CodecId::Pcm,
        }
    }

    /// The sample format the stream reports: what packets carry for PCM,
    /// what the decoder produces for the coded formats.
    fn sample_format(self) -> SampleFormat {
        match self {
            Plan::Pcm { format, .. } => format,
            Plan::U8 | Plan::G711(_) | Plan::Ima4 => SampleFormat::S16,
            Plan::Float { bytes: 8 } => SampleFormat::F64,
            Plan::Float { .. } => SampleFormat::F32,
        }
    }

    /// Repack whole file frames to what the packet carries.
    fn deliver(self, raw: Vec<u8>) -> Vec<u8> {
        match self {
            Plan::Pcm { bytes: 1, .. } => raw
                .iter()
                .flat_map(|&b| ((b as i8 as i16) << 8).to_le_bytes())
                .collect(),
            Plan::U8 => raw
                .iter()
                .flat_map(|&b| (((b as i16) - 128) << 8).to_le_bytes())
                .collect(),
            Plan::Pcm {
                big_endian: false, ..
            }
            | Plan::G711(_)
            | Plan::Ima4 => raw,
            Plan::Pcm { bytes, .. } | Plan::Float { bytes } => {
                let mut out = raw;
                for s in out.chunks_exact_mut(bytes) {
                    s.reverse();
                }
                out
            }
        }
    }
}

/// The plan for a compression type and sample size.
fn plan_for(compression: &[u8; 4], bits: u16) -> Result<Plan> {
    let pcm = |big_endian: bool| -> Result<Plan> {
        let bytes = bits.div_ceil(8) as usize;
        let format = match bytes {
            1 | 2 => SampleFormat::S16,
            3 => SampleFormat::S24,
            4 => SampleFormat::S32,
            _ => {
                return Err(Error::unsupported(format!(
                    "aiff demux: {bits}-bit PCM (1 to 32 bits)"
                )))
            }
        };
        Ok(Plan::Pcm {
            bytes,
            big_endian,
            format,
        })
    };
    match compression {
        b"NONE" | b"twos" => pcm(true),
        b"sowt" => pcm(false),
        b"in24" => Ok(Plan::Pcm {
            bytes: 3,
            big_endian: true,
            format: SampleFormat::S24,
        }),
        b"in32" => Ok(Plan::Pcm {
            bytes: 4,
            big_endian: true,
            format: SampleFormat::S32,
        }),
        b"raw " => Ok(Plan::U8),
        b"fl32" | b"FL32" => Ok(Plan::Float { bytes: 4 }),
        b"fl64" | b"FL64" => Ok(Plan::Float { bytes: 8 }),
        b"alaw" | b"ALAW" => Ok(Plan::G711(CodecId::PcmAlaw)),
        b"ulaw" | b"ULAW" => Ok(Plan::G711(CodecId::PcmMulaw)),
        b"ima4" => Ok(Plan::Ima4),
        other => Err(Error::unsupported(format!(
            "aiff demux: compression type `{}`",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// What `COMM` says.
struct Comm {
    channels: u16,
    sample_rate: u32,
    plan: Plan,
}

fn parse_comm(body: &[u8], aifc: bool) -> Result<Comm> {
    if body.len() < 18 {
        return Err(Error::invalid("aiff demux: short COMM chunk"));
    }
    let channels = be16(body, 0);
    let bits = be16(body, 6);
    let rate = extended_to_f64(body[8..18].try_into().unwrap());
    if channels == 0 {
        return Err(Error::invalid("aiff demux: COMM declares no channels"));
    }
    if !(rate >= 1.0 && rate <= u32::MAX as f64) {
        return Err(Error::invalid(format!("aiff demux: sample rate {rate}")));
    }
    // AIFF-C names its compression; plain AIFF is big-endian PCM. A short
    // AIFF-C COMM (some writers drop the type for NONE) reads as NONE.
    let compression: [u8; 4] = if aifc && body.len() >= 22 {
        body[18..22].try_into().unwrap()
    } else {
        *b"NONE"
    };
    Ok(Comm {
        channels,
        sample_rate: rate.round() as u32,
        plan: plan_for(&compression, bits)?,
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

/// Read up to `len` bytes, growing the buffer only as bytes arrive, so a
/// lying size field cannot force a huge allocation.
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

/// Where the samples come from.
enum Source {
    /// The usual layout: straight from the input, `remaining` bytes left.
    Stream {
        reader: BufReader<Input>,
        remaining: u64,
    },
    /// `SSND` before `COMM`: buffered.
    Buffered { data: Vec<u8>, pos: usize },
}

/// How much of a `COMM` chunk is kept (the rest, a compression name, is skipped).
const COMM_KEEP: u64 = 64;

/// Walk the chunks up to the samples.
fn open_aiff(input: Input) -> Result<(Comm, Source)> {
    let mut r = BufReader::new(input);
    let mut form = [0u8; 12];
    if read_full(&mut r, &mut form)? < 12 || probe_aiff(&form) == 0 {
        return Err(Error::invalid("aiff demux: not an AIFF/AIFF-C file"));
    }
    let aifc = &form[8..12] == b"AIFC";
    let mut comm: Option<Comm> = None;
    let mut early: Option<Vec<u8>> = None;
    loop {
        let mut ch = [0u8; 8];
        if read_full(&mut r, &mut ch)? < 8 {
            break;
        }
        let size = be32(&ch, 4) as u64;
        let pad = size & 1;
        match &ch[0..4] {
            b"COMM" if comm.is_none() => {
                let body = read_up_to(&mut r, size.min(COMM_KEEP))?;
                comm = Some(parse_comm(&body, aifc)?);
                skip(&mut r, size - body.len() as u64 + pad)?;
            }
            b"SSND" if early.is_none() => {
                // offset (4), blockSize (4), then `offset` bytes before the samples.
                let mut head = [0u8; 8];
                if read_full(&mut r, &mut head)? < 8 || size < 8 {
                    break;
                }
                let offset = (be32(&head, 0) as u64).min(size - 8);
                skip(&mut r, offset)?;
                let remaining = size - 8 - offset;
                if let Some(comm) = comm.take() {
                    return Ok((
                        comm,
                        Source::Stream {
                            reader: r,
                            remaining,
                        },
                    ));
                }
                early = Some(read_up_to(&mut r, remaining)?);
                skip(&mut r, pad)?;
            }
            _ => skip(&mut r, size + pad)?,
        }
    }
    let comm = comm.ok_or_else(|| Error::invalid("aiff demux: no COMM chunk"))?;
    let data = early.ok_or_else(|| Error::invalid("aiff demux: no SSND chunk"))?;
    Ok((comm, Source::Buffered { data, pos: 0 }))
}

/// File frames per packet for PCM (4096 samples); `ima4` frames per packet.
const FRAMES_PER_PACKET: usize = 4096;
const IMA4_FRAMES_PER_PACKET: usize = 64;
const MAX_PACKET_BYTES: usize = 1 << 20;

struct AiffDemuxer {
    input: Option<Input>,
    source: Option<Source>,
    plan: Plan,
    frame_bytes: usize,
    frames_per_packet: usize,
    next_pts: i64,
}

impl AiffDemuxer {
    fn new(input: Input) -> AiffDemuxer {
        AiffDemuxer {
            input: Some(input),
            source: None,
            plan: Plan::U8,
            frame_bytes: 1,
            frames_per_packet: FRAMES_PER_PACKET,
            next_pts: 0,
        }
    }
}

impl Demuxer for AiffDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("aiff demux: header already read"))?;
        let (comm, source) = open_aiff(input)?;
        self.plan = comm.plan;
        self.frame_bytes = comm.plan.frame_bytes(comm.channels as usize).max(1);
        let per = if comm.plan == Plan::Ima4 {
            IMA4_FRAMES_PER_PACKET
        } else {
            FRAMES_PER_PACKET
        };
        self.frames_per_packet = (MAX_PACKET_BYTES / self.frame_bytes).clamp(1, per);
        self.source = Some(source);

        let mut stream = Stream::new(0, comm.plan.codec());
        stream.sample_rate = comm.sample_rate;
        stream.channels = comm.channels;
        stream.sample_format = Some(comm.plan.sample_format());
        stream.time_base = Rational::new(1, comm.sample_rate.clamp(1, i32::MAX as u32) as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let block = self.frame_bytes * self.frames_per_packet;
        // A read comes back short only at the end of the samples.
        let mut raw = Vec::new();
        let want = block;
        match self.source.as_mut() {
            None => return Err(Error::Eof),
            Some(Source::Stream { reader, remaining }) => {
                let got = read_up_to(reader, (*remaining).min(want as u64))?;
                *remaining -= got.len() as u64;
                raw.extend_from_slice(&got);
            }
            Some(Source::Buffered { data, pos }) => {
                let end = (*pos + want).min(data.len());
                raw.extend_from_slice(&data[*pos..end]);
                *pos = end;
            }
        }
        // Whole frames only: a trailing partial frame at the end is dropped.
        let whole = raw.len() / self.frame_bytes * self.frame_bytes;
        if whole == 0 {
            self.source = None;
            return Err(Error::Eof);
        }
        raw.truncate(whole);
        let frames = (whole / self.frame_bytes * self.plan.frame_samples()) as i64;
        let mut packet = Packet::from_data(0, self.plan.deliver(raw));
        packet.pts = Some(self.next_pts);
        packet.duration = frames;
        self.next_pts += frames;
        Ok(packet)
    }
}

#[cfg(test)]
mod tests;
