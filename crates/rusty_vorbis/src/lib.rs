//! Pure-Rust **Ogg Vorbis I encoder**, no C and no FFI.
//!
//! Extracted from (and the engine of) the
//! [`remade_ffmpeg_rs`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)
//! project. The crate has **zero dependencies**. To our knowledge this is the
//! first pure-Rust, permissively-licensed Vorbis *encoder* — decoding is
//! intentionally out of scope (use [`lewton`](https://crates.io/crates/lewton)).
//!
//! Built brick by brick like the FLAC / MP3 / AAC encoders, validated against
//! the lewton decoder oracle + ffmpeg: window → forward MDCT ([`mdct`]) → a
//! Bark-scale masking-threshold floor ([`psy`] + [`floor`]) → forward channel
//! coupling + point stereo → rate-distortion residue VQ ([`frame`]), emitting
//! an embedded libvorbis setup header ([`setup`]). Quality is driven by the
//! Vorbis-style `-q` knob (see [`quality01_from_vorbis_q`]).
//!
//! - This module: the LSB-first bit writer, the three header writers, the
//!   streaming [`VorbisEncoder`] (buffers input, emits headers, then encodes
//!   all blocks **in parallel across cores** at [`VorbisEncoder::finish`]).
//! - [`setup`]: parses the embedded reference setup into encode-side codebook
//!   tables (Huffman codewords + VQ dictionaries) and the
//!   floor/residue/mapping/mode configs.
//!
//! The `simd` feature (default) enables the runtime-detected AVX2 residue-VQ
//! distance kernel on x86_64 — bit-exact vs the scalar path;
//! `--no-default-features` gives a 100%-safe scalar build.

#![allow(dead_code)]

mod error;
pub mod floor;
pub mod frame;
pub mod mdct;
pub mod psy;
pub mod setup;

use std::collections::VecDeque;

pub use error::{Error, Result};

use setup::{parse_setup, SetupTables, SETUP_Q4_STEREO};

/// log2 of the two blocksizes the embedded setup was trained for (256 = 2^8, 2048 = 2^11).
pub const BS0_LOG2: u8 = 8;
pub const BS1_LOG2: u8 = 11;
pub const BITRATE_NOMINAL: i32 = 128_000;

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
pub fn write_ident_header(channels: u8, rate: u32, bs0: u8, bs1: u8, bitrate_nom: i32) -> Vec<u8> {
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
pub fn write_comment_header(vendor: &str, comments: &[(&str, &str)]) -> Vec<u8> {
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
// The streaming encoder. Buffers input like the AAC encoder; all blocks are
// encoded in parallel at finish (window → MDCT → floor → residue → packet).
// ---------------------------------------------------------------------------

/// One encoded Vorbis packet with its timing.
///
/// The first three packets pulled from [`VorbisEncoder::next_packet`] are the
/// identification / comment / setup **header packets** (`pts` 0, `duration` 0);
/// the Ogg muxer pages them ahead of the audio (the natural Ogg logical-stream
/// order). Audio packets carry `pts` = the running granule position (total
/// decodable samples per channel) and `duration` = the samples this packet adds.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    /// Granule position after this packet (0 for header packets).
    pub pts: i64,
    /// Samples per channel this packet contributes (0 for header packets).
    pub duration: i64,
}

/// Encoder configuration. `Default` gives the ~q4-ish operating point.
#[derive(Debug, Clone, Copy)]
pub struct VorbisEncoderConfig {
    /// Nominal-bitrate hint (bps). Informational for now — the identification
    /// header carries the embedded profile's nominal rate ([`BITRATE_NOMINAL`]).
    pub bitrate_bps: i32,
    /// Normalized quality in [0, 1] driving the psy threshold + residue λ
    /// (brick 5). Map a Vorbis-style `-q` with [`quality01_from_vorbis_q`].
    pub quality: f32,
}

impl Default for VorbisEncoderConfig {
    fn default() -> Self {
        VorbisEncoderConfig {
            bitrate_bps: BITRATE_NOMINAL,
            quality: 0.6, // ~q4-ish default; override via `quality` / set_quality
        }
    }
}

/// Map a Vorbis-style `-q` (−1..=10) to the internal normalized quality in [0.05, 0.98],
/// staying clear of the total-masking extreme at q=0.
pub fn quality01_from_vorbis_q(q: f64) -> f32 {
    (((q + 1.0) / 11.0) as f32).clamp(0.05, 0.98)
}

pub struct VorbisEncoder {
    sample_rate: u32,
    channels: usize,
    bitrate: i32,
    /// Normalized quality in [0, 1] driving the psy threshold + residue λ (brick 5).
    quality: f32,
    chans: Vec<Vec<f32>>,
    setup: Option<SetupTables>,
    initialized: bool,
    queue: VecDeque<EncodedPacket>,
    flushed: bool,
    drained: bool,
}

impl VorbisEncoder {
    pub fn new(config: VorbisEncoderConfig) -> Self {
        VorbisEncoder {
            sample_rate: 0,
            channels: 0,
            bitrate: config.bitrate_bps,
            quality: config.quality,
            chans: Vec::new(),
            setup: None,
            initialized: false,
            queue: VecDeque::new(),
            flushed: false,
            drained: false,
        }
    }

    /// Set the normalized quality in [0, 1] (takes effect for packets not yet
    /// produced — production happens at [`VorbisEncoder::finish`]).
    pub fn set_quality(&mut self, quality: f32) {
        self.quality = quality;
    }

    /// Set the nominal-bitrate hint (bps). Informational for now — see
    /// [`VorbisEncoderConfig::bitrate_bps`].
    pub fn set_bitrate_bps(&mut self, bps: i32) {
        self.bitrate = bps;
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

    /// Feed interleaved `f32` PCM in [-1, 1]. The first push fixes the stream's
    /// channel count + sample rate and queues the three header packets — the
    /// first packets out of [`VorbisEncoder::next_packet`].
    ///
    /// Input is buffered only; all blocks are encoded in parallel at
    /// [`VorbisEncoder::finish`] (each is an independent pure function of its
    /// window, so there's no benefit to encoding incrementally).
    pub fn push_pcm_f32(&mut self, interleaved: &[f32], channels: u16, sample_rate: u32) -> Result<()> {
        self.init_stream(channels, sample_rate)?;
        let ch = self.channels;
        let n = interleaved.len() / ch;
        for i in 0..n {
            for c in 0..ch {
                self.chans[c].push(interleaved[i * ch + c]);
            }
        }
        Ok(())
    }

    /// Feed interleaved `i16` PCM (converted as `sample / 32768.0`, the exact
    /// math the rff adapter always used). See [`VorbisEncoder::push_pcm_f32`].
    pub fn push_pcm_s16(&mut self, interleaved: &[i16], channels: u16, sample_rate: u32) -> Result<()> {
        self.init_stream(channels, sample_rate)?;
        let ch = self.channels;
        let n = interleaved.len() / ch;
        for i in 0..n {
            for c in 0..ch {
                self.chans[c].push(interleaved[i * ch + c] as f32 / 32768.0);
            }
        }
        Ok(())
    }

    /// First-push initialization: fix the stream layout, parse the embedded
    /// setup, and queue the three header packets ahead of any audio.
    fn init_stream(&mut self, channels: u16, sample_rate: u32) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        self.sample_rate = sample_rate;
        self.channels = channels.max(1) as usize;
        self.chans = vec![Vec::new(); self.channels];
        // Parse the embedded setup into encode-side codebook tables now that we
        // know the channel count (the embedded profile is stereo).
        self.setup = Some(parse_setup(SETUP_Q4_STEREO, self.channels as u8)?);
        // Emit the three setup headers as the first packets — the Ogg muxer pages them
        // ahead of the audio (the natural Ogg logical-stream order).
        for h in self.headers() {
            self.queue.push_back(EncodedPacket {
                data: h,
                pts: 0,
                duration: 0,
            });
        }
        self.initialized = true;
        Ok(())
    }

    /// Pull the next packet (FFmpeg-style drain semantics): `Err(Again)` until
    /// [`VorbisEncoder::finish`] has been called, then every packet in order,
    /// then `Err(Eof)`.
    pub fn next_packet(&mut self) -> Result<EncodedPacket> {
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

    /// Signal end of input; the buffered stream is encoded on the next
    /// [`VorbisEncoder::next_packet`] pull.
    pub fn finish(&mut self) {
        self.flushed = true;
    }

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
            self.queue.push_back(EncodedPacket {
                data,
                pts: granule as i64,
                duration: HOP as i64,
            });
        }
        Ok(())
    }
}

impl Default for VorbisEncoder {
    fn default() -> Self {
        VorbisEncoder::new(VorbisEncoderConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The streaming encoder (push_pcm_f32 → next_packet, fed in odd-sized chunks) must
    /// emit the three header packets first (ident/comment/setup, in order), then multiple
    /// audio packets with a hop-advancing granule pts — the packet-ordering contract the
    /// Ogg muxer depends on. (The lewton decode of this exact stream lives in the
    /// rff-codec-vorbis adapter's tests, where the decoder dependency is allowed.)
    #[test]
    fn streaming_encode_emits_headers_then_audio() {
        let mut enc = VorbisEncoder::default();
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
            let mut pcm = Vec::with_capacity(chunk * 2);
            for k in 0..chunk {
                for ch in 0..2 {
                    pcm.push(sample(ch, i + k));
                }
            }
            enc.push_pcm_f32(&pcm, 2, sr).unwrap();
            i += chunk;
        }
        let mut packets = Vec::new();
        loop {
            match enc.next_packet() {
                Ok(p) => packets.push(p),
                Err(Error::Again) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        enc.finish();
        loop {
            match enc.next_packet() {
                Ok(p) => packets.push(p),
                Err(Error::Eof) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(packets.len() >= 5, "expected multiple audio packets, got {}", packets.len());
        // The first three packets are the setup headers, byte-equal to `headers()`.
        let headers = enc.headers();
        for (h, p) in headers.iter().zip(&packets) {
            assert_eq!(&p.data, h, "header packet mismatch");
            assert_eq!(p.pts, 0);
            assert_eq!(p.duration, 0);
        }
        // Audio packets: granule advances one hop (1024) per packet.
        for (k, p) in packets[3..].iter().enumerate() {
            assert_eq!(p.pts, 1024 * (k as i64 + 1));
            assert_eq!(p.duration, 1024);
        }
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

    /// The generated ident header must carry the exact configured fields at the
    /// spec'd byte offsets (§4.2.2) — the parse-side twin of this assertion
    /// (lewton reading it back) lives in the rff-codec-vorbis adapter tests.
    #[test]
    fn ident_header_layout() {
        let h = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
        assert_eq!(h[0], 0x01);
        assert_eq!(&h[1..7], b"vorbis");
        assert_eq!(u32::from_le_bytes([h[7], h[8], h[9], h[10]]), 0); // version
        assert_eq!(h[11], 2); // channels
        assert_eq!(u32::from_le_bytes([h[12], h[13], h[14], h[15]]), 44_100);
        assert_eq!(
            i32::from_le_bytes([h[20], h[21], h[22], h[23]]),
            BITRATE_NOMINAL
        );
        assert_eq!(h[28], BS0_LOG2 | (BS1_LOG2 << 4)); // blocksizes
        assert_eq!(h[29], 0x01); // framing flag
        assert_eq!(h.len(), 30);
    }
}
