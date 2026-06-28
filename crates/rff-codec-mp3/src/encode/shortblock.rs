//! Short-block coding (brick **Q5**) — the forward reorder and the short-block
//! quantizer.
//!
//! Short blocks give the filterbank finer time resolution, which suppresses the
//! pre-echo a long block smears before a transient. The forward MDCT (`mdct.rs`)
//! already produces the three short windows interleaved in *subband* order; the
//! bitstream stores them in *scalefactor-band* order (sfb-major, then window, then
//! frequency). [`reorder_subband_to_bitstream`] is the exact inverse of the
//! decoder's requantize reorder, so a coded short block round-trips.
//!
//! This first version quantises uniformly (flat per-window scalefactors,
//! `subblock_gain = 0`) under global-gain rate control — valid short blocks that
//! fix pre-echo. Per-window perceptual shaping is a later refinement.

use crate::frame::{BlockType, GRANULE_LINES};
use crate::header::FrameHeader;
use crate::tables;

use super::quantize::QuantizedGranule;

const MAX_UNCLIPPED: i32 = 8191;

/// Reorder a short-block spectrum from the MDCT's subband-interleaved order into
/// the bitstream's `(sfb, window, freq)` order — the forward of the decoder's
/// requantize reorder (`out[dst] = coeffs[src]`).
pub fn reorder_subband_to_bitstream(
    sample_rate: u32,
    subband: &[f32; GRANULE_LINES],
) -> [f32; GRANULE_LINES] {
    let off = tables::sfb_short_offsets(sample_rate);
    let mut out = [0f32; GRANULE_LINES];
    for sfb in 0..13 {
        let start = off[sfb] as usize;
        let width = (off[sfb + 1] - off[sfb]) as usize;
        for window in 0..3 {
            for f in 0..width {
                let src = start * 3 + window * width + f;
                let dst = start * 3 + window + f * 3;
                if src < GRANULE_LINES && dst < GRANULE_LINES {
                    out[src] = subband[dst];
                }
            }
        }
    }
    out
}

/// Uniformly quantize bitstream-order short-block lines at `global_gain`
/// (`subblock_gain = 0`, flat scalefactors), the forward of the decoder's short
/// requantization with those fields zero. Uses the precomputed `|freq|^(3/4)`
/// (A1): each gain probe is a multiply-and-round, no per-line `powf`.
fn quantize_uniform(
    freq: &[f32; GRANULE_LINES],
    xrp: &[f64; GRANULE_LINES],
    gain: i32,
) -> [i32; GRANULE_LINES] {
    // step = scale_inv^(3/4), scale_inv = 2^(-0.25·(gain−210)).
    let step = 2f64.powf(0.75 * -0.25 * (gain - 210) as f64);
    let mut coeffs = [0i32; GRANULE_LINES];
    for (i, &x) in freq.iter().enumerate() {
        let mag = super::quantize::level_from(xrp[i] * step);
        coeffs[i] = if x < 0.0 { -mag } else { mag };
    }
    coeffs
}

/// **Q5 short quantizer** — quantize a short-block granule (already reordered to
/// bitstream order) to fit `bit_budget`, with flat per-window scalefactors. Picks
/// the smallest non-clipping `global_gain` whose Huffman cost fits.
pub fn quantize_short(
    header: &FrameHeader,
    freq_bitstream: &[f32; GRANULE_LINES],
    bit_budget: usize,
) -> QuantizedGranule {
    let xrp = super::quantize::xrpow(freq_bitstream); // A1: hoist |freq|^(3/4)
    let ok = |g: i32| {
        let coeffs = quantize_uniform(freq_bitstream, &xrp, g);
        coeffs.iter().all(|&c| c.abs() <= MAX_UNCLIPPED)
            && super::huffman::cost_short(header, &coeffs) <= bit_budget
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
    let coeffs = quantize_uniform(freq_bitstream, &xrp, lo);
    let mut side = super::huffman::select(header, &coeffs, BlockType::Short);
    side.global_gain = lo as u8;
    side.scalefac_compress = 0; // flat scalefactors → zero scalefactor bits
    QuantizedGranule {
        coeffs,
        side,
        scalefactors: [0; 39],
    }
}

/// **Q5 block-type FSM.** Given the previous granule's window type and this
/// frame's per-granule attack flags, choose valid window types that bracket every
/// attack with the required transition windows. Produces sequences like
/// `LONG…LONG | START, SHORT | (SHORT, SHORT)* | STOP, LONG` — a `START` always
/// precedes a `SHORT` and a `STOP` always follows it, the constraint the decoder's
/// overlapping windows require. MPEG-1 (2 granules per frame).
pub fn decide_block_types(prev: BlockType, attacks: &[bool]) -> (Vec<BlockType>, BlockType) {
    if attacks.len() != 2 {
        // MPEG-2 (single granule) not yet block-switched.
        return (vec![BlockType::Long; attacks.len()], BlockType::Long);
    }
    let any = attacks[0] || attacks[1];
    let types = if prev == BlockType::Short {
        // Inside a short run: continue or wind down with a STOP.
        if any {
            [BlockType::Short, BlockType::Short]
        } else {
            [BlockType::Stop, BlockType::Long]
        }
    } else if any {
        // Long/Stop and an attack this frame: transition in (START → SHORT).
        [BlockType::Start, BlockType::Short]
    } else {
        [BlockType::Long, BlockType::Long]
    };
    let new_prev = types[1];
    (types.to_vec(), new_prev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::scalefactors::ScaleFactors;
    use crate::frame::{ChannelMode, SideInfo};
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
    fn short_coefficients_round_trip_through_decoder() {
        // A short-block spectrum (subband order) → reorder → short quantize →
        // Huffman → decoder Huffman + reorder-requantize must recover it.
        let header = hdr();
        let mut s = 0xABCD_1234u32;
        let mut freq = [0f32; GRANULE_LINES];
        for (i, v) in freq.iter_mut().enumerate() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // taper high lines so there's an rzero tail (realistic short block)
            let taper = 1.0 - (i as f32 / GRANULE_LINES as f32);
            *v = ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 8.0 * taper;
        }

        let freq_bs = reorder_subband_to_bitstream(header.sample_rate, &freq);
        let quant = quantize_short(&header, &freq_bs, 100_000);

        // Emit the Huffman spectrum, then decode it back.
        let mut w = crate::bitio::BitWriter::new();
        let hbits = super::super::huffman::encode(&quant, &header, &mut w);
        let bits = w.finish();

        let mut gi = quant.side.clone();
        gi.part2_3_length = hbits as u16;
        let mut pos = 0;
        let (coeffs, nz) = crate::decode::huffman::decode(&bits, &mut pos, hbits, &header, &gi);
        assert_eq!(
            coeffs, quant.coeffs,
            "short Huffman must round-trip exactly"
        );

        // Requantize (with the decoder's short reorder) → subband order.
        let mut si = SideInfo::default();
        si.granules[0][0] = gi.clone();
        let mut out = [0f32; GRANULE_LINES];
        crate::decode::requantize::apply(
            &header,
            &gi,
            &ScaleFactors::default(),
            &coeffs,
            nz,
            &mut out,
        );

        // out (subband order) ≈ freq (subband order), to quantization error.
        let mut sig = 0f64;
        let mut err = 0f64;
        for i in 0..GRANULE_LINES {
            sig += (freq[i] as f64).powi(2);
            err += ((freq[i] - out[i]) as f64).powi(2);
        }
        let snr = 10.0 * (sig / err).log10();
        eprintln!("[Q5] short-block coefficient round-trip SNR {snr:.1} dB");
        assert!(snr > 40.0, "short-block coding SNR too low: {snr:.1} dB");
    }

    #[test]
    fn reorder_inverts_the_decoder_mapping() {
        // Forward reorder then the decoder's src/dst mapping must return the
        // original subband-order spectrum.
        let subband: [f32; GRANULE_LINES] = std::array::from_fn(|i| (i as f32 * 0.013).sin());
        let bitstream = reorder_subband_to_bitstream(44100, &subband);

        let off = tables::sfb_short_offsets(44100);
        let mut recovered = [0f32; GRANULE_LINES];
        for sfb in 0..13 {
            let start = off[sfb] as usize;
            let width = (off[sfb + 1] - off[sfb]) as usize;
            for window in 0..3 {
                for f in 0..width {
                    let src = start * 3 + window * width + f;
                    let dst = start * 3 + window + f * 3;
                    recovered[dst] = bitstream[src]; // decoder's placement
                }
            }
        }
        for i in 0..GRANULE_LINES {
            assert!((recovered[i] - subband[i]).abs() < 1e-9, "line {i}");
        }
    }
}
