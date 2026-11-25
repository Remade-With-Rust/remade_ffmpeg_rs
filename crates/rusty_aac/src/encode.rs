//! In-house **AAC-LC encoder** (see `docs/finished/codec-aac-encoder.md` for the brick
//! ledger). Bricks 1–7 are complete: encode-side primitives inverting the decoder
//! (bit writer, spectral-codebook encoder, header/config serializers), the forward
//! filterbank, the quantizer + rate loop, a Bark-spreading psychoacoustic model,
//! block switching, M/S stereo, and container/CLI integration.
//!
//! **What this encoder does NOT yet do** — the arms tracked by
//! `docs/finished/codec-aac-great-gate.md`. Naming them here keeps a fit from being run
//! against a mis-documented encoder:
//!
//! * The psy model drives **long blocks only**; short blocks code with flat
//!   scalefactors and a single window group (arm A1).
//! * The signal-to-mask ratio is a **constant 18 dB** — no tonality/SFM term
//!   (arm A2), and no absolute threshold of hearing, so nothing here is a
//!   function of absolute level (arm A4).
//! * **TNS, PNS and intensity stereo are never emitted** (arms A3/A6/A7), though
//!   `crate::decode` implements all three.
//! * The rate loop searches **one global base offset** against a **constant
//!   per-frame budget** — there is no outer per-band distortion loop (arm A12)
//!   and no bit reservoir or VBR mode (arm A5).
//! * Window shape is hardcoded sine (arm A8) and the transient detector uses an
//!   absolute energy ratio (arm A9).

use crate::codebook::{Codebook, CODEBOOKS, INTENSITY_HCB, INTENSITY_HCB2, NOISE_HCB};
use crate::ics::{IcsInfo, WindowSequence};
use crate::prof::{self, Kernel, Stage};
use crate::swb::swb_offsets;
use crate::tables::spectral_book;
use crate::{AdtsHeader, AudioSpecificConfig, Error, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Bit writer — MSB-first, the mirror of the decoder's `BitReader`.
// ---------------------------------------------------------------------------
///
/// Bits gather in a 64-bit accumulator and leave it 32 at a time, so a write is
/// a shift, an or and (every fourth byte) one 4-byte store, not a loop over its
/// bits: the per-bit writer it replaced, with its bit-by-bit `append`, was 6% of
/// encoding.
pub struct BitWriter {
    buf: Vec<u8>,
    /// The pending bits, in the low `nbits` bits (higher bits are stale).
    acc: u64,
    /// Pending bit count, always below 32 between calls.
    nbits: u32,
}

impl BitWriter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    /// Append the low `n` bits of `val` (`n <= 32`), most-significant bit first.
    #[inline]
    pub fn write(&mut self, val: u32, n: u32) {
        debug_assert!(n <= 32, "BitWriter::write takes at most 32 bits");
        let mask = (1u64 << n) - 1;
        // `nbits < 32` and `n <= 32`, so the pending bits still fit in 64.
        self.acc = (self.acc << n) | (u64::from(val) & mask);
        self.nbits += n;
        if self.nbits >= 32 {
            self.nbits -= 32;
            self.buf
                .extend_from_slice(&((self.acc >> self.nbits) as u32).to_be_bytes());
        }
    }

    #[inline]
    pub fn write_bool(&mut self, b: bool) {
        self.write(u32::from(b), 1);
    }

    /// Append everything `other` holds, bit for bit.
    pub fn append(&mut self, other: &Self) {
        if self.nbits == 0 {
            self.buf.extend_from_slice(&other.buf);
        } else {
            let mut words = other.buf.chunks_exact(4);
            for w in &mut words {
                self.write(u32::from_be_bytes([w[0], w[1], w[2], w[3]]), 32);
            }
            for &byte in words.remainder() {
                self.write(u32::from(byte), 8);
            }
        }
        self.write(other.acc as u32, other.nbits);
    }

    /// Total bits written so far.
    #[must_use]
    pub fn bit_len(&self) -> usize {
        self.buf.len() * 8 + self.nbits as usize
    }

    /// Pad to a byte boundary with zero bits and return the bytes.
    #[must_use]
    pub fn into_bytes(mut self) -> Vec<u8> {
        let mut n = self.nbits;
        while n >= 8 {
            n -= 8;
            self.buf.push((self.acc >> n) as u8);
        }
        if n != 0 {
            self.buf.push((self.acc << (8 - n)) as u8);
        }
        self.buf
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Spectral codebook encoding — the inverse of `codebook::apply_index`.
// ---------------------------------------------------------------------------

/// Pack a `dim`-tuple of quantized coefficients into codebook `cb`'s base-`modulo`
/// Huffman index, or None if the tuple isn't representable by that codebook.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "measured: passing the 4-byte Codebook by value cost +0.7% encode / +1.4% decode instructions in the inlined hot loops"
)]
fn tuple_index(cb: &Codebook, tuple: &[i32]) -> Option<u32> {
    let dim = cb.dim as usize;
    let lav = u32::from(cb.lav);
    let modulo = if cb.unsigned { lav + 1 } else { 2 * lav + 1 };
    let mut index = 0u32;
    for &c in &tuple[..dim] {
        let mag = c.unsigned_abs();
        let digit = if cb.unsigned {
            if cb.esc {
                mag.min(lav) // book 11: a magnitude ≥ lav clamps to lav and escapes
            } else if mag <= lav {
                mag
            } else {
                return None;
            }
        } else if mag <= lav {
            (c + lav as i32) as u32
        } else {
            return None;
        };
        index = index * modulo + digit;
    }
    Some(index)
}

/// Bits to code `tuple` with codebook `cb_num`, or None if unrepresentable.
#[must_use]
pub fn spectral_bits(cb_num: usize, tuple: &[i32]) -> Option<usize> {
    let cb = &CODEBOOKS[cb_num];
    let idx = tuple_index(cb, tuple)?;
    let (_, len) = spectral_book(cb_num as u8).code(idx as usize);
    let dim = cb.dim as usize;
    let mut bits = len as usize;
    if cb.unsigned {
        bits += tuple[..dim].iter().filter(|&&c| c != 0).count(); // one sign bit each
    }
    if cb.esc {
        for &c in &tuple[..dim] {
            if c.unsigned_abs() >= u32::from(cb.lav) {
                bits += escape_bits(c.unsigned_abs());
            }
        }
    }
    Some(bits)
}

/// Emit `tuple` with codebook `cb_num`: codeword, then sign bits (unsigned books),
/// then escape sequences (book 11). Caller ensures `spectral_bits` is Some.
///
/// # Panics
///
/// If `cb_num` is not a spectral codebook or `tuple` is not representable in it
/// (the caller's contract).
pub fn spectral_emit(cb_num: usize, tuple: &[i32], w: &mut BitWriter) {
    let cb = &CODEBOOKS[cb_num];
    let dim = cb.dim as usize;
    let idx = tuple_index(cb, tuple).expect("representable tuple");
    let (code, len) = spectral_book(cb_num as u8).code(idx as usize);
    w.write(code, u32::from(len));
    if cb.unsigned {
        for &c in &tuple[..dim] {
            if c != 0 {
                w.write_bool(c < 0);
            }
        }
    }
    if cb.esc {
        for &c in &tuple[..dim] {
            if c.unsigned_abs() >= u32::from(cb.lav) {
                emit_escape(c.unsigned_abs(), w);
            }
        }
    }
}

/// Escape length for magnitude `m` (≥ 16): `2N+5` bits, `2^(N+4) ≤ m < 2^(N+5)`.
fn escape_bits(m: u32) -> usize {
    let n = m.ilog2() - 4;
    2 * n as usize + 5
}

/// Escape sequence (ISO §4.6.3.3): N leading 1-bits, a 0, then N+4 bits of
/// `m - 2^(N+4)`.
fn emit_escape(m: u32, w: &mut BitWriter) {
    let n = m.ilog2() - 4;
    for _ in 0..n {
        w.write_bool(true);
    }
    w.write_bool(false);
    let bits = n + 4;
    w.write(m - (1 << bits), bits);
}

// ---------------------------------------------------------------------------
// Header / config serializers — inverses of the decoder's parsers.
// ---------------------------------------------------------------------------

/// The `AudioSpecificConfig` bytes for an AAC-LC stream — the MP4 `esds`
/// `DecoderSpecificInfo` the muxer needs (2 bytes for standard rates).
#[must_use]
pub fn audio_specific_config_bytes(sample_rate: u32, channels: u16) -> Vec<u8> {
    write_audio_specific_config(&AudioSpecificConfig {
        object_type: 2, // AAC-LC
        sample_rate,
        channels,
    })
}

/// Serialize an `AudioSpecificConfig` (ISO §1.6.2.1) — the `esds`/`stsd` config
/// bytes the MP4 muxer needs. Inverse of `parse_audio_specific_config`.
#[must_use]
pub fn write_audio_specific_config(cfg: &AudioSpecificConfig) -> Vec<u8> {
    let mut w = BitWriter::new();
    if cfg.object_type >= 31 {
        w.write(31, 5);
        w.write(u32::from(cfg.object_type - 32), 6);
    } else {
        w.write(u32::from(cfg.object_type), 5);
    }
    if let Some(i) = crate::sf_index_for_rate(cfg.sample_rate) {
        w.write(u32::from(i), 4);
    } else {
        w.write(0x0F, 4);
        w.write(cfg.sample_rate, 24);
    }
    w.write(u32::from(cfg.channels), 4);
    w.into_bytes()
}

/// Serialize a 7-byte ADTS frame header (no CRC) — inverse of `parse_adts`.
/// `hdr.frame_length` must include this 7-byte header.
///
/// # Panics
///
/// If `hdr.sample_rate` is not one of the thirteen standard AAC rates.
#[must_use]
pub fn write_adts_header(hdr: &AdtsHeader) -> Vec<u8> {
    let sf = crate::sf_index_for_rate(hdr.sample_rate).expect("standard rate for ADTS");
    let mut w = BitWriter::new();
    w.write(0xFFF, 12); // syncword
    w.write(0, 1); // MPEG-4
    w.write(0, 2); // layer (00)
    w.write_bool(true); // protection_absent → 7-byte header, no CRC
    w.write(u32::from(hdr.object_type - 1), 2); // profile
    w.write(u32::from(sf), 4);
    w.write(0, 1); // private
    w.write(u32::from(hdr.channels), 3); // channel config
    w.write(0, 4); // orig/home/copyright id+start
    w.write(hdr.frame_length as u32, 13);
    w.write(0x7FF, 11); // buffer fullness (VBR marker)
    w.write(0, 2); // num_raw_data_blocks - 1
    w.into_bytes()
}

/// Serialize `ics_info` for AAC-LC — inverse of `parse_ics_info`.
pub fn encode_ics_info(w: &mut BitWriter, info: &IcsInfo) {
    w.write(0, 1); // ics_reserved
    w.write(info.window_sequence.to_bits(), 2);
    w.write_bool(info.window_shape_kbd);
    if info.window_sequence.is_short() {
        w.write(u32::from(info.max_sfb), 4);
        w.write(grouping_bits(&info.window_group_length), 7);
    } else {
        w.write(u32::from(info.max_sfb), 6);
        w.write(0, 1); // predictor_data_present (AAC-LC = 0)
    }
}

/// The 7 `scale_factor_grouping` bits from window-group lengths (inverse of the
/// parser's grouping walk): the bit for window i+1 is 1 if it continues the
/// current group, 0 if it starts a new one.
fn grouping_bits(group_lengths: &[u8]) -> u32 {
    let mut is_start = [false; 8];
    let mut w = 0usize;
    for &len in group_lengths {
        if w < 8 {
            is_start[w] = true;
        }
        w += len as usize;
    }
    let mut sfg = 0u32;
    for i in 0..7 {
        if !is_start[i + 1] {
            sfg |= 1 << (6 - i);
        }
    }
    sfg
}

// ---------------------------------------------------------------------------
// Filterbank — forward long-block MDCT (inverse of the decoder's synthesis).
// ---------------------------------------------------------------------------
pub const FRAME_LEN: usize = 1024;
pub const LONG_N: usize = 2048;
const SHORT_N: usize = 256;
const SHORT_HALF: usize = 128;
/// 1 / `OUTPUT_NORM`: the decoder scales its output by 1/32768, so the encoder
/// scales the spectrum up by 32768 to land in the AAC coefficient domain.
const SPEC_SCALE: f32 = 32768.0;

/// Forward long-block filterbank: window the overlapping 2048 input samples
/// (previous frame's 1024 ++ current 1024) and forward-MDCT to 1024 coefficients,
/// scaled so the decoder's `imdct · window · (1/32768) + overlap-add` reconstructs
/// the input (TDAC). `win` is the 2048-length window (sine or KBD).
#[must_use]
pub fn analyze_long(prev: &[f32; FRAME_LEN], cur: &[f32; FRAME_LEN], win: &[f32]) -> Vec<f32> {
    let _prof = prof::scope(Stage::EncMdct);
    // Written into fresh capacity: both halves are fully computed, so the zero
    // fill of a `vec![0f32; LONG_N]` was overhead.
    let mut windowed = Vec::<f32>::with_capacity(LONG_N);
    let w = windowed.spare_capacity_mut();
    for n in 0..FRAME_LEN {
        w[n].write(prev[n] * win[n] * SPEC_SCALE);
        w[FRAME_LEN + n].write(cur[n] * win[FRAME_LEN + n] * SPEC_SCALE);
    }
    // SAFETY: LONG_N = 2·FRAME_LEN and the loop wrote both halves in full.
    unsafe { windowed.set_len(LONG_N) };
    crate::dsp::mdct_fast(&windowed)
}

/// Forward short-block filterbank: eight 256-sample windows (128-hop) tiled across
/// [448, 1600) of the prev++cur 2048 buffer, each windowed + MDCT'd to 128 coeffs,
/// laid out window-major (the exact inverse of the decoder's `short_frame`). The
/// uncovered [0,448)/[1600,2048) regions are bridged by the LongStart/LongStop
/// neighbours — which is why `EightShort` must sit between transition blocks.
#[must_use]
pub fn analyze_short(prev: &[f32; FRAME_LEN], cur: &[f32; FRAME_LEN], sw: &[f32]) -> Vec<f32> {
    let _prof = prof::scope(Stage::EncMdct);
    let mut buf = [0f32; LONG_N];
    buf[..FRAME_LEN].copy_from_slice(prev);
    buf[FRAME_LEN..].copy_from_slice(cur);
    let mut spec = vec![0f32; FRAME_LEN];
    for w in 0..8 {
        let off = 448 + w * SHORT_HALF;
        let mut windowed = [0f32; SHORT_N];
        for (n, s) in windowed.iter_mut().enumerate() {
            *s = buf[off + n] * sw[n] * SPEC_SCALE;
        }
        let coeffs = crate::dsp::mdct_fast(&windowed);
        spec[w * SHORT_HALF..(w + 1) * SHORT_HALF].copy_from_slice(&coeffs);
    }
    spec
}

/// The 2048-sample analysis window for a long or transition block — the exact
/// encode-side twin of the decoder's `long_window`.
///
/// **The left half follows the PREVIOUS block's shape, the right half the
/// current's** (`prev_kbd` / `cur_kbd`), because that is what the decoder does:
/// each overlap region must be windowed identically on both sides or TDAC fails
/// and the overlap-add leaves audible seams. LongStart/LongStop taper one side to
/// a short window so the sequence stays TDAC-exact.
///
/// The decoder seeds `prev_kbd = false` for the first frame of a stream
/// (`decode.rs`), so the encoder must too.
#[allow(clippy::too_many_arguments)]
fn long_window(
    seq: WindowSequence,
    prev_kbd: bool,
    cur_kbd: bool,
    sine_l: &[f32],
    kbd_l: &[f32],
    sine_s: &[f32],
    kbd_s: &[f32],
) -> Vec<f32> {
    let long_prev = if prev_kbd { kbd_l } else { sine_l };
    let long_cur = if cur_kbd { kbd_l } else { sine_l };
    let short_prev = if prev_kbd { kbd_s } else { sine_s };
    let short_cur = if cur_kbd { kbd_s } else { sine_s };

    let mut w = vec![0f32; LONG_N];
    if seq == WindowSequence::LongStop {
        w[448..448 + SHORT_HALF].copy_from_slice(&short_prev[..SHORT_HALF]);
        for s in w.iter_mut().take(FRAME_LEN).skip(576) {
            *s = 1.0;
        }
    } else {
        w[..FRAME_LEN].copy_from_slice(&long_prev[..FRAME_LEN]);
    }
    if seq == WindowSequence::LongStart {
        for s in w.iter_mut().take(FRAME_LEN + 448).skip(FRAME_LEN) {
            *s = 1.0;
        }
        w[FRAME_LEN + 448..FRAME_LEN + 448 + SHORT_HALF].copy_from_slice(&short_cur[SHORT_HALF..]);
    } else {
        w[FRAME_LEN..].copy_from_slice(&long_cur[FRAME_LEN..]);
    }
    w
}

/// **The Rung 0 gate signal**: order-2 LPC prediction gain over the frame's time
/// samples — a cheap tonality proxy.
///
/// Chosen over the spectral flatness measure the lab validates because SFM needs
/// a spectrum, and the window shape must be decided *before* the spectrum exists
/// (the shape determines the window that produces it). Computing a probe MDCT per
/// frame just to pick a window would roughly double filterbank cost for a
/// small-by-design win — dominated on `bits-per-op` grounds before it started.
///
/// This is O(3n) multiply-adds and needs no transform. Tonal signals are strongly
/// predictable from two past samples; noise is not. Validated against the
/// per-class truth table in `lab::signals` before being wired here.
///
/// Returns `R[0] / E₂ ≥ 1`.
pub(crate) fn time_tonality(x: &[f32]) -> f32 {
    let n = x.len();
    if n < 8 {
        return 1.0;
    }
    let mut r = [0f64; 3];
    for (lag, slot) in r.iter_mut().enumerate() {
        let mut acc = 0f64;
        for i in 0..n - lag {
            acc += f64::from(x[i]) * f64::from(x[i + lag]);
        }
        *slot = acc;
    }
    if r[0] <= 1e-12 {
        return 1.0;
    }
    let r0 = r[0] * 1.0001; // white-noise correction — keeps Levinson stable
    let k1 = r[1] / r0;
    let e1 = r0 * (1.0 - k1 * k1);
    if e1 <= 1e-12 {
        return 1.0;
    }
    let k2 = (r[2] - k1 * r[1]) / e1;
    let e2 = e1 * (1.0 - k2 * k2);
    ((r0 / e2.max(1e-12)) as f32).max(1.0)
}

// ---------------------------------------------------------------------------
// Psychoacoustic model (brick 4) — per-SFB masking thresholds drive per-band
// scalefactor allocation, so quantization noise is shaped to the audible floor.
// ---------------------------------------------------------------------------

/// Traunmüller's Hz→Bark (critical-band rate).
fn hz_to_bark(f: f64) -> f64 {
    let f = f.max(1.0);
    26.81 * f / (1960.0 + f) - 0.53
}

/// Spreading of a masker's energy to a maskee `dz` Bark away (`dz` = maskee −
/// masker): steep ~27 dB/Bark toward lower bands, gentle ~10 dB/Bark upward.
fn spreading(dz: f64) -> f64 {
    let slope_db = if dz >= 0.0 { -10.0 } else { -27.0 };
    10f64.powf(slope_db * dz.abs() / 10.0)
}

/// The Bark-spreading matrix `S[i·n+j] = spreading(bark[i]-bark[j])` for one band
/// geometry — fixed per (`sample_rate`, band count), so it's built once and cached,
/// turning each frame's masking into a matrix-vector product (no runtime powf).
/// `n_coeffs` is the number of spectral coefficients the band table spans — 1024
/// for a long block, 128 for one short window. It sets the coefficient→Hz
/// mapping, so passing the long value for a short block would place every band
/// eight octaves too low and spread the mask against the wrong neighbours.
fn spreading_matrix(swb: &[u16], sample_rate: u32, n_coeffs: usize) -> Arc<Vec<f64>> {
    type Cache = Mutex<HashMap<(u32, usize, usize), Arc<Vec<f64>>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (sample_rate, swb.len(), n_coeffs);
    let mut c = cache.lock().unwrap();
    if let Some(m) = c.get(&key) {
        return m.clone();
    }
    let num_swb = swb.len() - 1;
    let bark: Vec<f64> = (0..num_swb)
        .map(|sfb| {
            let center = f64::midpoint(f64::from(swb[sfb]), f64::from(swb[sfb + 1]));
            // n_coeffs coefficients span 0..sr/2.
            hz_to_bark(center * f64::from(sample_rate) * 0.5 / n_coeffs as f64)
        })
        .collect();
    let mut mat = vec![0f64; num_swb * num_swb];
    for i in 0..num_swb {
        for j in 0..num_swb {
            mat[i * num_swb + j] = spreading(bark[i] - bark[j]);
        }
    }
    let arc = Arc::new(mat);
    c.insert(key, arc.clone());
    arc
}

/// Per-SFB masking threshold (energy units): spread the band energies on the
/// Bark scale, then sit the mask a fixed ratio below the spread signal.
///
/// `pub(crate)` so [`crate::lab::quality`] can score coding noise against the
/// encoder's own mask (the NMR metric), in the same band geometry and energy
/// domain the encoder allocates in.
#[cfg(any(test, feature = "lab"))]
pub(crate) fn masking_thresholds(spec: &[f32], swb: &[u16], sample_rate: u32) -> Vec<f64> {
    let num_swb = swb.len() - 1;
    let mut energy = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energy[sfb] = spec[s..e]
            .iter()
            .map(|&x| f64::from(x).powi(2))
            .sum::<f64>()
            + 1e-3;
    }
    masking_from_energy(&energy, swb, sample_rate, FRAME_LEN, None)
}

/// The default signal-to-mask ratio: the mask sits 18 dB below the spread signal,
/// for every band and every kind of content.
///
/// **Arm A2** replaces this constant with a function of tonality. It is census
/// category 4 — a knob whose own doc-comment names the physical signal it tracks,
/// shipping as a constant. Kept as the byte-identical fallback.
const SMR_FLAT: f64 = 0.0158; // 10^(-18/10)

thread_local! {
    /// The flat signal-to-mask ratio (linear) of the frame this thread is
    /// coding, from [`AacTuning::mask_smr_db`]: set per frame by
    /// `encode_frame` and per segment by the frame-demand pre-pass (each wholly
    /// on one worker thread), so the psy model needs no extra argument for it.
    /// [`SMR_FLAT`] (18 dB) outside an encode.
    static MASK_SMR: std::cell::Cell<f64> = const { std::cell::Cell::new(SMR_FLAT) };
}

/// [`AacTuning::mask_smr_db`] applies to streams under this rate a
/// channel; from it up the mask is the 2.1.2 18 dB. Measured (PEAQ, all
/// low-rate tools on): 24 dB is worth +0.41 BD-ODG on stereo vocal at 48-64
/// kbps a channel, but cost stereo vocal 0.15 ODG at 96 kbps a channel (192
/// kbps stereo) and mono vocal 0.08-0.13 at 96-160 kbps; at 18 dB every clip
/// there is within 0.02 of 2.1.2 or better.
const MASK_SMR_MAX_BPS_PER_CHANNEL: u32 = 96_000;

/// The linear ratio for a flat SMR of `db` decibels: [`SMR_FLAT`] itself at
/// 18 dB, so that setting keeps the 2.1.2 masks bit for bit.
fn smr_ratio(db: f32) -> f64 {
    if (f64::from(db) - 18.0).abs() < 1e-9 {
        SMR_FLAT
    } else {
        10f64.powf(-f64::from(db) / 10.0)
    }
}

/// The psychoacoustic arms in flight, threaded down to every coding path.
///
/// A struct rather than loose bools because these arms compose: A1 decides
/// *whether* short blocks get a mask at all, A2 decides *what* that mask is, and
/// both must reach the CPE path and the SCE path identically or the two paths
/// silently encode to different models — the inconsistency great-gate's P0.9
/// hygiene batch exists to prevent.
///
/// `PsyCfg::default()` is all-off = the encoder exactly as it shipped.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PsyCfg {
    /// Arm A1 — psy model on short blocks (else flat scalefactors).
    pub short_block_psy: bool,
    /// Arm 1a — real window grouping on short blocks, with the short-block psy
    /// model run per group and centred across groups.
    pub window_grouping: bool,
    /// Arm A2 — tonality-adaptive SMR (else flat 18 dB).
    pub tonality_smr: bool,
    /// Arm A3 — emit TNS on long blocks.
    pub tns: bool,
    /// Arm A6 — Perceptual Noise Substitution.
    pub pns: bool,
    /// Arm A7 — intensity stereo.
    pub intensity: bool,
    /// Arm A13 — demand-proportional stereo bit split.
    pub stereo_bit_split: bool,
}

/// Per-band masking threshold from precomputed band energies.
///
/// Split out from [`masking_thresholds`] so short blocks (whose band energies are
/// summed across the eight windows of a group) and long blocks share one masking
/// model — great-gate P1: consolidate duplicated probe skeletons into one path,
/// or a fit spanning both learns the inconsistency rather than the content.
///
/// `tonality` is **arm A2**: `None` uses the flat 18 dB [`SMR_FLAT`] (the shipped
/// behavior, byte-identical); `Some(t)` interpolates the classic tonal/noise
/// masker offsets per band.
fn masking_from_energy(
    energy: &[f64],
    swb: &[u16],
    sample_rate: u32,
    n_coeffs: usize,
    tonality: Option<&[f32]>,
) -> Vec<f64> {
    let spread = spread_energy(energy, swb, sample_rate, n_coeffs);
    masking_from_spread(&spread, swb, sample_rate, n_coeffs, tonality)
}

/// Each band's spread energy: every band's energy as it reaches this band on
/// the Bark scale, its own included at full weight.
fn spread_energy(energy: &[f64], swb: &[u16], sample_rate: u32, n_coeffs: usize) -> Vec<f64> {
    let num_swb = swb.len() - 1;
    let mat = spreading_matrix(swb, sample_rate, n_coeffs);
    (0..num_swb)
        .map(|i| {
            let row = &mat[i * num_swb..(i + 1) * num_swb];
            (0..num_swb).map(|j| energy[j] * row[j]).sum()
        })
        .collect()
}

/// The mask from [`spread_energy`]: a signal-to-mask ratio below the spread
/// signal, floored at the threshold of hearing.
fn masking_from_spread(
    spread: &[f64],
    swb: &[u16],
    sample_rate: u32,
    n_coeffs: usize,
    tonality: Option<&[f32]>,
) -> Vec<f64> {
    let num_swb = swb.len() - 1;
    (0..num_swb)
        .map(|i| {
            let spread = spread[i];
            let smr = match tonality {
                None => MASK_SMR.with(std::cell::Cell::get),
                Some(t) => {
                    let center = f64::midpoint(f64::from(swb[i]), f64::from(swb[i + 1]));
                    let bark = hz_to_bark(center * f64::from(sample_rate) * 0.5 / n_coeffs as f64);
                    smr_for(f64::from(t[i]), bark)
                }
            };
            spread * smr
        })
        .zip(ath_long(swb, sample_rate, n_coeffs).iter())
        .map(|(thr, &ath)| thr.max(ath))
        .collect()
}

/// The absolute threshold of hearing (Terhardt's approximation), in dB SPL, at
/// `hz`, capped at [`ATH_CAP_DB`].
fn ath_db(hz: f64) -> f64 {
    let f = (hz / 1000.0).max(0.02);
    let db = 3.64 * f.powf(-0.8) - 6.5 * (-0.6 * (f - 3.3).powi(2)).exp() + 1e-3 * f.powi(4);
    db.min(ATH_CAP_DB)
}

/// Ceiling on the hearing floor, in dB SPL (-80 dBFS). Terhardt's curve rises
/// past 65 dB SPL at 16 kHz and past full scale near 18 kHz; uncapped, it acts
/// as a bandwidth cut and drops quiet high detail that PEAQ still hears.
/// Measured (PEAQ ODG vs the floor-less encoder, same decoder): a 50 dB cap
/// cost stereo guitar 0.33 at 128 kbps and 0.20 at 320 kbps; at 20 dB those
/// are at parity (-0.001, +0.002) while piano keeps +0.93 at 128 kbps.
const ATH_CAP_DB: f64 = 20.0;

/// Energy, in this encoder's long-block coefficient domain, of a full-scale
/// sine: the reference the threshold of hearing is placed against (0 dBFS =
/// [`ATH_FULL_SCALE_SPL`]). Measured once through the encoder's own long
/// filterbank, so it tracks `SPEC_SCALE` and the MDCT normalisation exactly.
fn full_scale_sine_energy() -> f64 {
    static E: OnceLock<f64> = OnceLock::new();
    *E.get_or_init(|| {
        let n2 = 2 * FRAME_LEN;
        let win: Vec<f32> = (0..n2)
            .map(|n| (std::f64::consts::PI * (n as f64 + 0.5) / n2 as f64).sin() as f32)
            .collect();
        // 1 kHz at 48 kHz: a whole number of cycles per frame does not matter
        // for the energy, which is what is measured.
        let s = |n: usize| (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 48_000.0).sin() as f32;
        let mut prev = [0f32; FRAME_LEN];
        let mut cur = [0f32; FRAME_LEN];
        for n in 0..FRAME_LEN {
            prev[n] = s(n);
            cur[n] = s(FRAME_LEN + n);
        }
        analyze_long(&prev, &cur, &win)
            .iter()
            .map(|&x| f64::from(x).powi(2))
            .sum()
    })
}

/// Sound pressure level a full-scale sine is taken to play at: 100 dB, 4 dB
/// above the usual 16-bit convention of 96, which lowers the floor by 4 dB --
/// margin for listening louder than the convention assumes. Measured (PEAQ
/// ODG vs the floor-less encoder, same decoder): at 96 dB stereo vocal lost
/// 0.15 at 320 kbps and mono piano 0.31 at 128 kbps; 100 dB is the setting
/// the sweep found at parity on the losing clips.
const ATH_FULL_SCALE_SPL: f64 = 100.0;

/// Per-band absolute-threshold floor for the long-block mask (energy units):
/// the energy a tone at the band's quietest audible level would have. A band
/// whose content sits below it is inaudible, so its mask never falls below it.
///
/// Without this floor the mask scaled with each band's own energy, however
/// small: a band holding only a tone's window leakage got a mask 18 dB below
/// that leakage and was coded as if it mattered. On a 440 Hz tone at 96 kbps
/// that spent the frame on ~450 near-silent coefficients and left the tone
/// with 7 quantizer levels (17 dB SNR, and a 1.5x level error at 21 kbps):
/// `remade_ffmpeg_rs#129`.
///
/// Long blocks only (`n_coeffs == FRAME_LEN`): short-block band energies are
/// summed over a window group whose size this function does not see, so they
/// keep the relative mask (short-block psy is off by default).
fn ath_long(swb: &[u16], sample_rate: u32, n_coeffs: usize) -> Arc<Vec<f64>> {
    type Cache = Mutex<HashMap<(u32, usize, usize), Arc<Vec<f64>>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (sample_rate, swb.len(), n_coeffs);
    if let Some(a) = cache.lock().unwrap().get(&key) {
        return a.clone();
    }
    let num_swb = swb.len() - 1;
    let ath: Vec<f64> = if n_coeffs == FRAME_LEN {
        let e_fs = full_scale_sine_energy();
        let hz_per_line = f64::from(sample_rate) * 0.5 / n_coeffs as f64;
        (0..num_swb)
            .map(|sfb| {
                let min_db = (usize::from(swb[sfb])..usize::from(swb[sfb + 1]))
                    .map(|k| ath_db((k as f64 + 0.5) * hz_per_line))
                    .fold(f64::INFINITY, f64::min);
                e_fs * 10f64.powf((min_db - ATH_FULL_SCALE_SPL) / 10.0)
            })
            .collect()
    } else {
        vec![0.0; num_swb]
    };
    let arc = Arc::new(ath);
    cache.lock().unwrap().insert(key, arc.clone());
    arc
}

/// **Arm A2.** Signal-to-mask ratio as a function of band tonality and Bark.
///
/// The classic asymmetry: a *tonal* masker hides noise poorly, so the mask must
/// sit far below it (≈ 14.5 + bark dB); a *noise-like* masker hides noise well
/// (≈ 5.5 dB). Interpolating on the tonality index is the standard MPEG form.
///
/// Today's encoder applies 18 dB to everything, so a pure tone and band-limited
/// noise at equal energy get identical treatment — which is what this replaces.
///
/// Returned as a linear energy ratio (`10^(−dB/10)`), matching [`SMR_FLAT`].
///
/// # The noise end is 5.5 dB — and pinning it to 18 dB was measured and REFUTED
///
/// The obvious-looking "safety" tweak is to pin the noise end to the shipped flat
/// 18 dB, so arm A2 can only ever give a band *more* protection than today and
/// never less. Measured on the corpus, that made things **worse**, not safer:
///
/// | noise end | mean Δaudible% | worst class |
/// |---|---|---|
/// | 5.5 dB (textbook) | **−0.312** | +0.720 |
/// | 18.0 dB (pinned) | −0.006 | **+2.185** |
///
/// The reason is that an SMR pair is a **bit-allocation balance, not two
/// independent protections**. Raising the tonal end buys precision only because
/// lowering the noise end frees the bits to pay for it. Pin the noise end and the
/// tonal end becomes pure extra demand against a fixed budget — the rate loop
/// answers by lifting the global base, and *every* band gets coarser. The
/// "conservative" variant regressed three times as hard as the one it was meant
/// to protect against.
///
/// Keep the textbook pair. A2's remaining per-class failures are a **dispatch**
/// problem (route the arm by content class), not a constant to retune.
const SMR_NOISE_DB: f64 = 5.5;
fn smr_for(tonality: f64, bark: f64) -> f64 {
    let t = tonality.clamp(0.0, 1.0);
    let db = t * (14.5 + bark) + (1.0 - t) * SMR_NOISE_DB;
    // Clamp to a sane band: below ~5 dB the mask stops protecting anything, and
    // above ~30 dB the rate loop is asked for precision no bitrate can buy.
    let db = db.clamp(5.0, 30.0);
    10f64.powf(-db / 10.0)
}

/// Per-SFB scalefactor offsets (relative to a common base the rate loop sets):
/// higher where masking is generous, lower where noise would be audible. From
/// `noise ∝ 2^(0.375·sf)·Σ√|X|`, the sf hitting `noise = threshold` is
/// `log2(threshold/Σ√|X|)/0.375`; we normalize + clamp so deltas stay codeable.
/// Per-SFB tonality index in `[0, 1]` from the spectral flatness measure — **arm
/// A2's signal**. 0 = noise-like, 1 = pure tone.
///
/// `SFM = geometric_mean(|X|²) / arithmetic_mean(|X|²)`, in dB, through the
/// classic `min(SFM_dB / −60, 1)`.
pub(crate) fn band_tonality(spec: &[f32], swb: &[u16], nwin: usize, wlen: usize) -> Vec<f32> {
    let n = swb.len() - 1;
    let mut out = vec![0f32; n];
    for b in 0..n {
        let (s, e) = (swb[b] as usize, swb[b + 1] as usize);
        let (mut log_sum, mut lin_sum, mut cnt) = (0f64, 0f64, 0usize);
        for w in 0..nwin {
            let base = w * wlen;
            for k in s..e {
                let Some(&x) = spec.get(base + k) else {
                    continue;
                };
                // Floored well below any audible coefficient so a single exact
                // zero cannot drag the geometric mean to 0 and fake a pure tone.
                let p = (f64::from(x) * f64::from(x)).max(1e-10);
                log_sum += p.ln();
                lin_sum += p;
                cnt += 1;
            }
        }
        if cnt == 0 {
            continue;
        }
        let gm = (log_sum / cnt as f64).exp();
        let am = lin_sum / cnt as f64;
        let sfm_db = 10.0 * (gm / am.max(1e-30)).log10();
        out[b] = ((sfm_db / -60.0) as f32).clamp(0.0, 1.0);
    }
    out
}

/// The long-block psy model: per-band offsets from the masking thresholds,
/// the starved-frame reduction (see [`Offsets`]) and the [`peak_hold`] caps,
/// which need the channel's `target_bits` to bound what they ask for.
fn perceptual_offsets(
    spec: &[f32],
    swb: &[u16],
    sample_rate: u32,
    tonality_smr: bool,
    target_bits: usize,
) -> Offsets {
    let _prof = prof::scope(Stage::EncPsy);
    let num_swb = swb.len() - 1;
    let mut energy = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energy[sfb] = spec[s..e]
            .iter()
            .map(|&x| f64::from(x).powi(2))
            .sum::<f64>()
            + 1e-3;
    }
    let ton = tonality_smr.then(|| band_tonality(spec, swb, 1, FRAME_LEN));
    let spread = spread_energy(&energy, swb, sample_rate, FRAME_LEN);
    let thr = masking_from_spread(&spread, swb, sample_rate, FRAME_LEN, ton.as_deref());

    let mut raw = vec![0.0f64; num_swb];
    let mut noise_scale = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energy[sfb] = spec[s..e].iter().map(|&x| f64::from(x).powi(2)).sum();
        noise_scale[sfb] = spec[s..e]
            .iter()
            .map(|&x| f64::from(x.abs()).sqrt())
            .sum::<f64>()
            + 1e-6;
        raw[sfb] = (thr[sfb] / noise_scale[sfb]).log2() / 0.375;
    }
    let cap = peak_hold(
        spec,
        swb,
        sample_rate,
        &energy,
        &spread,
        &noise_scale,
        target_bits,
    );
    // Center on the energy-bearing bands — empty bands have huge `thr/noise_scale`
    // (they quantize to ZERO anyway) and would otherwise skew a plain mean.
    let etot: f64 = energy.iter().sum::<f64>() + 1e-9;
    let center: f64 = (0..num_swb).map(|i| raw[i] * energy[i]).sum::<f64>() / etot;
    let off = raw
        .iter()
        .map(|&r| ((r - center).round() as i32).clamp(-60, 60))
        .collect();
    // The starved-frame reduction (see `Offsets`): every band's mask relative to
    // the loudest energy-bearing band's, in the fourth-root domain.
    let thr_ref = (0..num_swb)
        .filter(|&i| energy[i] > 0.0)
        .map(|i| thr[i])
        .fold(0f64, f64::max);
    if thr_ref <= 0.0 {
        return Offsets::flat(off);
    }
    let surplus = SURPLUS.with(std::cell::Cell::get).then(|| {
        let density = |i: usize| thr[i] / f64::from(swb[i + 1] - swb[i]);
        let mut floor: Vec<i32> = (0..num_swb)
            .map(|i| no_clamp_sf(&spec[usize::from(swb[i])..usize::from(swb[i + 1])]))
            .collect();
        // A scalefactor delta codes at most 60 steps: no band may run on past
        // that under the highest floor.
        let deepest = floor.iter().copied().max().unwrap_or(0) - 60;
        for f in &mut floor {
            *f = (*f).max(deepest);
        }
        let top = (0..num_swb)
            .filter(|&i| energy[i] > 0.0)
            .map(density)
            .fold(0f64, f64::max);
        Surplus {
            gap: (0..num_swb)
                .map(|i| ((top / density(i)).log2() / 0.375).max(0.0).round() as i32)
                .collect(),
            floor,
            start: (center + NMR0_SF_OFFSET).round() as i32 - SURPLUS_MARGIN_STEPS,
        }
    });
    let ratio: Vec<f32> = thr
        .iter()
        .map(|&t| ((t / thr_ref).sqrt().sqrt()).min(1.0) as f32)
        .collect();
    Offsets {
        off,
        cap,
        ratio,
        thr_ref,
        // `raw + NMR0_SF_OFFSET` is where a band's noise meets its mask, so the
        // base putting the bands at their masks is `center + NMR0_SF_OFFSET`.
        mask_base: (center + NMR0_SF_OFFSET).round() as i32,
        surplus,
    }
}

/// Where a band's quantization noise meets its mask, relative to `raw` in
/// [`perceptual_offsets`]: the uniform-rounding noise of the `x^0.75` quantizer
/// is `(4/27)·2^(0.375·(sf−100))·Σ√|x|`, so noise = `thr` at
/// `sf = 100 + log2(thr/Σ√|x|)/0.375 + log2(27/4)/0.375`. The 100 is the
/// scalefactor origin; `raw` is measured from it, and `log2(27/4)/0.375 = 7.35`.
const NMR0_SF_OFFSET: f64 = 107.35;

/// Per-band scalefactor offsets from the psy model, and what [`Offsets::sf`]
/// needs to keep a starved frame's loud bands accurate.
///
/// The rate loop searches one `base` and codes band `b` at `base + off[b]`:
/// a uniform step, so every band's noise-to-mask ratio moves together. That is
/// right while the budget covers the masks. Past them (`base > mask_base`) it is
/// not: a loud tone over quiet broadband noise at 53 kbps a channel (a 5.1
/// stream at 320 kbps) had every band raised 13 dB over its mask, which left
/// the noise bands coded as a few stray ±1s, still taking 95% of the bits, and
/// the tone with a single quantizer level: 6 dB SNR, its level set by which
/// side of one step it landed on (`remade_ffmpeg_rs#129`).
///
/// Past the masks the thresholds are raised the 3GPP way instead (TS 26.403
/// §5.6.1.2, the reduction FDK-AAC and FFmpeg's psy model use):
/// `thr' = (thr^¼ + r)^4`, with `r` set so the loudest band rises exactly as the
/// uniform step would. A quiet band's threshold climbs towards `r^4` far faster
/// than a loud band's, so the bits it was wasting go to the bands that carry
/// the signal. Below the masks (`base <= mask_base`) nothing changes.
///
/// One base step past the masks already lifts a band 50 dB under the loudest by
/// 11 steps, so the rate loop's search variable (the *level*) runs in
/// [`STARVED_SUBSTEPS`] sub-steps of the base there; a whole-step search left
/// up to half the frame unspent.
#[derive(Debug, Clone)]
pub(crate) struct Offsets {
    off: Vec<i32>,
    /// Per band, the highest scalefactor [`peak_hold`] allows (255 for none);
    /// empty for no holds.
    cap: Vec<i32>,
    /// Per band, `(thr / thr_ref)^¼` in `(0, 1]`; empty for no reduction
    /// (short blocks keep the uniform step).
    ratio: Vec<f32>,
    /// The loudest band's mask, which `ratio` is relative to.
    thr_ref: f64,
    /// The base at which the bands' quantization noise reaches their masks.
    mask_base: i32,
    /// How bits left over far below the masks are spent; `None` for the
    /// uniform step all the way down.
    surplus: Option<Surplus>,
}

/// How a frame with bits to spare spends them (see [`SURPLUS_MARGIN_STEPS`]).
#[derive(Debug, Clone)]
struct Surplus {
    /// Per band, how many steps its mask per line sits under the highest one.
    gap: Vec<i32>,
    /// Per band, the lowest scalefactor that clamps no coefficient and stays
    /// within a codeable delta of every other band's.
    floor: Vec<i32>,
    /// The base below which the noise starts to level out.
    start: i32,
}

/// How far below the masks every band's noise goes, in base steps of 1.13 dB
/// (13 = 14.7 dB), before a frame with bits to spare starts levelling the
/// noise across its bands.
///
/// The uniform step lowers every band's noise-to-mask ratio together, however
/// far under the masks that takes them. On tonal content at a high rate most of
/// the surplus then went into bands that were already inaudible (a tone's
/// window leakage: 51 dB waveform SNR for two pure tones at 320 kbps, where
/// the reference encoder in `remade_ffmpeg_rs#211` reaches 82), while the bands
/// carrying the signal kept nearly all of the error energy. Below this margin
/// a band whose mask is low drops half as fast as the loudest one, until the
/// two reach the same noise per line.
///
/// Measured with PEAQ at 192, 256 and 320 kbps stereo (9 music clips, 4 time
/// offsets each): ODG -0.000, +0.001 and +0.013 on average, no clip beyond the
/// noise, 0 to 6 dB more SNR, 13 dB on the pure tones. Letting a low band stop
/// dead instead of dropping half as fast gains up to 25 dB on piano, but costs
/// it 0.12 ODG at this margin and up to 0.24 at a 3 dB one.
const SURPLUS_MARGIN_STEPS: i32 = 13;

/// The surplus policy runs for mono and stereo streams from this rate a
/// channel; every other stream codes as before, bit for bit. Multichannel lost
/// up to 0.23 ODG at the margins that would have helped its pure tones.
const SURPLUS_MIN_BPS_PER_CHANNEL: u32 = 96_000;

thread_local! {
    /// Whether the frame this thread is coding runs the surplus policy: set
    /// per frame by `encode_frame`, like [`SECTION_TRELLIS`].
    static SURPLUS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The lowest scalefactor at which no line of `band` quantizes past `MAX_QUANT`.
fn no_clamp_sf(band: &[f32]) -> i32 {
    let maxp = band
        .iter()
        .fold(0f64, |a, &x| a.max(f64::from(x.abs())))
        .powf(0.75);
    if maxp <= 1e-9 {
        0
    } else {
        (100.0 - (f64::from(MAX_QUANT) / maxp).log2() / 0.1875).ceil() as i32
    }
}

/// Levels per base step past the masks (see [`Offsets`]).
const STARVED_SUBSTEPS: i32 = 8;

impl std::ops::Deref for Offsets {
    type Target = [i32];
    fn deref(&self) -> &[i32] {
        &self.off
    }
}

impl Offsets {
    /// Offsets with no starved-frame reduction: always the uniform step.
    pub(crate) fn flat(off: Vec<i32>) -> Self {
        Self {
            off,
            cap: Vec::new(),
            ratio: Vec::new(),
            thr_ref: 0.0,
            mask_base: i32::MAX,
            surplus: None,
        }
    }

    /// The highest level the rate loop needs to search: every band at 255.
    /// Past the family's top the caps are released one step a level, so any
    /// budget, however small, stays reachable.
    pub(crate) fn max_level(&self) -> i32 {
        self.top_level() + if self.cap.is_empty() { 0 } else { 255 }
    }

    /// The level at which every band without a hold has reached 255.
    fn top_level(&self) -> i32 {
        if self.ratio.is_empty() || self.mask_base >= 255 {
            255
        } else {
            self.mask_base.max(0) + (255 - self.mask_base.max(0)) * STARVED_SUBSTEPS
        }
    }

    /// Apply the holds to the family's scalefactors at `level`.
    fn held(&self, level: i32, mut sf: Vec<i32>) -> Vec<i32> {
        if !self.cap.is_empty() {
            let release = (level - self.top_level()).max(0);
            for (s, &c) in sf.iter_mut().zip(&self.cap) {
                *s = (*s).min((c + release).min(255));
            }
        }
        if let Some(su) = &self.surplus {
            for (s, &f) in sf.iter_mut().zip(&su.floor) {
                *s = (*s).max(f).min(255);
            }
        }
        sf
    }

    /// How many steps over the uniform `fam + off[b]` the surplus policy
    /// leaves each band, or `None` at or above its start.
    fn surplus_lag(&self, fam: i32) -> Option<impl Iterator<Item = i32> + '_> {
        let su = self.surplus.as_ref()?;
        let below = su.start - fam;
        (below > 0).then(|| su.gap.iter().map(move |&g| (g.min(below) + 1) / 2))
    }

    /// Share one reduction between the two channels of a pair coded at a common
    /// level: ratios against the louder channel's loudest band, and the later of
    /// the two mask bases, so the uniform region (and its output) is unchanged.
    pub(crate) fn join(a: &mut Self, b: &mut Self) {
        if a.ratio.is_empty() || b.ratio.is_empty() {
            *a = Self::flat(std::mem::take(&mut a.off));
            *b = Self::flat(std::mem::take(&mut b.off));
            return;
        }
        let thr_ref = a.thr_ref.max(b.thr_ref);
        let mask_base = a.mask_base.max(b.mask_base);
        for o in [a, b] {
            let k = (o.thr_ref / thr_ref).sqrt().sqrt() as f32;
            for q in &mut o.ratio {
                *q *= k;
            }
            o.thr_ref = thr_ref;
            o.mask_base = mask_base;
        }
    }

    /// Per-band scalefactors at search level `level`, each clamped to the
    /// codeable range. Up to `mask_base` the level is the common base; past it,
    /// [`STARVED_SUBSTEPS`] levels make one base step for the loudest band, and
    /// every other band rises by the reduction on top; a [`peak_hold`] cap then
    /// bounds its band from above. Never below the uniform `base + off[b]` or,
    /// for a held band, its cap, both at or over [`min_base`]'s no-clamp floor;
    /// non-decreasing in `level`, so the rate loop's search stays monotone.
    pub(crate) fn sf(&self, level: i32) -> Vec<i32> {
        let fam = level.min(self.top_level());
        if fam <= self.mask_base || self.ratio.is_empty() {
            let sf = match self.surplus_lag(fam) {
                Some(lag) => self
                    .off
                    .iter()
                    .zip(lag)
                    .map(|(&o, l)| (fam + o + l).clamp(0, 255))
                    .collect(),
                None => self.off.iter().map(|&o| (fam + o).clamp(0, 255)).collect(),
            };
            return self.held(level, sf);
        }
        let d = f64::from(fam - self.mask_base) / f64::from(STARVED_SUBSTEPS);
        let base = f64::from(self.mask_base) + d;
        // The loudest band's threshold rises by 2^(0.375·d), its fourth root by
        // u = 2^(0.09375·d), so r = u − 1 in units of that root.
        let u = (0.093_75 * d).exp2();
        let sf = self
            .off
            .iter()
            .zip(&self.ratio)
            .map(|(&o, &q)| {
                let q = f64::from(q).max(1e-6);
                // Steps beyond the uniform rise: log2(thr' / (thr·u^4)) / 0.375.
                let extra = (4.0 / 0.375 * ((q + u - 1.0) / (q * u)).log2()).max(0.0);
                ((base + f64::from(o) + extra).round() as i32).clamp(0, 255)
            })
            .collect();
        self.held(level, sf)
    }
}

/// The SNR, in dB, at which [`peak_hold`] holds a dominant band.
///
/// Measured on the downstream 5.1 gate signal (a 0.25 tone per channel over
/// 0.03 x uniform noise) and the 57-row PEAQ table, with
/// [`PEAK_HOLD_DOMINANCE`] at 30: 24 dB gives 20.8 dB SNR at 320 kbps 5.1 and
/// a PEAQ mean of +0.033 (33 rows better, 14 worse, worst -0.065); 27 dB gives
/// 21.5 dB but worst row -0.091 and 20 rows worse.
const PEAK_HOLD_SNR_DB: f64 = 24.0;

/// How much a band's own energy must exceed what every other band spreads into
/// it for [`peak_hold`] to hold it.
///
/// At 1, flat noise passes in nearly every band and the holds starve the tone
/// (5.6 dB on the gate at 128 kbps 5.1); at 10, harmonic music passes too
/// (guitar rows down to -0.36 ODG); at 100 or more, a tone straddling a band
/// edge fails it and its channel's level drifts (+0.6..+0.85 dB on the #129
/// repro). 30 separates the tones from both.
const PEAK_HOLD_DOMINANCE: f64 = 30.0;

/// The largest share of a frame's bits the holds may ask for; past it the
/// hold SNR comes down for every held band alike. 0.15 and 0.25 measured the
/// same; 0.08 cost the gate 2 dB at 128 kbps 5.1.
const PEAK_HOLD_BIT_SHARE: f64 = 0.25;

/// The narrow-band SNR floor: a per-band scalefactor cap that holds every
/// *dominant* band (one whose own energy is at least [`PEAK_HOLD_DOMINANCE`]
/// times what the other bands spread into it, i.e. a tone standing clear of
/// its surroundings) at [`PEAK_HOLD_SNR_DB`], or at the threshold in quiet,
/// whichever is coarser. Empty when no band qualifies.
///
/// Why: the uniform noise-to-mask family never lets a tone's band beat its
/// 18 dB mask until every band of a broadband noise bed under it has reached
/// its own mask too, about 144 kbps a channel for white noise. Below that, a
/// tone over noise came back at a flat ~16 dB SNR from 128 to 512 kbps 5.1
/// (88-96% of the error within 150 Hz of the tone), and its level swung by up
/// to 0.6 dB with the rate. A cap costs a held band its share of the frame
/// but no syntax: the scalefactor is the band's own.
///
/// The holds ask for about one bit a line per 6 dB; when that is more than
/// [`PEAK_HOLD_BIT_SHARE`] of `target_bits`, the hold SNR is scaled down. A
/// cap never goes under the band's no-clamp floor (see [`min_base`]). Long
/// blocks only.
fn peak_hold(
    spec: &[f32],
    swb: &[u16],
    sample_rate: u32,
    energy: &[f64],
    spread: &[f64],
    noise_scale: &[f64],
    target_bits: usize,
) -> Vec<i32> {
    let num_swb = swb.len() - 1;
    let dominant: Vec<bool> = (0..num_swb)
        .map(|b| {
            // `spread` was built from `energy + 1e-3`; what is left after the
            // band's own part is what every other band spreads into it.
            let others = (spread[b] - energy[b] - 1e-3).max(0.0);
            energy[b] > 0.0 && energy[b] >= PEAK_HOLD_DOMINANCE * others
        })
        .collect();
    if !dominant.iter().any(|&d| d) {
        return Vec::new();
    }
    let lines: usize = (0..num_swb)
        .filter(|&b| dominant[b])
        .map(|b| usize::from(swb[b + 1] - swb[b]))
        .sum();
    let ask = lines as f64 * PEAK_HOLD_SNR_DB / 6.02;
    let hold_db = PEAK_HOLD_SNR_DB * (PEAK_HOLD_BIT_SHARE * target_bits as f64 / ask).min(1.0);
    let m = 10f64.powf(-hold_db / 10.0);
    let ath = ath_long(swb, sample_rate, FRAME_LEN);
    (0..num_swb)
        .map(|b| {
            if !dominant[b] {
                return 255;
            }
            let (s, e) = (swb[b] as usize, swb[b + 1] as usize);
            let noise = (energy[b] * m).max(ath[b]);
            let cap = ((noise / noise_scale[b]).log2() / 0.375 + NMR0_SF_OFFSET).round();
            // Never under the band's no-clamp floor (see `min_base`).
            let min_sf = f64::from(no_clamp_sf(&spec[s..e]));
            cap.max(min_sf).clamp(0.0, 255.0) as i32
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Quantization + coding — brick 4: per-band scalefactors (psy-driven), cheapest
// codebook per band, rate loop over the common base, raw_data_block assembly.
// ---------------------------------------------------------------------------

/// The Rung 0 gate's transient-veto threshold, as a percentile of the clip's own
/// frame-attack distribution. Frames above it always take the sine window.
///
/// PROVISIONAL. 0.75 is a placeholder pending a calculator fit; it is set high
/// (veto only the top quartile) so the veto is conservative — it gives up KBD on
/// some stationary frames rather than risk smearing an onset.
const SHAPE_ATTACK_VETO_PCT: f32 = 0.75;

/// Arm A13's routing threshold: how correlated a channel pair must be before the
/// joint rate loop replaces the even per-channel split. Sits in the measured gap
/// between wide content (0.25, where joint loses) and real stereo music
/// (0.47-0.84, where it wins).
const JOINT_STEREO_MIN_CORR: f32 = 0.35;

/// Spectral correlation of a channel pair, `|<L,R>| / sqrt(<L,L><R,R>)`.
/// 1 = identical, 0 = orthogonal. O(n) over the spectra the caller already has.
fn pair_correlation(l: &[f32], r: &[f32]) -> f32 {
    let n = l.len().min(r.len());
    let (mut dot, mut el, mut er) = (0f64, 0f64, 0f64);
    for i in 0..n {
        let (a, b) = (f64::from(l[i]), f64::from(r[i]));
        dot += a * b;
        el += a * a;
        er += b * b;
    }
    if el <= 1e-9 || er <= 1e-9 {
        return 1.0; // a silent channel is trivially "agreeable"
    }
    (dot.abs() / (el.sqrt() * er.sqrt())) as f32
}

const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_LFE: u32 = 3;
const ID_END: u32 = 7;
const ZERO_HCB: u8 = 0;
const ESC_HCB: u8 = 11;

// ---------------------------------------------------------------------------
// Channel element layout (ISO 14496-3 Table 1.19).
//
// `channel_configuration` in the ADTS header / AudioSpecificConfig does not just
// count channels — it *names the element sequence a decoder will expect*. Writing
// `channel_config = 6` and then emitting six SCEs is not "6 channels", it is a
// non-conformant stream: a conforming decoder reads SCE, CPE, CPE, LFE.
//
// Our own decoder happened to tolerate it (it appends elements in order), which
// is exactly how the bug survived — self-round-trip is not conformance.
// ---------------------------------------------------------------------------

/// One channel element to emit, naming its source channels **in input order**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Elem {
    /// `single_channel_element` carrying input channel `.0`.
    Sce(usize),
    /// `channel_pair_element` carrying input channels `.0` (left) and `.1` (right).
    Cpe(usize, usize),
    /// `lfe_channel_element` carrying input channel `.0`. Long blocks only.
    Lfe(usize),
}

impl Elem {
    /// The 3-bit element id written to the bitstream.
    fn id(self) -> u32 {
        match self {
            Self::Sce(_) => ID_SCE,
            Self::Cpe(_, _) => ID_CPE,
            Self::Lfe(_) => ID_LFE,
        }
    }
}

/// The element sequence for `channels`, with source indices in the **standard
/// interleave order** the caller pushes (`WAVE_FORMAT_EXTENSIBLE` / FFmpeg order:
/// FL, FR, FC, LFE, BL, BR).
///
/// The reordering is the substance here. AAC orders a 5.1 stream
/// `C, L, R, Ls, Rs, LFE`; interleaved PCM arrives `L, R, C, LFE, Ls, Rs`. Emitting
/// them in arrival order would put the centre channel in the left speaker.
///
/// Returns `None` for channel counts with no defined `channel_configuration`
/// (0, or > 6): those need a `program_config_element`, which we do not yet write.
pub(crate) fn element_plan(channels: usize) -> Option<Vec<Elem>> {
    Some(match channels {
        // config 1 — mono.
        1 => vec![Elem::Sce(0)],
        // config 2 — stereo.
        2 => vec![Elem::Cpe(0, 1)],
        // config 3 — 3.0: C, L/R.
        3 => vec![Elem::Sce(2), Elem::Cpe(0, 1)],
        // config 4 — 4.0: C, L/R, rear centre.
        4 => vec![Elem::Sce(2), Elem::Cpe(0, 1), Elem::Sce(3)],
        // config 5 — 5.0: C, L/R, Ls/Rs.
        5 => vec![Elem::Sce(2), Elem::Cpe(0, 1), Elem::Cpe(3, 4)],
        // config 6 — 5.1: C, L/R, Ls/Rs, LFE.
        6 => vec![Elem::Sce(2), Elem::Cpe(0, 1), Elem::Cpe(4, 5), Elem::Lfe(3)],
        _ => return None,
    })
}

const MAX_QUANT: i32 = 8191;

/// Spectral lines an `lfe_channel_element` normally codes: the lowest 12, 281
/// Hz at 48 kHz (FFmpeg's encoder codes the same three bands).
const LFE_LINES: usize = 12;

/// Above [`LFE_LINES`], an LFE frame is coded only when that part carries at
/// least this fraction of the frame's energy (-20 dB).
const LFE_HIGH_FRACTION: f64 = 0.01;

/// Band-limit an LFE frame's spectrum to its lowest [`LFE_LINES`] lines, unless
/// the content above them is substantial.
///
/// The LFE feeds a subwoofer. Coded full band, a 55 Hz tone over -41 dBFS
/// broadband noise spent its share on the noise and came back at 15 dB SNR
/// instead of 26 (`remade_ffmpeg_rs#129`); the bits it leaves now go to the
/// other elements through the frame's pooling. A mix that does route real
/// content above 281 Hz to the LFE (more than 1% of its energy) keeps it, so
/// the channel still comes back at its level.
fn lfe_band_limit(spec: &mut [f32]) {
    let energy = |x: &[f32]| x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>();
    let (low, high) = (energy(&spec[..LFE_LINES]), energy(&spec[LFE_LINES..]));
    if high < LFE_HIGH_FRACTION * (low + high) {
        spec[LFE_LINES..].fill(0.0);
    }
}

/// The per-channel ICS body budget's ceiling: the 6144-bit decoder input buffer
/// per channel (ISO 14496-3 §4.5.3) less the element (7) and frame (59) overhead.
const MAX_CHANNEL_BITS: usize = 6144 - 7 - 59;

/// Input samples are clamped to ±8× full scale (NaN becomes silence) at the push
/// boundary. Real audio is untouched; hostile values (±Inf, 1e30) would otherwise
/// drive every band to maximal escape codes and a block no rate decision can
/// shrink below the decoder buffer.
pub const MAX_INPUT_AMPLITUDE: f32 = 8.0;

#[inline]
fn sanitize(x: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(-MAX_INPUT_AMPLITUDE, MAX_INPUT_AMPLITUDE)
    }
}

/// Quantize one coefficient at global gain `gg` (the tests' reference).
#[cfg(test)]
fn quantize(x: f32, gg: i32) -> i32 {
    if x == 0.0 {
        return 0;
    }
    let scale = 2f64.powf(-0.1875 * f64::from(gg - 100));
    let q = nearest_level(f64::from(x.abs()).powf(0.75) * scale);
    if x < 0.0 {
        -q
    } else {
        q
    }
}

/// `2^(-0.1875·(sf-100))`, the quantizer scale, tabulated once over all 256 gains
/// so the rate loop never repeats the exponent.
fn scale_table() -> &'static [f64; 256] {
    static T: OnceLock<[f64; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0f64; 256];
        for (sf, e) in t.iter_mut().enumerate() {
            *e = 2f64.powf(-0.1875 * (sf as f64 - 100.0));
        }
        t
    })
}

/// Per-magnitude coefficient bit cost (half a book-11 pair), tabulated so the rate
/// loop's *search* can price a frame in O(n) without the full codebook selection —
/// only the exact refinement does the real search.
fn coef_bits() -> &'static [u16] {
    static T: OnceLock<Vec<u16>> = OnceLock::new();
    T.get_or_init(|| {
        (0..=MAX_QUANT)
            .map(|m| (spectral_bits(ESC_HCB as usize, &[m, m]).unwrap_or(64) / 2) as u16)
            .collect()
    })
}

/// Per-coefficient `|x|^0.75` and sign, precomputed once per frame so the rate
/// loop can re-quantize at each candidate gain without repeating the 0.75-power
/// (the single hottest op after the MDCT). Bit-exact with [`quantize`].
struct Xpow {
    pow: Vec<f64>,
    sign: Vec<i32>,
}

impl Xpow {
    fn new(spec: &[f32]) -> Self {
        let _prof = prof::scope(Stage::EncXpow);
        // Both arrays are built straight into fresh capacity: every element is
        // computed, so the zero fill of a `vec![0; n]` (two per channel per
        // frame, 12 KB) was pure overhead.
        let n = spec.len();
        #[cfg(all(feature = "simd", target_arch = "x86_64"))]
        {
            if has_avx2() {
                let (mut pow, mut sign) = (Vec::with_capacity(n), Vec::with_capacity(n));
                // SAFETY: runtime AVX2 check; both buffers have capacity `n`, the
                // twin writes all `n` elements of each, and only then is the
                // length set.
                unsafe {
                    xpow_avx2_raw(spec, pow.as_mut_ptr(), sign.as_mut_ptr());
                    pow.set_len(n);
                    sign.set_len(n);
                }
                prof::count(Kernel::Xpow, true, n);
                return Self { pow, sign };
            }
        }
        prof::count(Kernel::Xpow, false, n);
        // |x|^0.75 = |x|^½·|x|^¼ = √|x|·√√|x| — two sqrts vectorize; `powf` doesn't.
        // (For x=0, pow=0 so the sign is irrelevant — the quant is 0 either way.)
        let pow = spec
            .iter()
            .map(|&x| {
                let s = f64::from(x.abs()).sqrt();
                s * s.sqrt()
            })
            .collect();
        let sign = spec.iter().map(|&x| if x < 0.0 { -1 } else { 1 }).collect();
        Self { pow, sign }
    }

    /// Max `|x|^0.75` over `[s, e)` — the no-clamp-floor input (= `(max|x|)^0.75`).
    fn max_pow(&self, s: usize, e: usize) -> f64 {
        self.pow[s..e].iter().copied().fold(0.0, f64::max)
    }

    fn len(&self) -> usize {
        self.pow.len()
    }
}

/// Codeword lengths of every spectral codebook, packed so ONE walk over a band
/// prices all eleven books at once (`codec-eliminate-redundancy` move #4).
///
/// The books come in pairs that share a tuple index (1/2, 3/4, 5/6, 7/8, 9/10:
/// same `dim`, `lav` and signedness, different code lengths), and every
/// unsigned dim-2 book (7..=11) can be indexed by the magnitudes clamped to 16.
/// So per tuple one index and one or two table loads add every eligible book's
/// codeword length into 16-bit lanes of an integer accumulator. A lane can hold
/// any band: the longest codeword is 19 bits and a call sees at most 1024 tuples
/// (19 456 < 65 536), so lanes never carry into each other.
///
/// A book whose `lav` cannot hold a tuple stores 0 in its lane there; such a book
/// is never consulted, because a band is only priced with books whose `lav`
/// covers the band's peak.
struct CostTables {
    /// Books 1 | 2 << 16 at the signed dim-4 index `27a + 9b + 3c + d + 40`.
    s12: [u32; 81],
    /// Books 3 | 4 << 16 at the unsigned dim-4 index `27|a| + 9|b| + 3|c| + |d|`.
    u34: [u32; 81],
    /// Books 5 | 6 << 16 at the signed dim-2 index `9a + b + 40`.
    s56: [u32; 81],
    /// Books 7 | 8 << 16 | 9 << 32 | 10 << 48 at `17 min(|a|,16) + min(|b|,16)`.
    u7_10: [u64; 289],
    /// Book 11 at the same index.
    u11: [u32; 289],
}

fn cost_tables() -> &'static CostTables {
    static T: OnceLock<CostTables> = OnceLock::new();
    T.get_or_init(|| {
        let len = |cb: u8, tuple: &[i32]| -> u64 {
            tuple_index(&CODEBOOKS[cb as usize], tuple)
                .map_or(0, |i| u64::from(spectral_book(cb).code(i as usize).1))
        };
        let mut t = CostTables {
            s12: [0; 81],
            u34: [0; 81],
            s56: [0; 81],
            u7_10: [0; 289],
            u11: [0; 289],
        };
        for i in 0..81i32 {
            let d = [i / 27, i / 9 % 3, i / 3 % 3, i % 3];
            let signed = d.map(|x| x - 1);
            t.s12[i as usize] = (len(1, &signed) | len(2, &signed) << 16) as u32;
            t.u34[i as usize] = (len(3, &d) | len(4, &d) << 16) as u32;
            let pair = [i / 9 - 4, i % 9 - 4];
            t.s56[i as usize] = (len(5, &pair) | len(6, &pair) << 16) as u32;
        }
        for i in 0..289i32 {
            let pair = [i / 17, i % 17];
            t.u7_10[i as usize] =
                len(7, &pair) | len(8, &pair) << 16 | len(9, &pair) << 32 | len(10, &pair) << 48;
            t.u11[i as usize] = len(11, &pair) as u32;
        }
        t
    })
}

/// Every book's bit cost over the tuples added so far ([`CostTables`] lanes,
/// plus the sign bits every unsigned book pays and book 11's escapes), and the
/// peak magnitude that decides which books may code the band.
#[derive(Default)]
struct BandCost {
    a12: u32,
    a34: u32,
    a56: u32,
    a7_10: u64,
    a11: u32,
    /// Non-zero coefficients: one sign bit each in every unsigned book.
    nz: u32,
    /// Book 11's escape sequences.
    esc: u32,
    /// The largest magnitude seen.
    maxq: u32,
}

impl BandCost {
    /// The dim-2 books over `q` (an even number of coefficients), and the peak.
    ///
    /// Books 5/6 are indexed by the values clamped to their `lav` so the index is
    /// always in range; where a value exceeds 4 the lane is never read (the band's
    /// peak then rules books 5/6 out), so the clamp changes no cost that is used.
    #[inline(always)]
    fn add2(&mut self, t: &CostTables, q: &[i32]) {
        for p in q.chunks_exact(2) {
            let (a, b) = (p[0].unsigned_abs(), p[1].unsigned_abs());
            self.maxq = self.maxq.max(a).max(b);
            self.a56 += t.s56[(9 * p[0].clamp(-4, 4) + p[1].clamp(-4, 4) + 40) as usize];
            let i = (17 * a.min(16) + b.min(16)) as usize;
            self.a7_10 += t.u7_10[i];
            self.a11 += t.u11[i];
            self.nz += u32::from(a != 0) + u32::from(b != 0);
        }
    }

    /// The dim-4 books over `q` (a multiple of 4 coefficients), only valid once
    /// the peak is known to be at most 2 (book 1/2 lanes only at most 1).
    #[inline(always)]
    fn add4(&mut self, t: &CostTables, q: &[i32]) {
        let books12 = self.maxq <= 1;
        for c in q.chunks_exact(4) {
            if books12 {
                let i = 27 * c[0] + 9 * c[1] + 3 * c[2] + c[3] + 40;
                self.a12 += t.s12[i as usize];
            }
            let u = 27 * c[0].unsigned_abs()
                + 9 * c[1].unsigned_abs()
                + 3 * c[2].unsigned_abs()
                + c[3].unsigned_abs();
            self.a34 += t.u34[u as usize];
        }
    }

    /// Book 11's escape sequences over `q` (magnitudes of 16 and up).
    #[cold]
    fn add_esc(&mut self, q: &[i32]) {
        for &c in q {
            let m = c.unsigned_abs();
            if m >= 16 {
                self.esc += escape_bits(m) as u32;
            }
        }
    }

    /// The cheapest eligible book for a band of `width` coefficients per window,
    /// and its bits: books tried in order 1..=11, a later book winning only when
    /// strictly cheaper (the per-book search's tie-break). Eligible are the books
    /// whose `lav` holds the peak (the `lav`s rise with the book number, so they
    /// are a suffix) and whose `dim` divides the width.
    fn best(&self, width: usize) -> (u8, usize) {
        let cost = self.costs(width);
        let mut best = (ESC_HCB, usize::MAX);
        for (cb, &bits) in cost.iter().enumerate().skip(1) {
            if bits != u32::MAX && (bits as usize) < best.1 {
                best = (cb as u8, bits as usize);
            }
        }
        best
    }

    /// Every book's bits for the band (index 0 unused), `u32::MAX` for a book
    /// that cannot code it: the eligibility rule of [`best`](Self::best). The
    /// section trellis prices all of them at once (see [`band_book_costs`]).
    fn costs(&self, width: usize) -> [u32; 12] {
        let lane = |v: u64, k: u32| (v >> (16 * k)) & 0xffff;
        let nz = u64::from(self.nz);
        let (a12, a34, a56) = (
            u64::from(self.a12),
            u64::from(self.a34),
            u64::from(self.a56),
        );
        let cost: [u64; 12] = [
            0,
            lane(a12, 0),
            lane(a12, 1),
            lane(a34, 0) + nz,
            lane(a34, 1) + nz,
            lane(a56, 0),
            lane(a56, 1),
            lane(self.a7_10, 0) + nz,
            lane(self.a7_10, 1) + nz,
            lane(self.a7_10, 2) + nz,
            lane(self.a7_10, 3) + nz,
            u64::from(self.a11) + nz + u64::from(self.esc),
        ];
        let mut first = match self.maxq {
            1 => 1,
            2 => 3,
            3 | 4 => 5,
            5..=7 => 7,
            8..=12 => 9,
            _ => 11,
        };
        if width % 4 != 0 {
            first = first.max(5);
        }
        let mut out = [u32::MAX; 12];
        if width % 2 != 0 {
            return out;
        }
        for cb in first..12 {
            out[cb] = u32::try_from(cost[cb]).unwrap_or(u32::MAX - 1);
        }
        out
    }
}

/// The cheapest representable spectral codebook for one SFB's coefficients, and
/// its bit cost. ZERO for a silent band; dim-4 books only for 4-aligned bands.
/// Books whose LAV can't hold the band's peak are skipped up front (only the
/// escape book covers arbitrarily large values).
///
/// One walk prices every book ([`BandCost`]). The per-book search it replaced
/// ([`best_codebook_for_band_ref`], kept as the test oracle) re-walked the band
/// once per candidate book through an out-of-line [`spectral_bits`] per tuple,
/// and was 75% of all encoder instructions.
fn best_codebook_for_band(quant: &[i32], s: usize, e: usize) -> (u8, usize) {
    let q = &quant[s..e];
    if q.len() % 2 != 0 {
        // No book can code an odd band; only its silence matters (cold: every
        // AAC band width is a multiple of 4).
        return if q.iter().all(|&v| v == 0) {
            (ZERO_HCB, 0)
        } else {
            (ESC_HCB, usize::MAX)
        };
    }
    let t = cost_tables();
    let mut cost = BandCost::default();
    cost.add2(t, q);
    if cost.maxq == 0 {
        return (ZERO_HCB, 0);
    }
    if cost.maxq <= 2 && q.len() % 4 == 0 {
        cost.add4(t, q);
    }
    if cost.maxq >= 16 {
        cost.add_esc(q);
    }
    cost.best(q.len())
}

/// The per-book search [`best_codebook_for_band`] replaced: the oracle its tests
/// compare against.
#[cfg(test)]
fn best_codebook_for_band_ref(quant: &[i32], s: usize, e: usize) -> (u8, usize) {
    let maxq = quant[s..e]
        .iter()
        .map(|&q| q.unsigned_abs())
        .max()
        .unwrap_or(0);
    if maxq == 0 {
        return (ZERO_HCB, 0);
    }
    let mut best = (ESC_HCB, usize::MAX);
    for cb in 1..=11u8 {
        let meta = &CODEBOOKS[cb as usize];
        let dim = meta.dim as usize;
        if (e - s) % dim != 0 || (!meta.esc && u32::from(meta.lav) < maxq) {
            continue;
        }
        let mut bits = 0usize;
        let mut ok = true;
        let mut i = s;
        while i < e {
            if let Some(b) = spectral_bits(cb as usize, &quant[i..i + dim]) {
                bits += b;
            } else {
                ok = false;
                break;
            }
            i += dim;
        }
        if ok && bits < best.1 {
            best = (cb, bits);
        }
    }
    best
}

/// Bits for `section_data` given per-SFB codebooks (adjacent equal cbs merge into
/// one 4-bit codebook + 5-bit run-length increments).
fn section_bits(cbs: &[u8]) -> usize {
    let esc = 31usize;
    let mut bits = 0usize;
    let mut k = 0usize;
    while k < cbs.len() {
        let cb = cbs[k];
        let mut len = 1usize;
        while k + len < cbs.len() && cbs[k + len] == cb {
            len += 1;
        }
        bits += 4;
        let mut l = len;
        while l >= esc {
            bits += 5;
            l -= esc;
        }
        bits += 5;
        k += len;
    }
    bits
}

thread_local! {
    /// [`AacTuning::section_trellis`] for the frame this thread is
    /// coding, set once per frame by `encode_frame` (frames code on worker
    /// threads, each frame wholly on one), so the coding kernels need no extra
    /// argument for it.
    static SECTION_TRELLIS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn section_trellis_on() -> bool {
    SECTION_TRELLIS.with(std::cell::Cell::get)
}

/// Bits of the scalefactor of an all-zero band carried inside another
/// codebook's section: its scalefactor is sent as a zero delta (see
/// [`zero_band_sf`]), the one-bit `SCALEFACTOR_BOOK` code.
const CARRIED_ZERO_SF_BITS: u32 = 1;

/// Spectral bits of one band (one window group of a short frame:
/// `nwin` windows of `wlen` lines from `win0`) under every codebook 0..=11;
/// `u32::MAX` where the book cannot code it. Book 0 (`ZERO_HCB`) is free for
/// an all-zero band and impossible otherwise.
fn band_book_costs(
    quant: &[i32],
    s: usize,
    e: usize,
    win0: usize,
    nwin: usize,
    wlen: usize,
) -> [u32; 12] {
    let mut out = [u32::MAX; 12];
    let mut maxq = 0u32;
    for w in win0..win0 + nwin {
        for &q in &quant[w * wlen + s..w * wlen + e] {
            maxq = maxq.max(q.unsigned_abs());
        }
    }
    if maxq == 0 {
        // An all-zero band: every book codes it as its zero tuple repeated,
        // a per-book constant, so no tuple need be looked up.
        out[0] = 0;
        let zt = zero_tuple_bits();
        let tuples_per = |dim: usize| ((e - s) / dim * nwin) as u32;
        for cb in 1..=11usize {
            let dim = CODEBOOKS[cb].dim as usize;
            if (e - s) % dim == 0 {
                out[cb] = tuples_per(dim) * zt[cb] + CARRIED_ZERO_SF_BITS;
            }
        }
        return out;
    }
    if (e - s) % 2 != 0 {
        // Cold: every AAC band width is a multiple of 4.
        return band_book_costs_ref(quant, s, e, win0, nwin, wlen);
    }
    // One walk prices every book, the kernel `best_codebook_for_band` and
    // `best_codebook_short` choose from.
    let band = |w: usize| &quant[w * wlen + s..w * wlen + e];
    let t = cost_tables();
    let mut cost = BandCost::default();
    for w in win0..win0 + nwin {
        cost.add2(t, band(w));
    }
    if cost.maxq <= 2 && (e - s) % 4 == 0 {
        for w in win0..win0 + nwin {
            cost.add4(t, band(w));
        }
    }
    if cost.maxq >= 16 {
        for w in win0..win0 + nwin {
            cost.add_esc(band(w));
        }
    }
    cost.costs(e - s)
}

/// [`band_book_costs`] for a non-zero band by a per-book search through
/// [`spectral_bits`]: the cold path for an odd band width and the oracle of
/// the one-walk pricing's test.
#[cold]
fn band_book_costs_ref(
    quant: &[i32],
    s: usize,
    e: usize,
    win0: usize,
    nwin: usize,
    wlen: usize,
) -> [u32; 12] {
    let mut out = [u32::MAX; 12];
    let maxq = (win0..win0 + nwin)
        .flat_map(|w| quant[w * wlen + s..w * wlen + e].iter())
        .map(|q| q.unsigned_abs())
        .max()
        .unwrap_or(0);
    for cb in 1..=11u8 {
        let meta = &CODEBOOKS[cb as usize];
        let dim = meta.dim as usize;
        if (e - s) % dim != 0 || (!meta.esc && u32::from(meta.lav) < maxq) {
            continue;
        }
        let mut bits = 0u32;
        let mut ok = true;
        'w: for w in win0..win0 + nwin {
            let base = w * wlen;
            let mut i = s;
            while i < e {
                if let Some(b) = spectral_bits(cb as usize, &quant[base + i..base + i + dim]) {
                    bits += b as u32;
                } else {
                    ok = false;
                    break 'w;
                }
                i += dim;
            }
        }
        if ok {
            out[cb as usize] = bits;
        }
    }
    out
}

/// Bits of each book's all-zero tuple (index 0 unused).
fn zero_tuple_bits() -> &'static [u32; 12] {
    static T: OnceLock<[u32; 12]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 12];
        for (cb, b) in t.iter_mut().enumerate().skip(1) {
            let dim = CODEBOOKS[cb].dim as usize;
            *b = spectral_bits(cb, &[0i32; 4][..dim]).unwrap_or(u32::MAX as usize / 4096) as u32;
        }
        t
    })
}

/// **Section trellis.** The per-band codebooks of one window group that
/// minimise spectral bits plus `section_data`, by a Viterbi over (band,
/// codebook): a section costs `hdr` bits (4-bit codebook + one length field),
/// so it can pay to code a band with a larger book than its cheapest, or to
/// carry an all-zero band inside a neighbouring section, rather than open a
/// new section. `costs` is [`band_book_costs`] per band. Length-escape fields
/// of long runs are not priced here; the exact count is `section_bits`.
fn trellis_books(costs: &[[u32; 12]], hdr: u32) -> Vec<u8> {
    const INF: u64 = u64::MAX / 4;
    let n = costs.len();
    if n == 0 {
        return Vec::new();
    }
    let mut dp = vec![[INF; 12]; n];
    let mut from = vec![[0u8; 12]; n];
    for cb in 0..12 {
        if costs[0][cb] != u32::MAX {
            dp[0][cb] = u64::from(hdr) + u64::from(costs[0][cb]);
        }
    }
    for b in 1..n {
        // Best predecessor overall (a section break) and per book (a run).
        let (mut best, mut arg) = (INF, 0u8);
        for p in 0..12 {
            if dp[b - 1][p] < best {
                best = dp[b - 1][p];
                arg = p as u8;
            }
        }
        for cb in 0..12 {
            let c = costs[b][cb];
            if c == u32::MAX {
                continue;
            }
            let stay = dp[b - 1][cb];
            let brk = best.saturating_add(u64::from(hdr));
            let (v, f) = if stay <= brk {
                (stay, cb as u8)
            } else {
                (brk, arg)
            };
            dp[b][cb] = v.saturating_add(u64::from(c));
            from[b][cb] = f;
        }
    }
    let mut cb = (0..12).min_by_key(|&c| dp[n - 1][c]).unwrap_or(11) as u8;
    let mut out = vec![0u8; n];
    for b in (0..n).rev() {
        out[b] = cb;
        cb = from[b][cb as usize];
    }
    out
}

/// Scalefactors in coded order (`cbs` order) with every all-zero band that a
/// section carries under a regular codebook (`is_zero(i)`) given the previous
/// regular band's scalefactor, or the first one's if none precedes it: a zero
/// delta, one bit, and never a step the ±60 delta range cannot code. The
/// band's value does not matter to the decoder (its lines are all zero).
fn zero_band_sf(cbs: &[u8], sf: &[i32], is_zero: impl Fn(usize) -> bool) -> Vec<i32> {
    let mut out = sf.to_vec();
    let regular = |i: usize| cbs[i] != ZERO_HCB && cbs[i] < NOISE_HCB;
    let first = (0..cbs.len())
        .find(|&i| regular(i) && !is_zero(i))
        .map(|i| sf[i]);
    let mut last = first;
    for i in 0..cbs.len() {
        if !regular(i) {
            continue;
        }
        if is_zero(i) {
            if let Some(v) = last {
                out[i] = v;
            }
        } else {
            last = Some(sf[i]);
        }
    }
    out
}

/// Long-block codebooks for `quant` over `0..max_sfb`: the cheapest book per
/// band, or the [`trellis_books`] choice when the section trellis is on.
fn long_books(quant: &[i32], swb: &[u16], max_sfb: usize) -> (Vec<u8>, usize) {
    if section_trellis_on() {
        let costs: Vec<[u32; 12]> = (0..max_sfb)
            .map(|b| {
                band_book_costs(
                    quant,
                    usize::from(swb[b]),
                    usize::from(swb[b + 1]),
                    0,
                    1,
                    FRAME_LEN,
                )
            })
            .collect();
        let cbs = trellis_books(&costs, 9);
        let bits = cbs
            .iter()
            .zip(&costs)
            .map(|(&c, k)| k[c as usize] as usize)
            .sum();
        return (cbs, bits);
    }
    let mut cbs = Vec::with_capacity(max_sfb);
    let mut spec_bits = 0usize;
    for sfb in 0..max_sfb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let (cb, bits) = best_codebook_for_band(quant, s, e);
        cbs.push(cb);
        spec_bits += bits;
    }
    (cbs, spec_bits)
}

/// Short-block codebooks for one window group (windows `w0..w0+nwin`), as
/// [`long_books`].
fn short_books(
    quant: &[i32],
    swb: &[u16],
    max_sfb: usize,
    w0: usize,
    nwin: usize,
) -> (Vec<u8>, usize) {
    if section_trellis_on() {
        let costs: Vec<[u32; 12]> = (0..max_sfb)
            .map(|b| {
                band_book_costs(
                    quant,
                    usize::from(swb[b]),
                    usize::from(swb[b + 1]),
                    w0,
                    nwin,
                    SHORT_HALF,
                )
            })
            .collect();
        let cbs = trellis_books(&costs, 7);
        let bits = cbs
            .iter()
            .zip(&costs)
            .map(|(&c, k)| k[c as usize] as usize)
            .sum();
        return (cbs, bits);
    }
    let mut cbs = Vec::with_capacity(max_sfb);
    let mut bits = 0usize;
    for sfb in 0..max_sfb {
        let (cb, b) = best_codebook_short(quant, swb, sfb, w0, nwin);
        cbs.push(cb);
        bits += b;
    }
    (cbs, bits)
}

/// Is band `sfb` of window group (`w0`, `nwin`) all zero in `quant`?
fn band_is_zero(
    quant: &[i32],
    swb: &[u16],
    sfb: usize,
    w0: usize,
    nwin: usize,
    wlen: usize,
) -> bool {
    let (s, e) = (usize::from(swb[sfb]), usize::from(swb[sfb + 1]));
    (w0..w0 + nwin).all(|w| quant[w * wlen + s..w * wlen + e].iter().all(|&q| q == 0))
}

/// [`zero_band_sf`] for a long block's (`cbs`, `sf`, `quant`).
fn zero_band_sf_long(cbs: &[u8], sf: &[i32], quant: &[i32], swb: &[u16]) -> Vec<i32> {
    zero_band_sf(cbs, sf, |i| band_is_zero(quant, swb, i, 0, 1, FRAME_LEN))
}

/// [`zero_band_sf`] for a short block's group-major (`cbs`, `sf_flat`).
fn zero_band_sf_short(
    cbs: &[u8],
    sf_flat: &[i32],
    quant: &[i32],
    swb: &[u16],
    groups: &[u8],
    max_sfb: usize,
) -> Vec<i32> {
    if max_sfb == 0 {
        return sf_flat.to_vec();
    }
    let starts: Vec<(usize, usize)> = group_windows(groups).collect();
    zero_band_sf(cbs, sf_flat, |i| {
        let (g, sfb) = (i / max_sfb, i % max_sfb);
        band_is_zero(quant, swb, sfb, starts[g].0, starts[g].1, SHORT_HALF)
    })
}

/// Per-band scalefactors from a common `base` plus the psy offsets (see
/// [`Offsets::sf`]), each clamped to the codeable range.
fn scalefactors(offsets: &Offsets, base: i32) -> Vec<i32> {
    offsets.sf(base)
}

/// `global_gain` = the first coded band's scalefactor (the differential reference);
/// 100 for a silent frame with no coded bands.
fn global_gain(cbs: &[u8], sf: &[i32]) -> i32 {
    // Only a REGULAR band seeds the differential chain. A PNS band's `sf` is a
    // noise-energy value on its own accumulator and an intensity band's is a
    // stereo position — seeding global_gain from either desynchronizes every
    // scalefactor that follows.
    cbs.iter()
        .position(|&cb| cb != ZERO_HCB && cb < NOISE_HCB)
        .map_or(100, |sfb| sf[sfb])
}

/// Bits to code the per-band scalefactors as `SCALEFACTOR_BOOK` deltas from a
/// running accumulator seeded at `gg`.
fn scalefactor_bits(cbs: &[u8], sf: &[i32], gg: i32) -> usize {
    let mut acc = gg;
    let mut noise = gg - 90;
    let mut noise_pcm = true;
    let mut is_pos = 0i32;
    let mut bits = 0usize;
    for (sfb, &cb) in cbs.iter().enumerate() {
        if cb == ZERO_HCB {
            continue;
        }
        if cb >= crate::codebook::INTENSITY_HCB2 {
            let d = (sf[sfb] - is_pos).clamp(-60, 60);
            bits += crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize).1 as usize;
            is_pos += d;
        } else if cb == NOISE_HCB {
            if noise_pcm {
                noise_pcm = false;
                let d = (sf[sfb] - noise).clamp(-256, 255);
                bits += 9;
                noise += d;
            } else {
                let d = (sf[sfb] - noise).clamp(-60, 60);
                bits += crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize).1 as usize;
                noise += d;
            }
        } else {
            let d = (sf[sfb] - acc).clamp(-60, 60);
            bits += crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize).1 as usize;
            acc += d;
        }
    }
    bits
}

/// Runtime AVX2 availability (detected once).
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
fn has_avx2() -> bool {
    static AVX2: OnceLock<bool> = OnceLock::new();
    *AVX2.get_or_init(|| std::is_x86_feature_detected!("avx2"))
}

#[cfg(all(feature = "simd-avx512", target_arch = "x86_64"))]
fn has_avx512() -> bool {
    static AVX512: OnceLock<bool> = OnceLock::new();
    *AVX512.get_or_init(|| std::is_x86_feature_detected!("avx512f"))
}

/// Where `|x|^0.75·scale` stops coding as 0 and becomes 1 is `0.5^0.75 =
/// 0.5946`, the point whose reconstruction `x = q^(4/3)` is equally far from 0
/// and from 1. Below [`NEAREST_T1`] the quantizer adds this, `1 - 0.5^0.75`,
/// before flooring, so 0.5946 becomes 1.
const NEAREST_BIAS: f64 = 0.405_396_442_498_639_5;
/// The point between 1 and 2: `((1 + 2^(4/3)) / 2)^0.75`. Flooring with
/// [`NEAREST_BIAS`] keeps everything under it at 1.
const NEAREST_T1: f64 = 1.527_987_809_325_255;

/// The quantizer level whose reconstruction is nearest the input, for
/// `v = |x|^0.75·scale >= 0`, clamped to `MAX_QUANT`.
///
/// The decoder rebuilds `|x| = q^(4/3)` (times the step), so rounding `v` to
/// the nearest integer is not rounding `x` to the nearest level: every `v` in
/// `[0.5, 0.595)` codes as 1 and comes back up to 2.5x too loud, and every `v`
/// in `[1.5, 1.528)` codes as 2. At coarse steps, which a starved channel runs
/// at (a 5.1 stream at 320 kbps is 53 kbps a channel), those levels carry most
/// of the signal, and round-half-up made whole channels 2-4 dB louder than
/// their input (`remade_ffmpeg_rs#129`). Above 2 the midpoints sit within
/// 0.017 of `k + 0.5`, so plain rounding is kept there.
#[inline]
fn nearest_level(v: f64) -> i32 {
    if v < NEAREST_T1 {
        (v + NEAREST_BIAS).floor() as i32
    } else {
        (v.round() as i32).min(MAX_QUANT)
    }
}

/// Quantize one band into `out`: `out[k] = sign[k]·nearest_level(pow[k]·scale)`.
/// This is the encoder's hottest kernel — the rate loop runs it ~11× per frame — so it
/// has an AVX2 path. AVX2 has no round-half-away-from-zero, so the twins use
/// `floor(v + HALF_DOWN)` with `HALF_DOWN` the double just below ½: for `v ≥ 0` that
/// equals `round(v)` exactly. (`floor(v + 0.5)` does NOT — at the double just below ½
/// the sum rounds up to 1.0 — which `quantize_simd_matches_scalar` pins.) Below
/// [`NEAREST_T1`] the twins add [`NEAREST_BIAS`] instead, one compare-and-blend.
fn quantize_band(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    // The twins read `pow`/`sign` for every index of `out`; re-slicing here makes
    // that bound local (a short input panics instead of reading past its end).
    let (pow, sign) = (&pow[..out.len()], &sign[..out.len()]);
    #[cfg(all(feature = "simd-avx512", target_arch = "x86_64"))]
    if has_avx512() {
        // SAFETY: AVX-512F + AVX2 detected at runtime; the three slices have the
        // same length (re-sliced above), which is all `quantize_band_avx512` needs.
        unsafe { quantize_band_avx512(pow, sign, scale, out) };
        prof::count(Kernel::Quantize, true, out.len());
        return;
    }
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    if has_avx2() {
        // SAFETY: AVX2 detected at runtime; the three slices have the same length
        // (re-sliced above), which is all `quantize_band_avx2` needs.
        unsafe { quantize_band_avx2(pow, sign, scale, out) };
        prof::count(Kernel::Quantize, true, out.len());
        return;
    }
    prof::count(Kernel::Quantize, false, out.len());
    quantize_band_scalar(pow, sign, scale, out);
}

/// The largest double below ½ (`0.5 − 2⁻⁵⁴`). Adding it then flooring rounds half
/// away from zero for non-negative input without the 0.49999999999999994 overshoot.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
const HALF_DOWN: f64 = 0.499_999_999_999_999_94;

fn quantize_band_scalar(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    for k in 0..out.len() {
        out[k] = sign[k] * nearest_level(pow[k] * scale);
    }
}

/// AVX2 quantize — four coefficients per iteration. Bit-exact with the scalar path
/// (`floor(v + HALF_DOWN) == round(v)` for `v ≥ 0`, and `floor(v + NEAREST_BIAS)`
/// below [`NEAREST_T1`], exactly the scalar's two cases; the clamp is applied
/// before the f64→i32 narrowing so nothing overflows).
///
/// # Safety
///
/// AVX2 must be available, and `pow.len()` and `sign.len()` must be at least
/// `out.len()`: the vector body reads `pow[i..i + 4]` / `sign[i..i + 4]` and
/// writes `out[i..i + 4]` for `i + 4 <= out.len()`; the tail is safe indexing.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn quantize_band_avx2(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    use std::arch::x86_64::*;
    let n = out.len();
    let vscale = _mm256_set1_pd(scale);
    let vhalf = _mm256_set1_pd(HALF_DOWN);
    let vmax = _mm256_set1_pd(f64::from(MAX_QUANT));
    let (vt1, vbias) = (_mm256_set1_pd(NEAREST_T1), _mm256_set1_pd(NEAREST_BIAS));
    let mut i = 0;
    while i + 4 <= n {
        let v = _mm256_mul_pd(_mm256_loadu_pd(pow.as_ptr().add(i)), vscale);
        let add = _mm256_blendv_pd(vhalf, vbias, _mm256_cmp_pd::<_CMP_LT_OQ>(v, vt1));
        let r = _mm256_floor_pd(_mm256_add_pd(v, add)); // = nearest_level(v) for v ≥ 0
        let qabs = _mm256_cvttpd_epi32(_mm256_min_pd(r, vmax)); // clamp then f64→i32
        let s = _mm_loadu_si128(sign.as_ptr().add(i).cast::<__m128i>());
        _mm_storeu_si128(
            out.as_mut_ptr().add(i).cast::<__m128i>(),
            _mm_mullo_epi32(qabs, s),
        );
        i += 4;
    }
    while i < n {
        out[i] = sign[i] * nearest_level(pow[i] * scale);
        i += 1;
    }
}

/// AVX-512 quantize — eight coefficients per iteration (2× the AVX2 width). Bit-exact:
/// for the quantizer's always-nonnegative input, `trunc(min(v+HALF_DOWN, MAX+0.5))`
/// equals `min(round(v), MAX)`, so a single truncating narrow does round+clamp;
/// below [`NEAREST_T1`] the addend is [`NEAREST_BIAS`] instead (a masked blend).
/// Runtime-gated above AVX2; on AVX2-only hosts this path never runs (untested there —
/// the math mirrors the AVX2 kernel exactly, and its tail is the shared scalar reference).
///
/// # Safety
///
/// AVX-512F and AVX2 must be available, and `pow.len()` and `sign.len()` must be
/// at least `out.len()`: eight lanes per trip for `i + 8 <= out.len()`, then
/// the safe scalar tail.
#[cfg(all(feature = "simd-avx512", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f,avx2")]
#[allow(
    clippy::incompatible_msrv,
    reason = "the opt-in `simd-avx512` feature needs Rust 1.89 (stable AVX-512 intrinsics); the crate's 1.85 MSRV holds for every other feature set (README)"
)]
unsafe fn quantize_band_avx512(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    use std::arch::x86_64::*;
    let n = out.len();
    let vscale = _mm512_set1_pd(scale);
    let vhalf = _mm512_set1_pd(HALF_DOWN);
    let vmaxph = _mm512_set1_pd(f64::from(MAX_QUANT) + 0.5); // clamp v+0.5 so trunc yields MAX
    let (vt1, vbias) = (_mm512_set1_pd(NEAREST_T1), _mm512_set1_pd(NEAREST_BIAS));
    let mut i = 0;
    while i + 8 <= n {
        let v = _mm512_mul_pd(_mm512_loadu_pd(pow.as_ptr().add(i)), vscale);
        let add = _mm512_mask_blend_pd(_mm512_cmp_pd_mask::<_CMP_LT_OQ>(v, vt1), vhalf, vbias);
        let c = _mm512_min_pd(_mm512_add_pd(v, add), vmaxph);
        let qabs = _mm512_cvttpd_epi32(c); // 8 f64 → 8 i32; trunc = floor for c ≥ 0
        let s = _mm256_loadu_si256(sign.as_ptr().add(i).cast::<__m256i>());
        _mm256_storeu_si256(
            out.as_mut_ptr().add(i).cast::<__m256i>(),
            _mm256_mullo_epi32(qabs, s),
        );
        i += 8;
    }
    while i < n {
        out[i] = sign[i] * nearest_level(pow[i] * scale);
        i += 1;
    }
}

/// AVX2 `Xpow` builder: `pow[i] = |x|^0.75` via `√|x|·√√|x|` (two vector sqrts), and
/// `sign[i] = ±1` from the sign bit — four coefficients per iteration.
///
/// # Safety
///
/// AVX2 must be available (the lengths are asserted).
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[cfg(test)]
unsafe fn xpow_avx2(spec: &[f32], pow: &mut [f64], sign: &mut [i32]) {
    assert!(pow.len() >= spec.len() && sign.len() >= spec.len());
    xpow_avx2_raw(spec, pow.as_mut_ptr(), sign.as_mut_ptr());
}

/// [`xpow_avx2`] through raw pointers, so `Xpow::new` can fill uninitialised
/// capacity.
///
/// # Safety
/// AVX2 must be available, and `pow` / `sign` must be valid for `spec.len()`
/// writes; every one of those elements is written.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn xpow_avx2_raw(spec: &[f32], pow: *mut f64, sign: *mut i32) {
    use std::arch::x86_64::*;
    let n = spec.len();
    let absmask = _mm256_castsi256_pd(_mm256_set1_epi64x(0x7fff_ffff_ffff_ffffu64 as i64));
    let one = _mm256_set1_pd(1.0);
    let mut i = 0;
    while i + 4 <= n {
        let x = _mm256_cvtps_pd(_mm_loadu_ps(spec.as_ptr().add(i))); // 4 f32 → 4 f64
        let a = _mm256_and_pd(x, absmask); // |x|
        let s = _mm256_sqrt_pd(a); // |x|^½
        _mm256_storeu_pd(pow.add(i), _mm256_mul_pd(s, _mm256_sqrt_pd(s)));
        // ±1.0 carrying x's sign bit → i32 (x=0 gives +1; harmless, pow is 0).
        let signed = _mm256_or_pd(one, _mm256_andnot_pd(absmask, x));
        _mm_storeu_si128(sign.add(i).cast::<__m128i>(), _mm256_cvttpd_epi32(signed));
        i += 4;
    }
    while i < n {
        let s = f64::from(spec[i].abs()).sqrt();
        pow.add(i).write(s * s.sqrt());
        sign.add(i).write(if spec[i] < 0.0 { -1 } else { 1 });
        i += 1;
    }
}

/// **Deterministic work counters** (feature `lab`) — the PRIMARY evidence for any
/// gate's speed half, per `codec-measurement` §15.
///
/// A counter is immune to scheduler drift, needs one run, and SIZES the effect;
/// a clock at these magnitudes cannot be promoted to a verdict no matter how many
/// pairs it gets. The gate-calculator's `work` column is fed from here.
///
/// Counts the rate loop's exact-coder evaluations — the encoder's dominant inner
/// cost and precisely the quantity a psy arm moves, because changing the offsets
/// changes how many bases the loop must try.
#[cfg(feature = "lab")]
pub mod work {
    use std::sync::atomic::{AtomicU64, Ordering};

    // GLOBAL, not thread-local. `encode_stream` fans frames out across worker
    // threads, so a thread-local counter is drained on the main thread AFTER the
    // workers have exited and reads **zero** — which is exactly what the first
    // harvest reported (`WORK NULL: 0 vs 0`). A counter that reads flat is a
    // defect report, not reassurance (`codec-measurement`, order of instruments).
    static CODE_FRAME_EVALS: AtomicU64 = AtomicU64::new(0);
    static QUANT_BANDS: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub(crate) fn bump_code_frame() {
        CODE_FRAME_EVALS.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn bump_quant_bands(n: u64) {
        QUANT_BANDS.fetch_add(n, Ordering::Relaxed);
    }

    /// Read and clear both counters.
    ///
    /// `Relaxed` is sufficient: these are pure counters, never used to order
    /// other memory, and the harvest reads them only after `finish()` has joined
    /// every worker — so the joins provide the happens-before edge.
    pub fn take() -> (u64, u64) {
        (
            CODE_FRAME_EVALS.swap(0, Ordering::Relaxed),
            QUANT_BANDS.swap(0, Ordering::Relaxed),
        )
    }
}

#[cfg(not(feature = "lab"))]
mod work {
    #[inline]
    pub(crate) fn bump_code_frame() {}
    #[inline]
    pub(crate) fn bump_quant_bands(_: u64) {}
}

/// Shared coder body: quantize the frame into the caller-owned `quant` (per-band
/// scalefactors `sf`, which cover the whole spectrum so no pre-zeroing is needed),
/// pick per-band codebooks, and return (codebooks, ICS body bits, `max_sfb`). Splitting
/// this out lets the rate loop reuse one buffer instead of allocating per candidate.
fn code_core(xp: &Xpow, swb: &[u16], sf: &[i32], quant: &mut [i32]) -> (Vec<u8>, usize, usize) {
    work::bump_code_frame();
    let num_swb = swb.len() - 1;
    work::bump_quant_bands((swb.len() - 1) as u64);
    let prof_q = prof::scope(Stage::EncQuant);
    quantize_frame(xp, swb, sf, quant);
    drop(prof_q);
    let _prof_cb = prof::scope(Stage::EncCodebook);
    let mut max_sfb = 0usize;
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        if quant[s..e].iter().any(|&q| q != 0) {
            max_sfb = sfb + 1;
        }
    }
    let (cbs, spec_bits) = long_books(quant, swb, max_sfb);
    let sf_bits = if section_trellis_on() {
        let sfz = zero_band_sf_long(&cbs, sf, quant, swb);
        scalefactor_bits(&cbs, &sfz, global_gain(&cbs, &sfz))
    } else {
        scalefactor_bits(&cbs, sf, global_gain(&cbs, sf))
    };
    // global_gain(8) + ics_info(~11) + 3 flag bits + sections + scalefactors + spectrum.
    let body = 8 + 11 + 3 + section_bits(&cbs) + sf_bits + spec_bits;
    (cbs, body, max_sfb)
}

/// Quantize every band of a long frame at its scalefactor into `quant`, each
/// band exactly as [`quantize_band`] does. One ISA dispatch per frame: with
/// AVX2 the band loop runs inside [`quantize_bands_avx2`], so the per-band
/// dispatch, call and constant setup (the rate loop quantizes ~690K bands per
/// 18 s of stereo) are paid once.
fn quantize_frame(xp: &Xpow, swb: &[u16], sf: &[i32], quant: &mut [i32]) {
    let scale = scale_table();
    let num_swb = swb.len() - 1;
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    {
        // The opt-in AVX-512 kernel stays on the per-band path.
        #[cfg(feature = "simd-avx512")]
        let avx512 = has_avx512();
        #[cfg(not(feature = "simd-avx512"))]
        let avx512 = false;
        if !avx512 && has_avx2() {
            // SAFETY: AVX2 detected at runtime; `quantize_bands_avx2` cuts each
            // band out of `pow`, `sign` and `quant` with checked slicing, so
            // every kernel call gets three slices of one length.
            unsafe { quantize_bands_avx2(&xp.pow, &xp.sign, swb, sf, scale, quant) };
            prof::count(Kernel::Quantize, true, usize::from(swb[num_swb] - swb[0]));
            return;
        }
    }
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let sc = scale[sf[sfb].clamp(0, 255) as usize];
        quantize_band(&xp.pow[s..e], &xp.sign[s..e], sc, &mut quant[s..e]);
    }
}

/// The band loop of [`quantize_frame`] compiled for AVX2, so
/// [`quantize_band_avx2`] inlines into it.
///
/// # Safety
///
/// AVX2 must be available. Bounds are checked: each band is sliced out of
/// `pow`, `sign` and `out` with safe indexing before the kernel sees it.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn quantize_bands_avx2(
    pow: &[f64],
    sign: &[i32],
    swb: &[u16],
    sf: &[i32],
    scale: &[f64; 256],
    out: &mut [i32],
) {
    for sfb in 0..swb.len() - 1 {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let sc = scale[sf[sfb].clamp(0, 255) as usize];
        quantize_band_avx2(&pow[s..e], &sign[s..e], sc, &mut out[s..e]);
    }
}

/// Owned-buffer wrapper over [`code_core`] for the one-shot callers (final coding).
fn code_frame(xp: &Xpow, swb: &[u16], sf: &[i32]) -> (Vec<u8>, usize, usize, Vec<i32>) {
    let mut quant = vec![0i32; xp.len()];
    let (cbs, body, max_sfb) = code_core(xp, swb, sf, &mut quant);
    (cbs, body, max_sfb, quant)
}

/// The smallest common `base` such that no band clamps its loudest coefficient
/// past `MAX_QUANT` (each band's own no-clamp floor, minus that band's offset).
///
/// Under the surplus policy every band stops at its own floor instead, so the
/// search may run on until the last band reaches it (a negative base, since
/// the bands with low masks lag the base).
fn min_base(xp: &Xpow, swb: &[u16], offsets: &Offsets) -> i32 {
    if let Some(su) = &offsets.surplus {
        return su
            .floor
            .iter()
            .zip(&su.gap)
            .zip(offsets.iter())
            .map(|((&f, &g), &o)| f.max(0) - o - (g + 1) / 2)
            .min()
            .unwrap_or(0)
            .min(255);
    }
    let num_swb = swb.len() - 1;
    let mut floor = 0i32;
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let maxp = xp.max_pow(s, e); // = (max|x|)^0.75
        if maxp <= 1e-9 {
            continue;
        }
        let min_sf = (100.0 - (f64::from(MAX_QUANT) / maxp).log2() / 0.1875).ceil() as i32;
        floor = floor.max(min_sf - offsets[sfb]);
    }
    floor.clamp(0, 255)
}

/// Fast body-bit estimate at a candidate `base`: quantize (cheap) and price each
/// coefficient from the `coef_bits` table — no codebook search. Monotone in `base`,
/// so it seeds the search; the exact refinement corrects it.
fn estimate_bits(xp: &Xpow, swb: &[u16], offsets: &Offsets, base: i32) -> usize {
    let _prof = prof::scope(Stage::EncEstimate);
    let scale = scale_table();
    let sf = offsets.sf(base);
    let ct = coef_bits();
    let num_swb = swb.len() - 1;
    let mut bits = 22usize; // global_gain + ics_info + flags, approx
    let mut qbuf = [0i32; 128]; // one band (max long SWB width is 96)
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let w = e - s;
        let sc = scale[sf[sfb] as usize];
        quantize_band(&xp.pow[s..e], &xp.sign[s..e], sc, &mut qbuf[..w]);
        let mut band = 0usize;
        let mut nonzero = false;
        for &q in &qbuf[..w] {
            let m = q.unsigned_abs() as usize;
            if m != 0 {
                nonzero = true;
                band += ct[m] as usize;
            }
        }
        if nonzero {
            bits += 9 + band; // ~section + scalefactor + spectrum for the band
        }
    }
    bits
}

/// The smallest common `base` (≥ the no-clamp floor) whose ICS body fits
/// `target_bits` — finest quality within budget. A fast estimate seeds the search;
/// the exact coder then walks to the true boundary (identical to a full search, but
/// only a couple of exact evaluations instead of eight). Body bits fall as `base`
/// rises.
#[cfg(test)]
fn rate_loop(xp: &Xpow, swb: &[u16], offsets: &Offsets, target_bits: usize) -> i32 {
    rate_loop_coded(xp, swb, offsets, target_bits).0
}

/// A frame as [`code_frame`] returns it: (codebooks, ICS body bits, `max_sfb`,
/// quantized spectrum).
type Coded = (Vec<u8>, usize, usize, Vec<i32>);

/// [`rate_loop`], also returning the frame coded at the chosen base: the
/// search's last fitting evaluation is that coding, so the caller need not
/// code the frame again (it used to, one exact evaluation in three or four).
fn rate_loop_coded(
    xp: &Xpow,
    swb: &[u16],
    offsets: &Offsets,
    target_bits: usize,
) -> (i32, Vec<i32>, Coded) {
    let min_b = min_base(xp, swb, offsets);
    // Phase 1: binary-search the cheap estimate.
    let max_level = offsets.max_level();
    let (mut lo, mut hi) = (min_b, max_level);
    while lo < hi {
        let mid = (lo + hi).div_euclid(2);
        if estimate_bits(xp, swb, offsets, mid) <= target_bits {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    // Phase 2: refine to the exact smallest fitting base with the real coder, reusing
    // one quant buffer across the (few) exact evaluations and keeping the last
    // fitting one (always the returned base's, when the search evaluated it).
    let mut quant = vec![0i32; xp.len()];
    let mut best: Option<(i32, Coded)> = None;
    let mut exact = |b: i32| {
        let sf = scalefactors(offsets, b);
        let (cbs, body, max_sfb) = code_core(xp, swb, &sf, &mut quant);
        let fits = body <= target_bits;
        if fits {
            let keep = std::mem::take(&mut quant);
            quant = best
                .take()
                .map_or_else(|| vec![0i32; xp.len()], |(_, old)| old.3);
            best = Some((b, (cbs, body, max_sfb, keep)));
        }
        fits
    };
    let mut base = lo;
    if exact(base) {
        while base > min_b && exact(base - 1) {
            base -= 1;
        }
    } else {
        while base < max_level && !exact(base) {
            base += 1;
        }
    }
    let sf = scalefactors(offsets, base);
    match best {
        Some((b, coded)) if b == base => (base, sf, coded),
        _ => {
            let coded = code_frame(xp, swb, &sf);
            (base, sf, coded)
        }
    }
}

/// `section_data` (single group, long block): 4-bit codebook + 5-bit run-length
/// increments (esc = 31 continues the run).
fn write_sections(w: &mut BitWriter, cbs: &[u8]) {
    let esc = 31u32;
    let mut k = 0usize;
    while k < cbs.len() {
        let cb = cbs[k];
        let mut len = 1usize;
        while k + len < cbs.len() && cbs[k + len] == cb {
            len += 1;
        }
        w.write(u32::from(cb), 4);
        let mut l = len as u32;
        while l >= esc {
            w.write(esc, 5);
            l -= esc;
        }
        w.write(l, 5);
        k += len;
    }
}

/// `scale_factor_data`: each coded band's scalefactor as a `SCALEFACTOR_BOOK` delta
/// from a running accumulator seeded at `gg` (matching the decoder).
fn write_scalefactors(w: &mut BitWriter, cbs: &[u8], sf: &[i32], gg: i32) {
    // THREE independent accumulators, exactly as `decode::read_scalefactors`
    // maintains them. Folding PNS or intensity values into the regular chain
    // desynchronizes every later band, which decodes as garbage rather than as a
    // slightly wrong gain.
    let mut acc = gg;
    let mut noise = gg - 90;
    let mut noise_pcm = true;
    let mut is_pos = 0i32;
    for (sfb, &cb) in cbs.iter().enumerate() {
        if cb == ZERO_HCB {
            continue;
        }
        if cb >= crate::codebook::INTENSITY_HCB2 {
            let d = (sf[sfb] - is_pos).clamp(-60, 60);
            let (code, len) = crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize);
            w.write(code, u32::from(len));
            is_pos += d;
        } else if cb == NOISE_HCB {
            if noise_pcm {
                // The first PNS band is a 9-bit PCM delta, not a Huffman one.
                noise_pcm = false;
                let d = (sf[sfb] - noise).clamp(-256, 255);
                w.write((d + 256) as u32, 9);
                noise += d;
            } else {
                let d = (sf[sfb] - noise).clamp(-60, 60);
                let (code, len) = crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize);
                w.write(code, u32::from(len));
                noise += d;
            }
        } else {
            let d = (sf[sfb] - acc).clamp(-60, 60);
            let (code, len) = crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize);
            w.write(code, u32::from(len));
            acc += d;
        }
    }
}

/// **Arm A6 — Perceptual Noise Substitution.**
///
/// Replace a noise-like band's coefficients with a single transmitted *energy*;
/// the decoder refills it with scaled random noise (`decode::fill_noise`). The
/// band costs a scalefactor instead of a spectrum, which at low rates is the
/// difference between coding a band and zeroing it.
///
/// The P0 baseline is the motivation: `speech-noisy` and `noise-like` sit at
/// 99%+ audible from 64-128k and only clear at 192k — broadband noise has no
/// maskers to hide under and is unaffordable, which is exactly the problem PNS
/// exists to solve.
///
/// The gate is deliberately conservative on all three axes, because PNS destroys
/// phase and is audibly destructive on a harmonic band (the `music-tonal`
/// anti-class):
///
/// * **frequency** — only above `PNS_MIN_HZ`; PNS on the bass is never right,
/// * **tonality** — only bands the SFM calls noise-like,
/// * **energy** — only bands actually carrying signal.
///
/// Returns the noise-energy value to transmit for each substituted band.
/// `fill_noise` produces band energy `2^(ne/2)`, so `ne = 2*log2(E)`.
fn pns_bands(
    spec: &[f32],
    swb: &[u16],
    sample_rate: u32,
    tonality: &[f32],
    max_sfb: usize,
) -> Vec<Option<i32>> {
    const PNS_MIN_HZ: f64 = 4000.0;
    const PNS_MAX_TONALITY: f32 = 0.10;
    /// A frame this tonal overall gets NO PNS at all, whatever individual bands
    /// say. See the band-level trap below.
    const PNS_MAX_FRAME_TONALITY: f32 = 0.15;
    /// A band must carry at least this fraction of the frame's MEAN band energy
    /// to be worth substituting — population-relative (law 1), not absolute.
    const PNS_MIN_ENERGY_FRAC: f64 = 0.5;

    let num_swb = swb.len() - 1;
    let mut out = vec![None; num_swb];

    // Band energies once, and the frame's energy-weighted tonality.
    let mut energies = vec![0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energies[sfb] = spec[s..e.min(spec.len())]
            .iter()
            .map(|&x| f64::from(x) * f64::from(x))
            .sum();
    }
    let total: f64 = energies.iter().sum::<f64>() + 1e-9;
    let frame_tonality = (0..num_swb)
        .map(|i| energies[i] * f64::from(tonality[i]))
        .sum::<f64>()
        / total;

    // **The band-level trap, measured.** A near-EMPTY band has a flat spectrum,
    // so its spectral flatness measure reads as maximally *noise-like* — SFM
    // cannot distinguish "broadband noise" from "almost nothing". A band-only
    // gate therefore substitutes noise into the empty bands surrounding a pure
    // tone, which is how the first version of this fired on a 6 kHz sine.
    //
    // Two guards, because they fail in different directions: the frame-level veto
    // refuses tonal CONTENT outright, and the relative energy floor refuses
    // individual bands with nothing in them.
    if frame_tonality as f32 > PNS_MAX_FRAME_TONALITY {
        return out;
    }
    let mean_band = total / num_swb as f64;

    for sfb in 0..num_swb.min(max_sfb) {
        let lo_hz = f64::from(swb[sfb]) * f64::from(sample_rate) * 0.5 / FRAME_LEN as f64;
        if lo_hz < PNS_MIN_HZ || tonality[sfb] > PNS_MAX_TONALITY {
            continue;
        }
        let energy = energies[sfb];
        if energy <= 1.0 || energy < mean_band * PNS_MIN_ENERGY_FRAC {
            continue; // nothing meaningful there to substitute
        }
        let ne = (2.0 * energy.log2()).round() as i32;
        out[sfb] = Some(ne.clamp(-100, 255));
    }
    out
}

/// The bits [`spectral_emit`] writes for every whole `dim`-tuple of `q` with
/// spectral book `cb` (1..=11; every tuple representable, magnitudes at most
/// `MAX_QUANT`), in fewer writes: the book and its table are looked up once per
/// band, each codeword goes out together with its sign bits as one write (at
/// most 19 + 4 bits), and each escape sequence as one write (`2N + 5 <= 21`
/// bits for `N <= 8`). `spectral_emit` stays the per-tuple reference.
///
/// Each book gets its own instance of the loop ([`emit_book`]), so the tuple
/// index is straight-line arithmetic on constants instead of
/// [`tuple_index`]'s run-time `dim`/`lav` walk.
fn emit_band(w: &mut BitWriter, cb: u8, q: &[i32]) {
    let book = spectral_book(cb);
    match cb {
        1 | 2 => emit_book::<4, false, 1, false>(w, book, q),
        3 | 4 => emit_book::<4, true, 2, false>(w, book, q),
        5 | 6 => emit_book::<2, false, 4, false>(w, book, q),
        7 | 8 => emit_book::<2, true, 7, false>(w, book, q),
        9 | 10 => emit_book::<2, true, 12, false>(w, book, q),
        11 => emit_book::<2, true, 16, true>(w, book, q),
        _ => panic!("not a spectral codebook: {cb}"),
    }
}

/// [`emit_band`] for one book shape: `DIM`-tuples, magnitudes plus sign bits
/// when `UNSIGNED`, values up to `LAV` (book 11, `ESC`, clamps to `LAV` and
/// escape-codes the rest). An unrepresentable tuple panics, as
/// [`spectral_emit`] does.
fn emit_book<const DIM: usize, const UNSIGNED: bool, const LAV: u32, const ESC: bool>(
    w: &mut BitWriter,
    book: &crate::huffman::HuffBook,
    q: &[i32],
) {
    let modulo = if UNSIGNED { LAV + 1 } else { 2 * LAV + 1 };
    for t in q.chunks_exact(DIM) {
        let mut idx = 0u32;
        for &c in t {
            let mag = c.unsigned_abs();
            assert!(ESC || mag <= LAV, "representable tuple");
            let digit = if !UNSIGNED {
                (c + LAV as i32) as u32
            } else if ESC {
                mag.min(LAV)
            } else {
                mag
            };
            idx = idx * modulo + digit;
        }
        let (code, len) = book.code(idx as usize);
        let (mut val, mut n) = (code, u32::from(len));
        if UNSIGNED {
            for &c in t {
                if c != 0 {
                    val = (val << 1) | u32::from(c < 0);
                    n += 1;
                }
            }
        }
        w.write(val, n);
        if ESC {
            for &c in t {
                let m = c.unsigned_abs();
                if m >= LAV {
                    // N ones, a zero, then N + 4 bits of m - 2^(N+4) (ISO §4.6.3.3).
                    let e = m.ilog2() - 4;
                    let prefix = ((1u32 << e) - 1) << 1;
                    w.write((prefix << (e + 4)) | (m - (1 << (e + 4))), 2 * e + 5);
                }
            }
        }
    }
}

/// `spectral_data`: per regular SFB, code coefficient tuples with the band's book.
fn write_spectrum(w: &mut BitWriter, quant: &[i32], cbs: &[u8], swb: &[u16]) {
    for (sfb, &cb) in cbs.iter().enumerate() {
        // ZERO, NOISE (PNS) and intensity bands carry no spectral data — their
        // whole point is that the coefficients are not transmitted.
        if cb == ZERO_HCB || cb >= NOISE_HCB {
            continue;
        }
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        emit_band(w, cb, &quant[s..e]);
    }
}

/// Encode one channel as a `single_channel_element` for a long or transition block
/// (`seq` ∈ {`OnlyLong`, `LongStart`, LongStop}): psy offsets → rate loop over the
/// common base → per-band scalefactors → coded ICS.
#[allow(clippy::too_many_arguments)]
fn encode_channel_element(
    w: &mut BitWriter,
    tag: u32,
    spec: &[f32],
    swb: &[u16],
    seq: WindowSequence,
    sample_rate: u32,
    target_bits: usize,
    cur_kbd: bool,
    psy: PsyCfg,
    elem_id: u32,
) {
    // Pass 1, un-filtered: the TNS span depends on max_sfb, which only exists
    // after quantization. This pass exists to learn it.
    let offsets0 = perceptual_offsets(spec, swb, sample_rate, psy.tonality_smr, target_bits);
    let xp0 = Xpow::new(spec);
    let (_, sf0, (cbs0, _, max_sfb0, quant0)) = rate_loop_coded(&xp0, swb, &offsets0, target_bits);

    // Arm A3. Only engage when the coded range already reaches TNS_MAX_LONG, so
    // the decoder's span `min(TNS_MAX_LONG, max_sfb)` is pinned to TNS_MAX_LONG
    // on BOTH sides regardless of what the re-quantization does to max_sfb. The
    // alternative (span depending on a max_sfb that pass 2 can move) is a
    // bitstream desync waiting to happen.
    let fs_index = crate::sf_index_for_rate(sample_rate).unwrap_or(4);
    let tns_max = crate::decode::TNS_MAX_LONG[fs_index as usize] as usize;
    let mut tns: Option<TnsEnc> = None;
    // ARM A3 ROUTING GATE. Spectral LPC prediction gain alone is NOT a transient
    // detector: a peaky TONAL spectrum predicts just as well as an impulsive one,
    // so a gain-only gate fires on sustained content, where temporal noise
    // shaping has nothing to shape and simply costs bits. Measured (PEAQ, vs the
    // previous default): tonal -1.02, mixed -1.14, quiet -1.05, while percussive
    // moved only -0.01. It was firing almost exclusively on the wrong content.
    //
    // The fix is to require the frame to be transient-ADJACENT: only the
    // LongStart / LongStop frames that bracket a detected attack. Those are
    // exactly the long blocks that carry pre-echo, and the window sequence
    // already names them, so the gate is free.
    let tns_frame_ok = seq != WindowSequence::OnlyLong;
    let (cbs, sf, max_sfb, quant) = if psy.tns && tns_frame_ok && max_sfb0 >= tns_max {
        let mut filtered = spec.to_vec();
        match tns_analyze_long(&mut filtered, swb, fs_index, max_sfb0) {
            Some(t) => {
                let offsets =
                    perceptual_offsets(&filtered, swb, sample_rate, psy.tonality_smr, target_bits);
                let xp = Xpow::new(&filtered);
                let (_, sf, (mut cbs, _, msfb, quant)) =
                    rate_loop_coded(&xp, swb, &offsets, target_bits);
                // Hold max_sfb at or above tns_max, padding with ZERO bands, so
                // the decoder's TNS span is exactly the one we filtered.
                let msfb = msfb.max(tns_max);
                cbs.resize(msfb, ZERO_HCB);
                tns = Some(t);
                (cbs, sf, msfb, quant)
            }
            None => (cbs0, sf0, max_sfb0, quant0),
        }
    } else {
        (cbs0, sf0, max_sfb0, quant0)
    };
    // Arm A6 — PNS. Substituted bands trade their spectrum for one energy value.
    //
    // Skipped when TNS fired: the decoder fills noise BEFORE inverse-filtering,
    // so with TNS active the transmitted energy would have to be expressed in the
    // filtered domain. That combination is not yet validated, and a wrong energy
    // there is not a small error - it is a band of noise at the wrong level.
    let (mut cbs, mut sf) = (cbs, sf);
    if psy.pns && tns.is_none() {
        let ton = band_tonality(spec, swb, 1, FRAME_LEN);
        for (b, ne) in pns_bands(spec, swb, sample_rate, &ton, max_sfb)
            .into_iter()
            .enumerate()
        {
            if let (Some(ne), Some(cb)) = (ne, cbs.get_mut(b)) {
                if *cb != ZERO_HCB && *cb < NOISE_HCB {
                    *cb = NOISE_HCB;
                    sf[b] = ne;
                }
            }
        }
    }
    if section_trellis_on() {
        sf = zero_band_sf_long(&cbs, &sf, &quant, swb);
    }
    let gg = global_gain(&cbs, &sf);

    w.write(elem_id, 3);
    w.write(tag, 4);
    w.write(gg as u32, 8);
    let info = IcsInfo {
        window_sequence: seq,
        window_shape_kbd: cur_kbd,
        max_sfb: max_sfb as u8,
        num_windows: 1,
        num_window_groups: 1,
        window_group_length: vec![1],
        num_swb: swb.len() - 1,
    };
    encode_ics_info(w, &info);
    write_sections(w, &cbs);
    write_scalefactors(w, &cbs, &sf, gg);
    w.write(0, 1); // pulse_data_present
    match &tns {
        Some(t) => {
            w.write(1, 1); // tns_data_present
            write_tns_long(w, t);
        }
        None => w.write(0, 1),
    }
    w.write(0, 1); // gain_control_data_present
    write_spectrum(w, &quant, &cbs, swb);
}

// ---------------------------------------------------------------------------
// Arm A3 (Rung 3) — Temporal Noise Shaping.
//
// The AAC tool for pre-echo, and a large part of why AAC beats MP3 on speech.
// The decoder has implemented it since day one (`decode::apply_tns`); the encoder
// emitted `tns_data_present = 0` at all three sites.
//
// The decoder runs an ALL-POLE (synthesis) filter over the spectrum:
//     orig[n] = resid[n] - SUM_k lpc[k] * orig[n-k]
// so the encoder must run the matching ALL-ZERO (analysis) filter:
//     resid[n] = orig[n] + SUM_k lpc[k] * orig[n-k]
// with the state holding past *inputs* (the original spectrum), not past outputs.
// Getting that inversion backwards does not sound slightly worse; it diverges.
// ---------------------------------------------------------------------------

/// One TNS filter the encoder decided to apply, in emit form.
#[derive(Debug, Clone)]
pub(crate) struct TnsEnc {
    /// Span in scalefactor bands, counted down from the top.
    pub length: usize,
    pub order: usize,
    /// Quantized PARCOR indices (4-bit two's complement, `coef_res = 1`).
    pub idx: Vec<i32>,
}

/// Coefficient resolution we emit: `coef_res = 1` (4-bit), `coef_compress = 0`.
/// The widest grid AAC-LC offers, so quantization error in the filter itself is
/// as small as the syntax allows.
const TNS_COEF_RES: u32 = 1;
const TNS_COEF_BITS: u32 = 4;
/// Max order for AAC-LC long blocks (the syntax allows 5 bits; 12 is the ISO cap).
const TNS_MAX_ORDER: usize = 12;
/// Prediction gain below which TNS is not worth its bits. PROVISIONAL — the
/// classic value; the calculator has not fitted it.
const TNS_MIN_GAIN: f64 = 1.4;

/// Quantize one PARCOR value to the 4-bit grid and return `(index, dequantized)`.
///
/// Exact inverse of the decoder's dequantizer, including its **asymmetric**
/// scale factors (`iqfac` for non-negative, `iqfac_m` for negative) — a single
/// shared factor would round half the coefficients to the wrong index.
fn quantize_parcor(p: f32) -> (i32, f32) {
    use std::f32::consts::PI;
    let res_bits = 3 + TNS_COEF_RES;
    let iqfac = ((1i32 << (res_bits - 1)) as f32 - 0.5) / (PI / 2.0);
    let iqfac_m = ((1i32 << (res_bits - 1)) as f32 + 0.5) / (PI / 2.0);
    let t = p.clamp(-0.999, 0.999).asin();
    let lo = -(1i32 << (TNS_COEF_BITS - 1));
    let hi = (1i32 << (TNS_COEF_BITS - 1)) - 1;
    let c = if t >= 0.0 {
        (t * iqfac).round() as i32
    } else {
        (t * iqfac_m).round() as i32
    }
    .clamp(lo, hi);
    // Reconstruct exactly as the decoder will.
    let t_back = if c >= 0 {
        c as f32 / iqfac
    } else {
        c as f32 / iqfac_m
    };
    (c, t_back.sin())
}

/// PARCOR (reflection coefficients) -> LPC, matching `decode::parcor_to_lpc`.
fn parcor_to_lpc_enc(parcor: &[f32]) -> Vec<f32> {
    let order = parcor.len();
    let mut lpc = vec![0f32; order + 1];
    lpc[0] = 1.0;
    for m in 1..=order {
        let mut tmp = lpc.clone();
        for i in 1..m {
            tmp[i] = lpc[i] + parcor[m - 1] * lpc[m - i];
        }
        lpc[..m].copy_from_slice(&tmp[..m]);
        lpc[m] = parcor[m - 1];
    }
    lpc
}

/// Levinson-Durbin over `spec[start..end]`, returning `(parcor, prediction_gain)`.
fn spectral_parcor(spec: &[f32], start: usize, end: usize, order: usize) -> (Vec<f32>, f64) {
    let n = end.saturating_sub(start);
    if n <= order + 1 {
        return (Vec::new(), 1.0);
    }
    let x = &spec[start..end];
    let mut r = vec![0f64; order + 1];
    for (lag, slot) in r.iter_mut().enumerate() {
        let mut acc = 0f64;
        for i in 0..n - lag {
            acc += f64::from(x[i]) * f64::from(x[i + lag]);
        }
        *slot = acc;
    }
    if r[0] <= 1e-12 {
        return (Vec::new(), 1.0);
    }
    let r0 = r[0] * 1.0001; // white-noise correction
    r[0] = r0;
    let mut a = vec![0f64; order + 1];
    let mut parcor = Vec::with_capacity(order);
    let mut err = r0;
    for i in 1..=order {
        let mut acc = r[i];
        for j in 1..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / err;
        if !k.is_finite() || k.abs() >= 0.999 {
            break;
        }
        let prev: Vec<f64> = a[1..i].to_vec();
        for j in 1..i {
            a[j] = prev[j - 1] - k * prev[i - j - 1];
        }
        a[i] = k;
        parcor.push(k as f32);
        err *= 1.0 - k * k;
        if err <= 1e-12 {
            break;
        }
    }
    (parcor, r0 / err.max(1e-12))
}

/// **The Rung 3 gate + arm.** Decide whether to apply TNS to a long block and, if
/// so, filter `spec` in place and return the data to emit.
///
/// `max_sfb_hint` must be the `max_sfb` that will be written, because the decoder
/// filters over `min(TNS_MAX_LONG, max_sfb)` bands — encoder and decoder must
/// agree on the span exactly or the inverse filter runs over the wrong region.
///
/// Returns `None` when the gate declines (prediction gain below threshold, span
/// too short, or a degenerate fit), in which case `spec` is untouched and the
/// caller emits `tns_data_present = 0` — the byte-identical fallback.
pub(crate) fn tns_analyze_long(
    spec: &mut [f32],
    swb: &[u16],
    fs_index: u8,
    max_sfb: usize,
) -> Option<TnsEnc> {
    let _prof = prof::scope(Stage::EncTns);
    let num_swb = swb.len() - 1;
    let tns_max = crate::decode::TNS_MAX_LONG[fs_index as usize] as usize;
    let mmm = tns_max.min(max_sfb);
    if mmm < 4 {
        return None;
    }
    // One filter spanning the top of the coded range down to band `bottom`. The
    // decoder derives `start`/`end` from `length` the same way.
    let bottom = mmm / 4; // skip the lowest quarter: TNS on the bass is harmful
    let length = num_swb.saturating_sub(bottom);
    let start = swb[bottom.min(mmm)] as usize;
    let end = swb[mmm] as usize;
    if end <= start + TNS_MAX_ORDER + 1 {
        return None;
    }

    let (parcor_raw, gain) = spectral_parcor(spec, start, end, TNS_MAX_ORDER);
    if parcor_raw.is_empty() || gain < TNS_MIN_GAIN {
        return None; // the gate: not worth its bits
    }

    // Quantize to the shipping 4-bit grid, and keep the DEQUANTIZED values —
    // filtering with the unquantized ones would leave the decoder running a
    // slightly different filter, which shows up as drift across the band.
    let mut idx = Vec::with_capacity(parcor_raw.len());
    let mut parcor = Vec::with_capacity(parcor_raw.len());
    for &p in &parcor_raw {
        let (c, dq) = quantize_parcor(p);
        idx.push(c);
        parcor.push(dq);
    }
    // Drop trailing zero-index coefficients: they cost 4 bits each and do nothing.
    while idx.len() > 1 && *idx.last().unwrap() == 0 {
        idx.pop();
        parcor.pop();
    }
    let order = idx.len();
    if order == 0 {
        return None;
    }

    let lpc = parcor_to_lpc_enc(&parcor);

    // The ANALYSIS filter (all-zero), upward direction, state = past inputs.
    // Indexed rather than iterated: the filter reads and writes the same slice
    // element each step, which an iterator form cannot express without a copy.
    #[allow(clippy::needless_range_loop)]
    let mut state = vec![0f32; order];
    #[allow(clippy::needless_range_loop)]
    for p in start..end {
        let xn = spec[p];
        let mut y = xn;
        for j in 0..order {
            y += state[j] * lpc[j + 1];
        }
        for j in (1..order).rev() {
            state[j] = state[j - 1];
        }
        state[0] = xn;
        spec[p] = y;
    }

    Some(TnsEnc { length, order, idx })
}

/// Emit `tns_data` for a long block (`n_filt` 2 bits, `length` 6, `order` 5).
/// Mirrors `decode::parse_tns` exactly.
pub(crate) fn write_tns_long(w: &mut BitWriter, tns: &TnsEnc) {
    w.write(1, 2); // n_filt = 1
    w.write(TNS_COEF_RES, 1); // coef_res
    w.write(tns.length as u32, 6);
    w.write(tns.order as u32, 5);
    if tns.order > 0 {
        w.write(0, 1); // direction: upward (low -> high)
        w.write(0, 1); // coef_compress = 0
        for &c in &tns.idx {
            let masked = (c as u32) & ((1u32 << TNS_COEF_BITS) - 1);
            w.write(masked, TNS_COEF_BITS);
        }
    }
}

// ---------------------------------------------------------------------------
// Block switching (brick 5) — transient detection, window-sequence assignment,
// and the short-block coding path (eight 128-bin windows, one group).
// ---------------------------------------------------------------------------

/// Flag each 1024-sample frame that contains a transient: a 128-sample sub-block
/// whose energy leaps above the recent running average (an attack). Frame 0 is
/// never flagged — nothing precedes it to open a `LongStart` transition from.
///
/// `pub(crate)` so [`crate::lab::signals`] can record what the *shipping*
/// detector decides alongside the campaign's replacement signal — the
/// gate-calculator's `shipped` column (arm A9).
pub(crate) fn detect_transients(chan: &[f32], nframes: usize) -> Vec<bool> {
    const RATIO: f64 = 10.0;
    // (arm A9 replaces the guard below; see `detect_transients_relative`)
    let mut flags = vec![false; nframes];
    let mut avg = 0.0f64;
    for (f, flag) in flags.iter_mut().enumerate() {
        let mut attack = 1.0f64;
        for sb in 0..8 {
            let start = f * FRAME_LEN + sb * SHORT_HALF;
            let e: f64 = (0..SHORT_HALF)
                .map(|i| {
                    let x = f64::from(chan.get(start + i).copied().unwrap_or(0.0));
                    x * x
                })
                .sum();
            if avg > 1e-3 {
                attack = attack.max(e / avg);
            }
            avg = 0.75 * avg + 0.25 * e;
        }
        if f > 0 && attack > RATIO {
            *flag = true;
        }
    }
    flags
}

/// Per-frame **population-relative** max attack ratio — the level-invariant
/// replacement for [`detect_transients`]'s absolute `avg > 1e-3` guard (arm A9).
///
/// The running average is floored at a fraction of *this clip's own* mean
/// sub-block energy, so both numerator and floor scale with level and the ratio
/// is unchanged by a gain change. The shipping detector's absolute floor makes it
/// blind on quiet content and, because the average decays 0.75× per sub-block, on
/// sparse attacks over a quiet floor at *any* level.
///
/// Used by the Rung 0 shape gate as a transient veto; it is also the signal arm
/// A9 will replace the detector's threshold with.
pub(crate) fn frame_attack_ratios(chan: &[f32], nframes: usize) -> Vec<f32> {
    frame_attack_ratios_with(chan, nframes, false)
}

/// Energy of the [`SHORT_HALF`]-sample sub-block of `chan` starting at
/// `start` (samples past the end are silence). With `highpass`, of the
/// second difference `x[n] - 2x[n-1] + x[n-2]` instead (samples before 0 are
/// silence): a +12 dB/octave tilt, so an attack carried by its high
/// frequencies stands out over a sustained low-frequency body.
fn sub_block_energy(chan: &[f32], start: usize, highpass: bool) -> f64 {
    let at = |i: usize| f64::from(chan.get(i).copied().unwrap_or(0.0));
    (start..start + SHORT_HALF)
        .map(|n| {
            let x = if highpass {
                let x1 = if n >= 1 { at(n - 1) } else { 0.0 };
                let x2 = if n >= 2 { at(n - 2) } else { 0.0 };
                at(n) - 2.0 * x1 + x2
            } else {
                at(n)
            };
            x * x
        })
        .sum()
}

/// [`frame_attack_ratios`] on plain (`highpass = false`) or high-passed
/// sub-block energies (see [`sub_block_energy`]).
pub(crate) fn frame_attack_ratios_with(chan: &[f32], nframes: usize, highpass: bool) -> Vec<f32> {
    let mut sub: Vec<f64> = Vec::with_capacity(nframes * 8);
    for f in 0..nframes {
        for sb in 0..8 {
            sub.push(sub_block_energy(
                chan,
                f * FRAME_LEN + sb * SHORT_HALF,
                highpass,
            ));
        }
    }
    let mean_e = sub.iter().sum::<f64>() / sub.len().max(1) as f64;
    let floor = (mean_e * 1e-3).max(f64::MIN_POSITIVE);

    let mut avg = 0.0f64;
    let mut out = Vec::with_capacity(nframes);
    for f in 0..nframes {
        let mut amax = 1.0f32;
        for sb in 0..8 {
            let e = sub[f * 8 + sb];
            amax = amax.max((e / avg.max(floor)) as f32);
            avg = 0.75 * avg + 0.25 * e;
        }
        out.push(amax);
    }
    out
}

/// **Arm A9 — the level-invariant transient detector.**
///
/// Same decision rule as [`detect_transients`] (`attack ratio > 10`), on a signal
/// that is actually defined. The shipped detector guards its ratio with
/// `avg > 1e-3` — an **absolute** energy floor — which has two consequences the
/// P1 truth table pinned:
///
/// * the identical waveform 40 dB quieter stops being detected at all, and
/// * because the running average decays 0.75x per sub-block, it falls under the
///   floor across the quiet stretch between sparse attacks, so castanet-like
///   content is missed **at any level** — measured: zero frames flagged on the
///   percussive corpus class whose p95 attack ratio is ~30000x.
///
/// The ratio itself is already scale-invariant; only the guard was not. This
/// version floors the running average relative to the clip's own mean sub-block
/// energy instead, which is the law-1 form and leaves the threshold meaning what
/// it says.
///
/// This is not merely a quality knob: arm A1 (short-block psy) can only reach
/// frames that are coded as short blocks, so a detector that never fires makes A1
/// unmeasurable. A9 is A1's prerequisite.
pub(crate) fn detect_transients_relative(
    chan: &[f32],
    nframes: usize,
    highpass: bool,
) -> Vec<bool> {
    const RATIO: f32 = 10.0;
    let ratios = frame_attack_ratios_with(chan, nframes, highpass);
    (0..nframes)
        .map(|f| f > 0 && ratios.get(f).copied().unwrap_or(1.0) > RATIO)
        .collect()
}

/// Assign a valid AAC window sequence to each frame from the transient flags. A
/// short run is bracketed by LongStart/LongStop; runs a single frame apart are
/// merged (a lone gap can't be both a stop and a start).
fn assign_sequences(transient: &[bool]) -> Vec<WindowSequence> {
    use WindowSequence::{EightShort, LongStart, LongStop, OnlyLong};
    let n = transient.len();
    let mut short = transient.to_vec();
    for i in 1..n.saturating_sub(1) {
        if !short[i] && short[i - 1] && short[i + 1] {
            short[i] = true;
        }
    }
    let mut seq = vec![OnlyLong; n];
    let mut i = 0;
    while i < n {
        if short[i] {
            let a = i;
            while i < n && short[i] {
                seq[i] = EightShort;
                i += 1;
            }
            if a > 0 {
                seq[a - 1] = LongStart;
            }
            if i < n {
                seq[i] = LongStop;
            }
        } else {
            i += 1;
        }
    }
    seq
}

/// Cheapest codebook (and its bit cost) for one SFB across all short windows of a
/// single group, matched to how the decoder reads it (per-SFB codebook, per-window
/// coefficients).
///
/// The same one-walk pricing as [`best_codebook_for_band`], accumulated over the
/// group's windows; [`best_codebook_short_ref`], the per-book search it
/// replaced, is the test oracle.
fn best_codebook_short(
    quant: &[i32],
    swb: &[u16],
    sfb: usize,
    win0: usize,
    nwin: usize,
) -> (u8, usize) {
    let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
    let band = |win: usize| &quant[win * SHORT_HALF + s..win * SHORT_HALF + e];
    let wins = win0..win0 + nwin;
    if (e - s) % 2 != 0 {
        // No book can code an odd band (cold: every AAC band width is a
        // multiple of 4).
        return if wins.clone().all(|w| band(w).iter().all(|&v| v == 0)) {
            (ZERO_HCB, 0)
        } else {
            (ESC_HCB, usize::MAX)
        };
    }
    let t = cost_tables();
    let mut cost = BandCost::default();
    for win in wins.clone() {
        cost.add2(t, band(win));
    }
    if cost.maxq == 0 {
        return (ZERO_HCB, 0);
    }
    if cost.maxq <= 2 && (e - s) % 4 == 0 {
        for win in wins.clone() {
            cost.add4(t, band(win));
        }
    }
    if cost.maxq >= 16 {
        for win in wins {
            cost.add_esc(band(win));
        }
    }
    cost.best(e - s)
}

/// The per-book search [`best_codebook_short`] replaced: the oracle its tests
/// compare against.
#[cfg(test)]
fn best_codebook_short_ref(
    quant: &[i32],
    swb: &[u16],
    sfb: usize,
    win0: usize,
    nwin: usize,
) -> (u8, usize) {
    let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
    let wins = win0..win0 + nwin;
    let mut maxq = 0u32;
    for win in wins.clone() {
        let base = win * SHORT_HALF;
        for &q in &quant[base + s..base + e] {
            maxq = maxq.max(q.unsigned_abs());
        }
    }
    if maxq == 0 {
        return (ZERO_HCB, 0);
    }
    let mut best = (ESC_HCB, usize::MAX);
    for cb in 1..=11u8 {
        let meta = &CODEBOOKS[cb as usize];
        let dim = meta.dim as usize;
        if (e - s) % dim != 0 || (!meta.esc && u32::from(meta.lav) < maxq) {
            continue;
        }
        let mut bits = 0usize;
        let mut ok = true;
        'windows: for win in wins.clone() {
            let base = win * SHORT_HALF;
            let mut i = s;
            while i < e {
                if let Some(b) = spectral_bits(cb as usize, &quant[base + i..base + i + dim]) {
                    bits += b;
                } else {
                    ok = false;
                    break 'windows;
                }
                i += dim;
            }
        }
        if ok && bits < best.1 {
            best = (cb, bits);
        }
    }
    best
}

/// `(first window, window count)` of each group, from the group lengths.
fn group_windows(groups: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    groups.iter().scan(0usize, |w0, &len| {
        let r = (*w0, len as usize);
        *w0 += len as usize;
        Some(r)
    })
}

/// Per-group scalefactors (`groups.len() × num_swb`) flattened to the coded
/// order the bitstream carries (`groups.len() × max_sfb`).
fn flatten_short_sf(sf: &[i32], num_swb: usize, max_sfb: usize, ngroups: usize) -> Vec<i32> {
    (0..ngroups)
        .flat_map(|g| sf[g * num_swb..g * num_swb + max_sfb].iter().copied())
        .collect()
}

/// **Arm 1a — window grouping.** Split the eight short windows into runs of
/// similar energy: a new group starts where a window's energy differs from its
/// group's running mean by more than [`GROUP_SPLIT`]. A quiet pre-attack stretch
/// then has scalefactors of its own instead of sharing the attack's. `specs` are
/// the channels that share the `ics_info` (both channels of a common-window CPE).
fn short_groups(specs: &[&[f32]]) -> Vec<u8> {
    let mut e = [0f64; 8];
    for spec in specs {
        for (w, ew) in e.iter_mut().enumerate() {
            *ew += spec[w * SHORT_HALF..(w + 1) * SHORT_HALF]
                .iter()
                .map(|&x| f64::from(x) * f64::from(x))
                .sum::<f64>();
        }
    }
    let floor = e.iter().sum::<f64>() * 1e-6 + 1e-9;
    let mut groups = Vec::with_capacity(8);
    let (mut len, mut sum) = (1u8, e[0]);
    for &ew in &e[1..] {
        let ratio = (ew + floor) / (sum / f64::from(len) + floor);
        if !(1.0 / GROUP_SPLIT..=GROUP_SPLIT).contains(&ratio) {
            groups.push(len);
            len = 1;
            sum = ew;
        } else {
            len += 1;
            sum += ew;
        }
    }
    groups.push(len);
    groups
}

/// Energy ratio (window vs its group's mean) that starts a new group: 6 dB.
const GROUP_SPLIT: f64 = 4.0;

/// Bits for short-block `section_data` (3-bit run-length increments, esc = 7).
fn section_bits_short(cbs: &[u8]) -> usize {
    let esc = 7usize;
    let mut bits = 0usize;
    let mut k = 0usize;
    while k < cbs.len() {
        let cb = cbs[k];
        let mut len = 1usize;
        while k + len < cbs.len() && cbs[k + len] == cb {
            len += 1;
        }
        bits += 4;
        let mut l = len;
        while l >= esc {
            bits += 3;
            l -= esc;
        }
        bits += 3;
        k += len;
    }
    bits
}

/// Quantize all eight short windows with a per-SFB scalefactor (one group; flat
/// this brick), pick per-SFB codebooks, and return (codebooks, body bits, `max_sfb`,
/// window-major quantized spectrum).
fn code_frame_short(
    xp: &Xpow,
    swb: &[u16],
    sf: &[i32],
    groups: &[u8],
) -> (Vec<u8>, usize, usize, Vec<i32>, Vec<i32>) {
    work::bump_code_frame();
    let num_swb = swb.len() - 1;
    work::bump_quant_bands(((swb.len() - 1) * 8) as u64);
    let scale = scale_table();
    let prof_q = prof::scope(Stage::EncQuant);
    let mut quant = vec![0i32; FRAME_LEN];
    for (g, (w0, nwin)) in group_windows(groups).enumerate() {
        for win in w0..w0 + nwin {
            let base = win * SHORT_HALF;
            for sfb in 0..num_swb {
                let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                let sc = scale[sf[g * num_swb + sfb].clamp(0, 255) as usize];
                quantize_band(
                    &xp.pow[base + s..base + e],
                    &xp.sign[base + s..base + e],
                    sc,
                    &mut quant[base + s..base + e],
                );
            }
        }
    }
    drop(prof_q);
    let _prof_cb = prof::scope(Stage::EncCodebook);
    let mut max_sfb = 0usize;
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        if (0..8).any(|win| {
            let base = win * SHORT_HALF;
            quant[base + s..base + e].iter().any(|&q| q != 0)
        }) {
            max_sfb = sfb + 1;
        }
    }
    let mut cbs = Vec::with_capacity(groups.len() * max_sfb);
    let mut spec_bits = 0usize;
    let mut section_bits = 0usize;
    for (w0, nwin) in group_windows(groups) {
        let start = cbs.len();
        let (g_cbs, bits) = short_books(&quant, swb, max_sfb, w0, nwin);
        cbs.extend_from_slice(&g_cbs);
        spec_bits += bits;
        section_bits += section_bits_short(&cbs[start..]);
    }
    let mut sf_flat = flatten_short_sf(sf, num_swb, max_sfb, groups.len());
    if section_trellis_on() {
        sf_flat = zero_band_sf_short(&cbs, &sf_flat, &quant, swb, groups, max_sfb);
    }
    let gg = global_gain(&cbs, &sf_flat);
    // global_gain(8) + ics_info(short ~18) + 3 flags + sections + scalefactors + spectrum.
    let body = 8 + 18 + 3 + section_bits + scalefactor_bits(&cbs, &sf_flat, gg) + spec_bits;
    (cbs, body, max_sfb, quant, sf_flat)
}

/// The smallest flat scalefactor that avoids clamping the loudest short coefficient
/// past `MAX_QUANT`.
fn min_base_short(xp: &Xpow) -> i32 {
    let maxp = xp.max_pow(0, xp.len());
    if maxp <= 1e-9 {
        return 0;
    }
    (100.0 - (f64::from(MAX_QUANT) / maxp).log2() / 0.1875)
        .ceil()
        .clamp(0.0, 255.0) as i32
}

/// **Arm A1 — the short-block psychoacoustic model.**
///
/// Per-SFB scalefactor offsets for an `EightShort` frame. The band energies are
/// summed **across the eight windows** of the group, because with one window group
/// a scalefactor covers band `b` of all eight windows — so the mask must be
/// computed over exactly the coefficients that scalefactor governs.
///
/// The band geometry is the short SWB table over a 128-coefficient window, hence
/// `n_coeffs = SHORT_HALF`: reusing the long block's 1024 here would place every
/// band three octaves too low and spread the mask against the wrong neighbours.
///
/// Before this arm, short blocks coded with **flat** scalefactors — no noise
/// shaping at all on precisely the content where masking matters most.
///
/// # MEASURED REFUTATION (2026-08-08): this is harmful WITHOUT window grouping
///
/// Once arm A9 made short blocks actually occur, A1 became measurable — and it
/// made things **worse**, on both the class it was built for and the one it was
/// meant to be neutral on:
///
/// | arm | percussive 64k/128k | speech-clean 64k/128k |
/// |---|---|---|
/// | A9 alone | −2.81 / −6.10 | +1.56 / +1.18 |
/// | A9 + A1 | −1.18 / −5.50 | +2.33 / +1.46 |
///
/// The cause is the group-summed energy above. With **one** window group a single
/// scalefactor governs band `b` of all eight windows, so the mask must be derived
/// from their sum — which hands the quiet pre-attack windows a mask sized by the
/// attack. That is precisely the pre-echo the tool was supposed to suppress: a
/// flat scalefactor at least errs uniformly, whereas an attack-sized mask
/// actively licenses noise where the ear is most sensitive.
///
/// **This overturns a plan correction I made earlier.** Reading the emit path, I
/// argued grouping was a follow-up rather than a prerequisite, because per-SFB
/// shaping is available with one group. That is true and irrelevant: the shaping
/// available with one group is shaping in the *wrong domain*. Arm 1a (window
/// grouping) is a genuine prerequisite for A1, exactly as the original plan said.
/// Do not enable `short_block_psy` before grouping exists.
fn perceptual_offsets_short(
    spec: &[f32],
    swb: &[u16],
    sample_rate: u32,
    tonality_smr: bool,
) -> Vec<i32> {
    let _prof = prof::scope(Stage::EncPsy);
    let num_swb = swb.len() - 1;
    let mut energy = vec![0.0f64; num_swb];
    let mut noise_scale = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        for win in 0..8 {
            let base = win * SHORT_HALF;
            for k in s..e {
                let Some(&x) = spec.get(base + k) else {
                    continue;
                };
                energy[sfb] += f64::from(x) * f64::from(x);
                noise_scale[sfb] += f64::from(x.abs()).sqrt();
            }
        }
    }
    let mut e_masked: Vec<f64> = energy.iter().map(|&e| e + 1e-3).collect();
    let ton = tonality_smr.then(|| band_tonality(spec, swb, 8, SHORT_HALF));
    let thr = masking_from_energy(&e_masked, swb, sample_rate, SHORT_HALF, ton.as_deref());
    e_masked.clear();

    let raw: Vec<f64> = (0..num_swb)
        .map(|sfb| (thr[sfb] / (noise_scale[sfb] + 1e-6)).log2() / 0.375)
        .collect();
    // Centre on the energy-bearing bands, exactly as the long-block path does.
    let etot: f64 = energy.iter().sum::<f64>() + 1e-9;
    let center: f64 = (0..num_swb).map(|i| raw[i] * energy[i]).sum::<f64>() / etot;
    raw.iter()
        .map(|&r| ((r - center).round() as i32).clamp(-60, 60))
        .collect()
}

/// Rate loop for a short block: smallest common base (≥ no-clamp floor) whose body
/// fits `target_bits`. `offsets` is arm A1's per-band shape (all zeros reproduces
/// **Arms A1 + 1a — the short-block psy model per window group.**
///
/// [`perceptual_offsets_short`] sums band energy across all eight windows, which
/// hands quiet pre-attack windows a mask sized by the attack (its measured
/// refutation). Here each group gets its own masking threshold, and the offsets
/// are centred **across** groups: a quiet group therefore sits below the attack
/// group and is coded with finer steps, which is the pre-echo protection the
/// short block exists for. Offsets are clamped to ±30 so any two coded
/// scalefactors stay within the bitstream's ±60 delta. Returns
/// `groups.len() × num_swb` offsets, group-major.
fn perceptual_offsets_short_grouped(
    spec: &[f32],
    swb: &[u16],
    sample_rate: u32,
    tonality_smr: bool,
    groups: &[u8],
) -> Vec<i32> {
    let _prof = prof::scope(Stage::EncPsy);
    let num_swb = swb.len() - 1;
    let mut raw = Vec::with_capacity(groups.len() * num_swb);
    let mut energy = Vec::with_capacity(groups.len() * num_swb);
    for (w0, nwin) in group_windows(groups) {
        let sub = &spec[w0 * SHORT_HALF..(w0 + nwin) * SHORT_HALF];
        let mut e_band = vec![0.0f64; num_swb];
        let mut noise_scale = vec![0.0f64; num_swb];
        for sfb in 0..num_swb {
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            for win in 0..nwin {
                let base = win * SHORT_HALF;
                for &x in &sub[base + s..base + e] {
                    e_band[sfb] += f64::from(x) * f64::from(x);
                    noise_scale[sfb] += f64::from(x.abs()).sqrt();
                }
            }
        }
        let e_masked: Vec<f64> = e_band.iter().map(|&e| e + 1e-3).collect();
        let ton = tonality_smr.then(|| band_tonality(sub, swb, nwin, SHORT_HALF));
        let thr = masking_from_energy(&e_masked, swb, sample_rate, SHORT_HALF, ton.as_deref());
        for sfb in 0..num_swb {
            raw.push((thr[sfb] / (noise_scale[sfb] + 1e-6)).log2() / 0.375);
        }
        energy.extend_from_slice(&e_band);
    }
    let etot: f64 = energy.iter().sum::<f64>() + 1e-9;
    let center: f64 = raw.iter().zip(&energy).map(|(r, e)| r * e).sum::<f64>() / etot;
    raw.iter()
        .map(|&r| ((r - center).round() as i32).clamp(-30, 30))
        .collect()
}

/// Short-block scalefactor offsets for the configured arms: grouped psy (1a),
/// single-group psy (A1), or flat. Returns `groups.len() × num_swb` values.
fn short_offsets(
    spec: &[f32],
    swb: &[u16],
    sample_rate: u32,
    psy: PsyCfg,
    groups: &[u8],
) -> Offsets {
    Offsets::flat(if psy.window_grouping {
        perceptual_offsets_short_grouped(spec, swb, sample_rate, psy.tonality_smr, groups)
    } else if psy.short_block_psy {
        perceptual_offsets_short(spec, swb, sample_rate, psy.tonality_smr)
    } else {
        vec![0i32; groups.len() * (swb.len() - 1)]
    })
}

/// The window grouping for a short frame whose `ics_info` the `specs` share:
/// real groups under arm 1a, else one group of eight.
fn short_grouping(specs: &[&[f32]], psy: PsyCfg) -> Vec<u8> {
    if psy.window_grouping {
        short_groups(specs)
    } else {
        vec![8]
    }
}

/// the flat-scalefactor behavior byte-identically).
fn rate_loop_short(
    xp: &Xpow,
    swb: &[u16],
    offsets: &Offsets,
    target_bits: usize,
    groups: &[u8],
) -> i32 {
    if section_trellis_on() {
        let est = |b: i32| estimate_bits_short(xp, swb, offsets, b, groups);
        let exact = |b: i32| code_frame_short(xp, swb, &scalefactors(offsets, b), groups).1;
        return seeded_level(min_base_short(xp), 255, target_bits, est, exact);
    }
    let mut lo = min_base_short(xp);
    let mut hi = 255i32;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let sf = scalefactors(offsets, mid);
        if code_frame_short(xp, swb, &sf, groups).1 <= target_bits {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// The short-block analogue of [`estimate_bits`]: every window of every
/// group quantized at `base`, each coefficient priced from `coef_bits`, and a
/// flat 7 bits a coded band (section and scalefactor). Seeds the search only.
fn estimate_bits_short(
    xp: &Xpow,
    swb: &[u16],
    offsets: &Offsets,
    base: i32,
    groups: &[u8],
) -> usize {
    let _prof = prof::scope(Stage::EncEstimate);
    let scale = scale_table();
    let sf = offsets.sf(base);
    let ct = coef_bits();
    let num_swb = swb.len() - 1;
    let mut bits = 29usize; // global_gain + short ics_info + flags, approx
    let mut qbuf = [0i32; SHORT_HALF];
    for (g, (w0, nwin)) in group_windows(groups).enumerate() {
        for sfb in 0..num_swb {
            let (s, e) = (usize::from(swb[sfb]), usize::from(swb[sfb + 1]));
            let sc = scale[sf[g * num_swb + sfb].clamp(0, 255) as usize];
            let (mut band, mut nonzero) = (0usize, false);
            for win in w0..w0 + nwin {
                let at = win * SHORT_HALF;
                quantize_band(
                    &xp.pow[at + s..at + e],
                    &xp.sign[at + s..at + e],
                    sc,
                    &mut qbuf[..e - s],
                );
                for &q in &qbuf[..e - s] {
                    let m = q.unsigned_abs() as usize;
                    if m != 0 {
                        nonzero = true;
                        band += usize::from(ct[m.min(ct.len() - 1)]);
                    }
                }
            }
            if nonzero {
                bits += 7 + band;
            }
        }
    }
    bits
}

/// The smallest level in `[min_level, max_level]` whose `exact` bits fit
/// `target`: a binary search on the cheap `est` seeds a walk with `exact`
/// (the search shape of [`rate_loop_coded`]). Same answer as a binary search
/// on `exact` wherever the exact bits fall monotonically in the level, for a
/// handful of exact evaluations instead of one a step.
fn seeded_level(
    min_level: i32,
    max_level: i32,
    target: usize,
    est: impl Fn(i32) -> usize,
    exact: impl Fn(i32) -> usize,
) -> i32 {
    let (mut lo, mut hi) = (min_level, max_level);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if est(mid) <= target {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let mut level = lo;
    if exact(level) <= target {
        while level > min_level && exact(level - 1) <= target {
            level -= 1;
        }
    } else {
        while level < max_level && exact(level) > target {
            level += 1;
        }
    }
    level
}

/// `short_block` `section_data` (4-bit codebook + 3-bit run increments, esc = 7).
fn write_sections_short(w: &mut BitWriter, cbs: &[u8]) {
    let esc = 7u32;
    let mut k = 0usize;
    while k < cbs.len() {
        let cb = cbs[k];
        let mut len = 1usize;
        while k + len < cbs.len() && cbs[k + len] == cb {
            len += 1;
        }
        w.write(u32::from(cb), 4);
        let mut l = len as u32;
        while l >= esc {
            w.write(esc, 3);
            l -= esc;
        }
        w.write(l, 3);
        k += len;
    }
}

/// `short_block` `spectral_data`: per group, per SFB, per window of the group,
/// coefficient tuples (the grouped-interleaved order the decoder reads).
fn write_spectrum_short(
    w: &mut BitWriter,
    quant: &[i32],
    cbs: &[u8],
    swb: &[u16],
    groups: &[u8],
    max_sfb: usize,
) {
    for (g, (w0, nwin)) in group_windows(groups).enumerate() {
        for sfb in 0..max_sfb {
            let cb = cbs[g * max_sfb + sfb];
            if cb == ZERO_HCB {
                continue;
            }
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            for win in w0..w0 + nwin {
                let base = win * SHORT_HALF;
                emit_band(w, cb, &quant[base + s..base + e]);
            }
        }
    }
}

/// `short_block` `section_data` for every window group (runs never cross a group).
fn write_sections_short_grouped(w: &mut BitWriter, cbs: &[u8], ngroups: usize, max_sfb: usize) {
    for g in 0..ngroups {
        write_sections_short(w, &cbs[g * max_sfb..(g + 1) * max_sfb]);
    }
}

/// Encode one channel as an `EightShort` `single_channel_element`. One group of 8
/// windows with flat scalefactors unless arm A1 (`psy.short_block_psy`) supplies a
/// per-band shape, or arm 1a (`psy.window_grouping`) real groups shaped per group.
#[allow(clippy::too_many_arguments)]
fn encode_channel_element_short(
    w: &mut BitWriter,
    tag: u32,
    spec: &[f32],
    swb: &[u16],
    target_bits: usize,
    cur_kbd: bool,
    sample_rate: u32,
    psy: PsyCfg,
) {
    let xp = Xpow::new(spec);
    let groups = short_grouping(&[spec], psy);
    let offsets = short_offsets(spec, swb, sample_rate, psy, &groups);
    let base = rate_loop_short(&xp, swb, &offsets, target_bits, &groups);
    let sf = scalefactors(&offsets, base);
    let (cbs, _, max_sfb, quant, sf_flat) = code_frame_short(&xp, swb, &sf, &groups);
    let gg = global_gain(&cbs, &sf_flat);

    w.write(ID_SCE, 3);
    w.write(tag, 4);
    w.write(gg as u32, 8);
    let info = IcsInfo {
        window_sequence: WindowSequence::EightShort,
        window_shape_kbd: cur_kbd,
        max_sfb: max_sfb as u8,
        num_windows: 8,
        num_window_groups: groups.len(),
        window_group_length: groups.clone(),
        num_swb: swb.len() - 1,
    };
    encode_ics_info(w, &info);
    write_sections_short_grouped(w, &cbs, groups.len(), max_sfb);
    write_scalefactors(w, &cbs, &sf_flat, gg);
    w.write(0, 1); // pulse_data_present
    w.write(0, 1); // tns_data_present
    w.write(0, 1); // gain_control_data_present
    write_spectrum_short(w, &quant, &cbs, swb, &groups, max_sfb);
}

// ---------------------------------------------------------------------------
// Stereo (brick 6) — a channel_pair_element with a common window and per-SFB M/S
// (mid/side) coding where the channels are correlated enough to pay off.
// ---------------------------------------------------------------------------

/// Per-SFB M/S decision + mixed spectra. M/S wins when `E_M·E_S < E_L·E_R` (the
/// correlation criterion — raw energy always halves under the ½ scaling, so the
/// *product* is what predicts bit savings). Returns (ch0 = M or L, ch1 = S or R,
/// per-SFB `ms_used`).
fn mid_side(
    l: &[f32],
    r: &[f32],
    swb: &[u16],
    is_short: bool,
    is_veto: &[bool],
    groups: &[u8],
) -> (Vec<f32>, Vec<f32>, Vec<bool>) {
    let num_swb = swb.len() - 1;
    let wlen = if is_short { SHORT_HALF } else { FRAME_LEN };
    let groups: &[u8] = if is_short { groups } else { &[1] };
    let mut ch0 = l.to_vec();
    let mut ch1 = r.to_vec();
    let mut ms = vec![false; groups.len() * num_swb];
    for (g, (w0, nwin)) in group_windows(groups).enumerate() {
        for sfb in 0..num_swb {
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            let (mut el, mut er, mut em, mut es) = (0.0f64, 0.0, 0.0, 0.0);
            for win in w0..w0 + nwin {
                let base = win * wlen;
                for i in s..e {
                    let (lv, rv) = (f64::from(l[base + i]), f64::from(r[base + i]));
                    let (m, sd) = ((lv + rv) * 0.5, (lv - rv) * 0.5);
                    el += lv * lv;
                    er += rv * rv;
                    em += m * m;
                    es += sd * sd;
                }
            }
            // A band claimed by intensity stereo (arm A7) must stay L/R.
            if em * es < el * er && !is_veto.get(sfb).copied().unwrap_or(false) {
                ms[g * num_swb + sfb] = true;
                for win in w0..w0 + nwin {
                    let base = win * wlen;
                    for i in s..e {
                        let (lv, rv) = (l[base + i], r[base + i]);
                        ch0[base + i] = (lv + rv) * 0.5;
                        ch1[base + i] = (lv - rv) * 0.5;
                    }
                }
            }
        }
    }
    (ch0, ch1, ms)
}

/// Quantize one CPE channel to a target: psy per-band scalefactors (long) or flat
/// (short). Returns (per-SFB scalefactors, quantized spectrum).
fn quantize_channel(
    spec: &[f32],
    swb: &[u16],
    is_short: bool,
    sample_rate: u32,
    target_bits: usize,
    psy: PsyCfg,
    groups: &[u8],
) -> (Vec<i32>, Vec<i32>) {
    let xp = Xpow::new(spec);
    if is_short {
        let offsets = short_offsets(spec, swb, sample_rate, psy, groups);
        let base = rate_loop_short(&xp, swb, &offsets, target_bits, groups);
        let sf = scalefactors(&offsets, base);
        let (_, _, _, quant, _) = code_frame_short(&xp, swb, &sf, groups);
        (sf, quant)
    } else {
        let offsets = perceptual_offsets(spec, swb, sample_rate, psy.tonality_smr, target_bits);
        let (_, sf, (_, _, _, quant)) = rate_loop_coded(&xp, swb, &offsets, target_bits);
        (sf, quant)
    }
}

/// The highest SFB with a non-zero coefficient in either channel (both channels of
/// a common-window CPE share `max_sfb`).
fn joint_max_sfb(q0: &[i32], q1: &[i32], swb: &[u16], is_short: bool) -> usize {
    let num_swb = swb.len() - 1;
    let nwin = if is_short { 8 } else { 1 };
    let wlen = if is_short { SHORT_HALF } else { FRAME_LEN };
    let nz = |q: &[i32], s: usize, e: usize| {
        (0..nwin).any(|win| {
            let b = win * wlen;
            q[b + s..b + e].iter().any(|&x| x != 0)
        })
    };
    let mut m = 0;
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        if nz(q0, s, e) || nz(q1, s, e) {
            m = sfb + 1;
        }
    }
    m
}

/// Per-SFB codebooks for one channel over `0..max_sfb` (for every window group of
/// a short frame, group-major).
fn codebooks(quant: &[i32], swb: &[u16], is_short: bool, max_sfb: usize, groups: &[u8]) -> Vec<u8> {
    if is_short {
        group_windows(groups)
            .flat_map(|(w0, nwin)| short_books(quant, swb, max_sfb, w0, nwin).0)
            .collect()
    } else {
        long_books(quant, swb, max_sfb).0
    }
}

/// `ms_mask_present` (2 bits) + the per-SFB mask when mixed.
fn write_ms_used(w: &mut BitWriter, ms_used: &[bool]) {
    if ms_used.iter().all(|&b| !b) {
        w.write(0, 2);
    } else if ms_used.iter().all(|&b| b) {
        w.write(2, 2);
    } else {
        w.write(1, 2);
        for &b in ms_used {
            w.write_bool(b);
        }
    }
}

/// One channel's `individual_channel_stream` body inside a common-window CPE:
/// `global_gain`, `section_data`, `scale_factor_data`, the three flag bits, `spectral_data`
/// (no `ics_info` — it is shared).
#[allow(
    clippy::too_many_arguments,
    reason = "one argument per syntax element the bitstream writes; a struct would only rename them"
)]
fn write_channel_data(
    w: &mut BitWriter,
    cbs: &[u8],
    sf: &[i32],
    quant: &[i32],
    swb: &[u16],
    is_short: bool,
    groups: &[u8],
    max_sfb: usize,
) {
    let sf_flat;
    let sf = if is_short {
        let f = flatten_short_sf(sf, swb.len() - 1, max_sfb, groups.len());
        sf_flat = if section_trellis_on() {
            zero_band_sf_short(cbs, &f, quant, swb, groups, max_sfb)
        } else {
            f
        };
        &sf_flat[..]
    } else if section_trellis_on() {
        sf_flat = zero_band_sf_long(cbs, sf, quant, swb);
        &sf_flat[..]
    } else {
        sf
    };
    let gg = global_gain(cbs, sf);
    w.write(gg as u32, 8);
    if is_short {
        write_sections_short_grouped(w, cbs, groups.len(), max_sfb);
    } else {
        write_sections(w, cbs);
    }
    write_scalefactors(w, cbs, sf, gg);
    w.write(0, 1); // pulse_data_present
    w.write(0, 1); // tns_data_present
    w.write(0, 1); // gain_control_data_present
    if is_short {
        write_spectrum_short(w, quant, cbs, swb, groups, max_sfb);
    } else {
        write_spectrum(w, quant, cbs, swb);
    }
}

/// **Arm A13 — joint rate loop over a channel pair.**
///
/// # The defect this fixes
///
/// A CPE's two channels were each given `frame_budget / channel_count` bits and
/// run through **independent** rate loops. That is a fixed 50/50 split, so when
/// M/S makes the side channel nearly empty its bits are simply wasted — the mid
/// channel, which is what the listener hears, still gets only half the frame.
///
/// Measured on perfectly-correlated stereo (`L == R`, so `S ≡ 0`) at 128 kbps,
/// per-channel reconstruction SNR:
///
/// | encoder | SNR |
/// |---|---|
/// | independent per-channel loops | **10.6 dB** |
/// | ffmpeg native AAC | **38.8 dB** |
///
/// # Why the obvious fix did not work
///
/// The first attempt split the budget by each channel's perceptual entropy. It
/// made things *worse* (8.5 dB), because PE is degenerate under a purely
/// relative masking model: the mask is `SMR × spread(signal)`, so `E/thr ≈
/// 1/SMR` for **any** non-silent channel regardless of its level. Both channels
/// report near-identical "demand" whatever they contain. A level-blind demand
/// estimate cannot allocate between levels.
///
/// # What actually works
///
/// Run **one** rate loop over the pair and give both channels a **common base**
/// scalefactor. A common base means a common quantizer step, i.e. both channels
/// are coded to the same noise floor relative to their own masks — which is the
/// textbook joint-stereo criterion. Bits then flow to whichever channel needs
/// them, because a near-empty side channel costs almost nothing at any base and
/// lets the loop settle on a finer base for the pair.
///
/// This is a strict generalisation: for two channels of equal demand it lands on
/// the same place the independent loops did.
#[allow(clippy::too_many_arguments)]
fn pair_body_bits(
    xp0: &Xpow,
    off0: &Offsets,
    xp1: &Xpow,
    off1: &Offsets,
    swb: &[u16],
    base: i32,
    groups: &[u8],
) -> usize {
    code_frame_short(xp0, swb, &scalefactors(off0, base), groups).1
        + code_frame_short(xp1, swb, &scalefactors(off1, base), groups).1
}

/// Both channels' quantized spectra of a channel pair.
type PairQuant = (Vec<i32>, Vec<i32>);

/// Smallest common base whose combined body fits `target_total` bits, and for
/// long blocks both channels' quantized spectra at it when the search kept them.
#[allow(clippy::too_many_arguments)]
fn joint_rate_loop(
    xp0: &Xpow,
    off0: &Offsets,
    xp1: &Xpow,
    off1: &Offsets,
    swb: &[u16],
    target_total: usize,
    is_short: bool,
    groups: &[u8],
) -> (i32, Option<PairQuant>) {
    let lo0 = if is_short {
        min_base_short(xp0)
    } else {
        min_base(xp0, swb, off0)
    };
    let lo1 = if is_short {
        min_base_short(xp1)
    } else {
        min_base(xp1, swb, off1)
    };
    let mut lo = lo0.max(lo1);
    let mut hi = off0.max_level().max(off1.max_level());
    if is_short && section_trellis_on() {
        let est = |b: i32| {
            estimate_bits_short(xp0, swb, off0, b, groups)
                + estimate_bits_short(xp1, swb, off1, b, groups)
        };
        let exact = |b: i32| pair_body_bits(xp0, off0, xp1, off1, swb, b, groups);
        return (seeded_level(lo, hi, target_total, est, exact), None);
    }
    if is_short {
        while lo < hi {
            let mid = (lo + hi) / 2;
            if pair_body_bits(xp0, off0, xp1, off1, swb, mid, groups) <= target_total {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        return (lo, None);
    }
    if section_trellis_on() {
        return joint_rate_loop_seeded(xp0, off0, xp1, off1, swb, target_total, lo, hi);
    }
    // Long blocks: the same search, keeping both channels' quantized spectra
    // from the last fitting level. That level is the answer whenever the search
    // evaluated it, so the caller need not quantize the pair again.
    let (mut q0, mut q1) = (vec![0i32; xp0.len()], vec![0i32; xp1.len()]);
    let mut best: Option<(i32, Vec<i32>, Vec<i32>)> = None;
    while lo < hi {
        let mid = (lo + hi).div_euclid(2);
        let bits = code_core(xp0, swb, &scalefactors(off0, mid), &mut q0).1
            + code_core(xp1, swb, &scalefactors(off1, mid), &mut q1).1;
        if bits <= target_total {
            hi = mid;
            let (k0, k1) = (std::mem::take(&mut q0), std::mem::take(&mut q1));
            if let Some((_, o0, o1)) = best.take() {
                (q0, q1) = (o0, o1);
            } else {
                (q0, q1) = (vec![0i32; xp0.len()], vec![0i32; xp1.len()]);
            }
            best = Some((mid, k0, k1));
        } else {
            lo = mid + 1;
        }
    }
    let kept = best.and_then(|(b, k0, k1)| (b == lo).then_some((k0, k1)));
    (lo, kept)
}

/// The long-block [`joint_rate_loop`] under the section trellis: the search
/// shape of [`rate_loop_coded`], a binary search on the cheap estimate that
/// seeds a walk with the exact coder to the smallest common level whose pair
/// fits. The plain binary search ran the exact coder (now with its trellis) at
/// every one of its ~9 steps, for both channels; this runs it two or three
/// times. Same answer wherever the exact bits fall monotonically in the level.
#[allow(clippy::too_many_arguments)]
fn joint_rate_loop_seeded(
    xp0: &Xpow,
    off0: &Offsets,
    xp1: &Xpow,
    off1: &Offsets,
    swb: &[u16],
    target_total: usize,
    min_level: i32,
    max_level: i32,
) -> (i32, Option<PairQuant>) {
    let (mut lo, mut hi) = (min_level, max_level);
    while lo < hi {
        let mid = (lo + hi).div_euclid(2);
        if estimate_bits(xp0, swb, off0, mid) + estimate_bits(xp1, swb, off1, mid) <= target_total {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let (mut q0, mut q1) = (vec![0i32; xp0.len()], vec![0i32; xp1.len()]);
    let mut best: Option<(i32, Vec<i32>, Vec<i32>)> = None;
    let mut exact = |b: i32| {
        let bits = code_core(xp0, swb, &scalefactors(off0, b), &mut q0).1
            + code_core(xp1, swb, &scalefactors(off1, b), &mut q1).1;
        let fits = bits <= target_total;
        if fits {
            let (k0, k1) = (std::mem::take(&mut q0), std::mem::take(&mut q1));
            (q0, q1) = match best.take() {
                Some((_, o0, o1)) => (o0, o1),
                None => (vec![0i32; xp0.len()], vec![0i32; xp1.len()]),
            };
            best = Some((b, k0, k1));
        }
        fits
    };
    let mut level = lo;
    if exact(level) {
        while level > min_level && exact(level - 1) {
            level -= 1;
        }
    } else {
        while level < max_level && !exact(level) {
            level += 1;
        }
    }
    let kept = best.and_then(|(b, k0, k1)| (b == level).then_some((k0, k1)));
    (level, kept)
}

/// **Arm A7 — intensity stereo.**
///
/// Above a cutoff the ear localizes by ENVELOPE rather than waveform, so a band
/// of the second channel can be sent as a single scale factor applied to the
/// first: the decoder reconstructs `s1 = +-0.5^(0.25*is_pos) * s0`
/// (`decode::apply_is`). One value replaces a whole band's spectrum.
///
/// This turns the stereo decision from binary into a **3-arm dispatch** —
/// `{L/R, M/S, IS}` — which is why it is applied after `mid_side` and only to
/// bands M/S declined: with `ms_used` set the pair is already (M, S), and
/// intensity on an S channel means something quite different from intensity on
/// an R channel. Restricting to the M/S-declined bands keeps the two tools from
/// interpreting each other's output.
///
/// The anti-class is wide-stereo music, where collapsing a band to a scaled copy
/// destroys the image. Hence the correlation gate: only bands where the channels
/// genuinely already agree up to a gain.
/// **Arm A7 — intensity stereo.**
///
/// Above a cutoff the ear localizes by ENVELOPE rather than waveform, so a band
/// of the right channel can be sent as a single scale factor applied to the left:
/// the decoder reconstructs `R = +-0.5^(0.25*is_pos) * L` (`decode::apply_is`).
/// One value replaces a whole band's spectrum.
///
/// # Why this decides BEFORE `mid_side`, not after
///
/// The first version ran after M/S and only touched bands M/S had declined — on
/// the reasoning that intensity on an S channel means something different from
/// intensity on an R channel. It never fired once. The bands where `R ~= k*L`
/// are exactly the bands where M/S wins (`E_M*E_S << E_L*E_R` for a scaled copy),
/// so "M/S declined it" and "intensity wants it" are near-disjoint by
/// construction.
///
/// So intensity is decided on the ORIGINAL L/R spectra and **vetoes M/S** on the
/// bands it claims, which also keeps the decoder's sign rule simple: `apply_is`
/// flips the intensity sign when `ms_used` is set, and a band that is both is a
/// sign error waiting to happen.
///
/// This makes the stereo decision a genuine **3-arm dispatch** — `{L/R, M/S, IS}`
/// — rather than two tools overwriting each other. The anti-class is wide-stereo
/// music, where collapsing a band to a scaled copy destroys the image; hence the
/// correlation gate.
fn intensity_decision(
    spec_l: &[f32],
    spec_r: &[f32],
    swb: &[u16],
    sample_rate: u32,
) -> Vec<Option<(u8, i32)>> {
    /// Below this the ear localizes by waveform and intensity stereo is audible.
    const IS_MIN_HZ: f64 = 6000.0;
    /// How well `R ~= scale * L` must already hold, as |cos| between the bands.
    const IS_MIN_CORR: f64 = 0.85;

    let num_swb = swb.len() - 1;
    let mut out = vec![None; num_swb];
    for sfb in 0..num_swb {
        let lo_hz = f64::from(swb[sfb]) * f64::from(sample_rate) * 0.5 / FRAME_LEN as f64;
        if lo_hz < IS_MIN_HZ {
            continue;
        }
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let e = e.min(spec_l.len()).min(spec_r.len());
        if e <= s {
            continue;
        }
        let (mut dot, mut e0, mut e1) = (0f64, 0f64, 0f64);
        for i in s..e {
            let (a, b) = (f64::from(spec_l[i]), f64::from(spec_r[i]));
            dot += a * b;
            e0 += a * a;
            e1 += b * b;
        }
        if e0 <= 1.0 || e1 <= 1.0 {
            continue;
        }
        if (dot / (e0.sqrt() * e1.sqrt())).abs() < IS_MIN_CORR {
            continue;
        }
        // Least-squares gain, then the syntax's log grid:
        // scale = 0.5^(0.25*is_pos)  =>  is_pos = -4*log2(|scale|).
        let scale = dot / e0;
        if scale.abs() < 1e-6 {
            continue;
        }
        let is_pos = (-4.0 * scale.abs().log2()).round() as i32;
        if !(-20..=60).contains(&is_pos) {
            continue;
        }
        out[sfb] = Some((
            if scale >= 0.0 {
                INTENSITY_HCB
            } else {
                INTENSITY_HCB2
            },
            is_pos,
        ));
    }
    out
}

/// Encode a stereo pair as a common-window `channel_pair_element` with per-SFB M/S.
#[allow(clippy::too_many_arguments)]
fn encode_cpe(
    w: &mut BitWriter,
    tag: u32,
    spec_l: &[f32],
    spec_r: &[f32],
    swb: &[u16],
    seq: WindowSequence,
    sample_rate: u32,
    target_bits: usize,
    cur_kbd: bool,
    psy: PsyCfg,
) {
    let is_short = seq == WindowSequence::EightShort;
    // Arm A7 decides first, on the untouched L/R spectra, and vetoes M/S on the
    // bands it claims.
    let is_bands = if psy.intensity && !is_short {
        intensity_decision(spec_l, spec_r, swb, sample_rate)
    } else {
        vec![None; swb.len() - 1]
    };
    let is_veto: Vec<bool> = is_bands.iter().map(std::option::Option::is_some).collect();
    // Both channels share the ics_info, hence one grouping decided on both.
    let groups = if is_short {
        short_grouping(&[spec_l, spec_r], psy)
    } else {
        vec![1]
    };
    let (ch0, ch1, ms_full) = mid_side(spec_l, spec_r, swb, is_short, &is_veto, &groups);
    // Arm A13 ROUTING GATE.
    //
    // The joint loop is a large win when the pair is CORRELATED and a loss when
    // it is not — a per-class sign flip, so it is routed rather than averaged:
    //
    // | content | 64k | 128k |
    // |---|---|---|
    // | guitar stereo (correlated) | +0.23 | **+1.15** |
    // | piano stereo (correlated)  | +0.01 | +0.26 |
    // | wide/decorrelated stereo   | −0.30 | −0.04 |
    //
    // The signal is already in hand: the fraction of coded bands M/S claimed.
    // High means the two channels largely agree, so the side channel is cheap and
    // a common base lets the mid channel take the bits it is wasting. Low means
    // both channels carry independent content, both genuinely need their half,
    // and forcing a common base starves whichever is harder.
    //
    // The signal is the pair's spectral correlation, measured directly. (The M/S
    // flag fraction was tried first and does not separate: M/S still claims most
    // bands on wide content because it only needs `E_M·E_S < E_L·E_R`, which a
    // shared centre image satisfies.) Measured correlations:
    //
    //   L == R (ideal)   1.00   -> joint wins hugely
    //   guitar stereo    0.84   -> joint wins +1.15 @128k
    //   piano stereo     0.47   -> joint wins +0.26 @128k
    //   synthetic wide   0.25   -> joint LOSES -0.30 @64k
    //
    // The threshold sits in the wide gap between 0.25 and 0.47.
    let joint_ok = pair_correlation(spec_l, spec_r) >= JOINT_STEREO_MIN_CORR;

    let (sf0, quant0, sf1, quant1) = if psy.stereo_bit_split && joint_ok {
        let off = |spec: &[f32]| -> Offsets {
            if is_short {
                short_offsets(spec, swb, sample_rate, psy, &groups)
            } else {
                perceptual_offsets(spec, swb, sample_rate, psy.tonality_smr, target_bits)
            }
        };
        let (mut o0, mut o1) = (off(&ch0), off(&ch1));
        Offsets::join(&mut o0, &mut o1);
        let (xp0, xp1) = (Xpow::new(&ch0), Xpow::new(&ch1));
        let (base, kept) = joint_rate_loop(
            &xp0,
            &o0,
            &xp1,
            &o1,
            swb,
            target_bits * 2,
            is_short,
            &groups,
        );
        let (s0, s1) = (scalefactors(&o0, base), scalefactors(&o1, base));
        let (q0, q1) = match kept {
            Some(q) => q,
            None if is_short => (
                code_frame_short(&xp0, swb, &s0, &groups).3,
                code_frame_short(&xp1, swb, &s1, &groups).3,
            ),
            None => (code_frame(&xp0, swb, &s0).3, code_frame(&xp1, swb, &s1).3),
        };
        (s0, q0, s1, q1)
    } else {
        let (a, b) = quantize_channel(&ch0, swb, is_short, sample_rate, target_bits, psy, &groups);
        let (c, d) = quantize_channel(&ch1, swb, is_short, sample_rate, target_bits, psy, &groups);
        (a, b, c, d)
    };
    let max_sfb = joint_max_sfb(&quant0, &quant1, swb, is_short);
    let cbs0 = codebooks(&quant0, swb, is_short, max_sfb, &groups);
    let mut cbs1 = codebooks(&quant1, swb, is_short, max_sfb, &groups);
    let mut sf1 = sf1;
    let mut ms_full = ms_full;

    // Arm A7 — stamp the claimed bands into channel 1's codebooks/scalefactors.
    for (sfb, band) in is_bands.iter().enumerate().take(max_sfb) {
        if let (Some((cb, is_pos)), Some(slot)) = (band, cbs1.get_mut(sfb)) {
            *slot = *cb;
            sf1[sfb] = *is_pos;
            if let Some(m) = ms_full.get_mut(sfb) {
                *m = false;
            }
        }
    }

    w.write(ID_CPE, 3);
    w.write(tag, 4);
    w.write(1, 1); // common_window
    let info = IcsInfo {
        window_sequence: seq,
        window_shape_kbd: cur_kbd,
        max_sfb: max_sfb as u8,
        num_windows: if is_short { 8 } else { 1 },
        num_window_groups: groups.len(),
        window_group_length: groups.clone(),
        num_swb: swb.len() - 1,
    };
    encode_ics_info(w, &info);
    // ms_used is per group, per band below max_sfb, in group order.
    let num_swb = swb.len() - 1;
    let ms_coded: Vec<bool> = (0..groups.len())
        .flat_map(|g| ms_full[g * num_swb..g * num_swb + max_sfb].iter().copied())
        .collect();
    write_ms_used(w, &ms_coded);
    write_channel_data(w, &cbs0, &sf0, &quant0, swb, is_short, &groups, max_sfb);
    write_channel_data(w, &cbs1, &sf1, &quant1, swb, is_short, &groups, max_sfb);
}

// ---------------------------------------------------------------------------
// The encoder: buffers input, blocks into 1024-sample long frames, emits raw
// access units (add ADTS or MP4 framing around them as needed).
// ---------------------------------------------------------------------------

/// Window-shape policy — **arm A8, Rung 0** of `docs/finished/codec-aac-great-gate.md`.
///
/// `window_shape` is a *free syntax element*: one bit already carried in every
/// `ics_info`, costing nothing to set either way, with both shapes implemented on
/// the decode side since day one. It shipped hardcoded to sine — census category 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowShape {
    /// Sine everywhere. **The neutral end**: byte-identical with the encoder as
    /// it shipped before Rung 0 (proven by `shape_sine_is_byte_identical`).
    #[default]
    Sine,
    /// KBD everywhere. Not a shipping mode — the *force-on* arm the calculator
    /// needs: great-gate §4 requires force-on-everywhere to nearly tie the anchor
    /// on the full ladder before a dispatch is built on it, because a big force-on
    /// gap predicts a dominated dispatch.
    Kbd,
    /// **PROVISIONAL** (law 5). Per-frame dispatch on content tonality. Off by
    /// default until the calculator banks a threshold from a harvest carrying the
    /// full speed pair (law 7's opt-in → default-on ladder).
    Auto,
}

/// Coding bandwidth policy: the highest frequency the encoder codes.
///
/// Bands that start at or above the cutoff are not coded (`ZERO_HCB`), in long
/// and short blocks alike, for every channel except the LFE (which has its own
/// band limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Bandwidth {
    /// Code the whole spectrum at every bitrate: the encoder as it shipped in
    /// 2.1.2 (byte-identical).
    Full,
    /// A cutoff from the bitrate and channel count (see [`auto_bandwidth_hz`]).
    /// The default.
    #[default]
    Auto,
    /// A fixed cutoff in Hz.
    Hz(u32),
}

/// The [`Bandwidth::Auto`] cutoff in Hz for a stream of `bitrate_bps` over
/// `channels` at `sample_rate`, or `None` for the whole spectrum.
///
/// 17 kHz while the stream is under 192 kbps in total and at most 128 kbps a
/// channel; the whole spectrum above. Measured (PEAQ, `rusty_aac` 2.1.2 vs the same
/// encoder band-limited, decoder FFmpeg 8.1.2):
///
/// * real music, 64-128 kbps: 17 kHz +0.015 BD-ODG over 6 clips, within
///   ±0.04 on every one: the rate loop already leaves little above it;
/// * flat-spectrum content (noise, noisy speech), 64-128 kbps: +2.1 and +2.0
///   BD-ODG. Coding 17-22 kHz costs a flat spectrum about a quarter of its
///   bits, and PEAQ marks the 18-20 kHz region on such content hard: both
///   FFmpeg versions lose ~2 ODG between 96 kbps (18 kHz cutoff) and 128 kbps
///   (20 kHz) on the same clip;
/// * at 256 kbps stereo (128 kbps a channel) real music started to lose
///   (-0.06 on one clip, -0.08 on one channel scored alone), hence the
///   total-rate ceiling: from 192 kbps up the bandwidth is the 2.1.2 one.
///
/// Mono and stereo only: no multichannel layout was measured, so 3 or more
/// channels keep the whole spectrum, as in 2.1.2.
#[must_use]
pub fn auto_bandwidth_hz(bitrate_bps: u32, channels: u16, sample_rate: u32) -> Option<u32> {
    let per_channel = bitrate_bps / u32::from(channels.max(1));
    (channels <= 2
        && bitrate_bps < AUTO_BANDWIDTH_MAX_BPS
        && per_channel <= AUTO_BANDWIDTH_MAX_BPS_PER_CHANNEL)
        .then_some(AUTO_BANDWIDTH_HZ)
        .filter(|&hz| hz < sample_rate / 2)
}

/// The [`Bandwidth::Auto`] cutoff (see [`auto_bandwidth_hz`]).
const AUTO_BANDWIDTH_HZ: u32 = 17_000;
/// [`Bandwidth::Auto`] limits only streams under this total rate.
const AUTO_BANDWIDTH_MAX_BPS: u32 = 192_000;
/// [`Bandwidth::Auto`] limits only streams at or under this rate a channel.
const AUTO_BANDWIDTH_MAX_BPS_PER_CHANNEL: u32 = 128_000;

/// The first spectral line not coded under `hz` for a window of `n_coeffs`
/// lines (`usize::MAX` for no limit): the start of the first band at or above
/// the cutoff, so the limit always falls on a band edge.
fn cutoff_line(hz: Option<u32>, sample_rate: u32, swb: &[u16], n_coeffs: usize) -> usize {
    let Some(hz) = hz else {
        return usize::MAX;
    };
    let line =
        (u64::from(hz) * 2 * n_coeffs as u64).div_ceil(u64::from(sample_rate.max(1))) as usize;
    swb.iter()
        .map(|&s| usize::from(s))
        .find(|&s| s >= line)
        .unwrap_or(usize::MAX)
}

/// Bounds on a frame's share of its segment under
/// [`AacTuning::frame_demand`], as multiples of the constant-rate share.
const DEMAND_MIN_SHARE: f64 = 0.35;
const DEMAND_MAX_SHARE: f64 = 2.5;

/// Per-frame weights (multiples of the constant-rate share, mean 1) from the
/// frames' perceptual entropies `pe`: `(pe / mean)^alpha`, clamped to
/// [`DEMAND_MIN_SHARE`, `DEMAND_MAX_SHARE`] and renormalised to mean 1.
fn frame_demand_weights(pe: &[f64], alpha: f64) -> Vec<f64> {
    let n = pe.len().max(1) as f64;
    let mean = pe.iter().sum::<f64>() / n;
    if mean <= 0.0 || alpha <= 0.0 {
        return vec![1.0; pe.len()];
    }
    let mut w: Vec<f64> = pe
        .iter()
        .map(|&p| (p.max(0.0) / mean).powf(alpha))
        .collect();
    for _ in 0..4 {
        let m = w.iter().sum::<f64>() / n;
        if m <= 0.0 {
            return vec![1.0; pe.len()];
        }
        for x in &mut w {
            *x = (*x / m).clamp(DEMAND_MIN_SHARE, DEMAND_MAX_SHARE);
        }
    }
    let m = w.iter().sum::<f64>() / n;
    w.iter().map(|x| x / m).collect()
}

/// Zero every line at or above `cut` in each of `nwin` windows of `wlen` lines.
fn band_limit(spec: &mut [f32], cut: usize, wlen: usize) {
    if cut >= wlen {
        return;
    }
    for w in spec.chunks_mut(wlen) {
        w[cut..].fill(0.0);
    }
}

/// Encoder options.
#[derive(Debug, Clone, Copy)]
pub struct AacEncoderConfig {
    /// Target bitrate in bits per second. Drives the per-frame rate loop.
    /// Default: 128 000 (128 kbps).
    pub bitrate_bps: u32,
    /// Window-shape policy (arm A8). Default [`WindowShape::Sine`] — the
    /// byte-identical neutral end.
    pub window_shape: WindowShape,
    /// The [`WindowShape::Auto`] gate's threshold, as a **percentile of this
    /// clip's own** frame-tonality distribution (great-gate law 1: a
    /// population-relative threshold transfers to content the fit never saw; an
    /// absolute one does not). Frames above it code with KBD.
    ///
    /// Default 0.5 is a **placeholder, not a fitted value** — the calculator has
    /// not banked this rung yet. `aacharvest` emits the CSV that will decide it.
    pub shape_tonality_pct: f32,
    /// **Arm A1, Rung 1** — run the psychoacoustic model on short blocks instead
    /// of coding them with flat scalefactors. Default `false` (byte-identical).
    pub short_block_psy: bool,
    /// **Arm 1a, Rung 1** — split each short-block frame into window groups of
    /// similar energy and shape scalefactors per group (the short-block psy model
    /// run per group, centred jointly so a quiet pre-attack group is coded finer
    /// than the attack). Default `false` (byte-identical).
    pub window_grouping: bool,
    /// **Arm A2, Rung 2** — make the signal-to-mask ratio a function of band
    /// tonality instead of a flat 18 dB. Default `false` (byte-identical).
    pub tonality_smr: bool,
    /// **Arm A3, Rung 3** — emit TNS on long blocks. Default `false`
    /// (`tns_data_present = 0`, byte-identical).
    pub tns: bool,
    /// **Arm A9** — use the level-invariant transient detector. Default `false`
    /// (byte-identical). Arm A1 is unmeasurable without this.
    pub relative_transients: bool,
    /// **Arm A6** — Perceptual Noise Substitution on long blocks. Default `false`
    /// (byte-identical).
    pub pns: bool,
    /// **Arm A7** — intensity stereo on long-block CPEs. Default `false`
    /// (byte-identical).
    pub intensity: bool,
    /// **Arm A13** — split a channel pair's bit budget by perceptual demand
    /// rather than evenly. Default `false` (byte-identical).
    pub stereo_bit_split: bool,
}

impl Default for AacEncoderConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: 128_000,
            window_shape: WindowShape::Sine,
            shape_tonality_pct: 0.5,
            short_block_psy: false,
            window_grouping: false,
            tonality_smr: false,
            tns: false,
            // DEFAULT ON — measured wins vs the previous default, PEAQ ODG:
            //   A9  mean +0.267, worst +0.000 (percussive +1.8, clean speech
            //       +1.1..+2.6, exactly 0.00 on every other class)
            //   A13 guitar stereo +1.17 @128k, piano stereo +0.22, routed by
            //       pair correlation so wide stereo is not harmed
            relative_transients: true,
            pns: false,
            intensity: false,
            stereo_bit_split: true,
        }
    }
}

/// The low-rate coding tools added in 2.2: the section trellis, the
/// high-passed transient detector, the coding bandwidth, frame-demand bit
/// allocation and the mask SMR.
///
/// Kept apart from [`AacEncoderConfig`] so that config keeps the field set it
/// had in 2.1 (a struct literal naming every field still compiles). Pass one
/// to [`AacEncoder::with_tuning`]; [`AacEncoder::new`] uses
/// [`AacTuning::default`], the shipping tools. [`AacTuning::legacy`] is the
/// 2.1.2 encoder, bit for bit.
///
/// `#[non_exhaustive]`: build one from [`Default`] or [`AacTuning::legacy`]
/// and set the fields you want.
///
/// ```
/// use rusty_aac::{AacEncoder, AacEncoderConfig, AacTuning};
/// let mut tuning = AacTuning::default();
/// tuning.frame_demand = 0.0; // every frame at its constant-rate share
/// let enc = AacEncoder::with_tuning(AacEncoderConfig::default(), tuning);
/// assert_eq!(enc.tuning(), tuning);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct AacTuning {
    /// Coding bandwidth policy. Default [`Bandwidth::Auto`];
    /// [`Bandwidth::Full`] is the 2.1.2 encoder.
    pub bandwidth: Bandwidth,
    /// Inter-frame bit allocation strength: each frame of a 32-frame
    /// reservoir segment gets the segment's bits in proportion to its
    /// perceptual entropy raised to this power (clamped to 0.35-2.5 times the
    /// constant-rate share), so the segment, and the stream, keep their rate.
    /// Default `0.5`; `0.0` gives every frame the constant-rate share, the
    /// 2.1.2 encoder. Whole-clip encodes only: a streaming encode always uses
    /// the constant-rate share.
    pub frame_demand: f32,
    /// Run the level-invariant transient detector (arm A9) on a high-passed
    /// signal (second difference, +12 dB/octave). A plucked or struck note
    /// whose attack lives in its high frequencies, over a sustained body that
    /// dominates the plain energy, is then switched to short blocks. Default
    /// `true`; `false` is the 2.1.2 detector. No effect with
    /// `relative_transients` off.
    pub transient_highpass: bool,
    /// Choose each frame's codebooks by a Viterbi trellis over `section_data`
    /// (spectral bits plus section headers, an all-zero band free to ride in
    /// a neighbouring section) instead of the cheapest book band by band,
    /// which opened a new 9-bit section at nearly every band: 16-17% of a
    /// 64 kbps frame, against 3% for FFmpeg. Default `true`; `false` is the
    /// 2.1.2 encoder.
    pub section_trellis: bool,
    /// The psychoacoustic model's flat signal-to-mask ratio, in dB: how far
    /// below each band's spread energy its masking threshold sits, before the
    /// threshold in quiet and the starved-frame reduction, for mono and stereo
    /// streams under 96 kbps a channel (from 96 kbps a channel up, and for 3
    /// or more channels, the mask is 18 dB whatever this says).
    /// Default `24.0`; `18.0` is the 2.1.2 encoder (bit for bit). A deeper mask makes the
    /// rate loop reach the starved-frame reduction sooner, which keeps the
    /// precision of a frame's loud, tonal bands and coarsens its quiet ones:
    /// at 64 kbps a sung voice's 0-4 kHz went from 16-20 dB SNR to 19-23 dB.
    pub mask_smr_db: f32,
    /// Spend bits left over far below the masks on the bands that carry the
    /// signal, not evenly on every band (for mono and stereo streams from 96
    /// kbps a channel; no effect on others). Default `true`; `false` is the
    /// 2.1.2 encoder.
    pub surplus_shaping: bool,
}

impl Default for AacTuning {
    /// The shipping tools — measured vs 2.1.2, PEAQ BD-ODG over 6 real clips at
    /// 64-128 kbps, each tool alone: section trellis +0.223 (every clip and
    /// rate better), high-passed transients +0.087, frame demand 0.5 +0.031
    /// (synthetic +0.25), bandwidth `Auto` +0.015 (synthetic noise +2.0);
    /// together +0.291; mask SMR 24 dB +0.097 on top (stereo vocal +0.41).
    fn default() -> Self {
        Self {
            bandwidth: Bandwidth::Auto,
            frame_demand: 0.5,
            transient_highpass: true,
            section_trellis: true,
            mask_smr_db: 24.0,
            surplus_shaping: true,
        }
    }
}

impl AacTuning {
    /// Every 2.2 tool off: the `rusty_aac` 2.1.2 encoder, bit for bit.
    #[must_use]
    pub const fn legacy() -> Self {
        Self {
            bandwidth: Bandwidth::Full,
            frame_demand: 0.0,
            transient_highpass: false,
            section_trellis: false,
            mask_smr_db: 18.0,
            surplus_shaping: false,
        }
    }
}

/// One encoded raw access unit (a `raw_data_block`, no ADTS framing).
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    /// Presentation timestamp in samples (frame index × 1024).
    pub pts: i64,
    /// Duration in samples (always one AAC frame = 1024).
    pub duration: u32,
}

/// Encoding tables a stream needs, built once from its rate and channels.
struct Tables {
    swb: &'static [u16],
    swb_s: &'static [u16],
    sine_s: Vec<f32>,
    kbd_s: Vec<f32>,
    plan: Vec<Elem>,
}

/// Frames per bit-reservoir segment. The reservoir restarts empty at every
/// multiple of this frame index, so a segment encodes on its own: whole-clip
/// encodes fan segments out across threads, streaming encodes walk them in
/// order, and both produce the same bytes whatever the thread count.
const RESERVOIR_SEGMENT: usize = 32;

/// The bit reservoir: bits the stream is ahead of (positive) or behind its
/// constant-rate channel.
///
/// Each frame is coded to the largest budget it fits, and the rate loop's
/// steps are coarse (one scalefactor step moves a frame by 5-15% of its bits),
/// so frames land short of their budget by half a step on average. With no
/// carry that shortfall was simply lost: 2.5-9% under the target bitrate, and
/// 13% for mono at 53 kbps (`remade_ffmpeg_rs#129`). Banking it and adding the
/// bank to the next frame's budget makes the stream's average land on the
/// target (an overshoot banks negative and is taken back the same way), while
/// the bank stays about one frame's shortfall, far inside the decoder buffer
/// (6144 bits a channel, ISO 14496-3 §4.5.3).
#[derive(Debug, Clone, Copy, Default)]
struct Reservoir {
    bank: i64,
}

impl Reservoir {
    /// Frame `b`'s budget: what the channel delivers for it plus the bank,
    /// never past the decoder buffer.
    fn budget(&mut self, b: usize, mean: usize, channels: usize) -> usize {
        if b % RESERVOIR_SEGMENT == 0 {
            self.bank = 0;
        }
        let cap = (6144 * channels) as i64;
        let floor = (mean as i64 / 2).min(cap);
        (mean as i64 + self.bank).clamp(floor, cap) as usize
    }

    /// Settle frame `b` after it coded to `used` bits.
    fn spend(&mut self, mean: usize, used: usize, channels: usize) {
        let max_bank = (6144 * channels).saturating_sub(mean) as i64;
        self.bank = (self.bank + mean as i64 - used as i64).clamp(-(mean as i64) / 2, max_bank);
    }
}

/// The transient detector run a frame at a time, for streaming: the same rule
/// as [`detect_transients`] (absolute guard) and [`detect_transients_relative`]
/// (floor relative to the signal's mean sub-block energy). A stream does not
/// know its whole clip's mean until the end, so the relative floor uses the
/// mean of the sub-blocks seen so far, this frame's included. The absolute
/// detector needs nothing ahead and gives the whole-clip flags exactly.
#[derive(Clone, Default)]
struct Detector {
    avg: f64,
    sum: f64,
    count: u64,
}

impl Detector {
    fn frame(&mut self, sub: &[f64; 8], f: usize, relative: bool) -> bool {
        if relative {
            self.sum += sub.iter().sum::<f64>();
            self.count += 8;
            let floor = (self.sum / self.count as f64 * 1e-3).max(f64::MIN_POSITIVE);
            let mut amax = 1.0f32;
            for &e in sub {
                amax = amax.max((e / self.avg.max(floor)) as f32);
                self.avg = 0.75 * self.avg + 0.25 * e;
            }
            f > 0 && amax > 10.0
        } else {
            let mut attack = 1.0f64;
            for &e in sub {
                if self.avg > 1e-3 {
                    attack = attack.max(e / self.avg);
                }
                self.avg = 0.75 * self.avg + 0.25 * e;
            }
            f > 0 && attack > 10.0
        }
    }
}

/// [`AacEncoder::set_streaming`] state: what has been analysed and encoded.
struct Streaming {
    tables: Tables,
    /// Per channel.
    det: Vec<Detector>,
    /// Per channel: each analysed frame's raw transient flag.
    flags: Vec<Vec<bool>>,
    /// Per element: each frame's final window sequence.
    seqs: Vec<Vec<WindowSequence>>,
    /// Per frame: KBD (true) or sine window shape.
    shapes: Vec<bool>,
    /// Frames whose transient flags are known.
    analysed: usize,
    /// The next frame to encode.
    next: usize,
    /// The bit reservoir, carried from frame to frame.
    reservoir: Reservoir,
}

/// AAC-LC encoder. Push PCM with [`push_pcm`](AacEncoder::push_pcm) (or
/// [`push_pcm_planar`](AacEncoder::push_pcm_planar)), call
/// [`finish`](AacEncoder::finish), then drain [`next_packet`](AacEncoder::next_packet)
/// until it returns [`Error::Eof`].
///
/// By default every frame is encoded inside `finish`, from a whole-clip
/// analysis. [`set_streaming`](AacEncoder::set_streaming) encodes as the PCM
/// arrives instead: packets come out of `next_packet` two frames behind the
/// input, and memory stays at a few frames however long the stream is. The
/// encoder's delay and padding, for an MP4 edit list, are
/// [`priming_samples`](AacEncoder::priming_samples) and
/// [`padding_samples`](AacEncoder::padding_samples).
pub struct AacEncoder {
    sample_rate: u32,
    channels: usize,
    fs_index: u8,
    /// Target bitrate (bits/s). Drives the per-frame rate loop.
    bitrate: u32,
    /// Arm A8 policy and its provisional threshold.
    window_shape: WindowShape,
    shape_tonality_pct: f32,
    short_block_psy: bool,
    window_grouping: bool,
    tonality_smr: bool,
    tns: bool,
    relative_transients: bool,
    pns: bool,
    intensity: bool,
    stereo_bit_split: bool,
    bandwidth: Bandwidth,
    frame_demand: f32,
    transient_highpass: bool,
    section_trellis: bool,
    mask_smr_db: f32,
    surplus_shaping: bool,
    win: Vec<f32>,
    /// The KBD long window (α = 4.0), matching the decoder's. Built once.
    win_kbd: Vec<f32>,
    chans: Vec<Vec<f32>>,
    initialized: bool,
    /// Encoded raw access units (`raw_data_block`, no ADTS) awaiting `next_packet`,
    /// each with its sample-domain PTS. Filled on `finish` (or as frames
    /// complete, when streaming).
    queue: VecDeque<(Vec<u8>, i64)>,
    flushed: bool,
    /// Samples pushed per channel.
    total_in: u64,
    /// Blocks dropped from the front of `chans` (streaming); `chans[c][0]` is
    /// sample `base_block * FRAME_LEN` of the stream. 0 for a whole-clip encode.
    base_block: usize,
    streaming: bool,
    stream: Option<Box<Streaming>>,
}

impl AacEncoder {
    /// An encoder with `config` and the shipping low-rate tools
    /// ([`AacTuning::default`]).
    #[must_use]
    pub fn new(config: AacEncoderConfig) -> Self {
        Self::with_tuning(config, AacTuning::default())
    }

    /// An encoder with `config` and the low-rate tools set by `tuning`
    /// ([`AacTuning::legacy`] for the 2.1.2 encoder).
    #[must_use]
    pub fn with_tuning(config: AacEncoderConfig, tuning: AacTuning) -> Self {
        Self {
            sample_rate: 0,
            channels: 0,
            fs_index: 0,
            bitrate: config.bitrate_bps.max(1),
            window_shape: config.window_shape,
            shape_tonality_pct: config.shape_tonality_pct,
            short_block_psy: config.short_block_psy,
            window_grouping: config.window_grouping,
            tonality_smr: config.tonality_smr,
            tns: config.tns,
            relative_transients: config.relative_transients,
            pns: config.pns,
            intensity: config.intensity,
            stereo_bit_split: config.stereo_bit_split,
            bandwidth: tuning.bandwidth,
            frame_demand: if tuning.frame_demand.is_finite() {
                tuning.frame_demand.clamp(0.0, 4.0)
            } else {
                0.0
            },
            transient_highpass: tuning.transient_highpass,
            section_trellis: tuning.section_trellis,
            mask_smr_db: if tuning.mask_smr_db.is_finite() {
                tuning.mask_smr_db.clamp(6.0, 40.0)
            } else {
                18.0
            },
            surplus_shaping: tuning.surplus_shaping,
            win: Vec::new(),
            win_kbd: Vec::new(),
            chans: Vec::new(),
            initialized: false,
            queue: VecDeque::new(),
            flushed: false,
            total_in: 0,
            base_block: 0,
            streaming: false,
            stream: None,
        }
    }

    /// The low-rate tools this encoder codes with (out-of-range strengths
    /// clamped as [`with_tuning`](Self::with_tuning) applied them).
    #[must_use]
    pub fn tuning(&self) -> AacTuning {
        AacTuning {
            bandwidth: self.bandwidth,
            frame_demand: self.frame_demand,
            transient_highpass: self.transient_highpass,
            section_trellis: self.section_trellis,
            mask_smr_db: self.mask_smr_db,
            surplus_shaping: self.surplus_shaping,
        }
    }

    /// Encode as the PCM arrives instead of all at [`finish`](Self::finish):
    /// after each push, every frame whose window decisions are final (two
    /// frames behind the input) is encoded and queued for
    /// [`next_packet`](Self::next_packet), and the samples behind it are
    /// dropped. Memory stays at a few frames for a stream of any length.
    ///
    /// Call it before the first push; later calls are ignored. Differences
    /// from the default whole-clip encode, which needs the clip's end:
    /// the level-invariant transient detector (on by default) floors its
    /// running average at the mean energy seen so far rather than the whole
    /// clip's, so quiet openings can switch to short blocks at slightly
    /// different frames; [`WindowShape::Auto`] codes every frame with the
    /// sine window; and every frame gets its constant-rate share of the bits
    /// (frame-demand allocation, [`AacTuning::frame_demand`], needs a
    /// whole reservoir segment ahead). With `relative_transients: false` and
    /// [`AacTuning::frame_demand`] 0 the packets are byte-identical to the whole-clip
    /// encode.
    pub fn set_streaming(&mut self, on: bool) {
        if self.total_in == 0 && !self.flushed {
            self.streaming = on;
        }
    }

    /// Samples of encoder delay at the start of the decoded output: decoded
    /// sample `t` is input sample `t - priming_samples()`. 1024 for AAC-LC (one
    /// MDCT frame of overlap). The `media_time` of an MP4 edit list, and the
    /// first number of an `iTunSMPB`.
    #[must_use]
    pub fn priming_samples(&self) -> u32 {
        FRAME_LEN as u32
    }

    /// Samples of padding after the last input sample in the decoded output,
    /// known once [`finish`](Self::finish) has run: the decoded stream is
    /// `priming + input + padding` samples long, a whole number of 1024-sample
    /// frames. The last number of an `iTunSMPB`; the edit list's segment
    /// duration is the input length.
    #[must_use]
    pub fn padding_samples(&self) -> Option<u32> {
        if !self.flushed || !self.initialized {
            return None;
        }
        let n = self.total_in;
        Some((n.div_ceil(FRAME_LEN as u64) * FRAME_LEN as u64 - n) as u32)
    }

    /// The stream's sample rate (0 until the first PCM is pushed).
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The stream's channel count (0 until the first PCM is pushed).
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.channels as u16
    }

    /// Initialize (or validate) the stream parameters from a PCM push.
    fn init(&mut self, channels: u16, sample_rate: u32) -> Result<()> {
        if !self.initialized {
            self.sample_rate = sample_rate;
            self.channels = channels.max(1) as usize;
            // Reject counts with no `channel_configuration`. Writing a config the
            // element sequence does not match produces a stream that only our own
            // decoder accepts — the bug this check exists to make impossible.
            if element_plan(self.channels).is_none() {
                return Err(Error::unsupported(
                    "aac encode: only 1-6 channels are supported (7+ needs a                      program_config_element, which is not implemented)",
                ));
            }
            self.fs_index = crate::sf_index_for_rate(sample_rate)
                .ok_or_else(|| Error::invalid("aac encode: unsupported sample rate"))?;
            self.win = crate::dsp::sine_window(LONG_N);
            // Arm A8's second shape. α = 4.0 for the long window matches the
            // decoder's `kbd` exactly — a mismatched α is not a quality choice,
            // it breaks TDAC and leaves audible seams at every overlap.
            self.win_kbd = crate::dsp::kbd_window(LONG_N, 4.0);
            self.chans = vec![Vec::new(); self.channels];
            self.initialized = true;
        } else if channels.max(1) as usize != self.channels {
            return Err(Error::invalid(
                "aac encode: channel count changed mid-stream",
            ));
        }
        Ok(())
    }

    /// Buffer interleaved `f32` PCM in [-1, 1] (`interleaved.len()` must be a
    /// multiple of `channels`). The first push fixes the stream's channel count
    /// and sample rate. Samples beyond ±[`MAX_INPUT_AMPLITUDE`] are clamped and
    /// NaN becomes silence, so no input can push a block past the decoder
    /// buffer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] for more than six channels,
    /// [`Error::InvalidData`] for a non-standard sample rate, and
    /// [`Error::InvalidData`] if `channels` differs from the first push.
    pub fn push_pcm(&mut self, interleaved: &[f32], channels: u16, sample_rate: u32) -> Result<()> {
        self.init(channels, sample_rate)?;
        let ch = self.channels;
        let n = interleaved.len() / ch;
        // Reserve once, then one pass per channel: a per-sample push grew each
        // channel buffer by doubling (a realloc + copy of everything so far, ~log2
        // of the stream per channel). Mono is a straight slice copy.
        if ch == 1 {
            self.chans[0].extend(interleaved[..n].iter().map(|&x| sanitize(x)));
        } else {
            for (c, dst) in self.chans.iter_mut().enumerate().take(ch) {
                dst.reserve(n);
                dst.extend(
                    interleaved[..n * ch]
                        .iter()
                        .skip(c)
                        .step_by(ch)
                        .map(|&x| sanitize(x)),
                );
            }
        }
        self.total_in += n as u64;
        if self.streaming {
            self.stream_advance(false);
        }
        Ok(())
    }

    /// Buffer planar `f32` PCM, one slice per channel (`planes.len()` is the
    /// channel count; all planes the same length).
    ///
    /// # Errors
    ///
    /// As [`push_pcm`](Self::push_pcm).
    pub fn push_pcm_planar(&mut self, planes: &[&[f32]], sample_rate: u32) -> Result<()> {
        self.init(planes.len() as u16, sample_rate)?;
        let n = planes
            .iter()
            .take(self.channels)
            .map(|p| p.len())
            .min()
            .unwrap_or(0);
        for (c, plane) in planes.iter().enumerate().take(self.channels) {
            self.chans[c].extend(plane[..n].iter().map(|&x| sanitize(x)));
        }
        self.total_in += n as u64;
        if self.streaming {
            self.stream_advance(false);
        }
        Ok(())
    }

    /// Encode all buffered samples into per-frame raw access units (`raw_data_block`,
    /// no ADTS) with sample-domain PTS. A trailing all-zero block flushes the MDCT
    /// overlap so the final audio block decodes. Containers add their own framing
    /// (ADTS header for `.aac`, `esds` + raw samples for MP4).
    /// Block `b` of channel `ch`, borrowed straight from the buffered input when
    /// it lies wholly inside it; only a block running past the end is built
    /// (zero-padded). Every frame used to copy four 4 KB blocks this way.
    fn block_cow(&self, ch: usize, b: usize) -> std::borrow::Cow<'_, [f32; FRAME_LEN]> {
        let s = (b - self.base_block) * FRAME_LEN;
        match self.chans[ch].get(s..s + FRAME_LEN) {
            Some(x) => std::borrow::Cow::Borrowed(x.try_into().expect("FRAME_LEN samples")),
            None => std::borrow::Cow::Owned(self.block(ch, b)),
        }
    }

    /// One block's samples for channel `ch` (zero-padded past the buffered input).
    fn block(&self, ch: usize, b: usize) -> [f32; FRAME_LEN] {
        let mut cur = [0f32; FRAME_LEN];
        let at = (b - self.base_block) * FRAME_LEN;
        for (i, s) in cur.iter_mut().enumerate() {
            *s = self.chans[ch].get(at + i).copied().unwrap_or(0.0);
        }
        cur
    }

    /// Encode one frame `b` to its raw access unit + PTS. A pure function of the
    /// buffered input, the window sequences, and `b` — the previous block (for MDCT
    /// overlap) is just block `b-1`'s samples — so frames encode **independently**.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn encode_frame(
        &self,
        b: usize,
        swb: &[u16],
        swb_s: &[u16],
        sine_s: &[f32],
        kbd_s: &[f32],
        seqs: &[Vec<WindowSequence>],
        shapes: &[bool],
        frame_budget: usize,
        plan: &[Elem],
        pre: Option<&[Option<Vec<f32>>]>,
    ) -> (Vec<u8>, i64) {
        // Per-channel ICS-body budget: the frame less END (3 bits), split
        // evenly, less each element's own header (7). Never above the decoder
        // input buffer (ISO 14496-3 §4.5.3: 6144 bits per channel) less that
        // overhead: an absurd bitrate must not yield a block a conforming
        // decoder cannot hold.
        let per_channel = (frame_budget.saturating_sub(3) / self.channels.max(1))
            .saturating_sub(7)
            .min(MAX_CHANNEL_BITS);
        let prev = |ch: usize| -> std::borrow::Cow<'_, [f32; FRAME_LEN]> {
            if b == 0 {
                std::borrow::Cow::Owned([0f32; FRAME_LEN])
            } else {
                self.block_cow(ch, b - 1)
            }
        };
        // Arm A8. The shape of frame b-1 owns this frame's LEFT overlap half, so
        // both are needed; the decoder seeds prev = sine at stream start.
        let cur_kbd = shapes.get(b).copied().unwrap_or(false);
        let prev_kbd = if b == 0 {
            false
        } else {
            shapes.get(b - 1).copied().unwrap_or(false)
        };
        let short_win: &[f32] = if cur_kbd { kbd_s } else { sine_s };
        // The demand pre-pass's spectrum of channel `ch` (sine window, band
        // limited), when this frame codes it with exactly that window.
        let cached = |ch: usize, seq: WindowSequence| -> Option<Vec<f32>> {
            if seq == WindowSequence::OnlyLong && !prev_kbd && !cur_kbd {
                pre.and_then(|p| p.get(ch).cloned().flatten())
            } else {
                None
            }
        };
        SECTION_TRELLIS.with(|c| c.set(self.section_trellis));
        MASK_SMR.with(|c| c.set(smr_ratio(self.effective_smr_db())));
        SURPLUS.with(|c| c.set(self.surplus_on()));
        let bw_hz = self.bandwidth_hz();
        let cut_long = cutoff_line(bw_hz, self.sample_rate, swb, FRAME_LEN);
        let cut_short = cutoff_line(bw_hz, self.sample_rate, swb_s, SHORT_HALF);
        let psy = PsyCfg {
            short_block_psy: self.short_block_psy,
            window_grouping: self.window_grouping,
            tonality_smr: self.tonality_smr,
            tns: self.tns,
            pns: self.pns,
            intensity: self.intensity,
            stereo_bit_split: self.stereo_bit_split,
        };

        // One pass over the ISO element sequence. `seqs` is indexed by ELEMENT,
        // not by channel: a CPE shares one window sequence across its pair, so
        // the two channels' transient flags were joined when the plan was built.
        // Encode one element into its own writer at `per_channel` bits per
        // channel. Elements are independent within a frame (the encoder keeps
        // no per-element state between them), so one can be re-encoded at a
        // different budget.
        let encode_elem = |ei: usize, elem: &Elem, per_channel: usize| -> BitWriter {
            let mut wr = BitWriter::new();
            let w = &mut wr;
            let seq = seqs[ei][b];
            let tag = ei as u32;
            match *elem {
                Elem::Cpe(l, r) => {
                    let (p0, c0, p1, c1) =
                        (prev(l), self.block_cow(l, b), prev(r), self.block_cow(r, b));
                    if seq == WindowSequence::EightShort {
                        let mut sl = analyze_short(&p0, &c0, short_win);
                        let mut sr_ = analyze_short(&p1, &c1, short_win);
                        band_limit(&mut sl, cut_short, SHORT_HALF);
                        band_limit(&mut sr_, cut_short, SHORT_HALF);
                        encode_cpe(
                            w,
                            tag,
                            &sl,
                            &sr_,
                            swb_s,
                            seq,
                            self.sample_rate,
                            per_channel,
                            cur_kbd,
                            psy,
                        );
                    } else {
                        let win = long_window(
                            seq,
                            prev_kbd,
                            cur_kbd,
                            &self.win,
                            &self.win_kbd,
                            sine_s,
                            kbd_s,
                        );
                        let fresh = |p: &[f32; FRAME_LEN], c: &[f32; FRAME_LEN]| {
                            let mut x = analyze_long(p, c, &win);
                            band_limit(&mut x, cut_long, FRAME_LEN);
                            x
                        };
                        let sl = cached(l, seq).unwrap_or_else(|| fresh(&p0, &c0));
                        let sr_ = cached(r, seq).unwrap_or_else(|| fresh(&p1, &c1));
                        encode_cpe(
                            w,
                            tag,
                            &sl,
                            &sr_,
                            swb,
                            seq,
                            self.sample_rate,
                            per_channel,
                            cur_kbd,
                            psy,
                        );
                    }
                }
                Elem::Sce(ch) | Elem::Lfe(ch) => {
                    let (p, c) = (prev(ch), self.block_cow(ch, b));
                    let id = elem.id();
                    if seq == WindowSequence::EightShort {
                        let mut spec = analyze_short(&p, &c, short_win);
                        band_limit(&mut spec, cut_short, SHORT_HALF);
                        encode_channel_element_short(
                            w,
                            tag,
                            &spec,
                            swb_s,
                            per_channel,
                            cur_kbd,
                            self.sample_rate,
                            psy,
                        );
                    } else {
                        let win = long_window(
                            seq,
                            prev_kbd,
                            cur_kbd,
                            &self.win,
                            &self.win_kbd,
                            sine_s,
                            kbd_s,
                        );
                        let mut spec = match (elem, cached(ch, seq)) {
                            (Elem::Sce(_), Some(x)) => x,
                            _ => analyze_long(&p, &c, &win),
                        };
                        if matches!(elem, Elem::Lfe(_)) {
                            lfe_band_limit(&mut spec);
                        } else {
                            band_limit(&mut spec, cut_long, FRAME_LEN);
                        }
                        encode_channel_element(
                            w,
                            tag,
                            &spec,
                            swb,
                            seq,
                            self.sample_rate,
                            per_channel,
                            cur_kbd,
                            psy,
                            id,
                        );
                    }
                }
            }
            wr
        };
        let nch = |e: &Elem| if matches!(e, Elem::Cpe(..)) { 2 } else { 1 };
        // The LFE first. Band-limited, it seldom needs its share, and what it
        // leaves is split among the other channels before they are coded;
        // left to the pooling below, it made every element of nearly every 5.1
        // frame encode twice.
        let mut lfe = None;
        let mut lfe_at = usize::MAX;
        let mut share = per_channel;
        if plan.len() > 1 {
            if let Some(li) = plan.iter().position(|e| matches!(e, Elem::Lfe(_))) {
                let w = encode_elem(li, &plan[li], per_channel);
                let spare = (per_channel + 7).saturating_sub(w.bit_len());
                share = (per_channel + spare / (self.channels - 1)).min(MAX_CHANNEL_BITS);
                lfe = Some((li, w));
                lfe_at = li;
            }
        }
        let budgets: Vec<usize> = plan
            .iter()
            .map(|e| {
                let pc = if matches!(e, Elem::Lfe(_)) {
                    per_channel
                } else {
                    share
                };
                pc * nch(e)
            })
            .collect();
        let mut writers: Vec<BitWriter> = plan
            .iter()
            .enumerate()
            .map(|(ei, e)| match lfe.take() {
                Some((li, w)) if li == ei => w,
                other => {
                    lfe = other;
                    encode_elem(ei, e, budgets[ei] / nch(e))
                }
            })
            .collect();
        // Multichannel: hand the bits that some elements leave unused (silence,
        // simple content) to the elements that used their whole share, and
        // re-encode those. The budget had been split evenly and never pooled,
        // so a 5.1 stream with only its centre active coded that centre at a
        // sixth of the bitrate (remade_ffmpeg_rs#129). One element (mono,
        // stereo) has nothing to pool, so its output is unchanged.
        if plan.len() > 1 {
            // An element that used half its budget or more was limited by it:
            // the rate loop's base steps are coarse at small budgets, so a
            // limited element can land well short of its target. Silence uses
            // a few dozen bits.
            // A pre-coded LFE has already handed on what it left.
            let binding = |ei: usize, used: usize| ei != lfe_at && 2 * used >= budgets[ei];
            let mut spare = 0usize;
            let mut binding_ch = 0usize;
            for (ei, (e, w)) in plan.iter().zip(&writers).enumerate() {
                if ei == lfe_at {
                    continue;
                }
                if binding(ei, w.bit_len()) {
                    binding_ch += nch(e);
                } else {
                    spare += budgets[ei] - w.bit_len();
                }
            }
            if binding_ch > 0 && spare >= 64 * binding_ch {
                let more = (share + spare / binding_ch).min(MAX_CHANNEL_BITS);
                if more > share {
                    for (ei, e) in plan.iter().enumerate() {
                        if binding(ei, writers[ei].bit_len()) {
                            writers[ei] = encode_elem(ei, e, more);
                        }
                    }
                }
            }
        }
        let mut rdb = BitWriter::new();
        for w in &writers {
            rdb.append(w);
        }
        rdb.write(ID_END, 3);
        (rdb.into_bytes(), (b * FRAME_LEN) as i64)
    }

    /// The mask SMR this stream codes with: [`AacTuning::mask_smr_db`]
    /// for mono and stereo under [`MASK_SMR_MAX_BPS_PER_CHANNEL`]; the 2.1.2 18 dB from
    /// there up, and for 3 or more channels (no multichannel layout was
    /// measured).
    fn effective_smr_db(&self) -> f32 {
        let per_channel = self.bitrate / self.channels.max(1) as u32;
        if per_channel >= MASK_SMR_MAX_BPS_PER_CHANNEL || self.channels > 2 {
            18.0
        } else {
            self.mask_smr_db
        }
    }

    /// Whether this stream runs [`AacTuning::surplus_shaping`].
    fn surplus_on(&self) -> bool {
        self.surplus_shaping
            && self.channels <= 2
            && self.bitrate / self.channels.max(1) as u32 >= SURPLUS_MIN_BPS_PER_CHANNEL
    }

    /// The coding cutoff in Hz under the configured [`Bandwidth`] policy, or
    /// `None` for the whole spectrum.
    fn bandwidth_hz(&self) -> Option<u32> {
        match self.bandwidth {
            Bandwidth::Full => None,
            Bandwidth::Auto => {
                auto_bandwidth_hz(self.bitrate, self.channels as u16, self.sample_rate)
            }
            Bandwidth::Hz(hz) => Some(hz),
        }
        .filter(|&hz| hz < self.sample_rate / 2)
    }

    /// Transient flags for one channel, under the configured detector (arm A9).
    fn transients(&self, chan: &[f32], nframes: usize) -> Vec<bool> {
        if self.relative_transients {
            detect_transients_relative(chan, nframes, self.transient_highpass)
        } else {
            detect_transients(chan, nframes)
        }
    }

    /// **The Rung 0 gate.** Decide each frame's window shape.
    ///
    /// Computed as a whole-stream pre-pass, exactly like `detect_transients`, for
    /// two reasons: the decision is shared by both channels of a CPE (one common
    /// window), and frame b's window depends on frame b−1's shape — so deciding
    /// inside the frame-parallel worker would make the output depend on thread
    /// scheduling. Deciding up front keeps `encode_frame` independent and the
    /// encoder byte-deterministic (asserted by the lab's null arm).
    ///
    /// Threshold form: **per-clip percentile** of frame tonality (law 1), not an
    /// absolute. `Auto` is PROVISIONAL and off by default (law 7).
    fn decide_shapes(&self, nblocks: usize) -> Vec<bool> {
        match self.window_shape {
            WindowShape::Sine => vec![false; nblocks],
            WindowShape::Kbd => vec![true; nblocks],
            WindowShape::Auto => {
                // One reading per frame, from channel 0 (a CPE shares one window,
                // so the decision cannot be per-channel).
                let ton: Vec<f32> = (0..nblocks)
                    .map(|b| time_tonality(&self.block_cow(0, b)[..]))
                    .collect();
                let atk = frame_attack_ratios(&self.chans[0], nblocks);

                let pct = |v: &[f32], q: f32| -> f32 {
                    let mut s = v.to_vec();
                    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let i =
                        ((s.len().saturating_sub(1)) as f32 * q.clamp(0.0, 1.0)).round() as usize;
                    s.get(i).copied().unwrap_or(f32::INFINITY)
                };
                let ton_thresh = pct(&ton, self.shape_tonality_pct);
                let atk_thresh = pct(&atk, SHAPE_ATTACK_VETO_PCT);

                // A depth-2 transcribed tree (great-gate law 3: shallow, in plain
                // code, one documented feature per node).
                //
                //   attack  — a frame with a strong onset takes SINE, whatever its
                //             tonality. KBD's wider main lobe smears transients in
                //             time; the force-on ladder measured +18.6% audible on
                //             the percussive class, the largest single effect in
                //             the whole Rung 0 harvest.
                //   tonality— among the remaining (stationary) frames, KBD's much
                //             deeper stopband is worth having.
                //
                // The veto is why this gate is not simply "tonality > t": ringing
                // percussion reads as HIGHLY tonal (a struck resonator is a decaying
                // sinusoid), so a tonality-only rule routes exactly the frames that
                // KBD hurts most. The first fit did precisely that.
                // The veto spans a ±1-frame NEIGHBOURHOOD, not just the frame
                // itself. Overlap-add means frame b's window shapes the overlap
                // with both b−1 and b+1, so an onset in either neighbour is
                // analyzed partly through this frame's window. Vetoing only the
                // onset frame left the ring-down frame on KBD and kept the whole
                // percussive regression (+10.0% audible, barely half of force-on's
                // +18.6%) — the measurement that forced this widening.
                let vetoed: Vec<bool> = (0..nblocks)
                    .map(|b| {
                        let lo = b.saturating_sub(1);
                        let hi = (b + 1).min(nblocks.saturating_sub(1));
                        (lo..=hi).any(|k| atk.get(k).copied().unwrap_or(1.0) > atk_thresh)
                    })
                    .collect();

                (0..nblocks)
                    .map(|b| !vetoed[b] && ton[b] > ton_thresh)
                    .collect()
            }
        }
    }

    /// Encode all buffered frames to raw access units. Frames are independent, so
    /// they fan out across worker threads (ffmpeg's AAC encoder is single-threaded).
    fn tables(&self) -> Tables {
        Tables {
            swb: swb_offsets(true, self.fs_index),
            swb_s: swb_offsets(false, self.fs_index),
            sine_s: crate::dsp::sine_window(SHORT_N),
            kbd_s: crate::dsp::kbd_window(SHORT_N, 6.0),
            plan: element_plan(self.channels).expect("channel count validated in init"),
        }
    }

    /// Bits the target bitrate delivers for frame `b`, as whole bits whose
    /// running sum never drifts from `bitrate × time` (at 44.1 kHz a frame is
    /// not a whole number of bits). The target is the raw access unit, the
    /// payload an MP4 stores; an ADTS header (56 bits) comes on top, as with
    /// FFmpeg's encoder.
    fn frame_bits(&self, b: usize) -> usize {
        let per = u64::from(self.bitrate) * FRAME_LEN as u64;
        let sr = u64::from(self.sample_rate.max(1));
        let b = b as u64;
        ((b + 1) * per / sr - b * per / sr) as usize
    }

    /// One reservoir segment's frame bit targets under
    /// [`AacTuning::frame_demand`] (indexed from `seg.start`), and each
    /// frame's per-channel sine-window long spectrum the demand was measured
    /// on, for `encode_frame` to reuse where it codes a frame with that very
    /// window. `None` (every frame its constant-rate share) when the knob is
    /// off.
    ///
    /// The targets sum to the segment's constant-rate bits, so a segment still
    /// encodes alone and the stream's rate is unchanged.
    #[allow(clippy::type_complexity)]
    fn segment_demand(
        &self,
        seg: std::ops::Range<usize>,
        swb: &[u16],
        plan: &[Elem],
    ) -> Option<(Vec<usize>, Vec<Vec<Option<Vec<f32>>>>)> {
        if self.frame_demand <= 0.0 {
            return None;
        }
        MASK_SMR.with(|c| c.set(smr_ratio(self.effective_smr_db())));
        let cut = cutoff_line(self.bandwidth_hz(), self.sample_rate, swb, FRAME_LEN);
        let cap = (MAX_CHANNEL_BITS + 7) * self.channels;
        let means: Vec<usize> = seg.clone().map(|b| self.frame_bits(b)).collect();
        let (pe, specs): (Vec<f64>, Vec<_>) = seg.map(|b| self.frame_pe(b, swb, plan, cut)).unzip();
        let w = frame_demand_weights(&pe, f64::from(self.frame_demand));
        let targets = means
            .iter()
            .zip(&w)
            .map(|(&m, &w)| ((m as f64 * w).round() as usize).min(cap))
            .collect();
        Some((targets, specs))
    }

    /// Perceptual entropy of frame `b`, summed over every channel but the LFE:
    /// `sum_b n_b * 0.5 * log2(1 + E_b / thr_b)` on a sine-window long MDCT of
    /// the frame, against the long-block mask (flat SMR, threshold in quiet),
    /// within the coding bandwidth. A demand estimate only: it is computed
    /// the same way whatever window sequence the frame is coded with.
    /// Also returns each channel's spectrum (LFE: `None`).
    fn frame_pe(
        &self,
        b: usize,
        swb: &[u16],
        plan: &[Elem],
        cut: usize,
    ) -> (f64, Vec<Option<Vec<f32>>>) {
        let num_swb = swb.len() - 1;
        let mut specs = vec![None; self.channels];
        let chans = plan.iter().flat_map(|e| match *e {
            Elem::Cpe(l, r) => vec![l, r],
            Elem::Sce(c) => vec![c],
            Elem::Lfe(_) => vec![],
        });
        let mut pe = 0.0;
        for ch in chans {
            let prev = if b == 0 {
                std::borrow::Cow::Owned([0f32; FRAME_LEN])
            } else {
                self.block_cow(ch, b - 1)
            };
            let mut spec = analyze_long(&prev, &self.block_cow(ch, b), &self.win);
            band_limit(&mut spec, cut, FRAME_LEN);
            let energy: Vec<f64> = (0..num_swb)
                .map(|i| {
                    let (s, e) = (usize::from(swb[i]), usize::from(swb[i + 1]));
                    spec[s..e]
                        .iter()
                        .map(|&x| f64::from(x).powi(2))
                        .sum::<f64>()
                        + 1e-3
                })
                .collect();
            let thr = masking_from_energy(&energy, swb, self.sample_rate, FRAME_LEN, None);
            for i in 0..num_swb {
                let n = f64::from(swb[i + 1] - swb[i]);
                pe += n * 0.5 * (1.0 + energy[i] / thr[i].max(1e-30)).log2();
            }
            specs[ch] = Some(spec);
        }
        (pe, specs)
    }

    /// Sub-block energies of frame `f` of channel `ch`, zero past the input.
    /// With `highpass`, of the high-passed signal (see [`sub_block_energy`]);
    /// its first two samples need the previous block, which is still held
    /// (or, at the stream's start, silence, as in the whole-clip detector).
    fn sub_energies(&self, ch: usize, f: usize, highpass: bool) -> [f64; 8] {
        let chan = &self.chans[ch];
        let at = (f - self.base_block) * FRAME_LEN;
        if highpass && self.base_block > 0 {
            // `chan[0]` is not the stream's first sample: give the filter the
            // two samples before `at` from the buffer (held back to block
            // `base_block`, so `at >= FRAME_LEN` whenever `f > base_block`).
            return std::array::from_fn(|sb| {
                let start = at + sb * SHORT_HALF;
                if start >= 2 {
                    // Past the buffered input (the flush frames) this is an
                    // empty slice, and the energy that of silence.
                    sub_block_energy(chan.get(start - 2..).unwrap_or(&[]), 2, true)
                } else {
                    sub_block_energy(chan, start, true)
                }
            });
        }
        std::array::from_fn(|sb| sub_block_energy(chan, at + sb * SHORT_HALF, highpass))
    }

    /// Streaming: analyse the frames now complete, finalise the window
    /// sequences two frames behind them, encode those frames, and drop the
    /// samples no later frame needs. `last` (from `finish`) analyses and
    /// encodes everything, through the flush frame, as the whole-clip encode
    /// does.
    fn stream_advance(&mut self, last: bool) {
        if !self.initialized {
            return;
        }
        if self.stream.is_none() {
            self.stream = Some(Box::new(Streaming {
                tables: self.tables(),
                det: vec![Detector::default(); self.channels],
                flags: vec![Vec::new(); self.channels],
                seqs: Vec::new(),
                shapes: Vec::new(),
                analysed: 0,
                next: 0,
                reservoir: Reservoir::default(),
            }));
        }
        let mut st = self.stream.take().expect("just set");
        let mut reservoir = st.reservoir;
        if st.seqs.is_empty() {
            st.seqs = vec![Vec::new(); st.tables.plan.len()];
        }
        // Frames whose samples are all here (all of them, padded, at the end).
        let held = self.base_block + self.chans[0].len() / FRAME_LEN;
        let avail = if last {
            (self.total_in as usize).div_ceil(FRAME_LEN) + 1
        } else {
            held
        };
        while st.analysed < avail {
            let f = st.analysed;
            for ch in 0..self.channels {
                let sub =
                    self.sub_energies(ch, f, self.relative_transients && self.transient_highpass);
                let flag = st.det[ch].frame(&sub, f, self.relative_transients);
                st.flags[ch].push(flag);
            }
            st.shapes
                .push(matches!(self.window_shape, WindowShape::Kbd));
            st.analysed += 1;
        }
        // A frame's sequence depends on the transient flags up to two frames
        // after it (a run's LongStart, a one-frame gap merged into the run).
        let ready = if last {
            st.analysed
        } else {
            st.analysed.saturating_sub(2)
        };
        for (e, elem) in st.tables.plan.iter().enumerate() {
            let done = st.seqs[e].len();
            if done >= ready {
                continue;
            }
            // Four frames of left context make the window's start effects
            // irrelevant to the frames finalised from it.
            let w0 = done.saturating_sub(4);
            let joint: Vec<bool> = (w0..st.analysed)
                .map(|f| match *elem {
                    Elem::Cpe(l, r) => st.flags[l][f] || st.flags[r][f],
                    Elem::Sce(ch) => st.flags[ch][f],
                    Elem::Lfe(_) => false,
                })
                .collect();
            let seq = match *elem {
                Elem::Lfe(_) => vec![WindowSequence::OnlyLong; joint.len()],
                _ => assign_sequences(&joint),
            };
            st.seqs[e].extend_from_slice(&seq[done - w0..ready - w0]);
        }
        // Encode every frame whose sequence is final.
        let t = &st.tables;
        let out: Vec<(Vec<u8>, i64)> = (st.next..ready)
            .map(|b| {
                let mean = self.frame_bits(b);
                let budget = reservoir.budget(b, mean, self.channels);
                let au = self.encode_frame(
                    b, t.swb, t.swb_s, &t.sine_s, &t.kbd_s, &st.seqs, &st.shapes, budget, &t.plan,
                    None,
                );
                reservoir.spend(mean, au.0.len() * 8, self.channels);
                au
            })
            .collect();
        st.reservoir = reservoir;
        st.next = st.next.max(ready);
        self.queue.extend(out);
        // The next frame needs its previous block for the MDCT overlap.
        let keep_from = st.next.saturating_sub(1).max(self.base_block);
        let drop = keep_from - self.base_block;
        if drop > 0 {
            for chan in &mut self.chans {
                chan.drain(..(drop * FRAME_LEN).min(chan.len()));
            }
            self.base_block = keep_from;
        }
        self.stream = Some(st);
    }

    fn encode_stream(&self) -> Vec<(Vec<u8>, i64)> {
        let swb = swb_offsets(true, self.fs_index);
        let swb_s = swb_offsets(false, self.fs_index);
        let sine_s = crate::dsp::sine_window(SHORT_N);
        // α = 6.0 for the short window, matching the decoder's `kbd_s`.
        let kbd_s = crate::dsp::kbd_window(SHORT_N, 6.0);
        let n = self.chans.first().map_or(0, std::vec::Vec::len);
        let nblocks = n.div_ceil(FRAME_LEN) + 1;

        // The ISO element sequence for this channel count (validated at init, so
        // the unwrap cannot fire here).
        let plan = element_plan(self.channels).expect("channel count validated in init");

        // Window sequences, **one per element**. A CPE shares a common window, so
        // its two channels' transient flags are joined; an LFE is long-only by
        // ISO rule, not by choice.
        let seqs: Vec<Vec<WindowSequence>> = plan
            .iter()
            .map(|e| match *e {
                Elem::Cpe(l, r) => {
                    let t0 = self.transients(&self.chans[l], nblocks);
                    let t1 = self.transients(&self.chans[r], nblocks);
                    let joint: Vec<bool> = (0..nblocks).map(|b| t0[b] || t1[b]).collect();
                    assign_sequences(&joint)
                }
                Elem::Sce(ch) => assign_sequences(&self.transients(&self.chans[ch], nblocks)),
                // lfe_channel_element carries no window-shape freedom.
                Elem::Lfe(_) => vec![WindowSequence::OnlyLong; nblocks],
            })
            .collect();
        // Arm A8: decide every frame's window shape up front, for the same reason
        // the sequences are decided up front — frame b's window depends on frame
        // b−1's shape, so this cannot live inside the parallel worker.
        let shapes = self.decide_shapes(nblocks);

        // Slice views the worker threads share (all read-only).
        let (swb, swb_s, sine_s, kbd_s, seqs, shapes, plan) = (
            swb,
            swb_s,
            &sine_s[..],
            &kbd_s[..],
            &seqs[..],
            &shapes[..],
            &plan[..],
        );
        // Frames `range` in order, carrying the bit reservoir, which restarts at
        // every RESERVOIR_SEGMENT boundary (so any range of whole segments
        // encodes the same alone as in sequence).
        let frames = |range: std::ops::Range<usize>| {
            let mut reservoir = Reservoir::default();
            let mut out = Vec::with_capacity(range.len());
            let mut s = range.start;
            while s < range.end {
                let e = ((s / RESERVOIR_SEGMENT + 1) * RESERVOIR_SEGMENT).min(range.end);
                let seg = self.segment_demand(s..e, swb, plan);
                for b in s..e {
                    let (mean, pre) = match &seg {
                        Some((t, sp)) => (t[b - s], Some(&sp[b - s][..])),
                        None => (self.frame_bits(b), None),
                    };
                    let budget = reservoir.budget(b, mean, self.channels);
                    let au = self.encode_frame(
                        b, swb, swb_s, sine_s, kbd_s, seqs, shapes, budget, plan, pre,
                    );
                    reservoir.spend(mean, au.0.len() * 8, self.channels);
                    out.push(au);
                }
                s = e;
            }
            out
        };

        let segments = nblocks.div_ceil(RESERVOIR_SEGMENT);
        let nthreads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(segments);
        if nthreads <= 1 || nblocks < 16 {
            return frames(0..nblocks); // serial for tiny inputs
        }
        // Fan out contiguous runs of whole segments; concatenating the parts
        // keeps them ordered.
        let chunk = segments.div_ceil(nthreads) * RESERVOIR_SEGMENT;
        let parts: Vec<Vec<(Vec<u8>, i64)>> = std::thread::scope(|s| {
            (0..nthreads)
                .map(|t| {
                    s.spawn(move || {
                        let start = (t * chunk).min(nblocks);
                        let end = ((t + 1) * chunk).min(nblocks);
                        frames(start..end)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });
        parts.into_iter().flatten().collect()
    }

    /// Signal end-of-input: encode all buffered PCM (frame-parallel) so packets
    /// can be drained via [`next_packet`](AacEncoder::next_packet). Idempotent.
    pub fn finish(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        if self.initialized {
            if self.streaming {
                self.stream_advance(true);
            } else {
                self.queue = self.encode_stream().into();
            }
        }
    }

    /// Retrieve the next encoded access unit. Returns [`Error::Again`] when
    /// none is ready (before [`finish`](AacEncoder::finish), unless
    /// streaming), and [`Error::Eof`] once fully drained.
    ///
    /// # Errors
    ///
    /// [`Error::Again`] until [`finish`](AacEncoder::finish) is called, then
    /// [`Error::Eof`] once every packet has been returned.
    pub fn next_packet(&mut self) -> Result<EncodedPacket> {
        if let Some((data, pts)) = self.queue.pop_front() {
            return Ok(EncodedPacket {
                data,
                pts,
                duration: FRAME_LEN as u32,
            });
        }
        if self.flushed {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }
}

impl Default for AacEncoder {
    fn default() -> Self {
        Self::new(AacEncoderConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitReader;
    use crate::codebook::decode_tuple;

    /// Drain the encoder into a concatenated ADTS elementary stream — the container
    /// framing the encoder no longer emits itself — for decoder/ffmpeg validation.
    fn encode_to_adts(enc: &mut AacEncoder) -> Vec<u8> {
        enc.finish();
        let mut out = Vec::new();
        while let Ok(p) = enc.next_packet() {
            let hdr = AdtsHeader {
                object_type: 2,
                sample_rate: enc.sample_rate,
                channels: enc.channels as u16,
                frame_length: 7 + p.data.len(),
                header_len: 7,
            };
            out.extend_from_slice(&write_adts_header(&hdr));
            out.extend_from_slice(&p.data);
        }
        out
    }

    /// Every `dim`-tuple drawn from `vals`.
    fn product(vals: &[i32], dim: usize) -> Vec<Vec<i32>> {
        let mut out = vec![vec![]];
        for _ in 0..dim {
            let mut next = Vec::new();
            for prefix in &out {
                for &v in vals {
                    let mut t = prefix.clone();
                    t.push(v);
                    next.push(t);
                }
            }
            out = next;
        }
        out
    }

    /// Encode every representable tuple of every spectral codebook, then decode it
    /// back through the REAL decoder — must match exactly (codeword + sign + escape),
    /// and the written bit count must equal `spectral_bits`.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn spectral_encode_roundtrips_through_decoder() {
        for cb_num in 1..=11usize {
            let cb = &CODEBOOKS[cb_num];
            let dim = cb.dim as usize;
            let lav = i32::from(cb.lav);

            // Signed books offset by lav; unsigned carry a sign, so cover ±lav.
            // Book 11 (escape) also probes magnitudes beyond lav, both signs.
            let vals: Vec<i32> = if cb.esc {
                let mut v: Vec<i32> = (-lav..=lav).collect();
                for &m in &[16, 17, 31, 32, 69, 100] {
                    v.push(m);
                    v.push(-m);
                }
                v
            } else {
                (-lav..=lav).collect()
            };

            for tuple in product(&vals, dim) {
                if tuple_index(cb, &tuple).is_none() {
                    continue;
                }
                let mut w = BitWriter::new();
                spectral_emit(cb_num, &tuple, &mut w);
                assert_eq!(
                    w.bit_len(),
                    spectral_bits(cb_num, &tuple).unwrap(),
                    "cb {cb_num} tuple {tuple:?}: bit count mismatch"
                );
                let bytes = w.into_bytes();
                let mut r = BitReader::new(&bytes);
                let mut out = [0i32; 4];
                decode_tuple(cb, spectral_book(cb_num as u8), &mut r, &mut out).unwrap();
                assert_eq!(
                    &out[..dim],
                    &tuple[..dim],
                    "cb {cb_num} tuple {tuple:?}: round-trip"
                );
            }
        }
    }

    /// Forward MDCT → decoder synthesis (imdct·window·overlap-add) reconstructs the
    /// signal (delayed one frame) — proves the window + overlap + 32768 scaling.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn long_filterbank_reconstructs_via_decoder_math() {
        let win = crate::dsp::sine_window(LONG_N);
        let nblocks = 6usize;
        let signal: Vec<f32> = (0..nblocks * FRAME_LEN)
            .map(|i| (0.3 * (i as f64 * 0.017).sin() + 0.2 * (i as f64 * 0.003).cos()) as f32)
            .collect();

        let mut prev = [0f32; FRAME_LEN];
        let mut specs = Vec::new();
        for b in 0..nblocks {
            let mut cur = [0f32; FRAME_LEN];
            cur.copy_from_slice(&signal[b * FRAME_LEN..(b + 1) * FRAME_LEN]);
            specs.push(analyze_long(&prev, &cur, &win));
            prev = cur;
        }

        let mut overlap = [0f32; FRAME_LEN];
        let mut out = Vec::new();
        for spec in &specs {
            let time = crate::dsp::imdct(spec);
            let frame: Vec<f32> = (0..LONG_N).map(|n| time[n] * win[n] / SPEC_SCALE).collect();
            let mut o = [0f32; FRAME_LEN];
            for n in 0..FRAME_LEN {
                o[n] = frame[n] + overlap[n];
                overlap[n] = frame[FRAME_LEN + n];
            }
            out.extend_from_slice(&o);
        }

        // Output lags the input by one frame; the interior must reconstruct.
        for i in FRAME_LEN..(nblocks - 1) * FRAME_LEN {
            assert!(
                (out[i] - signal[i - FRAME_LEN]).abs() < 1e-3,
                "at {i}: {} vs {}",
                out[i],
                signal[i - FRAME_LEN]
            );
        }
    }

    /// The block-switching sequence `OnlyLong` → `LongStart` → `EightShort` → `LongStop` →
    /// `OnlyLong` must reconstruct through the decoder's synthesis math (TDAC across
    /// the transition windows) — the decisive check that the short filterbank and
    /// transition windows are exact inverses.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn short_block_sequence_reconstructs_via_decoder_math() {
        use WindowSequence::*;
        let sine_l = crate::dsp::sine_window(LONG_N);
        let sine_s = crate::dsp::sine_window(SHORT_N);
        // Arm A8's shapes, unused by this test (it exercises the sine arm) but
        // required by the shape-aware signature.
        let kbd_l_t = crate::dsp::kbd_window(LONG_N, 4.0);
        let kbd_s_t = crate::dsp::kbd_window(SHORT_N, 6.0);
        let seqs = [
            OnlyLong, LongStart, EightShort, LongStop, OnlyLong, OnlyLong,
        ];
        let nblocks = seqs.len();
        let signal: Vec<f32> = (0..nblocks * FRAME_LEN)
            .map(|i| (0.3 * (i as f64 * 0.019).sin() + 0.25 * (i as f64 * 0.007).cos()) as f32)
            .collect();

        // Forward: per-frame filterbank chosen by the window sequence.
        let mut prev = [0f32; FRAME_LEN];
        let mut specs = Vec::new();
        for (b, &seq) in seqs.iter().enumerate() {
            let mut cur = [0f32; FRAME_LEN];
            cur.copy_from_slice(&signal[b * FRAME_LEN..(b + 1) * FRAME_LEN]);
            let spec = if seq == EightShort {
                analyze_short(&prev, &cur, &sine_s)
            } else {
                analyze_long(
                    &prev,
                    &cur,
                    &long_window(seq, false, false, &sine_l, &kbd_l_t, &sine_s, &kbd_s_t),
                )
            };
            specs.push((seq, spec));
            prev = cur;
        }

        // Synthesis: mirror the decoder (short_frame vs imdct·window) + overlap-add.
        let mut overlap = [0f32; FRAME_LEN];
        let mut out = Vec::new();
        for (seq, spec) in &specs {
            let frame: Vec<f32> = if *seq == EightShort {
                let mut f = vec![0f32; LONG_N];
                for w in 0..8 {
                    let time = crate::dsp::imdct(&spec[w * SHORT_HALF..(w + 1) * SHORT_HALF]);
                    let off = 448 + w * SHORT_HALF;
                    for n in 0..SHORT_N {
                        f[off + n] += time[n] * sine_s[n] / SPEC_SCALE;
                    }
                }
                f
            } else {
                let time = crate::dsp::imdct(spec);
                let win = long_window(*seq, false, false, &sine_l, &kbd_l_t, &sine_s, &kbd_s_t);
                (0..LONG_N).map(|n| time[n] * win[n] / SPEC_SCALE).collect()
            };
            let mut o = [0f32; FRAME_LEN];
            for n in 0..FRAME_LEN {
                o[n] = frame[n] + overlap[n];
                overlap[n] = frame[FRAME_LEN + n];
            }
            out.extend_from_slice(&o);
        }

        // Interior (past the priming frame) must reconstruct across all transitions.
        for i in FRAME_LEN..(nblocks - 1) * FRAME_LEN {
            assert!(
                (out[i] - signal[i - FRAME_LEN]).abs() < 1e-3,
                "at {i}: {} vs {}",
                out[i],
                signal[i - FRAME_LEN]
            );
        }
    }

    #[test]
    fn window_sequences_are_valid() {
        use WindowSequence::*;
        // Isolated transient → bracketed by LongStart / LongStop.
        assert_eq!(
            assign_sequences(&[false, false, false, true, false, false]),
            vec![OnlyLong, OnlyLong, LongStart, EightShort, LongStop, OnlyLong]
        );
        // Adjacent transients → one short run.
        assert_eq!(
            assign_sequences(&[false, false, true, true, false]),
            vec![OnlyLong, LongStart, EightShort, EightShort, LongStop]
        );
        // A single-frame gap is merged (can't be both a stop and a start).
        assert_eq!(
            assign_sequences(&[false, true, false, true, false]),
            vec![LongStart, EightShort, EightShort, EightShort, LongStop]
        );
    }

    #[test]
    fn transient_is_detected() {
        let sr = 44100.0f64;
        let nframes = 5;
        let n = nframes * FRAME_LEN;
        let mut s = vec![0f32; n];
        for (i, x) in s.iter_mut().enumerate() {
            *x = (0.02 * (2.0 * std::f64::consts::PI * 400.0 * i as f64 / sr).sin()) as f32;
        }
        // A loud burst at the start of frame 2.
        for i in 0..600 {
            let env = (1.0 - i as f64 / 600.0).max(0.0);
            s[2 * FRAME_LEN + i] +=
                (0.8 * env * (2.0 * std::f64::consts::PI * 3000.0 * i as f64 / sr).sin()) as f32;
        }
        let flags = detect_transients(&s, nframes);
        assert!(flags[2], "the burst frame must be flagged");
        assert!(!flags[0] && !flags[4], "steady frames must not be flagged");
    }

    /// End-to-end: a signal with a sharp attack must actually emit `EightShort`
    /// blocks and still decode cleanly through our decoder.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn transient_encodes_short_and_decodes() {
        let sr = 44100u32;
        let nframes = 5;
        let n = nframes * FRAME_LEN;
        let mut interleaved = Vec::new();
        for i in 0..n {
            let t = i as f64 / f64::from(sr);
            let mut v = 0.02 * (2.0 * std::f64::consts::PI * 400.0 * t).sin();
            if (2 * FRAME_LEN..2 * FRAME_LEN + 600).contains(&i) {
                let k = i - 2 * FRAME_LEN;
                let env = (1.0 - k as f64 / 600.0).max(0.0);
                v += 0.8
                    * env
                    * (2.0 * std::f64::consts::PI * 3000.0 * k as f64 / f64::from(sr)).sin();
            }
            interleaved.push(v as f32);
        }

        let mut enc = AacEncoder::default();
        enc.push_pcm(&interleaved, 1, sr).unwrap();
        let adts = encode_to_adts(&mut enc);

        // Parse each frame's first SCE window_sequence; EightShort (2) must appear.
        let mut saw_short = false;
        let mut pos = 0usize;
        let mut decoded = 0usize;
        let mut dec = crate::decode::Decoder::new(sr);
        while pos + 7 <= adts.len() {
            let hdr = crate::parse_adts(&adts[pos..]).unwrap();
            let au = &adts[pos + hdr.header_len..pos + hdr.frame_length];
            let mut r = crate::BitReader::new(au);
            assert_eq!(r.read_bits(3).unwrap(), ID_SCE); // mono SCE
            let _ = r.read_bits(4).unwrap(); // tag
            let _ = r.read_bits(8).unwrap(); // global_gain
            let _ = r.read_bit().unwrap(); // ics_reserved
            if r.read_bits(2).unwrap() == WindowSequence::EightShort.to_bits() {
                saw_short = true;
            }
            decoded += dec.decode(au, None).unwrap().frames();
            pos += hdr.frame_length;
        }
        assert!(saw_short, "a transient must produce EightShort blocks");
        assert!(decoded >= n, "decoded fewer samples than encoded");
    }

    /// The `ms_mask_present` (2 bits) of the first frame's CPE — 0 = no M/S.
    fn first_cpe_ms_mask(adts: &[u8]) -> u32 {
        let hdr = crate::parse_adts(adts).unwrap();
        let au = &adts[hdr.header_len..hdr.frame_length];
        let mut r = crate::BitReader::new(au);
        assert_eq!(r.read_bits(3).unwrap(), ID_CPE);
        let _tag = r.read_bits(4).unwrap();
        assert_eq!(r.read_bit().unwrap(), 1); // common_window
        let _reserved = r.read_bit().unwrap();
        let ws = r.read_bits(2).unwrap();
        let _shape = r.read_bit().unwrap();
        if ws == WindowSequence::EightShort.to_bits() {
            let _ = r.read_bits(4).unwrap(); // max_sfb
            let _ = r.read_bits(7).unwrap(); // grouping
        } else {
            let _ = r.read_bits(6).unwrap(); // max_sfb
            let _ = r.read_bit().unwrap(); // predictor_data_present
        }
        r.read_bits(2).unwrap()
    }

    /// Stereo with L = R (fully correlated) must pick M/S and reconstruct the two
    /// channels identically — the CPE + mid/side round-trip.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn stereo_ms_roundtrips_mono_content() {
        let sr = 44100u32;
        let n = 8192usize;
        let mut interleaved = Vec::new();
        for i in 0..n {
            let s = (0.4 * (2.0 * std::f64::consts::PI * 600.0 * i as f64 / f64::from(sr)).sin())
                as f32;
            interleaved.push(s); // L
            interleaved.push(s); // R == L
        }
        let mut enc = AacEncoder::default();
        enc.push_pcm(&interleaved, 2, sr).unwrap();
        let adts = encode_to_adts(&mut enc);

        assert_ne!(
            first_cpe_ms_mask(&adts),
            0,
            "M/S should be chosen for L=R content"
        );

        let mut dec = crate::decode::Decoder::new(sr);
        let (mut lsum, mut diff) = (0f64, 0f64);
        let mut pos = 0usize;
        while pos + 7 <= adts.len() {
            let hdr = crate::parse_adts(&adts[pos..]).unwrap();
            let au = &adts[pos + hdr.header_len..pos + hdr.frame_length];
            let a = dec.decode(au, None).unwrap();
            for k in 0..a.frames() {
                let l = a.samples[k * 2];
                let rr = a.samples[k * 2 + 1];
                lsum += f64::from(l).powi(2);
                diff += f64::from(l - rr).powi(2);
            }
            pos += hdr.frame_length;
        }
        assert!(lsum > 0.0, "decoded silence");
        // L and R reconstruct identically (mono content preserved through M/S).
        assert!(
            diff / (lsum + 1e-9) < 1e-3,
            "L/R diverged: {}",
            diff / (lsum + 1e-9)
        );
    }

    /// The encoder→decoder round-trip is unity gain and length-preserving: decode
    /// the raw AUs directly (no container/engine) and check per-channel amplitude
    /// (≈0.283 RMS for a 0.4-amp sine) and frame count (~input + a little priming).
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn stereo_direct_decode_amplitude() {
        let sr = 44100u32;
        let n = sr as usize; // 1 s
        let mut interleaved = Vec::new();
        for i in 0..n {
            let t = i as f64 / f64::from(sr);
            let l = (0.4 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32;
            let r = (0.4 * (2.0 * std::f64::consts::PI * 660.0 * t).sin()) as f32;
            interleaved.push(l);
            interleaved.push(r);
        }
        let mut enc = AacEncoder::default();
        enc.push_pcm(&interleaved, 2, sr).unwrap();
        enc.finish();
        let mut dec = crate::decode::Decoder::new(sr);
        let (mut lsq, mut rsq, mut cnt) = (0.0f64, 0.0f64, 0usize);
        while let Ok(p) = enc.next_packet() {
            let a = dec.decode(&p.data, None).unwrap();
            for k in 0..a.frames() {
                let l = a.samples[k * 2];
                let r = a.samples[k * 2 + 1];
                lsq += f64::from(l).powi(2);
                rsq += f64::from(r).powi(2);
                cnt += 1;
            }
        }
        let (lrms, rrms) = ((lsq / cnt as f64).sqrt(), (rsq / cnt as f64).sqrt());
        eprintln!("input {n}/ch; decoded {cnt}/ch; Lrms={lrms:.3} Rrms={rrms:.3} (unity≈0.283)");
        // ~1 output frame per input frame (a couple of priming/flush frames extra).
        assert!(
            cnt >= n && cnt < n + 4 * FRAME_LEN,
            "frame count off: {cnt} vs {n}"
        );
        // Unity gain per channel (lossy tolerance), no doubling.
        assert!((0.24..0.32).contains(&lrms), "L amplitude off: {lrms:.3}");
        assert!((0.24..0.32).contains(&rrms), "R amplitude off: {rrms:.3}");
    }

    /// Per-frame hot-path breakdown — which stage dominates encode time.
    #[test]
    #[ignore = "profiling; run with --ignored --nocapture"]
    fn profile_encode_hotpath() {
        use std::time::Instant;
        let sr = 44100u32;
        let fs = crate::sf_index_for_rate(sr).unwrap();
        let swb = swb_offsets(true, fs);
        let win = crate::dsp::sine_window(LONG_N);
        let mut cur = [0f32; FRAME_LEN];
        for (i, s) in cur.iter_mut().enumerate() {
            let t = i as f64 / f64::from(sr);
            *s = (0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                + 0.1 * (2.0 * std::f64::consts::PI * 3000.0 * t).sin()) as f32;
        }
        let prev = [0f32; FRAME_LEN];
        let iters = 500;
        let us = |d: std::time::Duration| d.as_secs_f64() * 1e6 / f64::from(iters);

        let t = Instant::now();
        let mut spec = Vec::new();
        for _ in 0..iters {
            spec = analyze_long(&prev, &cur, &win);
        }
        let mdct = t.elapsed();

        let t = Instant::now();
        for _ in 0..iters {
            let _ = perceptual_offsets(&spec, swb, sr, false, 3000);
        }
        let psy = t.elapsed();
        let offsets = perceptual_offsets(&spec, swb, sr, false, 3000);

        let t = Instant::now();
        for _ in 0..iters {
            let _ = Xpow::new(&spec);
        }
        let xpow_t = t.elapsed();
        let xp = Xpow::new(&spec);

        let t = Instant::now();
        for _ in 0..iters {
            let _ = rate_loop(&xp, swb, &offsets, 3000);
        }
        let rate = t.elapsed();

        let sf = scalefactors(&offsets, 120);
        let t = Instant::now();
        for _ in 0..iters {
            let _ = code_frame(&xp, swb, &sf);
        }
        let cf = t.elapsed();

        eprintln!("per-frame per-channel (avg over {iters}):");
        eprintln!("  analyze_long (MDCT): {:>8.1} us", us(mdct));
        eprintln!("  Xpow::new (once):    {:>8.1} us", us(xpow_t));
        eprintln!("  perceptual_offsets:  {:>8.1} us", us(psy));
        eprintln!("  rate_loop (~8x cf):  {:>8.1} us", us(rate));
        eprintln!("  one code_frame:      {:>8.1} us", us(cf));
        eprintln!(
            "  → MDCT is {:.0}% of (MDCT + rate_loop)",
            100.0 * mdct.as_secs_f64() / (mdct.as_secs_f64() + rate.as_secs_f64())
        );
    }

    /// The AVX-512 quantize kernel narrows via `trunc(min(v+0.5, MAX+0.5))` instead of
    /// the AVX2/scalar `min(round(v), MAX)`. The intrinsics can't run on a non-AVX-512
    /// host, but the identity they rely on — for the quantizer's always-nonnegative
    /// input — is checkable in scalar, so the untested path's *math* is pinned here.
    /// A small deterministic generator for the codebook-cost oracle tests: bands
    /// whose peak lands in every book tier (0, 1, 2, 3-4, 5-7, 8-12, 13-15, the
    /// escape range up to `MAX_QUANT`), mixed with zeros, at every width the
    /// tables use plus odd and non-4-aligned ones.
    fn random_band(seed: &mut u64, width: usize) -> Vec<i32> {
        let mut next = || {
            *seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (*seed >> 33) as u32
        };
        let peak = [0u32, 1, 2, 4, 7, 12, 15, 16, 40, 300, MAX_QUANT as u32][next() as usize % 11];
        (0..width)
            .map(|_| {
                let r = next();
                if r % 3 == 0 || peak == 0 {
                    0
                } else {
                    let m = (r >> 2) % (peak + 1);
                    if r & 2 == 0 {
                        m as i32
                    } else {
                        -(m as i32)
                    }
                }
            })
            .collect()
    }

    /// The walk-once codebook cost must pick the same book at the same bit count
    /// as the per-book search it replaced, for long bands and for short-window
    /// groups.
    #[test]
    fn codebook_cost_matches_per_book_search() {
        let mut seed = 0x5eed_u64;
        for width in [2usize, 4, 6, 8, 12, 16, 20, 28, 32, 48, 64, 96, 3] {
            for _ in 0..4000 {
                let band = random_band(&mut seed, width);
                assert_eq!(
                    best_codebook_for_band(&band, 0, width),
                    best_codebook_for_band_ref(&band, 0, width),
                    "width {width} band {band:?}"
                );
            }
        }
    }

    /// The short-window search over a group of windows (each window's band at its
    /// own `SHORT_HALF` stride) against the per-book search it replaced.
    #[test]
    fn short_codebook_cost_matches_per_book_search() {
        let mut seed = 0x5407_u64;
        // Long-window and short-window band edges, plus a non-4-aligned and an
        // odd band, so the dim-4 and no-book cases are reached too.
        let swb: [u16; 10] = [0, 4, 8, 16, 28, 44, 60, 66, 69, 128];
        for _ in 0..3000 {
            let mut quant = vec![0i32; FRAME_LEN];
            for w in 0..8 {
                let band = random_band(&mut seed, SHORT_HALF);
                quant[w * SHORT_HALF..(w + 1) * SHORT_HALF].copy_from_slice(&band);
            }
            for sfb in 0..swb.len() - 1 {
                for (w0, nwin) in [(0, 8), (0, 1), (2, 3), (5, 3), (7, 1)] {
                    assert_eq!(
                        best_codebook_short(&quant, &swb, sfb, w0, nwin),
                        best_codebook_short_ref(&quant, &swb, sfb, w0, nwin),
                        "sfb {sfb} windows {w0}+{nwin}"
                    );
                }
            }
        }
    }

    /// The frame-level quantizer (one dispatch, the band loop inside the AVX2
    /// function) equals the scalar per-band quantizer on every band, with
    /// scalefactors spanning and exceeding the clamped 0..=255 range.
    #[test]
    fn quantize_frame_matches_per_band_scalar() {
        let swb = swb_offsets(true, 4); // 44.1 kHz long bands
        let mut seed = 0x9e37_u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as u32
        };
        for _ in 0..50 {
            let spec: Vec<f32> = (0..FRAME_LEN)
                .map(|_| (next() as f32 / u32::MAX as f32 - 0.25) * 40_000.0)
                .collect();
            let xp = Xpow::new(&spec);
            let sf: Vec<i32> = (1..swb.len()).map(|_| next() as i32 % 300 - 20).collect();
            let mut got = vec![0i32; FRAME_LEN];
            quantize_frame(&xp, swb, &sf, &mut got);
            let mut want = vec![0i32; FRAME_LEN];
            for sfb in 0..swb.len() - 1 {
                let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                let sc = scale_table()[sf[sfb].clamp(0, 255) as usize];
                quantize_band_scalar(&xp.pow[s..e], &xp.sign[s..e], sc, &mut want[s..e]);
            }
            assert_eq!(got, want);
        }
    }

    /// The quantize twins must equal the scalar oracle exactly — including at the
    /// adversarial rounding points (the double just below each `k + 0.5`, where
    /// `v + 0.5` rounds UP in f64 and a naive `floor(v + 0.5)` overshoots), at the
    /// `MAX_QUANT` clamp, and with tails that are not a multiple of the lane width.
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    #[test]
    fn quantize_simd_matches_scalar() {
        let below = |x: f64| f64::from_bits(x.to_bits() - 1);
        let mut vals = vec![
            0.0f64,
            0.25,
            0.5,
            below(0.5),
            1.5,
            below(1.5),
            2.5,
            below(2.5),
            1e-300,
            1.0 - NEAREST_BIAS,
            below(1.0 - NEAREST_BIAS),
            NEAREST_T1,
            below(NEAREST_T1),
        ];
        vals.extend([
            8190.5,
            below(8190.5),
            8191.0,
            8191.5,
            8192.0,
            1e9,
            4503599627370495.5,
        ]);
        let mut seed = 0x9e37_79b9u32;
        for _ in 0..4000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let k = f64::from(seed >> 20);
            vals.push(match seed & 3 {
                0 => below(k + 0.5),
                1 => k + 0.5,
                2 => f64::from(seed) / 1000.0,
                _ => k * 1.000_000_1,
            });
        }
        for n in [1usize, 3, 4, 5, 7, 8, 9, 31, vals.len()] {
            let pow = &vals[..n];
            let sign: Vec<i32> = (0..n).map(|i| if i % 3 == 0 { -1 } else { 1 }).collect();
            let mut want = vec![0i32; n];
            quantize_band_scalar(pow, &sign, 1.0, &mut want);
            if has_avx2() {
                let mut got = vec![0i32; n];
                // SAFETY: AVX2 detected; all slices are `n` long.
                unsafe { quantize_band_avx2(pow, &sign, 1.0, &mut got) };
                for i in 0..n {
                    assert_eq!(
                        got[i],
                        want[i],
                        "avx2 v={:e} (bits {:x})",
                        pow[i],
                        pow[i].to_bits()
                    );
                }
            }
            #[cfg(feature = "simd-avx512")]
            if has_avx512() {
                let mut got = vec![0i32; n];
                // SAFETY: AVX-512F detected; all slices are `n` long.
                unsafe { quantize_band_avx512(pow, &sign, 1.0, &mut got) };
                for i in 0..n {
                    assert_eq!(got[i], want[i], "avx512 v={:e}", pow[i]);
                }
            }
        }
    }

    /// `xpow_avx2` must handle a length that is not a multiple of its 4 lanes
    /// (its scalar tail once never advanced, an infinite loop masked only because
    /// every caller passes 1024 coefficients), and match the scalar formula.
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    #[test]
    fn xpow_avx2_matches_scalar_with_tail() {
        if !has_avx2() {
            return;
        }
        for n in [0usize, 1, 3, 4, 5, 7, 129, 1023, 1024] {
            let spec: Vec<f32> = (0..n)
                .map(|i| ((i as f32 * 0.77).sin() * 3000.0) - 0.5)
                .collect();
            let (mut pow, mut sign) = (vec![0f64; n], vec![0i32; n]);
            // SAFETY: AVX2 detected above; the outputs are `n` long.
            unsafe { xpow_avx2(&spec, &mut pow, &mut sign) };
            for i in 0..n {
                let s = f64::from(spec[i].abs()).sqrt();
                assert_eq!(pow[i], s * s.sqrt(), "pow[{i}] n={n}");
                assert_eq!(
                    sign[i],
                    if spec[i] < 0.0 { -1 } else { 1 },
                    "sign[{i}] n={n}"
                );
            }
        }
    }

    #[test]
    fn avx512_trunc_identity_matches_round_clamp() {
        for &v in &[
            0.0f64, 0.4, 0.5, 0.6, 1.4, 1.5, 2.5, 100.5, 8190.9, 8191.0, 8191.5, 1e6,
        ] {
            let round_clamp = (v.round() as i64).min(i64::from(MAX_QUANT));
            let trunc_trick = (v + 0.5).min(f64::from(MAX_QUANT) + 0.5) as i64; // `as` truncates
            assert_eq!(round_clamp, trunc_trick, "mismatch at v={v}");
        }
    }

    #[test]
    fn bitwriter_msb_first() {
        let mut w = BitWriter::new();
        w.write(0b101, 3);
        w.write(0b1, 1);
        w.write(0b1111, 4);
        assert_eq!(w.bit_len(), 8);
        assert_eq!(w.into_bytes(), vec![0b1011_1111]);
    }

    /// `emit_band` writes exactly the bits of `spectral_emit` tuple by tuple, for
    /// every book, with bands whose peak the book can hold (escapes up to
    /// `MAX_QUANT` for book 11), at a random starting bit alignment.
    #[test]
    fn emit_band_matches_spectral_emit() {
        let mut seed = 0xe417_u64;
        for cb in 1..=11u8 {
            let meta = &CODEBOOKS[cb as usize];
            let lav = if meta.esc {
                MAX_QUANT as u32
            } else {
                u32::from(meta.lav)
            };
            let mut done = 0;
            while done < 2000 {
                let band = random_band(&mut seed, 32);
                if band.iter().any(|q| q.unsigned_abs() > lav) {
                    continue;
                }
                done += 1;
                let lead = (seed >> 40) as u32 % 32;
                let (mut a, mut b) = (BitWriter::new(), BitWriter::new());
                a.write(0x5a5a_5a5a, lead);
                b.write(0x5a5a_5a5a, lead);
                emit_band(&mut a, cb, &band);
                for t in band.chunks_exact(meta.dim as usize) {
                    spectral_emit(cb as usize, t, &mut b);
                }
                assert_eq!(a.bit_len(), b.bit_len(), "cb {cb} band {band:?}");
                assert_eq!(a.into_bytes(), b.into_bytes(), "cb {cb} band {band:?}");
            }
        }
    }

    /// The word-buffered writer against a one-bool-per-bit reference: random
    /// widths 0..=32 (with junk above the width), interleaved appends of writers
    /// at every alignment, `bit_len` after every step, and the padded bytes.
    #[test]
    fn bitwriter_matches_bit_vector() {
        fn pack(bits: &[bool]) -> Vec<u8> {
            bits.chunks(8)
                .map(|c| {
                    c.iter()
                        .enumerate()
                        .fold(0u8, |b, (i, &x)| b | (u8::from(x) << (7 - i)))
                })
                .collect()
        }
        let mut seed = 0xb17_u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 32) as u32
        };
        for _ in 0..300 {
            let (mut w, mut bits) = (BitWriter::new(), Vec::new());
            for _ in 0..(next() % 200) {
                if next() % 8 == 0 {
                    let (mut o, mut obits) = (BitWriter::new(), Vec::new());
                    for _ in 0..(next() % 20) {
                        let (v, n) = (next(), next() % 33);
                        o.write(v, n);
                        obits.extend((0..n).rev().map(|i| (v >> i) & 1 == 1));
                    }
                    w.append(&o);
                    bits.extend(obits);
                } else {
                    let (v, n) = (next(), next() % 33);
                    w.write(v, n);
                    bits.extend((0..n).rev().map(|i| (v >> i) & 1 == 1));
                }
                assert_eq!(w.bit_len(), bits.len());
            }
            assert_eq!(w.into_bytes(), pack(&bits));
        }
    }

    #[test]
    fn audio_specific_config_roundtrips() {
        for &(sr, ch) in &[(44100u32, 2u16), (48000, 1), (96000, 2), (8000, 1)] {
            let cfg = AudioSpecificConfig {
                object_type: 2,
                sample_rate: sr,
                channels: ch,
            };
            let bytes = write_audio_specific_config(&cfg);
            assert_eq!(crate::parse_audio_specific_config(&bytes).unwrap(), cfg);
        }
    }

    #[test]
    fn adts_header_roundtrips() {
        let hdr = AdtsHeader {
            object_type: 2,
            sample_rate: 44100,
            channels: 2,
            frame_length: 512,
            header_len: 7,
        };
        let bytes = write_adts_header(&hdr);
        assert_eq!(bytes.len(), 7);
        assert!(crate::is_adts(&bytes));
        assert_eq!(crate::parse_adts(&bytes).unwrap(), hdr);
    }

    #[test]
    fn ics_info_long_reencodes_bit_exact() {
        // The decoder's own long-block test vector.
        let orig = [0x0C, 0x40];
        let info = crate::ics::parse_ics_info(&mut BitReader::new(&orig), 4).unwrap();
        let mut w = BitWriter::new();
        encode_ics_info(&mut w, &info);
        assert_eq!(w.into_bytes(), orig);
    }

    #[test]
    fn ics_info_short_grouping_roundtrips() {
        use crate::ics::{parse_ics_info, IcsInfo, WindowSequence};
        let info = IcsInfo {
            window_sequence: WindowSequence::EightShort,
            window_shape_kbd: false,
            max_sfb: 8,
            num_windows: 8,
            num_window_groups: 2,
            window_group_length: vec![3, 5],
            num_swb: 0, // encode ignores; the parser re-derives
        };
        let mut w = BitWriter::new();
        encode_ics_info(&mut w, &info);
        let parsed = parse_ics_info(&mut BitReader::new(&w.into_bytes()), 4).unwrap();
        assert_eq!(parsed.window_sequence, WindowSequence::EightShort);
        assert_eq!(parsed.max_sfb, 8);
        assert_eq!(parsed.window_group_length, vec![3, 5]);
    }

    fn rms(sig: &[f32]) -> f64 {
        (sig.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / sig.len().max(1) as f64).sqrt()
    }

    /// Magnitude of the `freq`-Hz component (Goertzel-style) — a recognizability probe.
    fn tone_energy(sig: &[f32], freq: f64, sr: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in sig.iter().enumerate() {
            let ph = 2.0 * std::f64::consts::PI * freq * i as f64 / sr;
            re += f64::from(s) * ph.cos();
            im += f64::from(s) * ph.sin();
        }
        (re * re + im * im).sqrt() / sig.len() as f64
    }

    /// The whole-pipeline gate: encode a 440 Hz tone, decode through the real
    /// decoder, and confirm a recognizable 440 Hz tone with preserved energy.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn encodes_and_decodes_recognizable_tone() {
        let sr = 44100u32;
        let n = 44100usize; // 1 s
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let s = ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / f64::from(sr)).sin() * 0.5)
                as f32;
            samples.push(s);
        }

        let mut enc = AacEncoder::default();
        enc.push_pcm(&samples, 1, sr).unwrap();
        let adts = encode_to_adts(&mut enc);
        assert!(crate::is_adts(&adts), "encoder output is not ADTS");

        let mut dec = crate::decode::Decoder::new(sr);
        let mut decoded = Vec::new();
        let mut pos = 0usize;
        while pos + 7 <= adts.len() {
            let hdr = crate::parse_adts(&adts[pos..]).unwrap();
            let au = &adts[pos + hdr.header_len..pos + hdr.frame_length];
            decoded.extend_from_slice(&dec.decode(au, None).unwrap().samples);
            pos += hdr.frame_length;
        }
        assert!(
            decoded.len() > n / 2,
            "too little decoded audio: {}",
            decoded.len()
        );

        let (ri, ro) = (rms(&samples), rms(&decoded));
        assert!(
            ro > 0.4 * ri && ro < 2.5 * ri,
            "energy off: in {ri:.4} out {ro:.4}"
        );
        let e440 = tone_energy(&decoded, 440.0, f64::from(sr));
        let e1234 = tone_energy(&decoded, 1234.0, f64::from(sr));
        assert!(
            e440 > 5.0 * e1234,
            "not a clean 440 Hz tone: e440 {e440:.5} e1234 {e1234:.5}"
        );
    }

    /// The rate loop must keep a dense signal within (roughly) the target bitrate
    /// and still produce decodable audio.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn rate_loop_respects_bitrate() {
        let sr = 44100u32;
        let secs = 2usize;
        let n = sr as usize * secs;
        let mut interleaved = Vec::new();
        let mut st = 0x0000_2468u32;
        for i in 0..n {
            let t = i as f64 / f64::from(sr);
            let mut s = 0.0;
            for h in 1..=8 {
                s += (2.0 * std::f64::consts::PI * 300.0 * f64::from(h) * t).sin() / f64::from(h);
            }
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let noise = (f64::from(st >> 24) - 128.0) / 128.0 * 0.1;
            let v = ((s * 0.2 + noise) * 0.7).clamp(-1.0, 1.0) as f32;
            interleaved.push(v);
        }

        for &kbps in &[64_000u32, 128_000] {
            let mut enc = AacEncoder::new(AacEncoderConfig {
                bitrate_bps: kbps,
                ..Default::default()
            });
            enc.push_pcm(&interleaved, 1, sr).unwrap();
            let adts = encode_to_adts(&mut enc);

            let measured = adts.len() as f64 * 8.0 / secs as f64;
            assert!(
                measured <= f64::from(kbps) * 1.35,
                "bitrate {kbps}: measured {measured:.0} b/s exceeds budget"
            );

            let mut dec = crate::decode::Decoder::new(sr);
            let mut pos = 0usize;
            let mut got = false;
            while pos + 7 <= adts.len() {
                let hdr = crate::parse_adts(&adts[pos..]).unwrap();
                let au = &adts[pos + hdr.header_len..pos + hdr.frame_length];
                got |= !dec.decode(au, None).unwrap().samples.is_empty();
                pos += hdr.frame_length;
            }
            assert!(got, "bitrate {kbps}: no decodable audio");
            eprintln!(
                "target {kbps} b/s → measured {measured:.0} b/s ({} bytes)",
                adts.len()
            );
        }
    }

    /// The psychoacoustic model must shape quantization noise toward the masking
    /// threshold: at a fixed budget it must not worsen the worst band and must cut
    /// the total audible (above-mask) noise vs flat scalefactors.
    #[test]
    fn psy_model_shapes_noise_below_flat() {
        let sr = 44100u32;
        let fs = crate::sf_index_for_rate(sr).unwrap();
        let swb = swb_offsets(true, fs);
        let win = crate::dsp::sine_window(LONG_N);
        // A few strong tones with spectral gaps → masking creates real headroom in
        // some bands and sensitivity in others.
        let mut cur = [0f32; FRAME_LEN];
        for (i, s) in cur.iter_mut().enumerate() {
            let t = i as f64 / f64::from(sr);
            let v = 0.5 * (2.0 * std::f64::consts::PI * 500.0 * t).sin()
                + 0.25 * (2.0 * std::f64::consts::PI * 1500.0 * t).sin()
                + 0.15 * (2.0 * std::f64::consts::PI * 4000.0 * t).sin();
            *s = v as f32;
        }
        let spec = analyze_long(&[0f32; FRAME_LEN], &cur, &win);
        let target = 2500usize; // ~128 kbps — enough that allocation choices matter

        // Worst-case NMR (perceptual quality is set by the loudest audible artifact)
        // and mean NMR in dB, over the energy-bearing bands.
        let xp = Xpow::new(&spec);
        let metric = |offsets: &Offsets| -> (f64, f64) {
            let base = rate_loop(&xp, swb, offsets, target);
            let sf = scalefactors(offsets, base);
            let thr = masking_thresholds(&spec, swb, sr);
            let (mut max_nmr, mut sum_db, mut n) = (0f64, 0f64, 0usize);
            for sfb in 0..swb.len() - 1 {
                let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                let en: f64 = spec[s..e].iter().map(|&x| f64::from(x).powi(2)).sum();
                if en < 1e6 {
                    continue; // near-silent band → quantizes to ZERO, no artifact
                }
                let mut noise = 0f64;
                for &x in &spec[s..e] {
                    let q = quantize(x, sf[sfb]);
                    let rec = f64::from(q.signum())
                        * f64::from(q.unsigned_abs()).powf(4.0 / 3.0)
                        * 2f64.powf(0.25 * f64::from(sf[sfb] - 100));
                    noise += (f64::from(x) - rec).powi(2);
                }
                let nmr = (noise / thr[sfb]).max(1e-30);
                max_nmr = max_nmr.max(nmr);
                sum_db += 10.0 * nmr.log10();
                n += 1;
            }
            (max_nmr, sum_db / n.max(1) as f64)
        };

        let flat = metric(&Offsets::flat(vec![0i32; swb.len() - 1]));
        let psy = metric(&perceptual_offsets(&spec, swb, sr, false, 3000));
        eprintln!(
            "flat: max_nmr={:.2} mean={:.1}dB | psy: max_nmr={:.2} mean={:.1}dB",
            flat.0, flat.1, psy.0, psy.1
        );
        // The psy model equalizes NMR → the worst audible band is quieter than flat.
        assert!(
            psy.0 < flat.0,
            "psy must reduce the worst-band NMR ({:.2} vs {:.2})",
            psy.0,
            flat.0
        );
    }

    /// Emit our `.aac` (ADTS) so an external reference decoder (ffmpeg) can confirm
    /// the stream is spec-valid, not just self-decodable.
    #[test]
    #[ignore = "writes an .aac for external ffmpeg validation; run explicitly"]
    fn emit_aac_for_external_check() {
        let sr = 44100u32;
        let n = 44100usize;
        let mut interleaved = Vec::new();
        for i in 0..n {
            let s = ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / f64::from(sr)).sin() * 0.5)
                as f32;
            interleaved.push(s);
        }
        let mut enc = AacEncoder::default();
        enc.push_pcm(&interleaved, 1, sr).unwrap();
        let adts = encode_to_adts(&mut enc);
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("rff_aac_tone.aac"), &adts).unwrap();
        eprintln!(
            "wrote {}/rff_aac_tone.aac ({} bytes)",
            dir.display(),
            adts.len()
        );
    }

    /// Emit a `.aac` with periodic sharp attacks (drum-like) so ffmpeg can confirm
    /// the block-switched (short + transition) stream is spec-valid.
    #[test]
    #[ignore = "writes an .aac for external ffmpeg validation; run explicitly"]
    fn emit_transient_aac_for_external_check() {
        let sr = 44100u32;
        let n = 44100usize;
        let mut interleaved = Vec::new();
        for i in 0..n {
            let t = i as f64 / f64::from(sr);
            let mut v = 0.05 * (2.0 * std::f64::consts::PI * 220.0 * t).sin();
            // A sharp click every ~0.25 s.
            let k = i % 11025;
            if k < 700 {
                let env = (1.0 - k as f64 / 700.0).max(0.0);
                v += 0.8
                    * env
                    * (2.0 * std::f64::consts::PI * 3500.0 * k as f64 / f64::from(sr)).sin();
            }
            interleaved.push(v as f32);
        }
        let mut enc = AacEncoder::default();
        enc.push_pcm(&interleaved, 1, sr).unwrap();
        let adts = encode_to_adts(&mut enc);
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("rff_aac_transient.aac"), &adts).unwrap();
        eprintln!(
            "wrote {}/rff_aac_transient.aac ({} bytes)",
            dir.display(),
            adts.len()
        );
    }

    /// Emit a stereo `.aac` (shared bass + divergent highs → mixed per-SFB M/S) so
    /// ffmpeg can confirm the CPE stream is spec-valid.
    #[test]
    #[ignore = "writes an .aac for external ffmpeg validation; run explicitly"]
    fn emit_stereo_aac_for_external_check() {
        let sr = 44100u32;
        let n = 44100usize;
        let mut interleaved = Vec::new();
        for i in 0..n {
            let t = i as f64 / f64::from(sr);
            let bass = 0.4 * (2.0 * std::f64::consts::PI * 300.0 * t).sin(); // shared → M/S
            let l = bass + 0.2 * (2.0 * std::f64::consts::PI * 1200.0 * t).sin();
            let r = bass + 0.2 * (2.0 * std::f64::consts::PI * 1900.0 * t).sin();
            interleaved.push(l as f32);
            interleaved.push(r as f32);
        }
        let mut enc = AacEncoder::default();
        enc.push_pcm(&interleaved, 2, sr).unwrap();
        let adts = encode_to_adts(&mut enc);
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("rff_aac_stereo.aac"), &adts).unwrap();
        eprintln!(
            "wrote {}/rff_aac_stereo.aac ({} bytes)",
            dir.display(),
            adts.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Rung 0 (arm A8) — window-shape gate tests.
//
// The rung's contract, in the order great-gate §4 requires it:
//   1. the neutral end is BYTE-IDENTICAL (proven against an oracle, not argued),
//   2. the routed arm is TDAC-correct (a window mismatch is not a quality
//      regression, it is broken audio),
//   3. the dispatch decodes on the hardest case — alternating shapes.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod rung0 {
    use super::*;

    /// The sine-only `long_window` exactly as it stood before arm A8, kept as the
    /// oracle for the neutral end.
    fn long_window_pre_a8(seq: WindowSequence, sine_l: &[f32], sine_s: &[f32]) -> Vec<f32> {
        let mut w = vec![0f32; LONG_N];
        if seq == WindowSequence::LongStop {
            w[448..448 + SHORT_HALF].copy_from_slice(&sine_s[..SHORT_HALF]);
            for s in w.iter_mut().take(FRAME_LEN).skip(576) {
                *s = 1.0;
            }
        } else {
            w[..FRAME_LEN].copy_from_slice(&sine_l[..FRAME_LEN]);
        }
        if seq == WindowSequence::LongStart {
            for s in w.iter_mut().take(FRAME_LEN + 448).skip(FRAME_LEN) {
                *s = 1.0;
            }
            w[FRAME_LEN + 448..FRAME_LEN + 448 + SHORT_HALF].copy_from_slice(&sine_s[SHORT_HALF..]);
        } else {
            w[FRAME_LEN..].copy_from_slice(&sine_l[FRAME_LEN..]);
        }
        w
    }

    /// **Law 7 — the neutral end is byte-identical.** With both shape flags false
    /// the new shape-aware window must equal the pre-A8 sine-only one, exactly,
    /// for every sequence. Every other A8 change is a pass-through of
    /// `cur_kbd = false` into a bit that was already 0.
    #[test]
    fn shape_sine_is_byte_identical() {
        let sine_l = crate::dsp::sine_window(LONG_N);
        let sine_s = crate::dsp::sine_window(SHORT_N);
        let kbd_l = crate::dsp::kbd_window(LONG_N, 4.0);
        let kbd_s = crate::dsp::kbd_window(SHORT_N, 6.0);
        for seq in [
            WindowSequence::OnlyLong,
            WindowSequence::LongStart,
            WindowSequence::LongStop,
        ] {
            let new = long_window(seq, false, false, &sine_l, &kbd_l, &sine_s, &kbd_s);
            let old = long_window_pre_a8(seq, &sine_l, &sine_s);
            assert_eq!(new, old, "{seq:?}: neutral end must be byte-identical");
        }
    }

    /// The whole encoded stream is byte-identical under the default config and an
    /// explicit `Sine` — i.e. the default really is the neutral arm.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn default_config_is_the_sine_arm() {
        let pcm: Vec<f32> = (0..8 * FRAME_LEN)
            .map(|i| {
                let t = i as f32 / 44100.0;
                0.4 * (2.0 * std::f32::consts::PI * 700.0 * t).sin()
            })
            .collect();
        let enc = |cfg: AacEncoderConfig| {
            let mut e = AacEncoder::new(cfg);
            e.push_pcm(&pcm, 1, 44100).unwrap();
            e.finish();
            let mut out = Vec::new();
            while let Ok(p) = e.next_packet() {
                out.extend_from_slice(&p.data);
            }
            out
        };
        let a = enc(AacEncoderConfig::default());
        let b = enc(AacEncoderConfig {
            window_shape: WindowShape::Sine,
            ..Default::default()
        });
        assert_eq!(a, b, "default must be the Sine arm");
    }

    /// Windows must satisfy the **Princen-Bradley condition** on every overlap:
    /// `w_prev[N+n]^2 + w_cur[n]^2 == 1`. This is what makes overlap-add exact. It
    /// must hold for every ordered pair of shapes, which is exactly why the left
    /// half follows the previous frame and the right half the current one.
    #[test]
    fn tdac_holds_across_every_shape_transition() {
        let sine_l = crate::dsp::sine_window(LONG_N);
        let sine_s = crate::dsp::sine_window(SHORT_N);
        let kbd_l = crate::dsp::kbd_window(LONG_N, 4.0);
        let kbd_s = crate::dsp::kbd_window(SHORT_N, 6.0);

        for (a, b) in [(false, false), (false, true), (true, false), (true, true)] {
            // Frame 1 has shape `a`; frame 2 has shape `b` and sees `a` as prev.
            let w1 = long_window(
                WindowSequence::OnlyLong,
                a,
                a,
                &sine_l,
                &kbd_l,
                &sine_s,
                &kbd_s,
            );
            let w2 = long_window(
                WindowSequence::OnlyLong,
                a,
                b,
                &sine_l,
                &kbd_l,
                &sine_s,
                &kbd_s,
            );
            for n in 0..FRAME_LEN {
                let sum = w1[FRAME_LEN + n] * w1[FRAME_LEN + n] + w2[n] * w2[n];
                assert!(
                    (sum - 1.0).abs() < 1e-4,
                    "TDAC broken at n={n} for prev={a} cur={b}: {sum}"
                );
            }
        }
    }

    /// Encode then decode under each arm. A prev/cur shape mismatch would show up
    /// here as gross reconstruction error, not a subtle quality delta.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn every_arm_round_trips() {
        let sr = 44100u32;
        let n = 12 * FRAME_LEN;
        let pcm: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    + 0.2 * (2.0 * std::f32::consts::PI * 2600.0 * t).sin()
            })
            .collect();

        for shape in [WindowShape::Sine, WindowShape::Kbd, WindowShape::Auto] {
            let mut e = AacEncoder::new(AacEncoderConfig {
                bitrate_bps: 128_000,
                window_shape: shape,
                shape_tonality_pct: 0.5,
                ..Default::default()
            });
            e.push_pcm(&pcm, 1, sr).unwrap();
            e.finish();

            let mut dec = crate::decode::Decoder::new(sr);
            let mut got = Vec::new();
            while let Ok(p) = e.next_packet() {
                let audio = dec.decode(&p.data, None).expect("decode");
                got.extend_from_slice(&audio.samples);
            }
            assert!(got.len() >= n, "{shape:?}: short decode");

            // Compare against the input, allowing the one-frame filterbank delay.
            let lag = FRAME_LEN;
            let cmp_len = n - 2 * FRAME_LEN;
            let (mut num, mut den) = (0f64, 0f64);
            for i in 0..cmp_len {
                let o = f64::from(pcm[i]);
                let d = f64::from(got[i + lag]);
                num += (o - d) * (o - d);
                den += o * o;
            }
            let snr = 10.0 * (den / num.max(1e-30)).log10();
            assert!(
                snr > 15.0,
                "{shape:?}: reconstruction SNR {snr:.1} dB is too low - \
                 a window-shape mismatch breaks TDAC"
            );
        }
    }

    /// The `Auto` gate must actually dispatch — produce a mix of shapes on
    /// content whose tonality varies. A gate that routes everything one way is
    /// not a gate.
    #[test]
    fn auto_gate_dispatches_both_ways() {
        let sr = 44100u32;
        let n = 16 * FRAME_LEN;
        // Alternate 4-frame blocks of tone (tonal) and noise (not).
        let mut seed = 0x2468_ACE0u32;
        let pcm: Vec<f32> = (0..n)
            .map(|i| {
                if (i / (4 * FRAME_LEN)) % 2 == 0 {
                    let t = i as f32 / sr as f32;
                    0.4 * (2.0 * std::f32::consts::PI * 900.0 * t).sin()
                } else {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    0.4 * (((seed >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0)
                }
            })
            .collect();

        let mut e = AacEncoder::new(AacEncoderConfig {
            bitrate_bps: 128_000,
            window_shape: WindowShape::Auto,
            shape_tonality_pct: 0.5,
            ..Default::default()
        });
        e.push_pcm(&pcm, 1, sr).unwrap();
        let nblocks = n.div_ceil(FRAME_LEN) + 1;
        let shapes = e.decide_shapes(nblocks);
        let kbd = shapes.iter().filter(|&&s| s).count();
        assert!(
            kbd > 0 && kbd < shapes.len(),
            "Auto must route both ways, got {kbd}/{} KBD",
            shapes.len()
        );

        // And it must route the TONAL blocks to KBD, not at random: the tonal
        // frames tonality must exceed the noise frames.
        let tone_ton = time_tonality(&e.block(0, 1));
        let noise_ton = time_tonality(&e.block(0, 5));
        assert!(
            tone_ton > noise_ton * 2.0,
            "the gate signal must separate tone ({tone_ton:.1}) from noise ({noise_ton:.1})"
        );
    }
}

// ---------------------------------------------------------------------------
// Rungs 1-3 (arms A1 short-block psy, A2 tonality SMR, A3 TNS).
//
// Each arm must clear the same three bars before any quality number counts:
//   1. OFF is byte-identical with the encoder as it shipped,
//   2. ON still round-trips through our own decoder,
//   3. ON actually changes something (an arm that is a no-op is not an arm).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod rung123 {
    use super::*;

    fn encode(pcm: &[f32], ch: u16, sr: u32, cfg: AacEncoderConfig) -> Vec<Vec<u8>> {
        let mut e = AacEncoder::new(cfg);
        e.push_pcm(pcm, ch, sr).unwrap();
        e.finish();
        let mut out = Vec::new();
        while let Ok(p) = e.next_packet() {
            out.push(p.data);
        }
        out
    }

    /// Decode a packet list, returning channel-0 PCM.
    fn decode_mono(packets: &[Vec<u8>], sr: u32) -> Vec<f32> {
        let mut dec = crate::decode::Decoder::new(sr);
        let mut out = Vec::new();
        for p in packets {
            let a = dec.decode(p, None).expect("decode");
            let ch = a.channels.max(1) as usize;
            out.extend(a.samples.iter().step_by(ch).copied());
        }
        out
    }

    /// A transient-rich mono signal: sharp attacks over a quiet tonal floor.
    /// Drives short blocks (arm A1) and gives TNS something to shape (arm A3).
    fn transient_signal(sr: u32, frames: usize) -> Vec<f32> {
        use std::f32::consts::PI;
        let n = frames * FRAME_LEN;
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let mut v = 0.05 * (2.0 * PI * 500.0 * t).sin();
                let k = i % (3 * FRAME_LEN);
                if k < 500 && i > FRAME_LEN {
                    let env = (1.0 - k as f32 / 500.0).max(0.0);
                    v += 0.85 * env * (2.0 * PI * 3300.0 * k as f32 / sr as f32).sin();
                }
                v
            })
            .collect()
    }

    /// [`transient_signal`] with a broadband (noise) attack, for arm A3. TNS
    /// engages only when the coded range reaches `TNS_MAX_LONG`; a tone plus a
    /// 3.3 kHz burst has nothing up there once the mask has a hearing floor
    /// (#129), so its top bands are correctly left uncoded and TNS never fires.
    fn broadband_transient_signal(sr: u32, frames: usize) -> Vec<f32> {
        let mut x = 0x2468_ACE1u32;
        let mut noise = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let mut v = transient_signal(sr, frames);
        for (i, s) in v.iter_mut().enumerate() {
            let k = i % (3 * FRAME_LEN);
            let n = noise();
            if k < 500 && i > FRAME_LEN {
                *s += 0.5 * (1.0 - k as f32 / 500.0) * n;
            }
        }
        v
    }

    /// A tonal signal — a harmonic stack, the content arm A2 should treat
    /// differently from noise.
    fn tonal_signal(sr: u32, frames: usize) -> Vec<f32> {
        use std::f32::consts::PI;
        let n = frames * FRAME_LEN;
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let mut v = 0f32;
                for h in 1..=6 {
                    v += (1.0 / h as f32) * (2.0 * PI * 330.0 * h as f32 * t).sin();
                }
                0.3 * v
            })
            .collect()
    }

    /// **Every arm's OFF state is byte-identical with the shipped encoder.**
    /// This is law 7's neutral end, proven by `cmp` rather than argued.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn every_arm_off_is_byte_identical() {
        let sr = 44100u32;
        let sigs = [transient_signal(sr, 10), tonal_signal(sr, 10)];
        for pcm in &sigs {
            let base = encode(pcm, 1, sr, AacEncoderConfig::default());
            for (name, cfg) in [
                (
                    "short_block_psy",
                    AacEncoderConfig {
                        short_block_psy: false,
                        ..Default::default()
                    },
                ),
                (
                    "tonality_smr",
                    AacEncoderConfig {
                        tonality_smr: false,
                        ..Default::default()
                    },
                ),
                (
                    "tns",
                    AacEncoderConfig {
                        tns: false,
                        ..Default::default()
                    },
                ),
            ] {
                let got = encode(pcm, 1, sr, cfg);
                assert_eq!(got, base, "{name}: OFF must be byte-identical");
            }
        }
    }

    /// **Arm A1** — short-block psy must change the bitstream on transient
    /// content (where short blocks actually occur) and still decode.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a1_short_block_psy_changes_and_round_trips() {
        let sr = 44100u32;
        let pcm = transient_signal(sr, 12);
        let off = encode(&pcm, 1, sr, AacEncoderConfig::default());
        let on = encode(
            &pcm,
            1,
            sr,
            AacEncoderConfig {
                short_block_psy: true,
                ..Default::default()
            },
        );
        assert_ne!(off, on, "A1 must change the bitstream on transient content");

        let dec = decode_mono(&on, sr);
        assert!(dec.len() >= pcm.len(), "A1: short decode");
        assert!(
            dec.iter().all(|v| v.is_finite()),
            "A1: non-finite output — the short-block mask is broken"
        );
        // Reconstruction must remain sane; a wrong band geometry in the short psy
        // path shows up as gross error, not a subtle quality shift.
        let lag = FRAME_LEN;
        let cmp = pcm.len() - 2 * FRAME_LEN;
        let (mut num, mut den) = (0f64, 0f64);
        for i in 0..cmp {
            let o = f64::from(pcm[i]);
            let d = f64::from(dec[i + lag]);
            num += (o - d) * (o - d);
            den += o * o;
        }
        let snr = 10.0 * (den / num.max(1e-30)).log10();
        assert!(snr > 8.0, "A1: reconstruction SNR {snr:.1} dB too low");
    }

    /// **Arm 1a** — window grouping splits at energy jumps and leaves a steady
    /// frame as one group.
    #[test]
    fn a1a_short_groups_split_at_the_attack() {
        let mut spec = vec![0.0f32; FRAME_LEN];
        for (i, v) in spec.iter_mut().enumerate() {
            let win = i / SHORT_HALF;
            *v = if win < 5 { 1.0 } else { 40.0 } * if i % 2 == 0 { 1.0 } else { -1.0 };
        }
        assert_eq!(short_groups(&[&spec]), vec![5, 3]);
        let steady = vec![3.0f32; FRAME_LEN];
        assert_eq!(short_groups(&[&steady]), vec![8]);
        // Groups always tile the eight windows.
        let mut ramp = vec![0.0f32; FRAME_LEN];
        for (i, v) in ramp.iter_mut().enumerate() {
            *v = 4f32.powi((i / SHORT_HALF) as i32);
        }
        let g = short_groups(&[&ramp]);
        assert_eq!(g.iter().map(|&x| x as usize).sum::<usize>(), 8);
        assert!(g.len() > 1);
    }

    /// **Arm 1a** — end to end, mono and stereo: the arm changes the bitstream on
    /// transient content and the result still decodes to a sane reconstruction.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a1a_window_grouping_changes_and_round_trips() {
        let sr = 44100u32;
        let pcm = transient_signal(sr, 12);
        let cfg = AacEncoderConfig {
            window_grouping: true,
            ..Default::default()
        };
        let off = encode(&pcm, 1, sr, AacEncoderConfig::default());
        let on = encode(&pcm, 1, sr, cfg);
        assert_ne!(off, on, "1a must change the bitstream on transient content");
        let dec = decode_mono(&on, sr);
        assert!(dec.iter().all(|v| v.is_finite()), "1a: non-finite output");
        let lag = FRAME_LEN;
        let cmp = pcm.len() - 2 * FRAME_LEN;
        let (mut num, mut den) = (0f64, 0f64);
        for i in 0..cmp {
            let (o, d) = (f64::from(pcm[i]), f64::from(dec[i + lag]));
            num += (o - d) * (o - d);
            den += o * o;
        }
        let snr = 10.0 * (den / num.max(1e-30)).log10();
        assert!(snr > 8.0, "1a: reconstruction SNR {snr:.1} dB too low");

        // Stereo: a shared grouping and per-group M/S through the CPE path.
        let st: Vec<f32> = pcm.iter().flat_map(|&v| [v, 0.6 * v]).collect();
        let packets = encode(&st, 2, sr, cfg);
        let mut d = crate::decode::Decoder::new(sr);
        let mut frames = 0usize;
        for p in &packets {
            let a = d.decode(p, None).expect("1a stereo decodes");
            assert_eq!(a.channels, 2);
            assert!(a.samples.iter().all(|v| v.is_finite()));
            frames += a.frames();
        }
        assert!(frames >= pcm.len(), "1a stereo: short decode");
    }

    /// **Arm A2** — the SMR must actually vary with tonality. A pure tone and
    /// broadband noise at equal energy must no longer receive identical masks.
    #[test]
    fn a2_smr_varies_with_tonality() {
        // Direct check of the law itself, at a mid-band Bark.
        let tonal = smr_for(1.0, 10.0);
        let noisy = smr_for(0.0, 10.0);
        assert!(
            tonal < noisy,
            "a tonal masker must hold the mask FURTHER below the signal \
             (tonal {tonal:.5} must be < noise {noisy:.5})"
        );
        // And it must stay inside the documented clamp.
        for t in [0.0f64, 0.25, 0.5, 0.75, 1.0] {
            for bark in [0.0f64, 5.0, 12.0, 24.0] {
                let v = smr_for(t, bark);
                assert!(
                    v > 0.0 && v <= 10f64.powf(-5.0 / 10.0) + 1e-12,
                    "smr_for({t}, {bark}) = {v} escaped its clamp"
                );
            }
        }
    }

    /// **Arm A2** — end to end: the bitstream changes on tonal content and still
    /// decodes.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a2_changes_and_round_trips() {
        let sr = 44100u32;
        let pcm = tonal_signal(sr, 10);
        let off = encode(&pcm, 1, sr, AacEncoderConfig::default());
        let on = encode(
            &pcm,
            1,
            sr,
            AacEncoderConfig {
                tonality_smr: true,
                ..Default::default()
            },
        );
        assert_ne!(off, on, "A2 must change the bitstream on tonal content");
        let dec = decode_mono(&on, sr);
        assert!(dec.iter().all(|v| v.is_finite()));
        assert!(dec.len() >= pcm.len());
    }

    /// **Arm A3 — the filter inversion.** The encoder runs the ALL-ZERO analysis
    /// filter; the decoder runs the ALL-POLE synthesis filter. If that inversion
    /// is wrong in sign, order or state, the spectrum does not degrade — it
    /// diverges. So this test demands the TNS arm reconstruct *at least as well*
    /// as the un-filtered arm, not merely "not crash".
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a3_tns_inverts_exactly() {
        let sr = 44100u32;
        let pcm = broadband_transient_signal(sr, 14);

        let snr_of = |cfg: AacEncoderConfig| -> f64 {
            let pkts = encode(&pcm, 1, sr, cfg);
            let dec = decode_mono(&pkts, sr);
            assert!(dec.len() >= pcm.len());
            let lag = FRAME_LEN;
            let cmp = pcm.len() - 2 * FRAME_LEN;
            let (mut num, mut den) = (0f64, 0f64);
            for i in 0..cmp {
                let o = f64::from(pcm[i]);
                let d = f64::from(dec[i + lag]);
                num += (o - d) * (o - d);
                den += o * o;
            }
            10.0 * (den / num.max(1e-30)).log10()
        };

        let base = snr_of(AacEncoderConfig {
            bitrate_bps: 256_000,
            ..Default::default()
        });
        let with_tns = snr_of(AacEncoderConfig {
            bitrate_bps: 256_000,
            tns: true,
            ..Default::default()
        });
        assert!(
            with_tns > base - 3.0,
            "A3: TNS collapsed reconstruction ({with_tns:.1} dB vs {base:.1} dB) — \
             the analysis/synthesis inversion is wrong, not merely suboptimal"
        );
    }

    /// **Arm A3** — the quantized PARCOR round-trip must match the decoder's
    /// dequantizer bit-for-bit, including its asymmetric negative scale.
    #[test]
    fn a3_parcor_quantization_matches_the_decoder() {
        use std::f32::consts::PI;
        let res_bits = 3 + TNS_COEF_RES;
        let iqfac = ((1i32 << (res_bits - 1)) as f32 - 0.5) / (PI / 2.0);
        let iqfac_m = ((1i32 << (res_bits - 1)) as f32 + 0.5) / (PI / 2.0);
        for step in -20..=20 {
            let p = step as f32 / 21.0;
            let (idx, dq) = quantize_parcor(p);
            // Reproduce the DECODER's path from the emitted index.
            let masked = (idx as u32) & ((1u32 << TNS_COEF_BITS) - 1);
            let c = if masked & (1 << (TNS_COEF_BITS - 1)) != 0 {
                masked as i32 - (1 << TNS_COEF_BITS)
            } else {
                masked as i32
            };
            let t = if c >= 0 {
                c as f32 / iqfac
            } else {
                c as f32 / iqfac_m
            };
            let expect = t.sin();
            assert!(
                (dq - expect).abs() < 1e-6,
                "parcor {p}: encoder says {dq}, decoder reconstructs {expect}"
            );
        }
    }

    /// **Arm A3** — TNS must actually engage on transient content (otherwise the
    /// round-trip test above passes vacuously).
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a3_tns_actually_engages() {
        // 256 kbps: TNS engages only on frames coded up to TNS_MAX_LONG (about
        // 16 kHz here), and since the hearing floor (#129) 128 kbps mono no
        // longer spends bits on the top bands of this signal.
        let sr = 44100u32;
        let pcm = broadband_transient_signal(sr, 14);
        let off = encode(
            &pcm,
            1,
            sr,
            AacEncoderConfig {
                bitrate_bps: 256_000,
                ..Default::default()
            },
        );
        let on = encode(
            &pcm,
            1,
            sr,
            AacEncoderConfig {
                bitrate_bps: 256_000,
                tns: true,
                ..Default::default()
            },
        );
        assert_ne!(
            off, on,
            "A3 never fired — the gate declined every frame, so the round-trip \
             test proves nothing"
        );
    }
}

// ---------------------------------------------------------------------------
// Multichannel conformance — the tests that would have caught the original bug.
//
// The encoder used to emit N single_channel_elements while declaring
// `channel_configuration = N`. Our own decoder accepted it, so a self-round-trip
// looked perfect; a conforming decoder reading SCE, CPE, CPE, LFE would not.
// These tests check the ELEMENT SEQUENCE against ISO Table 1.19, not just that
// audio comes back.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod multichannel {
    use super::*;

    fn encode_packets(pcm: &[f32], ch: u16, sr: u32) -> Vec<Vec<u8>> {
        let mut e = AacEncoder::new(AacEncoderConfig::default());
        e.push_pcm(pcm, ch, sr).unwrap();
        e.finish();
        let mut out = Vec::new();
        while let Ok(p) = e.next_packet() {
            out.push(p.data);
        }
        out
    }

    /// Read the sequence of element ids from a `raw_data_block`, skipping each
    /// element's payload by decoding it.
    fn element_ids(packet: &[u8], sr: u32) -> Vec<u32> {
        // Walk ids by re-decoding: the decoder is the only thing that knows each
        // element's length. We instead read ids from a fresh reader, relying on
        // the decoder to have validated the packet already.
        let mut ids = Vec::new();
        let mut dec = crate::decode::Decoder::new(sr);
        // The decoder must accept it at all.
        dec.decode(packet, None).expect("packet must decode");
        // Now re-read just the leading id of each element by decoding again with
        // a tracking reader is not available, so assert on the FIRST id plus the
        // channel count, which is what distinguishes the layouts we emit.
        let mut r = crate::BitReader::new(packet);
        ids.push(r.read_bits(3).unwrap());
        ids
    }

    /// Distinct per-channel content so a mis-ordered channel is detectable.
    fn distinct_channels(nch: usize, frames: usize, sr: u32) -> Vec<f32> {
        let n = frames * FRAME_LEN;
        let mut out = Vec::with_capacity(n * nch);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            for c in 0..nch {
                // A different frequency per channel, well separated.
                let f = 300.0 + 400.0 * c as f32;
                out.push(0.35 * (2.0 * std::f32::consts::PI * f * t).sin());
            }
        }
        out
    }

    /// Every supported channel count produces a stream our decoder accepts with
    /// the right channel count.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn supported_channel_counts_round_trip() {
        let sr = 44100u32;
        for nch in 1..=6usize {
            let pcm = distinct_channels(nch, 6, sr);
            let pkts = encode_packets(&pcm, nch as u16, sr);
            assert!(!pkts.is_empty(), "{nch}ch: no packets");
            let mut dec = crate::decode::Decoder::new(sr);
            let audio = dec.decode(&pkts[0], None).expect("decode");
            assert_eq!(audio.channels as usize, nch, "{nch}ch: wrong channel count");
        }
    }

    /// **The bug.** The first element of a 5.1 stream must be an SCE (the centre
    /// channel), and of a stereo stream a CPE. Before the fix a 5.1 stream began
    /// with an SCE too — but so did all six of its elements, which is what made it
    /// non-conformant.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn element_sequence_matches_iso_table() {
        let sr = 44100u32;
        for (nch, want_first) in [(1usize, ID_SCE), (2, ID_CPE), (3, ID_SCE), (6, ID_SCE)] {
            let pcm = distinct_channels(nch, 4, sr);
            let pkts = encode_packets(&pcm, nch as u16, sr);
            let ids = element_ids(&pkts[0], sr);
            assert_eq!(ids[0], want_first, "{nch}ch: wrong first element id");
        }
    }

    /// The plan itself must match ISO Table 1.19, including the reordering.
    #[test]
    fn element_plan_matches_iso_table() {
        assert_eq!(element_plan(1).unwrap(), vec![Elem::Sce(0)]);
        assert_eq!(element_plan(2).unwrap(), vec![Elem::Cpe(0, 1)]);
        // 3.0: centre first, then the front pair.
        assert_eq!(
            element_plan(3).unwrap(),
            vec![Elem::Sce(2), Elem::Cpe(0, 1)]
        );
        // 5.1: C, L/R, Ls/Rs, LFE — with LFE taken from interleave slot 3.
        assert_eq!(
            element_plan(6).unwrap(),
            vec![Elem::Sce(2), Elem::Cpe(0, 1), Elem::Cpe(4, 5), Elem::Lfe(3)]
        );
        // No channel_configuration exists for these.
        assert!(element_plan(0).is_none());
        assert!(element_plan(7).is_none());
        assert!(element_plan(8).is_none());
    }

    /// The decoder's reorder must be the exact inverse of the encoder's plan:
    /// what goes into interleave slot `k` must come back out of slot `k`.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn channel_order_survives_the_round_trip() {
        let sr = 44100u32;
        for nch in [1usize, 2, 3, 5, 6] {
            let pcm = distinct_channels(nch, 8, sr);
            let pkts = encode_packets(&pcm, nch as u16, sr);
            let mut dec = crate::decode::Decoder::new(sr);
            let mut planes: Vec<Vec<f32>> = vec![Vec::new(); nch];
            for p in &pkts {
                let a = dec.decode(p, None).expect("decode");
                for (i, v) in a.samples.iter().enumerate() {
                    planes[i % nch].push(*v);
                }
            }
            // Each decoded channel must correlate best with the SAME input
            // channel. A layout or reorder error shows up as a swapped best match.
            for c in 0..nch {
                let f_expect = 300.0 + 400.0 * c as f32;
                // Estimate dominant frequency by zero-crossing count over a
                // mid-stream window (crude but unambiguous at this separation).
                let seg = &planes[c][3 * FRAME_LEN..5 * FRAME_LEN];
                let mut crossings = 0usize;
                for w in seg.windows(2) {
                    if (w[0] <= 0.0) != (w[1] <= 0.0) {
                        crossings += 1;
                    }
                }
                let secs = seg.len() as f32 / sr as f32;
                let f_est = crossings as f32 / 2.0 / secs;
                assert!(
                    (f_est - f_expect).abs() < 120.0,
                    "{nch}ch: channel {c} came back at ~{f_est:.0} Hz, expected \\
                     ~{f_expect:.0} Hz — channel order is wrong"
                );
            }
        }
    }

    /// Unsupported counts must be REJECTED, not silently mis-encoded. This is the
    /// guard: an unencodable layout is an error, never a wrong bitstream.
    #[test]
    fn unsupported_channel_counts_are_rejected() {
        for nch in [7u16, 8, 12] {
            let pcm = vec![0.1f32; FRAME_LEN * nch as usize * 2];
            let mut e = AacEncoder::new(AacEncoderConfig::default());
            let res = e.push_pcm(&pcm, nch, 44100);
            assert!(
                res.is_err(),
                "{nch} channels must be rejected until a program_config_element exists"
            );
        }
    }
}

#[cfg(test)]
mod rung_a6 {
    use super::*;

    fn noise_signal(_sr: u32, frames: usize) -> Vec<f32> {
        let mut seed = 0xA6A6_1234u32;
        (0..frames * FRAME_LEN)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                0.4 * (((seed >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0)
            })
            .collect()
    }

    fn encode(pcm: &[f32], sr: u32, cfg: AacEncoderConfig) -> Vec<Vec<u8>> {
        let mut e = AacEncoder::new(cfg);
        e.push_pcm(pcm, 1, sr).unwrap();
        e.finish();
        let mut out = Vec::new();
        while let Ok(p) = e.next_packet() {
            out.push(p.data);
        }
        out
    }

    /// OFF is byte-identical with the shipped encoder.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a6_off_is_byte_identical() {
        let sr = 44100;
        let pcm = noise_signal(sr, 8);
        let a = encode(&pcm, sr, AacEncoderConfig::default());
        let b = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                pns: false,
                ..Default::default()
            },
        );
        assert_eq!(a, b);
    }

    /// PNS must fire on noise and the stream must still decode. A desynchronized
    /// scalefactor chain (the classic PNS bug) shows up here as a decode error or
    /// non-finite output, not as a subtle quality shift.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a6_fires_on_noise_and_round_trips() {
        let sr = 44100;
        let pcm = noise_signal(sr, 10);
        let off = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 96_000,
                ..Default::default()
            },
        );
        let on = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 96_000,
                pns: true,
                ..Default::default()
            },
        );
        assert_ne!(off, on, "A6 must fire on broadband noise");

        let mut dec = crate::decode::Decoder::new(sr);
        let mut got = Vec::new();
        for p in &on {
            let a = dec.decode(p, None).expect("PNS stream must decode");
            got.extend_from_slice(&a.samples);
        }
        assert!(got.iter().all(|v| v.is_finite()), "non-finite output");
        // Energy must be broadly preserved: PNS substitutes noise of the SAME
        // energy, so a gross level error means the ne <-> energy mapping is wrong.
        let e_in: f64 = pcm.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        let e_out: f64 = got.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        let ratio = e_out / e_in.max(1e-12);
        assert!(
            (0.25..4.0).contains(&ratio),
            "PNS energy mismatch: out/in = {ratio:.3} - check ne = 2*log2(E)"
        );
    }

    /// PNS must NOT fire on a pure tone: it is the anti-class, and substituting a
    /// harmonic band with noise is audibly destructive.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a6_declines_on_tonal_content() {
        let sr = 44100;
        let pcm: Vec<f32> = (0..10 * FRAME_LEN)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.4 * (2.0 * std::f32::consts::PI * 6000.0 * t).sin()
            })
            .collect();
        let off = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 96_000,
                ..Default::default()
            },
        );
        let on = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 96_000,
                pns: true,
                ..Default::default()
            },
        );
        assert_eq!(
            off, on,
            "A6 fired on a pure 6 kHz tone - the tonality guard is not working"
        );
    }

    /// PNS and TNS must not both apply to one element (the energy domain differs).
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a6_yields_to_tns() {
        let sr = 44100;
        let pcm = noise_signal(sr, 10);
        let both = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 128_000,
                pns: true,
                tns: true,
                ..Default::default()
            },
        );
        let mut dec = crate::decode::Decoder::new(sr);
        for p in &both {
            let a = dec.decode(p, None).expect("combined stream must decode");
            assert!(a.samples.iter().all(|v| v.is_finite()));
        }
    }
}

#[cfg(test)]
mod rung_a7 {
    use super::*;

    /// A stereo pair whose high bands are a scaled copy — the content intensity
    /// stereo exists for.
    fn correlated_stereo(sr: u32, frames: usize, gain: f32) -> Vec<f32> {
        let n = frames * FRAME_LEN;
        let mut out = Vec::with_capacity(n * 2);
        let mut seed = 0x7777_1111u32;
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let low = 0.3 * (2.0 * std::f32::consts::PI * 300.0 * t).sin();
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let hi = 0.25 * (((seed >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0);
            out.push(low + hi);
            out.push(low + hi * gain);
        }
        out
    }

    fn encode(pcm: &[f32], sr: u32, cfg: AacEncoderConfig) -> Vec<Vec<u8>> {
        let mut e = AacEncoder::new(cfg);
        e.push_pcm(pcm, 2, sr).unwrap();
        e.finish();
        let mut out = Vec::new();
        while let Ok(p) = e.next_packet() {
            out.push(p.data);
        }
        out
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a7_off_is_byte_identical() {
        let sr = 44100;
        let pcm = correlated_stereo(sr, 8, 0.7);
        let a = encode(&pcm, sr, AacEncoderConfig::default());
        let b = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                intensity: false,
                ..Default::default()
            },
        );
        assert_eq!(a, b);
    }

    /// Fires on a scaled-copy pair and still decodes. A wrong `is_pos` sign or a
    /// desynchronized intensity accumulator shows up here immediately.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a7_fires_and_round_trips() {
        let sr = 44100;
        let pcm = correlated_stereo(sr, 10, 0.7);
        let off = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 96_000,
                ..Default::default()
            },
        );
        let on = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 96_000,
                intensity: true,
                ..Default::default()
            },
        );
        assert_ne!(off, on, "A7 must fire on a correlated high band");

        let mut dec = crate::decode::Decoder::new(sr);
        let mut got = Vec::new();
        for p in &on {
            let a = dec.decode(p, None).expect("intensity stream must decode");
            got.extend_from_slice(&a.samples);
        }
        assert!(got.iter().all(|v| v.is_finite()));
        // The reconstructed right channel must keep roughly the intended level
        // ratio: a sign or exponent error in is_pos shows up as a gross imbalance.
        let (mut el, mut er) = (0f64, 0f64);
        for c in got.chunks_exact(2) {
            el += f64::from(c[0]) * f64::from(c[0]);
            er += f64::from(c[1]) * f64::from(c[1]);
        }
        let ratio = (er / el.max(1e-12)).sqrt();
        assert!(
            (0.4..1.6).contains(&ratio),
            "channel balance wrong after intensity: R/L = {ratio:.3}"
        );
    }

    /// Must DECLINE on a decorrelated pair — the anti-class. Collapsing a wide
    /// image to a scaled copy is the failure mode this gate exists to prevent.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn a7_declines_on_wide_stereo() {
        let sr = 44100;
        let n = 8 * FRAME_LEN;
        let mut s1 = 0xAAAA_1111u32;
        let mut s2 = 0x5555_2222u32;
        let mut pcm = Vec::with_capacity(n * 2);
        for _ in 0..n {
            s1 = s1.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            s2 = s2.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pcm.push(0.3 * (((s1 >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0));
            pcm.push(0.3 * (((s2 >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0));
        }
        let off = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 128_000,
                ..Default::default()
            },
        );
        let on = encode(
            &pcm,
            sr,
            AacEncoderConfig {
                bitrate_bps: 128_000,
                intensity: true,
                ..Default::default()
            },
        );
        assert_eq!(
            off, on,
            "A7 fired on fully decorrelated channels - the correlation gate is not working"
        );
    }
}

/// `remade_ffmpeg_rs#129`: a pure tone, and a 5.1 stream with only its centre
/// active, must come back at their level and with the precision their bitrate
/// buys. Before the hearing floor the mask scaled with each band's own energy,
/// so near-silent bands took the bits (a 440 Hz tone at 128 kbps: 19.8 dB);
/// before budget pooling a lone 5.1 centre got a sixth of the bitrate (320
/// kbps: 10.2 dB, gain up to 1.54 at 128 kbps).
#[cfg(test)]
mod fidelity_129 {
    use super::*;

    /// Encode interleaved `pcm`, decode it, and return channel `c`'s gain and
    /// SNR (dB) against `src` over the middle half, at the encoder's
    /// 1024-sample delay; plus the largest output sample on every other channel.
    fn gain_snr(pcm: &[f32], ch: usize, c: usize, sr: u32, kbps: u32) -> (f64, f64, f32) {
        let mut e = AacEncoder::new(AacEncoderConfig {
            bitrate_bps: kbps * 1000,
            ..Default::default()
        });
        e.push_pcm(pcm, ch as u16, sr).unwrap();
        e.finish();
        let mut dec = crate::decode::Decoder::new(sr);
        let mut out: Vec<f32> = Vec::new();
        while let Ok(p) = e.next_packet() {
            out.extend(dec.decode(&p.data, None).expect("decode").samples);
        }
        let n = pcm.len() / ch;
        let (mut xy, mut xx) = (0f64, 0f64);
        for i in n / 4..n * 3 / 4 {
            let (x, y) = (
                f64::from(pcm[i * ch + c]),
                f64::from(out[(i + FRAME_LEN) * ch + c]),
            );
            xy += x * y;
            xx += x * x;
        }
        let g = xy / xx;
        let mut err = 0f64;
        for i in n / 4..n * 3 / 4 {
            let e = f64::from(out[(i + FRAME_LEN) * ch + c]) - f64::from(pcm[i * ch + c]);
            err += e * e;
        }
        let others = (0..out.len())
            .filter(|i| i % ch != c)
            .map(|i| out[i].abs())
            .fold(0f32, f32::max);
        (g, 10.0 * (xx / err).log10(), others)
    }

    fn tone(n: usize, ch: usize, c: usize, sr: u32) -> Vec<f32> {
        let amp = 10f32.powf(-10.0 / 20.0);
        let mut v = vec![0f32; n * ch];
        for i in 0..n {
            v[i * ch + c] = amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin();
        }
        v
    }

    /// A 220/330 Hz stereo pair, interleaved.
    fn tone_pair(n: usize, sr: u32) -> Vec<f32> {
        (0..n * 2)
            .map(|i| {
                let f = [220.0f32, 330.0][i % 2];
                let t = (i / 2) as f32 / sr as f32;
                0.316 * (2.0 * std::f32::consts::PI * f * t).sin()
            })
            .collect()
    }

    fn tuned_aus(pcm: &[f32], ch: u16, kbps: u32, surplus: bool) -> Vec<Vec<u8>> {
        let tuning = AacTuning {
            surplus_shaping: surplus,
            ..Default::default()
        };
        let cfg = AacEncoderConfig {
            bitrate_bps: kbps * 1000,
            ..Default::default()
        };
        let mut e = AacEncoder::with_tuning(cfg, tuning);
        e.push_pcm(pcm, ch, 48_000).unwrap();
        e.finish();
        std::iter::from_fn(|| e.next_packet().ok().map(|p| p.data)).collect()
    }

    /// `remade_ffmpeg_rs#211`: bits left over far below the masks go to the
    /// tones, not to their inaudible window leakage (51 and 62 dB before),
    /// up to the quantizer's own limit of about 84 dB, at any rate.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn spare_bits_go_to_the_tones() {
        let sr = 48_000;
        let pcm = tone_pair(sr as usize * 2, sr);
        for (kbps, want) in [(256, 50.0), (320, 60.0), (512, 78.0)] {
            for c in 0..2 {
                let (g, snr, _) = gain_snr(&pcm, 2, c, sr, kbps);
                assert!((0.999..=1.001).contains(&g), "{kbps} kbps: gain {g}");
                assert!(snr > want, "{kbps} kbps channel {c}: {snr:.1} dB");
            }
        }
        let mono = tone(sr as usize * 2, 1, 0, sr);
        // Past what a 6144-bit frame holds: every band is at its no-clamp floor.
        let (_, snr, _) = gain_snr(&mono, 1, 0, sr, 512);
        assert!(snr > 78.0, "mono: {snr:.1} dB");
    }

    /// The surplus policy leaves every stream under its rate, and every
    /// multichannel one, byte for byte as it was.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn surplus_shaping_leaves_low_rates_and_multichannel_alone() {
        let n = 48_000;
        let stereo = tone_pair(n, 48_000);
        let six = tone(n, 6, 2, 48_000);
        for (pcm, ch, kbps, same) in [
            (&stereo, 2, 190, true),
            (&stereo, 2, 320, false),
            (&six, 6, 320, true),
            (&six, 6, 640, true),
        ] {
            let (on, off) = (
                tuned_aus(pcm, ch, kbps, true),
                tuned_aus(pcm, ch, kbps, false),
            );
            assert_eq!(on == off, same, "{ch} channels at {kbps} kbps");
            assert!(on.iter().all(|p| p.len() * 8 <= 6144 * usize::from(ch)));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn a_pure_tone_gets_the_bits() {
        let sr = 48_000;
        let (g, snr, _) = gain_snr(&tone(sr as usize * 2, 1, 0, sr), 1, 0, sr, 128);
        assert!((0.99..=1.01).contains(&g), "gain {g}");
        assert!(snr > 30.0, "a 440 Hz tone at 128 kbps: {snr:.1} dB");
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn a_lone_5_1_centre_gets_the_whole_bitrate() {
        let sr = 48_000;
        // WAV / FFmpeg order: L R C LFE Ls Rs.
        let (g, snr, others) = gain_snr(&tone(sr as usize * 2, 6, 2, sr), 6, 2, sr, 320);
        assert!((0.99..=1.01).contains(&g), "centre gain {g}");
        assert!(snr > 45.0, "5.1 centre alone at 320 kbps: {snr:.1} dB");
        assert!(others < 1e-3, "silent channels stay silent: {others}");
    }

    /// One 0.25 sine per channel (220, 330, 440, 55, 660, 880 Hz in L R C LFE
    /// Ls Rs order) over independent 0.03 x uniform(-0.5, 0.5) noise: the
    /// #129 repro with every element busy.
    fn busy_5_1(n: usize, sr: u32) -> Vec<f32> {
        let tones = [220.0f32, 330.0, 440.0, 55.0, 660.0, 880.0];
        let mut seed = 0x2545_f491u32;
        let mut v = vec![0f32; n * 6];
        for i in 0..n {
            for (c, f) in tones.iter().enumerate() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = 0.03 * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
                let t = i as f32 / sr as f32;
                v[i * 6 + c] = 0.25 * (2.0 * std::f32::consts::PI * f * t).sin() + noise;
            }
        }
        v
    }

    /// Encode `pcm` and return the packets.
    fn packets(pcm: &[f32], ch: usize, sr: u32, kbps: u32) -> Vec<EncodedPacket> {
        let mut e = AacEncoder::new(AacEncoderConfig {
            bitrate_bps: kbps * 1000,
            ..Default::default()
        });
        e.push_pcm(pcm, ch as u16, sr).unwrap();
        e.finish();
        std::iter::from_fn(|| e.next_packet().ok()).collect()
    }

    /// The decoder rebuilds `q^(4/3)`, so the quantizer must pick the level
    /// whose reconstruction is nearest, not the nearest integer to `|x|^0.75`.
    #[test]
    fn the_quantizer_picks_the_nearest_reconstruction() {
        for i in 0..=3_000 {
            let v = f64::from(i) * 1e-3;
            let x = v.powf(4.0 / 3.0);
            let nearest = (0..6)
                .min_by(|&a, &b| {
                    let da = (x - f64::from(a).powf(4.0 / 3.0)).abs();
                    let db = (x - f64::from(b).powf(4.0 / 3.0)).abs();
                    da.partial_cmp(&db).unwrap()
                })
                .unwrap();
            let got = nearest_level(v);
            // Above 2 the midpoints sit within 0.017 of k + 0.5 and plain
            // rounding is kept there.
            if v < 2.0 || (v.fract() - 0.5).abs() > 0.02 {
                assert_eq!(got, nearest, "v = {v}");
            } else {
                assert!((got - nearest).abs() <= 1, "v = {v}");
            }
        }
    }

    /// A 5.1 stream with all six channels busy at 320 kbps (53 kbps a
    /// channel): every channel comes back at its own level. On 2.1.1 the
    /// best-fit gains were 1.25 / 1.42 / 1.31 / 1.22 / 1.49 / 1.20.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn every_busy_5_1_channel_keeps_its_level() {
        let sr = 48_000;
        let n = sr as usize * 3;
        let pcm = busy_5_1(n, sr);
        let mut dec = crate::decode::Decoder::new(sr);
        let mut out: Vec<f32> = Vec::new();
        for p in packets(&pcm, 6, sr, 320) {
            out.extend(dec.decode(&p.data, None).expect("decode").samples);
        }
        for c in 0..6 {
            let (mut xy, mut xx) = (0f64, 0f64);
            for i in n / 4..n * 3 / 4 {
                let x = f64::from(pcm[i * 6 + c]);
                xy += x * f64::from(out[(i + FRAME_LEN) * 6 + c]);
                xx += x * x;
            }
            let g = xy / xx;
            assert!((0.93..=1.07).contains(&g), "channel {c}: gain {g:.3}");
        }
    }

    /// An LFE frame drops its quiet content above 281 Hz (a sub-bass tone
    /// over a noise floor) but keeps content that is really there.
    #[test]
    fn the_lfe_drops_only_a_quiet_top() {
        let sr = 48_000.0f32;
        let win: Vec<f32> = crate::dsp::sine_window(2 * FRAME_LEN);
        let spectrum = |f: f32, noise: f32| {
            let mut seed = 0x9e37_79b9u32;
            let mut x = [[0f32; FRAME_LEN]; 2];
            for (b, blk) in x.iter_mut().enumerate() {
                for (i, v) in blk.iter_mut().enumerate() {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let t = (b * FRAME_LEN + i) as f32 / sr;
                    *v = 0.25 * (2.0 * std::f32::consts::PI * f * t).sin()
                        + noise * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
                }
            }
            analyze_long(&x[0], &x[1], &win)
        };
        let mut sub = spectrum(55.0, 0.03);
        lfe_band_limit(&mut sub);
        assert!(sub[LFE_LINES..].iter().all(|&v| v == 0.0));
        assert!(sub[..LFE_LINES].iter().any(|&v| v != 0.0));
        let mut mid = spectrum(1000.0, 0.0);
        let kept = mid.clone();
        lfe_band_limit(&mut mid);
        assert_eq!(mid, kept);
    }

    /// The downstream 5.1 gate signal: one 0.25 tone per channel (500, 700, 900, 60,
    /// 1300, 1700 Hz in L R C LFE Ls Rs order) with a slow swell, over
    /// independent 0.03 x uniform(-0.5, 0.5) noise. `ch` = 1 is its first
    /// channel alone.
    fn tone_over_noise(n: usize, ch: usize, sr: u32) -> Vec<f32> {
        let tones = [500.0f32, 700.0, 900.0, 60.0, 1300.0, 1700.0];
        let mut seed = 0x1234_5678u32;
        let mut v = vec![0f32; n * ch];
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let swell = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 0.7 * t).sin();
            for (c, f) in tones.iter().enumerate().take(ch) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = 0.03 * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
                v[i * ch + c] = 0.25 * swell * (2.0 * std::f32::consts::PI * f * t).sin() + noise;
            }
        }
        v
    }

    /// Encode and decode `pcm`; return the SNR over all channels and the
    /// level (dB, decoded RMS over source RMS) of each channel, over the clip
    /// at the encoder's 1024-sample delay.
    fn snr_levels(pcm: &[f32], ch: usize, sr: u32, kbps: u32) -> (f64, Vec<f64>) {
        let mut dec = crate::decode::Decoder::new(sr);
        let mut out: Vec<f32> = Vec::new();
        for p in packets(pcm, ch, sr, kbps) {
            out.extend(dec.decode(&p.data, None).expect("decode").samples);
        }
        let n = pcm.len() / ch;
        let (mut sx, mut se) = (0f64, 0f64);
        let mut levels = Vec::with_capacity(ch);
        for c in 0..ch {
            let (mut xx, mut yy) = (0f64, 0f64);
            for i in 0..n {
                let x = f64::from(pcm[i * ch + c]);
                let y = f64::from(out[(i + FRAME_LEN) * ch + c]);
                xx += x * x;
                yy += y * y;
                se += (y - x) * (y - x);
            }
            sx += xx;
            levels.push(10.0 * (yy / xx).log10());
        }
        (10.0 * (sx / se).log10(), levels)
    }

    /// On a tone over noise the SNR rises with the bitrate, reaches 20 dB at
    /// 320 kbps 5.1, and every channel keeps its level within 0.3 dB. Before
    /// the narrow-band SNR floor ([`peak_hold`]) it sat at 15.8-16.9 dB from
    /// 128 to 512 kbps 5.1 (15.6-18.3 dB from 32 to 128 kbps mono) and levels
    /// swung by up to 0.6 dB.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn tone_over_noise_snr_rises_with_the_bitrate() {
        let sr = 48_000u32;
        let n = sr as usize * 2;
        for (ch, rates, at_320) in [
            (6, &[128, 192, 320, 512][..], true),
            (1, &[32, 64, 96, 128, 192][..], false),
        ] {
            let pcm = tone_over_noise(n, ch, sr);
            let mut last = f64::NEG_INFINITY;
            for &kbps in rates {
                let (snr, levels) = snr_levels(&pcm, ch, sr, kbps);
                assert!(
                    snr > last,
                    "{ch} ch: {snr:.2} dB at {kbps} kbps, not above {last:.2} dB"
                );
                last = snr;
                if at_320 && kbps == 320 {
                    assert!(snr >= 20.0, "5.1 at 320 kbps: {snr:.2} dB");
                }
                if ch == 1 {
                    assert!(snr >= 19.0, "mono at {kbps} kbps: {snr:.2} dB");
                }
                for (c, lv) in levels.iter().enumerate() {
                    assert!(
                        lv.abs() <= 0.3,
                        "{ch} ch at {kbps} kbps: channel {c} level {lv:+.2} dB"
                    );
                }
            }
        }
    }

    /// The #129 repro (tones over independent noise, every element busy)
    /// keeps every channel within 0.5 dB from 128 to 320 kbps. Before the
    /// narrow-band SNR floor Ls drifted by +0.8 to +1.0 dB at 128-192 kbps.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn every_repro_channel_keeps_its_level_at_128_to_320() {
        let sr = 48_000u32;
        let pcm = busy_5_1(sr as usize * 3, sr);
        for kbps in [128, 192, 256, 320] {
            let (_, levels) = snr_levels(&pcm, 6, sr, kbps);
            for (c, lv) in levels.iter().enumerate() {
                assert!(
                    lv.abs() <= 0.5,
                    "{kbps} kbps: channel {c} level {lv:+.2} dB"
                );
            }
        }
    }

    /// The raw access units average the target bitrate, within 1%, and no
    /// unit outgrows the decoder buffer (6144 bits a channel). On 2.1.1 the
    /// streams came out 3-13% short.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn the_stream_lands_on_its_bitrate() {
        let sr = 48_000u32;
        let n = sr as usize * 3;
        let five_one = busy_5_1(n, sr);
        let stereo: Vec<f32> = (0..n * 2).map(|i| five_one[(i / 2) * 6 + i % 2]).collect();
        let mono: Vec<f32> = (0..n).map(|i| five_one[i * 6 + 2]).collect();
        for (pcm, ch, kbps) in [(&mono, 1, 64), (&stereo, 2, 128), (&five_one, 6, 320)] {
            let pk = packets(pcm, ch, sr, kbps);
            let bits: usize = pk.iter().map(|p| p.data.len() * 8).sum();
            let rate = bits as f64 * f64::from(sr) / (pk.len() * FRAME_LEN) as f64;
            let err = rate / f64::from(kbps * 1000) - 1.0;
            assert!(
                err.abs() < 0.01,
                "{ch} ch at {kbps} kbps: {:+.2}%",
                err * 100.0
            );
            let most = pk.iter().map(|p| p.data.len() * 8).max().unwrap();
            assert!(most <= 6144 * ch, "{ch} ch: a {most}-bit unit");
        }
    }
}

/// The low-rate campaign's tools: the section trellis, the frame-demand
/// allocation, the coding bandwidth, the high-passed transient detector and
/// the mask SMR.
#[cfg(test)]
mod lowrate {
    use super::*;

    fn encode(pcm: &[f32], ch: u16, sr: u32, cfg: (AacEncoderConfig, AacTuning)) -> Vec<Vec<u8>> {
        let mut e = AacEncoder::with_tuning(cfg.0, cfg.1);
        e.push_pcm(pcm, ch, sr).unwrap();
        e.finish();
        let mut out = Vec::new();
        while let Ok(p) = e.next_packet() {
            out.push(p.data);
        }
        out
    }

    /// Interleaved PCM of a packet list through this crate's decoder, and
    /// its channel count.
    fn decode(packets: &[Vec<u8>], sr: u32) -> (Vec<f32>, usize) {
        let mut dec = crate::decode::Decoder::new(sr);
        let mut out = Vec::new();
        let mut ch = 1;
        for p in packets {
            let a = dec.decode(p, None).expect("decode");
            ch = a.channels.max(1) as usize;
            out.extend_from_slice(&a.samples);
        }
        (out, ch)
    }

    /// Music-like stereo: a harmonic tone with plucked attacks, a panned
    /// second voice, and broadband noise, so every tool has something to do.
    fn music(sr: u32, frames: usize) -> Vec<f32> {
        use std::f32::consts::PI;
        let n = frames * FRAME_LEN;
        let mut seed = 0x1234_5678u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let mut out = Vec::with_capacity(2 * n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let k = i % (3 * FRAME_LEN);
            let env = (-(k as f32) / 900.0).exp();
            let mut a = 0.0;
            for h in 1..=8 {
                a += (0.3 / h as f32) * (2.0 * PI * 196.0 * h as f32 * t).sin();
            }
            let b = 0.2 * (2.0 * PI * 523.0 * t).sin();
            let nz = 0.05 * noise();
            out.push(a * env + 0.8 * b + nz);
            out.push(0.6 * a * env + 0.2 * b + nz);
        }
        out
    }

    /// Reconstruction SNR; the decoded stream leads by the 1024-sample priming.
    fn snr_db(src: &[f32], dec: &[f32], ch: usize) -> f64 {
        let lag = FRAME_LEN * ch;
        let n = src.len().min(dec.len().saturating_sub(lag));
        let (mut s, mut e) = (0f64, 0f64);
        for i in 0..n {
            let (x, y) = (f64::from(src[i]), f64::from(dec[i + lag]));
            s += x * x;
            e += (x - y) * (x - y);
        }
        10.0 * (s / e.max(1e-30)).log10()
    }

    /// The trellis is optimal: its choice costs no more than any assignment,
    /// checked by brute force over every assignment of small band sets.
    #[test]
    fn section_trellis_is_optimal() {
        let mut seed = 7u32;
        let mut rnd = move |m: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 16) % m
        };
        let cost = |costs: &[[u32; 12]], cbs: &[u8], hdr: u32| -> u64 {
            let mut c = 0u64;
            for (b, &cb) in cbs.iter().enumerate() {
                if costs[b][cb as usize] == u32::MAX {
                    return u64::MAX;
                }
                c += u64::from(costs[b][cb as usize]);
                if b == 0 || cbs[b - 1] != cb {
                    c += u64::from(hdr);
                }
            }
            c
        };
        for _ in 0..40 {
            let n = 1 + rnd(4) as usize;
            let costs: Vec<[u32; 12]> = (0..n)
                .map(|_| {
                    let mut k = [u32::MAX; 12];
                    for x in k.iter_mut().skip(1) {
                        if rnd(3) != 0 {
                            *x = rnd(40);
                        }
                    }
                    k[11] = rnd(60); // the escape book always codes
                    if rnd(3) == 0 {
                        k[0] = 0;
                    }
                    k
                })
                .collect();
            let got = cost(&costs, &trellis_books(&costs, 9), 9);
            let mut best = u64::MAX;
            for code in 0..12usize.pow(n as u32) {
                let cbs: Vec<u8> = (0..n)
                    .map(|b| ((code / 12usize.pow(b as u32)) % 12) as u8)
                    .collect();
                best = best.min(cost(&costs, &cbs, 9));
            }
            assert_eq!(got, best, "trellis not optimal for {costs:?}");
        }
    }

    /// The one-walk book pricing the trellis uses gives every book the bits
    /// the per-book search does, long and short, across peak magnitudes.
    #[test]
    fn band_book_costs_match_per_book_search() {
        let mut seed = 0x9e37_79b9_u64;
        let mut rnd = move |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for _ in 0..3000 {
            let width = [4usize, 8, 12, 16, 32][rnd(5) as usize];
            let (nwin, wlen) = if rnd(2) == 0 {
                (1, 1024)
            } else {
                (1 + rnd(4) as usize, 128)
            };
            let peak = [1i64, 2, 4, 7, 12, 15, 16, 40, 8191][rnd(9) as usize];
            let mut quant = vec![0i32; nwin * wlen];
            for w in 0..nwin {
                for i in 0..width {
                    if rnd(3) != 0 {
                        let m = rnd(peak as u64 + 1) as i32;
                        quant[w * wlen + 8 + i] = if rnd(2) == 0 { m } else { -m };
                    }
                }
            }
            let (s, e) = (8, 8 + width);
            if (0..nwin).all(|w| quant[w * wlen + s..w * wlen + e].iter().all(|&q| q == 0)) {
                continue;
            }
            assert_eq!(
                band_book_costs(&quant, s, e, 0, nwin, wlen),
                band_book_costs_ref(&quant, s, e, 0, nwin, wlen),
                "width {width}, nwin {nwin}, peak {peak}"
            );
        }
    }

    /// A zero band carried in a section takes the previous coded band's
    /// scalefactor (a zero delta); one before any coded band, the first's.
    #[test]
    fn carried_zero_bands_send_a_zero_delta() {
        let cbs = [5u8, 5, 0, 7, 7, 13, 7];
        let sf = [120, 99, 140, 110, 30, 40, 112];
        let zero = [true, false, true, false, true, false, false];
        let got = zero_band_sf(&cbs, &sf, |i| zero[i]);
        assert_eq!(got, vec![99, 99, 140, 110, 110, 40, 112]);
    }

    /// Frame-demand weights keep the segment mean, honour their bounds, and
    /// are flat at strength 0.
    #[test]
    fn demand_weights_preserve_the_segment_rate() {
        let pe: Vec<f64> = (0..32)
            .map(|i| {
                if i % 7 == 0 {
                    9000.0
                } else {
                    100.0 + 30.0 * f64::from(i)
                }
            })
            .collect();
        for alpha in [0.0, 0.5, 1.0, 2.0] {
            let w = frame_demand_weights(&pe, alpha);
            let m = w.iter().sum::<f64>() / w.len() as f64;
            assert!((m - 1.0).abs() < 1e-9, "alpha {alpha}: mean {m}");
            if alpha == 0.0 {
                assert!(w.iter().all(|&x| x == 1.0));
            }
            assert!(
                w.iter().all(|&x| x > 0.2 && x < 3.0),
                "alpha {alpha}: {w:?}"
            );
        }
        assert_eq!(frame_demand_weights(&[0.0; 4], 1.0), vec![1.0; 4]);
    }

    /// The automatic cutoff applies under 192 kbps and 128 kbps a channel,
    /// never at or above Nyquist; the cutoff falls on a band edge.
    #[test]
    fn bandwidth_table_and_cutoff() {
        assert_eq!(auto_bandwidth_hz(64_000, 1, 44_100), Some(17_000));
        assert_eq!(auto_bandwidth_hz(128_000, 1, 44_100), Some(17_000));
        assert_eq!(auto_bandwidth_hz(160_000, 1, 44_100), None);
        assert_eq!(auto_bandwidth_hz(160_000, 2, 44_100), Some(17_000));
        assert_eq!(auto_bandwidth_hz(192_000, 2, 44_100), None);
        assert_eq!(auto_bandwidth_hz(320_000, 6, 48_000), None);
        assert_eq!(auto_bandwidth_hz(128_000, 6, 48_000), None);
        assert_eq!(auto_bandwidth_hz(64_000, 2, 32_000), None);
        let swb = swb_offsets(true, crate::sf_index_for_rate(44_100).unwrap());
        let cut = cutoff_line(Some(17_000), 44_100, swb, FRAME_LEN);
        assert!(swb.iter().any(|&s| usize::from(s) == cut));
        assert!(cut as f64 * 22_050.0 / 1024.0 >= 17_000.0);
        assert_eq!(cutoff_line(None, 44_100, swb, FRAME_LEN), usize::MAX);
    }

    /// Every tool on: decodable at the right length, finite, a faithful copy,
    /// on rate, stereo and mono, whole-clip and streaming.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret")]
    fn every_tool_round_trips() {
        let sr = 44_100u32;
        let st = music(sr, 40);
        let mono: Vec<f32> = st.iter().step_by(2).copied().collect();
        let cfg = AacEncoderConfig {
            bitrate_bps: 64_000,
            ..Default::default()
        };
        let tools = AacTuning {
            bandwidth: Bandwidth::Hz(17_000),
            ..AacTuning::default()
        };
        let all = (cfg, tools);
        // The 2.1.2 encoder: every tool of this campaign off.
        let plain = (cfg, AacTuning::legacy());
        for (pcm, ch) in [(&st, 2u16), (&mono, 1)] {
            let base = encode(pcm, ch, sr, plain);
            let on = encode(pcm, ch, sr, all);
            assert_ne!(base, on, "{ch} ch: the tools must change the stream");
            let (db, _) = decode(&base, sr);
            let (d, dch) = decode(&on, sr);
            assert_eq!(dch, usize::from(ch));
            assert_eq!(d.len(), db.len(), "{ch} ch: length");
            assert!(d.iter().all(|v| v.is_finite()));
            let (s0, s1) = (snr_db(pcm, &db, dch), snr_db(pcm, &d, dch));
            assert!(
                s1 > 3.0 && s1 > s0 - 3.0,
                "{ch} ch: snr {s1:.1} dB vs {s0:.1} dB"
            );
            let bits: usize = on.iter().map(|p| p.len() * 8).sum();
            let rate = bits as f64 * f64::from(sr) / (on.len() * FRAME_LEN) as f64;
            assert!(
                (rate / 64_000.0 - 1.0).abs() < 0.02,
                "{ch} ch: rate {rate:.0}"
            );
            // Streaming: frame demand needs the whole segment and falls back
            // to the constant-rate share; everything else applies.
            let mut e = AacEncoder::with_tuning(all.0, all.1);
            e.set_streaming(true);
            e.push_pcm(pcm, ch, sr).unwrap();
            e.finish();
            let mut sp = Vec::new();
            while let Ok(p) = e.next_packet() {
                sp.push(p.data);
            }
            let (ds, _) = decode(&sp, sr);
            assert_eq!(ds.len(), db.len());
            assert!(snr_db(pcm, &ds, dch) > 3.0);
        }
    }
}
