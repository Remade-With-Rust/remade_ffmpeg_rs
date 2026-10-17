//! Native FLAC audio container (`.flac`).
//!
//! A `.flac` file is `fLaC` + metadata blocks + audio frames. The demuxer
//! streams it: [`read_header`](Demuxer::read_header) reads only the magic and
//! the metadata block chain (sample rate + channels for `ffprobe` come from the
//! `STREAMINFO` block; the whole `fLaC` + metadata prefix becomes the stream's
//! `extradata`, which the [`flac`](rff-codec-flac) decoder needs), and
//! [`read_packet`](Demuxer::read_packet) yields **one packet per FLAC frame**,
//! holding at most one frame plus a read-ahead chunk in memory.
//!
//! FLAC frames carry no length, so a frame ends where the next one begins: a
//! position holding a frame sync code (`0xFFF8`/`0xFFF9`) whose header parses
//! with a matching CRC-8, *and* at which the bytes since the current frame's
//! start have a CRC-16 residue of zero (i.e. they end in their own valid
//! frame CRC). Requiring both makes a false split inside frame data
//! vanishingly unlikely. The last frame runs to end of input. The packets
//! partition the input exactly — nothing is skipped or invented — so a stream
//! copy reproduces the file byte for byte.
//!
//! The muxer writes the stream's `extradata` (`fLaC` + metadata) before the
//! first frame packet; a packet that already starts with `fLaC` (the encoder's
//! whole-stream output) passes straight through.

use std::io::{BufReader, Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};

/// Register the FLAC format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "flac",
        long_name: "FLAC (Free Lossless Audio Codec)",
        extensions: &["flac"],
        demuxer: Some(|input| Box::new(FlacDemuxer::new(input))),
        muxer: Some(|output| Box::new(FlacMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_flac),
        mux_caps: MuxCaps::single(&[CodecId::Flac]),
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

// ---------------------------------------------------------------------------
// Frame boundary detection
// ---------------------------------------------------------------------------

/// FLAC's CRC-8 (polynomial 0x07, init 0) over a frame header.
const CRC8_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// FLAC's CRC-16 (polynomial 0x8005, init 0) over a whole frame.
const CRC16_TABLE: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |c, &b| CRC8_TABLE[(c ^ b) as usize])
}

/// Slice-by-8 tables: `CRC16_SLICE[k][x]` is the CRC of byte `x` followed by
/// `k` zero bytes, so eight message bytes fold in with eight independent
/// lookups instead of a serial chain of eight.
const CRC16_SLICE: [[u16; 256]; 8] = {
    let mut t = [[0u16; 256]; 8];
    t[0] = CRC16_TABLE;
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev << 8) ^ CRC16_TABLE[(prev >> 8) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

/// Continue a CRC-16 over `data`. Appending a frame's own (big-endian) CRC
/// brings the residue to zero, which is how a frame end is recognized.
///
/// Every byte of the stream passes through here (frame boundaries are found
/// by CRC), so it runs slice-by-8; [`crc16_bytewise`] is the oracle.
fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let t = &CRC16_SLICE;
        crc = t[7][(c[0] ^ (crc >> 8) as u8) as usize]
            ^ t[6][(c[1] ^ crc as u8) as usize]
            ^ t[5][c[2] as usize]
            ^ t[4][c[3] as usize]
            ^ t[3][c[4] as usize]
            ^ t[2][c[5] as usize]
            ^ t[1][c[6] as usize]
            ^ t[0][c[7] as usize];
    }
    crc16_bytewise(crc, chunks.remainder())
}

fn crc16_bytewise(crc: u16, data: &[u8]) -> u16 {
    data.iter().fold(crc, |c, &b| {
        (c << 8) ^ CRC16_TABLE[((c >> 8) ^ b as u16) as usize]
    })
}

/// The fields of a frame header the demuxer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameHeader {
    /// Variable-blocksize stream: `number` is a sample number, not a frame one.
    variable: bool,
    number: u64,
    block_size: u32,
}

enum HeaderParse {
    /// More bytes are needed to decide.
    Need,
    /// Not a valid frame header.
    Bad,
    Ok(FrameHeader),
}

/// Parse a frame header at the start of `b` with the same acceptance rules as
/// the decoder (reserved bits clear, no reserved codes, valid coded number,
/// CRC-8 match), plus a channel count consistent with `STREAMINFO`
/// (`channels`, 0 = unknown). `eof` says no bytes can follow `b`.
fn parse_frame_header(b: &[u8], eof: bool, channels: u16) -> HeaderParse {
    let need = |n: usize| b.len() < n;
    let short = if eof {
        HeaderParse::Bad
    } else {
        HeaderParse::Need
    };
    if need(2) {
        return short;
    }
    if b[0] != 0xFF || b[1] & 0xFE != 0xF8 {
        return HeaderParse::Bad;
    }
    if need(5) {
        return short;
    }
    let bs_code = b[2] >> 4;
    let sr_code = b[2] & 0x0F;
    let ch_assign = b[3] >> 4;
    let ss_code = (b[3] >> 1) & 0x07;
    if bs_code == 0 || sr_code == 15 || ch_assign > 10 || ss_code == 3 || b[3] & 1 != 0 {
        return HeaderParse::Bad;
    }
    let nch = if ch_assign < 8 {
        ch_assign as u16 + 1
    } else {
        2
    };
    if channels != 0 && nch != channels {
        return HeaderParse::Bad;
    }
    // UTF-8-style coded frame / sample number.
    let lead = b[4];
    let nconts = if lead & 0x80 == 0 {
        0
    } else {
        let n = lead.leading_ones() as usize - 1;
        if n == 0 || n > 6 {
            return HeaderParse::Bad;
        }
        n
    };
    let mut pos = 5 + nconts;
    if need(pos) {
        return short;
    }
    let mut number = (lead & (0x7F >> nconts)) as u64;
    for &c in &b[5..pos] {
        if c & 0xC0 != 0x80 {
            return HeaderParse::Bad;
        }
        number = (number << 6) | (c & 0x3F) as u64;
    }
    let bs_extra = match bs_code {
        6 => 1,
        7 => 2,
        _ => 0,
    };
    let sr_extra = match sr_code {
        12 => 1,
        13 | 14 => 2,
        _ => 0,
    };
    if need(pos + bs_extra + sr_extra + 1) {
        return short;
    }
    let block_size = match bs_code {
        1 => 192,
        2..=5 => 576u32 << (bs_code - 2),
        6 => b[pos] as u32 + 1,
        7 => u16::from_be_bytes([b[pos], b[pos + 1]]) as u32 + 1,
        _ => 256u32 << (bs_code - 8),
    };
    pos += bs_extra + sr_extra;
    if crc8(&b[..pos]) != b[pos] {
        return HeaderParse::Bad;
    }
    HeaderParse::Ok(FrameHeader {
        variable: b[1] & 1 != 0,
        number,
        block_size,
    })
}

/// Smallest possible frame: 6-byte header, a 2-byte 8-bit CONSTANT subframe,
/// 2-byte CRC-16. No frame boundary is looked for closer than this.
const MIN_FRAME_BYTES: usize = 10;
/// Read-ahead granularity.
const READ_CHUNK: u64 = 64 * 1024;
/// A frame is at most ~2.2 MiB (65535 samples x 8 channels x 33 bits). If no
/// boundary turns up within this much data the input is not FLAC frames:
/// emit what is buffered and carry on, so memory stays bounded.
const MAX_FRAME_BYTES: usize = 16 << 20;

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct FlacDemuxer {
    input: Option<Input>,
    reader: Option<BufReader<Input>>,
    /// `buf[start..]` is the current frame (from its first byte) + read-ahead.
    buf: Vec<u8>,
    start: usize,
    /// Next position (in `buf`) to test as a frame boundary.
    scan: usize,
    /// CRC-16 over `buf[start..crc_pos]`.
    crc: u16,
    crc_pos: usize,
    /// The input is exhausted: nothing follows `buf`.
    eof: bool,
    channels: u16,
    time_base: Rational,
    /// Fixed-blocksize streams: the nominal block size (frame number x this =
    /// first sample), when `STREAMINFO` pins it.
    fixed_block: Option<u32>,
    /// Running sample position, for frames whose header can't place them.
    next_pts: i64,
}

impl FlacDemuxer {
    fn new(input: Input) -> FlacDemuxer {
        FlacDemuxer {
            input: Some(input),
            reader: None,
            buf: Vec::new(),
            start: 0,
            scan: 0,
            crc: 0,
            crc_pos: 0,
            eof: true,
            channels: 0,
            time_base: Rational::new(1, 1),
            fixed_block: None,
            next_pts: 0,
        }
    }

    /// Look for the end of the frame starting at `start`; `None` = need more
    /// input (or, at end of input, none before the end).
    fn find_boundary(&mut self) -> Option<usize> {
        self.scan = self.scan.max(self.start + MIN_FRAME_BYTES);
        while self.scan < self.buf.len() {
            let Some(off) = self.buf[self.scan..].iter().position(|&b| b == 0xFF) else {
                self.scan = self.buf.len();
                return None;
            };
            let p = self.scan + off;
            match parse_frame_header(&self.buf[p..], self.eof, self.channels) {
                HeaderParse::Need => {
                    self.scan = p;
                    return None;
                }
                HeaderParse::Bad => self.scan = p + 1,
                HeaderParse::Ok(_) => {
                    self.crc = crc16_update(self.crc, &self.buf[self.crc_pos..p]);
                    self.crc_pos = p;
                    if self.crc == 0 {
                        return Some(p);
                    }
                    self.scan = p + 1;
                }
            }
        }
        None
    }

    /// Cut `buf[start..end]` off as the next packet.
    fn emit(&mut self, end: usize) -> Packet {
        let data = self.buf[self.start..end].to_vec();
        self.start = end;
        self.scan = end;
        self.crc = 0;
        self.crc_pos = end;

        let mut packet = Packet::from_data(0, Vec::new());
        match parse_frame_header(&data, true, self.channels) {
            HeaderParse::Ok(h) => {
                let pts = match (h.variable, self.fixed_block) {
                    (true, _) => h.number as i64,
                    (false, Some(bs)) => h.number as i64 * bs as i64,
                    (false, None) => self.next_pts,
                };
                packet.pts = Some(pts);
                packet.duration = h.block_size as i64;
                self.next_pts = pts + h.block_size as i64;
            }
            _ => packet.pts = Some(self.next_pts),
        }
        packet.data = data;
        packet.time_base = self.time_base;
        packet.flags.keyframe = true; // every FLAC frame decodes on its own
        packet
    }

    /// Drop consumed bytes and read the next chunk.
    fn fill(&mut self) -> Result<()> {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.scan -= self.start;
            self.crc_pos -= self.start;
            self.start = 0;
        }
        let Some(reader) = self.reader.as_mut() else {
            self.eof = true;
            return Ok(());
        };
        if reader.take(READ_CHUNK).read_to_end(&mut self.buf)? == 0 {
            self.eof = true;
        }
        Ok(())
    }
}

impl Demuxer for FlacDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("flac demux: header already read"))?;
        let mut reader = BufReader::new(input);
        let mut header = Vec::new();
        (&mut reader).take(4).read_to_end(&mut header)?;
        if probe_flac(&header) == 0 {
            return Err(Error::invalid("flac demux: not a FLAC file"));
        }
        // The metadata block chain, verbatim. Each block is read with `take`,
        // so a lying length allocates only what the input actually holds.
        loop {
            let at = header.len();
            (&mut reader).take(4).read_to_end(&mut header)?;
            if header.len() - at < 4 {
                break; // truncated mid-metadata: no frames follow
            }
            let last = header[at] & 0x80 != 0;
            let len = u32::from_be_bytes([0, header[at + 1], header[at + 2], header[at + 3]]);
            let got = (&mut reader).take(len as u64).read_to_end(&mut header)?;
            if got < len as usize || last {
                break;
            }
        }
        let (sample_rate, channels) =
            streaminfo(&header).ok_or_else(|| Error::invalid("flac demux: bad STREAMINFO"))?;
        let min_block = u16::from_be_bytes([header[8], header[9]]) as u32;
        let max_block = u16::from_be_bytes([header[10], header[11]]) as u32;

        let mut stream = Stream::new(0, CodecId::Flac);
        stream.sample_rate = sample_rate;
        stream.channels = channels;
        stream.sample_format = Some(rff_core::SampleFormat::F32);
        stream.time_base = Rational::new(1, sample_rate.max(1) as i32);
        stream.extradata = header;

        self.reader = Some(reader);
        self.eof = false;
        self.channels = channels;
        self.time_base = stream.time_base;
        self.fixed_block = (min_block == max_block && min_block >= 16).then_some(max_block);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(end) = self.find_boundary() {
                return Ok(self.emit(end));
            }
            if self.eof {
                if self.start < self.buf.len() {
                    return Ok(self.emit(self.buf.len()));
                }
                return Err(Error::Eof);
            }
            if self.buf.len() - self.start > MAX_FRAME_BYTES {
                return Ok(self.emit(self.buf.len()));
            }
            self.fill()?;
        }
    }
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct FlacMuxer {
    out: Output,
    /// `fLaC` + metadata to write ahead of the first frame packet.
    header: Option<Vec<u8>>,
    started: bool,
}

impl FlacMuxer {
    fn new(out: Output) -> FlacMuxer {
        FlacMuxer {
            out,
            header: None,
            started: false,
        }
    }
}

/// The stream header to write before frame packets, from `extradata`:
/// `fLaC` + metadata as-is, or a bare 34-byte STREAMINFO wrapped as the
/// (last) metadata block.
fn stream_header(extradata: &[u8]) -> Option<Vec<u8>> {
    if extradata.starts_with(b"fLaC") {
        Some(extradata.to_vec())
    } else if extradata.len() == 34 {
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x80, 0, 0, 34]);
        v.extend_from_slice(extradata);
        Some(v)
    } else {
        None
    }
}

impl Muxer for FlacMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Flac => {
                self.header = stream_header(&s.extradata);
                Ok(())
            }
            Some(_) => Err(Error::unsupported(
                "flac mux: only the `flac` codec is supported",
            )),
            None => Err(Error::invalid("flac mux: no streams")),
        }
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.started {
            self.started = true;
            // The encoder emits the whole stream (with its own `fLaC` header)
            // as one packet; demuxed packets are bare frames.
            if !packet.data.starts_with(b"fLaC") {
                let header = self.header.as_ref().ok_or_else(|| {
                    Error::invalid("flac mux: frame packets need STREAMINFO extradata")
                })?;
                self.out.write_all(header)?;
            }
        }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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
        // No last-block flag: the zero slack parses as empty metadata blocks
        // to the end, so it is all header and there are no frames.
        assert_eq!(streams[0].extradata, flac);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

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

    struct Counting {
        inner: Cursor<Vec<u8>>,
        read: Arc<AtomicUsize>,
    }
    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }

    /// Mono 16-bit 44.1 kHz STREAMINFO (+ a padding block), fixed `block`.
    fn mono_header(block: u16, padding: usize) -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x00, 0, 0, 34]);
        v.extend_from_slice(&block.to_be_bytes());
        v.extend_from_slice(&block.to_be_bytes());
        v.extend_from_slice(&[0; 6]);
        let packed: u64 = (44_100u64 << 44) | (15u64 << 36);
        v.extend_from_slice(&packed.to_be_bytes());
        v.extend_from_slice(&[0; 16]);
        v.push(0x81); // last block: PADDING
        v.extend_from_slice(&(padding as u32).to_be_bytes()[1..]);
        v.resize(v.len() + padding, 0);
        v
    }

    /// One mono 16-bit fixed-blocksize frame with a VERBATIM subframe.
    fn verbatim_frame(number: u32, samples: &[i16]) -> Vec<u8> {
        assert!(number < 128);
        let mut f = vec![0xFF, 0xF8, (7 << 4) | 9, 4 << 1, number as u8];
        f.extend_from_slice(&(samples.len() as u16 - 1).to_be_bytes());
        f.push(crc8(&f));
        f.push(0x02); // VERBATIM, no wasted bits
        for s in samples {
            f.extend_from_slice(&s.to_be_bytes());
        }
        let crc = crc16_update(0, &f);
        f.extend_from_slice(&crc.to_be_bytes());
        f
    }

    /// Pseudo-random samples. With `fakes`, their bytes embed many *valid*
    /// frame headers (sync + CRC-8), so the splitter must rely on the CRC-16
    /// to reject them.
    fn tricky_samples(n: usize, seed: u32, fakes: bool) -> Vec<i16> {
        let mut bytes = Vec::with_capacity(n * 2);
        let fake = {
            let mut h = vec![0xFF, 0xF8, (7 << 4) | 9, 4 << 1, 3, 0x0F, 0xFF];
            h.push(crc8(&h));
            h
        };
        let mut x = seed | 1;
        while bytes.len() < n * 2 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            if fakes && x % 7 == 0 {
                bytes.extend_from_slice(&fake);
            } else {
                bytes.push(x as u8);
            }
        }
        bytes.truncate(n * 2);
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]))
            .collect()
    }

    fn test_file(
        frames: usize,
        block: u16,
        padding: usize,
        fakes: bool,
    ) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut file = mono_header(block, padding);
        let mut list = Vec::new();
        for i in 0..frames {
            let n = if i + 1 == frames {
                block as usize / 3 + 1
            } else {
                block as usize
            };
            let f = verbatim_frame((i % 128) as u32, &tricky_samples(n, i as u32 + 1, fakes));
            file.extend_from_slice(&f);
            list.push(f);
        }
        (file, list)
    }

    fn demux(file: &[u8]) -> Result<(Stream, Vec<Packet>)> {
        let mut dem = FlacDemuxer::new(Box::new(Cursor::new(file.to_vec())));
        let stream = dem.read_header()?.remove(0);
        let mut packets = Vec::new();
        loop {
            match dem.read_packet() {
                Ok(p) => packets.push(p),
                Err(Error::Eof) => return Ok((stream, packets)),
                Err(e) => return Err(e),
            }
        }
    }

    /// The slice-by-8 CRC must equal the bytewise one for every length (all
    /// remainders) and every starting CRC, including when resumed mid-stream.
    #[test]
    fn crc16_slice_by_8_matches_bytewise() {
        let mut s = 0x1234_5678_9ABC_DEF0u64;
        let data: Vec<u8> = (0..4099)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 56) as u8
            })
            .collect();
        for len in (0..64).chain([255, 1000, 4096, 4099]) {
            for init in [0u16, 1, 0x8005, 0xFFFF, 0xBEEF] {
                assert_eq!(
                    crc16_update(init, &data[..len]),
                    crc16_bytewise(init, &data[..len]),
                    "len={len} init={init:#x}"
                );
            }
        }
        // Resuming at an arbitrary split equals one pass.
        for split in [0, 1, 7, 8, 9, 500, 4098] {
            let one = crc16_update(0, &data);
            let two = crc16_update(crc16_update(0, &data[..split]), &data[split..]);
            assert_eq!(one, two, "split={split}");
        }
    }

    #[test]
    fn crc16_residue_of_a_frame_is_zero() {
        let f = verbatim_frame(0, &[1, -2, 3, 0x7FFF]);
        assert_eq!(crc16_update(0, &f), 0);
        assert!(matches!(
            parse_frame_header(&f, true, 1),
            HeaderParse::Ok(FrameHeader {
                variable: false,
                number: 0,
                block_size: 4
            })
        ));
        // Wrong channel count, short input, flipped CRC-8.
        assert!(matches!(parse_frame_header(&f, true, 2), HeaderParse::Bad));
        assert!(matches!(
            parse_frame_header(&f[..6], false, 1),
            HeaderParse::Need
        ));
        assert!(matches!(
            parse_frame_header(&f[..6], true, 1),
            HeaderParse::Bad
        ));
        let mut bad = f.clone();
        bad[7] ^= 1;
        assert!(matches!(
            parse_frame_header(&bad, true, 1),
            HeaderParse::Bad
        ));
    }

    #[test]
    fn one_packet_per_frame_with_sample_pts() {
        let (file, frames) = test_file(40, 1152, 300, true);
        let (stream, packets) = demux(&file).unwrap();
        assert_eq!(stream.sample_rate, 44_100);
        assert_eq!(stream.channels, 1);
        assert_eq!(stream.extradata, mono_header(1152, 300));
        assert_eq!(packets.len(), frames.len());
        for (i, (p, f)) in packets.iter().zip(&frames).enumerate() {
            assert_eq!(&p.data, f, "frame {i}");
            assert_eq!(p.pts, Some(i as i64 * 1152));
            assert!(p.flags.keyframe);
        }
        assert_eq!(packets.last().unwrap().duration, 1152 / 3 + 1);
    }

    #[test]
    fn read_header_reads_only_the_header() {
        // ~4.7 MB: 2 x 4096 bytes x 576 frames.
        let (file, frames) = test_file(576, 4096, 8192, false);
        assert!(file.len() > 3_000_000);
        let read = Arc::new(AtomicUsize::new(0));
        let mut dem = FlacDemuxer::new(Box::new(Counting {
            inner: Cursor::new(file.clone()),
            read: read.clone(),
        }));
        dem.read_header().unwrap();
        let consumed = read.load(Ordering::Relaxed);
        assert!(consumed < 64 * 1024, "read_header consumed {consumed}");
        assert_eq!(dem.read_packet().unwrap().data, frames[0]);
        let consumed = read.load(Ordering::Relaxed);
        assert!(consumed < 256 * 1024, "first packet consumed {consumed}");
        let mut n = 1;
        while let Ok(p) = dem.read_packet() {
            assert_eq!(p.data, frames[n]);
            n += 1;
        }
        assert_eq!(n, frames.len());
    }

    #[test]
    fn stream_copy_reproduces_the_file() {
        let (file, _) = test_file(25, 2048, 0, true);
        let mut dem = FlacDemuxer::new(Box::new(Cursor::new(file.clone())));
        let streams = dem.read_header().unwrap();
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let mut mux = FlacMuxer::new(Box::new(sink.clone()));
        mux.write_header(&streams).unwrap();
        while let Ok(p) = dem.read_packet() {
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
        assert_eq!(*sink.0.lock().unwrap(), file);
    }

    #[test]
    fn muxer_passes_whole_stream_packets_and_needs_streaminfo_for_frames() {
        let (file, frames) = test_file(3, 512, 0, true);
        // Encoder-style: one packet holding the whole stream, no extradata.
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let mut mux = FlacMuxer::new(Box::new(sink.clone()));
        mux.write_header(&[Stream::new(0, CodecId::Flac)]).unwrap();
        mux.write_packet(&Packet::from_data(0, file.clone()))
            .unwrap();
        assert_eq!(*sink.0.lock().unwrap(), file);
        // A bare frame without STREAMINFO can't make a valid file.
        let mut mux = FlacMuxer::new(Box::new(Vec::new()));
        mux.write_header(&[Stream::new(0, CodecId::Flac)]).unwrap();
        assert!(mux
            .write_packet(&Packet::from_data(0, frames[0].clone()))
            .is_err());
        // A bare 34-byte STREAMINFO is wrapped as the last metadata block.
        let mut s = Stream::new(0, CodecId::Flac);
        s.extradata = mono_header(512, 0)[8..42].to_vec();
        let header = stream_header(&s.extradata).unwrap();
        assert_eq!(&header[..8], b"fLaC\x80\x00\x00\x22");
    }

    #[test]
    fn truncation_and_garbage_partition_the_input() {
        let (file, _) = test_file(6, 300, 20, true);
        let header_len = mono_header(300, 20).len();
        // Every prefix: never panics; packets are exactly the bytes after the
        // header, in order.
        for cut in 0..=file.len() {
            let buf = &file[..cut];
            match demux(buf) {
                Ok((s, packets)) => {
                    let mut all = s.extradata.clone();
                    for p in &packets {
                        assert!(!p.data.is_empty());
                        all.extend_from_slice(&p.data);
                    }
                    assert_eq!(all, buf, "cut {cut}");
                    if cut > header_len {
                        assert!(!packets.is_empty());
                    }
                }
                Err(_) => assert!(cut < 22, "cut {cut} failed"),
            }
        }
        // Mutations and pure noise after a valid header.
        let mut x = 0xDEAD_BEEFu32;
        for round in 0..300 {
            let mut buf = file.clone();
            for _ in 0..(1 + round % 12) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let at = x as usize % buf.len();
                buf[at] = (x >> 24) as u8;
            }
            if let Ok((s, packets)) = demux(&buf) {
                let total: usize =
                    s.extradata.len() + packets.iter().map(|p| p.data.len()).sum::<usize>();
                assert_eq!(total, buf.len());
            }
        }
        let mut noise = mono_header(4096, 0);
        noise.extend((0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 | 0x80));
        let (_, packets) = demux(&noise).unwrap();
        assert!(!packets.is_empty());
        // Metadata lying about a 16 MiB block: no huge allocation, clean end.
        let mut liar = mono_header(4096, 0);
        let last = liar.len() - 4;
        liar[last..].copy_from_slice(&[0x01, 0xFF, 0xFF, 0xFF]);
        let (s, packets) = demux(&liar).unwrap();
        assert_eq!(s.extradata, liar);
        assert!(packets.is_empty());
        assert!(demux(b"").is_err());
        assert!(demux(b"fLa").is_err());
        assert!(demux(b"fLaC").is_err());
    }
}
