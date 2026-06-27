//! Alias reduction.
//!
//! The hybrid filterbank introduces aliasing between adjacent subbands. Eight
//! butterfly operations per subband boundary (using the `cs`/`ca` coefficients
//! from ISO Table B.9) cancel it. Applied to long blocks and the long part of
//! mixed blocks only — pure short blocks skip it.

use crate::frame::{GranuleSideInfo, GRANULE_LINES};

/// Apply the cross-subband alias-cancellation butterflies in place.
pub fn reduce(_gi: &GranuleSideInfo, _lines: &mut [f32; GRANULE_LINES]) {
    // brick: for each of the 31 subband boundaries (or fewer for mixed blocks),
    // run the 8 butterflies with cs[i]/ca[i]. Skip entirely for short blocks.
    todo!("mp3 decode: alias reduction")
}
