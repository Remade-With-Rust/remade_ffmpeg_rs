//! VP9 encoder — forward probability adaptation (Foundation F5).
//!
//! Backward adaptation is **identical** on both sides of the codec: after a
//! frame, every probability is nudged toward the symbol counts actually coded.
//! The encoder must run the *same* merge as the decoder ([`crate::adapt`]) on
//! the *same* counts, so the two saved frame contexts stay in lock-step. There
//! is no new math here — only the contract that the encoder reuses these
//! primitives verbatim. The per-context orchestration (over a real
//! `FrameContext` + `FrameCounts`) lands with the coding loop in Floor 2/3.

pub(crate) use crate::adapt::{merge_probs, mode_mv_merge_probs, tree_merge_probs};

/// Coefficient adaptation tuning (libvpx `COEF_COUNT_SAT` /
/// `COEF_MAX_UPDATE_FACTOR`) — the saturation + factor the encoder hands to
/// [`merge_probs`] for coefficient probabilities.
pub(crate) const COEF_COUNT_SAT: u32 = 24;
pub(crate) const COEF_MAX_UPDATE_FACTOR: u32 = 112;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_reuses_decoder_adaptation_exactly() {
        // Coefficient adaptation: the encoder's factors must reproduce the
        // decoder's merge (locks the shared contract, anchored to libvpx).
        assert_eq!(
            merge_probs(128, [24, 0], COEF_COUNT_SAT, COEF_MAX_UPDATE_FACTOR),
            184
        );
        // Mode/MV adaptation is the fixed count→factor merge.
        assert_eq!(mode_mv_merge_probs(128, [20, 0]), 192);
        // Tree-structured probabilities merge bottom-up.
        let tree = [0i8, -1];
        let mut probs = [0u8];
        tree_merge_probs(&tree, &[128], &[20, 0], &mut probs);
        assert_eq!(probs[0], 192);
    }
}
