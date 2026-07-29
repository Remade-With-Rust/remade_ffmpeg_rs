//! Forward alias reduction (encode) — the inverse of `decode/antialias.rs`.
//!
//! The decoder applies alias-cancellation butterflies (an orthogonal rotation per
//! subband boundary) to the requantized spectrum *before* the IMDCT. The encoder
//! must therefore apply the **inverse** rotation after the forward MDCT, so that
//! `decode::reduce(expand(x)) == x` and the stored spectrum survives the
//! round-trip. Same ISO Table B.9 coefficients as the decoder; the butterfly pairs
//! are disjoint, so order is irrelevant.

use std::sync::OnceLock;

use crate::frame::{BlockType, GranuleSideInfo, GRANULE_LINES};

/// The eight alias-reduction `ci` coefficients (ISO 11172-3 Table B.9) — the same
/// values the decoder uses.
const CI: [f32; 8] = [
    -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037,
];

/// `(cs, ca)` butterfly weights, identical to the decoder's.
fn weights() -> &'static ([f32; 8], [f32; 8]) {
    static T: OnceLock<([f32; 8], [f32; 8])> = OnceLock::new();
    T.get_or_init(|| {
        let mut cs = [0f32; 8];
        let mut ca = [0f32; 8];
        for i in 0..8 {
            let d = (1.0 + CI[i] * CI[i]).sqrt();
            cs[i] = 1.0 / d;
            ca[i] = CI[i] / d;
        }
        (cs, ca)
    })
}

/// Apply the forward (encode-side) alias butterflies in place — the inverse
/// rotation of `decode::antialias::reduce`.
pub fn expand(gi: &GranuleSideInfo, lines: &mut [f32; GRANULE_LINES]) {
    let is_short = gi.window_switching && gi.block_type == BlockType::Short;
    let boundaries = if is_short {
        if gi.mixed_block {
            1
        } else {
            0
        }
    } else {
        31
    };
    let (cs, ca) = weights();
    for sb in 1..=boundaries {
        let base = sb * 18;
        for i in 0..8 {
            let lower = base - 1 - i;
            let upper = base + i;
            let a = lines[lower];
            let b = lines[upper];
            // Inverse of decode's [[cs,-ca],[ca,cs]] rotation (its transpose).
            lines[lower] = a * cs[i] + b * ca[i];
            lines[upper] = -a * ca[i] + b * cs[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::antialias::reduce;

    #[test]
    fn expand_then_reduce_is_identity() {
        let mut lines = [0f32; GRANULE_LINES];
        for i in 0..GRANULE_LINES {
            lines[i] = ((i % 17) as f32 - 8.0) * 0.1;
        }
        let original = lines;
        expand(&GranuleSideInfo::default(), &mut lines); // long block
        reduce(&GranuleSideInfo::default(), &mut lines);
        for i in 0..GRANULE_LINES {
            assert!(
                (lines[i] - original[i]).abs() < 1e-5,
                "line {i}: {} vs {}",
                lines[i],
                original[i]
            );
        }
    }
}
