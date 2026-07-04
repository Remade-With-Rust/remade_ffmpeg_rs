//! In-house **AAC-LC encoder** (see `docs/codec-aac-encoder.md`). Built brick by
//! brick; **brick 1** is the scaffolding — the encode-side primitives that invert
//! the decoder: a bit writer and the spectral-codebook encoder (a quantized tuple
//! → Huffman codeword + sign/escape bits, the inverse of `codebook::apply_index`).

#![allow(dead_code)]

use crate::codebook::{Codebook, CODEBOOKS};
use crate::ics::{IcsInfo, WindowSequence};
use crate::swb::swb_offsets;
use crate::tables::spectral_book;
use crate::{AdtsHeader, AudioSpecificConfig};
use rff_codec::Encoder;
use rff_core::{AudioFrame, Dictionary, Error, Frame, Packet, Result, SampleFormat};
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

/// The 2048-sample sine window for a long or transition block — the encode-side
/// twin of the decoder's `long_window` (we always use sine shapes). The left half
/// follows the previous block, the right half the next; LongStart/LongStop taper
/// one side to a short window so the sequence stays TDAC-exact.
fn long_window(seq: WindowSequence, sine_l: &[f32], sine_s: &[f32]) -> Vec<f32> {
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
fn spreading_matrix(swb: &[u16], sample_rate: u32) -> Arc<Vec<f64>> {
    type Cache = Mutex<HashMap<(u32, usize), Arc<Vec<f64>>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (sample_rate, swb.len());
    let mut c = cache.lock().unwrap();
    if let Some(m) = c.get(&key) {
        return m.clone();
    }
    let num_swb = swb.len() - 1;
    let bark: Vec<f64> = (0..num_swb)
        .map(|sfb| {
            let center = (swb[sfb] as f64 + swb[sfb + 1] as f64) / 2.0;
            hz_to_bark(center * sample_rate as f64 / LONG_N as f64)
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
fn masking_thresholds(spec: &[f32], swb: &[u16], sample_rate: u32) -> Vec<f64> {
    let num_swb = swb.len() - 1;
    let mut energy = vec![0.0f64; num_swb];
    for sfb in 0..num_swb {
        let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
        energy[sfb] = spec[s..e].iter().map(|&x| (x as f64).powi(2)).sum::<f64>() + 1e-3;
    }
    let mat = spreading_matrix(swb, sample_rate);
    // Signal-to-mask ratio: the mask sits ~18 dB below the spread signal.
    const SMR: f64 = 0.0158; // 10^(-18/10)
    (0..num_swb)
        .map(|i| {
            let row = &mat[i * num_swb..(i + 1) * num_swb];
            let spread: f64 = (0..num_swb).map(|j| energy[j] * row[j]).sum();
            spread * SMR
        })
        .collect()
}

/// Per-SFB scalefactor offsets (relative to a common base the rate loop sets):
/// higher where masking is generous, lower where noise would be audible. From
/// `noise ∝ 2^(0.375·sf)·Σ√|X|`, the sf hitting `noise = threshold` is
/// `log2(threshold/Σ√|X|)/0.375`; we normalize + clamp so deltas stay codeable.
fn perceptual_offsets(spec: &[f32], swb: &[u16], sample_rate: u32) -> Vec<i32> {
    let num_swb = swb.len() - 1;
    let thr = masking_thresholds(spec, swb, sample_rate);
    let mut raw = vec![0.0f64; num_swb];
    let mut energy = vec![0.0f64; num_swb];
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

const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_END: u32 = 7;
const ZERO_HCB: u8 = 0;
const ESC_HCB: u8 = 11;
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
    cbs.iter()
        .position(|&cb| cb != ZERO_HCB)
        .map(|sfb| sf[sfb])
        .unwrap_or(100)
}

/// Bits to code the per-band scalefactors as `SCALEFACTOR_BOOK` deltas from a
/// running accumulator seeded at `gg`.
fn scalefactor_bits(cbs: &[u8], sf: &[i32], gg: i32) -> usize {
    let mut acc = gg;
    let mut bits = 0usize;
    for (sfb, &cb) in cbs.iter().enumerate() {
        if cb == ZERO_HCB {
            continue;
        }
        let delta = (sf[sfb] - acc).clamp(-60, 60);
        bits += crate::tables::SCALEFACTOR_BOOK
            .code((delta + 60) as usize)
            .1 as usize;
        acc += delta;
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

/// Shared coder body: quantize the frame into the caller-owned `quant` (per-band
/// scalefactors `sf`, which cover the whole spectrum so no pre-zeroing is needed),
/// pick per-band codebooks, and return (codebooks, ICS body bits, max_sfb). Splitting
/// this out lets the rate loop reuse one buffer instead of allocating per candidate.
fn code_core(xp: &Xpow, swb: &[u16], sf: &[i32], quant: &mut [i32]) -> (Vec<u8>, usize, usize) {
    let num_swb = swb.len() - 1;
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
    let mut acc = gg;
    for (sfb, &cb) in cbs.iter().enumerate() {
        if cb == ZERO_HCB {
            continue;
        }
        let delta = (sf[sfb] - acc).clamp(-60, 60);
        let (code, len) = crate::tables::SCALEFACTOR_BOOK.code((delta + 60) as usize);
        w.write(code, len as u32);
        acc += delta;
    }
}

/// spectral_data: per regular SFB, code coefficient tuples with the band's book.
fn write_spectrum(w: &mut BitWriter, quant: &[i32], cbs: &[u8], swb: &[u16]) {
    for (sfb, &cb) in cbs.iter().enumerate() {
        if cb == ZERO_HCB {
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
fn encode_channel_element(
    w: &mut BitWriter,
    tag: u32,
    spec: &[f32],
    swb: &[u16],
    seq: WindowSequence,
    sample_rate: u32,
    target_bits: usize,
) {
    let offsets = perceptual_offsets(spec, swb, sample_rate);
    let xp = Xpow::new(spec);
    let base = rate_loop(&xp, swb, &offsets, target_bits);
    let sf = scalefactors(&offsets, base);
    let (cbs, _, max_sfb, quant) = code_frame(&xp, swb, &sf);
    let gg = global_gain(&cbs, &sf);

    w.write(ID_SCE, 3);
    w.write(tag, 4);
    w.write(gg as u32, 8);
    let info = IcsInfo {
        window_sequence: seq,
        window_shape_kbd: false,
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
    w.write(0, 1); // tns_data_present
    w.write(0, 1); // gain_control_data_present
    write_spectrum(w, &quant, &cbs, swb);
}

// ---------------------------------------------------------------------------
// Block switching (brick 5) — transient detection, window-sequence assignment,
// and the short-block coding path (eight 128-bin windows, one group).
// ---------------------------------------------------------------------------

/// Flag each 1024-sample frame that contains a transient: a 128-sample sub-block
/// whose energy leaps above the recent running average (an attack). Frame 0 is
/// never flagged — nothing precedes it to open a LongStart transition from.
fn detect_transients(chan: &[f32], nframes: usize) -> Vec<bool> {
    const RATIO: f64 = 10.0;
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
    let num_swb = swb.len() - 1;
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

/// Rate loop for a short block: smallest flat scalefactor (≥ no-clamp floor) whose
/// body fits `target_bits`.
fn rate_loop_short(xp: &Xpow, swb: &[u16], target_bits: usize) -> i32 {
    let mut lo = min_base_short(xp);
    let mut hi = 255i32;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let sf = vec![mid; swb.len() - 1];
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
/// windows, flat scalefactors — the time-resolution win of block switching).
fn encode_channel_element_short(
    w: &mut BitWriter,
    tag: u32,
    spec: &[f32],
    swb: &[u16],
    target_bits: usize,
) {
    let xp = Xpow::new(spec);
    let base = rate_loop_short(&xp, swb, target_bits);
    let sf = vec![base; swb.len() - 1];
    let (cbs, _, max_sfb, quant) = code_frame_short(&xp, swb, &sf);
    let gg = global_gain(&cbs, &sf);

    w.write(ID_SCE, 3);
    w.write(tag, 4);
    w.write(gg as u32, 8);
    let info = IcsInfo {
        window_sequence: WindowSequence::EightShort,
        window_shape_kbd: false,
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
fn mid_side(l: &[f32], r: &[f32], swb: &[u16], is_short: bool) -> (Vec<f32>, Vec<f32>, Vec<bool>) {
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
        if em * es < el * er {
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
) -> (Vec<i32>, Vec<i32>) {
    let xp = Xpow::new(spec);
    if is_short {
        let base = rate_loop_short(&xp, swb, target_bits);
        let sf = vec![base; swb.len() - 1];
        let (_, _, _, quant) = code_frame_short(&xp, swb, &sf);
        (sf, quant)
    } else {
        let offsets = perceptual_offsets(spec, swb, sample_rate);
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
) {
    let is_short = seq == WindowSequence::EightShort;
    let (ch0, ch1, ms_full) = mid_side(spec_l, spec_r, swb, is_short);
    let (sf0, quant0) = quantize_channel(&ch0, swb, is_short, sample_rate, target_bits);
    let (sf1, quant1) = quantize_channel(&ch1, swb, is_short, sample_rate, target_bits);
    let max_sfb = joint_max_sfb(&quant0, &quant1, swb, is_short);
    let cbs0 = codebooks(&quant0, swb, is_short, max_sfb);
    let cbs1 = codebooks(&quant1, swb, is_short, max_sfb);

    w.write(ID_CPE, 3);
    w.write(tag, 4);
    w.write(1, 1); // common_window
    let info = IcsInfo {
        window_sequence: seq,
        window_shape_kbd: false,
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
// The encoder: buffers input, blocks into 1024-sample long frames, emits ADTS.
// ---------------------------------------------------------------------------
pub struct AacEncoder {
    sample_rate: u32,
    channels: usize,
    fs_index: u8,
    /// Target bitrate (bits/s), set by `-b`. Drives the per-frame rate loop.
    bitrate: u32,
    win: Vec<f32>,
    chans: Vec<Vec<f32>>,
    initialized: bool,
    /// Encoded raw access units (raw_data_block, no ADTS) awaiting `receive_packet`,
    /// each with its sample-domain PTS. Filled on `flush`.
    queue: VecDeque<(Vec<u8>, i64)>,
    flushed: bool,
}

impl AacEncoder {
    pub fn new() -> Self {
        AacEncoder {
            sample_rate: 0,
            channels: 0,
            fs_index: 0,
            bitrate: 128_000,
            win: Vec::new(),
            chans: Vec::new(),
            initialized: false,
            queue: VecDeque::new(),
            flushed: false,
        }
    }

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
                        self.chans[c].push(f32::from_le_bytes([
                            d[o],
                            d[o + 1],
                            d[o + 2],
                            d[o + 3],
                        ]));
                    }
                }
            }
            SampleFormat::F32Planar => {
                for (c, plane) in f.planes.iter().enumerate().take(ch) {
                    for i in 0..n {
                        let o = i * 4;
                        self.chans[c].push(f32::from_le_bytes([
                            plane[o],
                            plane[o + 1],
                            plane[o + 2],
                            plane[o + 3],
                        ]));
                    }
                }
            }
            _ => return Err(Error::invalid("aac encode: unsupported sample format")),
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
    fn encode_frame(
        &self,
        b: usize,
        swb: &[u16],
        swb_s: &[u16],
        sine_s: &[f32],
        seqs: &[Vec<WindowSequence>],
        per_channel: usize,
        stereo: bool,
    ) -> (Vec<u8>, i64) {
        let prev = |ch: usize| -> [f32; FRAME_LEN] {
            if b == 0 {
                [0f32; FRAME_LEN]
            } else {
                self.block(ch, b - 1)
            }
        };
        let mut rdb = BitWriter::new();
        if stereo {
            let seq = seqs[0][b];
            let (p0, c0, p1, c1) = (prev(0), self.block(0, b), prev(1), self.block(1, b));
            if seq == WindowSequence::EightShort {
                let l = analyze_short(&p0, &c0, sine_s);
                let r = analyze_short(&p1, &c1, sine_s);
                encode_cpe(
                    &mut rdb,
                    0,
                    &l,
                    &r,
                    swb_s,
                    seq,
                    self.sample_rate,
                    per_channel,
                );
            } else {
                let win = long_window(seq, &self.win, sine_s);
                let l = analyze_long(&p0, &c0, &win);
                let r = analyze_long(&p1, &c1, &win);
                encode_cpe(&mut rdb, 0, &l, &r, swb, seq, self.sample_rate, per_channel);
            }
        } else {
            for ch in 0..self.channels {
                let (p, c) = (prev(ch), self.block(ch, b));
                let seq = seqs[ch][b];
                if seq == WindowSequence::EightShort {
                    let spec = analyze_short(&p, &c, sine_s);
                    encode_channel_element_short(&mut rdb, ch as u32, &spec, swb_s, per_channel);
                } else {
                    let win = long_window(seq, &self.win, sine_s);
                    let spec = analyze_long(&p, &c, &win);
                    let sr = self.sample_rate;
                    encode_channel_element(&mut rdb, ch as u32, &spec, swb, seq, sr, per_channel);
                }
            }
        }
        rdb.write(ID_END, 3);
        (rdb.into_bytes(), (b * FRAME_LEN) as i64)
    }

    /// Encode all buffered frames to raw access units. Frames are independent, so
    /// they fan out across worker threads (ffmpeg's AAC encoder is single-threaded).
    fn encode_stream(&self) -> Vec<(Vec<u8>, i64)> {
        let swb = swb_offsets(true, self.fs_index);
        let swb_s = swb_offsets(false, self.fs_index);
        let sine_s = crate::dsp::sine_window(SHORT_N);
        let n = self.chans.first().map_or(0, |c| c.len());
        let nblocks = n.div_ceil(FRAME_LEN) + 1;
        // Per-channel ICS-body budget from the target bitrate (minus framing).
        let frame_budget = (self.bitrate as usize * FRAME_LEN / self.sample_rate.max(1) as usize)
            .saturating_sub(59); // ADTS header (56) + END (3)
        let per_channel = (frame_budget / self.channels.max(1)).saturating_sub(7); // element overhead
        let stereo = self.channels == 2;
        // Window sequences. A stereo CPE shares one common window, so the two
        // channels' transient flags are joined; SCE channels stay independent.
        let seqs: Vec<Vec<WindowSequence>> = if stereo {
            let t0 = detect_transients(&self.chans[0], nblocks);
            let t1 = detect_transients(&self.chans[1], nblocks);
            let joint: Vec<bool> = (0..nblocks).map(|b| t0[b] || t1[b]).collect();
            let s = assign_sequences(&joint);
            vec![s.clone(), s]
        } else {
            (0..self.channels)
                .map(|ch| assign_sequences(&detect_transients(&self.chans[ch], nblocks)))
                .collect()
        };
        // Slice views the worker threads share (all read-only).
        let (swb, swb_s, sine_s, seqs) = (&swb[..], &swb_s[..], &sine_s[..], &seqs[..]);
        let frame = |b: usize| self.encode_frame(b, swb, swb_s, sine_s, seqs, per_channel, stereo);

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
}

impl Default for AacEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder for AacEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(b) = options.get_int("b") {
            if b > 0 {
                self.bitrate = b as u32;
            }
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("aac encode: expected an audio frame"));
        };
        if !self.initialized {
            self.sample_rate = a.sample_rate;
            self.channels = a.channels.max(1) as usize;
            self.fs_index = crate::sf_index_for_rate(a.sample_rate)
                .ok_or_else(|| Error::invalid("aac encode: unsupported sample rate"))?;
            self.win = crate::dsp::sine_window(LONG_N);
            self.chans = vec![Vec::new(); self.channels];
            self.initialized = true;
        } else if a.channels.max(1) as usize != self.channels {
            return Err(Error::invalid(
                "aac encode: channel count changed mid-stream",
            ));
        }
        self.ingest(a)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some((data, pts)) = self.queue.pop_front() {
            let mut p = Packet::from_data(0, data);
            p.pts = Some(pts);
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
            self.queue = self.encode_stream().into();
        }
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
        enc.flush();
        let mut out = Vec::new();
        while let Ok(p) = enc.receive_packet() {
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
                analyze_long(&prev, &cur, &long_window(seq, &sine_l, &sine_s))
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
                let win = long_window(*seq, &sine_l, &sine_s);
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
            interleaved.extend_from_slice(&(v as f32).to_le_bytes());
        }

        let mut enc = AacEncoder::new();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        }))
        .unwrap();
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
            if let Frame::Audio(a) = dec.decode(au, None).unwrap() {
                decoded += a.samples;
            }
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
            interleaved.extend_from_slice(&s.to_le_bytes()); // L
            interleaved.extend_from_slice(&s.to_le_bytes()); // R == L
        }
        let mut enc = AacEncoder::new();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        }))
        .unwrap();
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
            if let Frame::Audio(a) = dec.decode(au, None).unwrap() {
                let d = &a.planes[0];
                for k in 0..a.samples {
                    let l =
                        f32::from_le_bytes([d[k * 8], d[k * 8 + 1], d[k * 8 + 2], d[k * 8 + 3]]);
                    let rr = f32::from_le_bytes([
                        d[k * 8 + 4],
                        d[k * 8 + 5],
                        d[k * 8 + 6],
                        d[k * 8 + 7],
                    ]);
                    lsum += (l as f64).powi(2);
                    diff += ((l - rr) as f64).powi(2);
                }
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
            interleaved.extend_from_slice(&l.to_le_bytes());
            interleaved.extend_from_slice(&r.to_le_bytes());
        }
        let mut enc = AacEncoder::new();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        }))
        .unwrap();
        enc.flush();
        let mut dec = crate::decode::Decoder::new(sr);
        let (mut lsq, mut rsq, mut cnt) = (0.0f64, 0.0f64, 0usize);
        while let Ok(p) = enc.receive_packet() {
            if let Frame::Audio(a) = dec.decode(&p.data, None).unwrap() {
                let d = &a.planes[0];
                for k in 0..a.samples {
                    let l =
                        f32::from_le_bytes([d[k * 8], d[k * 8 + 1], d[k * 8 + 2], d[k * 8 + 3]]);
                    let r = f32::from_le_bytes([
                        d[k * 8 + 4],
                        d[k * 8 + 5],
                        d[k * 8 + 6],
                        d[k * 8 + 7],
                    ]);
                    lsq += (l as f64).powi(2);
                    rsq += (r as f64).powi(2);
                    cnt += 1;
                }
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
            let _ = perceptual_offsets(&spec, swb, sr);
        }
        let psy = t.elapsed();
        let offsets = perceptual_offsets(&spec, swb, sr);

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
        let mut interleaved = Vec::with_capacity(n * 4);
        for i in 0..n {
            let s =
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / sr as f64).sin() * 0.5) as f32;
            samples.push(s);
            interleaved.extend_from_slice(&s.to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });

        let mut enc = AacEncoder::new();
        enc.send_frame(&frame).unwrap();
        let adts = encode_to_adts(&mut enc);
        assert!(crate::is_adts(&adts), "encoder output is not ADTS");

        let mut dec = crate::decode::Decoder::new(sr);
        let mut decoded = Vec::new();
        let mut pos = 0usize;
        while pos + 7 <= adts.len() {
            let hdr = crate::parse_adts(&adts[pos..]).unwrap();
            let au = &adts[pos + hdr.header_len..pos + hdr.frame_length];
            if let Frame::Audio(a) = dec.decode(au, None).unwrap() {
                for c in a.planes[0].chunks_exact(4) {
                    decoded.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                }
            }
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
            interleaved.extend_from_slice(&v.to_le_bytes());
        }

        for &kbps in &[64_000i64, 128_000] {
            let mut d = Dictionary::new();
            d.set("b", kbps.to_string());
            let mut enc = AacEncoder::new();
            enc.configure(&d).unwrap();
            let frame = Frame::Audio(AudioFrame {
                sample_rate: sr,
                channels: 1,
                format: SampleFormat::F32,
                planes: vec![interleaved.clone()],
                samples: n,
                pts: Some(0),
            });
            enc.send_frame(&frame).unwrap();
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
                if let Frame::Audio(a) = dec.decode(au, None).unwrap() {
                    got |= !a.planes[0].is_empty();
                }
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
        let psy = metric(&perceptual_offsets(&spec, swb, sr));
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
            interleaved.extend_from_slice(&s.to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        });
        let mut enc = AacEncoder::new();
        enc.send_frame(&frame).unwrap();
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
            interleaved.extend_from_slice(&(v as f32).to_le_bytes());
        }
        let mut enc = AacEncoder::new();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        }))
        .unwrap();
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
            interleaved.extend_from_slice(&(l as f32).to_le_bytes());
            interleaved.extend_from_slice(&(r as f32).to_le_bytes());
        }
        let mut enc = AacEncoder::new();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples: n,
            pts: Some(0),
        }))
        .unwrap();
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
