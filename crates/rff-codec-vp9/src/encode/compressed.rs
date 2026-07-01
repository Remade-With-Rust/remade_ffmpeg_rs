//! VP9 encoder — compressed frame-header serializer (Floor 2, brick B6).
//!
//! [`write_compressed_header`] is the exact inverse of
//! [`parse_compressed_header`](crate::decode::parse_compressed_header): given the
//! prior frame context (`pre`) and the encoder's chosen `target` context, it
//! writes `tx_mode`, the coefficient-probability updates, and (for inter frames)
//! the mode/reference/partition/MV probability updates — each through F4's
//! `diff_update_encode` / `update_mv_prob_encode`. Gated by a
//! `serialize → parse == target` round-trip.

use super::bitwriter::BoolEncoder;
use super::prob::{diff_update_encode, update_mv_prob_encode};
use crate::block::TX_MODE_SELECT;
use crate::decode::FrameContext;
use crate::prob_tables::NmvContext;
use crate::FrameHeader;

/// `tx_mode_to_biggest_tx_size` — the largest coded TX size per TX mode.
const TX_MODE_TO_BIGGEST_TX: [usize; 5] = [0, 1, 2, 3, 3];

fn diff_update_slice(enc: &mut BoolEncoder, pre: &[u8], target: &[u8]) {
    for (p, t) in pre.iter().zip(target) {
        diff_update_encode(enc, *p, *t);
    }
}

fn update_mv_slice(enc: &mut BoolEncoder, pre: &[u8], target: &[u8]) {
    for (p, t) in pre.iter().zip(target) {
        update_mv_prob_encode(enc, *p, *t);
    }
}

fn write_tx_mode(enc: &mut BoolEncoder, tx_mode: usize, lossless: bool) {
    if lossless {
        return; // tx_mode forced to ONLY_4X4
    }
    enc.write_literal(tx_mode.min(3) as u32, 2);
    if tx_mode >= 3 {
        enc.write_literal((tx_mode - 3) as u32, 1);
    }
}

fn write_tx_mode_probs(enc: &mut BoolEncoder, pre: &FrameContext, t: &FrameContext) {
    for i in 0..2 {
        diff_update_slice(enc, &pre.tx_p8x8[i], &t.tx_p8x8[i]);
    }
    for i in 0..2 {
        diff_update_slice(enc, &pre.tx_p16x16[i], &t.tx_p16x16[i]);
    }
    for i in 0..2 {
        diff_update_slice(enc, &pre.tx_p32x32[i], &t.tx_p32x32[i]);
    }
}

/// Whether any *coded* coefficient probability in TX size `tx` changed.
fn coef_tx_changed(pre: &FrameContext, t: &FrameContext, tx: usize) -> bool {
    for i in 0..2 {
        for j in 0..2 {
            for k in 0..6 {
                let nctx = if k == 0 { 3 } else { 6 };
                for l in 0..nctx {
                    for m in 0..3 {
                        if pre.coef_probs[tx][i][j][k][l][m] != t.coef_probs[tx][i][j][k][l][m] {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn write_coef_probs(enc: &mut BoolEncoder, pre: &FrameContext, t: &FrameContext) {
    let max_tx = TX_MODE_TO_BIGGEST_TX[t.tx_mode];
    for tx in 0..=max_tx {
        let changed = coef_tx_changed(pre, t, tx);
        enc.write_bool(changed as u32, 128); // update-present bit
        if changed {
            for i in 0..2 {
                for j in 0..2 {
                    for k in 0..6 {
                        let nctx = if k == 0 { 3 } else { 6 };
                        for l in 0..nctx {
                            for m in 0..3 {
                                diff_update_encode(
                                    enc,
                                    pre.coef_probs[tx][i][j][k][l][m],
                                    t.coef_probs[tx][i][j][k][l][m],
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn write_mv_probs(enc: &mut BoolEncoder, pre: &NmvContext, t: &NmvContext, allow_hp: bool) {
    update_mv_slice(enc, &pre.joints, &t.joints);
    for c in 0..2 {
        update_mv_prob_encode(enc, pre.comps[c].sign, t.comps[c].sign);
        update_mv_slice(enc, &pre.comps[c].classes, &t.comps[c].classes);
        update_mv_slice(enc, &pre.comps[c].class0, &t.comps[c].class0);
        update_mv_slice(enc, &pre.comps[c].bits, &t.comps[c].bits);
    }
    for c in 0..2 {
        for j in 0..2 {
            update_mv_slice(enc, &pre.comps[c].class0_fp[j], &t.comps[c].class0_fp[j]);
        }
        update_mv_slice(enc, &pre.comps[c].fp, &t.comps[c].fp);
    }
    if allow_hp {
        for c in 0..2 {
            update_mv_prob_encode(enc, pre.comps[c].class0_hp, t.comps[c].class0_hp);
            update_mv_prob_encode(enc, pre.comps[c].hp, t.comps[c].hp);
        }
    }
}

/// Serialize the compressed header — the inverse of
/// [`parse_compressed_header`](crate::decode::parse_compressed_header). `pre` is
/// the loaded frame context; `t` (target) is the encoder's adapted context.
pub fn write_compressed_header(
    enc: &mut BoolEncoder,
    pre: &FrameContext,
    t: &FrameContext,
    h: &FrameHeader,
) {
    write_tx_mode(enc, t.tx_mode, h.lossless);
    if t.tx_mode == TX_MODE_SELECT {
        write_tx_mode_probs(enc, pre, t);
    }
    write_coef_probs(enc, pre, t);
    for k in 0..3 {
        diff_update_encode(enc, pre.skip_probs[k], t.skip_probs[k]);
    }

    let intra_only = h.key_frame || h.intra_only;
    if intra_only {
        return;
    }

    for i in 0..7 {
        diff_update_slice(enc, &pre.inter_mode_probs[i], &t.inter_mode_probs[i]);
    }
    if h.interp_filter == 4 {
        for j in 0..4 {
            diff_update_slice(
                enc,
                &pre.switchable_interp_prob[j],
                &t.switchable_interp_prob[j],
            );
        }
    }
    diff_update_slice(enc, &pre.intra_inter_prob, &t.intra_inter_prob);

    // Reference mode (single / compound / select).
    let compound_allowed =
        h.ref_sign_bias[1] != h.ref_sign_bias[0] || h.ref_sign_bias[2] != h.ref_sign_bias[0];
    if compound_allowed {
        if t.reference_mode == 0 {
            enc.write_bool(0, 128);
        } else {
            enc.write_bool(1, 128);
            enc.write_bool((t.reference_mode == 2) as u32, 128);
        }
    }
    if t.reference_mode == 2 {
        diff_update_slice(enc, &pre.comp_inter_prob, &t.comp_inter_prob);
    }
    if t.reference_mode != 1 {
        for i in 0..5 {
            diff_update_encode(enc, pre.single_ref_prob[i][0], t.single_ref_prob[i][0]);
            diff_update_encode(enc, pre.single_ref_prob[i][1], t.single_ref_prob[i][1]);
        }
    }
    if t.reference_mode != 0 {
        diff_update_slice(enc, &pre.comp_ref_prob, &t.comp_ref_prob);
    }

    for j in 0..4 {
        diff_update_slice(enc, &pre.y_mode_prob[j], &t.y_mode_prob[j]);
    }
    for j in 0..16 {
        diff_update_slice(enc, &pre.partition_prob[j], &t.partition_prob[j]);
    }
    write_mv_probs(enc, &pre.nmvc, &t.nmvc, h.allow_high_precision_mv);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::parse_compressed_header;
    use crate::prob::inv_remap_prob;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// A reachable probability perturbation (~18% of the time): the encoder can
    /// only reach probs of the form `inv_remap_prob(delta, pre)`, so perturbing
    /// to one guarantees the round-trip lands exactly.
    fn pert(s: &mut u64, p: u8) -> u8 {
        if xs(s) % 100 < 18 {
            inv_remap_prob((xs(s) % 255) as i32, p as i32)
        } else {
            p
        }
    }
    fn pert_slice(s: &mut u64, pre: &[u8], out: &mut [u8]) {
        for (i, &p) in pre.iter().enumerate() {
            out[i] = pert(s, p);
        }
    }
    /// MV probs are always odd (`(literal<<1)|1`).
    fn pert_mv(s: &mut u64, p: u8) -> u8 {
        if xs(s) % 100 < 25 {
            ((xs(s) % 128) << 1 | 1) as u8
        } else {
            p
        }
    }
    fn pert_mv_slice(s: &mut u64, pre: &[u8], out: &mut [u8]) {
        for (i, &p) in pre.iter().enumerate() {
            out[i] = pert_mv(s, p);
        }
    }

    fn perturb_coefs(s: &mut u64, pre: &FrameContext, t: &mut FrameContext, max_tx: usize) {
        for tx in 0..=max_tx {
            for i in 0..2 {
                for j in 0..2 {
                    for k in 0..6 {
                        let nctx = if k == 0 { 3 } else { 6 };
                        for l in 0..nctx {
                            for m in 0..3 {
                                t.coef_probs[tx][i][j][k][l][m] =
                                    pert(s, pre.coef_probs[tx][i][j][k][l][m]);
                            }
                        }
                    }
                }
            }
        }
    }

    fn perturb_mv(s: &mut u64, pre: &NmvContext, t: &mut NmvContext) {
        pert_mv_slice(s, &pre.joints, &mut t.joints);
        for c in 0..2 {
            t.comps[c].sign = pert_mv(s, pre.comps[c].sign);
            pert_mv_slice(s, &pre.comps[c].classes, &mut t.comps[c].classes);
            pert_mv_slice(s, &pre.comps[c].class0, &mut t.comps[c].class0);
            pert_mv_slice(s, &pre.comps[c].bits, &mut t.comps[c].bits);
            for j in 0..2 {
                let pf = pre.comps[c].class0_fp[j];
                pert_mv_slice(s, &pf, &mut t.comps[c].class0_fp[j]);
            }
            pert_mv_slice(s, &pre.comps[c].fp, &mut t.comps[c].fp);
            t.comps[c].class0_hp = pert_mv(s, pre.comps[c].class0_hp);
            t.comps[c].hp = pert_mv(s, pre.comps[c].hp);
        }
    }

    fn roundtrip(pre: &FrameContext, t: &FrameContext, h: &FrameHeader) {
        let mut enc = BoolEncoder::new();
        write_compressed_header(&mut enc, pre, t, h);
        let bytes = enc.finish();
        let parsed = parse_compressed_header(&bytes, h, pre).unwrap();
        assert!(&parsed == t, "compressed header round-trip mismatch");
    }

    #[test]
    fn keyframe_compressed_header_roundtrips() {
        let mut s = 0xc0de_0001_0002_0003u64;
        let pre = FrameContext::defaults();
        for tx_mode in 0..=4usize {
            for _ in 0..40 {
                let mut t = pre.clone();
                t.tx_mode = tx_mode;
                perturb_coefs(&mut s, &pre, &mut t, TX_MODE_TO_BIGGEST_TX[tx_mode]);
                pert_slice(&mut s, &pre.skip_probs.to_vec(), &mut t.skip_probs);
                if tx_mode == TX_MODE_SELECT {
                    for i in 0..2 {
                        let p = pre.tx_p8x8[i];
                        pert_slice(&mut s, &p, &mut t.tx_p8x8[i]);
                        let p = pre.tx_p16x16[i];
                        pert_slice(&mut s, &p, &mut t.tx_p16x16[i]);
                        let p = pre.tx_p32x32[i];
                        pert_slice(&mut s, &p, &mut t.tx_p32x32[i]);
                    }
                }
                let h = FrameHeader {
                    key_frame: true,
                    lossless: false,
                    ..Default::default()
                };
                roundtrip(&pre, &t, &h);
            }
        }
    }

    #[test]
    fn inter_compressed_header_roundtrips() {
        let mut s = 0xbead_0009_0008_0007u64;
        let pre = FrameContext::defaults();
        // sign_bias=[false,true,false] → compound allowed; setup_compound_reference_mode
        // resolves comp_fixed_ref=2, comp_var_ref=[1,3].
        let sign_bias = [false, true, false];
        for ref_mode in 0..=2usize {
            for _ in 0..40 {
                let mut t = pre.clone();
                t.tx_mode = 4; // SELECT — exercise tx + all coef tx sizes
                perturb_coefs(&mut s, &pre, &mut t, 3);
                pert_slice(&mut s, &pre.skip_probs.to_vec(), &mut t.skip_probs);
                for i in 0..2 {
                    let p = pre.tx_p8x8[i];
                    pert_slice(&mut s, &p, &mut t.tx_p8x8[i]);
                    let p = pre.tx_p16x16[i];
                    pert_slice(&mut s, &p, &mut t.tx_p16x16[i]);
                    let p = pre.tx_p32x32[i];
                    pert_slice(&mut s, &p, &mut t.tx_p32x32[i]);
                }
                for i in 0..7 {
                    let p = pre.inter_mode_probs[i];
                    pert_slice(&mut s, &p, &mut t.inter_mode_probs[i]);
                }
                for j in 0..4 {
                    let p = pre.switchable_interp_prob[j];
                    pert_slice(&mut s, &p, &mut t.switchable_interp_prob[j]);
                }
                pert_slice(
                    &mut s,
                    &pre.intra_inter_prob.to_vec(),
                    &mut t.intra_inter_prob,
                );
                t.reference_mode = ref_mode;
                if ref_mode != 0 {
                    // setup_compound_reference_mode for this sign bias.
                    t.comp_fixed_ref = 2;
                    t.comp_var_ref = [1, 3];
                }
                if ref_mode == 2 {
                    pert_slice(
                        &mut s,
                        &pre.comp_inter_prob.to_vec(),
                        &mut t.comp_inter_prob,
                    );
                }
                if ref_mode != 1 {
                    for i in 0..5 {
                        let p = pre.single_ref_prob[i];
                        pert_slice(&mut s, &p, &mut t.single_ref_prob[i]);
                    }
                }
                if ref_mode != 0 {
                    pert_slice(&mut s, &pre.comp_ref_prob.to_vec(), &mut t.comp_ref_prob);
                }
                for j in 0..4 {
                    let p = pre.y_mode_prob[j];
                    pert_slice(&mut s, &p, &mut t.y_mode_prob[j]);
                }
                for j in 0..16 {
                    let p = pre.partition_prob[j];
                    pert_slice(&mut s, &p, &mut t.partition_prob[j]);
                }
                perturb_mv(&mut s, &pre.nmvc, &mut t.nmvc);
                let h = FrameHeader {
                    key_frame: false,
                    intra_only: false,
                    lossless: false,
                    interp_filter: 4,
                    allow_high_precision_mv: true,
                    ref_sign_bias: sign_bias,
                    ..Default::default()
                };
                roundtrip(&pre, &t, &h);
            }
        }
    }
}
