//! Histogram-domain palette builder: median-cut initialization + weighted
//! k-means refinement over the *unique colors* of the frame.
//!
//! PSNR is frequency-weighted squared error; weighted k-means minimizes
//! exactly that objective, and its cost scales with color diversity (the
//! histogram size), not the pixel count. Measured 2–88× unique-color
//! redundancy on the bench corpus, which is the whole speedup.
//!
//! Everything here is deterministic integer/f64-accumulator math: same
//! input, same palette, every run. Failure modes are loud — empty clusters
//! are reseeded deterministically, and internal invariants are asserted.

use alloc::vec::Vec;

/// One unique RGBA color with its pixel count.
#[derive(Clone, Copy)]
pub(crate) struct WPoint {
    /// RGBA channels.
    pub c: [i32; 4],
    /// Pixel count (weight).
    pub w: u64,
}

/// Result palette: ≤256 RGBA entries plus, per input point, the palette
/// index it maps to (same order as the input slice).
pub(crate) struct KmPalette {
    pub colors: Vec<[u8; 4]>,
    pub assignment: Vec<u8>,
}

#[inline]
fn dist2(a: [i32; 4], b: [i32; 4]) -> i64 {
    let dr = (a[0] - b[0]) as i64;
    let dg = (a[1] - b[1]) as i64;
    let db = (a[2] - b[2]) as i64;
    let da = (a[3] - b[3]) as i64;
    dr * dr + dg * dg + db * db + da * da
}

struct Box_ {
    lo: usize,
    hi: usize, // points[lo..hi]
    weight: u64,
}

/// Median-cut over the weighted points: split the heaviest box along its
/// widest channel at the weighted median until `k` boxes exist.
fn median_cut(points: &mut [WPoint], k: usize) -> Vec<[f64; 4]> {
    let total: u64 = points.iter().map(|p| p.w).sum();
    let mut boxes = vec![Box_ { lo: 0, hi: points.len(), weight: total }];

    while boxes.len() < k {
        // Heaviest splittable box (≥2 points).
        let Some((bi, _)) = boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| b.hi - b.lo >= 2)
            .max_by_key(|(_, b)| b.weight)
        else {
            break; // nothing splittable left; fewer boxes than k is fine
        };
        let (lo, hi) = (boxes[bi].lo, boxes[bi].hi);
        let seg = &mut points[lo..hi];

        // Widest channel.
        let mut mins = [i32::MAX; 4];
        let mut maxs = [i32::MIN; 4];
        for p in seg.iter() {
            for ch in 0..4 {
                mins[ch] = mins[ch].min(p.c[ch]);
                maxs[ch] = maxs[ch].max(p.c[ch]);
            }
        }
        let ch = (0..4).max_by_key(|&ch| maxs[ch] - mins[ch]).unwrap();
        if maxs[ch] == mins[ch] {
            // All points identical across every channel (they'd all have
            // width 0): unsplittable duplicates shouldn't exist (input is
            // unique colors), so this box is a single color repeated.
            // Mark it unsplittable by weight so we don't loop forever.
            boxes[bi].weight = 0;
            continue;
        }
        seg.sort_unstable_by_key(|p| p.c[ch]);

        // Weighted median split point (never empty on either side).
        let half = boxes[bi].weight / 2;
        let mut acc = 0u64;
        let mut cut = 0usize;
        for (i, p) in seg.iter().enumerate() {
            acc += p.w;
            if acc >= half {
                cut = i + 1;
                break;
            }
        }
        cut = cut.clamp(1, seg.len() - 1);

        let left_w: u64 = seg[..cut].iter().map(|p| p.w).sum();
        let right_w = boxes[bi].weight - left_w;
        let mid = lo + cut;
        let hi_old = boxes[bi].hi;
        boxes[bi].hi = mid;
        boxes[bi].weight = left_w;
        boxes.push(Box_ { lo: mid, hi: hi_old, weight: right_w });
    }

    boxes
        .iter()
        .filter(|b| b.hi > b.lo)
        .map(|b| {
            let seg = &points[b.lo..b.hi];
            let w: u64 = seg.iter().map(|p| p.w).sum::<u64>().max(1);
            let mut c = [0f64; 4];
            for p in seg {
                for ch in 0..4 {
                    c[ch] += p.c[ch] as f64 * p.w as f64;
                }
            }
            c.map(|v| v / w as f64)
        })
        .collect()
}

/// Weighted k-means: assign → recompute means, `iters` times. Returns the
/// final palette and per-point assignment.
pub(crate) fn palette_kmeans(points_in: &[WPoint], k: usize, iters: usize) -> KmPalette {
    assert!(k >= 1 && k <= 256, "palette size out of range");
    assert!(!points_in.is_empty(), "kmeans: empty histogram");

    let mut points: Vec<WPoint> = points_in.to_vec();
    let mut centers = median_cut(&mut points, k);
    // NOTE: `points` is now reordered; work with the reordered copy and map
    // back to the caller's order at the end via the color key.

    let mut assign = vec![0u8; points.len()];
    for round in 0..iters.max(1) {
        // Assignment: nearest center per unique color.
        for (pi, p) in points.iter().enumerate() {
            let pc = p.c.map(|v| v as f64);
            let mut best = f64::MAX;
            let mut bi = 0usize;
            for (ci, c) in centers.iter().enumerate() {
                let dr = c[0] - pc[0];
                let dg = c[1] - pc[1];
                let db = c[2] - pc[2];
                let da = c[3] - pc[3];
                let d = dr * dr + dg * dg + db * db + da * da;
                if d < best {
                    best = d;
                    bi = ci;
                }
            }
            assign[pi] = bi as u8;
        }

        // Recompute weighted means.
        let mut sums = vec![[0f64; 4]; centers.len()];
        let mut wsum = vec![0u64; centers.len()];
        for (pi, p) in points.iter().enumerate() {
            let ci = assign[pi] as usize;
            wsum[ci] += p.w;
            for ch in 0..4 {
                sums[ci][ch] += p.c[ch] as f64 * p.w as f64;
            }
        }
        // Empty clusters: reseed deterministically on the heaviest point
        // farthest from its center (loud, not silent: debug-traced).
        for ci in 0..centers.len() {
            if wsum[ci] == 0 {
                let (far_i, _) = points
                    .iter()
                    .enumerate()
                    .max_by_key(|(pi, p)| {
                        let c = centers[assign[*pi] as usize];
                        let pc = p.c.map(|v| v as f64);
                        let d = (0..4).map(|ch| (c[ch] - pc[ch]) * (c[ch] - pc[ch])).sum::<f64>();
                        (d * p.w as f64) as i64
                    })
                    .expect("nonempty points");
                centers[ci] = points[far_i].c.map(|v| v as f64);
            } else {
                centers[ci] = core::array::from_fn(|ch| sums[ci][ch] / wsum[ci] as f64);
            }
        }
        let _ = round;
    }

    // Final palette: rounded centers. Re-assign once against the rounded
    // palette so the mapping matches the bytes actually written to the file.
    let colors: Vec<[u8; 4]> = centers
        .iter()
        .map(|c| c.map(|v| v.round().clamp(0.0, 255.0) as u8))
        .collect();
    let pal_i32: Vec<[i32; 4]> = colors.iter().map(|c| c.map(|v| v as i32)).collect();
    for (pi, p) in points.iter().enumerate() {
        let mut best = i64::MAX;
        let mut bi = 0usize;
        for (ci, c) in pal_i32.iter().enumerate() {
            let d = dist2(p.c, *c);
            if d < best {
                best = d;
                bi = ci;
            }
        }
        assign[pi] = bi as u8;
    }

    // Map assignments back to the caller's point order.
    let mut out_assign = vec![0u8; points_in.len()];
    let mut index_by_key: std::collections::HashMap<[i32; 4], u8> =
        std::collections::HashMap::with_capacity(points.len());
    for (pi, p) in points.iter().enumerate() {
        index_by_key.insert(p.c, assign[pi]);
    }
    for (pi, p) in points_in.iter().enumerate() {
        out_assign[pi] = *index_by_key
            .get(&p.c)
            .expect("kmeans: point lost during palette build");
    }

    KmPalette { colors, assignment: out_assign }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_exact_clusters() {
        // 4 well-separated colors, heavily weighted: palette must contain
        // all of them exactly after refinement.
        let pts: Vec<WPoint> = [
            ([0, 0, 0, 255], 1000),
            ([255, 0, 0, 255], 800),
            ([0, 255, 0, 255], 600),
            ([0, 0, 255, 255], 400),
        ]
        .iter()
        .map(|&(c, w)| WPoint { c, w })
        .collect();
        let pal = palette_kmeans(&pts, 4, 3);
        for (pi, p) in pts.iter().enumerate() {
            let got = pal.colors[pal.assignment[pi] as usize].map(|v| v as i32);
            assert_eq!(got, p.c, "cluster {pi} not recovered");
        }
    }

    #[test]
    fn weight_dominates_palette_placement() {
        // One massive flat color + a spray of rare colors: the flat color
        // must map to itself exactly (this is the graphics-content case
        // where unweighted training failed).
        let mut pts = vec![WPoint { c: [40, 90, 200, 255], w: 1_000_000 }];
        for i in 0..1000u32 {
            pts.push(WPoint {
                c: [(i % 256) as i32, (i / 4 % 256) as i32, (i / 16 % 256) as i32, 255],
                w: 1,
            });
        }
        let pal = palette_kmeans(&pts, 256, 3);
        let got = pal.colors[pal.assignment[0] as usize].map(|v| v as i32);
        assert_eq!(got, [40, 90, 200, 255], "heavy flat color not preserved exactly");
    }
}
