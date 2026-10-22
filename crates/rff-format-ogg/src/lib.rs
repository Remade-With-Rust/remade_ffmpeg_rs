//! Ogg container, carrying Opus (`.opus` / `.ogg`).
//!
//! Ogg is a page-based stream: each page has the `OggS` header (with a CRC over
//! the whole page) and a *segment table* whose lacing values split the page's
//! bytes into packets. For Opus, the first packet is `OpusHead` (channels +
//! input sample rate), the second is `OpusTags`, and the rest are Opus audio
//! packets.
//!
//! This implementation exposes a single Opus or Vorbis stream. The demuxer
//! streams: it reads one page at a time from a plain (non-seekable) reader,
//! reassembles packets across pages per logical stream (serial number), and
//! exposes the first logical stream whose first packet is `OpusHead` or a
//! Vorbis identification header; other multiplexed streams are skipped, and a
//! chained link of the same codec (a new BOS page after the stream's EOS) is
//! followed with its header packets dropped. `read_header` reads only up to
//! the codec headers, so opening a file costs memory proportional to its
//! header and `read_packet` holds at most one page plus one packet. Like the
//! whole-buffer parser it replaced, page CRCs are not verified and reading
//! stops at the first position that is not a complete `OggS` page header.
//! Packets carry no timestamps (as before) and no duration is declared — the
//! last page's granule position would need the end of the stream.
//!
//! The muxer writes one packet per page (`OpusHead`, `OpusTags`, then audio).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};

/// Register the Ogg format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "ogg",
        long_name: "Ogg (Opus / Vorbis)",
        extensions: &["ogg", "opus"],
        demuxer: Some(|input| Box::new(OggDemuxer::new(input))),
        muxer: Some(|output| Box::new(OggMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_ogg),
        mux_caps: MuxCaps::single(&[CodecId::Opus, CodecId::Vorbis]),
    });
}

/// Sniff Ogg by the `OggS` capture pattern.
fn probe_ogg(data: &[u8]) -> i32 {
    if data.len() >= 4 && &data[0..4] == b"OggS" {
        100
    } else {
        0
    }
}

/// Ogg's CRC-32 (polynomial 0x04c11db7, no reflection, no final xor).
fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Page / packet parsing
// ---------------------------------------------------------------------------

/// Page header flag: this page begins a logical stream.
const FLAG_BOS: u8 = 0x02;
/// Page header flag: this page ends a logical stream.
const FLAG_EOS: u8 = 0x04;

/// One Ogg page, as read off the wire.
struct Page {
    header_type: u8,
    serial: u32,
    /// The segment (lacing) table.
    laces: Vec<u8>,
    /// The page body. Shorter than the laces declare only when the input
    /// ended mid-page.
    body: Vec<u8>,
}

impl Page {
    fn is_bos(&self) -> bool {
        self.header_type & FLAG_BOS != 0
    }

    fn is_eos(&self) -> bool {
        self.header_type & FLAG_EOS != 0
    }

    /// Split the page body by its lacing values, appending to `partial` (the
    /// stream's packet carried over from earlier pages) and calling `emit` for
    /// every packet that completes on this page. A lace of 255 continues the
    /// packet; anything shorter ends it. A body cut short by end of input is
    /// clamped, so the packets it truncates are emitted short.
    fn split(&self, partial: &mut Vec<u8>, mut emit: impl FnMut(Vec<u8>)) {
        let mut data = 0usize;
        for &lace in &self.laces {
            let end = (data + lace as usize).min(self.body.len());
            partial.extend_from_slice(&self.body[data..end]);
            data = end;
            if lace < 255 {
                emit(std::mem::take(partial));
            }
        }
    }
}

/// Fill `buf` from `r` as far as the input allows; returns the bytes read
/// (short only at end of input).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(got)
}

/// Streams Ogg pages one at a time off a plain (non-seekable) reader.
struct PageReader {
    r: std::io::BufReader<Input>,
    /// Set once the input is exhausted or stopped looking like Ogg.
    done: bool,
}

impl PageReader {
    fn new(input: Input) -> PageReader {
        PageReader {
            r: std::io::BufReader::new(input),
            done: false,
        }
    }

    /// The next page, or `None` at end of input. Like the whole-buffer parser
    /// this replaces, reading stops (without error) at the first position that
    /// does not hold a complete `OggS` page header + segment table; a page
    /// whose body is cut short by end of input is still returned (clamped).
    ///
    /// `first` makes a missing capture pattern on the very first page an
    /// error ("not an Ogg file") instead of a quiet end of stream.
    fn next_page(&mut self, first: bool) -> Result<Option<Page>> {
        if self.done {
            return Ok(None);
        }
        let mut hdr = [0u8; 27];
        let got = read_full(&mut self.r, &mut hdr)?;
        if first && (got < 4 || &hdr[0..4] != b"OggS") {
            self.done = true;
            return Err(Error::invalid("ogg demux: not an Ogg file"));
        }
        if got < 27 || &hdr[0..4] != b"OggS" {
            self.done = true;
            return Ok(None);
        }
        let mut laces = vec![0u8; hdr[26] as usize];
        if read_full(&mut self.r, &mut laces)? < laces.len() {
            self.done = true;
            return Ok(None);
        }
        // At most 255 * 255 bytes; `take` caps the read at what is declared,
        // and the allocation grows only with what is actually there.
        let body_len: u64 = laces.iter().map(|&l| l as u64).sum();
        let mut body = Vec::new();
        (&mut self.r).take(body_len).read_to_end(&mut body)?;
        if (body.len() as u64) < body_len {
            self.done = true;
        }
        Ok(Some(Page {
            header_type: hdr[5],
            serial: u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]),
            laces,
            body,
        }))
    }
}

/// What a logical stream's first packet says it carries.
struct Mapping {
    codec_id: CodecId,
    channels: u16,
    sample_rate: u32,
    sample_format: SampleFormat,
    /// Header packets that precede the audio (Opus 2, Vorbis 3).
    headers: usize,
}

/// Identify the Ogg-mapped codec from a logical stream's first packet.
fn identify(first: &[u8]) -> Option<Mapping> {
    if first.len() >= 19 && &first[0..8] == b"OpusHead" {
        Some(Mapping {
            codec_id: CodecId::Opus,
            channels: first[9] as u16,
            sample_rate: u32::from_le_bytes([first[12], first[13], first[14], first[15]]),
            sample_format: SampleFormat::F32,
            headers: 2,
        })
    } else if first.len() >= 16 && first[0] == 1 && &first[1..7] == b"vorbis" {
        Some(Mapping {
            codec_id: CodecId::Vorbis,
            channels: first[11] as u16,
            sample_rate: u32::from_le_bytes([first[12], first[13], first[14], first[15]]),
            // The Vorbis decoder emits float, as FFmpeg's does.
            sample_format: SampleFormat::F32,
            headers: 3,
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Page / OpusHead building
// ---------------------------------------------------------------------------

const SERIAL: u32 = 1;

/// Append one Ogg page wrapping a single `packet` (CRC computed over the page).
fn write_page(out: &mut Vec<u8>, header_type: u8, granule: u64, seq: u32, packet: &[u8]) {
    let full = packet.len() / 255;
    let last = (packet.len() % 255) as u8;
    let num_segments = full + 1;

    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0); // version
    out.push(header_type);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&SERIAL.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    let crc_at = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
    out.push(num_segments as u8);
    out.extend(std::iter::repeat(255).take(full));
    out.push(last);
    out.extend_from_slice(packet);

    let crc = ogg_crc32(&out[start..]);
    out[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
}

fn opus_head(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.push(1); // version
    v.push(channels);
    v.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    v.extend_from_slice(&sample_rate.to_le_bytes()); // input sample rate
    v.extend_from_slice(&0u16.to_le_bytes()); // output gain
    v.push(0); // channel mapping family 0 (mono/stereo)
    v
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"remade_ffmpeg_rs";
    let mut v = b"OpusTags".to_vec();
    v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    v.extend_from_slice(vendor);
    v.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    v
}

/// Pack header packets length-prefixed (`u32 LE` + bytes) for a codec's
/// `extradata` — Vorbis needs its three setup headers handed to the decoder.
fn pack_headers(headers: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for h in headers {
        v.extend_from_slice(&(h.len() as u32).to_le_bytes());
        v.extend_from_slice(h);
    }
    v
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// The logical stream exposed as stream 0.
struct Target {
    serial: u32,
    codec_id: CodecId,
    /// Header packets still to drop before audio resumes (OpusTags when
    /// `read_header` stopped at OpusHead, or a chained link's headers).
    skip: usize,
    /// The packet carried over from this stream's previous page.
    partial: Vec<u8>,
    /// This stream's EOS page has passed: a new BOS page may chain on.
    ended: bool,
    /// A chained link whose first packet has not been identified yet.
    verify: bool,
}

/// Streaming Ogg demuxer: pages are read one at a time, so memory is bounded
/// by the largest packet, not the file. `read_header` reads only as far as
/// the chosen stream's codec headers; audio packets it had to read past are
/// queued for [`read_packet`](Demuxer::read_packet).
struct OggDemuxer {
    input: Option<Input>,
    pages: Option<PageReader>,
    target: Option<Target>,
    queue: VecDeque<Vec<u8>>,
}

impl OggDemuxer {
    fn new(input: Input) -> OggDemuxer {
        OggDemuxer {
            input: Some(input),
            pages: None,
            target: None,
            queue: VecDeque::new(),
        }
    }
}

impl Demuxer for OggDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("ogg demux: header already read"))?;
        let mut pages = PageReader::new(input);

        // Logical streams whose first packet is still being reassembled.
        let mut pending: HashMap<u32, Vec<u8>> = HashMap::new();
        // Logical streams whose first packet named no codec we map.
        let mut ignored: HashSet<u32> = HashSet::new();
        let mut any_packet = false;
        // The chosen stream, its packets so far, and its carried-over packet.
        let mut chosen: Option<(u32, Mapping)> = None;
        let mut got: Vec<Vec<u8>> = Vec::new();
        let mut partial: Vec<u8> = Vec::new();
        let mut ended = false;
        let mut first = true;
        loop {
            if let Some((_, m)) = &chosen {
                // Opus needs only OpusHead up front; Vorbis needs all three.
                let need = if m.codec_id == CodecId::Vorbis { 3 } else { 1 };
                if got.len() >= need {
                    break;
                }
            }
            let Some(page) = pages.next_page(first)? else {
                break;
            };
            first = false;
            if let Some((serial, _)) = &chosen {
                if page.serial == *serial {
                    page.split(&mut partial, |p| got.push(p));
                    ended |= page.is_eos();
                }
                continue;
            }
            if ignored.contains(&page.serial) {
                // BOS pages all come first: once a stream we already gave up
                // on continues and nothing else is mid-identification, no
                // later page can introduce a codec we map.
                if !page.is_bos() && pending.is_empty() {
                    break;
                }
                continue;
            }
            let mut completed = Vec::new();
            page.split(pending.entry(page.serial).or_default(), |p| {
                completed.push(p)
            });
            if completed.is_empty() {
                continue;
            }
            any_packet = true;
            let left = pending.remove(&page.serial).unwrap_or_default();
            let mut packets = completed.into_iter();
            let Some(head) = packets.next() else {
                continue;
            };
            match identify(&head) {
                Some(m) => {
                    chosen = Some((page.serial, m));
                    got.push(head);
                    got.extend(packets);
                    partial = left;
                    ended = page.is_eos();
                    pending.clear(); // other logical streams are not exposed
                }
                None => {
                    ignored.insert(page.serial);
                }
            }
        }

        let Some((serial, m)) = chosen else {
            return Err(if any_packet {
                Error::unsupported("ogg demux: unrecognized codec (expected Opus or Vorbis)")
            } else {
                Error::invalid("ogg demux: no packets")
            });
        };
        let extradata = match m.codec_id {
            CodecId::Vorbis => {
                if got.len() < 3 {
                    return Err(Error::invalid("ogg demux: Vorbis is missing setup headers"));
                }
                let three: Vec<&[u8]> = got.iter().take(3).map(|p| p.as_slice()).collect();
                pack_headers(&three)
            }
            _ => got[0].clone(),
        };
        let consumed = got.len().min(m.headers);
        self.queue = got.drain(consumed..).collect();
        self.target = Some(Target {
            serial,
            codec_id: m.codec_id,
            skip: m.headers - consumed,
            partial,
            ended,
            verify: false,
        });
        self.pages = Some(pages);

        let mut stream = Stream::new(0, m.codec_id);
        stream.channels = m.channels;
        stream.sample_rate = if m.sample_rate == 0 {
            48_000
        } else {
            m.sample_rate
        };
        stream.sample_format = Some(m.sample_format);
        stream.extradata = extradata;
        stream.time_base = Rational::new(1, stream.sample_rate.max(1) as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(data) = self.queue.pop_front() {
                return Ok(Packet::from_data(0, data));
            }
            let (Some(pages), Some(t)) = (self.pages.as_mut(), self.target.as_mut()) else {
                return Err(Error::Eof);
            };
            let Some(page) = pages.next_page(false)? else {
                return Err(Error::Eof);
            };
            if page.serial != t.serial {
                if !(t.ended && page.is_bos()) {
                    continue; // another logical stream: not exposed
                }
                // A chained link starts: follow it if it carries the same codec.
                *t = Target {
                    serial: page.serial,
                    codec_id: t.codec_id,
                    skip: 0,
                    partial: Vec::new(),
                    ended: false,
                    verify: true,
                };
            }
            let Target {
                codec_id,
                skip,
                partial,
                ended,
                verify,
                ..
            } = t;
            let queue = &mut self.queue;
            page.split(partial, |p| {
                if *verify {
                    *verify = false;
                    match identify(&p) {
                        Some(m) if m.codec_id == *codec_id => *skip = m.headers - 1,
                        _ => {
                            // Different codec: drop this link, await another.
                            *skip = usize::MAX;
                            *ended = true;
                        }
                    }
                } else if *skip > 0 {
                    *skip -= 1;
                } else {
                    queue.push_back(p);
                }
            });
            *ended |= page.is_eos();
        }
    }
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct OggMuxer {
    out: Output,
    codec: CodecId,
    channels: u8,
    sample_rate: u32,
    packets: Vec<Vec<u8>>,
}

impl OggMuxer {
    fn new(out: Output) -> OggMuxer {
        OggMuxer {
            out,
            codec: CodecId::Opus,
            channels: 0,
            sample_rate: 0,
            packets: Vec::new(),
        }
    }
}

impl Muxer for OggMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .first()
            .filter(|s| matches!(s.codec_id, CodecId::Opus | CodecId::Vorbis))
            .ok_or_else(|| {
                Error::unsupported("ogg mux: needs a single `opus` or `vorbis` stream")
            })?;
        self.codec = s.codec_id;
        self.channels = s.channels.clamp(1, 255) as u8;
        self.sample_rate = s.sample_rate.max(1);
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.packets.push(packet.data.clone());
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let mut out = Vec::new();
        match self.codec {
            CodecId::Vorbis => self.write_vorbis(&mut out),
            _ => self.write_opus(&mut out),
        }
        self.out.write_all(&out)?;
        self.out.flush()?;
        Ok(())
    }
}

impl OggMuxer {
    /// Opus: synthesize OpusHead + OpusTags, then one audio packet per page (48 kHz
    /// granule, 960 samples per 20 ms frame).
    fn write_opus(&self, out: &mut Vec<u8>) {
        write_page(out, 0x02, 0, 0, &opus_head(self.channels, self.sample_rate));
        write_page(out, 0x00, 0, 1, &opus_tags());
        let mut granule: u64 = 0;
        let last = self.packets.len().saturating_sub(1);
        for (i, packet) in self.packets.iter().enumerate() {
            granule += 960;
            let header_type = if i == last { 0x04 } else { 0x00 };
            write_page(out, header_type, granule, (i + 2) as u32, packet);
        }
    }

    /// Vorbis: the encoder emits its three setup headers (ident/comment/setup) as the first
    /// three packets. Page those as the header pages, then the audio packets (one long-block
    /// hop = 1024 samples of granule each). The demuxer likewise treats the first 3 as headers.
    fn write_vorbis(&self, out: &mut Vec<u8>) {
        let nheaders = 3.min(self.packets.len());
        for (i, h) in self.packets[..nheaders].iter().enumerate() {
            let htype = if i == 0 { 0x02 } else { 0x00 }; // BOS on the ident header
            write_page(out, htype, 0, i as u32, h);
        }
        let audio = &self.packets[nheaders..];
        let last = audio.len().saturating_sub(1);
        let mut granule: u64 = 0;
        for (i, packet) in audio.iter().enumerate() {
            granule += 1024; // long-block hop
            let header_type = if i == last { 0x04 } else { 0x00 }; // EOS on last
            write_page(out, header_type, granule, (nheaders + i) as u32, packet);
        }
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

    /// Today's-behaviour oracle: the whole-buffer parser the streaming
    /// demuxer replaced, kept verbatim so equivalence is checked, not assumed.
    fn reference_packets(buf: &[u8]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut partial: Vec<u8> = Vec::new();
        let mut i = 0;
        while i + 27 <= buf.len() && &buf[i..i + 4] == b"OggS" {
            let num_segments = buf[i + 26] as usize;
            let table_end = i + 27 + num_segments;
            if table_end > buf.len() {
                break;
            }
            let segment_table = &buf[i + 27..table_end];
            let mut data = table_end;
            for &lace in segment_table {
                let end = (data + lace as usize).min(buf.len());
                partial.extend_from_slice(&buf[data..end]);
                data = end;
                if lace < 255 {
                    packets.push(std::mem::take(&mut partial));
                }
            }
            i = data;
        }
        packets
    }

    /// (codec, extradata, audio packets) as the old whole-file demuxer saw them.
    type Demuxed = (CodecId, Vec<u8>, Vec<Vec<u8>>);

    fn reference_demux(buf: &[u8]) -> std::result::Result<Demuxed, ()> {
        if probe_ogg(buf) == 0 {
            return Err(());
        }
        let packets = reference_packets(buf);
        let first = packets.first().ok_or(())?;
        let (codec, headers, extradata) = if first.len() >= 19 && &first[0..8] == b"OpusHead" {
            (CodecId::Opus, 2, first.clone())
        } else if first.len() >= 16 && first[0] == 1 && &first[1..7] == b"vorbis" {
            if packets.len() < 3 {
                return Err(());
            }
            let three: Vec<&[u8]> = packets.iter().take(3).map(|p| p.as_slice()).collect();
            (CodecId::Vorbis, 3, pack_headers(&three))
        } else {
            return Err(());
        };
        Ok((
            codec,
            extradata,
            packets.into_iter().skip(headers).collect(),
        ))
    }

    fn demux_all(input: Input) -> std::result::Result<Demuxed, ()> {
        let mut dem = OggDemuxer::new(input);
        let streams = dem.read_header().map_err(|_| ())?;
        let mut packets = Vec::new();
        loop {
            match dem.read_packet() {
                Ok(p) => {
                    assert_eq!(p.stream_index, 0);
                    assert_eq!(p.pts, None);
                    packets.push(p.data);
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("unexpected demux error {e}"),
            }
        }
        Ok((streams[0].codec_id, streams[0].extradata.clone(), packets))
    }

    fn demux_bytes(buf: &[u8]) -> std::result::Result<Demuxed, ()> {
        demux_all(Box::new(Cursor::new(buf.to_vec())))
    }

    /// Mux `n` pseudo-random "Opus" packets of up to `max_len` bytes.
    fn muxed_opus(n: usize, max_len: usize) -> Vec<u8> {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let mut mux = OggMuxer::new(Box::new(sink.clone()));
        let mut s = Stream::new(0, CodecId::Opus);
        s.channels = 2;
        s.sample_rate = 48_000;
        mux.write_header(&[s]).unwrap();
        let mut x = 0x9E37_79B9u32;
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let len = 1 + (x as usize % max_len);
            let data: Vec<u8> = (0..len).map(|k| (k as u32 ^ x ^ i as u32) as u8).collect();
            mux.write_packet(&Packet::from_data(0, data)).unwrap();
        }
        mux.write_trailer().unwrap();
        let file = sink.0.lock().unwrap().clone();
        file
    }

    /// A raw page with the given segment table and body (CRC filled in).
    fn raw_page(header_type: u8, serial: u32, seq: u32, segments: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = b"OggS".to_vec();
        out.push(0);
        out.push(header_type);
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.push(segments.len() as u8);
        out.extend_from_slice(segments);
        out.extend_from_slice(body);
        let crc = ogg_crc32(&out);
        out[22..26].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Lay `packet` out as pages of at most `per_page` lacing segments each,
    /// so long packets continue across pages.
    fn paged(
        header_type: u8,
        serial: u32,
        seq: &mut u32,
        packet: &[u8],
        per_page: usize,
    ) -> Vec<u8> {
        let mut laces = vec![255u8; packet.len() / 255];
        laces.push((packet.len() % 255) as u8);
        let mut out = Vec::new();
        let mut at = 0usize;
        for (i, chunk) in laces.chunks(per_page).enumerate() {
            let len: usize = chunk.iter().map(|&l| l as usize).sum();
            let ht = if i == 0 { header_type } else { 0x01 }; // continued
            out.extend(raw_page(ht, serial, *seq, chunk, &packet[at..at + len]));
            at += len;
            *seq += 1;
        }
        out
    }

    fn head_opus() -> Vec<u8> {
        opus_head(2, 48_000)
    }

    /// Counts the bytes the demuxer pulls from its input.
    struct Counting {
        inner: Cursor<Vec<u8>>,
        read: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            Ok(n)
        }
    }

    #[test]
    fn streaming_matches_whole_file_parser_on_muxed_files() {
        for (n, max_len) in [
            (0, 1),
            (1, 1),
            (50, 254),
            (300, 255),
            (200, 700),
            (40, 20_000),
        ] {
            let file = muxed_opus(n, max_len);
            let want = reference_demux(&file).unwrap();
            assert_eq!(demux_bytes(&file).unwrap(), want, "n={n} max_len={max_len}");
            assert_eq!(want.2.len(), n);
        }
    }

    #[test]
    fn read_header_reads_only_the_header() {
        // ~4 MB of audio: opening it must touch only the first page or two.
        let file = muxed_opus(8_000, 1_000);
        assert!(file.len() > 3_000_000, "{}", file.len());
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut dem = OggDemuxer::new(Box::new(Counting {
            inner: Cursor::new(file.clone()),
            read: read.clone(),
        }));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Opus);
        let consumed = read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            consumed < 64 * 1024,
            "read_header consumed {consumed} bytes"
        );
        // ...and packets stream: reading one doesn't pull the file in either.
        dem.read_packet().unwrap();
        let consumed = read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            consumed < 64 * 1024,
            "first packet consumed {consumed} bytes"
        );
        let mut n = 1;
        while dem.read_packet().is_ok() {
            n += 1;
        }
        assert_eq!(n, 8_000);
        assert_eq!(read.load(std::sync::atomic::Ordering::Relaxed), file.len());
    }

    #[test]
    fn vorbis_header_cost_and_queued_audio() {
        // Vorbis headers spanning pages, with audio sharing the setup's last
        // page: read_header must stop there and queue (not lose) the audio.
        let mut ident = vec![1u8];
        ident.extend_from_slice(b"vorbis");
        ident.extend_from_slice(&0u32.to_le_bytes());
        ident.push(2);
        ident.extend_from_slice(&44_100u32.to_le_bytes());
        ident.resize(30, 0);
        let comment: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let setup: Vec<u8> = (0..9000u32).map(|i| (i * 3) as u8).collect();
        let a = vec![0x10u8; 100];
        let b = vec![0x20u8; 70];
        let mut file = raw_page(0x02, 7, 0, &[30], &ident);
        let mut seq = 1;
        file.extend(paged(0, 7, &mut seq, &comment, 4));
        // The setup's tail page also carries packets a and b.
        let mut laces = vec![255u8; setup.len() / 255];
        laces.push((setup.len() % 255) as u8);
        let (head, tail) = laces.split_at(laces.len() - 2);
        let head_len: usize = head.iter().map(|&l| l as usize).sum();
        file.extend(raw_page(0, 7, seq, head, &setup[..head_len]));
        let mut tail = tail.to_vec();
        tail.extend_from_slice(&[100, 70]);
        let mut body = setup[head_len..].to_vec();
        body.extend_from_slice(&a);
        body.extend_from_slice(&b);
        file.extend(raw_page(0x01, 7, seq + 1, &tail, &body));
        let audio: Vec<u8> = vec![0x30u8; 60];
        for i in 0..20_000 {
            file.extend(raw_page(0, 7, seq + 2 + i, &[60], &audio));
        }

        let want = reference_demux(&file).unwrap();
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut dem = OggDemuxer::new(Box::new(Counting {
            inner: Cursor::new(file.clone()),
            read: read.clone(),
        }));
        let streams = dem.read_header().unwrap();
        assert!(read.load(std::sync::atomic::Ordering::Relaxed) < 64 * 1024);
        assert_eq!(streams[0].codec_id, CodecId::Vorbis);
        assert_eq!(streams[0].extradata, want.1);
        assert_eq!(dem.read_packet().unwrap().data, a);
        assert_eq!(dem.read_packet().unwrap().data, b);
        assert_eq!(demux_bytes(&file).unwrap(), want);
    }

    #[test]
    fn every_truncation_matches_the_whole_file_parser() {
        // Includes packets continued across pages and > 255-byte packets.
        let mut seq = 0;
        let mut file = paged(0x02, 3, &mut seq, &head_opus(), 255);
        file.extend(paged(0, 3, &mut seq, &opus_tags(), 255));
        for i in 0..12usize {
            let p: Vec<u8> = (0..(i * 211 % 900 + 1)).map(|k| (k + i) as u8).collect();
            file.extend(paged(0, 3, &mut seq, &p, 2));
        }
        for cut in 0..=file.len() {
            let buf = &file[..cut];
            assert_eq!(demux_bytes(buf), reference_demux(buf), "cut at {cut}");
        }
    }

    #[test]
    fn interleaved_streams_are_reassembled_per_serial() {
        // Two logical streams whose pages interleave mid-packet. The old
        // parser spliced them together; the exposed stream must be intact.
        let a1: Vec<u8> = (0..700u32).map(|i| i as u8).collect();
        let a2: Vec<u8> = vec![0xA2; 40];
        let b1: Vec<u8> = (0..600u32).map(|i| (i * 7) as u8).collect();
        let mut file = raw_page(0x02, 1, 0, &[19], &head_opus());
        let mut vorbis_ident = vec![1u8];
        vorbis_ident.extend_from_slice(b"vorbis");
        vorbis_ident.resize(30, 1);
        file.extend(raw_page(0x02, 2, 0, &[30], &vorbis_ident));
        file.extend(raw_page(0, 1, 1, &[10], b"OpusTags.."));
        file.extend(raw_page(0, 1, 2, &[255, 255], &a1[..510]));
        file.extend(raw_page(0, 2, 1, &[255, 255], &b1[..510]));
        file.extend(raw_page(
            0x01,
            1,
            3,
            &[190, 40],
            &[&a1[510..], &a2[..]].concat(),
        ));
        file.extend(raw_page(0x05, 2, 2, &[90], &b1[510..]));
        let (codec, extradata, packets) = demux_bytes(&file).unwrap();
        assert_eq!(codec, CodecId::Opus);
        assert_eq!(extradata, head_opus());
        assert_eq!(packets, vec![a1, a2]);
    }

    #[test]
    fn unmapped_first_stream_falls_through_to_a_mapped_one() {
        // A Theora-like BOS first, then an Opus BOS: Opus is exposed.
        let mut file = raw_page(0x02, 9, 0, &[7], b"\x80theora");
        file.extend(raw_page(0x02, 4, 0, &[19], &head_opus()));
        file.extend(raw_page(0, 9, 1, &[3], b"xyz"));
        file.extend(raw_page(0, 4, 1, &[10, 5], b"OpusTags..hello"));
        let (codec, _, packets) = demux_bytes(&file).unwrap();
        assert_eq!(codec, CodecId::Opus);
        assert_eq!(packets, vec![b"hello".to_vec()]);
        // Only unmapped streams: unsupported, decided without reading on.
        let mut file = raw_page(0x02, 9, 0, &[7], b"\x80theora");
        file.extend(raw_page(0, 9, 1, &[3], b"xyz"));
        let mut dem = OggDemuxer::new(Box::new(Cursor::new(file)));
        assert!(matches!(dem.read_header(), Err(Error::Unsupported(_))));
    }

    #[test]
    fn chained_links_of_the_same_codec_are_followed() {
        let mut file = raw_page(0x02, 1, 0, &[19], &head_opus());
        file.extend(raw_page(0, 1, 1, &[10], b"OpusTags.."));
        file.extend(raw_page(0x04, 1, 2, &[3], b"one"));
        file.extend(raw_page(0x02, 2, 0, &[19], &head_opus()));
        file.extend(raw_page(0, 2, 1, &[10, 3], b"OpusTags..two"));
        file.extend(raw_page(0x04, 2, 2, &[5], b"three"));
        let (_, _, packets) = demux_bytes(&file).unwrap();
        assert_eq!(
            packets,
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
    }

    #[test]
    fn garbage_never_panics_and_terminates() {
        assert!(demux_bytes(b"").is_err());
        assert!(demux_bytes(b"Og").is_err());
        assert!(demux_bytes(b"OggS").is_err());
        assert!(demux_bytes(b"RIFF....WAVEfmt ").is_err());
        // Pages that declare far more body than exists.
        let mut big = raw_page(0x02, 1, 0, &[255; 255], &[]);
        big.truncate(27 + 255 + 10);
        assert!(demux_bytes(&big).is_err());
        // Mutated real files: any result is fine, panics and hangs are not.
        let base = muxed_opus(60, 600);
        let mut x = 0x1234_5678u32;
        for _ in 0..400 {
            let mut buf = base.clone();
            for _ in 0..8 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let at = x as usize % buf.len();
                buf[at] = (x >> 24) as u8;
            }
            let _ = demux_bytes(&buf);
        }
    }

    #[test]
    fn ogg_crc_matches_known_vector() {
        // CRC of empty input is 0; non-empty is non-zero and deterministic.
        assert_eq!(ogg_crc32(&[]), 0);
        assert_eq!(ogg_crc32(b"OggS"), ogg_crc32(b"OggS"));
        assert_ne!(ogg_crc32(b"OggS"), 0);
    }

    #[test]
    fn ogg_mux_then_demux_roundtrips_opus_packets() {
        // Two arbitrary "Opus" payloads (the container doesn't decode them).
        let a = vec![0xDEu8; 40];
        let b = vec![0xADu8; 300]; // >255 → exercises lacing

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = OggMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Opus);
            s.channels = 2;
            s.sample_rate = 48_000;
            mux.write_header(&[s]).unwrap();
            mux.write_packet(&Packet::from_data(0, a.clone())).unwrap();
            mux.write_packet(&Packet::from_data(0, b.clone())).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[0..4], b"OggS");
        assert_eq!(probe_ogg(&file), 100);

        let mut dem = OggDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Opus);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(streams[0].sample_rate, 48_000);
        assert_eq!(dem.read_packet().unwrap().data, a);
        assert_eq!(dem.read_packet().unwrap().data, b);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn ogg_mux_then_demux_roundtrips_vorbis() {
        // Real-shaped Vorbis headers (ident/comment/setup) + two audio packets.
        let mut ident = vec![1u8];
        ident.extend_from_slice(b"vorbis");
        ident.extend_from_slice(&0u32.to_le_bytes());
        ident.push(2); // channels @ 11
        ident.extend_from_slice(&44_100u32.to_le_bytes()); // rate @ 12
        ident.resize(30, 0);
        let comment = {
            let mut v = vec![3u8];
            v.extend_from_slice(b"vorbis");
            v.resize(20, 0);
            v
        };
        let setup = {
            let mut v = vec![5u8];
            v.extend_from_slice(b"vorbis");
            v.resize(64, 0);
            v
        };
        let a = vec![0x00u8; 60]; // audio packet (type bit clear)
        let b = vec![0x00u8; 300]; // >255 → exercises lacing

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = OggMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Vorbis);
            s.channels = 2;
            s.sample_rate = 44_100;
            mux.write_header(&[s]).unwrap();
            for p in [&ident, &comment, &setup, &a, &b] {
                mux.write_packet(&Packet::from_data(0, p.clone())).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[0..4], b"OggS");

        let mut dem = OggDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Vorbis);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(streams[0].sample_rate, 44_100);
        assert!(!streams[0].extradata.is_empty()); // 3 headers packed
        assert_eq!(dem.read_packet().unwrap().data, a);
        assert_eq!(dem.read_packet().unwrap().data, b);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn demux_detects_vorbis_and_skips_three_headers() {
        // Synthetic Vorbis identification header: type 1 + "vorbis", then
        // version(4), channels@11, sample_rate@12.
        let mut ident = vec![1u8];
        ident.extend_from_slice(b"vorbis");
        ident.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
        ident.push(2); // channels
        ident.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate

        let comment = b"fake-comment-header".to_vec();
        let setup = b"fake-setup-header".to_vec();
        let a = vec![0x11u8; 50];
        let b = vec![0x22u8; 70];

        // Hand-build the Ogg stream (our muxer only writes Opus).
        let mut file = Vec::new();
        write_page(&mut file, 0x02, 0, 0, &ident);
        write_page(&mut file, 0x00, 0, 1, &comment);
        write_page(&mut file, 0x00, 0, 2, &setup);
        write_page(&mut file, 0x00, 960, 3, &a);
        write_page(&mut file, 0x04, 1920, 4, &b);

        let mut dem = OggDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Vorbis);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(streams[0].sample_rate, 44_100);
        // The Vorbis decoder emits float, like FFmpeg's: advertise that.
        assert_eq!(streams[0].sample_format, Some(SampleFormat::F32));
        // The three setup headers are packed into extradata for the decoder.
        assert!(!streams[0].extradata.is_empty());
        // Only the audio packets are yielded.
        assert_eq!(dem.read_packet().unwrap().data, a);
        assert_eq!(dem.read_packet().unwrap().data, b);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
