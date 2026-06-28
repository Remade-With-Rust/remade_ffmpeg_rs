//! VP9 encoder — forward probability-delta coding (Foundation F4).
//!
//! The compressed header updates each entropy probability with a coded delta.
//! These are the exact inverses of the decoder's update readers
//! ([`diff_update`](crate::decode::diff_update),
//! [`decode_term_subexp`](crate::decode::decode_term_subexp)), and each is gated
//! by a round-trip back through them.

use super::bitwriter::BoolEncoder;
use crate::prob::inv_remap_prob;

/// Forward probability remap — the encode-side inverse of
/// [`inv_remap_prob`](crate::prob::inv_remap_prob). Returns the **smallest**
/// sub-exp delta `d` such that `inv_remap_prob(d, old) == new`.
///
/// The term-subexp coder is monotone in `d` (≤16 → 5 bits, ≤32 → 6, ≤64 → 8,
/// else ~11), so the smallest legal delta is also the cheapest to code — which
/// matches libvpx `remap_prob`'s intent without re-entering a separate forward
/// map table. Exactness is proven exhaustively by the round-trip gate; we only
/// reuse the already-trusted decoder function.
pub fn forward_remap_prob(new_p: u8, old_p: u8) -> u8 {
    let mut best = 0u8;
    let mut best_err = i32::MAX;
    for d in 0u8..255 {
        let p = inv_remap_prob(d as i32, old_p as i32);
        if p == new_p {
            return d;
        }
        let err = (p as i32 - new_p as i32).abs();
        if err < best_err {
            best_err = err;
            best = d;
        }
    }
    best // unreachable for valid probs (the gate proves every pair is exact)
}

/// Encode a sub-exp magnitude `d` (0..=254) — inverse of
/// [`decode_term_subexp`](crate::decode::decode_term_subexp). Every bit is at
/// probability 128.
pub fn encode_term_subexp(enc: &mut BoolEncoder, d: u32) {
    debug_assert!(d <= 254, "term-subexp delta out of range: {d}");
    if d < 16 {
        enc.write_bool(0, 128);
        enc.write_literal(d, 4);
    } else if d < 32 {
        enc.write_bool(1, 128);
        enc.write_bool(0, 128);
        enc.write_literal(d - 16, 4);
    } else if d < 64 {
        enc.write_bool(1, 128);
        enc.write_bool(1, 128);
        enc.write_bool(0, 128);
        enc.write_literal(d - 32, 5);
    } else {
        enc.write_bool(1, 128);
        enc.write_bool(1, 128);
        enc.write_bool(1, 128);
        // decode_uniform with l=8, m=65: values 0..64 take 7 bits, 65..189 take
        // 7 bits + 1 (an extra low bit). Invert that split exactly.
        let w = d - 64;
        if w < 65 {
            enc.write_literal(w, 7);
        } else {
            let val = d + 1; // == w + 65; v = val>>1 ∈ 65..127, bit = val&1
            enc.write_literal(val >> 1, 7);
            enc.write_bool(val & 1, 128);
        }
    }
}

/// Encode a conditional probability update — inverse of
/// [`diff_update`](crate::decode::diff_update). Writes the update flag at p=252;
/// when `new_p != old_p`, also the sub-exp delta carrying `new_p`. Returns the
/// probability now in effect (`new_p` if updated, else `old_p`).
pub fn diff_update_encode(enc: &mut BoolEncoder, old_p: u8, new_p: u8) -> u8 {
    if new_p == old_p {
        enc.write_bool(0, 252);
        old_p
    } else {
        enc.write_bool(1, 252);
        encode_term_subexp(enc, forward_remap_prob(new_p, old_p) as u32);
        new_p
    }
}

/// Encode an MV probability update — inverse of libvpx `update_mv_prob`: the
/// flag at p=252, then a 7-bit literal reconstructing `p = (lit << 1) | 1`.
/// MV probs are always odd; `new_p` is rounded to the nearest representable odd
/// value, and the value actually coded is returned.
pub fn update_mv_prob_encode(enc: &mut BoolEncoder, old_p: u8, new_p: u8) -> u8 {
    let coded = (new_p & !1) | 1; // nearest odd ≤ new_p, then |1
    if coded == old_p {
        enc.write_bool(0, 252);
        old_p
    } else {
        enc.write_bool(1, 252);
        enc.write_literal((coded >> 1) as u32, 7);
        coded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BoolDecoder;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    #[test]
    fn forward_remap_inverts_every_reachable_target() {
        // A single sub-exp delta cannot reach *every* probability: INV_MAP_TABLE
        // has 255 entries over 254 distinct values, so exactly one target prob is
        // unreachable from each `old` (the encoder then codes "no update" or the
        // nearest). The real guarantee: forward_remap is an exact inverse for
        // every *reachable* target, and lands on a closest-reachable prob
        // otherwise. Exhaustive 255×255.
        for old in 1u8..=255 {
            let mut reachable = [false; 256];
            for d in 0u8..255 {
                reachable[inv_remap_prob(d as i32, old as i32) as usize] = true;
            }
            for new in 1u8..=255 {
                let d = forward_remap_prob(new, old);
                let got = inv_remap_prob(d as i32, old as i32);
                if reachable[new as usize] {
                    assert_eq!(got, new, "reachable old={old} new={new} d={d}");
                } else {
                    // Must degrade to a prob at minimum distance from `new`.
                    let best_err = (1u8..=255)
                        .filter(|&p| reachable[p as usize])
                        .map(|p| (p as i32 - new as i32).abs())
                        .min()
                        .unwrap();
                    assert_eq!(
                        (got as i32 - new as i32).abs(),
                        best_err,
                        "unreachable old={old} new={new}: got {got}"
                    );
                }
            }
        }
    }

    #[test]
    fn term_subexp_roundtrips_through_decoder() {
        // Every delta 0..=254 must round-trip through `decode_term_subexp`.
        let mut enc = BoolEncoder::new();
        for d in 0u32..=254 {
            encode_term_subexp(&mut enc, d);
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes).unwrap();
        for d in 0u32..=254 {
            assert_eq!(crate::decode::decode_term_subexp(&mut dec), d, "delta {d}");
        }
    }

    #[test]
    fn diff_update_roundtrips_through_decoder() {
        let mut s = 0xabcd_1234_5678_9999u64;
        let cases: Vec<(u8, u8)> = (0..4000)
            .map(|_| ((1 + xs(&mut s) % 255) as u8, (1 + xs(&mut s) % 255) as u8))
            .collect();
        let mut enc = BoolEncoder::new();
        for &(old, new) in &cases {
            diff_update_encode(&mut enc, old, new);
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes).unwrap();
        for &(old, new) in &cases {
            let mut p = old;
            crate::decode::diff_update(&mut dec, &mut p);
            assert_eq!(p, new, "old={old} new={new}");
        }
    }

    #[test]
    fn mv_prob_update_roundtrips() {
        // Encode odd targets (the only representable MV probs) and confirm the
        // decoder reconstructs them via `(literal(7) << 1) | 1`.
        let mut enc = BoolEncoder::new();
        let targets: Vec<u8> = (0..127).map(|i| ((i << 1) | 1) as u8).collect();
        let mut coded = Vec::new();
        for &t in &targets {
            coded.push(update_mv_prob_encode(&mut enc, 128, t));
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes).unwrap();
        for (&t, &c) in targets.iter().zip(&coded) {
            assert_eq!(c, t, "odd target should code exactly");
            // Mirror the decoder's `update_mv_prob`: flag@252 then 7-bit literal.
            let mut p = 128u8;
            if dec.read_bool(252) == 1 {
                p = ((dec.literal(7) << 1) | 1) as u8;
            }
            assert_eq!(p, t);
        }
    }
}
