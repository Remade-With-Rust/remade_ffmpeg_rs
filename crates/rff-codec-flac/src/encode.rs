//! In-house **FLAC encoder** — lossless, pure Rust, no external encoder crate.
//!
//! Built brick by brick (see `docs/codec-flac-encoder.md`). This is **brick 1**:
//! the bitstream scaffold — a `fLaC` marker + STREAMINFO + FLAC frames whose
//! subframes are CONSTANT (a flat channel) or VERBATIM (raw samples). No
//! prediction yet, so it does not *compress* — but every byte is spec-valid and
//! the audio round-trips bit-for-bit (the lossless invariant the later bricks
//! preserve while they add FIXED / LPC / Rice coding).
//!
//! The engine's FLAC muxer is a passthrough, so the encoder emits the **whole**
//! native stream and owns the framing end to end.

use rff_codec::Encoder;
use rff_core::{AudioFrame, Dictionary, Error, Frame, Packet, Result, SampleFormat};

/// Nominal samples-per-channel per FLAC frame. 4096 is FLAC's usual default and
/// encodes as an explicit 16-bit block size (frame-header block-size code 7).
const BLOCK_SIZE: usize = 4096;
/// Quantized LPC coefficient precision in bits. 14 is a solid default (a later
/// brick can adapt it per block-size / bit-depth like libFLAC).
const LPC_PRECISION: u32 = 14;
/// Highest LPC order searched — subset-compliant, and claxon's fast decode path
/// is specialized for orders ≤ 12.
const LPC_MAX_ORDER: usize = 12;

// ---------------------------------------------------------------------------
// Bit writer — MSB-first, the order FLAC packs bits.
// ---------------------------------------------------------------------------
struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    nbits: u8, // bits currently held in `cur` (0..8)
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            buf: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    /// Write the low `n` bits of `val`, most-significant bit first.
    fn write_bits(&mut self, val: u64, n: u32) {
        for i in (0..n).rev() {
            let bit = ((val >> i) & 1) as u8;
            self.cur = (self.cur << 1) | bit;
            self.nbits += 1;
            if self.nbits == 8 {
                self.buf.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }

    /// Write `val` as an `n`-bit two's-complement signed field.
    fn write_signed(&mut self, val: i64, n: u32) {
        let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        self.write_bits((val as u64) & mask, n);
    }

    /// Write `q` zero bits — safe for large `q` (a unary Rice quotient), where
    /// `write_bits(0, q)` would overflow-shift for `q >= 64`.
    fn write_zeros(&mut self, mut q: u32) {
        while q >= 32 {
            self.write_bits(0, 32);
            q -= 32;
        }
        if q > 0 {
            self.write_bits(0, q);
        }
    }

    /// Pad the current partial byte with zero bits so the stream is byte-aligned.
    fn align_to_byte(&mut self) {
        if self.nbits != 0 {
            self.cur <<= 8 - self.nbits;
            self.buf.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    /// The complete bytes written so far. Only meaningful when byte-aligned
    /// (`nbits == 0`) — used to CRC a header/frame that has just been aligned.
    fn bytes(&self) -> &[u8] {
        debug_assert_eq!(self.nbits, 0, "bytes() called mid-byte");
        &self.buf
    }

    fn into_bytes(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.buf
    }
}

/// FLAC's UTF-8-style coding of the frame number (fixed blocking strategy).
/// Same shape as UTF-8 codepoint encoding, extended up to 6 bytes (31 bits).
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

// ---------------------------------------------------------------------------
// FLAC CRCs (no reflection, init 0).
// ---------------------------------------------------------------------------
/// CRC-8, polynomial x^8 + x^2 + x^1 + x^0 (0x07) — over the frame header.
fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-16, polynomial x^16 + x^15 + x^2 + x^0 (0x8005) — over the whole frame.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// The encoder.
// ---------------------------------------------------------------------------
pub struct FlacEncoder {
    sample_rate: u32,
    channels: usize,
    bits_per_sample: u32,
    /// Max LPC order to search — lowered by `-compression_level` for speed.
    max_lpc_order: usize,
    /// De-interleaved integer samples, one Vec per channel.
    chans: Vec<Vec<i32>>,
    initialized: bool,
    /// Finished stream, produced on `flush()`.
    out: Option<Vec<u8>>,
    flushed: bool,
}

impl FlacEncoder {
    pub fn new() -> Self {
        FlacEncoder {
            sample_rate: 0,
            channels: 0,
            bits_per_sample: 16,
            max_lpc_order: LPC_MAX_ORDER,
            chans: Vec::new(),
            initialized: false,
            out: None,
            flushed: false,
        }
    }

    fn init_from(&mut self, f: &AudioFrame) {
        self.sample_rate = f.sample_rate;
        self.channels = f.channels.max(1) as usize;
        // S16 is a native 16-bit grid; float carries ~24 bits of mantissa, so map
        // it to a 24-bit grid (lossless for int-derived floats, higher fidelity
        // for the rest).
        self.bits_per_sample = match f.format {
            SampleFormat::S16 => 16,
            _ => 24,
        };
        self.chans = vec![Vec::new(); self.channels];
        self.initialized = true;
    }

    /// De-interleave one input frame into per-channel `i32` sample columns.
    fn ingest(&mut self, f: &AudioFrame) -> Result<()> {
        let ch = self.channels;
        let n = f.samples;
        match f.format {
            SampleFormat::S16 => {
                let d = &f.planes[0];
                for i in 0..n {
                    for c in 0..ch {
                        let o = (i * ch + c) * 2;
                        let v = i16::from_le_bytes([d[o], d[o + 1]]) as i32;
                        self.chans[c].push(v);
                    }
                }
            }
            SampleFormat::F32 => {
                let d = &f.planes[0];
                let scale = (1i64 << (self.bits_per_sample - 1)) as f32;
                for i in 0..n {
                    for c in 0..ch {
                        let o = (i * ch + c) * 4;
                        let s = f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
                        self.chans[c].push(quantize(s, scale));
                    }
                }
            }
            SampleFormat::F32Planar => {
                let scale = (1i64 << (self.bits_per_sample - 1)) as f32;
                for c in 0..ch {
                    let d = &f.planes[c];
                    for i in 0..n {
                        let o = i * 4;
                        let s = f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
                        self.chans[c].push(quantize(s, scale));
                    }
                }
            }
            _ => {
                return Err(Error::invalid(
                    "flac encode: unsupported sample format (need S16/F32/F32Planar)",
                ))
            }
        }
        Ok(())
    }

    /// MD5 of the unencoded audio: interleaved samples, little-endian, at the
    /// coded bit depth — FLAC's STREAMINFO integrity signature. Matches what the
    /// decoder hashes from the reconstructed samples, so `flac -t` verifies it.
    fn compute_md5(&self, bps: u32) -> [u8; 16] {
        let bytes_per = (bps / 8) as usize;
        let n = self.chans.first().map_or(0, |c| c.len());
        let mut md5 = crate::md5::Md5::new();
        let mut row = Vec::with_capacity(self.channels * bytes_per);
        for i in 0..n {
            row.clear();
            for c in 0..self.channels {
                row.extend_from_slice(&self.chans[c][i].to_le_bytes()[..bytes_per]);
            }
            md5.update(&row);
        }
        md5.finalize()
    }

    /// Encode all buffered samples into a complete native FLAC stream.
    fn encode_stream(&self) -> Vec<u8> {
        let n = self.chans.first().map_or(0, |c| c.len());
        let bps = self.bits_per_sample;

        let mut frames: Vec<u8> = Vec::new();
        let (mut min_bs, mut max_bs) = (u32::MAX, 0u32);
        let (mut min_fs, mut max_fs) = (u32::MAX, 0u32);
        let mut frame_number = 0u64;
        let mut start = 0usize;
        while start < n {
            let bs = (n - start).min(BLOCK_SIZE);
            let frame = self.encode_frame(frame_number, start, bs, bps);
            min_bs = min_bs.min(bs as u32);
            max_bs = max_bs.max(bs as u32);
            min_fs = min_fs.min(frame.len() as u32);
            max_fs = max_fs.max(frame.len() as u32);
            frames.extend_from_slice(&frame);
            start += bs;
            frame_number += 1;
        }
        if frames.is_empty() {
            min_bs = 0;
            min_fs = 0;
            max_fs = 0;
        }

        // STREAMINFO (34 bytes). MD5 (audio signature) is left zero until brick 6.
        let mut si = BitWriter::new();
        si.write_bits(min_bs as u64, 16);
        si.write_bits(max_bs as u64, 16);
        si.write_bits(min_fs as u64, 24);
        si.write_bits(max_fs as u64, 24);
        si.write_bits(self.sample_rate as u64, 20);
        si.write_bits((self.channels as u64) - 1, 3);
        si.write_bits((bps as u64) - 1, 5);
        si.write_bits(n as u64, 36);
        for &byte in &self.compute_md5(bps) {
            si.write_bits(byte as u64, 8); // MD5 audio signature — enables `flac -t`
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

    fn encode_frame(&self, frame_number: u64, start: usize, bs: usize, bps: u32) -> Vec<u8> {
        // Decide the channel layout: stereo picks the cheapest decorrelation mode;
        // mono / multichannel code each channel independently.
        let (assignment, subframes): (u64, Vec<(Vec<i32>, u32, SubframeChoice)>) =
            if self.channels == 2 {
                decide_stereo(
                    &self.chans[0][start..start + bs],
                    &self.chans[1][start..start + bs],
                    bps,
                    self.max_lpc_order,
                )
            } else {
                let subs = (0..self.channels)
                    .map(|c| {
                        let s = self.chans[c][start..start + bs].to_vec();
                        let choice = analyze_subframe(&s, bps, self.max_lpc_order);
                        (s, bps, choice)
                    })
                    .collect();
                ((self.channels as u64) - 1, subs)
            };

        let mut bw = BitWriter::new();
        // --- frame header ---
        bw.write_bits(0x3FFE, 14); // sync
        bw.write_bits(0, 1); // reserved (mandatory 0)
        bw.write_bits(0, 1); // blocking strategy: fixed block size
        bw.write_bits(7, 4); // block-size code 7 => explicit 16-bit (bs-1) below
        bw.write_bits(0, 4); // sample-rate code 0 => from STREAMINFO
        bw.write_bits(assignment, 4); // 0/1..7 = independent, 8/9/10 = L-S / R-S / M-S
                                      // Explicit sample-size code (nominal bit depth; the side channel's +1 bit
                                      // is implied by the assignment). Some strict decoders (claxon) reject code 0.
        bw.write_bits(sample_size_code(bps), 3);
        bw.write_bits(0, 1); // reserved (mandatory 0)
        write_utf8(&mut bw, frame_number);
        bw.write_bits((bs as u64) - 1, 16); // block size - 1
                                            // Header is byte-aligned here; CRC-8 covers it.
        let hcrc = crc8(bw.bytes());
        bw.write_bits(hcrc as u64, 8);

        // --- subframes (each at its own bit depth; side channels use bps+1) ---
        for (samples, sf_bps, choice) in &subframes {
            write_subframe_from(&mut bw, samples, *sf_bps, choice);
        }

        // --- frame footer: pad to byte, then CRC-16 of the whole frame ---
        bw.align_to_byte();
        let fcrc = crc16(bw.bytes());
        bw.write_bits(fcrc as u64, 16);
        bw.into_bytes()
    }
}

impl Default for FlacEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// FLAC frame-header sample-size code for a bit depth (0 = "from STREAMINFO",
/// which we avoid). Depths without a dedicated code fall back to 0.
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

/// Round a float sample in [-1, 1) onto the encoder's integer grid.
fn quantize(s: f32, scale: f32) -> i32 {
    (s * scale).round().clamp(-scale, scale - 1.0) as i32
}

/// FLAC fixed polynomial predictor residual of a given order (0–4): the
/// order-th finite difference. The residual is returned for `samples[order..]`
/// (the first `order` samples are stored verbatim as warm-up).
fn fixed_residual(samples: &[i32], order: usize) -> Vec<i32> {
    // Difference in i64 to stay overflow-safe, then narrow (fits i32 for ≤24-bit).
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

/// Exact Rice-coded bit count for a residual slice at parameter `k`.
fn rice_bits(res: &[i32], k: u32) -> u64 {
    let mut bits = 0u64;
    for &v in res {
        bits += (zigzag(v) >> k) as u64 + 1 + k as u64;
    }
    bits
}

/// Best Rice parameter (0..=14 — never 15, the escape code claxon can't decode)
/// and its exact bit cost for a residual slice.
fn best_rice(res: &[i32]) -> (u32, u64) {
    let mut best_k = 0u32;
    let mut best = rice_bits(res, 0);
    for k in 1..=14 {
        let b = rice_bits(res, k);
        if b < best {
            best = b;
            best_k = k;
        }
    }
    (best_k, best)
}

/// Rice-code one residual: quotient in unary (`q` zeros then a 1), then the low
/// `k` bits of the folded value.
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
/// `p`: `bs` must split evenly into 2^order partitions, and partition 0 (which
/// loses `p` warm-up samples) must stay non-empty. Capped at 8 (256 partitions).
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

/// Search partition orders 0..=max, each partition getting its own exact-best
/// Rice parameter. Order 0 (one param for the whole residual) is always a
/// candidate, so a partitioned plan never loses to single-partition coding.
fn plan_partitions(res: &[i32], bs: usize, p: usize) -> ResidualPlan {
    let max_po = max_partition_order(bs, p);
    let mut best = ResidualPlan {
        partition_order: 0,
        ks: Vec::new(),
        bits: u64::MAX,
    };
    for po in 0..=max_po {
        let n_part = 1usize << po;
        let psize = bs >> po;
        let mut ks = Vec::with_capacity(n_part);
        let mut bits = 0u64;
        let mut idx = 0usize;
        for part in 0..n_part {
            let cnt = if part == 0 { psize - p } else { psize };
            let (k, kb) = best_rice(&res[idx..idx + cnt]);
            ks.push(k);
            bits += 4 + kb;
            idx += cnt;
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

/// Write a partitioned Rice residual body: per partition, a 4-bit param then its
/// Rice codes. Partition 0 is short by the `p` warm-up samples.
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
        bw.write_bits(plan.ks[part] as u64, 4);
        for &r in &res[idx..idx + cnt] {
            write_rice(bw, r, plan.ks[part]);
        }
        idx += cnt;
    }
}

/// Tukey(0.5) apodization window (the libFLAC default): flat middle with cosine
/// tapers over the outer quarter at each end — trims autocorrelation edge bias.
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
        // The recursion solves the AR model (x[i] + Σ lpc·x[i-1-j] = e), so the
        // PREDICTOR coefficients are the negation — matching libFLAC's
        // `lp_coeff = -lpc`. Without this the predictor anti-correlates.
        per_order.push((lpc[..=i].iter().map(|&c| -c).collect(), err));
    }
    per_order
}

/// Quantize float LPC coefficients to `precision`-bit integers + a NON-negative
/// shift (claxon/libavcodec reject negative shift), with libFLAC-style rounding
/// error feedback. None if the coefficients are degenerate.
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

/// LPC residual using the quantized coefficients — computed with the exact i64
/// arithmetic the decoder inverts (`pred = (Σ qlp[j]·x[i-1-j]) >> shift`), so it
/// round-trips losslessly.
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

/// Build the best LPC subframe for a block, searching a couple of apodization
/// windows and keeping the smallest. None if too small / degenerate.
fn try_lpc(samples: &[i32], bps: u32, max_lpc_order: usize) -> Option<LpcCandidate> {
    let n = samples.len();
    let max_order = max_lpc_order.min(n / 2);
    if max_order < 1 {
        return None;
    }
    // Different apodizations suit different spectra; keep the best candidate.
    let mut best: Option<LpcCandidate> = None;
    for &alpha in &[0.5f64, 0.2] {
        let win = tukey_window(n, alpha);
        if let Some(c) = lpc_candidate(samples, bps, max_order, &win) {
            if best.as_ref().is_none_or(|b| c.bits < b.bits) {
                best = Some(c);
            }
        }
    }
    best
}

/// One LPC candidate for a given apodization window.
fn lpc_candidate(samples: &[i32], bps: u32, max_order: usize, win: &[f64]) -> Option<LpcCandidate> {
    let n = samples.len();
    let autoc = autocorrelation(samples, max_order, win);
    if autoc[0] <= 0.0 {
        return None;
    }
    let orders = levinson(&autoc, max_order);
    if orders.is_empty() {
        return None;
    }
    // Pick the order from the Levinson residual energy (libFLAC's heuristic:
    // header cost vs the entropy of a residual with that variance).
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
    let (qlp, shift) = quantize_lpc(&orders[best_idx].0, LPC_PRECISION)?;
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

fn write_subframe_fixed(
    bw: &mut BitWriter,
    samples: &[i32],
    order: usize,
    res: &[i32],
    plan: &ResidualPlan,
    bps: u32,
) {
    bw.write_bits(0, 1);
    bw.write_bits(0b001000 | order as u64, 6); // FIXED, order in low 3 bits
    bw.write_bits(0, 1);
    for &s in &samples[..order] {
        bw.write_signed(s as i64, bps);
    }
    bw.write_bits(0, 2); // residual method 0
    bw.write_bits(plan.partition_order as u64, 4);
    write_partitioned_residual(bw, res, samples.len(), order, plan);
}

fn write_subframe_lpc(bw: &mut BitWriter, samples: &[i32], c: &LpcCandidate, bps: u32) {
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

/// The chosen subframe encoding for a channel + its bit cost. Splitting the
/// decision from the writing lets stereo mode selection cost L / R / mid / side
/// before committing to a channel assignment.
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

/// Choose the cheapest subframe type (CONSTANT / LPC / FIXED / VERBATIM) for one
/// channel at `bps` bits per sample.
fn analyze_subframe(samples: &[i32], bps: u32, max_lpc_order: usize) -> SubframeChoice {
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

    let lpc = try_lpc(samples, bps, max_lpc_order);
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

/// Write a previously-analyzed subframe.
fn write_subframe_from(bw: &mut BitWriter, samples: &[i32], bps: u32, choice: &SubframeChoice) {
    match &choice.kind {
        SubframeKind::Constant(v) => {
            bw.write_bits(0, 1);
            bw.write_bits(0b000000, 6);
            bw.write_bits(0, 1);
            bw.write_signed(*v as i64, bps);
        }
        SubframeKind::Verbatim => {
            bw.write_bits(0, 1);
            bw.write_bits(0b000001, 6);
            bw.write_bits(0, 1);
            for &s in samples {
                bw.write_signed(s as i64, bps);
            }
        }
        SubframeKind::Fixed { order, res, plan } => {
            write_subframe_fixed(bw, samples, *order, res, plan, bps);
        }
        SubframeKind::Lpc(c) => {
            write_subframe_lpc(bw, samples, c, bps);
        }
    }
}

/// Brick 5: choose the cheapest of the four FLAC stereo modes for one block,
/// returning the channel-assignment code + the (samples, bps, choice) to write.
/// side = L − R (needs bps+1 bits); mid = (L + R) >> 1 (bps).
fn decide_stereo(
    l: &[i32],
    r: &[i32],
    bps: u32,
    max_lpc_order: usize,
) -> (u64, Vec<(Vec<i32>, u32, SubframeChoice)>) {
    let side: Vec<i32> = l.iter().zip(r).map(|(&a, &b)| a - b).collect();
    let mid: Vec<i32> = l.iter().zip(r).map(|(&a, &b)| (a + b) >> 1).collect();

    let cl = analyze_subframe(l, bps, max_lpc_order);
    let cr = analyze_subframe(r, bps, max_lpc_order);
    let cm = analyze_subframe(&mid, bps, max_lpc_order);
    let cs = analyze_subframe(&side, bps + 1, max_lpc_order);

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

impl Encoder for FlacEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        // `-compression_level 0..8`: trade encode speed for ratio via LPC order.
        if let Some(level) = options.get_int("compression_level") {
            self.max_lpc_order = if level <= 2 {
                4
            } else if level <= 5 {
                8
            } else {
                12
            };
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("flac encode: expected an audio frame"));
        };
        if !self.initialized {
            // FLAC's channel-assignment field caps independent channels at 8.
            if a.channels.max(1) > 8 {
                return Err(Error::invalid(
                    "flac encode: more than 8 channels is unsupported",
                ));
            }
            self.init_from(a);
        } else if a.channels.max(1) as usize != self.channels {
            return Err(Error::invalid(
                "flac encode: channel count changed mid-stream",
            ));
        }
        self.ingest(a)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(data) = self.out.take() {
            let mut p = Packet::from_data(0, data);
            p.pts = Some(0);
            return Ok(p);
        }
        if self.flushed {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        if self.initialized {
            self.out = Some(self.encode_stream());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claxon::FlacReader;
    use std::io::Cursor;

    /// Build an S16 stereo frame: a sine on L, a constant on R (exercises both
    /// VERBATIM and CONSTANT subframes), and the ground-truth integer columns.
    fn s16_stereo(n: usize) -> (Frame, Vec<Vec<i32>>) {
        let mut interleaved = Vec::with_capacity(n * 2 * 2);
        let mut expect = vec![Vec::with_capacity(n); 2];
        for i in 0..n {
            let l = ((i as f64 * 0.05).sin() * 20000.0) as i16;
            let r = 1234i16;
            interleaved.extend_from_slice(&l.to_le_bytes());
            interleaved.extend_from_slice(&r.to_le_bytes());
            expect[0].push(l as i32);
            expect[1].push(r as i32);
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 44100,
            channels: 2,
            format: SampleFormat::S16,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });
        (frame, expect)
    }

    fn encode(frame: &Frame) -> Vec<u8> {
        let mut enc = FlacEncoder::new();
        enc.send_frame(frame).unwrap();
        enc.flush();
        let mut stream = Vec::new();
        loop {
            match enc.receive_packet() {
                Ok(p) => stream.extend_from_slice(&p.data),
                Err(_) => break,
            }
        }
        stream
    }

    /// The brick-1 gate: encode → decode with claxon → integers are identical
    /// (lossless), and the stream is a spec-valid FLAC (claxon parses framing +
    /// verifies both CRCs).
    #[test]
    fn fixed_roundtrip_lossless_and_compresses() {
        // > 2 blocks so the multi-frame path + a short final block are exercised.
        let (frame, expect) = s16_stereo(10_000);
        let stream = encode(&frame);

        assert_eq!(&stream[..4], b"fLaC", "missing FLAC marker");
        // Uniform-amplitude sine + a constant channel: partitioning can't beat a
        // single param here, so this must not regress vs brick 2's 4342 B (order 0
        // is always in the partition search, so brick 3 is ≤ brick 2 by design).
        assert!(
            stream.len() <= 4342,
            "regressed vs brick 2: {} bytes",
            stream.len()
        );

        let mut reader = FlacReader::new(Cursor::new(&stream)).expect("claxon parses our FLAC");
        let info = reader.streaminfo();
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.samples, Some(10_000));

        let mut got = vec![Vec::new(); 2];
        for (idx, s) in reader.samples().enumerate() {
            got[idx % 2].push(s.expect("claxon sample"));
        }
        assert_eq!(got, expect, "FLAC round-trip is not lossless");
    }

    /// High-entropy 16-bit noise: FIXED can't help, so this exercises the
    /// VERBATIM fallback, proves we never emit the (claxon-unsupported) escape
    /// code, and confirms the output can't pathologically blow up.
    #[test]
    fn noise_roundtrips_lossless_no_blowup() {
        let n = 5000usize;
        let mut st = 0x1234_5678u32;
        let mut interleaved = Vec::new();
        let mut expect = Vec::new();
        for _ in 0..n {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let s = (st >> 16) as i16;
            interleaved.extend_from_slice(&s.to_le_bytes());
            expect.push(s as i32);
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 44100,
            channels: 1,
            format: SampleFormat::S16,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });
        let stream = encode(&frame);
        let mut reader = FlacReader::new(Cursor::new(&stream)).expect("valid flac");
        let got: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got, expect, "noise round-trip is not lossless");
        assert!(
            stream.len() < n * 2 + 1000,
            "pathological blow-up: {} bytes",
            stream.len()
        );
    }

    /// Loud first half + quiet second half of each block: a single Rice param is
    /// suboptimal, so partitioned Rice earns its keep — and must stay lossless.
    #[test]
    fn partitioned_rice_lossless_on_varying_dynamics() {
        let n = 8192usize; // two full 4096 blocks
        let mut interleaved = Vec::new();
        let mut expect = Vec::new();
        let mut st = 0x9E37_79B9u32;
        for i in 0..n {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let dither = (st >> 26) as i32 - 32; // small ±32 texture
            let amp = if i % 4096 < 2048 { 18000.0 } else { 150.0 };
            let s = ((((i as f64) * 0.11).sin() * amp) as i32 + dither).clamp(-32768, 32767) as i16;
            interleaved.extend_from_slice(&s.to_le_bytes());
            expect.push(s as i32);
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 44100,
            channels: 1,
            format: SampleFormat::S16,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });
        let stream = encode(&frame);
        let mut reader = FlacReader::new(Cursor::new(&stream)).expect("valid flac");
        let got: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got, expect, "varying-dynamics round-trip is not lossless");
    }

    /// Direct proof the win is real: on a varying-dynamics block the partitioned
    /// plan uses strictly fewer bits than one Rice param for the whole residual.
    #[test]
    fn partitioning_beats_single_partition() {
        let bs = 4096usize;
        let mut samples = Vec::with_capacity(bs);
        let mut st = 0x0000_BEEFu32;
        for i in 0..bs {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let amp = if i < 2048 { 18000.0 } else { 150.0 };
            samples.push((((i as f64) * 0.11).sin() * amp) as i32 + ((st >> 26) as i32 - 32));
        }
        let order = 2usize;
        let res = fixed_residual(&samples, order);
        let single = best_rice(&res).1 + 4; // one partition: 4-bit param + Rice
        let plan = plan_partitions(&res, bs, order);
        eprintln!(
            "partitioning: {} bits (order {}) vs {} single-partition — {:.1}% smaller",
            plan.bits,
            plan.partition_order,
            single,
            100.0 * (single - plan.bits) as f64 / single as f64
        );
        assert!(
            plan.bits < single,
            "partitioning should win: {} vs {}",
            plan.bits,
            single
        );
    }

    /// An AR(2) resonator — noise driving a sharp 2-pole filter — which an LPC
    /// predictor models near-exactly but FIXED's fixed polynomial cannot. LPC
    /// must use markedly fewer bits, and round-trip losslessly.
    #[test]
    fn lpc_beats_fixed_on_resonant_signal() {
        let n = 4096usize;
        let mut st = 0x1357_9BDFu32;
        let mut samples = Vec::with_capacity(n);
        let (mut x1, mut x2) = (0.0f64, 0.0f64);
        for _ in 0..n {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let e = ((st >> 24) as f64 - 128.0) * 20.0; // white excitation
                                                        // Mid-band resonance (poles near ±j·0.95): FIXED order-2's -2·x[i-1]
                                                        // term is badly wrong here, but LPC order 2 (a1≈0, a2≈-0.9) fits it.
            let x = (-0.9 * x2 + e).clamp(-30000.0, 30000.0);
            x2 = x1;
            x1 = x;
            samples.push(x.round() as i32);
        }

        let (fo, fres) = best_fixed(&samples, 16);
        let fixed_bits = 8 + fo as u64 * 16 + 6 + plan_partitions(&fres, n, fo).bits;
        let lpc = try_lpc(&samples, 16, LPC_MAX_ORDER).expect("lpc candidate");
        eprintln!(
            "FIXED order {fo}: {fixed_bits} bits  vs  LPC order {}: {} bits  ({:.1}% smaller)",
            lpc.order,
            lpc.bits,
            100.0 * (fixed_bits as f64 - lpc.bits as f64) / fixed_bits as f64
        );
        assert!(
            lpc.bits < fixed_bits,
            "LPC should beat FIXED: {} vs {}",
            lpc.bits,
            fixed_bits
        );

        // And the full encode must round-trip losslessly.
        let mut interleaved = Vec::new();
        for &s in &samples {
            interleaved.extend_from_slice(&(s as i16).to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 44100,
            channels: 1,
            format: SampleFormat::S16,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });
        let stream = encode(&frame);
        let mut reader = FlacReader::new(Cursor::new(&stream)).unwrap();
        let got: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got, samples, "LPC round-trip is not lossless");
    }

    /// L and a nearly-identical R: the side channel (L−R) is tiny, so a
    /// decorrelated mode must beat independent — and both channels round-trip.
    #[test]
    fn stereo_decorrelation_lossless_and_chosen() {
        let n = 8000usize;
        let mut interleaved = Vec::new();
        let (mut el, mut er) = (Vec::new(), Vec::new());
        let mut st = 0x2468_ACE0u32;
        let mut prev = 0i32;
        for i in 0..n {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let l = ((i as f64 * 0.03).sin() * 15000.0) as i32 + ((st >> 25) as i32 - 64);
            let r = l - (l - prev) / 8; // R closely tracks L → a tiny side channel
            prev = l;
            interleaved.extend_from_slice(&(l as i16).to_le_bytes());
            interleaved.extend_from_slice(&(r as i16).to_le_bytes());
            el.push(l);
            er.push(r);
        }

        // A decorrelated mode (8/9/10) must be chosen over independent (1).
        let (assignment, subs) = decide_stereo(&el, &er, 16, LPC_MAX_ORDER);
        let chosen: u64 = subs.iter().map(|(_, _, c)| c.bits).sum();
        let independent = analyze_subframe(&el, 16, LPC_MAX_ORDER).bits
            + analyze_subframe(&er, 16, LPC_MAX_ORDER).bits;
        eprintln!(
            "stereo mode {assignment}: {chosen} bits vs independent {independent}  ({:.1}% smaller)",
            100.0 * (independent - chosen) as f64 / independent as f64
        );
        assert_ne!(
            assignment, 1,
            "expected a decorrelated stereo mode, got independent"
        );
        assert!(
            chosen < independent,
            "decorrelation should beat independent"
        );

        let frame = Frame::Audio(AudioFrame {
            sample_rate: 44100,
            channels: 2,
            format: SampleFormat::S16,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });
        let stream = encode(&frame);
        let mut reader = FlacReader::new(Cursor::new(&stream)).unwrap();
        let mut got = vec![Vec::new(); 2];
        for (idx, s) in reader.samples().enumerate() {
            got[idx % 2].push(s.unwrap());
        }
        assert_eq!(got[0], el, "L channel is not lossless");
        assert_eq!(got[1], er, "R channel is not lossless");
    }

    /// `-compression_level` must stay lossless at every level and not get larger
    /// as the level rises (more LPC order searched ⇒ ≤ size).
    #[test]
    fn compression_level_lossless_and_monotonic() {
        let n = 8192usize;
        let mut interleaved = Vec::new();
        let mut expect = Vec::new();
        let mut st = 0x00C0_FFEEu32;
        let (mut x1, mut x2) = (0.0f64, 0.0f64);
        for _ in 0..n {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let e = ((st >> 24) as f64 - 128.0) * 30.0;
            let x = (1.6 * x1 - 0.7 * x2 + e).clamp(-30000.0, 30000.0);
            x2 = x1;
            x1 = x;
            let s = x.round() as i32;
            interleaved.extend_from_slice(&(s as i16).to_le_bytes());
            expect.push(s);
        }
        let encode_at = |level: i64| -> Vec<u8> {
            let mut d = Dictionary::new();
            d.set("compression_level", level.to_string());
            let mut enc = FlacEncoder::new();
            enc.configure(&d).unwrap();
            let frame = Frame::Audio(AudioFrame {
                sample_rate: 44100,
                channels: 1,
                format: SampleFormat::S16,
                planes: vec![interleaved.clone()],
                samples: n,
                pts: Some(0),
            });
            enc.send_frame(&frame).unwrap();
            enc.flush();
            let mut stream = Vec::new();
            while let Ok(p) = enc.receive_packet() {
                stream.extend_from_slice(&p.data);
            }
            stream
        };
        let low = encode_at(0); // max LPC order 4
        let high = encode_at(8); // max LPC order 12
        for stream in [&low, &high] {
            let mut reader = FlacReader::new(Cursor::new(stream)).unwrap();
            let got: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
            assert_eq!(got, expect, "compression-level round-trip is not lossless");
        }
        assert!(
            high.len() <= low.len(),
            "level 8 ({}) should be <= level 0 ({})",
            high.len(),
            low.len()
        );
    }

    /// 24-bit: F32 samples that are exactly int/2^23 map to a 24-bit grid and
    /// round-trip losslessly at bit depth 24.
    #[test]
    fn depth_24bit_roundtrip_lossless() {
        let n = 4000usize;
        let mut planar = Vec::new();
        let mut expect = Vec::new();
        let mut st = 0x0000_ABCDu32;
        for i in 0..n {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let v24 = (((i as f64 * 0.02).sin() * 4_000_000.0) as i32 + ((st >> 20) as i32 - 2048))
                .clamp(-8_388_608, 8_388_607);
            expect.push(v24);
            let f = v24 as f32 / 8_388_608.0; // exactly representable (24-bit mantissa)
            planar.extend_from_slice(&f.to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 96000,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![planar],
            samples: n,
            pts: Some(0),
        });
        let stream = encode(&frame);
        let mut reader = FlacReader::new(Cursor::new(&stream)).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 24);
        let got: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got, expect, "24-bit round-trip is not lossless");
    }

    /// Multichannel (3 independent channels) round-trips losslessly.
    #[test]
    fn multichannel_3ch_roundtrip_lossless() {
        let n = 2000usize;
        let ch = 3usize;
        let mut interleaved = Vec::new();
        let mut expect = vec![Vec::new(); ch];
        let mut st = 0x0000_9999u32;
        for i in 0..n {
            for (c, e) in expect.iter_mut().enumerate() {
                st ^= st << 13;
                st ^= st >> 17;
                st ^= st << 5;
                let s = ((i as f64 * (0.02 + c as f64 * 0.01)).sin() * 10000.0) as i16
                    + ((st >> 26) as i16 - 32);
                interleaved.extend_from_slice(&s.to_le_bytes());
                e.push(s as i32);
            }
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 48000,
            channels: 3,
            format: SampleFormat::S16,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });
        let stream = encode(&frame);
        let mut reader = FlacReader::new(Cursor::new(&stream)).unwrap();
        assert_eq!(reader.streaminfo().channels, 3);
        let mut got = vec![Vec::new(); ch];
        for (idx, s) in reader.samples().enumerate() {
            got[idx % ch].push(s.unwrap());
        }
        assert_eq!(got, expect, "3-channel round-trip is not lossless");
    }

    /// Emit our `.flac` + the original interleaved PCM so an external reference
    /// decoder (ffmpeg/flac) can confirm the stream is spec-valid + bit-exact.
    #[test]
    #[ignore = "writes files for external ffmpeg validation; run explicitly"]
    fn emit_for_external_check() {
        // Correlated stereo so the ffmpeg gate exercises decorrelation (side channel).
        let n = 10_000usize;
        let mut interleaved = Vec::new();
        let mut st = 0x1111_2222u32;
        let mut prev = 0i32;
        for i in 0..n {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let l = ((i as f64 * 0.04).sin() * 14000.0) as i32 + ((st >> 25) as i32 - 64);
            let r = l - (l - prev) / 6;
            prev = l;
            interleaved.extend_from_slice(&(l as i16).to_le_bytes());
            interleaved.extend_from_slice(&(r as i16).to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 44100,
            channels: 2,
            format: SampleFormat::S16,
            planes: vec![interleaved.clone()],
            samples: n,
            pts: Some(0),
        });
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("rff_flac_orig.raw"), &interleaved).unwrap();
        std::fs::write(dir.join("rff_flac_our.flac"), encode(&frame)).unwrap();

        // Also a 24-bit mono file so the ffmpeg gate covers the wide-sample path.
        let mut planar = Vec::new();
        for i in 0..n {
            let v = (((i as f64 * 0.03).sin() * 3_000_000.0) as i32).clamp(-8_388_608, 8_388_607);
            planar.extend_from_slice(&(v as f32 / 8_388_608.0).to_le_bytes());
        }
        let f24 = Frame::Audio(AudioFrame {
            sample_rate: 96000,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![planar],
            samples: n,
            pts: Some(0),
        });
        std::fs::write(dir.join("rff_flac_24.flac"), encode(&f24)).unwrap();
        eprintln!(
            "wrote {}/rff_flac_{{orig.raw,our.flac,24.flac}}",
            dir.display()
        );
    }

    /// A fully-flat mono block must take the CONSTANT path and still round-trip.
    #[test]
    fn constant_channel_roundtrips() {
        let mut interleaved = Vec::new();
        for _ in 0..1000 {
            interleaved.extend_from_slice(&555i16.to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: 48000,
            channels: 1,
            format: SampleFormat::S16,
            planes: vec![interleaved],
            samples: 1000,
            pts: Some(0),
        });
        let stream = encode(&frame);
        let mut reader = FlacReader::new(Cursor::new(&stream)).expect("valid flac");
        let got: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got.len(), 1000);
        assert!(got.iter().all(|&s| s == 555));
    }
}
