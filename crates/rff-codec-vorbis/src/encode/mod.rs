//! In-house **Ogg Vorbis I encoder** (see `docs/codec-vorbis-encoder.md`). Built
//! brick by brick like the FLAC / MP3 / AAC encoders, validated against the lewton
//! decoder oracle + ffmpeg.
//!
//! - This module: the LSB-first bit writer, the three header writers, the encoder
//!   skeleton (buffers input, emits headers).
//! - [`setup`]: parses the embedded reference setup into encode-side codebook tables
//!   (Huffman codewords + VQ dictionaries) and the floor/residue/mapping/mode configs.
//!
//! The audio path (filterbank → floor → residue → packet) arrives in bricks 2+.

#![allow(dead_code)]

mod floor;
mod frame;
mod mdct;
mod psy;
mod setup;

use std::collections::VecDeque;

use rff_codec::Encoder;
use rff_core::{AudioFrame, Dictionary, Error, Frame, Packet, Result, SampleFormat};

use setup::{parse_setup, SetupTables, SETUP_Q4_STEREO};

/// log2 of the two blocksizes the embedded setup was trained for (256 = 2^8, 2048 = 2^11).
const BS0_LOG2: u8 = 8;
const BS1_LOG2: u8 = 11;
const BITRATE_NOMINAL: i32 = 128_000;

// ---------------------------------------------------------------------------
// LSB-first bit writer. Vorbis packs bits least-significant-first within each
// byte — the opposite of AAC/MP3's MSB-first framing.
// ---------------------------------------------------------------------------

pub struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    bit: u32,
}

impl BitWriter {
    pub fn new() -> BitWriter {
        BitWriter {
            bytes: Vec::new(),
            cur: 0,
            bit: 0,
        }
    }

    /// Write the low `n` bits of `val`, least-significant bit first.
    pub fn write(&mut self, val: u32, n: u32) {
        for i in 0..n {
            if (val >> i) & 1 == 1 {
                self.cur |= 1 << self.bit;
            }
            self.bit += 1;
            if self.bit == 8 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.bit = 0;
            }
        }
    }

    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + self.bit as usize
    }

    /// Flush the partial byte (zero-padded) and return the packet bytes.
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.bit > 0 {
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        BitWriter::new()
    }
}

// ---------------------------------------------------------------------------
// The three Vorbis headers (ident, comment, setup). ident + comment are
// byte-aligned little-endian records; setup is embedded verbatim.
// ---------------------------------------------------------------------------

/// Identification header (ISO Vorbis I §4.2.2): version, channels, rate, bitrate
/// hints, blocksizes, framing bit. `bs0/bs1` are the log2 blocksizes.
fn write_ident_header(channels: u8, rate: u32, bs0: u8, bs1: u8, bitrate_nom: i32) -> Vec<u8> {
    let mut h = Vec::with_capacity(30);
    h.push(0x01);
    h.extend_from_slice(b"vorbis");
    h.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
    h.push(channels);
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&0i32.to_le_bytes()); // bitrate_maximum (unset)
    h.extend_from_slice(&bitrate_nom.to_le_bytes()); // bitrate_nominal
    h.extend_from_slice(&0i32.to_le_bytes()); // bitrate_minimum (unset)
    h.push(bs0 | (bs1 << 4)); // blocksize_0 | blocksize_1
    h.push(0x01); // framing flag
    h
}

/// Comment header (§4.2.3): vendor string + user comment list + framing bit.
fn write_comment_header(vendor: &str, comments: &[(&str, &str)]) -> Vec<u8> {
    let mut h = Vec::new();
    h.push(0x03);
    h.extend_from_slice(b"vorbis");
    h.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    h.extend_from_slice(vendor.as_bytes());
    h.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (k, v) in comments {
        let c = format!("{k}={v}");
        h.extend_from_slice(&(c.len() as u32).to_le_bytes());
        h.extend_from_slice(c.as_bytes());
    }
    h.push(0x01); // framing flag
    h
}

/// The three header packets (ident, comment, setup) for a stream. The Ogg muxer
/// takes these as the stream's `extradata` (length-prefixed) or as the first three
/// packets. Only the stereo/44.1 kHz/q4 profile is embedded so far.
fn header_packets(channels: u8, rate: u32) -> Vec<Vec<u8>> {
    vec![
        write_ident_header(channels, rate, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL),
        write_comment_header("remade_ffmpeg_rs (rff-codec-vorbis)", &[]),
        SETUP_Q4_STEREO.to_vec(),
    ]
}

// ---------------------------------------------------------------------------
// The encoder skeleton. Buffers input like the AAC encoder; the audio path
// (window → MDCT → floor → residue → packet) lands in bricks 2+.
// ---------------------------------------------------------------------------

pub struct VorbisEncoder {
    sample_rate: u32,
    channels: usize,
    bitrate: i32,
    /// Normalized quality in [0, 1] driving the psy threshold + residue λ (brick 5).
    quality: f32,
    chans: Vec<Vec<f32>>,
    setup: Option<SetupTables>,
    initialized: bool,
    queue: VecDeque<Packet>,
    flushed: bool,
    drained: bool,
}

/// Map a Vorbis-style `-q` (−1..=10) to the internal normalized quality in [0.05, 0.98],
/// staying clear of the total-masking extreme at q=0.
fn quality01_from_vorbis_q(q: f64) -> f32 {
    (((q + 1.0) / 11.0) as f32).clamp(0.05, 0.98)
}

impl VorbisEncoder {
    pub fn new() -> Self {
        VorbisEncoder {
            sample_rate: 0,
            channels: 0,
            bitrate: BITRATE_NOMINAL,
            quality: 0.6, // ~q4-ish default; overridden by `-q`
            chans: Vec::new(),
            setup: None,
            initialized: false,
            queue: VecDeque::new(),
            flushed: false,
            drained: false,
        }
    }

    /// The three Vorbis setup headers for the configured stream.
    pub fn headers(&self) -> Vec<Vec<u8>> {
        header_packets(self.channels.max(1) as u8, self.sample_rate)
    }

    /// The three setup headers packed as length-prefixed `extradata` (`u32 LE len + bytes`
    /// each) — the format the Ogg muxer and the Vorbis decoder both use.
    pub fn extradata(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for h in self.headers() {
            out.extend_from_slice(&(h.len() as u32).to_le_bytes());
            out.extend_from_slice(&h);
        }
        out
    }
}

impl Default for VorbisEncoder {
    fn default() -> Self {
        VorbisEncoder::new()
    }
}

impl Encoder for VorbisEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(b) = options.get_int("b") {
            if b > 0 {
                self.bitrate = b as i32;
            }
        }
        // `-q:a` / `-qscale:a` (Vorbis quality, −1..=10) takes precedence when present.
        if let Some(q) = options.get_int("q").or_else(|| options.get_int("qscale")) {
            self.quality = quality01_from_vorbis_q(q as f64);
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("vorbis encode: expected an audio frame"));
        };
        if !self.initialized {
            self.sample_rate = a.sample_rate;
            self.channels = a.channels.max(1) as usize;
            self.chans = vec![Vec::new(); self.channels];
            // Parse the embedded setup into encode-side codebook tables now that we
            // know the channel count (the embedded profile is stereo).
            self.setup = Some(parse_setup(SETUP_Q4_STEREO, self.channels as u8)?);
            // Emit the three setup headers as the first packets — the Ogg muxer pages them
            // ahead of the audio (the natural Ogg logical-stream order).
            for h in self.headers() {
                let mut pkt = Packet::from_data(0, h);
                pkt.pts = Some(0);
                self.queue.push_back(pkt);
            }
            self.initialized = true;
        }
        // Buffer only; all blocks are encoded in parallel at flush (each is an independent
        // pure function of its window, so there's no benefit to encoding incrementally).
        self.ingest(a)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if self.flushed && !self.drained {
            self.produce_all()?;
            self.drained = true;
        }
        if let Some(p) = self.queue.pop_front() {
            return Ok(p);
        }
        if self.flushed {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.flushed = true;
    }
}

impl VorbisEncoder {
    /// Encode all buffered long blocks (mode 1, n=2048, hop n/2) plus the final zero-padded
    /// block into audio packets, **in parallel across cores** — each block is a pure function of
    /// its window, and libvorbis is single-threaded per stream, so this is the structural speed
    /// win. Called once at flush. Vorbis's first packet decodes to 0 samples (it primes the
    /// overlap), so the granule / packet pts advances a hop per packet.
    fn produce_all(&mut self) -> Result<()> {
        const N: usize = 2048;
        const HOP: usize = N / 2;
        let buffered = self.chans.first().map_or(0, |c| c.len());
        let mut starts: Vec<usize> = Vec::new();
        let mut p = 0;
        while p + N <= buffered {
            starts.push(p);
            p += HOP;
        }
        let tail = (p < buffered).then_some(p);
        let nblocks = starts.len() + tail.is_some() as usize;
        if nblocks == 0 {
            return Ok(());
        }

        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(nblocks);
        let mut out: Vec<Result<Vec<u8>>> = (0..nblocks).map(|_| Ok(Vec::new())).collect();
        {
            let Some(setup) = self.setup.as_ref() else {
                return Ok(());
            };
            let chans = &self.chans;
            let starts = &starts;
            let (channels, sr, q) = (self.channels, self.sample_rate, self.quality);
            let chunk = nblocks.div_ceil(threads);
            std::thread::scope(|s| {
                for (ti, slot) in out.chunks_mut(chunk).enumerate() {
                    let base = ti * chunk;
                    s.spawn(move || {
                        for (j, res) in slot.iter_mut().enumerate() {
                            let bi = base + j;
                            let blocks: Vec<Vec<f32>> = if bi < starts.len() {
                                let pos = starts[bi];
                                (0..channels).map(|c| chans[c][pos..pos + N].to_vec()).collect()
                            } else {
                                let pos = tail.unwrap();
                                (0..channels)
                                    .map(|c| {
                                        let mut b = chans[c][pos..].to_vec();
                                        b.resize(N, 0.0);
                                        b
                                    })
                                    .collect()
                            };
                            *res = frame::encode_long_packet(setup, &blocks, sr, q);
                        }
                    });
                }
            });
        }

        let mut granule = 0u64;
        for res in out {
            let data = res?;
            granule += HOP as u64;
            let mut pkt = Packet::from_data(0, data);
            pkt.pts = Some(granule as i64);
            pkt.duration = HOP as i64;
            self.queue.push_back(pkt);
        }
        Ok(())
    }

    /// Buffer one input frame's samples into per-channel planes (f32).
    fn ingest(&mut self, f: &AudioFrame) -> Result<()> {
        let ch = self.channels;
        let n = f.samples;
        match f.format {
            SampleFormat::S16 => {
                let d = &f.planes[0];
                for i in 0..n {
                    for c in 0..ch {
                        let o = (i * ch + c) * 2;
                        self.chans[c].push(i16::from_le_bytes([d[o], d[o + 1]]) as f32 / 32768.0);
                    }
                }
            }
            SampleFormat::F32 => {
                let d = &f.planes[0];
                for i in 0..n {
                    for c in 0..ch {
                        let o = (i * ch + c) * 4;
                        self.chans[c]
                            .push(f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]));
                    }
                }
            }
            _ => return Err(Error::invalid("vorbis encode: unsupported sample format")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The streaming encoder (send_frame → receive_packet, fed in odd-sized chunks) must
    /// produce audio packets that lewton decodes to non-trivial audio, using the encoder's
    /// own `headers()`. Validates the block-management + header plumbing end to end.
    #[test]
    fn streaming_encode_decodes_in_lewton() {
        let mut enc = VorbisEncoder::new();
        let sr = 44_100u32;
        let total = 2048 * 6usize;
        let sample = |ch: usize, i: usize| -> f32 {
            let f = if ch == 0 { 0.02 } else { 0.023 };
            0.4 * (f * i as f32).sin()
        };
        // Feed in 1000-sample chunks to exercise arbitrary frame boundaries.
        let mut i = 0;
        while i < total {
            let chunk = 1000.min(total - i);
            let mut plane = Vec::with_capacity(chunk * 2 * 4);
            for k in 0..chunk {
                for ch in 0..2 {
                    plane.extend_from_slice(&sample(ch, i + k).to_le_bytes());
                }
            }
            let frame = Frame::Audio(AudioFrame {
                sample_rate: sr,
                channels: 2,
                format: SampleFormat::F32,
                planes: vec![plane],
                samples: chunk,
                pts: Some(i as i64),
            });
            enc.send_frame(&frame).unwrap();
            i += chunk;
        }
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
        enc.flush();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
        assert!(packets.len() >= 5, "expected multiple audio packets, got {}", packets.len());

        let headers = enc.headers();
        let l_ident = lewton::header::read_header_ident(&headers[0]).unwrap();
        let l_setup =
            lewton::header::read_header_setup(&headers[2], 2, (BS0_LOG2, BS1_LOG2)).unwrap();
        let mut pwr = lewton::audio::PreviousWindowRight::new();
        let mut decoded: Vec<f32> = Vec::new();
        for p in &packets {
            // The first three packets are the setup headers, not audio.
            if p.data.len() >= 7 && p.data[0] & 1 == 1 && &p.data[1..7] == b"vorbis" {
                continue;
            }
            let pcm =
                lewton::audio::read_audio_packet(&l_ident, &l_setup, &p.data, &mut pwr).unwrap();
            if !pcm.is_empty() && !pcm[0].is_empty() {
                decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
            }
        }
        assert!(!decoded.is_empty(), "no audio decoded");
        let energy: f32 = decoded.iter().map(|x| x * x).sum::<f32>() / decoded.len() as f32;
        assert!(energy > 1e-4 && energy < 1.0, "decoded energy out of range: {energy}");
    }

    /// LSB-first packing: the first bit written is bit 0 of byte 0.
    #[test]
    fn bitwriter_lsb_first() {
        let mut w = BitWriter::new();
        w.write(0b1, 1); // bit 0
        w.write(0b0, 1); // bit 1
        w.write(0b1, 1); // bit 2
        w.write(0b1111, 4); // bits 3..7
        w.write(0b1, 1); // bit 7
        assert_eq!(w.bit_len(), 8);
        // byte = bit0=1, bit2=1, bits3-6=1111, bit7=1 -> 1111_1101 = 0xFD
        assert_eq!(w.into_bytes(), vec![0b1111_1101]);
    }

    /// THE SPIKE: our generated ident + comment and the embedded q4 setup must all
    /// parse in lewton — proving the codebook/setup strategy end to end.
    #[test]
    fn headers_parse_in_lewton() {
        let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
        let ident = lewton::header::read_header_ident(&ident_bytes).expect("ident parses");
        assert_eq!(ident.audio_channels, 2);
        assert_eq!(ident.audio_sample_rate, 44_100);
        // lewton stores blocksizes as log2 exponents (256 = 2^8, 2048 = 2^11).
        assert_eq!(ident.blocksize_0, BS0_LOG2);
        assert_eq!(ident.blocksize_1, BS1_LOG2);

        let comment_bytes = write_comment_header("remade_ffmpeg_rs", &[("ENCODER", "rff")]);
        lewton::header::read_header_comment(&comment_bytes).expect("comment parses");

        // The crux: the embedded libvorbis q4 setup must be lewton-decodable.
        lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2))
            .expect("setup parses");
    }
}
