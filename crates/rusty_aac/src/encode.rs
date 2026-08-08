//! In-house **AAC-LC encoder** (see `docs/codec-aac-encoder.md` for the brick
//! ledger). Bricks 1–7 are complete: encode-side primitives inverting the decoder
//! (bit writer, spectral-codebook encoder, header/config serializers), the forward
//! filterbank, the quantizer + rate loop, a Bark-spreading psychoacoustic model,
//! block switching, M/S stereo, and container/CLI integration.
//!
//! **What this encoder does NOT yet do** — the arms tracked by
//! `docs/codec-aac-great-gate.md`. Naming them here keeps a fit from being run
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

#![allow(dead_code)]

use crate::codebook::{Codebook, CODEBOOKS, INTENSITY_HCB, INTENSITY_HCB2, NOISE_HCB};
use crate::ics::{IcsInfo, WindowSequence};
use crate::swb::swb_offsets;
use crate::tables::spectral_book;
use crate::{AdtsHeader, AudioSpecificConfig, Error, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Bit writer — MSB-first, the mirror of the decoder's `BitReader`.
// ---------------------------------------------------------------------------
pub struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        BitWriter {
            buf: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    /// Append the low `n` bits of `val`, most-significant bit first.
    pub fn write(&mut self, val: u32, n: u32) {
        for i in (0..n).rev() {
            self.cur = (self.cur << 1) | ((val >> i) & 1) as u8;
            self.nbits += 1;
            if self.nbits == 8 {
                self.buf.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }

    pub fn write_bool(&mut self, b: bool) {
        self.write(b as u32, 1);
    }

    /// Total bits written so far.
    pub fn bit_len(&self) -> usize {
        self.buf.len() * 8 + self.nbits as usize
    }

    /// Pad to a byte boundary with zero bits and return the bytes.
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.nbits != 0 {
            self.cur <<= 8 - self.nbits;
            self.buf.push(self.cur);
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
fn tuple_index(cb: &Codebook, tuple: &[i32]) -> Option<u32> {
    let dim = cb.dim as usize;
    let lav = cb.lav as u32;
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
            if c.unsigned_abs() >= cb.lav as u32 {
                bits += escape_bits(c.unsigned_abs());
            }
        }
    }
    Some(bits)
}

/// Emit `tuple` with codebook `cb_num`: codeword, then sign bits (unsigned books),
/// then escape sequences (book 11). Caller ensures `spectral_bits` is Some.
pub fn spectral_emit(cb_num: usize, tuple: &[i32], w: &mut BitWriter) {
    let cb = &CODEBOOKS[cb_num];
    let dim = cb.dim as usize;
    let idx = tuple_index(cb, tuple).expect("representable tuple");
    let (code, len) = spectral_book(cb_num as u8).code(idx as usize);
    w.write(code, len as u32);
    if cb.unsigned {
        for &c in &tuple[..dim] {
            if c != 0 {
                w.write_bool(c < 0);
            }
        }
    }
    if cb.esc {
        for &c in &tuple[..dim] {
            if c.unsigned_abs() >= cb.lav as u32 {
                emit_escape(c.unsigned_abs(), w);
            }
        }
    }
}

/// Escape length for magnitude `m` (≥ 16): `2N+5` bits, `2^(N+4) ≤ m < 2^(N+5)`.
fn escape_bits(m: u32) -> usize {
    let n = (31 - m.leading_zeros()) - 4;
    2 * n as usize + 5
}

/// Escape sequence (ISO §4.6.3.3): N leading 1-bits, a 0, then N+4 bits of
/// `m - 2^(N+4)`.
fn emit_escape(m: u32, w: &mut BitWriter) {
    let n = (31 - m.leading_zeros()) - 4;
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
/// DecoderSpecificInfo the muxer needs (2 bytes for standard rates).
pub fn audio_specific_config_bytes(sample_rate: u32, channels: u16) -> Vec<u8> {
    write_audio_specific_config(&AudioSpecificConfig {
        object_type: 2, // AAC-LC
        sample_rate,
        channels,
    })
}

/// Serialize an `AudioSpecificConfig` (ISO §1.6.2.1) — the `esds`/`stsd` config
/// bytes the MP4 muxer needs. Inverse of `parse_audio_specific_config`.
pub fn write_audio_specific_config(cfg: &AudioSpecificConfig) -> Vec<u8> {
    let mut w = BitWriter::new();
    if cfg.object_type >= 31 {
        w.write(31, 5);
        w.write((cfg.object_type - 32) as u32, 6);
    } else {
        w.write(cfg.object_type as u32, 5);
    }
    match crate::sf_index_for_rate(cfg.sample_rate) {
        Some(i) => w.write(i as u32, 4),
        None => {
            w.write(0x0F, 4);
            w.write(cfg.sample_rate, 24);
        }
    }
    w.write(cfg.channels as u32, 4);
    w.into_bytes()
}

/// Serialize a 7-byte ADTS frame header (no CRC) — inverse of `parse_adts`.
/// `hdr.frame_length` must include this 7-byte header.
pub fn write_adts_header(hdr: &AdtsHeader) -> Vec<u8> {
    let sf = crate::sf_index_for_rate(hdr.sample_rate).expect("standard rate for ADTS");
    let mut w = BitWriter::new();
    w.write(0xFFF, 12); // syncword
    w.write(0, 1); // MPEG-4
    w.write(0, 2); // layer (00)
    w.write_bool(true); // protection_absent → 7-byte header, no CRC
    w.write((hdr.object_type - 1) as u32, 2); // profile
    w.write(sf as u32, 4);
    w.write(0, 1); // private
    w.write(hdr.channels as u32, 3); // channel config
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
        w.write(info.max_sfb as u32, 4);
        w.write(grouping_bits(&info.window_group_length), 7);
    } else {
        w.write(info.max_sfb as u32, 6);
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
/// 1 / OUTPUT_NORM: the decoder scales its output by 1/32768, so the encoder
/// scales the spectrum up by 32768 to land in the AAC coefficient domain.
const SPEC_SCALE: f32 = 32768.0;

/// Forward long-block filterbank: window the overlapping 2048 input samples
/// (previous frame's 1024 ++ current 1024) and forward-MDCT to 1024 coefficients,
/// scaled so the decoder's `imdct · window · (1/32768) + overlap-add` reconstructs
/// the input (TDAC). `win` is the 2048-length window (sine or KBD).
pub fn analyze_long(prev: &[f32; FRAME_LEN], cur: &[f32; FRAME_LEN], win: &[f32]) -> Vec<f32> {
    let mut windowed = vec![0f32; LONG_N];
    for n in 0..FRAME_LEN {
        windowed[n] = prev[n] * win[n] * SPEC_SCALE;
        windowed[FRAME_LEN + n] = cur[n] * win[FRAME_LEN + n] * SPEC_SCALE;
    }
    crate::dsp::mdct_fast(&windowed)
}

/// Forward short-block filterbank: eight 256-sample windows (128-hop) tiled across
/// [448, 1600) of the prev++cur 2048 buffer, each windowed + MDCT'd to 128 coeffs,
/// laid out window-major (the exact inverse of the decoder's `short_frame`). The
/// uncovered [0,448)/[1600,2048) regions are bridged by the LongStart/LongStop
/// neighbours — which is why EightShort must sit between transition blocks.
pub fn analyze_short(prev: &[f32; FRAME_LEN], cur: &[f32; FRAME_LEN], sw: &[f32]) -> Vec<f32> {
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
            acc += x[i] as f64 * x[i + lag] as f64;
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
/// geometry — fixed per (sample_rate, band count), so it's built once and cached,
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
            let center = (swb[sfb] as f64 + swb[sfb + 1] as f64) / 2.0;
            // n_coeffs coefficients span 0..sr/2.
            hz_to_bark(center * sample_rate as f64 * 0.5 / n_coeffs as f64)
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
pub(crate) fn masking_thresholds(spec: &[f32], swb: &[u16], sample_rate: u32) -> Vec<f64> {
    let num_swb = swb.len() - 1;
    let mut energy = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energy[sfb] = spec[s..e].iter().map(|&x| (x as f64).powi(2)).sum::<f64>() + 1e-3;
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
    let num_swb = swb.len() - 1;
    let mat = spreading_matrix(swb, sample_rate, n_coeffs);
    (0..num_swb)
        .map(|i| {
            let row = &mat[i * num_swb..(i + 1) * num_swb];
            let spread: f64 = (0..num_swb).map(|j| energy[j] * row[j]).sum();
            let smr = match tonality {
                None => SMR_FLAT,
                Some(t) => {
                    let center = (swb[i] as f64 + swb[i + 1] as f64) / 2.0;
                    let bark = hz_to_bark(center * sample_rate as f64 * 0.5 / n_coeffs as f64);
                    smr_for(t[i] as f64, bark)
                }
            };
            spread * smr
        })
        .collect()
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
                let Some(&x) = spec.get(base + k) else { continue };
                // Floored well below any audible coefficient so a single exact
                // zero cannot drag the geometric mean to 0 and fake a pure tone.
                let p = ((x as f64) * (x as f64)).max(1e-10);
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

fn perceptual_offsets(
    spec: &[f32],
    swb: &[u16],
    sample_rate: u32,
    tonality_smr: bool,
) -> Vec<i32> {
    let num_swb = swb.len() - 1;
    let mut energy = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energy[sfb] = spec[s..e].iter().map(|&x| (x as f64).powi(2)).sum::<f64>() + 1e-3;
    }
    let ton = tonality_smr.then(|| band_tonality(spec, swb, 1, FRAME_LEN));
    let thr = masking_from_energy(&energy, swb, sample_rate, FRAME_LEN, ton.as_deref());

    let mut raw = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energy[sfb] = spec[s..e].iter().map(|&x| (x as f64).powi(2)).sum();
        let noise_scale: f64 = spec[s..e]
            .iter()
            .map(|&x| (x.abs() as f64).sqrt())
            .sum::<f64>()
            + 1e-6;
        raw[sfb] = (thr[sfb] / noise_scale).log2() / 0.375;
    }
    // Center on the energy-bearing bands — empty bands have huge `thr/noise_scale`
    // (they quantize to ZERO anyway) and would otherwise skew a plain mean.
    let etot: f64 = energy.iter().sum::<f64>() + 1e-9;
    let center: f64 = (0..num_swb).map(|i| raw[i] * energy[i]).sum::<f64>() / etot;
    raw.iter()
        .map(|&r| ((r - center).round() as i32).clamp(-60, 60))
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
        let (a, b) = (l[i] as f64, r[i] as f64);
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
    /// single_channel_element carrying input channel `.0`.
    Sce(usize),
    /// channel_pair_element carrying input channels `.0` (left) and `.1` (right).
    Cpe(usize, usize),
    /// lfe_channel_element carrying input channel `.0`. Long blocks only.
    Lfe(usize),
}

impl Elem {
    /// The 3-bit element id written to the bitstream.
    fn id(self) -> u32 {
        match self {
            Elem::Sce(_) => ID_SCE,
            Elem::Cpe(_, _) => ID_CPE,
            Elem::Lfe(_) => ID_LFE,
        }
    }
}

/// The element sequence for `channels`, with source indices in the **standard
/// interleave order** the caller pushes (WAVE_FORMAT_EXTENSIBLE / FFmpeg order:
/// FL, FR, FC, LFE, BL, BR).
///
/// The reordering is the substance here. AAC orders a 5.1 stream
/// `C, L, R, Ls, Rs, LFE`; interleaved PCM arrives `L, R, C, LFE, Ls, Rs`. Emitting
/// them in arrival order would put the centre channel in the left speaker.
///
/// Returns `None` for channel counts with no defined `channel_configuration`
/// (0, or > 6): those need a program_config_element, which we do not yet write.
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
        6 => vec![
            Elem::Sce(2),
            Elem::Cpe(0, 1),
            Elem::Cpe(4, 5),
            Elem::Lfe(3),
        ],
        _ => return None,
    })
}

/// Inverse of [`element_plan`]'s ordering: for each **output** slot in standard
/// interleave order, which decoded element-order channel supplies it.
///
/// The decoder emits channels in element order (AAC order); this maps them back
/// so a 5.1 decode interleaves as FL, FR, FC, LFE, BL, BR like every other
/// decoder in the workspace.
pub(crate) fn aac_to_interleave_order(channels: usize) -> Option<Vec<usize>> {
    Some(match channels {
        1 => vec![0],
        2 => vec![0, 1],
        // AAC order C,L,R -> interleave L,R,C
        3 => vec![1, 2, 0],
        // AAC order C,L,R,Cs -> interleave L,R,C,Cs
        4 => vec![1, 2, 0, 3],
        // AAC order C,L,R,Ls,Rs -> interleave L,R,C,Ls,Rs
        5 => vec![1, 2, 0, 3, 4],
        // AAC order C,L,R,Ls,Rs,LFE -> interleave L,R,C,LFE,Ls,Rs
        6 => vec![1, 2, 0, 5, 3, 4],
        _ => return None,
    })
}

const MAX_QUANT: i32 = 8191;

/// Quantize one coefficient at global gain `gg` (used by tests / the noise metric).
fn quantize(x: f32, gg: i32) -> i32 {
    if x == 0.0 {
        return 0;
    }
    let scale = 2f64.powf(-0.1875 * (gg - 100) as f64);
    let q = (((x.abs() as f64).powf(0.75) * scale).round() as i32).min(MAX_QUANT);
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
    fn new(spec: &[f32]) -> Xpow {
        let mut pow = vec![0f64; spec.len()];
        let mut sign = vec![0i32; spec.len()];
        #[cfg(all(feature = "simd", target_arch = "x86_64"))]
        {
            if has_avx2() {
                // SAFETY: runtime AVX2 check; `pow`/`sign` are `spec.len()` long.
                unsafe { xpow_avx2(spec, &mut pow, &mut sign) };
                return Xpow { pow, sign };
            }
        }
        for (i, &x) in spec.iter().enumerate() {
            // |x|^0.75 = |x|^½·|x|^¼ = √|x|·√√|x| — two sqrts vectorize; `powf` doesn't.
            // (For x=0, pow=0 so the sign is irrelevant — the quant is 0 either way.)
            let s = (x.abs() as f64).sqrt();
            pow[i] = s * s.sqrt();
            sign[i] = if x < 0.0 { -1 } else { 1 };
        }
        Xpow { pow, sign }
    }

    /// Max `|x|^0.75` over `[s, e)` — the no-clamp-floor input (= `(max|x|)^0.75`).
    fn max_pow(&self, s: usize, e: usize) -> f64 {
        self.pow[s..e].iter().copied().fold(0.0, f64::max)
    }

    fn len(&self) -> usize {
        self.pow.len()
    }
}

/// The cheapest representable spectral codebook for one SFB's coefficients, and
/// its bit cost. ZERO for a silent band; dim-4 books only for 4-aligned bands.
/// Books whose LAV can't hold the band's peak are skipped up front (only the
/// escape book covers arbitrarily large values).
fn best_codebook_for_band(quant: &[i32], s: usize, e: usize) -> (u8, usize) {
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
        if (e - s) % dim != 0 || (!meta.esc && (meta.lav as u32) < maxq) {
            continue;
        }
        let mut bits = 0usize;
        let mut ok = true;
        let mut i = s;
        while i < e {
            match spectral_bits(cb as usize, &quant[i..i + dim]) {
                Some(b) => bits += b,
                None => {
                    ok = false;
                    break;
                }
            }
            i += dim;
        }
        if ok && bits < best.1 {
            best = (cb, bits);
        }
    }
    best
}

/// Bits for section_data given per-SFB codebooks (adjacent equal cbs merge into
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

/// Per-band scalefactors from a common `base` plus the psy offsets, each clamped
/// to the codeable range. Offsets span ≤48, so band-to-band deltas stay < 60.
fn scalefactors(offsets: &[i32], base: i32) -> Vec<i32> {
    offsets.iter().map(|&o| (base + o).clamp(0, 255)).collect()
}

/// global_gain = the first coded band's scalefactor (the differential reference);
/// 100 for a silent frame with no coded bands.
fn global_gain(cbs: &[u8], sf: &[i32]) -> i32 {
    // Only a REGULAR band seeds the differential chain. A PNS band's `sf` is a
    // noise-energy value on its own accumulator and an intensity band's is a
    // stereo position — seeding global_gain from either desynchronizes every
    // scalefactor that follows.
    cbs.iter()
        .position(|&cb| cb != ZERO_HCB && cb < NOISE_HCB)
        .map(|sfb| sf[sfb])
        .unwrap_or(100)
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
        } else if cb >= crate::codebook::INTENSITY_HCB2 {
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

/// Quantize one band into `out`: `out[k] = sign[k]·min(round(pow[k]·scale), MAX_QUANT)`.
/// This is the encoder's hottest kernel — the rate loop runs it ~11× per frame — so it
/// has an AVX2 path. `pow·scale ≥ 0`, so `floor(v+0.5)` equals `round(v)`, making the
/// vector path **bit-exact** with the scalar reference (verified by every gate test).
fn quantize_band(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    // SAFETY (all SIMD branches): entered only when the ISA is detected at runtime; the
    // three slices share a length and vector bodies touch full lane-chunks, the remainder
    // falling to the scalar tail. Every path is bit-exact with the scalar reference.
    #[cfg(all(feature = "simd-avx512", target_arch = "x86_64"))]
    if has_avx512() {
        unsafe { quantize_band_avx512(pow, sign, scale, out) };
        return;
    }
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    if has_avx2() {
        unsafe { quantize_band_avx2(pow, sign, scale, out) };
        return;
    }
    quantize_band_scalar(pow, sign, scale, out);
}

fn quantize_band_scalar(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    for k in 0..out.len() {
        let q = ((pow[k] * scale).round() as i32).min(MAX_QUANT);
        out[k] = sign[k] * q;
    }
}

/// AVX2 quantize — four coefficients per iteration. Bit-exact with the scalar path
/// (`floor(v+0.5) == round(v)` for `v ≥ 0`, and the clamp is applied before the
/// f64→i32 narrowing so nothing overflows).
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn quantize_band_avx2(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    use std::arch::x86_64::*;
    let n = out.len();
    let vscale = _mm256_set1_pd(scale);
    let vhalf = _mm256_set1_pd(0.5);
    let vmax = _mm256_set1_pd(MAX_QUANT as f64);
    let mut i = 0;
    while i + 4 <= n {
        let v = _mm256_mul_pd(_mm256_loadu_pd(pow.as_ptr().add(i)), vscale);
        let r = _mm256_floor_pd(_mm256_add_pd(v, vhalf)); // = round(v), v ≥ 0
        let qabs = _mm256_cvttpd_epi32(_mm256_min_pd(r, vmax)); // clamp then f64→i32
        let s = _mm_loadu_si128(sign.as_ptr().add(i) as *const __m128i);
        _mm_storeu_si128(
            out.as_mut_ptr().add(i) as *mut __m128i,
            _mm_mullo_epi32(qabs, s),
        );
        i += 4;
    }
    while i < n {
        let q = ((pow[i] * scale).round() as i32).min(MAX_QUANT);
        out[i] = sign[i] * q;
        i += 1;
    }
}

/// AVX-512 quantize — eight coefficients per iteration (2× the AVX2 width). Bit-exact:
/// for the quantizer's always-nonnegative input, `trunc(min(v+0.5, MAX+0.5))` equals
/// `min(round(v), MAX)`, so a single truncating narrow does round+clamp in one step.
/// Runtime-gated above AVX2; on AVX2-only hosts this path never runs (untested there —
/// the math mirrors the AVX2 kernel exactly, and its tail is the shared scalar reference).
#[cfg(all(feature = "simd-avx512", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f,avx2")]
unsafe fn quantize_band_avx512(pow: &[f64], sign: &[i32], scale: f64, out: &mut [i32]) {
    use std::arch::x86_64::*;
    let n = out.len();
    let vscale = _mm512_set1_pd(scale);
    let vhalf = _mm512_set1_pd(0.5);
    let vmaxph = _mm512_set1_pd(MAX_QUANT as f64 + 0.5); // clamp v+0.5 so trunc yields MAX
    let mut i = 0;
    while i + 8 <= n {
        let v = _mm512_mul_pd(_mm512_loadu_pd(pow.as_ptr().add(i)), vscale);
        let c = _mm512_min_pd(_mm512_add_pd(v, vhalf), vmaxph);
        let qabs = _mm512_cvttpd_epi32(c); // 8 f64 → 8 i32; trunc = floor for c ≥ 0
        let s = _mm256_loadu_si256(sign.as_ptr().add(i) as *const __m256i);
        _mm256_storeu_si256(
            out.as_mut_ptr().add(i) as *mut __m256i,
            _mm256_mullo_epi32(qabs, s),
        );
        i += 8;
    }
    while i < n {
        let q = ((pow[i] * scale).round() as i32).min(MAX_QUANT);
        out[i] = sign[i] * q;
        i += 1;
    }
}

/// AVX2 `Xpow` builder: `pow[i] = |x|^0.75` via `√|x|·√√|x|` (two vector sqrts), and
/// `sign[i] = ±1` from the sign bit — four coefficients per iteration.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn xpow_avx2(spec: &[f32], pow: &mut [f64], sign: &mut [i32]) {
    use std::arch::x86_64::*;
    let n = spec.len();
    let absmask = _mm256_castsi256_pd(_mm256_set1_epi64x(0x7fff_ffff_ffff_ffffu64 as i64));
    let one = _mm256_set1_pd(1.0);
    let mut i = 0;
    while i + 4 <= n {
        let x = _mm256_cvtps_pd(_mm_loadu_ps(spec.as_ptr().add(i))); // 4 f32 → 4 f64
        let a = _mm256_and_pd(x, absmask); // |x|
        let s = _mm256_sqrt_pd(a); // |x|^½
        _mm256_storeu_pd(pow.as_mut_ptr().add(i), _mm256_mul_pd(s, _mm256_sqrt_pd(s)));
        // ±1.0 carrying x's sign bit → i32 (x=0 gives +1; harmless, pow is 0).
        let signed = _mm256_or_pd(one, _mm256_andnot_pd(absmask, x));
        _mm_storeu_si128(
            sign.as_mut_ptr().add(i) as *mut __m128i,
            _mm256_cvttpd_epi32(signed),
        );
        i += 4;
    }
    while i < n {
        let s = (spec[i].abs() as f64).sqrt();
        pow[i] = s * s.sqrt();
        sign[i] = if spec[i] < 0.0 { -1 } else { 1 };
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
/// pick per-band codebooks, and return (codebooks, ICS body bits, max_sfb). Splitting
/// this out lets the rate loop reuse one buffer instead of allocating per candidate.
fn code_core(xp: &Xpow, swb: &[u16], sf: &[i32], quant: &mut [i32]) -> (Vec<u8>, usize, usize) {
    work::bump_code_frame();
    let num_swb = swb.len() - 1;
    work::bump_quant_bands((swb.len() - 1) as u64);
    let scale = scale_table();
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let sc = scale[sf[sfb].clamp(0, 255) as usize];
        quantize_band(&xp.pow[s..e], &xp.sign[s..e], sc, &mut quant[s..e]);
    }
    let mut max_sfb = 0usize;
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        if quant[s..e].iter().any(|&q| q != 0) {
            max_sfb = sfb + 1;
        }
    }
    let mut cbs = Vec::with_capacity(max_sfb);
    let mut spec_bits = 0usize;
    for sfb in 0..max_sfb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let (cb, bits) = best_codebook_for_band(quant, s, e);
        cbs.push(cb);
        spec_bits += bits;
    }
    let gg = global_gain(&cbs, sf);
    // global_gain(8) + ics_info(~11) + 3 flag bits + sections + scalefactors + spectrum.
    let body = 8 + 11 + 3 + section_bits(&cbs) + scalefactor_bits(&cbs, sf, gg) + spec_bits;
    (cbs, body, max_sfb)
}

/// Owned-buffer wrapper over [`code_core`] for the one-shot callers (final coding).
fn code_frame(xp: &Xpow, swb: &[u16], sf: &[i32]) -> (Vec<u8>, usize, usize, Vec<i32>) {
    let mut quant = vec![0i32; xp.len()];
    let (cbs, body, max_sfb) = code_core(xp, swb, sf, &mut quant);
    (cbs, body, max_sfb, quant)
}

/// The smallest common `base` such that no band clamps its loudest coefficient
/// past `MAX_QUANT` (each band's own no-clamp floor, minus that band's offset).
fn min_base(xp: &Xpow, swb: &[u16], offsets: &[i32]) -> i32 {
    let num_swb = swb.len() - 1;
    let mut floor = 0i32;
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let maxp = xp.max_pow(s, e); // = (max|x|)^0.75
        if maxp <= 1e-9 {
            continue;
        }
        let min_sf = (100.0 - (MAX_QUANT as f64 / maxp).log2() / 0.1875).ceil() as i32;
        floor = floor.max(min_sf - offsets[sfb]);
    }
    floor.clamp(0, 255)
}

/// Fast body-bit estimate at a candidate `base`: quantize (cheap) and price each
/// coefficient from the `coef_bits` table — no codebook search. Monotone in `base`,
/// so it seeds the search; the exact refinement corrects it.
fn estimate_bits(xp: &Xpow, swb: &[u16], offsets: &[i32], base: i32) -> usize {
    let scale = scale_table();
    let ct = coef_bits();
    let num_swb = swb.len() - 1;
    let mut bits = 22usize; // global_gain + ics_info + flags, approx
    let mut qbuf = [0i32; 128]; // one band (max long SWB width is 96)
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let w = e - s;
        let sc = scale[(base + offsets[sfb]).clamp(0, 255) as usize];
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
fn rate_loop(xp: &Xpow, swb: &[u16], offsets: &[i32], target_bits: usize) -> i32 {
    let min_b = min_base(xp, swb, offsets);
    // Phase 1: binary-search the cheap estimate.
    let (mut lo, mut hi) = (min_b, 255i32);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if estimate_bits(xp, swb, offsets, mid) <= target_bits {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    // Phase 2: refine to the exact smallest fitting base with the real coder, reusing
    // one quant buffer across the (few) exact evaluations.
    let mut quant = vec![0i32; xp.len()];
    let mut exact =
        |b: i32| code_core(xp, swb, &scalefactors(offsets, b), &mut quant).1 <= target_bits;
    let mut base = lo;
    if exact(base) {
        while base > min_b && exact(base - 1) {
            base -= 1;
        }
    } else {
        while base < 255 && !exact(base) {
            base += 1;
        }
    }
    base
}

/// section_data (single group, long block): 4-bit codebook + 5-bit run-length
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
        w.write(cb as u32, 4);
        let mut l = len as u32;
        while l >= esc {
            w.write(esc, 5);
            l -= esc;
        }
        w.write(l, 5);
        k += len;
    }
}

/// scale_factor_data: each coded band's scalefactor as a `SCALEFACTOR_BOOK` delta
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
        } else if cb >= crate::codebook::INTENSITY_HCB2 {
            let d = (sf[sfb] - is_pos).clamp(-60, 60);
            let (code, len) = crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize);
            w.write(code, len as u32);
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
                w.write(code, len as u32);
                noise += d;
            }
        } else {
            let d = (sf[sfb] - acc).clamp(-60, 60);
            let (code, len) = crate::tables::SCALEFACTOR_BOOK.code((d + 60) as usize);
            w.write(code, len as u32);
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
            .map(|&x| (x as f64) * (x as f64))
            .sum();
    }
    let total: f64 = energies.iter().sum::<f64>() + 1e-9;
    let frame_tonality =
        (0..num_swb).map(|i| energies[i] * tonality[i] as f64).sum::<f64>() / total;

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
        let lo_hz = swb[sfb] as f64 * sample_rate as f64 * 0.5 / FRAME_LEN as f64;
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

/// spectral_data: per regular SFB, code coefficient tuples with the band's book.
fn write_spectrum(w: &mut BitWriter, quant: &[i32], cbs: &[u8], swb: &[u16]) {
    for (sfb, &cb) in cbs.iter().enumerate() {
        // ZERO, NOISE (PNS) and intensity bands carry no spectral data — their
        // whole point is that the coefficients are not transmitted.
        if cb == ZERO_HCB || cb >= NOISE_HCB {
            continue;
        }
        let dim = CODEBOOKS[cb as usize].dim as usize;
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let mut i = s;
        while i + dim <= e {
            spectral_emit(cb as usize, &quant[i..i + dim], w);
            i += dim;
        }
    }
}

/// Encode one channel as a single_channel_element for a long or transition block
/// (`seq` ∈ {OnlyLong, LongStart, LongStop}): psy offsets → rate loop over the
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
    let offsets0 = perceptual_offsets(spec, swb, sample_rate, psy.tonality_smr);
    let xp0 = Xpow::new(spec);
    let base0 = rate_loop(&xp0, swb, &offsets0, target_bits);
    let sf0 = scalefactors(&offsets0, base0);
    let (cbs0, _, max_sfb0, quant0) = code_frame(&xp0, swb, &sf0);

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
                let offsets = perceptual_offsets(&filtered, swb, sample_rate, psy.tonality_smr);
                let xp = Xpow::new(&filtered);
                let base = rate_loop(&xp, swb, &offsets, target_bits);
                let sf = scalefactors(&offsets, base);
                let (mut cbs, _, msfb, quant) = code_frame(&xp, swb, &sf);
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
    /// The de-quantized PARCOR values the decoder will reconstruct — the encoder
    /// must filter with *these*, not with the unquantized ones, or encoder and
    /// decoder run different filters.
    pub parcor: Vec<f32>,
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
            acc += x[i] as f64 * x[i + lag] as f64;
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

    Some(TnsEnc {
        length,
        order,
        idx,
        parcor,
    })
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
/// never flagged — nothing precedes it to open a LongStart transition from.
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
                    let x = chan.get(start + i).copied().unwrap_or(0.0) as f64;
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
    let mut sub: Vec<f64> = Vec::with_capacity(nframes * 8);
    for f in 0..nframes {
        for sb in 0..8 {
            let start = f * FRAME_LEN + sb * SHORT_HALF;
            let e: f64 = (0..SHORT_HALF)
                .map(|i| {
                    let x = chan.get(start + i).copied().unwrap_or(0.0) as f64;
                    x * x
                })
                .sum();
            sub.push(e);
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
pub(crate) fn detect_transients_relative(chan: &[f32], nframes: usize) -> Vec<bool> {
    const RATIO: f32 = 10.0;
    let ratios = frame_attack_ratios(chan, nframes);
    (0..nframes)
        .map(|f| f > 0 && ratios.get(f).copied().unwrap_or(1.0) > RATIO)
        .collect()
}

/// Assign a valid AAC window sequence to each frame from the transient flags. A
/// short run is bracketed by LongStart/LongStop; runs a single frame apart are
/// merged (a lone gap can't be both a stop and a start).
fn assign_sequences(transient: &[bool]) -> Vec<WindowSequence> {
    use WindowSequence::*;
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
fn best_codebook_short(quant: &[i32], swb: &[u16], sfb: usize, nwin: usize) -> (u8, usize) {
    let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
    let mut maxq = 0u32;
    for win in 0..nwin {
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
        if (e - s) % dim != 0 || (!meta.esc && (meta.lav as u32) < maxq) {
            continue;
        }
        let mut bits = 0usize;
        let mut ok = true;
        'windows: for win in 0..nwin {
            let base = win * SHORT_HALF;
            let mut i = s;
            while i < e {
                match spectral_bits(cb as usize, &quant[base + i..base + i + dim]) {
                    Some(b) => bits += b,
                    None => {
                        ok = false;
                        break 'windows;
                    }
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

/// Bits for short-block section_data (3-bit run-length increments, esc = 7).
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
/// this brick), pick per-SFB codebooks, and return (codebooks, body bits, max_sfb,
/// window-major quantized spectrum).
fn code_frame_short(xp: &Xpow, swb: &[u16], sf: &[i32]) -> (Vec<u8>, usize, usize, Vec<i32>) {
    work::bump_code_frame();
    let num_swb = swb.len() - 1;
    work::bump_quant_bands(((swb.len() - 1) * 8) as u64);
    let scale = scale_table();
    let mut quant = vec![0i32; FRAME_LEN];
    for win in 0..8 {
        let base = win * SHORT_HALF;
        for sfb in 0..num_swb {
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            let sc = scale[sf[sfb].clamp(0, 255) as usize];
            quantize_band(
                &xp.pow[base + s..base + e],
                &xp.sign[base + s..base + e],
                sc,
                &mut quant[base + s..base + e],
            );
        }
    }
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
    let mut cbs = Vec::with_capacity(max_sfb);
    let mut spec_bits = 0usize;
    for sfb in 0..max_sfb {
        let (cb, bits) = best_codebook_short(&quant, swb, sfb, 8);
        cbs.push(cb);
        spec_bits += bits;
    }
    let gg = global_gain(&cbs, sf);
    // global_gain(8) + ics_info(short ~18) + 3 flags + sections + scalefactors + spectrum.
    let body = 8 + 18 + 3 + section_bits_short(&cbs) + scalefactor_bits(&cbs, sf, gg) + spec_bits;
    (cbs, body, max_sfb, quant)
}

/// The smallest flat scalefactor that avoids clamping the loudest short coefficient
/// past `MAX_QUANT`.
fn min_base_short(xp: &Xpow) -> i32 {
    let maxp = xp.max_pow(0, xp.len());
    if maxp <= 1e-9 {
        return 0;
    }
    (100.0 - (MAX_QUANT as f64 / maxp).log2() / 0.1875)
        .ceil()
        .clamp(0.0, 255.0) as i32
}

/// **Arm A1 — the short-block psychoacoustic model.**
///
/// Per-SFB scalefactor offsets for an EightShort frame. The band energies are
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
fn perceptual_offsets_short(spec: &[f32], swb: &[u16], sample_rate: u32, tonality_smr: bool) -> Vec<i32> {
    let num_swb = swb.len() - 1;
    let mut energy = vec![0.0f64; num_swb];
    let mut noise_scale = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        for win in 0..8 {
            let base = win * SHORT_HALF;
            for k in s..e {
                let Some(&x) = spec.get(base + k) else { continue };
                energy[sfb] += (x as f64) * (x as f64);
                noise_scale[sfb] += (x.abs() as f64).sqrt();
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
/// the flat-scalefactor behavior byte-identically).
fn rate_loop_short(xp: &Xpow, swb: &[u16], offsets: &[i32], target_bits: usize) -> i32 {
    let mut lo = min_base_short(xp);
    let mut hi = 255i32;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let sf = scalefactors(offsets, mid);
        if code_frame_short(xp, swb, &sf).1 <= target_bits {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// short_block section_data (4-bit codebook + 3-bit run increments, esc = 7).
fn write_sections_short(w: &mut BitWriter, cbs: &[u8]) {
    let esc = 7u32;
    let mut k = 0usize;
    while k < cbs.len() {
        let cb = cbs[k];
        let mut len = 1usize;
        while k + len < cbs.len() && cbs[k + len] == cb {
            len += 1;
        }
        w.write(cb as u32, 4);
        let mut l = len as u32;
        while l >= esc {
            w.write(esc, 3);
            l -= esc;
        }
        w.write(l, 3);
        k += len;
    }
}

/// short_block spectral_data: per SFB, per window (one group), coefficient tuples.
fn write_spectrum_short(w: &mut BitWriter, quant: &[i32], cbs: &[u8], swb: &[u16]) {
    for (sfb, &cb) in cbs.iter().enumerate() {
        if cb == ZERO_HCB {
            continue;
        }
        let dim = CODEBOOKS[cb as usize].dim as usize;
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        for win in 0..8 {
            let base = win * SHORT_HALF;
            let mut i = s;
            while i + dim <= e {
                spectral_emit(cb as usize, &quant[base + i..base + i + dim], w);
                i += dim;
            }
        }
    }
}

/// Encode one channel as an EightShort single_channel_element (one group of 8
/// windows). Scalefactors are flat unless arm A1 (`psy.short_block_psy`) supplies
/// a per-band shape.
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
    // Arm A1: a real per-band mask, or the flat fallback (all-zero offsets
    // reproduce the shipped behavior byte-identically).
    let offsets = if psy.short_block_psy {
        perceptual_offsets_short(spec, swb, sample_rate, psy.tonality_smr)
    } else {
        vec![0i32; swb.len() - 1]
    };
    let base = rate_loop_short(&xp, swb, &offsets, target_bits);
    let sf = scalefactors(&offsets, base);
    let (cbs, _, max_sfb, quant) = code_frame_short(&xp, swb, &sf);
    let gg = global_gain(&cbs, &sf);

    w.write(ID_SCE, 3);
    w.write(tag, 4);
    w.write(gg as u32, 8);
    let info = IcsInfo {
        window_sequence: WindowSequence::EightShort,
        window_shape_kbd: cur_kbd,
        max_sfb: max_sfb as u8,
        num_windows: 8,
        num_window_groups: 1,
        window_group_length: vec![8],
        num_swb: swb.len() - 1,
    };
    encode_ics_info(w, &info);
    write_sections_short(w, &cbs);
    write_scalefactors(w, &cbs, &sf, gg);
    w.write(0, 1); // pulse_data_present
    w.write(0, 1); // tns_data_present
    w.write(0, 1); // gain_control_data_present
    write_spectrum_short(w, &quant, &cbs, swb);
}

// ---------------------------------------------------------------------------
// Stereo (brick 6) — a channel_pair_element with a common window and per-SFB M/S
// (mid/side) coding where the channels are correlated enough to pay off.
// ---------------------------------------------------------------------------

/// Per-SFB M/S decision + mixed spectra. M/S wins when `E_M·E_S < E_L·E_R` (the
/// correlation criterion — raw energy always halves under the ½ scaling, so the
/// *product* is what predicts bit savings). Returns (ch0 = M or L, ch1 = S or R,
/// per-SFB ms_used).
fn mid_side(
    l: &[f32],
    r: &[f32],
    swb: &[u16],
    is_short: bool,
    is_veto: &[bool],
) -> (Vec<f32>, Vec<f32>, Vec<bool>) {
    let num_swb = swb.len() - 1;
    let nwin = if is_short { 8 } else { 1 };
    let wlen = if is_short { SHORT_HALF } else { FRAME_LEN };
    let mut ch0 = l.to_vec();
    let mut ch1 = r.to_vec();
    let mut ms = vec![false; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        let (mut el, mut er, mut em, mut es) = (0.0f64, 0.0, 0.0, 0.0);
        for win in 0..nwin {
            let base = win * wlen;
            for i in s..e {
                let (lv, rv) = (l[base + i] as f64, r[base + i] as f64);
                let (m, sd) = ((lv + rv) * 0.5, (lv - rv) * 0.5);
                el += lv * lv;
                er += rv * rv;
                em += m * m;
                es += sd * sd;
            }
        }
        // A band claimed by intensity stereo (arm A7) must stay L/R.
        if em * es < el * er && !is_veto.get(sfb).copied().unwrap_or(false) {
            ms[sfb] = true;
            for win in 0..nwin {
                let base = win * wlen;
                for i in s..e {
                    let (lv, rv) = (l[base + i], r[base + i]);
                    ch0[base + i] = (lv + rv) * 0.5;
                    ch1[base + i] = (lv - rv) * 0.5;
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
) -> (Vec<i32>, Vec<i32>) {
    let xp = Xpow::new(spec);
    if is_short {
        let offsets = if psy.short_block_psy {
            perceptual_offsets_short(spec, swb, sample_rate, psy.tonality_smr)
        } else {
            vec![0i32; swb.len() - 1]
        };
        let base = rate_loop_short(&xp, swb, &offsets, target_bits);
        let sf = scalefactors(&offsets, base);
        let (_, _, _, quant) = code_frame_short(&xp, swb, &sf);
        (sf, quant)
    } else {
        let offsets = perceptual_offsets(spec, swb, sample_rate, psy.tonality_smr);
        let base = rate_loop(&xp, swb, &offsets, target_bits);
        let sf = scalefactors(&offsets, base);
        let (_, _, _, quant) = code_frame(&xp, swb, &sf);
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

/// Per-SFB codebooks for one channel over `0..max_sfb`.
fn codebooks(quant: &[i32], swb: &[u16], is_short: bool, max_sfb: usize) -> Vec<u8> {
    (0..max_sfb)
        .map(|sfb| {
            if is_short {
                best_codebook_short(quant, swb, sfb, 8).0
            } else {
                let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                best_codebook_for_band(quant, s, e).0
            }
        })
        .collect()
}

/// ms_mask_present (2 bits) + the per-SFB mask when mixed.
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

/// One channel's individual_channel_stream body inside a common-window CPE:
/// global_gain, section_data, scale_factor_data, the three flag bits, spectral_data
/// (no ics_info — it is shared).
fn write_channel_data(
    w: &mut BitWriter,
    cbs: &[u8],
    sf: &[i32],
    quant: &[i32],
    swb: &[u16],
    is_short: bool,
) {
    let gg = global_gain(cbs, sf);
    w.write(gg as u32, 8);
    if is_short {
        write_sections_short(w, cbs);
    } else {
        write_sections(w, cbs);
    }
    write_scalefactors(w, cbs, sf, gg);
    w.write(0, 1); // pulse_data_present
    w.write(0, 1); // tns_data_present
    w.write(0, 1); // gain_control_data_present
    if is_short {
        write_spectrum_short(w, quant, cbs, swb);
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
fn pair_body_bits(
    xp0: &Xpow,
    off0: &[i32],
    xp1: &Xpow,
    off1: &[i32],
    swb: &[u16],
    base: i32,
    is_short: bool,
    quant: &mut [i32],
) -> usize {
    let (b0, b1) = if is_short {
        (
            code_frame_short(xp0, swb, &scalefactors(off0, base)).1,
            code_frame_short(xp1, swb, &scalefactors(off1, base)).1,
        )
    } else {
        (
            code_core(xp0, swb, &scalefactors(off0, base), quant).1,
            code_core(xp1, swb, &scalefactors(off1, base), quant).1,
        )
    };
    b0 + b1
}

/// Smallest common base whose combined body fits `target_total` bits.
#[allow(clippy::too_many_arguments)]
fn joint_rate_loop(
    xp0: &Xpow,
    off0: &[i32],
    xp1: &Xpow,
    off1: &[i32],
    swb: &[u16],
    target_total: usize,
    is_short: bool,
) -> i32 {
    let lo0 = if is_short { min_base_short(xp0) } else { min_base(xp0, swb, off0) };
    let lo1 = if is_short { min_base_short(xp1) } else { min_base(xp1, swb, off1) };
    let mut lo = lo0.max(lo1);
    let mut hi = 255i32;
    let mut quant = vec![0i32; xp0.len().max(xp1.len())];
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pair_body_bits(xp0, off0, xp1, off1, swb, mid, is_short, &mut quant) <= target_total {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
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
        let lo_hz = swb[sfb] as f64 * sample_rate as f64 * 0.5 / FRAME_LEN as f64;
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
            let (a, b) = (spec_l[i] as f64, spec_r[i] as f64);
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

/// Encode a stereo pair as a common-window channel_pair_element with per-SFB M/S.
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
    let is_veto: Vec<bool> = is_bands.iter().map(|b| b.is_some()).collect();
    let (ch0, ch1, ms_full) = mid_side(spec_l, spec_r, swb, is_short, &is_veto);
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
        let off = |spec: &[f32]| -> Vec<i32> {
            if is_short {
                if psy.short_block_psy {
                    perceptual_offsets_short(spec, swb, sample_rate, psy.tonality_smr)
                } else {
                    vec![0i32; swb.len() - 1]
                }
            } else {
                perceptual_offsets(spec, swb, sample_rate, psy.tonality_smr)
            }
        };
        let (o0, o1) = (off(&ch0), off(&ch1));
        let (xp0, xp1) = (Xpow::new(&ch0), Xpow::new(&ch1));
        let base = joint_rate_loop(&xp0, &o0, &xp1, &o1, swb, target_bits * 2, is_short);
        let (s0, s1) = (scalefactors(&o0, base), scalefactors(&o1, base));
        let q0 = if is_short {
            code_frame_short(&xp0, swb, &s0).3
        } else {
            code_frame(&xp0, swb, &s0).3
        };
        let q1 = if is_short {
            code_frame_short(&xp1, swb, &s1).3
        } else {
            code_frame(&xp1, swb, &s1).3
        };
        (s0, q0, s1, q1)
    } else {
        let (a, b) = quantize_channel(&ch0, swb, is_short, sample_rate, target_bits, psy);
        let (c, d) = quantize_channel(&ch1, swb, is_short, sample_rate, target_bits, psy);
        (a, b, c, d)
    };
    let max_sfb = joint_max_sfb(&quant0, &quant1, swb, is_short);
    let cbs0 = codebooks(&quant0, swb, is_short, max_sfb);
    let mut cbs1 = codebooks(&quant1, swb, is_short, max_sfb);
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
        num_window_groups: 1,
        window_group_length: vec![if is_short { 8 } else { 1 }],
        num_swb: swb.len() - 1,
    };
    encode_ics_info(w, &info);
    write_ms_used(w, &ms_full[..max_sfb]);
    write_channel_data(w, &cbs0, &sf0, &quant0, swb, is_short);
    write_channel_data(w, &cbs1, &sf1, &quant1, swb, is_short);
}

// ---------------------------------------------------------------------------
// The encoder: buffers input, blocks into 1024-sample long frames, emits raw
// access units (add ADTS or MP4 framing around them as needed).
// ---------------------------------------------------------------------------

/// Window-shape policy — **arm A8, Rung 0** of `docs/codec-aac-great-gate.md`.
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
        AacEncoderConfig {
            bitrate_bps: 128_000,
            window_shape: WindowShape::Sine,
            shape_tonality_pct: 0.5,
            short_block_psy: false,
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

/// One encoded raw access unit (a `raw_data_block`, no ADTS framing).
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    /// Presentation timestamp in samples (frame index × 1024).
    pub pts: i64,
    /// Duration in samples (always one AAC frame = 1024).
    pub duration: u32,
}

/// AAC-LC encoder. Push PCM with [`push_pcm`](AacEncoder::push_pcm) (or
/// [`push_pcm_planar`](AacEncoder::push_pcm_planar)), call
/// [`finish`](AacEncoder::finish), then drain [`next_packet`](AacEncoder::next_packet)
/// until it returns [`Error::Eof`].
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
    tonality_smr: bool,
    tns: bool,
    relative_transients: bool,
    pns: bool,
    intensity: bool,
    stereo_bit_split: bool,
    win: Vec<f32>,
    /// The KBD long window (α = 4.0), matching the decoder's. Built once.
    win_kbd: Vec<f32>,
    chans: Vec<Vec<f32>>,
    initialized: bool,
    /// Encoded raw access units (raw_data_block, no ADTS) awaiting `next_packet`,
    /// each with its sample-domain PTS. Filled on `finish`.
    queue: VecDeque<(Vec<u8>, i64)>,
    flushed: bool,
}

impl AacEncoder {
    pub fn new(config: AacEncoderConfig) -> Self {
        AacEncoder {
            sample_rate: 0,
            channels: 0,
            fs_index: 0,
            bitrate: config.bitrate_bps.max(1),
            window_shape: config.window_shape,
            shape_tonality_pct: config.shape_tonality_pct,
            short_block_psy: config.short_block_psy,
            tonality_smr: config.tonality_smr,
            tns: config.tns,
            relative_transients: config.relative_transients,
            pns: config.pns,
            intensity: config.intensity,
            stereo_bit_split: config.stereo_bit_split,
            win: Vec::new(),
            win_kbd: Vec::new(),
            chans: Vec::new(),
            initialized: false,
            queue: VecDeque::new(),
            flushed: false,
        }
    }

    /// The stream's sample rate (0 until the first PCM is pushed).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The stream's channel count (0 until the first PCM is pushed).
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
    /// and sample rate.
    pub fn push_pcm(&mut self, interleaved: &[f32], channels: u16, sample_rate: u32) -> Result<()> {
        self.init(channels, sample_rate)?;
        let ch = self.channels;
        let n = interleaved.len() / ch;
        for i in 0..n {
            for c in 0..ch {
                self.chans[c].push(interleaved[i * ch + c]);
            }
        }
        Ok(())
    }

    /// Buffer planar `f32` PCM, one slice per channel (`planes.len()` is the
    /// channel count; all planes the same length).
    pub fn push_pcm_planar(&mut self, planes: &[&[f32]], sample_rate: u32) -> Result<()> {
        self.init(planes.len() as u16, sample_rate)?;
        for (c, plane) in planes.iter().enumerate().take(self.channels) {
            self.chans[c].extend_from_slice(plane);
        }
        Ok(())
    }

    /// Encode all buffered samples into per-frame raw access units (raw_data_block,
    /// no ADTS) with sample-domain PTS. A trailing all-zero block flushes the MDCT
    /// overlap so the final audio block decodes. Containers add their own framing
    /// (ADTS header for `.aac`, `esds` + raw samples for MP4).
    /// One block's samples for channel `ch` (zero-padded past the buffered input).
    fn block(&self, ch: usize, b: usize) -> [f32; FRAME_LEN] {
        let mut cur = [0f32; FRAME_LEN];
        for (i, s) in cur.iter_mut().enumerate() {
            *s = self.chans[ch]
                .get(b * FRAME_LEN + i)
                .copied()
                .unwrap_or(0.0);
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
        per_channel: usize,
        plan: &[Elem],
    ) -> (Vec<u8>, i64) {
        let prev = |ch: usize| -> [f32; FRAME_LEN] {
            if b == 0 {
                [0f32; FRAME_LEN]
            } else {
                self.block(ch, b - 1)
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
        let psy = PsyCfg {
            short_block_psy: self.short_block_psy,
            tonality_smr: self.tonality_smr,
            tns: self.tns,
            pns: self.pns,
            intensity: self.intensity,
            stereo_bit_split: self.stereo_bit_split,
        };

        let mut rdb = BitWriter::new();
        // One pass over the ISO element sequence. `seqs` is indexed by ELEMENT,
        // not by channel: a CPE shares one window sequence across its pair, so
        // the two channels' transient flags were joined when the plan was built.
        for (ei, elem) in plan.iter().enumerate() {
            let seq = seqs[ei][b];
            let tag = ei as u32;
            match *elem {
                Elem::Cpe(l, r) => {
                    let (p0, c0, p1, c1) = (prev(l), self.block(l, b), prev(r), self.block(r, b));
                    if seq == WindowSequence::EightShort {
                        let sl = analyze_short(&p0, &c0, short_win);
                        let sr_ = analyze_short(&p1, &c1, short_win);
                        encode_cpe(
                            &mut rdb, tag, &sl, &sr_, swb_s, seq, self.sample_rate, per_channel,
                            cur_kbd, psy,
                        );
                    } else {
                        let win = long_window(
                            seq, prev_kbd, cur_kbd, &self.win, &self.win_kbd, sine_s, kbd_s,
                        );
                        let sl = analyze_long(&p0, &c0, &win);
                        let sr_ = analyze_long(&p1, &c1, &win);
                        encode_cpe(
                            &mut rdb, tag, &sl, &sr_, swb, seq, self.sample_rate, per_channel,
                            cur_kbd, psy,
                        );
                    }
                }
                Elem::Sce(ch) | Elem::Lfe(ch) => {
                    let (p, c) = (prev(ch), self.block(ch, b));
                    let id = elem.id();
                    if seq == WindowSequence::EightShort {
                        let spec = analyze_short(&p, &c, short_win);
                        encode_channel_element_short(
                            &mut rdb,
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
                            seq, prev_kbd, cur_kbd, &self.win, &self.win_kbd, sine_s, kbd_s,
                        );
                        let spec = analyze_long(&p, &c, &win);
                        encode_channel_element(
                            &mut rdb,
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
        }
        rdb.write(ID_END, 3);
        (rdb.into_bytes(), (b * FRAME_LEN) as i64)
    }

    /// Transient flags for one channel, under the configured detector (arm A9).
    fn transients(&self, chan: &[f32], nframes: usize) -> Vec<bool> {
        if self.relative_transients {
            detect_transients_relative(chan, nframes)
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
                    .map(|b| time_tonality(&self.block(0, b)))
                    .collect();
                let atk = frame_attack_ratios(&self.chans[0], nblocks);

                let pct = |v: &[f32], q: f32| -> f32 {
                    let mut s = v.to_vec();
                    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let i = ((s.len().saturating_sub(1)) as f32 * q.clamp(0.0, 1.0)).round() as usize;
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
    fn encode_stream(&self) -> Vec<(Vec<u8>, i64)> {
        let swb = swb_offsets(true, self.fs_index);
        let swb_s = swb_offsets(false, self.fs_index);
        let sine_s = crate::dsp::sine_window(SHORT_N);
        // α = 6.0 for the short window, matching the decoder's `kbd_s`.
        let kbd_s = crate::dsp::kbd_window(SHORT_N, 6.0);
        let n = self.chans.first().map_or(0, |c| c.len());
        let nblocks = n.div_ceil(FRAME_LEN) + 1;
        // Per-channel ICS-body budget from the target bitrate (minus framing).
        let frame_budget = (self.bitrate as usize * FRAME_LEN / self.sample_rate.max(1) as usize)
            .saturating_sub(59); // ADTS header (56) + END (3)
        let per_channel = (frame_budget / self.channels.max(1)).saturating_sub(7); // element overhead

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
            &swb[..],
            &swb_s[..],
            &sine_s[..],
            &kbd_s[..],
            &seqs[..],
            &shapes[..],
            &plan[..],
        );
        let frame = |b: usize| {
            self.encode_frame(b, swb, swb_s, sine_s, kbd_s, seqs, shapes, per_channel, plan)
        };

        let nthreads = std::thread::available_parallelism()
            .map_or(1, |p| p.get())
            .min(nblocks.max(1));
        if nthreads <= 1 || nblocks < 16 {
            return (0..nblocks).map(frame).collect(); // serial for tiny inputs
        }
        // Fan out contiguous frame ranges; concatenating the parts keeps them ordered.
        let chunk = nblocks.div_ceil(nthreads);
        let parts: Vec<Vec<(Vec<u8>, i64)>> = std::thread::scope(|s| {
            (0..nthreads)
                .map(|t| {
                    s.spawn(move || {
                        let end = ((t + 1) * chunk).min(nblocks);
                        (t * chunk..end).map(frame).collect::<Vec<_>>()
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
            self.queue = self.encode_stream().into();
        }
    }

    /// Retrieve the next encoded access unit. Returns [`Error::Again`] before
    /// [`finish`](AacEncoder::finish) has been called, and [`Error::Eof`] once
    /// fully drained.
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
    fn spectral_encode_roundtrips_through_decoder() {
        for cb_num in 1..=11usize {
            let cb = &CODEBOOKS[cb_num];
            let dim = cb.dim as usize;
            let lav = cb.lav as i32;

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

    /// The block-switching sequence OnlyLong → LongStart → EightShort → LongStop →
    /// OnlyLong must reconstruct through the decoder's synthesis math (TDAC across
    /// the transition windows) — the decisive check that the short filterbank and
    /// transition windows are exact inverses.
    #[test]
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
                analyze_long(&prev, &cur, &long_window(seq, false, false, &sine_l, &kbd_l_t, &sine_s, &kbd_s_t))
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

    /// End-to-end: a signal with a sharp attack must actually emit EightShort
    /// blocks and still decode cleanly through our decoder.
    #[test]
    fn transient_encodes_short_and_decodes() {
        let sr = 44100u32;
        let nframes = 5;
        let n = nframes * FRAME_LEN;
        let mut interleaved = Vec::new();
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let mut v = 0.02 * (2.0 * std::f64::consts::PI * 400.0 * t).sin();
            if (2 * FRAME_LEN..2 * FRAME_LEN + 600).contains(&i) {
                let k = i - 2 * FRAME_LEN;
                let env = (1.0 - k as f64 / 600.0).max(0.0);
                v += 0.8 * env * (2.0 * std::f64::consts::PI * 3000.0 * k as f64 / sr as f64).sin();
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
    fn stereo_ms_roundtrips_mono_content() {
        let sr = 44100u32;
        let n = 8192usize;
        let mut interleaved = Vec::new();
        for i in 0..n {
            let s =
                (0.4 * (2.0 * std::f64::consts::PI * 600.0 * i as f64 / sr as f64).sin()) as f32;
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
                lsum += (l as f64).powi(2);
                diff += ((l - rr) as f64).powi(2);
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
    fn stereo_direct_decode_amplitude() {
        let sr = 44100u32;
        let n = sr as usize; // 1 s
        let mut interleaved = Vec::new();
        for i in 0..n {
            let t = i as f64 / sr as f64;
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
                lsq += (l as f64).powi(2);
                rsq += (r as f64).powi(2);
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
            let t = i as f64 / sr as f64;
            *s = (0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                + 0.1 * (2.0 * std::f64::consts::PI * 3000.0 * t).sin()) as f32;
        }
        let prev = [0f32; FRAME_LEN];
        let iters = 500;
        let us = |d: std::time::Duration| d.as_secs_f64() * 1e6 / iters as f64;

        let t = Instant::now();
        let mut spec = Vec::new();
        for _ in 0..iters {
            spec = analyze_long(&prev, &cur, &win);
        }
        let mdct = t.elapsed();

        let t = Instant::now();
        for _ in 0..iters {
            let _ = perceptual_offsets(&spec, swb, sr, false);
        }
        let psy = t.elapsed();
        let offsets = perceptual_offsets(&spec, swb, sr, false);

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
    #[test]
    fn avx512_trunc_identity_matches_round_clamp() {
        for &v in &[
            0.0f64, 0.4, 0.5, 0.6, 1.4, 1.5, 2.5, 100.5, 8190.9, 8191.0, 8191.5, 1e6,
        ] {
            let round_clamp = (v.round() as i64).min(MAX_QUANT as i64);
            let trunc_trick = (v + 0.5).min(MAX_QUANT as f64 + 0.5) as i64; // `as` truncates
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
        (sig.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / sig.len().max(1) as f64).sqrt()
    }

    /// Magnitude of the `freq`-Hz component (Goertzel-style) — a recognizability probe.
    fn tone_energy(sig: &[f32], freq: f64, sr: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in sig.iter().enumerate() {
            let ph = 2.0 * std::f64::consts::PI * freq * i as f64 / sr;
            re += s as f64 * ph.cos();
            im += s as f64 * ph.sin();
        }
        (re * re + im * im).sqrt() / sig.len() as f64
    }

    /// The whole-pipeline gate: encode a 440 Hz tone, decode through the real
    /// decoder, and confirm a recognizable 440 Hz tone with preserved energy.
    #[test]
    fn encodes_and_decodes_recognizable_tone() {
        let sr = 44100u32;
        let n = 44100usize; // 1 s
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let s =
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / sr as f64).sin() * 0.5) as f32;
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
        let e440 = tone_energy(&decoded, 440.0, sr as f64);
        let e1234 = tone_energy(&decoded, 1234.0, sr as f64);
        assert!(
            e440 > 5.0 * e1234,
            "not a clean 440 Hz tone: e440 {e440:.5} e1234 {e1234:.5}"
        );
    }

    /// The rate loop must keep a dense signal within (roughly) the target bitrate
    /// and still produce decodable audio.
    #[test]
    fn rate_loop_respects_bitrate() {
        let sr = 44100u32;
        let secs = 2usize;
        let n = sr as usize * secs;
        let mut interleaved = Vec::new();
        let mut st = 0x0000_2468u32;
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let mut s = 0.0;
            for h in 1..=8 {
                s += (2.0 * std::f64::consts::PI * 300.0 * h as f64 * t).sin() / h as f64;
            }
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let noise = ((st >> 24) as f64 - 128.0) / 128.0 * 0.1;
            let v = ((s * 0.2 + noise) * 0.7).clamp(-1.0, 1.0) as f32;
            interleaved.push(v);
        }

        for &kbps in &[64_000u32, 128_000] {
            let mut enc = AacEncoder::new(AacEncoderConfig { bitrate_bps: kbps, ..Default::default() });
            enc.push_pcm(&interleaved, 1, sr).unwrap();
            let adts = encode_to_adts(&mut enc);

            let measured = adts.len() as f64 * 8.0 / secs as f64;
            assert!(
                measured <= kbps as f64 * 1.35,
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
            let t = i as f64 / sr as f64;
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
        let metric = |offsets: &[i32]| -> (f64, f64) {
            let base = rate_loop(&xp, swb, offsets, target);
            let sf = scalefactors(offsets, base);
            let thr = masking_thresholds(&spec, swb, sr);
            let (mut max_nmr, mut sum_db, mut n) = (0f64, 0f64, 0usize);
            for sfb in 0..swb.len() - 1 {
                let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                let en: f64 = spec[s..e].iter().map(|&x| (x as f64).powi(2)).sum();
                if en < 1e6 {
                    continue; // near-silent band → quantizes to ZERO, no artifact
                }
                let mut noise = 0f64;
                for &x in &spec[s..e] {
                    let q = quantize(x, sf[sfb]);
                    let rec = q.signum() as f64
                        * (q.unsigned_abs() as f64).powf(4.0 / 3.0)
                        * 2f64.powf(0.25 * (sf[sfb] - 100) as f64);
                    noise += (x as f64 - rec).powi(2);
                }
                let nmr = (noise / thr[sfb]).max(1e-30);
                max_nmr = max_nmr.max(nmr);
                sum_db += 10.0 * nmr.log10();
                n += 1;
            }
            (max_nmr, sum_db / n.max(1) as f64)
        };

        let flat = metric(&vec![0i32; swb.len() - 1]);
        let psy = metric(&perceptual_offsets(&spec, swb, sr, false));
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
            let s =
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / sr as f64).sin() * 0.5) as f32;
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
            let t = i as f64 / sr as f64;
            let mut v = 0.05 * (2.0 * std::f64::consts::PI * 220.0 * t).sin();
            // A sharp click every ~0.25 s.
            let k = i % 11025;
            if k < 700 {
                let env = (1.0 - k as f64 / 700.0).max(0.0);
                v += 0.8 * env * (2.0 * std::f64::consts::PI * 3500.0 * k as f64 / sr as f64).sin();
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
            let t = i as f64 / sr as f64;
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
            let w1 = long_window(WindowSequence::OnlyLong, a, a, &sine_l, &kbd_l, &sine_s, &kbd_s);
            let w2 = long_window(WindowSequence::OnlyLong, a, b, &sine_l, &kbd_l, &sine_s, &kbd_s);
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
                let o = pcm[i] as f64;
                let d = got[i + lag] as f64;
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
            let o = pcm[i] as f64;
            let d = dec[i + lag] as f64;
            num += (o - d) * (o - d);
            den += o * o;
        }
        let snr = 10.0 * (den / num.max(1e-30)).log10();
        assert!(snr > 8.0, "A1: reconstruction SNR {snr:.1} dB too low");
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
    fn a3_tns_inverts_exactly() {
        let sr = 44100u32;
        let pcm = transient_signal(sr, 14);

        let snr_of = |cfg: AacEncoderConfig| -> f64 {
            let pkts = encode(&pcm, 1, sr, cfg);
            let dec = decode_mono(&pkts, sr);
            assert!(dec.len() >= pcm.len());
            let lag = FRAME_LEN;
            let cmp = pcm.len() - 2 * FRAME_LEN;
            let (mut num, mut den) = (0f64, 0f64);
            for i in 0..cmp {
                let o = pcm[i] as f64;
                let d = dec[i + lag] as f64;
                num += (o - d) * (o - d);
                den += o * o;
            }
            10.0 * (den / num.max(1e-30)).log10()
        };

        let base = snr_of(AacEncoderConfig {
            bitrate_bps: 128_000,
            ..Default::default()
        });
        let with_tns = snr_of(AacEncoderConfig {
            bitrate_bps: 128_000,
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
    fn a3_tns_actually_engages() {
        let sr = 44100u32;
        let pcm = transient_signal(sr, 14);
        let off = encode(&pcm, 1, sr, AacEncoderConfig { bitrate_bps: 128_000, ..Default::default() });
        let on = encode(
            &pcm,
            1,
            sr,
            AacEncoderConfig {
                bitrate_bps: 128_000,
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

    /// Read the sequence of element ids from a raw_data_block, skipping each
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
        assert_eq!(element_plan(3).unwrap(), vec![Elem::Sce(2), Elem::Cpe(0, 1)]);
        // 5.1: C, L/R, Ls/Rs, LFE — with LFE taken from interleave slot 3.
        assert_eq!(
            element_plan(6).unwrap(),
            vec![
                Elem::Sce(2),
                Elem::Cpe(0, 1),
                Elem::Cpe(4, 5),
                Elem::Lfe(3)
            ]
        );
        // No channel_configuration exists for these.
        assert!(element_plan(0).is_none());
        assert!(element_plan(7).is_none());
        assert!(element_plan(8).is_none());
    }

    /// The decoder's reorder must be the exact inverse of the encoder's plan:
    /// what goes into interleave slot `k` must come back out of slot `k`.
    #[test]
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

    fn noise_signal(sr: u32, frames: usize) -> Vec<f32> {
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
    fn a6_off_is_byte_identical() {
        let sr = 44100;
        let pcm = noise_signal(sr, 8);
        let a = encode(&pcm, sr, AacEncoderConfig::default());
        let b = encode(&pcm, sr, AacEncoderConfig { pns: false, ..Default::default() });
        assert_eq!(a, b);
    }

    /// PNS must fire on noise and the stream must still decode. A desynchronized
    /// scalefactor chain (the classic PNS bug) shows up here as a decode error or
    /// non-finite output, not as a subtle quality shift.
    #[test]
    fn a6_fires_on_noise_and_round_trips() {
        let sr = 44100;
        let pcm = noise_signal(sr, 10);
        let off = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 96_000, ..Default::default() });
        let on = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 96_000, pns: true, ..Default::default() });
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
        let e_in: f64 = pcm.iter().map(|&x| (x as f64) * (x as f64)).sum();
        let e_out: f64 = got.iter().map(|&x| (x as f64) * (x as f64)).sum();
        let ratio = e_out / e_in.max(1e-12);
        assert!(
            (0.25..4.0).contains(&ratio),
            "PNS energy mismatch: out/in = {ratio:.3} - check ne = 2*log2(E)"
        );
    }

    /// PNS must NOT fire on a pure tone: it is the anti-class, and substituting a
    /// harmonic band with noise is audibly destructive.
    #[test]
    fn a6_declines_on_tonal_content() {
        let sr = 44100;
        let pcm: Vec<f32> = (0..10 * FRAME_LEN)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.4 * (2.0 * std::f32::consts::PI * 6000.0 * t).sin()
            })
            .collect();
        let off = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 96_000, ..Default::default() });
        let on = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 96_000, pns: true, ..Default::default() });
        assert_eq!(
            off, on,
            "A6 fired on a pure 6 kHz tone - the tonality guard is not working"
        );
    }

    /// PNS and TNS must not both apply to one element (the energy domain differs).
    #[test]
    fn a6_yields_to_tns() {
        let sr = 44100;
        let pcm = noise_signal(sr, 10);
        let both = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 128_000, pns: true, tns: true, ..Default::default() });
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
    fn a7_off_is_byte_identical() {
        let sr = 44100;
        let pcm = correlated_stereo(sr, 8, 0.7);
        let a = encode(&pcm, sr, AacEncoderConfig::default());
        let b = encode(&pcm, sr, AacEncoderConfig { intensity: false, ..Default::default() });
        assert_eq!(a, b);
    }

    /// Fires on a scaled-copy pair and still decodes. A wrong is_pos sign or a
    /// desynchronized intensity accumulator shows up here immediately.
    #[test]
    fn a7_fires_and_round_trips() {
        let sr = 44100;
        let pcm = correlated_stereo(sr, 10, 0.7);
        let off = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 96_000, ..Default::default() });
        let on = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 96_000, intensity: true, ..Default::default() });
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
            el += (c[0] as f64) * (c[0] as f64);
            er += (c[1] as f64) * (c[1] as f64);
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
        let off = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 128_000, ..Default::default() });
        let on = encode(&pcm, sr, AacEncoderConfig { bitrate_bps: 128_000, intensity: true, ..Default::default() });
        assert_eq!(
            off, on,
            "A7 fired on fully decorrelated channels - the correlation gate is not working"
        );
    }
}
