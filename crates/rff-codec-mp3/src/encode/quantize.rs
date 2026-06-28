//! The two-loop quantizer — rate control + noise shaping.
//!
//! * **Inner loop (rate):** raise the quantization step until the Huffman-coded
//!   spectrum fits the granule's bit budget.
//! * **Outer loop (distortion):** raise per-band scalefactors where quantization
//!   noise exceeds the psychoacoustic threshold, re-running the inner loop, until
//!   noise is masked everywhere or no scalefactor budget remains.
//!
//! Produces the quantized integer spectrum plus the side-info fields (global
//! gain, scalefactors, scalefac_compress, block flags) that describe it.

use std::sync::OnceLock;

use crate::frame::{BlockType, GranuleSideInfo, GRANULE_LINES};
use crate::header::FrameHeader;

use super::psychoacoustic::PsyResult;

// ── Brick N4: the nonuniform quantizer power law ──────────────────────────────
//
// The decoder requantizes `xr = sign(is)·|is|^(4/3)·scale` (see
// `decode/requantize.rs`). The encoder inverts the `|is|^(4/3)` core: a magnitude
// `xr` quantizes to the integer level `ix = nint(|xr|^(3/4) − BIAS)`. The two are
// exact inverses on the integer lattice — `quantize_level(requant_magnitude(ix))
// == ix` for every representable `ix` — which is N4's verification gate. The
// global-gain / scalefactor `scale` is applied by the rate loop (C2); N4 is the
// unit-step power law it builds on.

/// Largest Huffman magnitude reachable with `linbits` (matches the decoder).
pub const MAX_LEVEL: i32 = 8206;

/// ISO rounding bias subtracted before the round in the forward quantizer
/// (ISO/IEC 11172-3 2.4.2.7). It biases the decision boundary so the truncating
/// `nint` recovers the intended level.
pub const QUANT_BIAS: f64 = 0.0946;

/// `|level|^(4/3)`, table-backed for the `0..=MAX_LEVEL` magnitudes — the
/// requantization power law (the same curve the decoder applies, factored out so
/// the encoder's rate loop can predict what the decoder will reconstruct).
pub fn requant_magnitude(level: i32) -> f64 {
    static T: OnceLock<Vec<f64>> = OnceLock::new();
    let t = T.get_or_init(|| {
        (0..=MAX_LEVEL as usize)
            .map(|i| (i as f64).powf(4.0 / 3.0))
            .collect()
    });
    let a = level.unsigned_abs() as usize;
    t.get(a)
        .copied()
        .unwrap_or_else(|| (a as f64).powf(4.0 / 3.0))
}

/// Forward-quantize a (positive) frequency-line magnitude to its integer level
/// under unit step: `ix = nint(|xr|^(3/4) − BIAS)`, clamped to `[0, MAX_LEVEL]`.
/// The sign is carried separately, exactly as the bitstream does.
pub fn quantize_level(xr: f64) -> i32 {
    let m = xr.abs().powf(0.75) - QUANT_BIAS;
    if m <= 0.0 {
        0
    } else {
        (m.round() as i32).clamp(0, MAX_LEVEL)
    }
}

/// One granule's quantized output.
#[derive(Debug, Clone)]
pub struct QuantizedGranule {
    /// Quantized integer spectrum (`is`), 576 lines.
    pub coeffs: [i32; GRANULE_LINES],
    /// Side-info describing how to dequantize it (gain, tables, regions, flags).
    pub side: GranuleSideInfo,
    /// Scalefactors per band (long: 22; short: 3×13 packed).
    pub scalefactors: [u8; 39],
}

impl Default for QuantizedGranule {
    fn default() -> Self {
        // Arrays larger than 32 don't derive Default.
        QuantizedGranule {
            coeffs: [0; GRANULE_LINES],
            side: GranuleSideInfo::default(),
            scalefactors: [0; 39],
        }
    }
}

/// Largest non-clipping quantized level. Above this the value would saturate at
/// `MAX_LEVEL`, losing precision — so a gain that produces it is *too fine*.
const MAX_UNCLIPPED: i32 = 8191;
/// Scalefactor multiplier when `scalefac_scale = 0` (the half-step we use).
const SF_MULT: f64 = 0.5;
/// Largest scalefactor value (a 4-bit `slen` field caps it).
const MAX_SF: u8 = 15;
/// Outer distortion-loop iteration cap.
const MAX_OUTER: usize = 24;

/// Quantize one granule at `global_gain` with per-band scalefactors applied:
/// band `b` is amplified by `2^(SF_MULT·sf[b])` before quantizing (finer step →
/// less noise there), the forward of the decoder's per-band requantization.
fn quantize_with_sf(
    header: &FrameHeader,
    freq: &[f32; GRANULE_LINES],
    gain: i32,
    sf: &[u8; 22],
) -> [i32; GRANULE_LINES] {
    let off = crate::tables::sfb_long_offsets(header.sample_rate);
    let base = -0.25 * (gain - 210) as f64;
    let mut coeffs = [0i32; GRANULE_LINES];
    for b in 0..22 {
        let s = if b < 21 { sf[b] } else { 0 } as f64; // band 21 is uncoded
        let scale_inv = 2f64.powf(base + SF_MULT * s);
        let (lo, hi) = (off[b] as usize, (off[b + 1] as usize).min(GRANULE_LINES));
        for i in lo..hi {
            let mag = quantize_level(freq[i].abs() as f64 * scale_inv);
            coeffs[i] = if freq[i] < 0.0 { -mag } else { mag };
        }
    }
    coeffs
}

/// Per-band quantization-noise energy: `Σ (freq − requantized)²` over each of the
/// 21 coded long bands, using the decoder's exact requantization.
fn band_noise(
    header: &FrameHeader,
    freq: &[f32; GRANULE_LINES],
    coeffs: &[i32; GRANULE_LINES],
    gain: i32,
    sf: &[u8; 22],
) -> [f32; 21] {
    let off = crate::tables::sfb_long_offsets(header.sample_rate);
    let mut noise = [0f32; 21];
    for (b, n) in noise.iter_mut().enumerate() {
        let scale = 2f64.powf(0.25 * (gain - 210) as f64 - SF_MULT * sf[b] as f64);
        let (lo, hi) = (off[b] as usize, (off[b + 1] as usize).min(GRANULE_LINES));
        let mut e = 0f64;
        for i in lo..hi {
            let xr = coeffs[i].signum() as f64 * requant_magnitude(coeffs[i]) * scale;
            let d = freq[i] as f64 - xr;
            e += d * d;
        }
        *n = e as f32;
    }
    noise
}

/// Bits to represent values `0..=v`.
fn bits_for(v: u8) -> u8 {
    if v == 0 {
        0
    } else {
        (8 - v.leading_zeros()) as u8
    }
}

/// Pick the smallest `scalefac_compress` covering the current scalefactors, and
/// its scalefactor-bit cost (`11·slen1 + 10·slen2`).
fn choose_compress(sf: &[u8; 22]) -> (u16, usize) {
    let max1 = sf[0..11].iter().copied().max().unwrap_or(0);
    let max2 = sf[11..21].iter().copied().max().unwrap_or(0);
    let (need1, need2) = (bits_for(max1), bits_for(max2));
    for (idx, &(slen1, slen2)) in crate::tables::SCALEFAC_COMPRESS_V1.iter().enumerate() {
        if slen1 >= need1 && slen2 >= need2 {
            return (idx as u16, 11 * slen1 as usize + 10 * slen2 as usize);
        }
    }
    (15, 11 * 4 + 10 * 3)
}

/// Huffman bit cost of a coefficient set under the best table selection for
/// `block_type` (long vs window-switched regions — the emit must use the same).
fn huff_cost(
    header: &FrameHeader,
    coeffs: &[i32; GRANULE_LINES],
    block_type: BlockType,
) -> (GranuleSideInfo, usize) {
    let side = super::huffman::select(header, coeffs, block_type);
    let q = QuantizedGranule {
        coeffs: *coeffs,
        side: side.clone(),
        scalefactors: [0; 39],
    };
    let mut w = crate::bitio::BitWriter::new();
    let bits = super::huffman::encode(&q, header, &mut w);
    (side, bits)
}

/// Inner rate loop: smallest `global_gain` (finest, best quality) that neither
/// clips nor exceeds `huff_budget`, for the given scalefactors.
fn inner_gain(
    header: &FrameHeader,
    freq: &[f32; GRANULE_LINES],
    sf: &[u8; 22],
    huff_budget: usize,
    block_type: BlockType,
) -> i32 {
    let ok = |g: i32| {
        let coeffs = quantize_with_sf(header, freq, g, sf);
        coeffs.iter().all(|&c| c.abs() <= MAX_UNCLIPPED)
            && huff_cost(header, &coeffs, block_type).1 <= huff_budget
    };
    let (mut lo, mut hi) = (0i32, 255i32);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if ok(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// **C2 + Q6 — the two-loop quantizer.** The inner loop ([`inner_gain`]) hits the
/// bit budget; the outer distortion loop raises the scalefactor of the worst
/// over-threshold band, re-runs the inner loop, and keeps the lowest-peak-NMR
/// result. With a flat threshold (C1) it degrades to pure rate control; with the
/// real psymodel (Q1–Q4) it shapes quantization noise under the masking curve.
pub fn loops(
    header: &FrameHeader,
    freq: &[f32; GRANULE_LINES],
    psy: &PsyResult,
    bit_budget: usize,
    block_type: BlockType,
) -> QuantizedGranule {
    let mut sf = [0u8; 22];
    let mut best: Option<(f32, QuantizedGranule)> = None;

    for _ in 0..MAX_OUTER {
        let (compress, sf_bits) = choose_compress(&sf);
        let huff_budget = bit_budget.saturating_sub(sf_bits);

        let gain = inner_gain(header, freq, &sf, huff_budget, block_type);
        let coeffs = quantize_with_sf(header, freq, gain, &sf);
        let (mut side, _) = huff_cost(header, &coeffs, block_type);
        side.global_gain = gain as u8;
        side.scalefac_compress = compress;
        let mut scalefactors = [0u8; 39];
        scalefactors[..22].copy_from_slice(&sf);
        let granule = QuantizedGranule {
            coeffs,
            side,
            scalefactors,
        };

        // Score: peak noise-to-mask ratio across the coded bands.
        let noise = band_noise(header, freq, &granule.coeffs, gain, &sf);
        let mut peak_nmr = f32::NEG_INFINITY;
        let mut worst: Option<usize> = None;
        let mut worst_nmr = f32::NEG_INFINITY;
        for (b, &n) in noise.iter().enumerate() {
            let nmr = n / psy.thresholds[b].max(1e-20);
            peak_nmr = peak_nmr.max(nmr);
            if n > psy.thresholds[b] && sf[b] < MAX_SF && nmr > worst_nmr {
                worst_nmr = nmr;
                worst = Some(b);
            }
        }
        if best.as_ref().is_none_or(|(bn, _)| peak_nmr < *bn) {
            best = Some((peak_nmr, granule));
        }
        match worst {
            Some(b) => sf[b] += 1, // amplify the worst band, then re-quantize
            None => break,         // every band already masked, or all saturated
        }
    }

    best.expect("at least one iteration runs").1
}

/// Smallest gain whose flat-scalefactor quantization doesn't clip — the finest
/// representable step for this spectrum.
fn nonclip_floor(header: &FrameHeader, freq: &[f32; GRANULE_LINES]) -> i32 {
    let flat = [0u8; 22];
    let ok = |g: i32| {
        quantize_with_sf(header, freq, g, &flat)
            .iter()
            .all(|&c| c.abs() <= MAX_UNCLIPPED)
    };
    let (mut lo, mut hi) = (0i32, 255i32);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if ok(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// **R2 (VBR)** — quantize to a *quality* target instead of a bit budget. Picks the
/// coarsest gain whose peak noise-to-mask ratio stays under `target_nmr` (fewest
/// bits that still meet quality), never finer than the no-clip floor, then shapes
/// scalefactors under the threshold. The resulting bit count — and hence the
/// frame's bitrate — varies with content.
pub fn loops_vbr(
    header: &FrameHeader,
    freq: &[f32; GRANULE_LINES],
    psy: &PsyResult,
    target_nmr: f32,
    block_type: BlockType,
) -> QuantizedGranule {
    let flat = [0u8; 22];
    let peak = |g: i32| {
        let coeffs = quantize_with_sf(header, freq, g, &flat);
        let noise = band_noise(header, freq, &coeffs, g, &flat);
        noise
            .iter()
            .enumerate()
            .map(|(b, &n)| n / psy.thresholds[b].max(1e-20))
            .fold(0f32, f32::max)
    };
    // Largest gain (coarsest → fewest bits) whose peak NMR meets the target.
    let (mut lo, mut hi) = (0i32, 255i32);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if peak(mid) <= target_nmr {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let gain = lo.max(nonclip_floor(header, freq));

    // Distortion loop at the fixed gain: raise the worst over-threshold band.
    let mut sf = [0u8; 22];
    for _ in 0..MAX_OUTER {
        let coeffs = quantize_with_sf(header, freq, gain, &sf);
        let noise = band_noise(header, freq, &coeffs, gain, &sf);
        let mut worst = None;
        let mut worst_nmr = f32::NEG_INFINITY;
        for (b, &n) in noise.iter().enumerate() {
            let nmr = n / psy.thresholds[b].max(1e-20);
            if n > psy.thresholds[b] && sf[b] < MAX_SF && nmr > worst_nmr {
                worst_nmr = nmr;
                worst = Some(b);
            }
        }
        match worst {
            Some(b) => sf[b] += 1,
            None => break,
        }
    }

    let coeffs = quantize_with_sf(header, freq, gain, &sf);
    let (compress, _) = choose_compress(&sf);
    let mut side = super::huffman::select(header, &coeffs, block_type);
    side.global_gain = gain as u8;
    side.scalefac_compress = compress;
    let mut scalefactors = [0u8; 39];
    scalefactors[..22].copy_from_slice(&sf);
    QuantizedGranule {
        coeffs,
        side,
        scalefactors,
    }
}

#[cfg(test)]
mod c2_tests {
    use super::*;
    use crate::decode::scalefactors::ScaleFactors;
    use crate::frame::ChannelMode;
    use crate::header::MpegVersion;

    fn hdr() -> FrameHeader {
        FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: ChannelMode::Mono,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn spectrum_roundtrip_through_quantizer() {
        // A small synthetic spectrum: quantize then requantize should recover it.
        let header = hdr();
        let mut freq = [0f32; GRANULE_LINES];
        freq[40] = 5.0;
        freq[41] = -3.0;
        freq[100] = 1.2;
        freq[200] = 0.6;

        let psy = PsyResult::default();
        let q = loops(&header, &freq, &psy, 100_000, BlockType::Long); // generous → fine gain
        eprintln!(
            "[C2dbg] gain={} part2_3={} nz_coeffs={}",
            q.side.global_gain,
            q.side.part2_3_length,
            q.coeffs.iter().filter(|&&c| c != 0).count()
        );

        // Requantize the way the decoder does, with the granule's scalefactors.
        let mut sf = ScaleFactors::default();
        sf.long.copy_from_slice(&q.scalefactors[..22]);
        let mut out = [0f32; GRANULE_LINES];
        let nz = q.coeffs.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
        crate::decode::requantize::apply(&header, &q.side, &sf, &q.coeffs, nz, &mut out);

        let mut maxerr = 0f32;
        for i in 0..GRANULE_LINES {
            maxerr = maxerr.max((out[i] - freq[i]).abs());
        }
        eprintln!(
            "[C2dbg] requant maxerr={maxerr} out[40]={} out[41]={}",
            out[40], out[41]
        );
        assert!(maxerr < 0.2, "spectrum round-trip error {maxerr}");
    }
}

#[cfg(test)]
mod q6_tests {
    use super::*;
    use crate::frame::{BlockType, ChannelMode, GranuleSideInfo};
    use crate::header::MpegVersion;

    fn hdr() -> FrameHeader {
        FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: ChannelMode::Mono,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    /// Peak noise-to-mask ratio (dB) of a granule under `thresholds`.
    fn peak_nmr_db(
        header: &FrameHeader,
        freq: &[f32; GRANULE_LINES],
        g: &QuantizedGranule,
        thresholds: &[f32; 22],
    ) -> f32 {
        let mut sf = [0u8; 22];
        sf.copy_from_slice(&g.scalefactors[..22]);
        let noise = band_noise(header, freq, &g.coeffs, g.side.global_gain as i32, &sf);
        let mut peak = f32::NEG_INFINITY;
        for (b, &n) in noise.iter().enumerate() {
            peak = peak.max(10.0 * (n / thresholds[b].max(1e-20)).log10());
        }
        peak
    }

    #[test]
    fn distortion_loop_beats_flat_on_a_complex_signal() {
        // Two tones in different critical bands → low-masking bands sit next to
        // high-masking ones, so shaping noise to the threshold helps.
        let header = hdr();
        let sr = 44100.0;
        let pcm: Vec<f32> = (0..1152)
            .map(|i| {
                let t = i as f32 / sr;
                0.35 * (2.0 * std::f32::consts::PI * 600.0 * t).sin()
                    + 0.2 * (2.0 * std::f32::consts::PI * 5200.0 * t).sin()
            })
            .collect();

        let psy = super::super::psychoacoustic::analyze(&pcm, 44100);

        // Forward path to the MDCT spectrum.
        let mut fifo = [0f32; 512];
        let sub = super::super::filterbank::analyze(&pcm, &mut fifo);
        let mut overlap = [0f32; GRANULE_LINES];
        let mut freq = super::super::mdct::forward(&sub, BlockType::Long, &mut overlap);
        super::super::antialias::expand(&GranuleSideInfo::default(), &mut freq);

        let budget = 1600;
        let flat = PsyResult {
            thresholds: [f32::MAX; 22], // never over threshold → no shaping (pure rate)
            ..psy.clone()
        };
        let shaped = loops(&header, &freq, &psy, budget, BlockType::Long);
        let plain = loops(&header, &freq, &flat, budget, BlockType::Long);

        let nmr_shaped = peak_nmr_db(&header, &freq, &shaped, &psy.thresholds);
        let nmr_plain = peak_nmr_db(&header, &freq, &plain, &psy.thresholds);
        eprintln!("[Q6] peak NMR: shaped {nmr_shaped:.1} dB vs flat {nmr_plain:.1} dB");
        assert!(
            nmr_shaped <= nmr_plain + 0.01,
            "psymodel shaping must not worsen peak NMR: {nmr_shaped} vs {nmr_plain}"
        );
    }
}

#[cfg(test)]
mod n4_tests {
    use super::*;

    #[test]
    fn requant_magnitude_matches_power_law() {
        for level in [0, 1, 2, 3, 17, 255, 1024, MAX_LEVEL] {
            let expect = (level as f64).powf(4.0 / 3.0);
            assert!((requant_magnitude(level) - expect).abs() < 1e-9);
        }
        // Sign is carried separately, so the magnitude ignores it.
        assert_eq!(requant_magnitude(-3), requant_magnitude(3));
    }

    #[test]
    fn forward_inverse_round_trip_on_the_lattice() {
        // The verification gate: every representable level survives
        // requantize → quantize unchanged. If the BIAS or the power law were
        // wrong, some level would round to a neighbour.
        for level in 0..=MAX_LEVEL {
            let xr = requant_magnitude(level);
            assert_eq!(
                quantize_level(xr),
                level,
                "round-trip failed at level {level} (xr={xr})"
            );
        }
    }

    #[test]
    fn quantizer_clamps_and_zeroes() {
        assert_eq!(quantize_level(0.0), 0);
        // A value just below 1^(4/3) still rounds to 0 (below the first lattice
        // point, the bias pulls it under 0.5).
        assert_eq!(quantize_level(0.3), 0);
        // Saturates at MAX_LEVEL rather than overflowing.
        assert_eq!(quantize_level(1.0e9), MAX_LEVEL);
    }
}
