//! VP9 backward probability adaptation — core merge math (ISO/VP9 §9.4 /
//! libvpx `vpx_dsp/prob.{h,c}`).
//!
//! Area-1 primitive 1.1. After a frame is decoded, each probability is nudged
//! toward the empirical distribution of the symbols actually decoded. Two merge
//! rules are used: `merge_probs` (coefficients, with a configurable saturation /
//! update factor) and `mode_mv_merge_probs` (everything else, via a fixed
//! count→factor table). Tree-structured probabilities merge bottom-up.

/// `MODE_MV_COUNT_SAT` — the count at which the mode/mv update factor saturates.
const MODE_MV_COUNT_SAT: u32 = 20;

/// `count_to_update_factor[MODE_MV_COUNT_SAT + 1]` = 128 * count / 20.
const COUNT_TO_UPDATE_FACTOR: [u32; 21] = [
    0, 6, 12, 19, 25, 32, 38, 44, 51, 57, 64, 70, 76, 83, 89, 96, 102, 108, 115, 121, 128,
];

#[inline]
fn round_pow2(v: u32, n: u32) -> u32 {
    (v + (1 << (n - 1))) >> n
}

/// `get_prob(num, den)` — `num/den` as a 1..=255 probability (rounded).
#[inline]
pub fn get_prob(num: u32, den: u32) -> u8 {
    debug_assert!(den != 0);
    let p = ((num as u64 * 256 + (den as u64 / 2)) / den as u64) as i32;
    p.clamp(1, 255) as u8
}

/// `get_binary_prob(n0, n1)` — probability of the `n0` branch (128 if no counts).
#[inline]
pub fn get_binary_prob(n0: u32, n1: u32) -> u8 {
    let den = n0 + n1;
    if den == 0 {
        128
    } else {
        get_prob(n0, den)
    }
}

/// `weighted_prob(p1, p2, factor)` — blend two probs by an 8-bit factor.
#[inline]
pub fn weighted_prob(p1: u8, p2: u8, factor: u32) -> u8 {
    round_pow2(p1 as u32 * (256 - factor) + p2 as u32 * factor, 8) as u8
}

/// `merge_probs` — coefficient adaptation (configurable saturation / factor).
#[inline]
pub fn merge_probs(pre: u8, ct: [u32; 2], count_sat: u32, max_update_factor: u32) -> u8 {
    let prob = get_binary_prob(ct[0], ct[1]);
    let count = (ct[0] + ct[1]).min(count_sat);
    let factor = max_update_factor * count / count_sat;
    weighted_prob(pre, prob, factor)
}

/// `mode_mv_merge_probs` — mode / motion-vector adaptation.
#[inline]
pub fn mode_mv_merge_probs(pre: u8, ct: [u32; 2]) -> u8 {
    let den = ct[0] + ct[1];
    if den == 0 {
        pre
    } else {
        let count = den.min(MODE_MV_COUNT_SAT);
        let factor = COUNT_TO_UPDATE_FACTOR[count as usize];
        let prob = get_prob(ct[0], den);
        weighted_prob(pre, prob, factor)
    }
}

/// `vpx_tree_merge_probs` — merge a tree-structured probability set bottom-up.
/// `tree` holds the node layout (leaves are non-positive `-symbol`), `counts`
/// holds per-symbol counts, `pre` the prior probs; writes the merged `probs`.
pub fn tree_merge_probs(tree: &[i8], pre: &[u8], counts: &[u32], probs: &mut [u8]) {
    fn rec(i: usize, tree: &[i8], pre: &[u8], counts: &[u32], probs: &mut [u8]) -> u32 {
        let l = tree[i];
        let left = if l <= 0 {
            counts[(-l) as usize]
        } else {
            rec(l as usize, tree, pre, counts, probs)
        };
        let r = tree[i + 1];
        let right = if r <= 0 {
            counts[(-r) as usize]
        } else {
            rec(r as usize, tree, pre, counts, probs)
        };
        probs[i >> 1] = mode_mv_merge_probs(pre[i >> 1], [left, right]);
        left + right
    }
    rec(0, tree, pre, counts, probs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_prob_rounds_and_clamps() {
        assert_eq!(get_prob(10, 20), 128); // (2560+10)/20 = 128
        assert_eq!(get_prob(20, 20), 255); // 256 -> clamped to 255
        assert_eq!(get_prob(0, 20), 1); // 0 -> clamped to 1
        assert_eq!(get_binary_prob(0, 0), 128);
    }

    #[test]
    fn weighted_prob_blends() {
        // factor 0 keeps p1; factor 256 takes p2.
        assert_eq!(weighted_prob(100, 200, 0), 100);
        assert_eq!(weighted_prob(100, 200, 256), 200);
        assert_eq!(weighted_prob(100, 200, 64), 125); // (100*192+200*64+128)>>8
    }

    #[test]
    fn mode_mv_merge_matches_libvpx() {
        assert_eq!(mode_mv_merge_probs(128, [0, 0]), 128); // no counts -> unchanged
        // den=20 -> factor 128, prob=get_prob(20,20)=255 -> weighted(128,255,128)=192
        assert_eq!(mode_mv_merge_probs(128, [20, 0]), 192);
        // equal counts -> prob 128 -> stays 128
        assert_eq!(mode_mv_merge_probs(128, [10, 10]), 128);
    }

    #[test]
    fn merge_probs_coef() {
        // get_binary_prob(24,0)=255; count=min(24,24)=24; factor=112*24/24=112.
        // weighted(128,255,112) = (128*144 + 255*112 + 128)>>8 = 184.
        assert_eq!(merge_probs(128, [24, 0], 24, 112), 184);
        // zero counts: prob=128, factor=0 -> stays pre.
        assert_eq!(merge_probs(200, [0, 0], 24, 112), 200);
    }

    #[test]
    fn tree_merge_two_leaf() {
        // A 2-leaf tree {-0, -1}: probs[0] = mode_mv_merge(pre[0], [c0, c1]).
        let tree = [0i8, -1];
        let pre = [128u8];
        let counts = [20u32, 0];
        let mut probs = [0u8];
        tree_merge_probs(&tree, &pre, &counts, &mut probs);
        assert_eq!(probs[0], 192);
    }
}
