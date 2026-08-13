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
use crate::crc::{crc8, crc16};

/// Nominal samples-per-channel per FLAC frame. 4096 is FLAC's usual default and
/// encodes as an explicit 16-bit block size (frame-header block-size code 7).
const BLOCK_SIZE: usize = 4096;
/// Quantized LPC coefficient precision in bits.
const LPC_PRECISION: u32 = 14;
/// Highest LPC order searched — subset-compliant.
pub(crate) const LPC_MAX_ORDER: usize = 12;
/// Rice parameters searched (0..=14; 15 is the escape code).
const RICE_KMAX: usize = 14;

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
            EncodeError::RaggedInput => write!(f, "flac: interleaved length not a channel multiple"),
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
            buf.clear();
            match (bytes_per, ch) {
                // The hot shapes get direct loops; the rest go generic.
                (2, 1) => {
                    let a = &self.chans[0];
                    for j in i..end {
                        buf.extend_from_slice(&(a[j] as i16).to_le_bytes());
                    }
                }
                (2, 2) => {
                    let (l, r) = (&self.chans[0], &self.chans[1]);
                    for j in i..end {
                        buf.extend_from_slice(&(l[j] as i16).to_le_bytes());
                        buf.extend_from_slice(&(r[j] as i16).to_le_bytes());
                    }
                }
                (3, 2) => {
                    let (l, r) = (&self.chans[0], &self.chans[1]);
                    for j in i..end {
                        buf.extend_from_slice(&l[j].to_le_bytes()[..3]);
                        buf.extend_from_slice(&r[j].to_le_bytes()[..3]);
                    }
                }
                _ => {
                    for j in i..end {
                        for c in 0..ch {
                            buf.extend_from_slice(&self.chans[c][j].to_le_bytes()[..bytes_per]);
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
        let n = self.chans.first().map_or(0, |c| c.len());
        let bps = self.bps;

        // Whole-stream output estimate: raw size is the ceiling for lossless.
        let raw = n * self.channels * (bps as usize / 8);
        let mut frames: Vec<u8> = Vec::with_capacity(raw / 2 + 4096);
        let (mut min_bs, mut max_bs) = (u32::MAX, 0u32);
        let (mut min_fs, mut max_fs) = (u32::MAX, 0u32);
        let mut frame_number = 0u64;
        let mut start = 0usize;
        let mut wins = WindowCache::default();
        while start < n {
            let bs = (n - start).min(BLOCK_SIZE);
            wins.ensure(bs);
            let frame = self.encode_frame(frame_number, start, bs, bps, &wins);
            min_bs = min_bs.min(bs as u32);
            max_bs = max_bs.max(bs as u32);
            min_fs = min_fs.min(frame.len() as u32);
            max_fs = max_fs.max(frame.len() as u32);
            frames.extend_from_slice(&frame);
            start += bs;
            frame_number += 1;
            self.stats.frames += 1;
        }
        if frames.is_empty() {
            min_bs = 0;
            min_fs = 0;
            max_fs = 0;
        }

        // STREAMINFO (34 bytes).
        let mut si = BitWriter::with_capacity(34);
        si.write_bits(min_bs as u64, 16);
        si.write_bits(max_bs as u64, 16);
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
                let subs = (0..self.channels)
                    .map(|c| {
                        let s = self.chans[c][start..start + bs].to_vec();
                        let choice =
                            analyze_subframe(&s, bps, self.max_lpc_order, wins, &mut self.stats);
                        (s, bps, choice)
                    })
                    .collect();
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

/// FLAC fixed polynomial predictor residual of a given order (0–4).
fn fixed_residual(samples: &[i32], order: usize) -> Vec<i32> {
    let mut r: Vec<i64> = samples.iter().map(|&s| s as i64).collect();
    for _ in 0..order {
        for i in (1..r.len()).rev() {
            r[i] -= r[i - 1];
        }
    }
    r[order..].iter().map(|&v| v as i32).collect()
}

/// Zigzag-fold a signed residual to the unsigned value FLAC Rice-codes.
#[inline]
fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// `sums[k] = Σ (zigzag(v) >> k)` over a residual slice, for k = 0..=14.
/// The exact Rice bit cost at parameter k is `sums[k] + cnt·(1 + k)` — one
/// pass yields every parameter's exact cost.
#[inline]
fn rice_sums(res: &[i32]) -> [u64; RICE_KMAX + 1] {
    let mut sums = [0u64; RICE_KMAX + 1];
    for &v in res {
        let u = zigzag(v);
        // Fixed-count inner loop: unrolled / vectorized by the compiler.
        for (k, s) in sums.iter_mut().enumerate() {
            *s += (u >> k) as u64;
        }
    }
    sums
}

/// Best Rice parameter (lowest k on ties, matching the original scan order)
/// and its exact bit cost, from precomputed shifted sums.
#[inline]
fn best_k_from_sums(sums: &[u64; RICE_KMAX + 1], cnt: u64) -> (u32, u64) {
    let mut best_k = 0u32;
    let mut best = sums[0] + cnt;
    for (k, &s) in sums.iter().enumerate().skip(1) {
        let b = s + cnt * (1 + k as u64);
        if b < best {
            best = b;
            best_k = k as u32;
        }
    }
    (best_k, best)
}

/// Best Rice parameter + exact cost for a residual slice (single partition).
fn best_rice(res: &[i32]) -> (u32, u64) {
    best_k_from_sums(&rice_sums(res), res.len() as u64)
}

/// Rice-code one residual: quotient in unary, then the low `k` bits.
#[inline]
fn write_rice(bw: &mut BitWriter, v: i32, k: u32) {
    let u = zigzag(v);
    bw.write_zeros(u >> k);
    bw.write_bits(1, 1);
    if k > 0 {
        bw.write_bits((u & ((1u32 << k) - 1)) as u64, k);
    }
}

/// A residual coding plan: the chosen partition order + per-partition Rice
/// parameters, and the residual-body bit cost (Σ 4-bit param + Rice codes).
struct ResidualPlan {
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

    // Per-level partition sums, finest level first. Partition 0 is short by
    // the `p` warm-up samples at EVERY level, which pairwise merging preserves.
    let finest_parts = 1usize << max_po;
    let finest_size = bs >> max_po;
    let mut level_sums: Vec<Vec<[u64; RICE_KMAX + 1]>> = Vec::with_capacity(max_po as usize + 1);
    let mut finest: Vec<[u64; RICE_KMAX + 1]> = Vec::with_capacity(finest_parts);
    let mut idx = 0usize;
    for part in 0..finest_parts {
        let cnt = if part == 0 { finest_size - p } else { finest_size };
        finest.push(rice_sums(&res[idx..idx + cnt]));
        idx += cnt;
    }
    level_sums.push(finest);
    for _ in 0..max_po {
        let prev = level_sums.last().unwrap();
        let mut merged = Vec::with_capacity(prev.len() / 2);
        for pair in prev.chunks_exact(2) {
            let mut m = pair[0];
            for (a, b) in m.iter_mut().zip(&pair[1]) {
                *a += b;
            }
            merged.push(m);
        }
        level_sums.push(merged);
    }
    // level_sums[i] holds partitions at order (max_po - i).

    let mut best = ResidualPlan {
        partition_order: 0,
        ks: Vec::new(),
        bits: u64::MAX,
    };
    // Match the original's search order (po ascending, strict <).
    for po in 0..=max_po {
        let sums = &level_sums[(max_po - po) as usize];
        let psize = bs >> po;
        let n_part = 1usize << po;
        let mut ks = Vec::with_capacity(n_part);
        let mut bits = 0u64;
        for (part, s) in sums.iter().enumerate() {
            let cnt = if part == 0 { psize - p } else { psize } as u64;
            let (k, kb) = best_k_from_sums(s, cnt);
            ks.push(k);
            bits += 4 + kb;
        }
        if bits < best.bits {
            best = ResidualPlan {
                partition_order: po,
                ks,
                bits,
            };
        }
    }
    best
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
    let mut idx = 0usize;
    for part in 0..n_part {
        let cnt = if part == 0 { psize - p } else { psize };
        let k = plan.ks[part];
        bw.write_bits(k as u64, 4);
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
fn autocorrelation(samples: &[i32], max_order: usize, win: &[f64]) -> Vec<f64> {
    let n = samples.len();
    let w: Vec<f64> = samples
        .iter()
        .zip(win)
        .map(|(&s, &g)| s as f64 * g)
        .collect();
    let mut autoc = vec![0.0f64; max_order + 1];
    for (lag, a) in autoc.iter_mut().enumerate() {
        let mut sum = 0.0;
        for i in lag..n {
            sum += w[i] * w[i - lag];
        }
        *a = sum;
    }
    autoc
}

/// Levinson-Durbin: (coefficients, residual energy) for every order 1..=max.
/// Coefficients follow the FLAC convention: predicted = Σ c[j]·x[i-1-j].
fn levinson(autoc: &[f64], max_order: usize) -> Vec<(Vec<f64>, f64)> {
    let mut lpc = vec![0.0f64; max_order];
    let mut err = autoc[0];
    let mut per_order = Vec::with_capacity(max_order);
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
        // The recursion solves the AR model, so the PREDICTOR coefficients are
        // the negation (libFLAC's `lp_coeff = -lpc`).
        per_order.push((lpc[..=i].iter().map(|&c| -c).collect(), err));
    }
    per_order
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

/// LPC residual using the quantized coefficients — exact i64 arithmetic the
/// decoder inverts, so it round-trips losslessly.
fn lpc_residual(samples: &[i32], qlp: &[i32], shift: i32, order: usize) -> Vec<i32> {
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

/// A complete LPC subframe candidate + its total bit cost.
struct LpcCandidate {
    order: usize,
    qlp: Vec<i32>,
    shift: i32,
    res: Vec<i32>,
    plan: ResidualPlan,
    bits: u64,
}

/// Build the best LPC subframe for a block, searching the cached apodization
/// windows and keeping the smallest. None if too small / degenerate.
fn try_lpc(
    samples: &[i32],
    bps: u32,
    max_lpc_order: usize,
    wins: &WindowCache,
    stats: &mut EncodeStats,
) -> Option<LpcCandidate> {
    let n = samples.len();
    let max_order = max_lpc_order.min(n / 2);
    if max_order < 1 {
        return None;
    }
    debug_assert_eq!(wins.n, n, "window cache not sized for this block");
    let mut best: Option<LpcCandidate> = None;
    for (widx, win) in wins.w.iter().enumerate() {
        if let Some(c) = lpc_candidate(samples, bps, max_order, win, stats) {
            if best.as_ref().is_none_or(|b| c.bits < b.bits) {
                if widx == 1 {
                    stats.lpc_window_second_won += 1;
                }
                best = Some(c);
            }
        }
    }
    best
}

/// One LPC candidate for a given apodization window.
fn lpc_candidate(
    samples: &[i32],
    bps: u32,
    max_order: usize,
    win: &[f64],
    stats: &mut EncodeStats,
) -> Option<LpcCandidate> {
    let n = samples.len();
    let autoc = autocorrelation(samples, max_order, win);
    if autoc[0] <= 0.0 {
        return None;
    }
    let orders = levinson(&autoc, max_order);
    if orders.is_empty() {
        return None;
    }
    if orders.len() < max_order {
        stats.lpc_levinson_exhausted += 1;
    }
    // Pick the order from the Levinson residual energy (header cost vs the
    // entropy of a residual with that variance).
    let mut best_idx = 0usize;
    let mut best_est = f64::INFINITY;
    for (idx, (_, err)) in orders.iter().enumerate() {
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
    let order = best_idx + 1;
    let Some((qlp, shift)) = quantize_lpc(&orders[best_idx].0, LPC_PRECISION) else {
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

/// Best FIXED order (0–4) + its residual, by single-partition cost.
fn best_fixed(samples: &[i32], bps: u32) -> (usize, Vec<i32>) {
    let n = samples.len();
    let max_order = 4.min(n.saturating_sub(1));
    let mut best = (0usize, u64::MAX, Vec::new());
    for order in 0..=max_order {
        let res = fixed_residual(samples, order);
        let (_, rb) = best_rice(&res);
        let cost = order as u64 * bps as u64 + rb;
        if cost < best.1 {
            best = (order, cost, res);
        }
    }
    (best.0, best.2)
}

/// The chosen subframe encoding for a channel + its bit cost.
struct SubframeChoice {
    bits: u64,
    kind: SubframeKind,
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

/// Choose the cheapest subframe type (CONSTANT / LPC / FIXED / VERBATIM).
fn analyze_subframe(
    samples: &[i32],
    bps: u32,
    max_lpc_order: usize,
    wins: &WindowCache,
    stats: &mut EncodeStats,
) -> SubframeChoice {
    let n = samples.len();

    if samples.iter().all(|&s| s == samples[0]) {
        return SubframeChoice {
            bits: 8 + bps as u64,
            kind: SubframeKind::Constant(samples[0]),
        };
    }

    let (fx_order, fx_res) = best_fixed(samples, bps);
    let fx_plan = plan_partitions(&fx_res, n, fx_order);
    let fixed_bits = 8 + fx_order as u64 * bps as u64 + 6 + fx_plan.bits;

    let lpc = try_lpc(samples, bps, max_lpc_order, wins, stats);
    let lpc_bits = lpc.as_ref().map_or(u64::MAX, |c| c.bits);

    let verbatim_bits = 8 + n as u64 * bps as u64;

    if lpc_bits <= fixed_bits && lpc_bits <= verbatim_bits {
        SubframeChoice {
            bits: lpc_bits,
            kind: SubframeKind::Lpc(Box::new(lpc.unwrap())),
        }
    } else if fixed_bits <= verbatim_bits {
        SubframeChoice {
            bits: fixed_bits,
            kind: SubframeKind::Fixed {
                order: fx_order,
                res: fx_res,
                plan: fx_plan,
            },
        }
    } else {
        SubframeChoice {
            bits: verbatim_bits,
            kind: SubframeKind::Verbatim,
        }
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
            bw.write_bits(0, 1);
            bw.write_bits(0b000000, 6);
            bw.write_bits(0, 1);
            bw.write_signed(*v as i64, bps);
        }
        SubframeKind::Verbatim => {
            stats.sub_verbatim += 1;
            bw.write_bits(0, 1);
            bw.write_bits(0b000001, 6);
            bw.write_bits(0, 1);
            for &s in samples {
                bw.write_signed(s as i64, bps);
            }
        }
        SubframeKind::Fixed { order, res, plan } => {
            stats.sub_fixed += 1;
            stats.fixed_orders[*order] += 1;
            stats.partition_orders[plan.partition_order as usize] += 1;
            bw.write_bits(0, 1);
            bw.write_bits(0b001000 | *order as u64, 6); // FIXED, order in low 3 bits
            bw.write_bits(0, 1);
            for &s in &samples[..*order] {
                bw.write_signed(s as i64, bps);
            }
            bw.write_bits(0, 2); // residual method 0
            bw.write_bits(plan.partition_order as u64, 4);
            write_partitioned_residual(bw, res, samples.len(), *order, plan);
        }
        SubframeKind::Lpc(c) => {
            stats.sub_lpc += 1;
            stats.partition_orders[c.plan.partition_order as usize] += 1;
            bw.write_bits(0, 1);
            bw.write_bits(0b100000 | (c.order as u64 - 1), 6); // LPC, (order-1) in low 5 bits
            bw.write_bits(0, 1);
            for &s in &samples[..c.order] {
                bw.write_signed(s as i64, bps); // warm-up
            }
            bw.write_bits((LPC_PRECISION - 1) as u64, 4); // qlp precision - 1
            bw.write_bits(c.shift as u64 & 0x1F, 5); // shift (non-negative, 5-bit)
            for &q in &c.qlp {
                bw.write_signed(q as i64, LPC_PRECISION); // coefficients, qlp[0] first
            }
            bw.write_bits(0, 2); // residual method 0
            bw.write_bits(c.plan.partition_order as u64, 4);
            write_partitioned_residual(bw, &c.res, samples.len(), c.order, &c.plan);
        }
    }
}

/// Choose the cheapest of the four FLAC stereo modes for one block.
/// side = L − R (needs bps+1 bits); mid = (L + R) >> 1 (bps).
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

    let cl = analyze_subframe(l, bps, max_lpc_order, wins, stats);
    let cr = analyze_subframe(r, bps, max_lpc_order, wins, stats);
    let cm = analyze_subframe(&mid, bps, max_lpc_order, wins, stats);
    let cs = analyze_subframe(&side, bps + 1, max_lpc_order, wins, stats);

    // independent / left-side / right-side / mid-side.
    let costs = [
        cl.bits + cr.bits,
        cl.bits + cs.bits,
        cs.bits + cr.bits,
        cm.bits + cs.bits,
    ];
    let mode = (0..4).min_by_key(|&i| costs[i]).unwrap();

    match mode {
        0 => (1, vec![(l.to_vec(), bps, cl), (r.to_vec(), bps, cr)]),
        1 => (8, vec![(l.to_vec(), bps, cl), (side, bps + 1, cs)]),
        2 => (9, vec![(side, bps + 1, cs), (r.to_vec(), bps, cr)]),
        _ => (10, vec![(mid, bps, cm), (side, bps + 1, cs)]),
    }
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
        assert!(stream.len() < 10_000 * 4 / 2, "no compression: {}", stream.len());
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
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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
        assert!(stats.sub_lpc + stats.sub_fixed > 0, "no predictive subframes");
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
