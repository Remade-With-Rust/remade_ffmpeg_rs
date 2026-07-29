//! VP9 encoder — uncompressed frame-header serializer (Floor 2, brick B7).
//!
//! [`write_uncompressed_header`] is the exact inverse of
//! [`parse_uncompressed_header`](crate::parse_uncompressed_header), writing every
//! field MSB-first through the [`BitWriter`] in the same order the parser reads
//! it (ISO/VP9 §6.2). Gated by a `serialize → parse == FrameHeader` round-trip.

use super::bitwriter::BitWriter;
use crate::{FrameHeader, CS_RGB, FRAME_MARKER, SYNC_CODE};

fn write_color_config(w: &mut BitWriter, h: &FrameHeader) {
    if h.profile >= 2 {
        w.put_bit((h.bit_depth == 12) as u32); // 1 → 12-bit, 0 → 10-bit
    }
    w.put(h.color_space, 3);
    if h.color_space != CS_RGB {
        w.put_bit(0); // color_range
        if h.profile == 1 || h.profile == 3 {
            w.put_bit(h.subsampling_x);
            w.put_bit(h.subsampling_y);
            w.put_bit(0); // reserved_zero
        }
    } else if h.profile == 1 || h.profile == 3 {
        w.put_bit(0); // reserved_zero
    }
}

fn write_frame_size(w: &mut BitWriter, h: &FrameHeader) {
    w.put(h.width - 1, 16);
    w.put(h.height - 1, 16);
}

fn write_loop_filter(w: &mut BitWriter, h: &FrameHeader) {
    w.put(h.loop_filter_level, 6);
    w.put(h.loop_filter_sharpness, 3);
    w.put_bit(h.lf_delta_enabled as u32);
    if h.lf_delta_enabled {
        let any =
            h.lf_ref_delta_updated.iter().any(|&x| x) || h.lf_mode_delta_updated.iter().any(|&x| x);
        w.put_bit(any as u32); // mode_ref_delta_update
        if any {
            for i in 0..4 {
                w.put_bit(h.lf_ref_delta_updated[i] as u32);
                if h.lf_ref_delta_updated[i] {
                    w.put_signed(h.lf_ref_deltas[i], 6);
                }
            }
            for i in 0..2 {
                w.put_bit(h.lf_mode_delta_updated[i] as u32);
                if h.lf_mode_delta_updated[i] {
                    w.put_signed(h.lf_mode_deltas[i], 6);
                }
            }
        }
    }
}

fn write_delta_q(w: &mut BitWriter, dq: i32) {
    if dq != 0 {
        w.put_bit(1);
        w.put_signed(dq, 4);
    } else {
        w.put_bit(0);
    }
}

fn write_quant(w: &mut BitWriter, h: &FrameHeader) {
    w.put(h.base_q_idx, 8);
    write_delta_q(w, h.delta_q_y_dc);
    write_delta_q(w, h.delta_q_uv_dc);
    write_delta_q(w, h.delta_q_uv_ac);
}

fn write_segmentation(w: &mut BitWriter, h: &FrameHeader) {
    w.put_bit(h.seg_enabled as u32);
    if !h.seg_enabled {
        return;
    }
    w.put_bit(h.seg_update_map as u32);
    if h.seg_update_map {
        for i in 0..7 {
            if h.seg_tree_probs[i] != 255 {
                w.put_bit(1);
                w.put(h.seg_tree_probs[i] as u32, 8);
            } else {
                w.put_bit(0);
            }
        }
        w.put_bit(h.seg_temporal_update as u32);
        if h.seg_temporal_update {
            for i in 0..3 {
                if h.seg_pred_probs[i] != 255 {
                    w.put_bit(1);
                    w.put(h.seg_pred_probs[i] as u32, 8);
                } else {
                    w.put_bit(0);
                }
            }
        }
    }
    w.put_bit(h.seg_update_data as u32);
    if h.seg_update_data {
        w.put_bit(h.seg_abs_delta as u32);
        const BITS: [u32; 4] = [8, 6, 2, 0];
        const SIGNED: [bool; 4] = [true, true, false, false];
        for i in 0..8 {
            for j in 0..4 {
                w.put_bit(h.seg_feature_enabled[i][j] as u32);
                if h.seg_feature_enabled[i][j] {
                    let data = h.seg_feature_data[i][j];
                    if BITS[j] > 0 {
                        w.put(data.unsigned_abs(), BITS[j]);
                    }
                    if SIGNED[j] {
                        w.put_bit((data < 0) as u32);
                    }
                }
            }
        }
    }
}

fn write_tile_info(w: &mut BitWriter, h: &FrameHeader) {
    let mi_cols = (h.width + 7) >> 3;
    let sb64_cols = (mi_cols + 7) >> 3;
    let mut min_log2 = 0u32;
    while (64u32 << min_log2) < sb64_cols {
        min_log2 += 1;
    }
    let mut max_log2 = 1u32;
    while (sb64_cols >> max_log2) >= 4 {
        max_log2 += 1;
    }
    max_log2 -= 1;
    // Increment bits from min_log2 up to the chosen tile_cols_log2.
    let mut cur = min_log2;
    while cur < max_log2 {
        if cur < h.tile_cols_log2 {
            w.put_bit(1);
            cur += 1;
        } else {
            w.put_bit(0);
            break;
        }
    }
    w.put_bit((h.tile_rows_log2 >= 1) as u32);
    if h.tile_rows_log2 >= 1 {
        w.put_bit((h.tile_rows_log2 >= 2) as u32);
    }
}

/// Serialize the VP9 uncompressed frame header — the inverse of
/// [`parse_uncompressed_header`](crate::parse_uncompressed_header).
pub fn write_uncompressed_header(w: &mut BitWriter, h: &FrameHeader) {
    w.put(FRAME_MARKER, 2);
    w.put_bit(h.profile & 1); // profile_low
    w.put_bit((h.profile >> 1) & 1); // profile_high
    if h.profile == 3 {
        w.put_bit(0); // reserved_zero
    }

    w.put_bit(h.show_existing_frame as u32);
    if h.show_existing_frame {
        w.put(h.frame_to_show, 3);
        return;
    }

    w.put_bit((!h.key_frame) as u32); // 0 = key frame
    w.put_bit(h.show_frame as u32);
    w.put_bit(h.error_resilient as u32);

    if h.key_frame {
        w.put(SYNC_CODE, 24);
        write_color_config(w, h);
        write_frame_size(w, h);
        w.put_bit(0); // render_and_frame_size_different = 0
    } else {
        if !h.show_frame {
            w.put_bit(h.intra_only as u32);
        }
        if !h.error_resilient {
            w.put(h.reset_frame_context, 2);
        }
        if h.intra_only {
            w.put(SYNC_CODE, 24);
            if h.profile > 0 {
                write_color_config(w, h);
            }
            w.put(h.refresh_frame_flags, 8);
            write_frame_size(w, h);
            w.put_bit(0); // render_size
        } else {
            w.put(h.refresh_frame_flags, 8);
            for i in 0..3 {
                w.put(h.ref_frame_idx[i] as u32, 3);
                w.put_bit(h.ref_sign_bias[i] as u32);
            }
            // frame_size_with_refs: encode an explicit size (no inheritance).
            for _ in 0..3 {
                w.put_bit(0); // found_ref = 0
            }
            write_frame_size(w, h);
            w.put_bit(0); // render_size
            w.put_bit(h.allow_high_precision_mv as u32);
            if h.interp_filter == 4 {
                w.put_bit(1); // is_filter_switchable
            } else {
                w.put_bit(0);
                // Inverse of literal_to_filter = [1, 0, 2, 3] (self-inverse).
                const F2L: [u32; 4] = [1, 0, 2, 3];
                w.put(F2L[h.interp_filter as usize], 2);
            }
        }
    }

    // Common tail (ISO/VP9 §6.2).
    if !h.error_resilient {
        w.put_bit(h.refresh_frame_context as u32);
        w.put_bit(h.frame_parallel_decoding_mode as u32);
    }
    w.put(h.frame_context_idx, 2);
    write_loop_filter(w, h);
    write_quant(w, h);
    write_segmentation(w, h);
    write_tile_info(w, h);
    w.put(h.header_size, 16);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_uncompressed_header, BitReader};

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// Round-trip a header: serialize → parse → struct-equal. `uncompressed_bytes`
    /// is a parse-side artifact (the byte cursor), so it is normalised first.
    fn roundtrip(h: &FrameHeader) {
        let mut w = BitWriter::new();
        write_uncompressed_header(&mut w, h);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let parsed = parse_uncompressed_header(&mut r, &[(0, 0); 8]).unwrap();
        let mut expect = h.clone();
        expect.uncompressed_bytes = parsed.uncompressed_bytes;
        assert_eq!(parsed, expect);
    }

    fn key_frame(w: u32, hgt: u32) -> FrameHeader {
        FrameHeader {
            profile: 0,
            key_frame: true,
            show_frame: true,
            bit_depth: 8,
            color_space: 1, // CS_BT_601
            subsampling_x: 1,
            subsampling_y: 1,
            width: w,
            height: hgt,
            lf_ref_deltas: [1, 0, -1, -1], // spec defaults
            lf_mode_deltas: [0, 0],
            seg_tree_probs: [255; 7],
            seg_pred_probs: [255; 3],
            sized: true,
            ..Default::default()
        }
    }

    #[test]
    fn keyframe_headers_roundtrip() {
        let mut s = 0x1122_3344_5566_7788u64;
        for _ in 0..400 {
            let mut h = key_frame(
                1 + (xs(&mut s) % 4096) as u32,
                1 + (xs(&mut s) % 4096) as u32,
            );
            h.color_space = (xs(&mut s) % 7) as u32; // 0..6 (not RGB)
            h.error_resilient = xs(&mut s) & 1 == 0;
            if !h.error_resilient {
                h.refresh_frame_context = xs(&mut s) & 1 == 0;
                h.frame_parallel_decoding_mode = xs(&mut s) & 1 == 0;
            } else {
                h.frame_parallel_decoding_mode = true;
            }
            h.frame_context_idx = (xs(&mut s) % 4) as u32;
            h.loop_filter_level = (xs(&mut s) % 64) as u32;
            h.loop_filter_sharpness = (xs(&mut s) % 8) as u32;
            h.base_q_idx = (xs(&mut s) % 256) as u32;
            h.delta_q_y_dc = (xs(&mut s) % 31) as i32 - 15;
            h.delta_q_uv_dc = (xs(&mut s) % 31) as i32 - 15;
            h.delta_q_uv_ac = (xs(&mut s) % 31) as i32 - 15;
            h.lossless = h.base_q_idx == 0
                && h.delta_q_y_dc == 0
                && h.delta_q_uv_dc == 0
                && h.delta_q_uv_ac == 0;
            h.tile_rows_log2 = (xs(&mut s) % 3) as u32;
            h.header_size = 1 + (xs(&mut s) % 60000) as u32;
            roundtrip(&h);
        }
    }

    #[test]
    fn keyframe_with_loopfilter_and_segmentation() {
        let mut h = key_frame(1920, 1080);
        h.base_q_idx = 64;
        h.header_size = 1234;
        // Loop-filter deltas (only the updated ones must differ from defaults).
        h.lf_delta_enabled = true;
        h.lf_ref_delta_updated = [true, false, false, true];
        h.lf_ref_deltas = [3, 0, -1, 2]; // index 1,2 stay at defaults (0, -1)
        h.lf_mode_delta_updated = [false, true];
        h.lf_mode_deltas = [0, -2];
        // Segmentation with a map + feature data.
        h.seg_enabled = true;
        h.seg_update_map = true;
        h.seg_tree_probs = [120, 255, 90, 200, 255, 130, 60];
        h.seg_update_data = true;
        h.seg_abs_delta = true;
        h.seg_feature_enabled[0][0] = true; // ALT_Q (8-bit signed)
        h.seg_feature_data[0][0] = -40;
        h.seg_feature_enabled[2][2] = true; // REF_FRAME (2-bit unsigned)
        h.seg_feature_data[2][2] = 2;
        h.seg_feature_enabled[3][3] = true; // SKIP (flag, 0 bits)
        roundtrip(&h);
    }

    #[test]
    fn inter_headers_roundtrip() {
        let mut s = 0x9090_a0a0_b0b0_c0c0u64;
        for _ in 0..200 {
            let mut h = key_frame(1280, 720);
            h.key_frame = false;
            // Inter frames carry no color config (the decoder inherits it), so the
            // parser leaves these at default — match that.
            h.bit_depth = 0;
            h.color_space = 0;
            h.subsampling_x = 0;
            h.subsampling_y = 0;
            h.show_frame = xs(&mut s) & 1 == 0;
            h.intra_only = false;
            h.refresh_frame_flags = (xs(&mut s) % 256) as u32;
            for i in 0..3 {
                h.ref_frame_idx[i] = (xs(&mut s) % 8) as usize;
                h.ref_sign_bias[i] = xs(&mut s) & 1 == 0;
            }
            h.allow_high_precision_mv = xs(&mut s) & 1 == 0;
            h.interp_filter = (xs(&mut s) % 5) as u32; // 0..3 fixed, 4 switchable
            h.reset_frame_context = (xs(&mut s) % 4) as u32;
            h.base_q_idx = 1 + (xs(&mut s) % 255) as u32;
            h.lossless = false;
            h.header_size = 1 + (xs(&mut s) % 60000) as u32;
            roundtrip(&h);
        }
    }
}
