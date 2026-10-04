//! Parametric Stereo (HE-AAC v2) — placeholder until the PS brick lands.

use crate::bits::BitReader;
use crate::Result;

type Cpx = [f32; 2];

#[derive(Clone, Default)]
pub(crate) struct PsState {
    start: bool,
}

impl PsState {
    pub(crate) fn new() -> PsState {
        PsState::default()
    }

    pub(crate) fn started(&self) -> bool {
        self.start
    }

    /// Consume `bits_left` bits of PS data; returns the bits used.
    pub(crate) fn read_data(&mut self, r: &mut BitReader, bits_left: usize) -> Result<usize> {
        r.skip(bits_left)?;
        Ok(bits_left)
    }

    pub(crate) fn apply(&mut self, _l: &mut [[Cpx; 64]], _r: &mut [[Cpx; 64]], _top: usize) {}
}
