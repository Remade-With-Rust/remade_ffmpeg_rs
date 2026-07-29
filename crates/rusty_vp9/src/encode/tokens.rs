//! VP9 encoder — coefficient token coding (Floor 2, brick B1).
//!
//! [`encode_coefs`] is the exact inverse of the decoder's
//! [`decode_coefs`](crate::token::decode_coefs): it walks the same scan order,
//! emitting per position an EOB / zero-run / token-tree decision, the category
//! extra bits, and the sign — through the [`BoolEncoder`]. It reuses the
//! decoder's context derivation (`get_coef_context`), scan/neighbour/band
//! tables, Pareto expansion and category probabilities verbatim, and accumulates
//! the identical symbol counts. A round-trip through `decode_coefs` recovers the
//! exact dequantized block, EOB, and counts.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use super::bitwriter::BoolEncoder;
use crate::prob_tables::{
    CAT1_PROB, CAT2_PROB, CAT3_PROB, CAT4_PROB, CAT5_PROB, CAT6_PROB, CAT6_PROB_HIGH10,
    CAT6_PROB_HIGH12, PARETO8_FULL,
};
use crate::scan_tables::{COEFBAND_4X4, COEFBAND_8X8PLUS};
use crate::token::get_coef_context;

/// A destination for boolean symbols: either emit them (encode) or accumulate
/// their bit cost (RDO). Both B1 and B2 drive the *same* tree through this, so
/// the cost can never drift from what the encoder actually writes.
trait BitSink {
    fn put(&mut self, bit: u32, prob: u8);

    /// Code the magnitude of a non-zero coefficient; returns its energy class.
    /// Default walks the token tree bit-by-bit (the emit path); the costing sink
    /// overrides it with a table lookup whose entries were BUILT by this walk,
    /// so the two can never drift.
    #[inline]
    fn put_magnitude(&mut self, aval: u32, prob2: u8, cat6: &[u8], cat6_bits: usize) -> u8
    where
        Self: Sized,
    {
        code_magnitude(self, aval, prob2, cat6, cat6_bits)
    }
}

impl BitSink for BoolEncoder {
    #[inline]
    fn put(&mut self, bit: u32, prob: u8) {
        self.write_bool(bit, prob);
    }
}

/// Boolean-coder bit cost in Q8 (256ths of a bit): `cost_q8[q] = -log2(q/256)·256`.
/// `P(bit=0) ≈ prob/256`, `P(bit=1) ≈ (256-prob)/256`.
fn cost_table() -> &'static [u16; 257] {
    static T: OnceLock<[u16; 257]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u16; 257];
        for (q, slot) in t.iter_mut().enumerate().skip(1) {
            *slot = (-(q as f64 / 256.0).log2() * 256.0).round() as u16;
        }
        t[0] = t[1];
        t
    })
}

#[inline]
pub(crate) fn cost_bit(prob: u8, bit: u32) -> u64 {
    let t = cost_table();
    if bit == 0 {
        t[prob as usize] as u64
    } else {
        t[256 - prob as usize] as u64
    }
}

/// Q8 bit cost of coding `symbol` through a boolean `tree` with `probs` — the
/// cost-model mirror of [`BoolEncoder::write_tree`], for RD estimation.
pub(crate) fn tree_bit_cost(tree: &[i8], probs: &[u8], symbol: i32) -> u64 {
    let mut path: Vec<(usize, u32)> = Vec::new();
    super::bitwriter::find_tree_path(tree, 0, symbol, &mut path);
    path.iter()
        .map(|&(node, bit)| cost_bit(probs[node >> 1], bit))
        .sum()
}

/// Energy class of a magnitude — mirrors `code_magnitude`'s returns.
#[inline]
fn class_of(aval: u32) -> u8 {
    match aval {
        1 => 1,
        2 => 2,
        3..=4 => 3,
        5..=10 => 4,
        _ => 5,
    }
}

/// Static magnitude-cost table: `MAG_COST[prob2][aval]` = Q8 cost of coding
/// magnitude `aval` (1..=66, i.e. through CAT5) with pivot prob `prob2`. The
/// magnitude cost depends ONLY on (prob2, aval) plus the constant Pareto/CAT
/// tables — never on per-frame state — so it's built once per process, BY the
/// canonical tree walk itself (identical sums by construction). CAT6 (aval ≥ 67,
/// bit-depth-dependent) still walks. This kills the ~12-sequential-`cost_bit`
/// chain per nonzero coefficient that dominated coef_cost/trellis at low crf.
fn mag_cost_table() -> &'static [[u16; 67]; 256] {
    static T: OnceLock<Box<[[u16; 67]; 256]>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![[0u16; 67]; 256];
        for prob2 in 1..=255usize {
            for aval in 1..=66u32 {
                let mut sink = CostSink(0);
                // cat6 args unused for aval ≤ 66.
                code_magnitude(&mut sink, aval, prob2 as u8, &CAT6_PROB, 14);
                t[prob2][aval as usize] = sink.0 as u16;
            }
        }
        t.into_boxed_slice().try_into().unwrap()
    })
}

/// Q8 cost of magnitude `aval` under pivot `prob2` — table hit for aval ≤ 66
/// (through CAT5), `None` for the rare CAT6 range (caller falls back to exact).
pub(crate) fn mag_cost_q8(prob2: u8, aval: u32) -> Option<u16> {
    if (1..=66).contains(&aval) {
        Some(mag_cost_table()[prob2 as usize][aval as usize])
    } else {
        None
    }
}

/// Cost-accumulating sink (Q8 bits) for B2.
struct CostSink(u64);

impl BitSink for CostSink {
    #[inline]
    fn put(&mut self, bit: u32, prob: u8) {
        self.0 += cost_bit(prob, bit);
    }

    #[inline]
    fn put_magnitude(&mut self, aval: u32, prob2: u8, cat6: &[u8], cat6_bits: usize) -> u8 {
        if aval <= 66 {
            self.0 += mag_cost_table()[prob2 as usize][aval as usize] as u64;
            class_of(aval)
        } else {
            code_magnitude(self, aval, prob2, cat6, cat6_bits)
        }
    }
}

/// Code an `n`-bit magnitude MSB-first — the inverse of the decoder's `read_coeff`.
#[inline]
fn code_extra<S: BitSink>(sink: &mut S, value: u32, probs: &[u8], n: usize) {
    for i in 0..n {
        sink.put((value >> (n - 1 - i)) & 1, probs[i]);
    }
}

/// Code the magnitude `aval` (≥ 1) of a non-zero coefficient through the token
/// tree (inverse of `decode_coefs`'s token branch). `prob2` is the model pivot
/// node, which also indexes the Pareto tail. Returns the energy class to store
/// in the token cache (matching the decoder).
fn code_magnitude<S: BitSink>(
    sink: &mut S,
    aval: u32,
    prob2: u8,
    cat6: &[u8],
    cat6_bits: usize,
) -> u8 {
    if aval == 1 {
        sink.put(0, prob2); // ONE
        return 1;
    }
    sink.put(1, prob2); // TWO+
    let p = &PARETO8_FULL[prob2 as usize - 1];
    if aval <= 4 {
        sink.put(0, p[0]);
        if aval == 2 {
            sink.put(0, p[1]);
            2
        } else {
            sink.put(1, p[1]);
            sink.put(aval - 3, p[2]); // 3→0, 4→1
            3
        }
    } else {
        sink.put(1, p[0]);
        if aval <= 10 {
            sink.put(0, p[3]);
            if aval <= 6 {
                sink.put(0, p[4]);
                code_extra(sink, aval - 5, &CAT1_PROB, 1);
            } else {
                sink.put(1, p[4]);
                code_extra(sink, aval - 7, &CAT2_PROB, 2);
            }
            4
        } else {
            sink.put(1, p[3]);
            if aval <= 34 {
                sink.put(0, p[5]);
                if aval <= 18 {
                    sink.put(0, p[6]);
                    code_extra(sink, aval - 11, &CAT3_PROB, 3);
                } else {
                    sink.put(1, p[6]);
                    code_extra(sink, aval - 19, &CAT4_PROB, 4);
                }
            } else {
                sink.put(1, p[5]);
                if aval <= 66 {
                    sink.put(0, p[7]);
                    code_extra(sink, aval - 35, &CAT5_PROB, 5);
                } else {
                    sink.put(1, p[7]);
                    code_extra(sink, aval - 67, cat6, cat6_bits);
                }
            }
            5
        }
    }
}

/// The shared coefficient-block walk used by both [`encode_coefs`] (emit) and
/// [`coef_cost`] (cost). Mirrors `decode_coefs` exactly, including the token
/// cache, context derivation and symbol counts.
#[allow(clippy::too_many_arguments)]
fn code_block<S: BitSink, const COUNTS: bool>(
    sink: &mut S,
    levels: &[i32],
    scan: &[i16],
    nb: &[i16],
    eob: usize,
    coef_probs: &[[[u8; 3]; 6]; 6],
    tx_size: usize,
    mut ctx: usize,
    token_cache: &mut [u8],
    coef_cnt: &mut [[[u32; 4]; 6]; 6],
    eob_cnt: &mut [[u32; 6]; 6],
    bit_depth: u32,
) {
    let max_eob = 16usize << (tx_size << 1);
    let band_translate: &[u8] = if tx_size == 0 {
        &COEFBAND_4X4
    } else {
        &COEFBAND_8X8PLUS
    };
    let (cat6, cat6_bits): (&[u8], usize) = match bit_depth {
        10 => (&CAT6_PROB_HIGH10, 16),
        12 => (&CAT6_PROB_HIGH12, 18),
        _ => (&CAT6_PROB, 14),
    };
    // NO defensive `token_cache` pre-fill: `get_coef_context(nb, cache, c)` only
    // reads positions whose scan index precedes `c` (VP9 neighbour tables), and
    // the walk writes every position (zero or class) before advancing past it —
    // so every read hits a written entry. (Byte-identical; the fill was ~5% of
    // the costing walk.)

    let mut c = 0usize;
    while c < max_eob {
        let band = band_translate[c] as usize;
        if COUNTS {
            eob_cnt[band][ctx] += 1;
        }
        if c == eob {
            // End of block: no more non-zero coefficients.
            sink.put(0, coef_probs[band][ctx][0]);
            if COUNTS {
                coef_cnt[band][ctx][3] += 1; // EOB_MODEL_TOKEN
            }
            break;
        }
        sink.put(1, coef_probs[band][ctx][0]); // not EOB

        // Zero-run, then the non-zero coefficient (the inner `while` of decode).
        loop {
            let band = band_translate[c] as usize;
            let pos = scan[c] as usize;
            if levels[pos] == 0 {
                sink.put(0, coef_probs[band][ctx][1]); // ZERO
                if COUNTS {
                    coef_cnt[band][ctx][0] += 1;
                }
                token_cache[pos] = 0;
                c += 1;
                ctx = get_coef_context(nb, token_cache, c);
            } else {
                sink.put(1, coef_probs[band][ctx][1]); // non-zero
                break;
            }
        }

        let band = band_translate[c] as usize;
        let pos = scan[c] as usize;
        let lvl = levels[pos];
        let aval = lvl.unsigned_abs();
        if COUNTS {
            coef_cnt[band][ctx][if aval >= 2 { 2 } else { 1 }] += 1; // TWO+ / ONE
        }
        let class = sink.put_magnitude(aval, coef_probs[band][ctx][2], cat6, cat6_bits);
        token_cache[pos] = class;
        sink.put((lvl < 0) as u32, 128); // sign
        c += 1;
        ctx = get_coef_context(nb, token_cache, c);
    }
}

/// Reverse neighbour map for a scan: `rev[pos]` lists the scan indices `d` (≥1)
/// whose context reads `pos` (i.e. `nb[2d]==pos || nb[2d+1]==pos`). Because a
/// coefficient's energy class is a function of its magnitude ALONE (never its
/// context), changing one coefficient perturbs only its own cost plus the cost of
/// the ~1–2 positions that neighbour it — this map is what makes the trellis rate
/// O(1) per candidate instead of an O(eob) re-walk. Cached per (tx_size, scan).
fn reverse_nb(tx_size: usize, tx_type_id: u8, nb: &[i16]) -> Rc<[Box<[u16]>]> {
    thread_local! {
        // 16 (tx_size × tx_type) combos — a flat array beats a hashed lookup on this
        // per-block hot path.
        static CACHE: RefCell<[Option<Rc<[Box<[u16]>]>>; 16]> =
            const { RefCell::new([const { None }; 16]) };
    }
    let key = tx_size * 4 + tx_type_id as usize;
    CACHE.with(|c| {
        if let Some(r) = &c.borrow()[key] {
            return r.clone();
        }
        let max_eob = 16usize << (tx_size << 1);
        let mut rev: Vec<Vec<u16>> = vec![Vec::new(); max_eob];
        // c == 0 uses the passed-in ctx0, not `nb`, so it depends on no coefficient.
        for d in 1..max_eob {
            let a = nb[2 * d] as usize;
            let b = nb[2 * d + 1] as usize;
            if a < max_eob {
                rev[a].push(d as u16);
            }
            if b != a && b < max_eob {
                rev[b].push(d as u16);
            }
        }
        let boxed: Rc<[Box<[u16]>]> = rev.into_iter().map(Vec::into_boxed_slice).collect();
        c.borrow_mut()[key] = Some(boxed.clone());
        boxed
    })
}

const NO_OVR: usize = usize::MAX;

/// Incremental coefficient-block rate model — the O(1)-per-candidate replacement
/// for re-walking [`coef_cost`] on every trellis probe. It maintains a per-scan-
/// position cost profile (`pcost`) + the token cache so that dropping the EOB tail
/// or lowering one coefficient only recomputes the handful of affected positions.
/// `total()` is always exactly equal to `coef_cost(current levels, current eob)`.
pub(crate) struct RateTracker<'a> {
    scan: &'a [i16],
    nb: &'a [i16],
    band: &'a [u8],
    probs: &'a [[[u8; 3]; 6]; 6],
    rev: Rc<[Box<[u16]>]>,
    cat6: &'a [u8],
    cat6_bits: usize,
    ctx0: usize,
    max_eob: usize,
    // Borrowed reusable scratch (both ≥ max_eob) — NOT owned, so constructing a
    // tracker per trellis call costs no allocation / 9 KB zero-init.
    cache: &'a mut [u8],
    pcost: &'a mut [u64],
    total: u64,
    eob: usize,
    /// Build-time context per scan index (the FROZEN ctx view for DP-lite).
    /// Borrowed scratch — an inline `[u8; 1024]` field cost a 1 KB zero-init
    /// per tracker construction (the snapshot-memset lesson again).
    bctx: &'a mut [u8],
    // Pending change stashed by `probe` so `commit` applies it without recomputing
    // the affected recosts a second time.
    p_total: u64,
    p_ne: usize,
    p_tail: bool,
    p_added: u64,
    p_i: usize,
    p_class: u8,
    p_costs: [(usize, u64); 9],
    p_n: usize,
}

impl<'a> RateTracker<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        levels: &[i32],
        scan: &'a [i16],
        nb: &'a [i16],
        eob: usize,
        probs: &'a [[[u8; 3]; 6]; 6],
        tx_size: usize,
        tx_type_id: u8,
        ctx0: usize,
        bit_depth: u32,
        cache: &'a mut [u8],
        pcost: &'a mut [u64],
        bctx: &'a mut [u8],
    ) -> Self {
        let band: &[u8] = if tx_size == 0 {
            &COEFBAND_4X4
        } else {
            &COEFBAND_8X8PLUS
        };
        let (cat6, cat6_bits): (&[u8], usize) = match bit_depth {
            10 => (&CAT6_PROB_HIGH10, 16),
            12 => (&CAT6_PROB_HIGH12, 18),
            _ => (&CAT6_PROB, 14),
        };
        let mut t = RateTracker {
            scan,
            nb,
            band,
            probs,
            rev: reverse_nb(tx_size, tx_type_id, nb),
            cat6,
            cat6_bits,
            ctx0,
            max_eob: 16usize << (tx_size << 1),
            cache,
            pcost,
            total: 0,
            eob,
            bctx,
            p_total: 0,
            p_ne: 0,
            p_tail: false,
            p_added: 0,
            p_i: 0,
            p_class: 0,
            p_costs: [(0, 0); 9],
            p_n: 0,
        };
        t.build(levels);
        t
    }

    #[inline]
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Context at scan index `c`, with an optional single-position class override
    /// (`ovr_pos`/`ovr_class`) so a probe can see the hypothetical new class of the
    /// changed coefficient without mutating the committed cache. Mirrors
    /// `get_coef_context` exactly (and `c==0` uses the block's entry context).
    #[inline]
    fn ctx_at(&self, c: usize, ovr_pos: usize, ovr_class: u8) -> usize {
        if c == 0 {
            return self.ctx0;
        }
        let a = self.nb[2 * c] as usize;
        let b = self.nb[2 * c + 1] as usize;
        let ca = if a == ovr_pos { ovr_class } else { self.cache[a] };
        let cb = if b == ovr_pos { ovr_class } else { self.cache[b] };
        (1 + ca as usize + cb as usize) >> 1
    }

    /// Cost (Q8 bits) + energy class contributed AT scan index `c` — the EOB/not-EOB
    /// flag (only when `c` starts a run, i.e. `c==0` or the previous coded level is
    /// non-zero) plus this position's ZERO / (non-zero + magnitude + sign) tokens.
    /// Exactly mirrors `code_block`'s per-position emission.
    #[inline]
    fn recost(&self, c: usize, levels: &[i32], eob: usize, ovr_pos: usize, ovr_class: u8) -> (u64, u8) {
        let band = self.band[c] as usize;
        let ctx = self.ctx_at(c, ovr_pos, ovr_class);
        let mut cost = 0u64;
        let checkpoint = c == 0 || levels[self.scan[c - 1] as usize] != 0;
        if checkpoint {
            cost += cost_bit(self.probs[band][ctx][0], if c == eob { 0 } else { 1 });
        }
        if c == eob {
            return (cost, 0);
        }
        let pos = self.scan[c] as usize;
        let lvl = levels[pos];
        if lvl == 0 {
            cost += cost_bit(self.probs[band][ctx][1], 0);
            (cost, 0)
        } else {
            cost += cost_bit(self.probs[band][ctx][1], 1);
            let mut sink = CostSink(0);
            let class = sink.put_magnitude(
                lvl.unsigned_abs(),
                self.probs[band][ctx][2],
                self.cat6,
                self.cat6_bits,
            );
            cost += sink.0;
            cost += cost_bit(128, (lvl < 0) as u32);
            (cost, class)
        }
    }

    /// The frozen (band, ctx, pivot-prob) of scan index `c` as of the build walk.
    #[inline]
    pub(crate) fn frozen(&self, c: usize) -> (usize, usize) {
        (self.band[c] as usize, self.bctx[c] as usize)
    }

    /// The probs table this tracker was built with.
    #[inline]
    pub(crate) fn probs(&self) -> &[[[u8; 3]; 6]; 6] {
        self.probs
    }

    /// Whether an explicit EOB terminator token exists at `eob`. A FULL block
    /// (`eob == max_eob`) is implicitly terminated by reaching the block end — its
    /// `code_block` loop stops at `c < max_eob` and never codes an EOB flag.
    #[inline]
    fn has_term(&self, eob: usize) -> bool {
        eob < self.max_eob
    }

    /// Baseline walk in scan order — fills `cache`, `pcost`, and `total`.
    fn build(&mut self, levels: &[i32]) {
        self.cache[..self.max_eob].fill(0);
        self.total = 0;
        let eob = self.eob;
        for c in 0..eob {
            self.bctx[c] = self.ctx_at(c, NO_OVR, 0) as u8;
            let (cost, class) = self.recost(c, levels, eob, NO_OVR, 0);
            self.pcost[c] = cost;
            self.total += cost;
            self.cache[self.scan[c] as usize] = class;
        }
        if self.has_term(eob) {
            let (cost, _) = self.recost(eob, levels, eob, NO_OVR, 0);
            self.pcost[eob] = cost;
            self.total += cost;
        }
    }

    /// Sum of the per-position costs of the coded tail `ne..old_eob` plus the old
    /// EOB terminator (if one existed) — the block that a drop-to-`ne` removes.
    #[inline]
    fn tail_cost(&self, ne: usize) -> u64 {
        let mut s: u64 = self.pcost[ne..self.eob].iter().sum();
        if self.has_term(self.eob) {
            s += self.pcost[self.eob];
        }
        s
    }

    /// Collect the scan indices whose cost changes when `scan[i]` is altered to
    /// `new_class` (own position + neighbours + the checkpoint successor `i+1` when
    /// `i`'s zero/non-zero status flips). Small (≤ ~4); deduped, all ≤ `eob`.
    fn affected(&self, i: usize, class_changed: bool, status_flipped: bool, eob: usize, out: &mut [usize; 8]) -> usize {
        let mut n = 0usize;
        let codeable = |d: usize| d < eob || (d == eob && eob < self.max_eob);
        let push = |out: &mut [usize; 8], n: &mut usize, d: usize| {
            if codeable(d) && !out[..*n].contains(&d) {
                out[*n] = d;
                *n += 1;
            }
        };
        if class_changed {
            for &d in self.rev[self.scan[i] as usize].iter() {
                push(out, &mut n, d as usize);
            }
        }
        if status_flipped {
            push(out, &mut n, i + 1);
        }
        n
    }

    /// New total rate (Q8) if `scan[i]` is changed to the value already written into
    /// `levels`, with the block trimmed to `ne`. Stashes the affected recosts so a
    /// following `commit` applies them without recomputing; mutates no committed state.
    pub(crate) fn probe(&mut self, levels: &[i32], i: usize, ne: usize) -> u64 {
        if ne < self.eob {
            // Tail drop: the coded tail ne..old_eob + old terminator are removed,
            // replaced by a single new EOB terminator at ne (ne < max_eob always).
            let removed = self.tail_cost(ne);
            let added = self.recost(ne, levels, ne, NO_OVR, 0).0;
            self.p_tail = true;
            self.p_ne = ne;
            self.p_added = added;
            self.p_total = self.total - removed + added;
            return self.p_total;
        }
        // Interior change (eob unchanged).
        let (new_i, new_class) = self.recost(i, levels, ne, NO_OVR, 0);
        let old_class = self.cache[self.scan[i] as usize];
        let class_changed = new_class != old_class;
        let status_flipped = (levels[self.scan[i] as usize] != 0) != (old_class != 0);
        let mut buf = [0usize; 8];
        let m = self.affected(i, class_changed, status_flipped, ne, &mut buf);
        let mut delta = new_i as i64 - self.pcost[i] as i64;
        self.p_costs[0] = (i, new_i);
        let mut pn = 1;
        for &d in &buf[..m] {
            let nc = self.recost(d, levels, ne, self.scan[i] as usize, new_class).0;
            delta += nc as i64 - self.pcost[d] as i64;
            self.p_costs[pn] = (d, nc);
            pn += 1;
        }
        self.p_tail = false;
        self.p_i = i;
        self.p_class = new_class;
        self.p_n = pn;
        self.p_total = (self.total as i64 + delta) as u64;
        self.p_total
    }

    /// Apply the last `probe` (its args are captured in the pending state) — no
    /// recompute, just write the stashed costs/class and the new total/eob.
    pub(crate) fn commit(&mut self, _levels: &[i32], _i: usize, _ne: usize) {
        self.total = self.p_total;
        if self.p_tail {
            let ne = self.p_ne;
            self.pcost[ne] = self.p_added;
            // Drop the tail from the cache (harmless but keeps state clean).
            for c in ne..self.eob {
                self.cache[self.scan[c] as usize] = 0;
            }
            self.eob = ne;
            return;
        }
        self.cache[self.scan[self.p_i] as usize] = self.p_class;
        for k in 0..self.p_n {
            let (idx, cost) = self.p_costs[k];
            self.pcost[idx] = cost;
        }
    }
}

/// Encode one transform block's coefficient `levels` (signed, natural row-major
/// order) — the inverse of [`decode_coefs`](crate::token::decode_coefs).
///
/// * `scan` / `nb` — the coefficient scan + neighbour table for this block.
/// * `eob` — scan positions through the last non-zero level (from `quantize`).
/// * `coef_probs` — `[band][ctx][3]` model probs for this (tx, plane, ref).
/// * `ctx` — initial above/left context (0..=2).
/// * `token_cache` — scratch (`>= max_eob`), maintained as the decoder does.
/// * `coef_cnt` / `eob_cnt` — symbol counts, accumulated identically to decode.
#[allow(clippy::too_many_arguments)]
pub fn encode_coefs(
    enc: &mut BoolEncoder,
    levels: &[i32],
    scan: &[i16],
    nb: &[i16],
    eob: usize,
    coef_probs: &[[[u8; 3]; 6]; 6],
    tx_size: usize,
    ctx: usize,
    token_cache: &mut [u8],
    coef_cnt: &mut [[[u32; 4]; 6]; 6],
    eob_cnt: &mut [[u32; 6]; 6],
    bit_depth: u32,
) {
    code_block::<_, true>(
        enc,
        levels,
        scan,
        nb,
        eob,
        coef_probs,
        tx_size,
        ctx,
        token_cache,
        coef_cnt,
        eob_cnt,
        bit_depth,
    );
}

/// Estimate the boolean-coder bit cost (Q8 — 256ths of a bit) of a coefficient
/// block **without emitting** — the RDO inner-loop oracle (brick B2). Walks the
/// identical tree as [`encode_coefs`], so its total equals what the encoder
/// actually spends.
#[allow(clippy::too_many_arguments)]
pub fn coef_cost(
    levels: &[i32],
    scan: &[i16],
    nb: &[i16],
    eob: usize,
    coef_probs: &[[[u8; 3]; 6]; 6],
    tx_size: usize,
    ctx: usize,
    token_cache: &mut [u8],
    bit_depth: u32,
) -> u64 {
    // The symbol counts (`cc`/`ec`) are write-only garbage on the costing path —
    // only `sink.0` is returned. Reuse thread-local scratch instead of zeroing
    // ~720 bytes on every call (the trellis calls this millions of times).
    thread_local! {
        static SCRATCH: std::cell::RefCell<([[[u32; 4]; 6]; 6], [[u32; 6]; 6])> =
            const { std::cell::RefCell::new(([[[0; 4]; 6]; 6], [[0; 6]; 6])) };
    }
    SCRATCH.with(|s| {
        let mut b = s.borrow_mut();
        let (cc, ec) = &mut *b;
        let mut sink = CostSink(0);
        code_block::<_, false>(
            &mut sink, levels, scan, nb, eob, coef_probs, tx_size, ctx, token_cache, cc, ec,
            bit_depth,
        );
        sink.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BoolDecoder;
    use crate::quant::{ac_quant, dc_quant};
    use crate::token::{decode_coefs, default_coef_probs, get_scan};
    use crate::transform::TxType;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// A magnitude weighted toward small values but reaching every CAT range
    /// (so the full token tree, including CAT6, is exercised).
    fn rand_mag(s: &mut u64) -> u32 {
        match xs(s) % 100 {
            0..=70 => 1 + (xs(s) % 4) as u32,    // 1..4 (ONE/TWO/THREE/FOUR)
            71..=85 => 5 + (xs(s) % 6) as u32,   // 5..10 (CAT1/CAT2)
            86..=94 => 11 + (xs(s) % 24) as u32, // 11..34 (CAT3/CAT4)
            95..=98 => 35 + (xs(s) % 32) as u32, // 35..66 (CAT5)
            _ => 67 + (xs(s) % 4000) as u32,     // CAT6
        }
    }

    /// A random coefficient block: a valid EOB with the last scan position
    /// forced non-zero, ~40% interior zeros, magnitudes across every CAT range.
    fn gen_block(s: &mut u64, scan: &[i16], n: usize, max_eob: usize) -> (Vec<i32>, usize) {
        let eob = 1 + (xs(s) as usize % max_eob);
        let mut levels = vec![0i32; n * n];
        for c in 0..eob {
            let pos = scan[c] as usize;
            if c != eob - 1 && xs(s) % 5 < 2 {
                continue;
            }
            let mag = rand_mag(s) as i32;
            levels[pos] = if xs(s) & 1 == 0 { mag } else { -mag };
        }
        if levels[scan[eob - 1] as usize] == 0 {
            levels[scan[eob - 1] as usize] = 1;
        }
        (levels, eob)
    }

    #[test]
    fn encode_coefs_roundtrips_through_decoder() {
        let cases = [
            (0usize, TxType::DctDct),
            (0, TxType::AdstDct),
            (0, TxType::DctAdst),
            (1, TxType::DctDct),
            (2, TxType::DctDct),
            (3, TxType::DctDct),
        ];
        let mut s = 0x5eed_1234_abcd_0001u64;
        for &(tx_size, tx) in &cases {
            let n = 4usize << tx_size;
            let max_eob = 16usize << (tx_size << 1);
            let (scan, nb) = get_scan(tx_size, tx);
            let coef_probs = default_coef_probs(tx_size, 0, 0);
            let (dc, ac) = (dc_quant(40, 8), ac_quant(40, 8));
            let dq_shift = if tx_size == 3 { 1 } else { 0 };
            for _ in 0..150 {
                let (levels, eob) = gen_block(&mut s, scan, n, max_eob);
                let ctx0 = (xs(&mut s) % 3) as usize;
                // Encode.
                let mut enc = BoolEncoder::new();
                let mut tc_e = vec![0u8; max_eob];
                let mut cc_e = [[[0u32; 4]; 6]; 6];
                let mut ec_e = [[0u32; 6]; 6];
                encode_coefs(
                    &mut enc, &levels, scan, nb, eob, coef_probs, tx_size, ctx0, &mut tc_e,
                    &mut cc_e, &mut ec_e, 8,
                );
                let bytes = enc.finish();

                // Decode.
                let mut bd = BoolDecoder::new(&bytes).unwrap();
                let mut dqcoeff = vec![0i32; max_eob];
                let mut tc_d = vec![0u8; max_eob];
                let mut cc_d = [[[0u32; 4]; 6]; 6];
                let mut ec_d = [[0u32; 6]; 6];
                let (c, _) = decode_coefs(
                    &mut bd,
                    coef_probs,
                    tx_size,
                    scan,
                    nb,
                    (dc, ac),
                    ctx0,
                    &mut dqcoeff,
                    &mut tc_d,
                    &mut cc_d,
                    &mut ec_d,
                    8,
                );

                assert_eq!(c, eob, "eob {tx_size} {tx:?}");
                for pos in 0..n * n {
                    let lvl = levels[pos];
                    let step = if pos == 0 { dc } else { ac } as i64;
                    let want = if lvl == 0 {
                        0
                    } else {
                        let v = ((lvl.unsigned_abs() as i64 * step) >> dq_shift) as i32;
                        if lvl < 0 {
                            -v
                        } else {
                            v
                        }
                    };
                    assert_eq!(dqcoeff[pos], want, "dqcoeff {tx_size} {tx:?} pos {pos}");
                }
                assert_eq!(cc_e, cc_d, "coef counts {tx_size} {tx:?}");
                assert_eq!(ec_e, ec_d, "eob counts {tx_size} {tx:?}");
            }
        }
    }

    #[test]
    fn coef_cost_predicts_emitted_bits() {
        // B2: the summed cost (without emitting) must predict the bits the bool
        // coder actually spends. Encode many blocks into one stream and compare
        // the predicted total to the real output size.
        let cases = [
            (0usize, TxType::DctDct),
            (1, TxType::DctDct),
            (2, TxType::DctDct),
            (3, TxType::DctDct),
        ];
        let mut s = 0x2024_0a0b_0c0d_0e0fu64;
        let mut enc = BoolEncoder::new();
        let mut total_cost_q8 = 0u64;
        for &(tx_size, tx) in &cases {
            let n = 4usize << tx_size;
            let max_eob = 16usize << (tx_size << 1);
            let (scan, nb) = get_scan(tx_size, tx);
            let coef_probs = default_coef_probs(tx_size, 0, 0);
            for _ in 0..400 {
                let (levels, eob) = gen_block(&mut s, scan, n, max_eob);
                let ctx0 = (xs(&mut s) % 3) as usize;
                let mut tc = vec![0u8; max_eob];
                total_cost_q8 += coef_cost(
                    &levels, scan, nb, eob, coef_probs, tx_size, ctx0, &mut tc, 8,
                );
                let mut tc2 = vec![0u8; max_eob];
                let mut cc = [[[0u32; 4]; 6]; 6];
                let mut ec = [[0u32; 6]; 6];
                encode_coefs(
                    &mut enc, &levels, scan, nb, eob, coef_probs, tx_size, ctx0, &mut tc2, &mut cc,
                    &mut ec, 8,
                );
            }
        }
        let actual_bits = enc.finish().len() as f64 * 8.0;
        let predicted_bits = total_cost_q8 as f64 / 256.0;
        let rel = (predicted_bits - actual_bits).abs() / actual_bits;
        // The bool coder achieves close to the entropy; a thin margin covers the
        // coding loss + the one-time marker/flush overhead.
        assert!(
            rel < 0.01,
            "cost prediction off by {:.3}% (predicted {predicted_bits:.0} vs actual {actual_bits:.0})",
            rel * 100.0
        );
    }

    /// The magnitude-cost table must equal the canonical tree walk for EVERY
    /// (prob2, magnitude) it covers, and `class_of` must match the walk's class.
    #[test]
    fn mag_cost_table_matches_walk_exhaustively() {
        for prob2 in 1..=255u16 {
            for aval in 1..=66u32 {
                let mut walk = CostSink(0);
                let class = code_magnitude(&mut walk, aval, prob2 as u8, &CAT6_PROB, 14);
                assert_eq!(
                    mag_cost_table()[prob2 as usize][aval as usize] as u64, walk.0,
                    "prob2={prob2} aval={aval}"
                );
                assert_eq!(class_of(aval), class, "class prob2={prob2} aval={aval}");
            }
        }
    }

    /// The incremental `RateTracker` must equal `coef_cost` after EVERY trellis-style
    /// mutation (EOB-tail drop + interior single-step lowering) — the parity path is
    /// only safe if its rate is bit-for-bit the re-walk it replaces.
    #[test]
    fn rate_tracker_matches_coef_cost_incrementally() {
        let cases = [
            (0usize, TxType::DctDct),
            (0, TxType::AdstDct),
            (0, TxType::DctAdst),
            (1, TxType::DctDct),
            (1, TxType::AdstAdst),
            (2, TxType::DctDct),
            (3, TxType::DctDct),
        ];
        let mut s = 0x1357_9bdf_2468_ace0u64;
        for &(tx_size, tx) in &cases {
            let n = 4usize << tx_size;
            let max_eob = 16usize << (tx_size << 1);
            let (scan, nb) = get_scan(tx_size, tx);
            let coef_probs = default_coef_probs(tx_size, 0, 0);
            for _ in 0..200 {
                let (mut levels, eob0) = gen_block(&mut s, scan, n, max_eob);
                let ctx0 = (xs(&mut s) % 3) as usize;
                let mut tc = vec![0u8; max_eob];
                let cc = |lv: &[i32], e: usize, tc: &mut [u8]| {
                    coef_cost(lv, scan, nb, e, coef_probs, tx_size, ctx0, tc, 8)
                };
                let mut cbuf = vec![0u8; max_eob];
                let mut pbuf = vec![0u64; max_eob + 1];
                let mut bbuf = vec![0u8; max_eob];
                let mut tr = RateTracker::new(
                    &levels, scan, nb, eob0, coef_probs, tx_size, tx as u8, ctx0, 8, &mut cbuf,
                    &mut pbuf, &mut bbuf,
                );
                assert_eq!(tr.total(), cc(&levels, eob0, &mut tc), "baseline {tx_size} {tx:?}");
                let mut eob = eob0;

                // EOB-tail drop pass (mirror trellis_eob): repeatedly zero scan[eob-1].
                while eob > 0 && xs(&mut s) % 2 == 0 {
                    let last = scan[eob - 1] as usize;
                    levels[last] = 0;
                    let mut ne = eob - 1;
                    while ne > 0 && levels[scan[ne - 1] as usize] == 0 {
                        ne -= 1;
                    }
                    let want = cc(&levels, ne, &mut tc);
                    assert_eq!(tr.probe(&levels, eob - 1, ne), want, "drop {tx_size} {tx:?} eob={eob}");
                    tr.commit(&levels, eob - 1, ne);
                    assert_eq!(tr.total(), want, "drop commit {tx_size} {tx:?}");
                    eob = ne;
                }

                // Interior single-step lowering pass (reverse scan order).
                let mut i = eob;
                while i > 0 {
                    i -= 1;
                    let pos = scan[i] as usize;
                    if levels[pos] == 0 {
                        continue;
                    }
                    let sign = if levels[pos] < 0 { -1i32 } else { 1 };
                    let mag = levels[pos].unsigned_abs() as i32 - 1;
                    let saved = levels[pos];
                    levels[pos] = sign * mag;
                    let mut ne = eob;
                    while ne > 0 && levels[scan[ne - 1] as usize] == 0 {
                        ne -= 1;
                    }
                    let want = cc(&levels, ne, &mut tc);
                    assert_eq!(
                        tr.probe(&levels, i, ne),
                        want,
                        "lower {tx_size} {tx:?} i={i} eob={eob} {saved}->{}",
                        sign * mag
                    );
                    // Randomly accept (commit) or reject (restore) — exercises both paths.
                    if xs(&mut s) % 2 == 0 {
                        tr.commit(&levels, i, ne);
                        eob = ne;
                        assert_eq!(tr.total(), want, "lower commit {tx_size} {tx:?}");
                    } else {
                        levels[pos] = saved;
                        assert_eq!(tr.total(), cc(&levels, eob, &mut tc), "lower reject {tx_size} {tx:?}");
                    }
                }
            }
        }
    }
}
