//! VP9 encoder — motion-vector coding (Floor 2, brick B4).
//!
//! [`encode_mv`] is the exact inverse of [`read_mv`](crate::mv::read_mv): it
//! decomposes an MV difference into joint type and, per present component, the
//! sign / class / integer bits / fractional / high-precision symbols, writing
//! them through the same MV trees the decoder reads and accumulating the
//! identical `NmvCounts`. A round-trip through `read_mv` recovers the exact MV
//! and counts.

use super::bitwriter::BoolEncoder;
use crate::mv::{NmvCounts, MV_CLASS_TREE, MV_FP_TREE, MV_JOINT_TREE};
use crate::prob_tables::{NmvComp, NmvContext};

/// High precision is used only when the reference MV is small (`use_mv_hp`,
/// replicated from the decoder — it must agree exactly).
#[inline]
fn use_mv_hp(ref_mv: (i32, i32)) -> bool {
    ref_mv.0.abs() < 64 && ref_mv.1.abs() < 64
}

/// Encode one MV component difference `comp` (≠ 0) — the inverse of
/// `read_mv_component`. Decomposes `|comp| - 1` into `(class, d, fp, hp)`.
fn encode_mv_component(
    enc: &mut BoolEncoder,
    comp: i32,
    c: &NmvComp,
    usehp: bool,
    cnt: &mut crate::mv::NmvCompCounts,
) {
    let sign = comp < 0;
    enc.write_bool(sign as u32, c.sign);
    cnt.sign[sign as usize] += 1;

    // z = mag - 1 partitions into classes: class 0 spans [0,16); class c≥1 spans
    // [2^(c+3), 2^(c+4)). base(c) = (c==0) ? 0 : 1<<(c+3); offset = z - base
    // packs as (d<<3) | (fp<<1) | hp.
    let z = comp.unsigned_abs() as i32 - 1;
    let mv_class = if z < 16 {
        0usize
    } else {
        ((31 - (z as u32).leading_zeros()) as usize - 3).min(10)
    };
    enc.write_tree(&MV_CLASS_TREE, &c.classes, mv_class as i32);
    cnt.classes[mv_class] += 1;
    let class0 = mv_class == 0;
    let base = if class0 { 0 } else { 1i32 << (mv_class + 3) };
    let offset = z - base;
    let d = offset >> 3;
    let fp = (offset >> 1) & 3;
    let hp = offset & 1;

    if class0 {
        enc.write_bool(d as u32, c.class0[0]); // CLASS0_BITS = 1
        cnt.class0[d as usize] += 1;
    } else {
        // CLASS0_BITS - 1 + mv_class == mv_class integer bits, LSB first.
        for i in 0..mv_class {
            let bit = (d >> i) & 1;
            enc.write_bool(bit as u32, c.bits[i]);
            cnt.bits[i][bit as usize] += 1;
        }
    }

    let fp_probs = if class0 {
        &c.class0_fp[d as usize]
    } else {
        &c.fp
    };
    enc.write_tree(&MV_FP_TREE, fp_probs, fp);
    if class0 {
        cnt.class0_fp[d as usize][fp as usize] += 1;
    } else {
        cnt.fp[fp as usize] += 1;
    }

    // The decoder forces hp = 1 when high precision is unavailable, and counts
    // it either way. A non-hp MV must therefore carry hp == 1 (even magnitude).
    if usehp {
        enc.write_bool(hp as u32, if class0 { c.class0_hp } else { c.hp });
    }
    let hp_count = if usehp { hp } else { 1 } as usize;
    if class0 {
        cnt.class0_hp[hp_count] += 1;
    } else {
        cnt.hp[hp_count] += 1;
    }
}

/// Encode the MV `mv` against its predictor `ref_mv` — the inverse of
/// [`read_mv`](crate::mv::read_mv). Both are in 1/8-pel units.
pub fn encode_mv(
    enc: &mut BoolEncoder,
    mv: (i32, i32),
    ref_mv: (i32, i32),
    ctx: &NmvContext,
    allow_hp: bool,
    cnt: &mut NmvCounts,
) {
    let diff = (mv.0 - ref_mv.0, mv.1 - ref_mv.1);
    // joint: +2 if the row (vertical) differs, +1 if the col (horizontal) differs.
    let joint = (if diff.0 != 0 { 2 } else { 0 }) + (if diff.1 != 0 { 1 } else { 0 });
    enc.write_tree(&MV_JOINT_TREE, &ctx.joints, joint as i32);
    cnt.joints[joint] += 1;

    let use_hp = allow_hp && use_mv_hp(ref_mv);
    if diff.0 != 0 {
        encode_mv_component(enc, diff.0, &ctx.comps[0], use_hp, &mut cnt.comps[0]);
    }
    if diff.1 != 0 {
        encode_mv_component(enc, diff.1, &ctx.comps[1], use_hp, &mut cnt.comps[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BoolDecoder;
    use crate::mv::read_mv;
    use crate::prob_tables::DEFAULT_NMV_CONTEXT;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// A component difference: ~25% zero, otherwise a magnitude spanning every
    /// class. When high precision is off, the magnitude is forced even (hp == 1).
    fn rand_diff(s: &mut u64, use_hp: bool) -> i32 {
        if xs(s) % 4 == 0 {
            return 0;
        }
        let mut mag = match xs(s) % 10 {
            0..=6 => 1 + (xs(s) % 32) as i32,   // small (classes 0..1)
            7..=8 => 1 + (xs(s) % 1024) as i32, // mid classes
            _ => 1 + (xs(s) % 8000) as i32,     // high classes incl. 10
        };
        if !use_hp {
            mag &= !1; // even magnitude → hp = 1
            if mag == 0 {
                mag = 2;
            }
        }
        if xs(s) & 1 == 0 {
            mag
        } else {
            -mag
        }
    }

    #[test]
    fn encode_mv_roundtrips_through_decoder() {
        let ctx = DEFAULT_NMV_CONTEXT;
        let mut s = 0x4d56_0a0b_0c0d_0e0fu64;
        for _ in 0..4000 {
            let ref_mv = (
                (xs(&mut s) % 256) as i32 - 128,
                (xs(&mut s) % 256) as i32 - 128,
            );
            let allow_hp = xs(&mut s) & 1 == 0;
            let use_hp = allow_hp && use_mv_hp(ref_mv);
            let diff = (rand_diff(&mut s, use_hp), rand_diff(&mut s, use_hp));
            let mv = (ref_mv.0 + diff.0, ref_mv.1 + diff.1);

            let mut enc = BoolEncoder::new();
            let mut cnt_e = NmvCounts::default();
            encode_mv(&mut enc, mv, ref_mv, &ctx, allow_hp, &mut cnt_e);
            let bytes = enc.finish();

            let mut bd = BoolDecoder::new(&bytes).unwrap();
            let mut cnt_d = NmvCounts::default();
            let got = read_mv(&mut bd, ref_mv, &ctx, allow_hp, &mut cnt_d);

            assert_eq!(got, mv, "mv ref={ref_mv:?} diff={diff:?} hp={allow_hp}");
            assert!(cnt_e == cnt_d, "nmv counts diverged");
        }
    }
}
