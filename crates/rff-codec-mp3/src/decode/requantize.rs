//! Requantization: integer coefficients → dequantized frequency lines.
//!
//! Per line: `xr = sign(is) · |is|^(4/3) · 2^(0.25·A) · 2^(-B)` where `A` folds in
//! `global_gain` (minus 210, and `8·subblock_gain` per window for short blocks)
//! and `B` folds in the band scalefactor scaled by `scalefac_scale` (and
//! `preflag·pretab` for long blocks). Short blocks are simultaneously reordered
//! from scalefactor-band order (Huffman output) into subband order (IMDCT input):
//! within a band, coded `window·width + freq` maps to interleaved `window +
//! freq·3`.

use std::sync::OnceLock;

use crate::frame::{BlockType, GranuleSideInfo, GRANULE_LINES};
use crate::header::FrameHeader;
use crate::tables;

use super::scalefactors::ScaleFactors;

/// `|n|^(4/3)`, table-backed (Huffman magnitudes reach 8206 with linbits).
fn pow43(n: i32) -> f64 {
    static T: OnceLock<Vec<f64>> = OnceLock::new();
    let t = T.get_or_init(|| (0..8207).map(|i| (i as f64).powf(4.0 / 3.0)).collect());
    let a = n.unsigned_abs() as usize;
    t.get(a)
        .copied()
        .unwrap_or_else(|| (a as f64).powf(4.0 / 3.0))
}

fn dequant(c: i32, scale: f64) -> f32 {
    if c == 0 {
        0.0
    } else {
        (c.signum() as f64 * pow43(c) * scale) as f32
    }
}

/// Dequantize `coeffs` into `out` (576 lines), applying gains, scalefactors, and
/// — for short blocks — the reorder.
pub fn apply(
    header: &FrameHeader,
    gi: &GranuleSideInfo,
    sf: &ScaleFactors,
    coeffs: &[i32; GRANULE_LINES],
    _nz: usize,
    out: &mut [f32; GRANULE_LINES],
) {
    out.fill(0.0);
    let gain = gi.global_gain as i32 - 210;
    let sf_mult = if gi.scalefac_scale { 1.0f64 } else { 0.5 };
    let is_short = gi.window_switching && gi.block_type == BlockType::Short;

    if !is_short {
        // Long block: each band scales a contiguous run of lines.
        let off = tables::sfb_long_offsets(header.sample_rate);
        for sfb in 0..22 {
            let start = off[sfb] as usize;
            let end = (off[sfb + 1] as usize).min(GRANULE_LINES);
            let pre = if gi.preflag { tables::PRETAB[sfb] } else { 0 } as f64;
            let exp = 0.25 * gain as f64 - sf_mult * (sf.long[sfb] as f64 + pre);
            let scale = 2f64.powf(exp);
            for i in start..end {
                out[i] = dequant(coeffs[i], scale);
            }
        }
    } else {
        // Short block: dequant per (band, window) and reorder into subband order.
        // brick: mixed blocks (long bands 0..2 then short) are not yet special-cased.
        let off = tables::sfb_short_offsets(header.sample_rate);
        for sfb in 0..13 {
            let start = off[sfb] as usize;
            let width = (off[sfb + 1] - off[sfb]) as usize;
            for window in 0..3 {
                let gain_w = gain - 8 * gi.subblock_gain[window] as i32;
                let exp = 0.25 * gain_w as f64 - sf_mult * (sf.short[window][sfb] as f64);
                let scale = 2f64.powf(exp);
                for f in 0..width {
                    let src = start * 3 + window * width + f;
                    let dst = start * 3 + window + f * 3;
                    if src < GRANULE_LINES && dst < GRANULE_LINES {
                        out[dst] = dequant(coeffs[src], scale);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn long_block_power_law_and_sign() {
        // global_gain 210 → gain 0, scalefac 0, no preflag → scale = 2^0 = 1, so
        // xr = sign(is)·|is|^(4/3).
        let gi = GranuleSideInfo {
            global_gain: 210,
            ..Default::default()
        };
        let sf = ScaleFactors::default();
        let mut coeffs = [0i32; GRANULE_LINES];
        coeffs[0] = 3;
        coeffs[1] = -2;
        coeffs[2] = 1;
        let mut out = [0f32; GRANULE_LINES];
        apply(&hdr(), &gi, &sf, &coeffs, 3, &mut out);
        assert!((out[0] - 3f32.powf(4.0 / 3.0)).abs() < 1e-4);
        assert!((out[1] + 2f32.powf(4.0 / 3.0)).abs() < 1e-4);
        assert!((out[2] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn scalefactor_halves_the_step() {
        // scalefac 2 on band 0, scalefac_scale 0 (×0.5) → B = 0.5·2 = 1 → ÷2.
        let gi = GranuleSideInfo {
            global_gain: 210,
            ..Default::default()
        };
        let mut sf = ScaleFactors::default();
        sf.long[0] = 2;
        let mut coeffs = [0i32; GRANULE_LINES];
        coeffs[0] = 1;
        let mut out = [0f32; GRANULE_LINES];
        apply(&hdr(), &gi, &sf, &coeffs, 1, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-4, "1^(4/3)·2^-1 = 0.5");
    }
}
