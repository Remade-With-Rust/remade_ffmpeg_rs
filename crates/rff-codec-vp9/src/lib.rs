//! In-house **VP9 decoder**, pure Rust with no C/FFI.
//!
//! VP9 is a large block-based video codec. Like our AAC decoder, it is built in
//! verifiable stages; this layer is the bitstream foundation: the MSB bit reader
//! and the boolean arithmetic decoder ([`bits`]), plus the **uncompressed frame
//! header** parser (frame type, profile, colour config, frame size). The
//! compressed header, intra/inter reconstruction, transforms and loop filter
//! land in later stages — until then [`receive_frame`](rff_codec::Decoder::receive_frame)
//! reports [`Error::Unimplemented`] while framing/headers are fully functional.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder};
use rff_core::{CodecId, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

mod adapt;
mod bits;
mod block;
mod decode;
#[allow(dead_code, unused_imports)] // Foundation API; consumed from plan Floor 1+.
mod encode;
mod geom_tables;
mod inter;
mod loopfilter;
mod mv;
mod predict;
mod prob;
mod prob_tables;
mod quant;
mod scan_tables;
mod token;
mod transform;
pub use bits::{BitReader, BoolDecoder};

pub(crate) const FRAME_MARKER: u32 = 2;
pub(crate) const SYNC_CODE: u32 = 0x49_8342;
pub(crate) const CS_RGB: u32 = 7;

/// Register the VP9 decoder into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Vp9,
        name: "vp9",
        long_name: "Google VP9",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(Vp9Decoder::default())),
        encoder: None,
    });
}

/// The parsed VP9 uncompressed frame header (the fields decoded so far).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameHeader {
    pub profile: u32,
    pub show_existing_frame: bool,
    pub frame_to_show: u32,
    pub key_frame: bool,
    pub show_frame: bool,
    pub error_resilient: bool,
    pub intra_only: bool,
    pub bit_depth: u32,
    pub color_space: u32,
    pub subsampling_x: u32,
    pub subsampling_y: u32,
    pub width: u32,
    pub height: u32,
    pub lossless: bool,
    pub base_q_idx: u32,
    pub delta_q_y_dc: i32,
    pub delta_q_uv_dc: i32,
    pub delta_q_uv_ac: i32,
    pub loop_filter_level: u32,
    pub loop_filter_sharpness: u32,
    pub lf_delta_enabled: bool,
    /// Per-reference-frame loop-filter level deltas (INTRA, LAST, GOLDEN, ALT).
    pub lf_ref_deltas: [i32; 4],
    /// Per-mode loop-filter level deltas (ZEROMV-vs-other).
    pub lf_mode_deltas: [i32; 2],
    /// Which ref/mode deltas this frame signalled an update for (the rest carry
    /// over the persisted value — resolved by the stateful decoder).
    pub lf_ref_delta_updated: [bool; 4],
    pub lf_mode_delta_updated: [bool; 2],
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
    /// Compressed-header length in bytes (follows the uncompressed header).
    pub header_size: u32,
    /// Byte length of the uncompressed header (where the compressed one starts).
    pub uncompressed_bytes: usize,
    /// True once the frame is fully sized (key or intra-only).
    pub sized: bool,
    // ---- inter-frame fields (ISO/VP9 §6.2) ----
    /// `reset_frame_context` (0..3) — how much frame-context state to reset.
    pub reset_frame_context: u32,
    /// Bitmask of the 8 reference slots refreshed by this frame.
    pub refresh_frame_flags: u32,
    /// Slot index in the 8-entry ref map for LAST/GOLDEN/ALTREF.
    pub ref_frame_idx: [usize; 3],
    /// Sign bias per reference (LAST/GOLDEN/ALTREF).
    pub ref_sign_bias: [bool; 3],
    /// True if MVs use 1/8-pel precision (else 1/4-pel).
    pub allow_high_precision_mv: bool,
    /// Interpolation filter: 0..3 fixed, 4 = SWITCHABLE.
    pub interp_filter: u32,
    pub refresh_frame_context: bool,
    pub frame_parallel_decoding_mode: bool,
    pub frame_context_idx: u32,
    // ---- segmentation (ISO/VP9 §6.2.11) ----
    pub seg_enabled: bool,
    pub seg_update_map: bool,
    pub seg_update_data: bool,
    pub seg_temporal_update: bool,
    pub seg_abs_delta: bool,
    pub seg_tree_probs: [u8; 7],
    pub seg_pred_probs: [u8; 3],
    pub seg_feature_enabled: [[bool; 4]; 8],
    pub seg_feature_data: [[i32; 4]; 8],
}

/// Parse the VP9 uncompressed header (ISO/VP9 spec §6.2). `ref_dims` gives the
/// (width, height) of each of the 8 reference slots, used to resolve an inter
/// frame's `frame_size_with_refs`; pass zeros when only key/intra frames are
/// expected (the size then comes from the explicit fields).
pub fn parse_uncompressed_header(
    r: &mut BitReader,
    ref_dims: &[(u32, u32); 8],
) -> Result<FrameHeader> {
    let mut h = FrameHeader::default();
    if r.f(2)? != FRAME_MARKER {
        return Err(Error::invalid("vp9: bad frame marker"));
    }
    let profile_low = r.f1()?;
    let profile_high = r.f1()?;
    h.profile = (profile_high << 1) | profile_low;
    if h.profile == 3 {
        r.f1()?; // reserved_zero
    }

    h.show_existing_frame = r.f1()? == 1;
    if h.show_existing_frame {
        h.frame_to_show = r.f(3)?;
        return Ok(h);
    }

    h.key_frame = r.f1()? == 0;
    h.show_frame = r.f1()? == 1;
    h.error_resilient = r.f1()? == 1;

    if h.key_frame {
        frame_sync_code(r)?;
        color_config(r, &mut h)?;
        frame_size(r, &mut h)?;
        render_size(r)?;
        h.sized = true;
    } else {
        h.intra_only = if !h.show_frame { r.f1()? == 1 } else { false };
        h.reset_frame_context = if !h.error_resilient { r.f(2)? } else { 0 };
        if h.intra_only {
            frame_sync_code(r)?;
            if h.profile > 0 {
                color_config(r, &mut h)?;
            } else {
                h.bit_depth = 8;
                h.color_space = 1; // CS_BT_601
                h.subsampling_x = 1;
                h.subsampling_y = 1;
            }
            h.refresh_frame_flags = r.f(8)?;
            frame_size(r, &mut h)?;
            render_size(r)?;
            h.sized = true;
        } else {
            // Inter frame: refresh mask, three references + sign bias, then a
            // frame size that may be inherited from a reference.
            h.refresh_frame_flags = r.f(8)?;
            for i in 0..3 {
                h.ref_frame_idx[i] = r.f(3)? as usize;
                h.ref_sign_bias[i] = r.f1()? == 1;
            }
            // setup_frame_size_with_refs
            let mut found = false;
            for i in 0..3 {
                if r.f1()? == 1 {
                    let (w, hh) = ref_dims[h.ref_frame_idx[i]];
                    h.width = w;
                    h.height = hh;
                    found = true;
                    break;
                }
            }
            if !found {
                frame_size(r, &mut h)?;
            }
            render_size(r)?;
            h.allow_high_precision_mv = r.f1()? == 1;
            // read_interp_filter
            h.interp_filter = if r.f1()? == 1 {
                4 // SWITCHABLE
            } else {
                // literal_to_filter: {EIGHTTAP_SMOOTH(1), EIGHTTAP(0), EIGHTTAP_SHARP(2), BILINEAR(3)}
                const L2F: [u32; 4] = [1, 0, 2, 3];
                L2F[r.f(2)? as usize]
            };
            // Inter frames inherit color/subsampling from the active sequence;
            // the decoder fills these from the key frame's color_config.
            h.sized = true;
        }
    }

    if !h.sized {
        return Ok(h);
    }

    // ---- common tail (ISO/VP9 §6.2) --------------------------------------
    if !h.error_resilient {
        h.refresh_frame_context = r.f1()? == 1;
        h.frame_parallel_decoding_mode = r.f1()? == 1;
    } else {
        h.frame_parallel_decoding_mode = true;
    }
    h.frame_context_idx = r.f(2)?;
    loop_filter_params(r, &mut h)?;
    quantization_params(r, &mut h)?;
    segmentation_params(r, &mut h)?;
    tile_info(r, &mut h)?;
    h.header_size = r.f(16)?;
    h.uncompressed_bytes = r.bit_pos().div_ceil(8);
    Ok(h)
}

fn loop_filter_params(r: &mut BitReader, h: &mut FrameHeader) -> Result<()> {
    h.loop_filter_level = r.f(6)?;
    h.loop_filter_sharpness = r.f(3)?;
    // Key/intra-only frames reset the deltas to the spec defaults
    // (`setup_past_independence`) before any signalled update.
    // Defaults for direct callers; the stateful decoder overrides these with the
    // persisted values (the deltas carry frame-to-frame; only `update`-flagged
    // ones change), resetting to these defaults on past-independence frames.
    h.lf_ref_deltas = [1, 0, -1, -1];
    h.lf_mode_deltas = [0, 0];
    h.lf_ref_delta_updated = [false; 4];
    h.lf_mode_delta_updated = [false; 2];
    h.lf_delta_enabled = r.f1()? == 1;
    if h.lf_delta_enabled {
        // mode_ref_delta_update
        if r.f1()? == 1 {
            for i in 0..4 {
                if r.f1()? == 1 {
                    h.lf_ref_deltas[i] = r.s(6)?;
                    h.lf_ref_delta_updated[i] = true;
                }
            }
            for i in 0..2 {
                if r.f1()? == 1 {
                    h.lf_mode_deltas[i] = r.s(6)?;
                    h.lf_mode_delta_updated[i] = true;
                }
            }
        }
    }
    Ok(())
}

fn quantization_params(r: &mut BitReader, h: &mut FrameHeader) -> Result<()> {
    h.base_q_idx = r.f(8)?;
    let dc_y = read_delta_q(r)?;
    let dc_uv = read_delta_q(r)?;
    let ac_uv = read_delta_q(r)?;
    h.delta_q_y_dc = dc_y;
    h.delta_q_uv_dc = dc_uv;
    h.delta_q_uv_ac = ac_uv;
    h.lossless = h.base_q_idx == 0 && dc_y == 0 && dc_uv == 0 && ac_uv == 0;
    Ok(())
}

fn read_delta_q(r: &mut BitReader) -> Result<i32> {
    if r.f1()? == 1 {
        r.s(4)
    } else {
        Ok(0)
    }
}

fn segmentation_params(r: &mut BitReader, h: &mut FrameHeader) -> Result<()> {
    h.seg_tree_probs = [255; 7];
    h.seg_pred_probs = [255; 3];
    h.seg_enabled = r.f1()? == 1;
    if !h.seg_enabled {
        return Ok(());
    }
    h.seg_update_map = r.f1()? == 1;
    if h.seg_update_map {
        for i in 0..7 {
            h.seg_tree_probs[i] = if r.f1()? == 1 { r.f(8)? as u8 } else { 255 };
        }
        h.seg_temporal_update = r.f1()? == 1;
        if h.seg_temporal_update {
            for i in 0..3 {
                h.seg_pred_probs[i] = if r.f1()? == 1 { r.f(8)? as u8 } else { 255 };
            }
        }
    }
    // update_data
    h.seg_update_data = r.f1()? == 1;
    if h.seg_update_data {
        h.seg_abs_delta = r.f1()? == 1;
        const BITS: [u32; 4] = [8, 6, 2, 0];
        const SIGNED: [bool; 4] = [true, true, false, false];
        h.seg_feature_enabled = [[false; 4]; 8];
        h.seg_feature_data = [[0; 4]; 8];
        for i in 0..8 {
            for j in 0..4 {
                let enabled = r.f1()? == 1;
                h.seg_feature_enabled[i][j] = enabled;
                if enabled {
                    let mut data = if BITS[j] > 0 { r.f(BITS[j])? as i32 } else { 0 };
                    if SIGNED[j] && r.f1()? == 1 {
                        data = -data;
                    }
                    h.seg_feature_data[i][j] = data;
                }
            }
        }
    }
    Ok(())
}

fn tile_info(r: &mut BitReader, h: &mut FrameHeader) -> Result<()> {
    let mi_cols = (h.width + 7) >> 3;
    let sb64_cols = (mi_cols + 7) >> 3;
    // calc_min_log2_tile_cols
    let mut min_log2 = 0u32;
    while (64u32 << min_log2) < sb64_cols {
        min_log2 += 1;
    }
    // calc_max_log2_tile_cols
    let mut max_log2 = 1u32;
    while (sb64_cols >> max_log2) >= 4 {
        max_log2 += 1;
    }
    max_log2 -= 1;

    h.tile_cols_log2 = min_log2;
    while h.tile_cols_log2 < max_log2 {
        if r.f1()? == 1 {
            h.tile_cols_log2 += 1;
        } else {
            break;
        }
    }
    h.tile_rows_log2 = r.f1()?;
    if h.tile_rows_log2 == 1 {
        h.tile_rows_log2 += r.f1()?;
    }
    Ok(())
}

fn frame_sync_code(r: &mut BitReader) -> Result<()> {
    if r.f(24)? != SYNC_CODE {
        return Err(Error::invalid("vp9: bad frame sync code"));
    }
    Ok(())
}

fn color_config(r: &mut BitReader, h: &mut FrameHeader) -> Result<()> {
    h.bit_depth = if h.profile >= 2 {
        if r.f1()? == 1 {
            12
        } else {
            10
        }
    } else {
        8
    };
    h.color_space = r.f(3)?;
    if h.color_space != CS_RGB {
        r.f1()?; // color_range
        if h.profile == 1 || h.profile == 3 {
            h.subsampling_x = r.f1()?;
            h.subsampling_y = r.f1()?;
            r.f1()?; // reserved_zero
        } else {
            h.subsampling_x = 1;
            h.subsampling_y = 1;
        }
    } else {
        // sRGB: full range, no chroma subsampling.
        if h.profile == 1 || h.profile == 3 {
            r.f1()?; // reserved_zero
        }
    }
    Ok(())
}

fn frame_size(r: &mut BitReader, h: &mut FrameHeader) -> Result<()> {
    h.width = r.f(16)? + 1;
    h.height = r.f(16)? + 1;
    Ok(())
}

fn render_size(r: &mut BitReader) -> Result<()> {
    if r.f1()? == 1 {
        r.f(16)?; // render_width_minus_1
        r.f(16)?; // render_height_minus_1
    }
    Ok(())
}

// ---- compressed header (key-frame path), ISO/VP9 §6.3 --------------------

/// Structurally decode the compressed header for a key/intra frame over `data`
/// (which must be exactly `header_size` bytes), returning the bits the boolean
/// decoder consumed. A correct structure consumes ~all of `data`.
pub fn consume_compressed_header(data: &[u8], lossless: bool) -> Result<usize> {
    let mut b = BoolDecoder::new(data)?;
    let tx_mode = read_tx_mode(&mut b, lossless);
    if tx_mode == TX_MODE_SELECT {
        tx_mode_probs(&mut b);
    }
    read_coef_probs(&mut b, tx_mode);
    read_skip_prob(&mut b);
    Ok(b.bit_pos())
}

const TX_MODE_SELECT: u32 = 4;

fn read_tx_mode(b: &mut BoolDecoder, lossless: bool) -> u32 {
    if lossless {
        0
    } else {
        let mut t = b.literal(2);
        if t == 3 {
            t += b.literal(1); // ALLOW_32X32 → TX_MODE_SELECT
        }
        t
    }
}

fn tx_mode_probs(b: &mut BoolDecoder) {
    for _ in 0..2 * 1 {
        diff_update(b); // 8x8
    }
    for _ in 0..2 * 2 {
        diff_update(b); // 16x16
    }
    for _ in 0..2 * 3 {
        diff_update(b); // 32x32
    }
}

/// Coefficient-probability updates (libvpx `read_coef_probs`): transform sizes
/// `TX_4X4..=max_tx` where `max_tx = tx_mode_to_biggest_tx_size[tx_mode]`, each
/// gated by an update-present bit, then `[plane=2][ref=2][band=6][ctx][node=3]`
/// with band 0 having 3 contexts and bands 1-5 having 6.
fn read_coef_probs(b: &mut BoolDecoder, tx_mode: u32) {
    let max_tx = [0u32, 1, 2, 3, 3][tx_mode as usize];
    for _tx in 0..=max_tx {
        if b.read_bool(128) == 1 {
            for _plane in 0..2 {
                for _ref in 0..2 {
                    for band in 0..6 {
                        let ctxs = if band == 0 { 3 } else { 6 };
                        for _ctx in 0..ctxs {
                            for _node in 0..3 {
                                diff_update(b);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn read_skip_prob(b: &mut BoolDecoder) {
    for _ in 0..3 {
        diff_update(b);
    }
}

/// `diff_update_prob`: a flag at prob 252, then a sub-exponential delta.
fn diff_update(b: &mut BoolDecoder) {
    if b.read_bool(252) == 1 {
        decode_term_subexp(b);
    }
}

fn decode_term_subexp(b: &mut BoolDecoder) -> u32 {
    if b.read_bool(128) == 0 {
        return b.literal(4);
    }
    if b.read_bool(128) == 0 {
        return b.literal(4) + 16;
    }
    if b.read_bool(128) == 0 {
        return b.literal(5) + 32;
    }
    let v = b.literal(7);
    if v < 65 {
        return v + 64;
    }
    (v << 1) - 1 + b.read_bool(128)
}

#[derive(Default)]
struct Vp9Decoder {
    width: u32,
    height: u32,
    queue: VecDeque<(Vec<u8>, Option<i64>, bool)>,
    eof: bool,
    /// The eight reference-frame slots (`ref_frame_map`).
    ref_frames: [Option<std::sync::Arc<decode::RefFrame>>; 8],
    /// The four saved entropy contexts (`frame_contexts`).
    frame_contexts: [decode::FrameContext; 4],
    /// Previous frame's per-mi motion records (for temporal MV prediction).
    prev_mvs: Option<std::sync::Arc<Vec<mv::MvRef>>>,
    /// Previous frame's segment map (for temporal segment-id prediction).
    prev_seg_map: Option<std::sync::Arc<Vec<u8>>>,
    /// Persistent segmentation features (cleared on key/intra; replaced by an
    /// `update_data` signal; otherwise carried frame to frame).
    seg_feature_enabled: [[bool; 4]; 8],
    seg_feature_data: [[i32; 4]; 8],
    seg_abs_delta: bool,
    /// Persistent loop-filter deltas (cleared on past-independence; otherwise
    /// carried frame to frame, with only `update`-flagged entries changing).
    lf_ref_deltas: [i32; 4],
    lf_mode_deltas: [i32; 2],
    /// Whether the previously decoded frame was a key frame (coef-adapt factor).
    last_frame_key: bool,
    last_show_frame: bool,
    last_intra_only: bool,
    last_width: u32,
    last_height: u32,
    /// Color subsampling from the active key frame; inter frames inherit it.
    ss_x: u32,
    ss_y: u32,
    /// Bit depth from the active key frame's color config; inter frames inherit it.
    bit_depth: u32,
}

impl Vp9Decoder {
    /// (width, height) of each of the 8 reference slots, for `frame_size_with_refs`.
    fn ref_dims(&self) -> [(u32, u32); 8] {
        let mut d = [(0u32, 0u32); 8];
        for (i, slot) in self.ref_frames.iter().enumerate() {
            if let Some(rf) = slot {
                d[i] = (rf.w[0] as u32, rf.h[0] as u32);
            }
        }
        d
    }
}

impl Decoder for Vp9Decoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        self.width = params.width;
        self.height = params.height;
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // A VP9 "superframe" packs several coded frames (e.g. a hidden alt-ref
        // plus the shown frame) into one packet, with a trailing index. Split it
        // so each coded frame is decoded in turn. libvpx outputs exactly one
        // frame per packet — the *last* displayable one — so for spatial
        // scalability (multiple shown layers in a superframe) only the top layer
        // is emitted; the lower layers are decoded as references then suppressed.
        let frames = split_superframe(&packet.data);
        let last_disp = frames.iter().rposition(|f| peek_displayable(f));
        for (i, f) in frames.into_iter().enumerate() {
            self.queue.push_back((f, packet.pts, last_disp == Some(i)));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let Some((data, pts, display)) = self.queue.pop_front() else {
            return if self.eof {
                Err(Error::Eof)
            } else {
                Err(Error::Again)
            };
        };
        let mut r = BitReader::new(&data);
        let mut h = parse_uncompressed_header(&mut r, &self.ref_dims())?;

        // show_existing_frame: re-emit a previously decoded reference, no decode.
        if h.show_existing_frame {
            let rf = self.ref_frames[h.frame_to_show as usize]
                .clone()
                .ok_or_else(|| Error::invalid("vp9: show_existing_frame on empty slot"))?;
            if !display {
                return Err(Error::Again);
            }
            return Ok(crop_frame(&rf, self.ss_x, self.ss_y, pts));
        }

        // Key/intra frames carry their own color config; inter frames inherit it.
        if h.key_frame || h.intra_only {
            self.ss_x = h.subsampling_x;
            self.ss_y = h.subsampling_y;
            self.bit_depth = h.bit_depth;
        } else {
            h.subsampling_x = self.ss_x;
            h.subsampling_y = self.ss_y;
            h.bit_depth = self.bit_depth;
        }

        // Resolve the three active references (LAST/GOLDEN/ALTREF).
        let mut active: [Option<std::sync::Arc<decode::RefFrame>>; 3] = [None, None, None];
        if !(h.key_frame || h.intra_only) {
            for i in 0..3 {
                active[i] = self.ref_frames[h.ref_frame_idx[i]].clone();
            }
        }

        // Frame-context management (`setup_past_independence` + load/save).
        let reset_all = h.key_frame || h.intra_only || h.error_resilient;
        if reset_all {
            for c in &mut self.frame_contexts {
                *c = decode::FrameContext::defaults();
            }
        }

        // Segmentation features persist across frames: cleared on a key/intra
        // frame, replaced by an `update_data` signal, otherwise carried over.
        if h.key_frame || h.intra_only {
            self.seg_feature_enabled = [[false; 4]; 8];
            self.seg_feature_data = [[0; 4]; 8];
            self.seg_abs_delta = false;
        }
        if h.seg_update_data {
            self.seg_feature_enabled = h.seg_feature_enabled;
            self.seg_feature_data = h.seg_feature_data;
            self.seg_abs_delta = h.seg_abs_delta;
        }
        h.seg_feature_enabled = self.seg_feature_enabled;
        h.seg_feature_data = self.seg_feature_data;
        h.seg_abs_delta = self.seg_abs_delta;

        // Loop-filter deltas persist the same way: reset to spec defaults on a
        // past-independence frame, otherwise carry over with only the entries
        // this frame flagged `update` changing.
        if reset_all {
            self.lf_ref_deltas = [1, 0, -1, -1];
            self.lf_mode_deltas = [0, 0];
        }
        for i in 0..4 {
            if h.lf_ref_delta_updated[i] {
                self.lf_ref_deltas[i] = h.lf_ref_deltas[i];
            }
        }
        for i in 0..2 {
            if h.lf_mode_delta_updated[i] {
                self.lf_mode_deltas[i] = h.lf_mode_deltas[i];
            }
        }
        h.lf_ref_deltas = self.lf_ref_deltas;
        h.lf_mode_deltas = self.lf_mode_deltas;
        // Key/intra/error-resilient frames are forced onto context 0.
        let ctx_idx = if reset_all {
            0
        } else {
            h.frame_context_idx as usize
        };
        let pre_fc = self.frame_contexts[ctx_idx].clone();

        // Temporal MV prediction is valid only when the previous frame was a
        // same-size, shown, non-intra, non-key inter frame.
        let use_prev_mvs = !h.error_resilient
            && h.width == self.last_width
            && h.height == self.last_height
            && !self.last_intra_only
            && self.last_show_frame
            && !self.last_frame_key;
        let prev_mvs = self.prev_mvs.clone();
        // The previous segment map feeds temporal segment-id prediction, but
        // `setup_past_independence` (key / intra-only / error-resilient frames)
        // clears it — so those frames predict from an all-zero map. It is also
        // only reusable at the same frame size.
        let prev_seg_map = if h.key_frame
            || h.intra_only
            || h.error_resilient
            || h.width != self.last_width
            || h.height != self.last_height
        {
            None
        } else {
            self.prev_seg_map.clone()
        };

        // Safety net for untrusted input: the decode body indexes many
        // bitstream-derived positions, and a malformed stream can drive one out
        // of range. `decode_frame` borrows immutable state and returns a value
        // (it never mutates `self`), so a caught panic leaves the decoder
        // consistent — the frame just fails. The common malformed cases are
        // already rejected as `Err` upstream; this contains the long tail.
        let decode_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decode::decode_frame(
                &h,
                &data,
                &active,
                &pre_fc,
                self.last_frame_key,
                prev_mvs,
                use_prev_mvs,
                prev_seg_map,
            )
        }));
        let (decoded, out_fc, mvs, seg_map) = match decode_res {
            Ok(inner) => inner?,
            Err(_) => return Err(Error::invalid("vp9: decode aborted on malformed input")),
        };
        if h.refresh_frame_context {
            self.frame_contexts[ctx_idx] = out_fc;
        }
        self.prev_mvs = Some(mvs);
        // The segment map persists exactly like libvpx's ping-pong buffers: the
        // swap that promotes this frame's map to the next frame's "previous"
        // (`vp9_swap_current_and_last_seg_map`) only runs when segmentation is
        // enabled. A frame with segmentation *disabled* therefore leaves the
        // last enabled frame's map intact for the next frame's temporal
        // prediction — overwriting it with this frame's all-zero map (as we did
        // before) silently wiped the segmentation across a disabled frame and
        // broke later SEG_LVL_SKIP blocks (the skip-02 conformance vector).
        // Key / intra / error-resilient frames clear the map instead
        // (setup_past_independence).
        if h.seg_enabled {
            self.prev_seg_map = Some(seg_map);
        } else if h.key_frame || h.intra_only || h.error_resilient {
            self.prev_seg_map = None;
        }
        self.last_frame_key = h.key_frame;
        self.last_show_frame = h.show_frame;
        self.last_intra_only = h.intra_only;
        self.last_width = h.width;
        self.last_height = h.height;
        let rf = std::sync::Arc::new(decoded);

        // Update the reference slots selected by refresh_frame_flags.
        let refresh = if h.key_frame {
            0xFF
        } else {
            h.refresh_frame_flags
        };
        for i in 0..8 {
            if refresh & (1 << i) != 0 {
                self.ref_frames[i] = Some(rf.clone());
            }
        }
        self.width = h.width;
        self.height = h.height;

        if !h.show_frame || !display {
            // Decoded but not output: a hidden alt-ref (`!show_frame`), or a
            // lower spatial layer superseded by a later shown frame in the same
            // superframe (`!display`). The references are updated either way;
            // the caller should ask again.
            return Err(Error::Again);
        }
        Ok(crop_frame(&rf, self.ss_x, self.ss_y, pts))
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

/// Peek whether a coded frame would be displayed (`show_frame` or
/// `show_existing_frame`), reading only the leading uncompressed-header bits.
/// Used to pick the single output frame of a superframe. Any parse shortfall
/// defaults to `true` so a malformed frame still reaches the decoder's own
/// error path rather than being silently dropped.
fn peek_displayable(data: &[u8]) -> bool {
    let mut r = BitReader::new(data);
    (|| -> Result<bool> {
        if r.f(2)? != FRAME_MARKER {
            return Ok(true);
        }
        let pl = r.f1()?;
        let ph = r.f1()?;
        if (ph << 1) | pl == 3 {
            r.f1()?; // reserved_zero (profile 3)
        }
        if r.f1()? == 1 {
            return Ok(true); // show_existing_frame
        }
        let _key_frame = r.f1()?;
        Ok(r.f1()? == 1) // show_frame
    })()
    .unwrap_or(true)
}

/// Split a VP9 superframe packet into its component coded frames
/// (libvpx `parse_superframe_index`). Returns `[data]` when not a superframe.
fn split_superframe(data: &[u8]) -> Vec<Vec<u8>> {
    let n = data.len();
    if n >= 2 {
        let marker = data[n - 1];
        if marker & 0xe0 == 0xc0 {
            let frames = (marker & 0x7) as usize + 1;
            let mag = ((marker >> 3) & 0x3) as usize + 1;
            let index_sz = 2 + mag * frames;
            if n >= index_sz && data[n - index_sz] == marker {
                let mut out = Vec::with_capacity(frames);
                let mut off = 0usize;
                let mut x = n - index_sz + 1;
                for _ in 0..frames {
                    let mut sz = 0usize;
                    for j in 0..mag {
                        sz |= (data[x] as usize) << (j * 8);
                        x += 1;
                    }
                    if off + sz <= n {
                        out.push(data[off..off + sz].to_vec());
                    }
                    off += sz;
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }
    vec![data.to_vec()]
}

/// Crop a decoded reference frame to its visible planes and wrap it as a `Frame`.
/// 8-bit planes are emitted as bytes; 10/12-bit planes as little-endian `u16`.
fn crop_frame(rf: &decode::RefFrame, ss_x: u32, ss_y: u32, pts: Option<i64>) -> Frame {
    let hbd = rf.bit_depth > 8;
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(3);
    let mut strides: Vec<usize> = Vec::with_capacity(3);
    for p in 0..3 {
        let (stride, w, hh) = (rf.stride[p], rf.w[p], rf.h[p]);
        let mut v = Vec::with_capacity(w * hh * if hbd { 2 } else { 1 });
        for y in 0..hh {
            let row = &rf.planes[p][y * stride..y * stride + w];
            if hbd {
                for &px in row {
                    v.extend_from_slice(&px.to_le_bytes());
                }
            } else {
                v.extend(row.iter().map(|&px| px as u8));
            }
        }
        planes.push(v);
        strides.push(if hbd { w * 2 } else { w });
    }
    let format = match (ss_x, ss_y, rf.bit_depth) {
        (1, 1, 8) => PixelFormat::Yuv420p,
        (1, 0, 8) => PixelFormat::Yuv422p,
        (_, _, 8) => PixelFormat::Yuv444p,
        (1, 1, 10) => PixelFormat::Yuv420p10,
        (1, 0, 10) => PixelFormat::Yuv422p10,
        (_, _, 10) => PixelFormat::Yuv444p10,
        (1, 1, _) => PixelFormat::Yuv420p12,
        (1, 0, _) => PixelFormat::Yuv422p12,
        (_, _, _) => PixelFormat::Yuv444p12,
    };
    Frame::Video(VideoFrame {
        width: rf.w[0] as u32,
        height: rf.h[0] as u32,
        format,
        planes,
        strides,
        pts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_keyframe_header() {
        let frame = include_bytes!("testdata/keyframe.vp9");
        let mut r = BitReader::new(frame);
        let h = parse_uncompressed_header(&mut r, &[(0, 0); 8]).unwrap();
        assert_eq!(h.profile, 1);
        assert!(h.key_frame);
        assert!(h.show_frame);
        assert_eq!(h.color_space, CS_RGB);
        assert_eq!(h.bit_depth, 8);
        assert_eq!((h.width, h.height), (96, 64));
    }

    /// Robustness fuzz: feed mutated / truncated / random byte streams (seeded
    /// from real coded frames) through the public decode API and assert the
    /// decoder never panics — a malformed stream must surface as `Err`, never a
    /// crash. `VP9_FUZZ_SEEDS` adds `.vp9` seed files; `VP9_FUZZ_ITERS` /
    /// `VP9_FUZZ_SEED` tune the run. Reproduce a crash by re-running with the
    /// printed seed.
    #[test]
    #[ignore]
    fn fuzz_robustness() {
        use rff_core::Packet;
        use std::panic::{catch_unwind, AssertUnwindSafe};
        use std::sync::Mutex;

        let mut seeds: Vec<Vec<u8>> = vec![include_bytes!("testdata/keyframe.vp9").to_vec()];
        if let Ok(dir) = std::env::var("VP9_FUZZ_SEEDS") {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    if e.path().extension().is_some_and(|x| x == "vp9") {
                        if let Ok(d) = std::fs::read(e.path()) {
                            if !d.is_empty() {
                                seeds.push(d);
                            }
                        }
                    }
                }
            }
        }
        let iters: u64 = std::env::var("VP9_FUZZ_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000);
        let mut st: u64 = std::env::var("VP9_FUZZ_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x9e3779b97f4a7c15);
        let mut rng = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };

        static LAST: Mutex<String> = Mutex::new(String::new());
        std::panic::set_hook(Box::new(|info| {
            *LAST.lock().unwrap() = info.to_string();
        }));

        let mutate = |b: &mut Vec<u8>, rng: &mut dyn FnMut() -> u64| {
            let rounds = 1 + rng() % 24;
            for _ in 0..rounds {
                if b.is_empty() {
                    b.push((rng() & 0xff) as u8);
                    continue;
                }
                match rng() % 6 {
                    0 => {
                        let i = rng() as usize % b.len();
                        b[i] ^= 1 << (rng() % 8);
                    }
                    1 => {
                        let i = rng() as usize % b.len();
                        b[i] = (rng() & 0xff) as u8;
                    }
                    2 => {
                        let n = rng() as usize % b.len();
                        b.truncate(n);
                    }
                    3 => {
                        let i = rng() as usize % (b.len() + 1);
                        b.insert(i, (rng() & 0xff) as u8);
                    }
                    4 => {
                        let i = rng() as usize % b.len();
                        b.remove(i);
                    }
                    _ => {
                        let i = rng() as usize % b.len();
                        for _ in 0..(rng() % 8) {
                            if i < b.len() {
                                b[i] = (rng() & 0xff) as u8;
                            }
                        }
                    }
                }
            }
        };

        let mut crashes = 0u64;
        for it in 0..iters {
            // Build a short packet sequence; optionally start from a clean seed so
            // inter / reference-dependent paths are reachable, then mutate.
            let npkts = 1 + rng() % 4;
            let mut packets: Vec<Vec<u8>> = Vec::new();
            for k in 0..npkts {
                if rng() % 16 == 0 {
                    packets.push(
                        (0..(rng() % 4096) as usize)
                            .map(|_| (rng() & 0xff) as u8)
                            .collect(),
                    );
                } else {
                    let mut b = seeds[rng() as usize % seeds.len()].clone();
                    if !(k == 0 && rng() % 4 == 0) {
                        mutate(&mut b, &mut rng);
                    }
                    packets.push(b);
                }
            }
            let snapshot = packets.clone();
            let res = catch_unwind(AssertUnwindSafe(|| {
                let mut dec = Vp9Decoder::default();
                for (i, pk) in packets.into_iter().enumerate() {
                    let mut p = Packet::from_data(0, pk);
                    p.pts = Some(i as i64);
                    let _ = dec.send_packet(&p);
                    for _ in 0..256 {
                        match dec.receive_frame() {
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
                dec.flush();
                for _ in 0..256 {
                    if dec.receive_frame().is_err() {
                        break;
                    }
                }
            }));
            if res.is_err() {
                crashes += 1;
                if crashes <= 12 {
                    let loc = LAST.lock().unwrap().clone();
                    let hexes: Vec<String> = snapshot
                        .iter()
                        .map(|p| {
                            p.iter()
                                .take(48)
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        })
                        .collect();
                    eprintln!("[fuzz] CRASH iter={it}: {loc}\n        packets={hexes:?}");
                }
            }
        }
        let _ = std::panic::take_hook();
        assert_eq!(crashes, 0, "{crashes}/{iters} inputs crashed the decoder");
    }

    /// Decode the real key frame and dump the three planes for bit-exact
    /// Decode a whole inter sequence through the stateful decoder, dumping each
    /// shown frame's planes for comparison against FFmpeg (run with `--ignored`).
    #[test]
    #[ignore]
    fn dump_sequence() {
        use rff_core::Packet;
        let dir = std::env::var("VP9_SEQ_DIR").unwrap();
        let prefix = std::env::var("VP9_SEQ_PREFIX").unwrap_or_else(|_| "seqfp_f".into());
        let n: usize = std::env::var("VP9_SEQ_N")
            .unwrap_or_else(|_| "8".into())
            .parse()
            .unwrap();
        let mut dec = Vp9Decoder::default();
        for i in 0..n {
            let data = std::fs::read(format!("{dir}/{prefix}{i}.vp9")).unwrap();
            let mut p = Packet::from_data(0, data);
            p.pts = Some(i as i64);
            dec.send_packet(&p).unwrap();
        }
        dec.flush();
        let mut idx = 0;
        loop {
            match dec.receive_frame() {
                Ok(Frame::Video(vf)) => {
                    let mut buf = Vec::new();
                    for pl in &vf.planes {
                        buf.extend_from_slice(pl);
                    }
                    std::fs::write(format!("{dir}/my_seq_f{idx}.raw"), &buf).unwrap();
                    idx += 1;
                }
                Err(Error::Again) => continue,
                _ => break,
            }
        }
        eprintln!("decoded {idx} shown frames");
    }

    /// comparison against FFmpeg (run with `--ignored`).
    #[test]
    #[ignore]
    fn dump_keyframe_planes() {
        let embedded = include_bytes!("testdata/keyframe.vp9").to_vec();
        let frame = match std::env::var("VP9_INPUT") {
            Ok(p) => std::fs::read(p).unwrap(),
            Err(_) => embedded,
        };
        let mut r = BitReader::new(&frame);
        let h = parse_uncompressed_header(&mut r, &[(0, 0); 8]).unwrap();
        let (planes, widths, heights) = decode::decode_intra_frame(&h, &frame).unwrap();
        let dir = std::env::var("VP9_DUMP_DIR")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        for (i, p) in planes.iter().enumerate() {
            std::fs::write(format!("{dir}/my_plane{i}.raw"), p).unwrap();
        }
        eprintln!(
            "dims {:?} {:?} loop_filter_level={} -> {dir}",
            widths, heights, h.loop_filter_level
        );
    }

    /// Decode-throughput benchmark. Pre-loads all packets into memory, then times
    /// `passes` full decodes of the sequence (no file I/O in the timed region).
    /// `VP9_BENCH_DIR`/`VP9_BENCH_N` locate `bench_f{i}.vp9`; optional
    /// `VP9_BENCH_REF` (a concatenated i420 reference) enables a correctness check.
    #[test]
    #[ignore]
    fn bench_decode() {
        use rff_core::Packet;
        use std::time::Instant;
        let dir = std::env::var("VP9_BENCH_DIR").unwrap();
        let pre = std::env::var("VP9_BENCH_PREFIX").unwrap_or_else(|_| "bench_f".into());
        let n: usize = std::env::var("VP9_BENCH_N").unwrap().parse().unwrap();
        let passes: usize = std::env::var("VP9_BENCH_PASSES")
            .unwrap_or_else(|_| "5".into())
            .parse()
            .unwrap();
        let packets: Vec<Vec<u8>> = (0..n)
            .map(|i| std::fs::read(format!("{dir}/{pre}{i}.vp9")).unwrap())
            .collect();

        // Optional correctness check + frame geometry from the first pass.
        let mut shown = 0usize;
        let mut pix = 0u64;
        let refdata = std::env::var("VP9_BENCH_REF")
            .ok()
            .map(|p| std::fs::read(p).unwrap());
        {
            let mut dec = Vp9Decoder::default();
            let mut off = 0usize;
            let mut mism = 0usize;
            let drain = |dec: &mut Vp9Decoder,
                         shown: &mut usize,
                         pix: &mut u64,
                         off: &mut usize,
                         mism: &mut usize| loop {
                match dec.receive_frame() {
                    Ok(Frame::Video(vf)) => {
                        *shown += 1;
                        for pl in &vf.planes {
                            *pix += pl.len() as u64;
                        }
                        if let Some(rd) = &refdata {
                            let mut buf = Vec::new();
                            for pl in &vf.planes {
                                buf.extend_from_slice(pl);
                            }
                            if *off + buf.len() <= rd.len() && rd[*off..*off + buf.len()] != buf[..]
                            {
                                *mism += 1;
                            }
                            *off += buf.len();
                        }
                    }
                    Err(Error::Again) => break,
                    _ => break,
                }
            };
            for (i, d) in packets.iter().enumerate() {
                let mut p = Packet::from_data(0, d.clone());
                p.pts = Some(i as i64);
                dec.send_packet(&p).unwrap();
                drain(&mut dec, &mut shown, &mut pix, &mut off, &mut mism);
            }
            dec.flush();
            drain(&mut dec, &mut shown, &mut pix, &mut off, &mut mism);
            if refdata.is_some() {
                eprintln!("[bench] correctness: {} mismatched frames / {shown}", mism);
            }
        }

        // Timed region: `passes` full decodes.
        let t = Instant::now();
        let mut total = 0usize;
        for _ in 0..passes {
            let mut dec = Vp9Decoder::default();
            for (i, d) in packets.iter().enumerate() {
                let mut p = Packet::from_data(0, d.clone());
                p.pts = Some(i as i64);
                dec.send_packet(&p).unwrap();
                while let Ok(Frame::Video(_)) = dec.receive_frame() {
                    total += 1;
                }
            }
            dec.flush();
            while let Ok(Frame::Video(_)) = dec.receive_frame() {
                total += 1;
            }
        }
        let el = t.elapsed();
        let fps = total as f64 / el.as_secs_f64();
        let mpix = (pix as f64 * 2.0 / 3.0) * passes as f64 / 1e6 / el.as_secs_f64();
        eprintln!(
            "[bench] {total} frames in {:.3}s over {passes} passes -> {fps:.1} fps, {mpix:.1} Mpix/s ({shown} frames/pass)",
            el.as_secs_f64()
        );
    }

    /// Throughput scaling across N independent decoder instances (the natural
    /// unit of parallelism for a decoder: a single stream is serially
    /// dependent, but separate streams share no mutable state). `VP9_BENCH_T`
    /// threads each decode the sequence `VP9_BENCH_PASSES` times concurrently.
    #[test]
    #[ignore]
    fn bench_decode_parallel() {
        use rff_core::Packet;
        use std::time::Instant;
        let dir = std::env::var("VP9_BENCH_DIR").unwrap();
        let n: usize = std::env::var("VP9_BENCH_N").unwrap().parse().unwrap();
        let passes: usize = std::env::var("VP9_BENCH_PASSES")
            .unwrap_or_else(|_| "8".into())
            .parse()
            .unwrap();
        let threads: usize = std::env::var("VP9_BENCH_T")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        let packets: Vec<Vec<u8>> = (0..n)
            .map(|i| std::fs::read(format!("{dir}/bench_f{i}.vp9")).unwrap())
            .collect();
        let decode_all = |packets: &[Vec<u8>]| -> usize {
            let mut total = 0;
            for _ in 0..passes {
                let mut dec = Vp9Decoder::default();
                for (i, d) in packets.iter().enumerate() {
                    let mut p = Packet::from_data(0, d.clone());
                    p.pts = Some(i as i64);
                    dec.send_packet(&p).unwrap();
                    while let Ok(Frame::Video(_)) = dec.receive_frame() {
                        total += 1;
                    }
                }
                dec.flush();
                while let Ok(Frame::Video(_)) = dec.receive_frame() {
                    total += 1;
                }
            }
            total
        };
        let t = Instant::now();
        let total: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| s.spawn(|| decode_all(&packets)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        let el = t.elapsed();
        eprintln!(
            "[bench-par] threads={threads} {total} frames in {:.3}s -> {:.0} fps aggregate ({:.0} fps/thread)",
            el.as_secs_f64(), total as f64 / el.as_secs_f64(), total as f64 / el.as_secs_f64() / threads as f64
        );
    }

    #[test]
    fn rejects_bad_marker() {
        let mut r = BitReader::new(&[0x00, 0x00]);
        assert!(parse_uncompressed_header(&mut r, &[(0, 0); 8]).is_err());
    }

    /// Full-header verification on a real 1771-byte libvpx keyframe: the
    /// uncompressed header parses to a sane `header_size`, and structurally
    /// decoding the compressed header consumes ~exactly that many bytes — which
    /// only holds if the whole header + probability-update structure is correct.
    #[test]
    fn full_header_consumes_compressed_header_exactly() {
        let frame = include_bytes!("testdata/keyframe.vp9");
        let mut r = BitReader::new(frame);
        let h = parse_uncompressed_header(&mut r, &[(0, 0); 8]).unwrap();
        assert!(h.sized && h.key_frame);
        assert_eq!((h.width, h.height), (96, 64));
        assert!(h.base_q_idx > 0); // not lossless
        assert!(h.header_size > 0 && (h.header_size as usize) < frame.len());

        let start = h.uncompressed_bytes;
        let end = start + h.header_size as usize;
        let consumed = consume_compressed_header(&frame[start..end], h.lossless).unwrap();
        // The structure lands at the compressed-header end within the boolean
        // decoder's renorm look-ahead (~1 byte). The *exact* consume was
        // confirmed against three real libvpx frames (114/106/103 bytes).
        let hs = h.header_size as usize;
        assert!(
            consumed.div_ceil(8).abs_diff(hs) <= 1,
            "consumed {} bytes vs header_size {hs}",
            consumed.div_ceil(8),
        );
    }
}
