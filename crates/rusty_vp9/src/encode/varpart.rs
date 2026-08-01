//! Variance-partition content signal — the cheap, content-*invariant* measurement
//! the content-adaptive dispatcher steers on.
//!
//! The recursive RD partition search (`rd_pick_partition`) evaluates a full mode
//! decision (motion search + transform + quantize) at every node it visits, and the
//! number of nodes it visits scales ~15× with content complexity (akiyo ~132/frame →
//! mobile ~1969/frame). That is the root of the encoder's content-speed variance.
//!
//! This module is the alternative signal libvpx's realtime ladder (`VAR_BASED_PARTITION`,
//! cpu-used ≥ 4) uses instead: an O(pixels) variance tree over the 64×64 superblock,
//! aggregated 8×8 → 16×16 → 32×32 → 64×64. Its cost is *fixed* per pixel regardless of
//! content, so routing an SB through it — instead of the RD search — makes that SB's
//! encode time content-invariant. Whether to route is the dispatcher's job
//! (`decide_tile_var`); this module only produces the signal and the variance-driven
//! partition shape.
//!
//! Variance is computed over the *prediction residual* (source − reference) when a
//! reference plane is supplied (inter frames: the zero-MV co-located last-frame recon),
//! and over the raw source otherwise (key frames). Residual variance is the better
//! dispatch feature — it measures how hard the block is to *code*, not merely how
//! textured it is — so a flat region the reference already predicts well reads as
//! "easy" even if it is busy.
//!
//! Ported from libvpx `vp9_encodeframe.c` (`fill_variance`/`get_variance`/
//! `sum_2_variances`/`set_vt_partitioning`); the variance scale (`256·per-sample-var`)
//! and the split/take logic mirror the reference so the thresholds transfer.

/// Sum and sum-of-squares of a set of samples; the additive primitive of the tree.
#[derive(Clone, Copy, Default)]
pub struct Var {
    sum: i64,
    sum_sq: u64,
}

impl Var {
    #[inline]
    fn add(self, o: Var) -> Var {
        Var {
            sum: self.sum + o.sum,
            sum_sq: self.sum_sq + o.sum_sq,
        }
    }
    /// libvpx `get_variance`: `256 · (SSE − sum²/count) >> log2_count`, i.e. the
    /// per-sample variance scaled by 256. `log2_count` = log2 of the sample count.
    #[inline]
    fn variance(self, log2_count: u32) -> i64 {
        let mean_sq = ((self.sum * self.sum) as u64) >> log2_count;
        let centred = self.sum_sq.saturating_sub(mean_sq);
        (256u64.wrapping_mul(centred) >> log2_count) as i64
    }
}

/// The four levels: index 0 = 8×8 (64 leaves), 1 = 16×16 (16 nodes), 2 = 32×32
/// (4 nodes), 3 = 64×64 (1 node). Each node stores its aggregated `Var`; the grid
/// at level L is `(8 >> L)` on a side.
#[derive(Clone)]
pub struct VarTree {
    /// `level[L]` holds the `(8>>L)²` node stats in raster order within the SB.
    level: [Vec<Var>; 4],
}

const LOG2_COUNT: [u32; 4] = [6, 8, 10, 12]; // log2(64/256/1024/4096)

impl VarTree {
    #[inline]
    fn grid(level: usize) -> usize {
        8 >> level
    }

    /// Build the tree for the 64×64 superblock whose top-left source pixel is
    /// (`x0`,`y0`). `src`/`s_stride` describe the (edge-clamped) source plane;
    /// `refp`/`r_stride` optionally describe an aligned reference plane — when
    /// `Some`, variance is over the residual (source − reference). Samples past the
    /// frame edge are clamped to the last in-frame pixel (matches how the encoder
    /// pads and codes overhang blocks), so the tree is always full 64×64.
    pub fn build(
        src: &[u16],
        s_stride: usize,
        refp: Option<(&[u16], usize)>,
        x0: usize,
        y0: usize,
        w: usize,
        h: usize,
    ) -> VarTree {
        let mut leaves = vec![Var::default(); 64]; // 8×8 grid of 8×8 blocks
        for by in 0..8 {
            for bx in 0..8 {
                let (mut sum, mut sum_sq) = (0i64, 0u64);
                for iy in 0..8 {
                    let sy = (y0 + by * 8 + iy).min(h - 1);
                    for ix in 0..8 {
                        let sx = (x0 + bx * 8 + ix).min(w - 1);
                        let s = src[sy * s_stride + sx] as i64;
                        let d = match refp {
                            Some((rp, rs)) => rp[sy * rs + sx] as i64,
                            None => 0,
                        };
                        let v = s - d;
                        sum += v;
                        sum_sq += (v * v) as u64;
                    }
                }
                leaves[by * 8 + bx] = Var { sum, sum_sq };
            }
        }

        // Aggregate each level from four children of the level below.
        let mut level: [Vec<Var>; 4] = [leaves, Vec::new(), Vec::new(), Vec::new()];
        for l in 1..4 {
            let g = Self::grid(l);
            let cg = Self::grid(l - 1);
            let mut nodes = vec![Var::default(); g * g];
            for r in 0..g {
                for c in 0..g {
                    let (r2, c2) = (r * 2, c * 2);
                    let tl = level[l - 1][r2 * cg + c2];
                    let tr = level[l - 1][r2 * cg + c2 + 1];
                    let bl = level[l - 1][(r2 + 1) * cg + c2];
                    let br = level[l - 1][(r2 + 1) * cg + c2 + 1];
                    nodes[r * g + c] = tl.add(tr).add(bl).add(br);
                }
            }
            level[l] = nodes;
        }
        VarTree { level }
    }

    /// Node `none` variance at `level` (0..3) and node grid position (`r`,`c`).
    #[inline]
    pub fn variance(&self, level: usize, r: usize, c: usize) -> i64 {
        self.level[level][r * Self::grid(level) + c].variance(LOG2_COUNT[level])
    }

    /// The four children `(TL, TR, BL, BR)` of node (`level`,`r`,`c`) — for the
    /// horz/vert split checks. `level` must be ≥ 1.
    #[inline]
    fn children(&self, level: usize, r: usize, c: usize) -> [Var; 4] {
        let cl = level - 1;
        let cg = Self::grid(cl);
        let (r2, c2) = (r * 2, c * 2);
        [
            self.level[cl][r2 * cg + c2],
            self.level[cl][r2 * cg + c2 + 1],
            self.level[cl][(r2 + 1) * cg + c2],
            self.level[cl][(r2 + 1) * cg + c2 + 1],
        ]
    }

    /// `horz[0], horz[1]` (top-row / bottom-row halves) variances of node (level,r,c).
    #[inline]
    pub fn horz_variance(&self, level: usize, r: usize, c: usize) -> (i64, i64) {
        let ch = self.children(level, r, c);
        let lc = LOG2_COUNT[level] - 1; // half the samples
        (ch[0].add(ch[1]).variance(lc), ch[2].add(ch[3]).variance(lc))
    }

    /// `vert[0], vert[1]` (left-col / right-col halves) variances of node (level,r,c).
    #[inline]
    pub fn vert_variance(&self, level: usize, r: usize, c: usize) -> (i64, i64) {
        let ch = self.children(level, r, c);
        let lc = LOG2_COUNT[level] - 1;
        (ch[0].add(ch[2]).variance(lc), ch[1].add(ch[3]).variance(lc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Brute-force per-sample variance over an arbitrary region (the oracle).
    fn brute(src: &[u16], stride: usize, x0: usize, y0: usize, sz: usize) -> i64 {
        let (mut sum, mut sum_sq, n) = (0i64, 0u64, (sz * sz) as u64);
        for y in 0..sz {
            for x in 0..sz {
                let v = src[(y0 + y) * stride + x0 + x] as i64;
                sum += v;
                sum_sq += (v * v) as u64;
            }
        }
        let log2 = n.trailing_zeros();
        let mean_sq = ((sum * sum) as u64) >> log2;
        (256u64.wrapping_mul(sum_sq.saturating_sub(mean_sq)) >> log2) as i64
    }

    #[test]
    fn variance_matches_bruteforce_all_levels() {
        // A deterministic textured 64×64 source (no edge clamping needed).
        let (w, h) = (64usize, 64usize);
        let mut src = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                // A gradient + a higher-frequency ripple so variances differ per region.
                src[y * w + x] =
                    ((x * 3 + y * 5) as u16 & 0xff) ^ (((x / 4 + y / 4) as u16 * 17) & 0x3f);
            }
        }
        let vt = VarTree::build(&src, w, None, 0, 0, w, h);

        // Level 3 (whole 64×64), level 2 (32×32), level 1 (16×16), level 0 (8×8).
        for (level, sz) in [(3usize, 64usize), (2, 32), (1, 16), (0, 8)] {
            let g = 8 >> level;
            for r in 0..g {
                for c in 0..g {
                    let got = vt.variance(level, r, c);
                    let want = brute(&src, w, c * sz, r * sz, sz);
                    assert_eq!(
                        got, want,
                        "variance mismatch level={} r={} c={}",
                        level, r, c
                    );
                }
            }
        }
    }

    #[test]
    fn residual_variance_is_zero_when_ref_equals_src() {
        let (w, h) = (64usize, 64usize);
        let mut src = vec![0u16; w * h];
        for (i, p) in src.iter_mut().enumerate() {
            *p = (i as u16 * 7) & 0xff;
        }
        let refp = src.clone();
        let vt = VarTree::build(&src, w, Some((&refp, w)), 0, 0, w, h);
        // Perfect prediction ⇒ residual is all-zero ⇒ variance 0 everywhere.
        for level in 0..4 {
            let g = 8 >> level;
            for r in 0..g {
                for c in 0..g {
                    assert_eq!(vt.variance(level, r, c), 0);
                }
            }
        }
    }

    #[test]
    fn edge_clamp_fills_full_sb() {
        // A 20×12 frame — the SB (64×64) mostly overhangs; build must not panic and
        // clamped samples equal the last in-frame pixel.
        let (w, h) = (20usize, 12usize);
        let mut src = vec![0u16; w * h];
        for (i, p) in src.iter_mut().enumerate() {
            *p = (i as u16) & 0xff;
        }
        let vt = VarTree::build(&src, w, None, 0, 0, w, h);
        // A leaf fully inside the clamped (constant) region has zero variance.
        let v = vt.variance(0, 7, 7); // bottom-right 8×8, entirely past the frame
        assert_eq!(v, 0);
    }
}
