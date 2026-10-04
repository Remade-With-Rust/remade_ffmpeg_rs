//! SBR / PS reconstruction stage (filled in by the HE-AAC bricks).

use crate::bits::BitReader;
use crate::decode::{Decoder, Element};
use crate::Result;

/// Per-element SBR state.
pub(crate) struct SbrChannelState;

impl Clone for SbrChannelState {
    fn clone(&self) -> Self {
        SbrChannelState
    }
}

/// Decoder-wide SBR context.
pub(crate) struct SbrDecoderCtx {
    dual: bool,
}

impl SbrDecoderCtx {
    pub(crate) fn dual_rate(&self) -> bool {
        self.dual
    }
}

/// Apply SBR (and PS) to an element after synthesis.
pub(crate) fn apply(dec: &mut Decoder, el: &mut Element, ty: u8, n: usize) {
    if ty == crate::decode::layout::TYPE_SCE && dec.ps_on() {
        let (a, b) = el.ch.split_at_mut(1);
        b[0].output[..n].copy_from_slice(&a[0].output[..n]);
    }
}

/// ELD low-delay SBR payload.
pub(crate) fn decode_eld_sbr(_dec: &mut Decoder, _r: &mut BitReader) -> Result<()> {
    Err(crate::Error::unsupported("aac: ELD low-delay SBR not yet supported"))
}
