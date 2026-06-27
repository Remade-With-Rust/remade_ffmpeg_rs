//! VP9 probability model. The default tables live in `prob_tables.rs`
//! (generated from libvpx and validated on extraction); this module holds the
//! algorithms that consume them:
//!
//! * [`inv_remap_prob`] — applies a decoded sub-exp delta to a probability,
//!   used for every compressed-header update (ISO/VP9 §9.3.2; libvpx
//!   `inv_remap_prob`), including the fixed `INV_MAP_TABLE`.
//! * [`model_to_full`] — expands the 3 stored "model" coefficient probabilities
//!   to the full 11-node token-tree probabilities via `PARETO8_FULL`
//!   (libvpx `vp9_model_to_full_probs`).

#![allow(dead_code)]

pub(crate) use crate::prob_tables::*;

const MAX_PROB: i32 = 255;

fn inv_recenter_nonneg(v: i32, m: i32) -> i32 {
    if v > 2 * m {
        v
    } else if v & 1 != 0 {
        m - ((v + 1) >> 1)
    } else {
        m + (v >> 1)
    }
}

/// Apply a decoded term sub-exp delta `v` (0..=254) to the current probability
/// `m` (1..=255), returning the updated probability. Mirrors libvpx
/// `inv_remap_prob` exactly, including the `INV_MAP_TABLE` indirection.
pub fn inv_remap_prob(v: i32, m: i32) -> u8 {
    let v = INV_MAP_TABLE[v as usize] as i32;
    let m = m - 1;
    if (m << 1) <= MAX_PROB {
        (1 + inv_recenter_nonneg(v, m)) as u8
    } else {
        (MAX_PROB - inv_recenter_nonneg(v, MAX_PROB - 1 - m)) as u8
    }
}

/// Number of internal nodes in the VP9 coefficient token tree.
pub const ENTROPY_NODES: usize = 11;
/// Pivot model node whose probability selects the Pareto tail.
const PIVOT_NODE: usize = 2;

/// Expand a 3-node coefficient *model* probability set to the full
/// 11-node token-tree probabilities: nodes 0..3 are copied from the model,
/// nodes 3..11 come from `PARETO8_FULL[model[PIVOT_NODE] - 1]`.
pub fn model_to_full(model: &[u8; 3]) -> [u8; ENTROPY_NODES] {
    let mut full = [0u8; ENTROPY_NODES];
    full[..3].copy_from_slice(model);
    full[3..].copy_from_slice(&PARETO8_FULL[model[PIVOT_NODE] as usize - 1]);
    full
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_map_table_is_a_near_permutation() {
        assert_eq!(INV_MAP_TABLE.len(), 255);
        // Anchor values verified against libvpx vp9_dsubexp.c.
        assert_eq!(INV_MAP_TABLE[0], 7);
        assert_eq!(INV_MAP_TABLE[19], 254);
        assert_eq!(INV_MAP_TABLE[20], 1);
        assert_eq!(INV_MAP_TABLE[254], 253);
        assert!(INV_MAP_TABLE.iter().all(|&v| (1..=254).contains(&v)));
    }

    #[test]
    fn inv_remap_always_yields_valid_prob() {
        // For every (delta, prob) the result must stay a legal prob (1..=255).
        for v in 0..255i32 {
            for m in 1..=255i32 {
                let p = inv_remap_prob(v, m);
                assert!((1..=255).contains(&p), "v={v} m={m} -> {p}");
            }
        }
        // Spot value verified against libvpx: INV_MAP_TABLE[20]=1, m'=99,
        // 1 + inv_recenter_nonneg(1, 99) = 1 + 98 = 99.
        assert_eq!(inv_remap_prob(20, 100), 99);
    }

    #[test]
    fn pareto_and_model_expansion() {
        assert_eq!(PARETO8_FULL.len(), 255);
        assert!(PARETO8_FULL.iter().flatten().all(|&v| v >= 1));
        // Anchor row (p=128) verified against libvpx vp9_pareto8_full.
        assert_eq!(PARETO8_FULL[127], [213, 145, 170, 238, 173, 255, 237, 252]);
        // model_to_full copies the 3 model nodes, then appends the Pareto tail.
        let model = [10u8, 50, 128];
        let full = model_to_full(&model);
        assert_eq!(&full[..3], &model);
        assert_eq!(&full[3..], &PARETO8_FULL[127]);
    }

    #[test]
    fn default_tables_well_formed() {
        // Coef-probs anchor (4x4, plane0/ref0/band0/ctx0) from libvpx.
        assert_eq!(DEFAULT_COEF_PROBS[0][0][0][0][0], [195, 29, 183]);
        // Every *real* coef prob (band 0 uses 3 contexts; 3..6 are padding) is valid.
        for tx in &DEFAULT_COEF_PROBS {
            for plane in tx {
                for refr in plane {
                    for (band, ctxs) in refr.iter().enumerate() {
                        let nctx = if band == 0 { 3 } else { 6 };
                        for ctx in &ctxs[..nctx] {
                            assert!(ctx.iter().all(|&p| (1..=255).contains(&p)));
                        }
                    }
                }
            }
        }
        // Mode / partition / skip anchors verified against libvpx.
        assert_eq!(KF_UV_MODE_PROBS[0], [144, 11, 54, 157, 195, 130, 46, 58, 108]);
        assert_eq!(KF_PARTITION_PROBS[0], [158, 97, 94]);
        assert_eq!(DEFAULT_SKIP_PROB, [192, 128, 64]);
        assert!(KF_Y_MODE_PROBS.iter().flatten().flatten().all(|&p| (1..=255).contains(&p)));
    }
}
