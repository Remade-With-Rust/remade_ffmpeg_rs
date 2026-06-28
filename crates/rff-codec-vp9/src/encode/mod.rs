//! VP9 encoder — the inverse house (plan: `docs/vp9-encoder-plan.md`).
//!
//! **Foundation layer (F1–F5).** The bitstream writer core plus the forward
//! probability machinery — each brick the exact inverse of a decoder reader and
//! gated by a round-trip *through that reader*. Nothing here fabricates a table:
//! every codebook (probabilities, scans, quant steps, filters) is reused from
//! the decoder, already validated by 315/315 conformance. Floors 1+ (forward
//! transforms, coefficient/mode coding, the control brain) build on this.
//!
//! The encoder is not yet registered as a [`Codec`](rff_codec::Codec) encoder —
//! that wiring lands with the first decodable key frame (plan Floor 3, C3).

mod adapt;
mod bitwriter;
mod compressed;
mod frame;
mod header;
mod intermode;
mod mv;
mod prob;
mod quantize;
mod syntax;
mod tokens;
mod transform;

pub(crate) use bitwriter::{BitWriter, BoolEncoder};
pub(crate) use compressed::write_compressed_header;
pub(crate) use frame::{assemble_frame, assemble_tiles};
pub(crate) use header::write_uncompressed_header;
pub(crate) use intermode::{
    write_comp_inter, write_comp_ref, write_inter_mode, write_interp_filter, write_is_inter,
    write_single_ref,
};
pub(crate) use mv::encode_mv;
pub(crate) use prob::{
    diff_update_encode, encode_term_subexp, forward_remap_prob, update_mv_prob_encode,
};
pub(crate) use quantize::quantize;
pub(crate) use syntax::{
    write_intra_mode, write_partition, write_segment_id, write_selected_tx_size, write_skip,
};
pub(crate) use tokens::{coef_cost, encode_coefs};
pub(crate) use transform::forward_transform;

#[cfg(test)]
mod tests {
    //! F1 — confirm the decoder codebooks the encoder reuses are reachable from
    //! the `encode` module (no re-entry, no fabrication).

    #[test]
    fn reused_codebooks_are_reachable() {
        // Coefficient model probabilities (anchor from libvpx).
        assert_eq!(
            crate::prob_tables::DEFAULT_COEF_PROBS[0][0][0][0][0],
            [195, 29, 183]
        );
        // Dequant steps (the encoder divides by the same table it inverts).
        assert!(crate::quant::dc_quant(0, 8) > 0);
        assert!(crate::quant::ac_quant(255, 8) > 0);
        // Coefficient scan order.
        assert_eq!(crate::scan_tables::DEFAULT_SCAN_4X4.len(), 16);
    }
}
