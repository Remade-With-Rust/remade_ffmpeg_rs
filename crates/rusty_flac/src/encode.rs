//! In-house **FLAC encoder** — lossless, pure Rust, no FFI.
//!
//! Ported from `rff-codec-flac` (built brick by brick; see that crate's
//! `docs/codec-flac-encoder.md` history). The port keeps the *decisions*
//! byte-identical to the original while replacing the primitives underneath:
//! an accumulator bit writer (was bit-by-bit), table CRCs (was bitwise),
//! cached apodization windows (was cos() per sample per subframe), an exact
//! bottom-up sum-merged Rice partition planner (was a full 15-parameter scan
//! per partition per order), and batched MD5 feeding (was per-sample rows).
//!
//! The encoder buffers the whole stream and emits a complete native FLAC
//! stream from [`Encoder::finish`] — framing, STREAMINFO and MD5 included.

use crate::bitio::BitWriter;
use crate::crc::{crc16, crc8};

/// Nominal samples-per-channel per FLAC frame. 4096 is FLAC's usual default and
/// encodes as an explicit 16-bit block size (frame-header block-size code 7).
const BLOCK_SIZE: usize = 4096;
/// Quantized LPC coefficient precision in bits.
const LPC_PRECISION: u32 = 14;
/// Highest LPC order searched — subset-compliant.
pub(crate) const LPC_MAX_ORDER: usize = 12;
/// Rice parameters searched. Method 0 (4-bit params) covers k 0..=14; method
/// 1 (Rice2, 5-bit params) extends to k 0..=30 — essential for high-entropy
/// 24-bit residuals, where the optimal parameter sits well above 14.
const RICE_KMAX: usize = 30;
/// Largest parameter expressible in a method-0 (4-bit) partition. 15/31 are
/// the escape codes and are never emitted.
const RICE_KMAX_M0: usize = 14;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Encoder configuration / stream errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// FLAC's channel-assignment field caps independent channels at 8.
    TooManyChannels(u32),
    /// Zero channels.
    NoChannels,
    /// Bits per sample outside the supported 8/16/24 set.
    UnsupportedBps(u32),
    /// Sample rate must fit STREAMINFO's 20-bit field and be non-zero.
    BadSampleRate(u32),
    /// push_interleaved got a slice whose length is not a channel multiple.
    RaggedInput,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::TooManyChannels(c) => write!(f, "flac: {c} channels (max 8)"),
            EncodeError::NoChannels => write!(f, "flac: zero channels"),
            EncodeError::UnsupportedBps(b) => write!(f, "flac: unsupported bit depth {b}"),
            EncodeError::BadSampleRate(r) => write!(f, "flac: bad sample rate {r}"),
            EncodeError::RaggedInput => {
                write!(f, "flac: interleaved length not a channel multiple")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Wiring-audit counters: every decision path in the encoder counts what it
/// chose, so a corpus run can prove no path is silently dead and no fallback
/// is silently hot. Cheap (a few increments per subframe).
#[derive(Debug, Default, Clone)]
pub struct EncodeStats {
    pub frames: u64,
    /// Chosen subframe kinds, over all written subframes.
    pub sub_constant: u64,
    pub sub_verbatim: u64,
    pub sub_fixed: u64,
    pub sub_lpc: u64,
    /// Stereo channel assignments chosen (stereo streams only).
    pub stereo_independent: u64,
    pub stereo_left_side: u64,
    pub stereo_right_side: u64,
    pub stereo_mid_side: u64,
    /// LPC machinery health.
    pub lpc_quantize_failed: u64,
    pub lpc_levinson_exhausted: u64,
    pub lpc_window_second_won: u64,
    /// Histogram of chosen partition orders (0..=8).
    pub partition_orders: [u64; 9],
    /// Fixed-predictor orders chosen (0..=4).
    pub fixed_orders: [u64; 5],
    /// Subframes that shifted out trailing zero bits (wasted-bits path).
    pub sub_wasted_bits: u64,
}

/// A pure-Rust FLAC encoder. Feed planar or interleaved `i32` samples at the
/// configured bit depth, then [`Encoder::finish`] returns the complete stream.
pub struct Encoder {
    sample_rate: u32,
    channels: usize,
    bps: u32,
    max_lpc_order: usize,
    chans: Vec<Vec<i32>>,
    stats: EncodeStats,
}

impl Encoder {
    pub fn new(sample_rate: u32, channels: u32, bits_per_sample: u32) -> Result<Self, EncodeError> {
        if channels == 0 {
            return Err(EncodeError::NoChannels);
        }
        if channels > 8 {
            return Err(EncodeError::TooManyChannels(channels));
        }
        if !matches!(bits_per_sample, 8 | 16 | 24) {
            return Err(EncodeError::UnsupportedBps(bits_per_sample));
        }
        if sample_rate == 0 || sample_rate >= (1 << 20) {
            return Err(EncodeError::BadSampleRate(sample_rate));
        }
        Ok(Encoder {
            sample_rate,
            channels: channels as usize,
            bps: bits_per_sample,
            max_lpc_order: LPC_MAX_ORDER,
            chans: vec![Vec::new(); channels as usize],
            stats: EncodeStats::default(),
        })
    }

    /// `0..=8`, the ffmpeg/libFLAC-style speed-vs-ratio knob (maps onto the max
    /// LPC order searched).
    pub fn set_compression_level(&mut self, level: u32) {
        self.max_lpc_order = if level <= 2 {
            4
        } else if level <= 5 {
            8
        } else {
            12
        };
    }

    /// Append interleaved samples (len must be a channel multiple).
    pub fn push_interleaved(&mut self, samples: &[i32]) -> Result<(), EncodeError> {
        let ch = self.channels;
        if samples.len() % ch != 0 {
            return Err(EncodeError::RaggedInput);
        }
        if ch == 1 {
            self.chans[0].extend_from_slice(samples);
            return Ok(());
        }
        let n = samples.len() / ch;
        for (c, chan) in self.chans.iter_mut().enumerate() {
            chan.reserve(n);
            chan.extend(samples[c..].iter().step_by(ch));
        }
        Ok(())
    }

    /// Append interleaved little-endian s16 PCM bytes (the WAV `data` layout)
    /// in one pass — no intermediate i32 buffer needed by the caller.
    pub fn push_s16le_bytes(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        let ch = self.channels;
        if bytes.len() % (2 * ch) != 0 {
            return Err(EncodeError::RaggedInput);
        }
        let frames = bytes.len() / (2 * ch);
        for chan in self.chans.iter_mut() {
            chan.reserve(frames);
        }
        match ch {
            1 => {
                self.chans[0].extend(
                    bytes
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32),
                );
            }
            2 => {
                let (l, r) = self.chans.split_at_mut(1);
                for c in bytes.chunks_exact(4) {
                    l[0].push(i16::from_le_bytes([c[0], c[1]]) as i32);
                    r[0].push(i16::from_le_bytes([c[2], c[3]]) as i32);
                }
            }
            _ => {
                for row in bytes.chunks_exact(2 * ch) {
                    for (c, chan) in self.chans.iter_mut().enumerate() {
                        chan.push(i16::from_le_bytes([row[c * 2], row[c * 2 + 1]]) as i32);
                    }
                }
            }
        }
        Ok(())
    }

    /// Append interleaved little-endian f32 PCM bytes, quantized onto this
    /// encoder's `bits_per_sample` grid (round-half-away, clamped) — the
    /// float-input convention shared with ffmpeg's flac encoder.
    pub fn push_f32le_bytes(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        let ch = self.channels;
        if bytes.len() % (4 * ch) != 0 {
            return Err(EncodeError::RaggedInput);
        }
        let scale = (1i64 << (self.bps - 1)) as f32;
        let frames = bytes.len() / (4 * ch);
        for chan in self.chans.iter_mut() {
            chan.reserve(frames);
        }
        let quant = |c: &[u8]| -> i32 {
            let s = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            (s * scale).round().clamp(-scale, scale - 1.0) as i32
        };
        match ch {
            1 => self.chans[0].extend(bytes.chunks_exact(4).map(quant)),
            2 => {
                let (l, r) = self.chans.split_at_mut(1);
                for c in bytes.chunks_exact(8) {
                    l[0].push(quant(&c[0..4]));
                    r[0].push(quant(&c[4..8]));
                }
            }
            _ => {
                for row in bytes.chunks_exact(4 * ch) {
                    for (c, chan) in self.chans.iter_mut().enumerate() {
                        chan.push(quant(&row[c * 4..c * 4 + 4]));
                    }
                }
            }
        }
        Ok(())
    }

    /// Append per-channel (planar) samples; all planes must be equal length.
    pub fn push_planar(&mut self, planes: &[&[i32]]) -> Result<(), EncodeError> {
        if planes.len() != self.channels {
            return Err(EncodeError::RaggedInput);
        }
        let n = planes[0].len();
        if planes.iter().any(|p| p.len() != n) {
            return Err(EncodeError::RaggedInput);
        }
        for (chan, plane) in self.chans.iter_mut().zip(planes) {
            chan.extend_from_slice(plane);
        }
        Ok(())
    }

    /// Encode all buffered samples into a complete native FLAC stream.
    pub fn finish(mut self) -> Vec<u8> {
        self.encode_stream()
    }

    /// Like [`Encoder::finish`], but also returns the wiring-audit counters.
    pub fn finish_with_stats(mut self) -> (Vec<u8>, EncodeStats) {
        let out = self.encode_stream();
        let stats = std::mem::take(&mut self.stats);
        (out, stats)
    }

    /// MD5 of the unencoded audio: interleaved samples, little-endian, at the
    /// coded bit depth — FLAC's STREAMINFO integrity signature.
    fn compute_md5(&self) -> [u8; 16] {
        let bytes_per = (self.bps / 8) as usize;
        let n = self.chans.first().map_or(0, |c| c.len());
        let ch = self.channels;
        let mut md5 = crate::md5::Md5::new();
        // Batch: build interleaved LE rows for a run of frames, hash per chunk.
        const CHUNK_FRAMES: usize = 16 * 1024;
        let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_FRAMES * ch * bytes_per);
        let mut i = 0usize;
        while i < n {
            let end = (i + CHUNK_FRAMES).min(n);
            buf.resize((end - i) * ch * bytes_per, 0);
            match (bytes_per, ch) {
                // The hot shapes fill a preallocated chunk (no per-sample
                // Vec bookkeeping); the rest go generic.
                (2, 1) => {
                    let a = &self.chans[0][i..end];
                    for (out, &v) in buf.chunks_exact_mut(2).zip(a) {
                        out.copy_from_slice(&(v as i16).to_le_bytes());
                    }
                }
                (2, 2) => {
                    let (l, r) = (&self.chans[0][i..end], &self.chans[1][i..end]);
                    for (j, out) in buf.chunks_exact_mut(4).enumerate() {
                        out[0..2].copy_from_slice(&(l[j] as i16).to_le_bytes());
                        out[2..4].copy_from_slice(&(r[j] as i16).to_le_bytes());
                    }
                }
                (3, 2) => {
                    let (l, r) = (&self.chans[0][i..end], &self.chans[1][i..end]);
                    for (j, out) in buf.chunks_exact_mut(6).enumerate() {
                        out[0..3].copy_from_slice(&l[j].to_le_bytes()[..3]);
                        out[3..6].copy_from_slice(&r[j].to_le_bytes()[..3]);
                    }
                }
                _ => {
                    for (j, out) in buf.chunks_exact_mut(ch * bytes_per).enumerate() {
                        for c in 0..ch {
                            out[c * bytes_per..(c + 1) * bytes_per]
                                .copy_from_slice(&self.chans[c][i + j].to_le_bytes()[..bytes_per]);
                        }
                    }
                }
            }
            md5.update(&buf);
            i = end;
        }
        md5.finalize()
    }

    fn encode_stream(&mut self) -> Vec<u8> {
        // RUSTY_FLAC_TIMING=1: print coarse stage shares to stderr (wiring
        // audit / campaign tool; zero cost when unset).
        let timing = std::env::var_os("RUSTY_FLAC_TIMING").is_some();
        let t0 = std::time::Instant::now();
        let n = self.chans.first().map_or(0, |c| c.len());
        let bps = self.bps;

        // Whole-stream output estimate: raw size is the ceiling for lossless.
        let raw = n * self.channels * (bps as usize / 8);
        let mut frames: Vec<u8> = Vec::with_capacity(raw / 2 + 4096);
        let (mut min_fs, mut max_fs) = (u32::MAX, 0u32);
        let mut frame_number = 0u64;
        let mut start = 0usize;
        let mut wins = WindowCache::default();
        while start < n {
            let bs = (n - start).min(BLOCK_SIZE);
            wins.ensure(bs);
            let frame = self.encode_frame(frame_number, start, bs, bps, &wins);
            min_fs = min_fs.min(frame.len() as u32);
            max_fs = max_fs.max(frame.len() as u32);
            frames.extend_from_slice(&frame);
            start += bs;
            frame_number += 1;
            self.stats.frames += 1;
        }
        if frames.is_empty() {
            min_fs = 0;
            max_fs = 0;
        }

        let t_frames = t0.elapsed();
        let t1 = std::time::Instant::now();

        // STREAMINFO (34 bytes). Block sizes are the NOMINAL blocking (the
        // spec's min/max exclude the final short block, and requires >= 16 —
        // a 5-sample stream still declares its nominal 4096, like libFLAC).
        let mut si = BitWriter::with_capacity(34);
        si.write_bits(BLOCK_SIZE as u64, 16);
        si.write_bits(BLOCK_SIZE as u64, 16);
        si.write_bits(min_fs as u64, 24);
        si.write_bits(max_fs as u64, 24);
        si.write_bits(self.sample_rate as u64, 20);
        si.write_bits((self.channels as u64) - 1, 3);
        si.write_bits((bps as u64) - 1, 5);
        si.write_bits(n as u64, 36);
        for &byte in &self.compute_md5() {
            si.write_bits(byte as u64, 8);
        }
        let si = si.into_bytes();
        if timing {
            eprintln!(
                "rusty_flac timing: frames {:.1} ms, md5+streaminfo {:.1} ms",
                t_frames.as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
        }

        let mut stream = Vec::with_capacity(4 + 4 + si.len() + frames.len());
        stream.extend_from_slice(b"fLaC");
        // Metadata block header: last-block=1, type=0 (STREAMINFO), length=34.
        stream.push(0x80);
        let len = si.len() as u32;
        stream.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        stream.extend_from_slice(&si);
        stream.extend_from_slice(&frames);
        stream
    }

    fn encode_frame(
        &mut self,
        frame_number: u64,
        start: usize,
        bs: usize,
        bps: u32,
        wins: &WindowCache,
    ) -> Vec<u8> {
        // Decide the channel layout: stereo picks the cheapest decorrelation
        // mode; mono / multichannel code each channel independently.
        let (assignment, subframes): (u64, Vec<(Vec<i32>, u32, SubframeChoice)>) =
            if self.channels == 2 {
                let (assignment, subs) = decide_stereo(
                    &self.chans[0][start..start + bs],
                    &self.chans[1][start..start + bs],
                    bps,
                    self.max_lpc_order,
                    wins,
                    &mut self.stats,
                );
                match assignment {
                    1 => self.stats.stereo_independent += 1,
                    8 => self.stats.stereo_left_side += 1,
                    9 => self.stats.stereo_right_side += 1,
                    _ => self.stats.stereo_mid_side += 1,
                }
                (assignment, subs)
            } else {
                let max_lpc_order = self.max_lpc_order;
                let chans = std::mem::take(&mut self.chans);
                let subs = (0..self.channels)
                    .map(|c| {
                        let arm = ArmInput::prepare(&chans[c][start..start + bs], bps);
                        let choice = analyze_subframe(&arm, max_lpc_order, wins, &mut self.stats);
                        let ebps = arm.ebps;
                        (arm.into_samples(), ebps, choice)
                    })
                    .collect();
                self.chans = chans;
                ((self.channels as u64) - 1, subs)
            };

        let mut bw = BitWriter::with_capacity(bs * self.channels * (bps as usize) / 8 / 2 + 64);
        // --- frame header ---
        bw.write_bits(0x3FFE, 14); // sync
        bw.write_bits(0, 1); // reserved (mandatory 0)
        bw.write_bits(0, 1); // blocking strategy: fixed block size
        bw.write_bits(7, 4); // block-size code 7 => explicit 16-bit (bs-1) below
        bw.write_bits(0, 4); // sample-rate code 0 => from STREAMINFO
        bw.write_bits(assignment, 4); // 0/1..7 = independent, 8/9/10 = L-S / R-S / M-S
        bw.write_bits(sample_size_code(bps), 3);
        bw.write_bits(0, 1); // reserved (mandatory 0)
        write_utf8(&mut bw, frame_number);
        bw.write_bits((bs as u64) - 1, 16); // block size - 1
        let hcrc = crc8(bw.bytes());
        bw.write_bits(hcrc as u64, 8);

        // --- subframes (each at its own bit depth; side channels use bps+1) ---
        for (samples, sf_bps, choice) in &subframes {
            write_subframe_from(&mut bw, samples, *sf_bps, choice, &mut self.stats);
        }

        // --- frame footer: pad to byte, then CRC-16 of the whole frame ---
        bw.align_to_byte();
        let fcrc = crc16(bw.bytes());
        bw.write_bits(fcrc as u64, 16);
        bw.into_bytes()
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// The two apodization windows tried per LPC candidate, cached per block size
/// (only the final short block differs from BLOCK_SIZE, so this rebuilds twice
/// per stream instead of twice per subframe).
#[derive(Default)]
struct WindowCache {
    n: usize,
    w: [Vec<f64>; 2],
}

const WINDOW_ALPHAS: [f64; 2] = [0.5, 0.2];

impl WindowCache {
    fn ensure(&mut self, n: usize) {
        if self.n == n {
            return;
        }
        self.n = n;
        for (slot, &alpha) in self.w.iter_mut().zip(&WINDOW_ALPHAS) {
            *slot = tukey_window(n, alpha);
        }
    }
}

/// Tukey apodization window: flat middle with cosine tapers.
fn tukey_window(n: usize, alpha: f64) -> Vec<f64> {
    let mut w = vec![1.0f64; n];
    if n <= 1 {
        return w;
    }
    for (i, wi) in w.iter_mut().enumerate() {
        let x = i as f64 / (n - 1) as f64;
        if x < alpha / 2.0 {
            *wi = 0.5 * (1.0 + (std::f64::consts::PI * (2.0 * x / alpha - 1.0)).cos());
        } else if x > 1.0 - alpha / 2.0 {
            *wi =
                0.5 * (1.0 + (std::f64::consts::PI * (2.0 * x / alpha - 2.0 / alpha + 1.0)).cos());
        }
    }
    w
}

// ---------------------------------------------------------------------------
// Frame-header helpers
// ---------------------------------------------------------------------------

/// FLAC's UTF-8-style coding of the frame number (fixed blocking strategy).
fn write_utf8(bw: &mut BitWriter, val: u64) {
    if val < 0x80 {
        bw.write_bits(val, 8);
        return;
    }
    let nconts: u32 = if val < 0x800 {
        1
    } else if val < 0x1_0000 {
        2
    } else if val < 0x20_0000 {
        3
    } else if val < 0x400_0000 {
        4
    } else {
        5
    };
    let lead_ones = nconts + 1;
    let prefix = (((1u64 << lead_ones) - 1) << (8 - lead_ones)) & 0xFF;
    bw.write_bits(prefix | (val >> (6 * nconts)), 8);
    for i in (0..nconts).rev() {
        bw.write_bits(0x80 | ((val >> (6 * i)) & 0x3F), 8);
    }
}

/// FLAC frame-header sample-size code for a bit depth.
fn sample_size_code(bps: u32) -> u64 {
    match bps {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Residual coding — exact Rice costs via per-partition shifted sums
// ---------------------------------------------------------------------------

/// Zigzag-fold a signed residual to the unsigned value FLAC Rice-codes.
#[inline]
fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// `sums[k] = Σ (zigzag(v) >> k)` over a residual slice, for k = 0..=14.
/// The exact Rice bit cost at parameter k is `sums[k] + cnt·(1 + k)` — one
/// pass yields every parameter's exact cost. Integer sums, so the AVX2 path
/// is exact (gated by `rice_sums_avx2_matches_scalar`).
#[inline]
fn rice_sums(res: &[i32]) -> [u64; RICE_KMAX + 1] {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: guarded by the runtime AVX2 check.
            return unsafe { rice_sums_avx2(res) };
        }
    }
    rice_sums_scalar(res)
}

fn rice_sums_scalar(res: &[i32]) -> [u64; RICE_KMAX + 1] {
    let mut sums = [0u64; RICE_KMAX + 1];
    for &v in res {
        let u = zigzag(v);
        for (k, s) in sums.iter_mut().enumerate() {
            *s += (u >> k) as u64;
        }
    }
    sums
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn rice_sums_avx2(res: &[i32]) -> [u64; RICE_KMAX + 1] {
    use std::arch::x86_64::*;
    // 16 u64 accumulator lanes for k 0..15; k 16..=30 is folded from lane 15
    // afterwards by re-scanning IF any residual is big enough to need it
    // (rare outside high-entropy 24-bit content), so the common path stays 4
    // shift/add register pairs per sample.
    let sh0 = _mm256_setr_epi64x(0, 1, 2, 3);
    let sh1 = _mm256_setr_epi64x(4, 5, 6, 7);
    let sh2 = _mm256_setr_epi64x(8, 9, 10, 11);
    let sh3 = _mm256_setr_epi64x(12, 13, 14, 15);
    let mut a0 = _mm256_setzero_si256();
    let mut a1 = _mm256_setzero_si256();
    let mut a2 = _mm256_setzero_si256();
    let mut a3 = _mm256_setzero_si256();
    let mut or_acc = 0u32;
    for &v in res {
        let u = zigzag(v);
        or_acc |= u;
        let b = _mm256_set1_epi64x(u as i64);
        a0 = _mm256_add_epi64(a0, _mm256_srlv_epi64(b, sh0));
        a1 = _mm256_add_epi64(a1, _mm256_srlv_epi64(b, sh1));
        a2 = _mm256_add_epi64(a2, _mm256_srlv_epi64(b, sh2));
        a3 = _mm256_add_epi64(a3, _mm256_srlv_epi64(b, sh3));
    }
    let mut lanes = [0u64; 16];
    _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, a0);
    _mm256_storeu_si256(lanes.as_mut_ptr().add(4) as *mut __m256i, a1);
    _mm256_storeu_si256(lanes.as_mut_ptr().add(8) as *mut __m256i, a2);
    _mm256_storeu_si256(lanes.as_mut_ptr().add(12) as *mut __m256i, a3);
    let mut sums = [0u64; RICE_KMAX + 1];
    sums[..16].copy_from_slice(&lanes);
    // High-k tail: only when some residual exceeds 16 bits after folding, and
    // only up to the top set bit (sums above it are zero by construction).
    if or_acc >> 16 != 0 {
        let top = (32 - or_acc.leading_zeros() as usize).min(RICE_KMAX);
        for &v in res {
            let u = zigzag(v);
            for (k, s) in sums.iter_mut().enumerate().take(top + 1).skip(16) {
                *s += (u >> k) as u64;
            }
        }
    }
    sums
}

/// Best Rice parameter within `0..=kmax` (lowest k on ties) and its exact
/// body bit cost, from precomputed shifted sums. The cost is convex in k
/// (unary halves, suffix grows by cnt), so the scan stops at the first rise.
#[inline]
fn best_k_from_sums(sums: &[u64; RICE_KMAX + 1], cnt: u64, kmax: usize) -> (u32, u64) {
    let mut best_k = 0u32;
    let mut best = sums[0] + cnt;
    for (k, &s) in sums.iter().enumerate().take(kmax + 1).skip(1) {
        let b = s + cnt * (1 + k as u64);
        if b < best {
            best = b;
            best_k = k as u32;
        } else {
            break; // convex: it only grows from here
        }
    }
    (best_k, best)
}

/// Rice-code one residual: quotient in unary, then the low `k` bits. The
/// common case (short quotient) fuses unary + stop bit + low bits into one
/// accumulator write.
#[inline]
fn write_rice(bw: &mut BitWriter, v: i32, k: u32) {
    let u = zigzag(v);
    let q = u >> k;
    let total = q + 1 + k;
    if total <= 56 {
        let low = (u as u64) & ((1u64 << k) - 1);
        bw.write_bits((1u64 << k) | low, total);
    } else {
        bw.write_zeros(q);
        bw.write_bits(1, 1);
        if k > 0 {
            bw.write_bits((u & ((1u32 << k) - 1)) as u64, k);
        }
    }
}

/// A residual coding plan: the coding method (0 = 4-bit Rice params, 1 =
/// Rice2 5-bit params), the chosen partition order, per-partition parameters,
/// and the residual-body bit cost (Σ param-field + Rice codes).
struct ResidualPlan {
    method: u32,
    partition_order: u32,
    ks: Vec<u32>,
    bits: u64,
}

/// Largest usable partition order for a `bs`-sample block with predictor order
/// `p`. Capped at 8 (256 partitions).
fn max_partition_order(bs: usize, p: usize) -> u32 {
    let mut po = 0u32;
    while po < 8 {
        let next = po + 1;
        if bs & ((1usize << next) - 1) != 0 {
            break; // bs not a multiple of 2^next
        }
        if (bs >> next) <= p {
            break; // partition 0 would be empty
        }
        po = next;
    }
    po
}

/// Choose the best partition order + per-partition Rice parameters, with costs
/// identical to an independent exhaustive scan per order (the original), but
/// computed in ONE pass: shifted sums per finest partition, merged pairwise
/// upward — O(15n) total instead of O(15n) per order.
fn plan_partitions(res: &[i32], bs: usize, p: usize) -> ResidualPlan {
    let max_po = max_partition_order(bs, p);
    let finest_parts = 1usize << max_po;
    let finest_size = bs >> max_po;

    thread_local! {
        // Partition-sum scratch, reused across every plan on this thread.
        static SUMS: std::cell::RefCell<Vec<[u64; RICE_KMAX + 1]>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    SUMS.with(|cell| {
        let mut sums = cell.borrow_mut();
        sums.clear();
        sums.reserve(finest_parts);
        let mut idx = 0usize;
        for part in 0..finest_parts {
            let cnt = if part == 0 {
                finest_size - p
            } else {
                finest_size
            };
            sums.push(rice_sums(&res[idx..idx + cnt]));
            idx += cnt;
        }

        // Cost one level from `sums[..n_part]`, both methods (method 0 pays
        // 4 bits/param but caps k at 14; Rice2 pays 5 for k up to 30).
        let cost_level = |sums: &[[u64; RICE_KMAX + 1]], po: u32| -> (u32, Vec<u32>, u64) {
            let psize = bs >> po;
            let mut ks0 = Vec::with_capacity(sums.len());
            let mut ks1 = Vec::with_capacity(sums.len());
            let (mut bits0, mut bits1) = (0u64, 0u64);
            for (part, s) in sums.iter().enumerate() {
                let cnt = if part == 0 { psize - p } else { psize } as u64;
                let (k1, kb1) = best_k_from_sums(s, cnt, RICE_KMAX);
                let (k0, kb0) = if k1 as usize <= RICE_KMAX_M0 {
                    (k1, kb1)
                } else {
                    best_k_from_sums(s, cnt, RICE_KMAX_M0)
                };
                ks0.push(k0);
                ks1.push(k1);
                bits0 += 4 + kb0;
                bits1 += 5 + kb1;
            }
            if bits1 < bits0 {
                (1, ks1, bits1)
            } else {
                (0, ks0, bits0)
            }
        };

        // Evaluate from the finest level down, merging pairs in place. Taking
        // ties with `<=` while descending reproduces the ascending strict-<
        // search's lowest-po-wins-ties rule.
        let mut best = ResidualPlan {
            method: 0,
            partition_order: 0,
            ks: Vec::new(),
            bits: u64::MAX,
        };
        let mut po = max_po;
        loop {
            let n_part = 1usize << po;
            let (method, ks, bits) = cost_level(&sums[..n_part], po);
            if bits <= best.bits {
                best = ResidualPlan {
                    method,
                    partition_order: po,
                    ks,
                    bits,
                };
            }
            if po == 0 {
                break;
            }
            // Merge pairs into the front half for the next-coarser level.
            for i in 0..n_part / 2 {
                let (a, b) = sums.split_at_mut(2 * i + 1);
                let dst = &mut a[2 * i];
                for (x, y) in dst.iter_mut().zip(&b[0]) {
                    *x += y;
                }
                if i != 2 * i {
                    sums.swap(i, 2 * i);
                }
            }
            po -= 1;
        }
        best
    })
}

/// Write a partitioned Rice residual body.
fn write_partitioned_residual(
    bw: &mut BitWriter,
    res: &[i32],
    bs: usize,
    p: usize,
    plan: &ResidualPlan,
) {
    let n_part = 1usize << plan.partition_order;
    let psize = bs >> plan.partition_order;
    let param_bits = 4 + plan.method;
    let mut idx = 0usize;
    for part in 0..n_part {
        let cnt = if part == 0 { psize - p } else { psize };
        let k = plan.ks[part];
        bw.write_bits(k as u64, param_bits);
        for &r in &res[idx..idx + cnt] {
            write_rice(bw, r, k);
        }
        idx += cnt;
    }
}

// ---------------------------------------------------------------------------
// LPC
// ---------------------------------------------------------------------------

/// Autocorrelation of the windowed samples, lags 0..=max_order.
///
/// The summation uses four striped accumulators reduced as
/// `(a0+a1) + (a2+a3)` — the same order in the scalar twin and the AVX2
/// kernel, so the two are bit-identical and the kernel is gated by direct
/// comparison (`autocorr_avx2_matches_scalar`).
fn autocorrelation(samples: &[i32], max_order: usize, win: &[f64]) -> Vec<f64> {
    thread_local! {
        // Windowed-product scratch, reused across every subframe analysis on
        // this thread (a fresh Vec per call was ~8 × 32 KB allocations per
        // block).
        static W_SCRATCH: std::cell::RefCell<Vec<f64>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    W_SCRATCH.with(|cell| {
        let mut w = cell.borrow_mut();
        w.clear();
        w.extend(samples.iter().zip(win).map(|(&s, &g)| s as f64 * g));
        let mut autoc = vec![0.0f64; max_order + 1];
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: guarded by the runtime AVX2 check.
                unsafe { autocorr_avx2(&w, &mut autoc) };
                return autoc;
            }
        }
        autocorr_scalar(&w, &mut autoc);
        autoc
    })
}

/// Scalar twin of the AVX2 kernel: identical striping, identical reduction.
fn autocorr_scalar(w: &[f64], autoc: &mut [f64]) {
    let n = w.len();
    for (lag, a) in autoc.iter_mut().enumerate() {
        let m = n - lag;
        let mut acc = [0.0f64; 4];
        let chunks = m / 4;
        for c in 0..chunks {
            let i = c * 4;
            acc[0] += w[lag + i] * w[i];
            acc[1] += w[lag + i + 1] * w[i + 1];
            acc[2] += w[lag + i + 2] * w[i + 2];
            acc[3] += w[lag + i + 3] * w[i + 3];
        }
        let mut sum = (acc[0] + acc[1]) + (acc[2] + acc[3]);
        for i in chunks * 4..m {
            sum += w[lag + i] * w[i];
        }
        *a = sum;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn autocorr_avx2(w: &[f64], autoc: &mut [f64]) {
    use std::arch::x86_64::*;
    let n = w.len();
    let p = w.as_ptr();
    for (lag, a) in autoc.iter_mut().enumerate() {
        let m = n - lag;
        let chunks = m / 4;
        let mut acc = _mm256_setzero_pd();
        for c in 0..chunks {
            let i = c * 4;
            let x = _mm256_loadu_pd(p.add(lag + i));
            let y = _mm256_loadu_pd(p.add(i));
            // Plain mul+add (no FMA) so the scalar twin matches bit-for-bit.
            acc = _mm256_add_pd(acc, _mm256_mul_pd(x, y));
        }
        // Reduce as (a0+a1) + (a2+a3), matching the scalar twin.
        let lo = _mm256_castpd256_pd128(acc);
        let hi = _mm256_extractf128_pd(acc, 1);
        let a01 = _mm_add_pd(lo, _mm_unpackhi_pd(lo, lo));
        let a23 = _mm_add_pd(hi, _mm_unpackhi_pd(hi, hi));
        let mut sum = _mm_cvtsd_f64(a01) + _mm_cvtsd_f64(a23);
        for i in chunks * 4..m {
            sum += *p.add(lag + i) * *p.add(i);
        }
        *a = sum;
    }
}

/// Levinson-Durbin, error-only pass: fills `errs[i]` with the residual energy
/// after order i+1 and returns how many orders were reachable before the
/// recursion exhausted numerically. No per-order coefficient allocation —
/// [`levinson_coeffs`] re-derives the chosen order's coefficients on demand
/// (O(order²), amortized to nothing next to the O(order·n) autocorrelation).
fn levinson_errs(autoc: &[f64], max_order: usize, errs: &mut [f64; 32]) -> usize {
    let mut lpc = [0.0f64; 32];
    let mut err = autoc[0];
    let mut found = 0usize;
    for i in 0..max_order {
        if err <= 0.0 {
            break; // numerically exhausted; keep the orders found so far
        }
        let mut r = -autoc[i + 1];
        for j in 0..i {
            r -= lpc[j] * autoc[i - j];
        }
        r /= err;
        lpc[i] = r;
        for j in 0..(i / 2) {
            let tmp = lpc[j];
            lpc[j] = tmp + r * lpc[i - 1 - j];
            lpc[i - 1 - j] += r * tmp;
        }
        if i & 1 == 1 {
            lpc[i / 2] += r * lpc[i / 2];
        }
        err *= 1.0 - r * r;
        errs[i] = err;
        found = i + 1;
    }
    found
}

/// Coefficients for one specific order, re-running the recursion. The
/// PREDICTOR coefficients are the negation of the AR solution (libFLAC's
/// `lp_coeff = -lpc`).
fn levinson_coeffs(autoc: &[f64], order: usize) -> Vec<f64> {
    let mut lpc = [0.0f64; 32];
    let mut err = autoc[0];
    for i in 0..order {
        debug_assert!(err > 0.0, "caller checked reachability via levinson_errs");
        let mut r = -autoc[i + 1];
        for j in 0..i {
            r -= lpc[j] * autoc[i - j];
        }
        r /= err;
        lpc[i] = r;
        for j in 0..(i / 2) {
            let tmp = lpc[j];
            lpc[j] = tmp + r * lpc[i - 1 - j];
            lpc[i - 1 - j] += r * tmp;
        }
        if i & 1 == 1 {
            lpc[i / 2] += r * lpc[i / 2];
        }
        err *= 1.0 - r * r;
    }
    lpc[..order].iter().map(|&c| -c).collect()
}

/// Quantize float LPC coefficients to `precision`-bit integers + a NON-negative
/// shift, with libFLAC-style rounding error feedback.
fn quantize_lpc(lpc: &[f64], precision: u32) -> Option<(Vec<i32>, i32)> {
    let cmax = lpc.iter().fold(0.0f64, |m, &c| m.max(c.abs()));
    if !cmax.is_finite() || cmax <= 0.0 {
        return None;
    }
    let exp = cmax.log2().floor() as i32 + 1; // frexp exponent of cmax
    let shift = (precision as i32 - exp - 1).clamp(0, 15);
    let qmax = (1i32 << (precision - 1)) - 1;
    let qmin = -(1i32 << (precision - 1));
    let scale = (shift as f64).exp2();
    let mut error = 0.0f64;
    let mut qlp = Vec::with_capacity(lpc.len());
    for &c in lpc {
        let v = c * scale + error;
        let q = v.round().clamp(qmin as f64, qmax as f64);
        error = v - q;
        qlp.push(q as i32);
    }
    if qlp.iter().all(|&q| q == 0) {
        return None; // no predictive power left after quantization
    }
    Some((qlp, shift))
}

/// LPC residual using the quantized coefficients — exact arithmetic the
/// decoder inverts, so it round-trips losslessly.
///
/// The AVX2 path runs the dot products in f64 FMA lanes: with |sample| < 2^25
/// and 14-bit-plus-sign coefficients every product is < 2^39 and every partial
/// sum of ≤32 terms is < 2^44 — integers well inside f64's exact range, so
/// FMA ordering cannot change a bit, and `floor(sum · 2^-shift)` equals the
/// arithmetic shift (gated by `lpc_residual_avx2_matches_scalar`).
fn lpc_residual(samples: &[i32], qlp: &[i32], shift: i32, order: usize) -> Vec<i32> {
    #[cfg(target_arch = "x86_64")]
    {
        // Exactness guard: the vector path converts the prediction to i32
        // with saturation, while the scalar/decoder truncate — identical only
        // while |Σ|c|·2^25 >> shift| stays inside i32 (always true for sane
        // predictors; degenerate quantizations fall back to scalar).
        let sum_abs: i64 = qlp[..order].iter().map(|&c| (c as i64).abs()).sum();
        let in_range = (sum_abs << 25) >> shift < (1i64 << 31);
        if in_range
            && samples.len() > order + 8
            && std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("fma")
        {
            // SAFETY: guarded by the runtime AVX2+FMA check.
            return unsafe { lpc_residual_avx2(samples, qlp, shift, order) };
        }
    }
    lpc_residual_scalar(samples, qlp, shift, order)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn lpc_residual_avx2(samples: &[i32], qlp: &[i32], shift: i32, order: usize) -> Vec<i32> {
    use std::arch::x86_64::*;
    let n = samples.len();
    let mut res: Vec<i32> = Vec::with_capacity(n - order);
    let mut coeffs = [0.0f64; 32];
    for j in 0..order {
        coeffs[j] = qlp[j] as f64;
    }
    let scale = _mm256_set1_pd((-(shift as f64)).exp2()); // 2^-shift, exact
    let p = samples.as_ptr();
    let mut i = order;
    while i + 4 <= n {
        let mut sum = _mm256_setzero_pd();
        for (j, &c) in coeffs[..order].iter().enumerate() {
            let x = _mm256_cvtepi32_pd(_mm_loadu_si128(p.add(i - 1 - j) as *const __m128i));
            sum = _mm256_fmadd_pd(x, _mm256_set1_pd(c), sum);
        }
        // pred = floor(sum / 2^shift) == sum >> shift (arithmetic).
        let pred = _mm256_floor_pd(_mm256_mul_pd(sum, scale));
        let predi = _mm256_cvtpd_epi32(pred); // exact: pred is integral, |pred| < 2^31
        let s = _mm_loadu_si128(p.add(i) as *const __m128i);
        let r = _mm_sub_epi32(s, predi);
        let mut out4 = [0i32; 4];
        _mm_storeu_si128(out4.as_mut_ptr() as *mut __m128i, r);
        res.extend_from_slice(&out4);
        i += 4;
    }
    for j in i..n {
        let mut sum: i64 = 0;
        for (k, &c) in qlp[..order].iter().enumerate() {
            sum += c as i64 * *p.add(j - 1 - k) as i64;
        }
        res.push(*p.add(j) - (sum >> shift) as i32);
    }
    res
}

fn lpc_residual_scalar(samples: &[i32], qlp: &[i32], shift: i32, order: usize) -> Vec<i32> {
    #[inline(always)]
    fn run<const ORDER: usize>(samples: &[i32], qlp: &[i32], shift: i32) -> Vec<i32> {
        let mut res = Vec::with_capacity(samples.len() - ORDER);
        // i32 coefficients so every product is a 32×32→64 widening multiply
        // (the pattern LLVM lowers to pmuldq lanes).
        let mut coeffs = [0i32; 32];
        coeffs[..ORDER].copy_from_slice(&qlp[..ORDER]);
        for i in ORDER..samples.len() {
            let mut sum: i64 = 0;
            for j in 0..ORDER {
                sum += coeffs[j] as i64 * samples[i - 1 - j] as i64;
            }
            res.push(samples[i] - (sum >> shift) as i32);
        }
        res
    }
    match order {
        1 => run::<1>(samples, qlp, shift),
        2 => run::<2>(samples, qlp, shift),
        3 => run::<3>(samples, qlp, shift),
        4 => run::<4>(samples, qlp, shift),
        5 => run::<5>(samples, qlp, shift),
        6 => run::<6>(samples, qlp, shift),
        7 => run::<7>(samples, qlp, shift),
        8 => run::<8>(samples, qlp, shift),
        9 => run::<9>(samples, qlp, shift),
        10 => run::<10>(samples, qlp, shift),
        11 => run::<11>(samples, qlp, shift),
        12 => run::<12>(samples, qlp, shift),
        _ => {
            let mut res = Vec::with_capacity(samples.len() - order);
            for i in order..samples.len() {
                let mut sum: i64 = 0;
                for j in 0..order {
                    sum += qlp[j] as i64 * samples[i - 1 - j] as i64;
                }
                res.push(samples[i] - (sum >> shift) as i32);
            }
            res
        }
    }
}

/// A complete LPC subframe candidate + its total bit cost.
struct LpcCandidate {
    order: usize,
    qlp: Vec<i32>,
    shift: i32,
    res: Vec<i32>,
    plan: ResidualPlan,
    bits: u64,
}

/// An estimated (not yet realized) LPC candidate: chosen order + float
/// coefficients + the Levinson bit estimate that ranked it.
#[derive(Clone)]
struct LpcEstimate {
    order: usize,
    coeffs: Vec<f64>,
    est_bits: f64,
}

/// When two windows' estimates are within this relative margin, both are
/// realized exactly and compared — outside it, only the estimated winner is.
/// (The second window wins ~63% of subframes on real music, so it can never
/// be dropped outright; this only prunes the clear-loser realizations.)
const WINDOW_EST_MARGIN: f64 = 0.02;

/// Realize the best LPC subframe from precomputed per-window estimates:
/// realize the estimated winner exactly, and a runner-up only when its
/// estimate is within [`WINDOW_EST_MARGIN`]. None if degenerate.
fn realize_best_window(
    samples: &[i32],
    bps: u32,
    ests: &[Option<LpcEstimate>],
    stats: &mut EncodeStats,
) -> Option<LpcCandidate> {
    let best_est = ests
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_ref().map(|e| (i, e.est_bits)))
        .min_by(|a, b| a.1.total_cmp(&b.1))?
        .0;

    let mut best: Option<(usize, LpcCandidate)> = None;
    for (widx, est) in ests.iter().enumerate() {
        let Some(est) = est else { continue };
        if widx != best_est {
            let winner = ests[best_est].as_ref().expect("winner exists").est_bits;
            if est.est_bits > winner * (1.0 + WINDOW_EST_MARGIN) {
                continue; // clear loser: skip the expensive realization
            }
        }
        if let Some(c) = realize_lpc(samples, bps, est, stats) {
            if best.as_ref().is_none_or(|(_, b)| c.bits < b.bits) {
                best = Some((widx, c));
            }
        }
    }
    let (widx, cand) = best?;
    if widx == 1 {
        stats.lpc_window_second_won += 1;
    }
    Some(cand)
}

/// The cheap half of an LPC candidate: autocorrelation + Levinson + order
/// selection from residual energy. No residual computed yet.
fn lpc_estimate(
    samples: &[i32],
    bps: u32,
    max_order: usize,
    win: &[f64],
    stats: &mut EncodeStats,
) -> Option<LpcEstimate> {
    let n = samples.len();
    let autoc = autocorrelation(samples, max_order, win);
    if autoc[0] <= 0.0 {
        return None;
    }
    let mut errs = [0.0f64; 32];
    let found = levinson_errs(&autoc, max_order, &mut errs);
    if found == 0 {
        return None;
    }
    if found < max_order {
        stats.lpc_levinson_exhausted += 1;
    }
    // Pick the order from the Levinson residual energy (header cost vs the
    // entropy of a residual with that variance).
    let mut best_idx = 0usize;
    let mut best_est = f64::INFINITY;
    for (idx, &err) in errs[..found].iter().enumerate() {
        let order = idx + 1;
        let var = err / n as f64;
        let bits_per = if var > 0.0 {
            (0.5 * var.log2()).max(0.0)
        } else {
            0.0
        };
        let est = order as f64 * (bps + LPC_PRECISION) as f64 + bits_per * (n - order) as f64;
        if est < best_est {
            best_est = est;
            best_idx = idx;
        }
    }
    let coeffs = levinson_coeffs(&autoc, best_idx + 1);
    Some(LpcEstimate {
        order: best_idx + 1,
        coeffs,
        est_bits: best_est,
    })
}

/// The expensive half: quantize, compute the exact residual, plan partitions.
fn realize_lpc(
    samples: &[i32],
    bps: u32,
    est: &LpcEstimate,
    stats: &mut EncodeStats,
) -> Option<LpcCandidate> {
    let n = samples.len();
    let order = est.order;
    let Some((qlp, shift)) = quantize_lpc(&est.coeffs, LPC_PRECISION) else {
        stats.lpc_quantize_failed += 1;
        return None;
    };
    let res = lpc_residual(samples, &qlp, shift, order);
    let plan = plan_partitions(&res, n, order);
    // hdr(8) + warm-up + precision(4) + shift(5) + coeffs + residual hdr(6) + body.
    let bits =
        8 + order as u64 * bps as u64 + 4 + 5 + order as u64 * LPC_PRECISION as u64 + 6 + plan.bits;
    Some(LpcCandidate {
        order,
        qlp,
        shift,
        res,
        plan,
        bits,
    })
}

// ---------------------------------------------------------------------------
// Subframe selection
// ---------------------------------------------------------------------------

/// One-pass FIXED-order estimator: |residual| sums for orders 0..=4 via the
/// direct difference formulas — no allocation, single sweep (the libFLAC
/// order-selection method). Returns the chosen order and its |residual| sum.
/// Integer math, so the AVX2 kernel is exact (gated by
/// `fixed_sums_avx2_matches_scalar`).
fn fixed_order_estimate(samples: &[i32]) -> (usize, u64) {
    let n = samples.len();
    let max_order = 4.min(n.saturating_sub(1));
    #[cfg(target_arch = "x86_64")]
    let sums = if n >= 16 && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: guarded by the runtime AVX2 check.
        unsafe { fixed_sums_avx2(samples) }
    } else {
        fixed_sums_scalar(samples)
    };
    #[cfg(not(target_arch = "x86_64"))]
    let sums = fixed_sums_scalar(samples);

    let mut best = 0usize;
    for order in 1..=max_order {
        if sums[order] < sums[best] {
            best = order;
        }
    }
    (best, sums[best])
}

fn fixed_sums_scalar(samples: &[i32]) -> [u64; 5] {
    let n = samples.len();
    let mut sums = [0u64; 5];
    sums[0] = samples.iter().map(|&v| (v as i64).unsigned_abs()).sum();
    // Ramp-in: orders become defined at i >= order.
    for i in 1..n.min(4) {
        let s = |j: usize| samples[i - j] as i64;
        sums[1] += (s(0) - s(1)).unsigned_abs();
        if i >= 2 {
            sums[2] += (s(0) - 2 * s(1) + s(2)).unsigned_abs();
        }
        if i >= 3 {
            sums[3] += (s(0) - 3 * s(1) + 3 * s(2) - s(3)).unsigned_abs();
        }
    }
    for i in 4..n {
        let s0 = samples[i] as i64;
        let s1 = samples[i - 1] as i64;
        let s2 = samples[i - 2] as i64;
        let s3 = samples[i - 3] as i64;
        let s4 = samples[i - 4] as i64;
        sums[1] += (s0 - s1).unsigned_abs();
        sums[2] += (s0 - 2 * s1 + s2).unsigned_abs();
        sums[3] += (s0 - 3 * s1 + 3 * s2 - s3).unsigned_abs();
        sums[4] += (s0 - 4 * s1 + 6 * s2 - 4 * s3 + s4).unsigned_abs();
    }
    sums
}

/// 8-lane i32 differences (nested first-differences give every order), then
/// abs + widening u64 accumulation. Bounded: |sample| < 2^25 (24-bit + side),
/// order-4 coefficient sum 16 ⇒ every intermediate fits i32.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn fixed_sums_avx2(samples: &[i32]) -> [u64; 5] {
    use std::arch::x86_64::*;
    let n = samples.len();
    debug_assert!(n >= 16);

    #[inline(always)]
    unsafe fn accum(acc: &mut __m256i, v: __m256i) {
        // |i32| widened to 4+4 u64 lanes and added.
        let a = _mm256_abs_epi32(v);
        let lo = _mm256_cvtepu32_epi64(_mm256_castsi256_si128(a));
        let hi = _mm256_cvtepu32_epi64(_mm256_extracti128_si256(a, 1));
        *acc = _mm256_add_epi64(*acc, _mm256_add_epi64(lo, hi));
    }
    #[inline(always)]
    unsafe fn hsum(acc: __m256i) -> u64 {
        let lo = _mm256_castsi256_si128(acc);
        let hi = _mm256_extracti128_si256(acc, 1);
        let s = _mm_add_epi64(lo, hi);
        (_mm_cvtsi128_si64(s) as u64).wrapping_add(_mm_extract_epi64(s, 1) as u64)
    }

    let p = samples.as_ptr();
    let mut a0 = _mm256_setzero_si256();
    let mut a1 = _mm256_setzero_si256();
    let mut a2 = _mm256_setzero_si256();
    let mut a3 = _mm256_setzero_si256();
    let mut a4 = _mm256_setzero_si256();
    let mut i = 4usize;
    while i + 8 <= n {
        let s0 = _mm256_loadu_si256(p.add(i) as *const __m256i);
        let s1 = _mm256_loadu_si256(p.add(i - 1) as *const __m256i);
        let s2 = _mm256_loadu_si256(p.add(i - 2) as *const __m256i);
        let s3 = _mm256_loadu_si256(p.add(i - 3) as *const __m256i);
        let s4 = _mm256_loadu_si256(p.add(i - 4) as *const __m256i);
        let r1 = _mm256_sub_epi32(s0, s1);
        let d1 = _mm256_sub_epi32(s1, s2); // r1 shifted one sample back
        let r2 = _mm256_sub_epi32(r1, d1);
        let d2 = _mm256_sub_epi32(d1, _mm256_sub_epi32(s2, s3)); // r2 shifted
        let r3 = _mm256_sub_epi32(r2, d2);
        let e2 = _mm256_sub_epi32(_mm256_sub_epi32(s2, s3), _mm256_sub_epi32(s3, s4));
        let d3 = _mm256_sub_epi32(d2, e2); // r3 shifted
        let r4 = _mm256_sub_epi32(r3, d3);
        accum(&mut a0, s0);
        accum(&mut a1, r1);
        accum(&mut a2, r2);
        accum(&mut a3, r3);
        accum(&mut a4, r4);
        i += 8;
    }
    let mut sums = [hsum(a0), hsum(a1), hsum(a2), hsum(a3), hsum(a4)];

    // Head (order-0 covers 0..4 + ramp-in of orders 1..3) and tail, scalar.
    for &v in &samples[..4.min(n)] {
        sums[0] += (v as i64).unsigned_abs();
    }
    for j in 1..n.min(4) {
        let s = |k: usize| samples[j - k] as i64;
        sums[1] += (s(0) - s(1)).unsigned_abs();
        if j >= 2 {
            sums[2] += (s(0) - 2 * s(1) + s(2)).unsigned_abs();
        }
        if j >= 3 {
            sums[3] += (s(0) - 3 * s(1) + 3 * s(2) - s(3)).unsigned_abs();
        }
    }
    for j in i..n {
        let s0 = samples[j] as i64;
        let s1 = samples[j - 1] as i64;
        let s2 = samples[j - 2] as i64;
        let s3 = samples[j - 3] as i64;
        let s4 = samples[j - 4] as i64;
        sums[0] += s0.unsigned_abs();
        sums[1] += (s0 - s1).unsigned_abs();
        sums[2] += (s0 - 2 * s1 + s2).unsigned_abs();
        sums[3] += (s0 - 3 * s1 + 3 * s2 - s3).unsigned_abs();
        sums[4] += (s0 - 4 * s1 + 6 * s2 - 4 * s3 + s4).unsigned_abs();
    }
    sums
}

/// Estimated single-partition Rice bit cost from a |residual| sum: pick the
/// parameter from the folded mean and price `Σ(u>>k) ≈ (Σu)>>k` (error < cnt).
fn rice_bits_estimate(abs_sum: u64, cnt: u64) -> u64 {
    if cnt == 0 {
        return 0;
    }
    let usum = abs_sum.saturating_mul(2); // zigzag(v) ∈ {2|v|, 2|v|−1}
    let mean = usum / cnt;
    let k = if mean > 0 {
        63 - mean.leading_zeros()
    } else {
        0
    }
    .min(RICE_KMAX as u32);
    // Check k−1, k, k+1 — the mean-derived parameter is within one of optimal.
    let mut best = u64::MAX;
    for kk in k.saturating_sub(1)..=(k + 1).min(RICE_KMAX as u32) {
        let bits = cnt * (1 + kk as u64) + (usum >> kk);
        best = best.min(bits);
    }
    best
}

/// The FIXED residual of one order via its direct formula — one vectorizable
/// pass, i32 arithmetic (bounded: |sample| < 2^25, coefficient sum ≤ 16 ⇒
/// |residual| < 2^30).
fn fixed_residual(samples: &[i32], order: usize) -> Vec<i32> {
    let n = samples.len();
    let mut res = Vec::with_capacity(n - order);
    match order {
        0 => res.extend_from_slice(samples),
        1 => {
            for i in 1..n {
                res.push(samples[i].wrapping_sub(samples[i - 1]));
            }
        }
        2 => {
            for i in 2..n {
                res.push(samples[i] - 2 * samples[i - 1] + samples[i - 2]);
            }
        }
        3 => {
            for i in 3..n {
                res.push(samples[i] - 3 * samples[i - 1] + 3 * samples[i - 2] - samples[i - 3]);
            }
        }
        4 => {
            for i in 4..n {
                res.push(
                    samples[i] - 4 * samples[i - 1] + 6 * samples[i - 2] - 4 * samples[i - 3]
                        + samples[i - 4],
                );
            }
        }
        _ => unreachable!("fixed order 0..=4"),
    }
    res
}

/// The chosen subframe encoding for a channel + its bit cost.
struct SubframeChoice {
    bits: u64,
    /// Trailing zero bits shifted out of every sample of this subframe
    /// (FLAC's wasted-bits field). The analysis ran on the SHIFTED samples at
    /// `bps - wasted`; `bits` includes the unary wasted-count header cost.
    wasted: u32,
    kind: SubframeKind,
}

/// Trailing zero bits common to every sample of the block (0 for all-zero
/// input — that's the CONSTANT path). This is what ffmpeg/libFLAC strip on
/// 16-bit-content-in-24-bit-container material, worth 8 bits/sample there.
fn detect_wasted(samples: &[i32], bps: u32) -> u32 {
    let mut acc = 0i32;
    for &v in samples {
        acc |= v;
        if acc & 1 != 0 {
            return 0; // early out: any odd sample kills the shift
        }
    }
    if acc == 0 {
        return 0;
    }
    (acc.trailing_zeros()).min(bps - 1)
}

enum SubframeKind {
    Constant(i32),
    Verbatim,
    Fixed {
        order: usize,
        res: Vec<i32>,
        plan: ResidualPlan,
    },
    Lpc(Box<LpcCandidate>),
}

/// The cheap phase of one arm's analysis: constant detection, LPC estimates
/// for every window, the fixed-order estimate — everything short of residual
/// realization. `est_bits` is the arm's estimated subframe cost, used for
/// stereo-mode gating before any expensive realization happens.
struct ArmEstimate {
    constant: Option<i32>,
    ests: Vec<Option<LpcEstimate>>,
    est_bits: u64,
}

/// One arm's analysis input: samples with any wasted bits already shifted
/// out, the effective bit depth, and the wasted count for the header.
struct ArmInput<'a> {
    samples: std::borrow::Cow<'a, [i32]>,
    /// Effective coded depth: nominal bps − wasted.
    ebps: u32,
    wasted: u32,
}

impl<'a> ArmInput<'a> {
    /// Detect trailing-zero (wasted) bits and shift them out.
    fn prepare(samples: &'a [i32], bps: u32) -> ArmInput<'a> {
        let wasted = detect_wasted(samples, bps);
        if wasted == 0 {
            ArmInput {
                samples: std::borrow::Cow::Borrowed(samples),
                ebps: bps,
                wasted: 0,
            }
        } else {
            ArmInput {
                samples: std::borrow::Cow::Owned(samples.iter().map(|&v| v >> wasted).collect()),
                ebps: bps - wasted,
                wasted,
            }
        }
    }

    fn into_samples(self) -> Vec<i32> {
        self.samples.into_owned()
    }
}

fn estimate_arm(
    arm: &ArmInput<'_>,
    max_lpc_order: usize,
    wins: &WindowCache,
    stats: &mut EncodeStats,
) -> ArmEstimate {
    let samples: &[i32] = &arm.samples;
    let bps = arm.ebps;
    let n = samples.len();
    if samples.iter().all(|&s| s == samples[0]) {
        return ArmEstimate {
            constant: Some(samples[0]),
            ests: Vec::new(),
            est_bits: 8 + arm.wasted as u64 + bps as u64,
        };
    }
    let max_order = max_lpc_order.min(n / 2);
    // Phase 1 estimates only the FIRST window — arm/mode ranking correlates
    // strongly across windows, so the second window's estimate is deferred to
    // realization (realize_arm), skipping two autocorrelations per pruned arm.
    let ests: Vec<Option<LpcEstimate>> = if max_order >= 1 {
        debug_assert_eq!(wins.n, n, "window cache not sized for this block");
        vec![lpc_estimate(samples, bps, max_order, &wins.w[0], stats)]
    } else {
        Vec::new()
    };
    let lpc_est = ests
        .iter()
        .flatten()
        .map(|e| e.est_bits)
        .fold(f64::INFINITY, f64::min);
    let verbatim = 8 + n as u64 * bps as u64;
    // The FIXED estimate is computed lazily in realize_arm — for arm RANKING
    // the LPC estimate suffices (FIXED wins only degenerate content, and pure
    // silence is already caught by the constant check above).
    let est_bits = if lpc_est.is_finite() {
        (lpc_est as u64).min(verbatim)
    } else {
        let (fx_order, fx_abs) = fixed_order_estimate(samples);
        let fx_est = 8
            + fx_order as u64 * bps as u64
            + 6
            + rice_bits_estimate(fx_abs, (n - fx_order) as u64);
        fx_est.min(verbatim)
    };
    ArmEstimate {
        constant: None,
        ests,
        est_bits: est_bits + arm.wasted as u64,
    }
}

/// The expensive phase: realize the estimated LPC winner (and close runner-up
/// windows), the estimate-gated FIXED plan, and pick the cheapest subframe.
/// The remaining windows' estimates (deferred by phase 1) are computed here.
fn realize_arm(
    arm: &ArmInput<'_>,
    est: &ArmEstimate,
    max_lpc_order: usize,
    wins: &WindowCache,
    stats: &mut EncodeStats,
) -> SubframeChoice {
    let samples: &[i32] = &arm.samples;
    let bps = arm.ebps;
    // The wasted-bits header cost (unary count) rides on every kind's bits so
    // stereo-mode comparisons stay honest.
    let wb = arm.wasted as u64;
    let n = samples.len();
    if let Some(v) = est.constant {
        return SubframeChoice {
            bits: 8 + wb + bps as u64,
            wasted: arm.wasted,
            kind: SubframeKind::Constant(v),
        };
    }

    // Complete the window-estimate set (phase 1 only did window 0).
    let max_order = max_lpc_order.min(n / 2);
    let mut all_ests: Vec<Option<LpcEstimate>> = est.ests.clone();
    if max_order >= 1 {
        for win in wins.w.iter().skip(all_ests.len()) {
            all_ests.push(lpc_estimate(samples, bps, max_order, win, stats));
        }
    }
    let lpc = realize_best_window(samples, bps, &all_ests, stats);
    let lpc_bits = lpc.as_ref().map_or(u64::MAX, |c| c.bits.saturating_add(wb));

    // FIXED: one-pass order estimate, then the exact residual + partition
    // plan only when the estimate says FIXED could still beat the realized
    // LPC candidate (a wide 10% margin — partitioning can undercut the
    // single-partition estimate). LPC wins ~99.6% of real subframes, so this
    // skips the second-most-expensive per-arm stage almost always.
    let (fx_order, fx_abs) = fixed_order_estimate(samples);
    let fx_est =
        8 + fx_order as u64 * bps as u64 + 6 + rice_bits_estimate(fx_abs, (n - fx_order) as u64);
    let fixed = if lpc.is_none() || fx_est <= lpc_bits.saturating_add(lpc_bits / 10) {
        let fx_res = fixed_residual(samples, fx_order);
        let fx_plan = plan_partitions(&fx_res, n, fx_order);
        let fixed_bits = 8 + wb + fx_order as u64 * bps as u64 + 6 + fx_plan.bits;
        Some((fx_res, fx_plan, fixed_bits))
    } else {
        None
    };
    let fixed_bits = fixed.as_ref().map_or(u64::MAX, |f| f.2);

    let verbatim_bits = 8 + wb + n as u64 * bps as u64;

    if lpc_bits <= fixed_bits && lpc_bits <= verbatim_bits {
        SubframeChoice {
            bits: lpc_bits,
            wasted: arm.wasted,
            kind: SubframeKind::Lpc(Box::new(lpc.unwrap())),
        }
    } else if let Some((fx_res, fx_plan, fixed_bits)) = fixed {
        if fixed_bits <= verbatim_bits {
            SubframeChoice {
                bits: fixed_bits,
                wasted: arm.wasted,
                kind: SubframeKind::Fixed {
                    order: fx_order,
                    res: fx_res,
                    plan: fx_plan,
                },
            }
        } else {
            SubframeChoice {
                bits: verbatim_bits,
                wasted: arm.wasted,
                kind: SubframeKind::Verbatim,
            }
        }
    } else {
        SubframeChoice {
            bits: verbatim_bits,
            wasted: arm.wasted,
            kind: SubframeKind::Verbatim,
        }
    }
}

/// Write the shared subframe-header prefix: padding bit, 6-bit type, and the
/// wasted-bits flag (+ unary count).
fn write_subframe_header(bw: &mut BitWriter, type_code: u64, wasted: u32, stats: &mut EncodeStats) {
    bw.write_bits(0, 1);
    bw.write_bits(type_code, 6);
    if wasted == 0 {
        bw.write_bits(0, 1);
    } else {
        stats.sub_wasted_bits += 1;
        bw.write_bits(1, 1);
        bw.write_zeros(wasted - 1); // unary: (wasted-1) zeros then a 1
        bw.write_bits(1, 1);
    }
}

fn write_subframe_from(
    bw: &mut BitWriter,
    samples: &[i32],
    bps: u32,
    choice: &SubframeChoice,
    stats: &mut EncodeStats,
) {
    match &choice.kind {
        SubframeKind::Constant(v) => {
            stats.sub_constant += 1;
            write_subframe_header(bw, 0b000000, choice.wasted, stats);
            bw.write_signed(*v as i64, bps);
        }
        SubframeKind::Verbatim => {
            stats.sub_verbatim += 1;
            write_subframe_header(bw, 0b000001, choice.wasted, stats);
            for &s in samples {
                bw.write_signed(s as i64, bps);
            }
        }
        SubframeKind::Fixed { order, res, plan } => {
            stats.sub_fixed += 1;
            stats.fixed_orders[*order] += 1;
            stats.partition_orders[plan.partition_order as usize] += 1;
            // FIXED, order in low 3 bits
            write_subframe_header(bw, 0b001000 | *order as u64, choice.wasted, stats);
            for &s in &samples[..*order] {
                bw.write_signed(s as i64, bps);
            }
            bw.write_bits(plan.method as u64, 2);
            bw.write_bits(plan.partition_order as u64, 4);
            write_partitioned_residual(bw, res, samples.len(), *order, plan);
        }
        SubframeKind::Lpc(c) => {
            stats.sub_lpc += 1;
            stats.partition_orders[c.plan.partition_order as usize] += 1;
            // LPC, (order-1) in low 5 bits
            write_subframe_header(bw, 0b100000 | (c.order as u64 - 1), choice.wasted, stats);
            for &s in &samples[..c.order] {
                bw.write_signed(s as i64, bps); // warm-up
            }
            bw.write_bits((LPC_PRECISION - 1) as u64, 4); // qlp precision - 1
            bw.write_bits(c.shift as u64 & 0x1F, 5); // shift (non-negative, 5-bit)
            for &q in &c.qlp {
                bw.write_signed(q as i64, LPC_PRECISION); // coefficients, qlp[0] first
            }
            bw.write_bits(c.plan.method as u64, 2);
            bw.write_bits(c.plan.partition_order as u64, 4);
            write_partitioned_residual(bw, &c.res, samples.len(), c.order, &c.plan);
        }
    }
}

/// Choose the cheapest subframe type (CONSTANT / LPC / FIXED / VERBATIM) —
/// the mono / multichannel path (stereo goes through [`decide_stereo`]'s
/// two-phase arm gating instead).
fn analyze_subframe(
    arm: &ArmInput<'_>,
    max_lpc_order: usize,
    wins: &WindowCache,
    stats: &mut EncodeStats,
) -> SubframeChoice {
    let est = estimate_arm(arm, max_lpc_order, wins, stats);
    realize_arm(arm, &est, max_lpc_order, wins, stats)
}

/// Stereo modes whose estimated cost is within this relative margin of the
/// estimated best are realized exactly and compared; the rest are pruned
/// before any residual work.
const STEREO_EST_MARGIN_PCT: u64 = 1;

/// Choose the cheapest of the four FLAC stereo modes for one block.
/// side = L − R (needs bps+1 bits); mid = (L + R) >> 1 (bps).
///
/// Two-phase: every arm (L, R, mid, side) gets a cheap ESTIMATE (window
/// autocorrelations + Levinson + fixed-order sums); only the arms belonging
/// to estimate-competitive modes are REALIZED (residuals, exact Rice plans).
/// The final mode decision uses exact realized costs.
fn decide_stereo(
    l: &[i32],
    r: &[i32],
    bps: u32,
    max_lpc_order: usize,
    wins: &WindowCache,
    stats: &mut EncodeStats,
) -> (u64, Vec<(Vec<i32>, u32, SubframeChoice)>) {
    let side: Vec<i32> = l.iter().zip(r).map(|(&a, &b)| a - b).collect();
    let mid: Vec<i32> = l.iter().zip(r).map(|(&a, &b)| (a + b) >> 1).collect();

    // Wasted-bits detection + shift per arm, then estimates for all four.
    let arms = [
        ArmInput::prepare(l, bps),
        ArmInput::prepare(r, bps),
        ArmInput::prepare(&mid, bps),
        ArmInput::prepare(&side, bps + 1),
    ];
    let ests = [
        estimate_arm(&arms[0], max_lpc_order, wins, stats),
        estimate_arm(&arms[1], max_lpc_order, wins, stats),
        estimate_arm(&arms[2], max_lpc_order, wins, stats),
        estimate_arm(&arms[3], max_lpc_order, wins, stats),
    ];
    // Mode order: independent / left-side / right-side / mid-side.
    let mode_arms: [[usize; 2]; 4] = [[0, 1], [0, 3], [3, 1], [2, 3]];
    let est_costs: Vec<u64> = mode_arms
        .iter()
        .map(|&[a, b]| ests[a].est_bits + ests[b].est_bits)
        .collect();
    let best_est = *est_costs.iter().min().expect("4 modes");
    let cutoff = best_est + best_est * STEREO_EST_MARGIN_PCT / 100;
    let candidate: Vec<bool> = est_costs.iter().map(|&c| c <= cutoff).collect();

    // Phase 2: realize exactly the arms candidate modes need.
    let mut choices: [Option<SubframeChoice>; 4] = [None, None, None, None];
    for (m, &is_cand) in candidate.iter().enumerate() {
        if !is_cand {
            continue;
        }
        for &arm in &mode_arms[m] {
            if choices[arm].is_none() {
                choices[arm] = Some(realize_arm(
                    &arms[arm],
                    &ests[arm],
                    max_lpc_order,
                    wins,
                    stats,
                ));
            }
        }
    }

    // Exact decision over the candidate modes (ties → lowest mode index,
    // matching the original exhaustive search's ordering).
    let mut mode = usize::MAX;
    let mut best_bits = u64::MAX;
    for (m, &is_cand) in candidate.iter().enumerate() {
        if !is_cand {
            continue;
        }
        let [a, b] = mode_arms[m];
        let bits = choices[a].as_ref().expect("realized").bits
            + choices[b].as_ref().expect("realized").bits;
        if bits < best_bits {
            best_bits = bits;
            mode = m;
        }
    }
    debug_assert!(mode < 4);

    // Hand back the two chosen arms: their (possibly wasted-shifted) samples,
    // effective bit depth, and choices.
    let assignment = [1u64, 8, 9, 10][mode];
    let [a, b] = mode_arms[mode];
    let mut arms = arms;
    let mut take_arm = |arm: usize, choices: &mut [Option<SubframeChoice>; 4]| {
        let input = std::mem::replace(
            &mut arms[arm],
            ArmInput {
                samples: std::borrow::Cow::Borrowed(&[]),
                ebps: 0,
                wasted: 0,
            },
        );
        let ebps = input.ebps;
        let choice = choices[arm].take().expect("chosen arm realized");
        (input.into_samples(), ebps, choice)
    };
    let first = take_arm(a, &mut choices);
    let second = take_arm(b, &mut choices);
    (assignment, vec![first, second])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_stereo(n: usize) -> (Vec<i32>, Vec<i32>) {
        let l: Vec<i32> = (0..n)
            .map(|i| ((i as f64 * 0.05).sin() * 20000.0) as i32)
            .collect();
        let r = vec![1234i32; n];
        (l, r)
    }

    fn decode_with_claxon(stream: &[u8]) -> (u32, u32, u32, Vec<Vec<i32>>) {
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(stream)).expect("parse");
        let info = reader.streaminfo();
        let ch = info.channels as usize;
        let mut chans = vec![Vec::new(); ch];
        let mut c = 0usize;
        for s in reader.samples() {
            chans[c].push(s.expect("sample"));
            c = (c + 1) % ch;
        }
        (info.sample_rate, info.channels, info.bits_per_sample, chans)
    }

    #[test]
    fn roundtrip_lossless_stereo_s16() {
        let (l, r) = sine_stereo(10_000);
        let mut enc = Encoder::new(44100, 2, 16).unwrap();
        enc.push_planar(&[&l, &r]).unwrap();
        let stream = enc.finish();
        assert_eq!(&stream[..4], b"fLaC");
        let (sr, ch, bps, chans) = decode_with_claxon(&stream);
        assert_eq!((sr, ch, bps), (44100, 2, 16));
        assert_eq!(chans[0], l);
        assert_eq!(chans[1], r);
        // It must actually compress (sine + constant).
        assert!(
            stream.len() < 10_000 * 4 / 2,
            "no compression: {}",
            stream.len()
        );
    }

    #[test]
    fn roundtrip_interleaved_matches_planar() {
        let (l, r) = sine_stereo(5_000);
        let inter: Vec<i32> = l.iter().zip(&r).flat_map(|(&a, &b)| [a, b]).collect();

        let mut e1 = Encoder::new(48000, 2, 16).unwrap();
        e1.push_planar(&[&l, &r]).unwrap();
        let mut e2 = Encoder::new(48000, 2, 16).unwrap();
        e2.push_interleaved(&inter).unwrap();
        assert_eq!(e1.finish(), e2.finish());
    }

    #[test]
    fn compression_level_lossless_and_monotonic() {
        // Noisy-ish deterministic signal so LPC order matters.
        let n = 20_000;
        let mut x = 0i64;
        let s: Vec<i32> = (0..n)
            .map(|i| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let noise = ((x >> 33) & 0xFF) as i32 - 128;
                ((i as f64 * 0.03).sin() * 12000.0) as i32 + noise
            })
            .collect();
        let encode_at = |level: u32| -> Vec<u8> {
            let mut e = Encoder::new(44100, 1, 16).unwrap();
            e.set_compression_level(level);
            e.push_planar(&[&s]).unwrap();
            e.finish()
        };
        let l0 = encode_at(0);
        let l8 = encode_at(8);
        for stream in [&l0, &l8] {
            let (_, _, _, chans) = decode_with_claxon(stream);
            assert_eq!(chans[0], s, "compression-level round-trip is not lossless");
        }
        assert!(l8.len() <= l0.len(), "level 8 larger than level 0");
    }

    #[test]
    fn stats_paths_wired() {
        let (l, r) = sine_stereo(10_000);
        let mut enc = Encoder::new(44100, 2, 16).unwrap();
        enc.push_planar(&[&l, &r]).unwrap();
        let (_, stats) = enc.finish_with_stats();
        assert!(stats.frames > 0);
        assert!(stats.sub_constant > 0, "constant channel not detected");
        assert!(
            stats.sub_lpc + stats.sub_fixed > 0,
            "no predictive subframes"
        );
    }

    /// The AVX2 autocorrelation must match the scalar twin bit-for-bit
    /// (identical striping and reduction order — no FMA, no reassociation).
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn autocorr_avx2_matches_scalar() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut x = 3u64;
        for n in [15usize, 64, 1000, 4096, 4097] {
            let w: Vec<f64> = (0..n)
                .map(|i| {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(99);
                    ((x >> 33) as i32 as f64) * 1e-3 + (i as f64 * 0.13).sin() * 500.0
                })
                .collect();
            let mut a = vec![0.0f64; 13];
            let mut b = vec![0.0f64; 13];
            autocorr_scalar(&w, &mut a);
            unsafe { autocorr_avx2(&w, &mut b) };
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                assert_eq!(x.to_bits(), y.to_bits(), "lag {i} differs at n={n}");
            }
        }
    }

    /// 16-bit content stored in a 24-bit container (8 zero LSBs per sample)
    /// must trigger the wasted-bits path: dramatically smaller than the naive
    /// coding, still exactly lossless.
    #[test]
    fn wasted_bits_on_16_in_24_content() {
        let n = 20_000;
        let mut x = 5u64;
        let s16: Vec<i32> = (0..n)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(11);
                ((i as f64 * 0.02).sin() * 9000.0) as i32 + ((x >> 40) & 0xFF) as i32 - 128
            })
            .collect();
        let s24: Vec<i32> = s16.iter().map(|&v| v << 8).collect();

        let encode = |data: &Vec<i32>, bps: u32| -> Vec<u8> {
            let mut e = Encoder::new(48000, 1, bps).unwrap();
            e.push_planar(&[data]).unwrap();
            e.finish()
        };
        let native16 = encode(&s16, 16);
        let in24 = encode(&s24, 24);

        // Lossless round-trip of the 24-bit stream.
        let (info, chans) = crate::decode::decode(&in24).unwrap();
        assert_eq!(info.bits_per_sample, 24);
        assert_eq!(chans[0], s24, "wasted-bits round-trip broke losslessness");

        // The 24-bit container must cost within ~2% of the true 16-bit coding
        // (8 zero LSBs are shifted out, not Rice-coded).
        let ratio = in24.len() as f64 / native16.len() as f64;
        assert!(
            ratio < 1.02,
            "wasted-bits not engaging: 24-bit container {} B vs 16-bit {} B ({ratio:.3}x)",
            in24.len(),
            native16.len()
        );

        // And the stats counter must show the path fired.
        let mut e = Encoder::new(48000, 1, 24).unwrap();
        e.push_planar(&[&s24]).unwrap();
        let (_, stats) = e.finish_with_stats();
        assert!(stats.sub_wasted_bits > 0, "wasted-bits counter never fired");
    }

    /// The AVX2 shifted-sum kernel is integer math — it must match the scalar
    /// twin EXACTLY on every length (including the empty/short tails).
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn rice_sums_avx2_matches_scalar() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut x = 11u64;
        for n in [0usize, 1, 3, 16, 255, 4096] {
            let res: Vec<i32> = (0..n)
                .map(|_| {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(7);
                    ((x >> 30) as i32) >> ((x >> 60) & 15) // wide dynamic range
                })
                .collect();
            assert_eq!(
                rice_sums_scalar(&res),
                unsafe { rice_sums_avx2(&res) },
                "n={n}"
            );
        }
    }

    /// The AVX2 fixed-order |residual| sums are integer math — exact match
    /// against the scalar twin on every length and alignment.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn fixed_sums_avx2_matches_scalar() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut x = 17u64;
        for n in [16usize, 17, 23, 64, 4095, 4096] {
            let s: Vec<i32> = (0..n)
                .map(|_| {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(3);
                    ((x >> 33) as i32) >> ((x >> 59) & 7) // ±2^30-ish range
                })
                .map(|v| v.clamp(-(1 << 24), (1 << 24) - 1)) // 25-bit domain
                .collect();
            assert_eq!(
                fixed_sums_scalar(&s),
                unsafe { fixed_sums_avx2(&s) },
                "n={n}"
            );
        }
    }

    /// The FMA-f64 LPC residual must equal the scalar i64 path exactly on
    /// realistic magnitudes (the dispatcher's range guard keeps it off the
    /// degenerate ones).
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn lpc_residual_avx2_matches_scalar() {
        if !(std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("fma"))
        {
            return;
        }
        let mut x = 23u64;
        let samples: Vec<i32> = (0..5000)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(13);
                ((i as f64 * 0.07).sin() * 3_000_000.0) as i32 + ((x >> 40) & 0xFFF) as i32
            })
            .collect();
        for order in [1usize, 2, 4, 8, 12] {
            let qlp: Vec<i32> = (0..order)
                .map(|j| ((x >> (j * 3)) & 0xFFF) as i32 - 2048)
                .collect();
            for shift in [11i32, 14] {
                // Same exactness precondition the dispatcher enforces.
                let sum_abs: i64 = qlp.iter().map(|&c| (c as i64).abs()).sum();
                assert!((sum_abs << 25) >> shift < (1i64 << 31), "test setup");
                let a = lpc_residual_scalar(&samples, &qlp, shift, order);
                let b = unsafe { lpc_residual_avx2(&samples, &qlp, shift, order) };
                assert_eq!(a, b, "order={order} shift={shift}");
            }
        }
    }

    #[test]
    fn rejects_bad_config() {
        assert!(Encoder::new(44100, 0, 16).is_err());
        assert!(Encoder::new(44100, 9, 16).is_err());
        assert!(Encoder::new(44100, 2, 12).is_err());
        assert!(Encoder::new(0, 2, 16).is_err());
        let mut e = Encoder::new(44100, 2, 16).unwrap();
        assert!(e.push_interleaved(&[1, 2, 3]).is_err());
    }
}
