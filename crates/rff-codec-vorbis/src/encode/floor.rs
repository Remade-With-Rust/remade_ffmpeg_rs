//! Floor-1 encode (brick 3): fit a real piecewise-linear spectral envelope.
//!
//! Inverts lewton's floor-1 decode + synthesis. Each post is fit to the local spectral
//! magnitude, then coded *differentially* against the value predicted from its bracketing
//! neighbours (the inverse of `floor_one_curve_compute_amplitude`) and huffman-coded through
//! the class / subclass / masterbook structure. The exact reconstructed curve is returned so
//! the caller divides it out of the spectrum to form a well-conditioned residue.

use rff_core::{Error, Result};

use super::setup::{Codebook, Floor1};
use super::BitWriter;

/// Floor-1 inverse-dB table (index 0..255 → linear amplitude); matches lewton's
/// `FLOOR1_INVERSE_DB_TABLE`.
#[rustfmt::skip]
#[allow(clippy::excessive_precision)] // verbatim spec table; extra digits round to f32
pub const FLOOR1_INV_DB: [f32; 256] = [
    1.0649863e-07, 1.1341951e-07, 1.2079015e-07, 1.2863978e-07, 1.3699951e-07, 1.4590251e-07,
    1.5538408e-07, 1.6548181e-07, 1.7623575e-07, 1.8768855e-07, 1.9988561e-07, 2.1287530e-07,
    2.2670913e-07, 2.4144197e-07, 2.5713223e-07, 2.7384213e-07, 2.9163793e-07, 3.1059021e-07,
    3.3077411e-07, 3.5226968e-07, 3.7516214e-07, 3.9954229e-07, 4.2550680e-07, 4.5315863e-07,
    4.8260743e-07, 5.1396998e-07, 5.4737065e-07, 5.8294187e-07, 6.2082472e-07, 6.6116941e-07,
    7.0413592e-07, 7.4989464e-07, 7.9862701e-07, 8.5052630e-07, 9.0579828e-07, 9.6466216e-07,
    1.0273513e-06, 1.0941144e-06, 1.1652161e-06, 1.2409384e-06, 1.3215816e-06, 1.4074654e-06,
    1.4989305e-06, 1.5963394e-06, 1.7000785e-06, 1.8105592e-06, 1.9282195e-06, 2.0535261e-06,
    2.1869758e-06, 2.3290978e-06, 2.4804557e-06, 2.6416497e-06, 2.8133190e-06, 2.9961443e-06,
    3.1908506e-06, 3.3982101e-06, 3.6190449e-06, 3.8542308e-06, 4.1047004e-06, 4.3714470e-06,
    4.6555282e-06, 4.9580707e-06, 5.2802740e-06, 5.6234160e-06, 5.9888572e-06, 6.3780469e-06,
    6.7925283e-06, 7.2339451e-06, 7.7040476e-06, 8.2047000e-06, 8.7378876e-06, 9.3057248e-06,
    9.9104632e-06, 1.0554501e-05, 1.1240392e-05, 1.1970856e-05, 1.2748789e-05, 1.3577278e-05,
    1.4459606e-05, 1.5399272e-05, 1.6400004e-05, 1.7465768e-05, 1.8600792e-05, 1.9809576e-05,
    2.1096914e-05, 2.2467911e-05, 2.3928002e-05, 2.5482978e-05, 2.7139006e-05, 2.8902651e-05,
    3.0780908e-05, 3.2781225e-05, 3.4911534e-05, 3.7180282e-05, 3.9596466e-05, 4.2169667e-05,
    4.4910090e-05, 4.7828601e-05, 5.0936773e-05, 5.4246931e-05, 5.7772202e-05, 6.1526565e-05,
    6.5524908e-05, 6.9783085e-05, 7.4317983e-05, 7.9147585e-05, 8.4291040e-05, 8.9768747e-05,
    9.5602426e-05, 0.00010181521, 0.00010843174, 0.00011547824, 0.00012298267, 0.00013097477,
    0.00013948625, 0.00014855085, 0.00015820453, 0.00016848555, 0.00017943469, 0.00019109536,
    0.00020351382, 0.00021673929, 0.00023082423, 0.00024582449, 0.00026179955, 0.00027881276,
    0.00029693158, 0.00031622787, 0.00033677814, 0.00035866388, 0.00038197188, 0.00040679456,
    0.00043323036, 0.00046138411, 0.00049136745, 0.00052329927, 0.00055730621, 0.00059352311,
    0.00063209358, 0.00067317058, 0.00071691700, 0.00076350630, 0.00081312324, 0.00086596457,
    0.00092223983, 0.00098217216, 0.0010459992,  0.0011139742,  0.0011863665,  0.0012634633,
    0.0013455702,  0.0014330129,  0.0015261382,  0.0016253153,  0.0017309374,  0.0018434235,
    0.0019632195,  0.0020908006,  0.0022266726,  0.0023713743,  0.0025254795,  0.0026895994,
    0.0028643847,  0.0030505286,  0.0032487691,  0.0034598925,  0.0036847358,  0.0039241906,
    0.0041792066,  0.0044507950,  0.0047400328,  0.0050480668,  0.0053761186,  0.0057254891,
    0.0060975636,  0.0064938176,  0.0069158225,  0.0073652516,  0.0078438871,  0.0083536271,
    0.0088964928,  0.009474637,   0.010090352,   0.010746080,   0.011444421,   0.012188144,
    0.012980198,   0.013823725,   0.014722068,   0.015678791,   0.016697687,   0.017782797,
    0.018938423,   0.020169149,   0.021479854,   0.022875735,   0.024362330,   0.025945531,
    0.027631618,   0.029427276,   0.031339626,   0.033376252,   0.035545228,   0.037855157,
    0.040315199,   0.042935108,   0.045725273,   0.048696758,   0.051861348,   0.055231591,
    0.058820850,   0.062643361,   0.066714279,   0.071049749,   0.075666962,   0.080584227,
    0.085821044,   0.091398179,   0.097337747,   0.10366330,    0.11039993,    0.11757434,
    0.12521498,    0.13335215,    0.14201813,    0.15124727,    0.16107617,    0.17154380,
    0.18269168,    0.19456402,    0.20720788,    0.22067342,    0.23501402,    0.25028656,
    0.26655159,    0.28387361,    0.30232132,    0.32196786,    0.34289114,    0.36517414,
    0.38890521,    0.41417847,    0.44109412,    0.46975890,    0.50028648,    0.53279791,
    0.56742212,    0.60429640,    0.64356699,    0.68538959,    0.72993007,    0.77736504,
    0.82788260,    0.88168307,    0.9389798,     1.0,
];

const RANGES: [i32; 4] = [256, 128, 86, 64];

/// Nearest earlier post with x strictly below `x_list[i]` (index, x).
fn low_neighbor(x_list: &[u32], i: usize) -> (usize, u32) {
    let bound = x_list[i];
    let mut best = (0usize, 0u32);
    let mut found = false;
    for (k, &x) in x_list[..i].iter().enumerate() {
        if x < bound && (!found || x > best.1) {
            best = (k, x);
            found = true;
        }
    }
    best
}

/// Nearest earlier post with x strictly above `x_list[i]` (index, x).
fn high_neighbor(x_list: &[u32], i: usize) -> (usize, u32) {
    let bound = x_list[i];
    let mut best = (0usize, 0u32);
    let mut found = false;
    for (k, &x) in x_list[..i].iter().enumerate() {
        if x > bound && (!found || x < best.1) {
            best = (k, x);
            found = true;
        }
    }
    best
}

/// Linear-interpolate the post value at `x` between `(x0,y0)` and `(x1,y1)` (lewton's
/// `render_point`).
fn render_point(x0: u32, y0: u32, x1: u32, y1: u32, x: u32) -> u32 {
    let dy = y1 as i32 - y0 as i32;
    let adx = x1 - x0;
    let ady = dy.unsigned_abs();
    let off = ady * (x - x0) / adx;
    if dy < 0 {
        y0 - off
    } else {
        y0 + off
    }
}

/// Rasterize a floor line segment into `v` (lewton's `render_line`).
fn render_line(x0: u32, y0: u32, x1: u32, y1: u32, v: &mut Vec<u32>) {
    let dy = y1 as i32 - y0 as i32;
    let adx = x1 as i32 - x0 as i32;
    let mut ady = dy.abs();
    let base = dy / adx;
    let mut y = y0 as i32;
    let mut err = 0i32;
    let sy = base + if dy < 0 { -1 } else { 1 };
    ady -= base.abs() * adx;
    v.push(y as u32);
    for _ in (x0 + 1)..x1 {
        err += ady;
        if err >= adx {
            err -= adx;
            y += sy;
        } else {
            y += base;
        }
        v.push(y as u32);
    }
}

/// Decode a coded floor residual `val` back to a final post value, given the neighbour
/// prediction (the per-post branch of `floor_one_curve_compute_amplitude`).
fn decode_val(val: i32, predicted: i32, range: i32) -> i32 {
    if val <= 0 {
        return predicted;
    }
    let highroom = range - predicted;
    let lowroom = predicted;
    let room = highroom.min(lowroom) * 2;
    if val >= room {
        if highroom > lowroom {
            predicted + val - lowroom
        } else {
            predicted - val + highroom - 1
        }
    } else {
        let temp = if val % 2 == 1 { -val - 1 } else { val };
        predicted + (temp >> 1)
    }
}

/// Code a desired post value as a residual against `predicted` (exact inverse of
/// `decode_val`). `target` is assumed clamped to `[0, range)`.
fn encode_val(target: i32, predicted: i32, range: i32) -> u32 {
    let diff = target - predicted;
    if diff == 0 {
        return 0;
    }
    let highroom = range - predicted;
    let lowroom = predicted;
    let room = highroom.min(lowroom) * 2;
    let zz = if diff > 0 { 2 * diff } else { -2 * diff - 1 };
    if zz < room {
        return zz as u32;
    }
    if highroom > lowroom {
        (diff + lowroom) as u32
    } else {
        (highroom - 1 - diff) as u32
    }
}

fn can_encode(book: &Codebook, e: u32) -> bool {
    (e as usize) < book.lengths.len() && book.lengths[e as usize] > 0
}

/// Find the largest `val' <= val` a subclass book of class `c` can encode, and which
/// subclass carries it. `val' == 0` always works (a `-1` subclass, or entry 0).
fn fit_val_to_books(fl: &Floor1, codebooks: &[Codebook], c: usize, val: u32) -> (u32, usize) {
    let books = &fl.subclass_books[c];
    let mut v = val;
    loop {
        for (sc, &book) in books.iter().enumerate() {
            if v == 0 {
                if book < 0 || can_encode(&codebooks[book as usize], 0) {
                    return (0, sc);
                }
            } else if book >= 0 && can_encode(&codebooks[book as usize], v) {
                return (v, sc);
            }
        }
        if v == 0 {
            return (0, 0);
        }
        v -= 1;
    }
}

/// Nearest floor post value whose curve amplitude best matches `mag`.
fn db_index_for(mag: f32, multiplier: u8, range: i32) -> u32 {
    let max_y = (range as u32).min(256 / multiplier as u32);
    let mut best = 0u32;
    let mut best_d = f32::INFINITY;
    for y in 0..max_y {
        let d = (FLOOR1_INV_DB[(y * multiplier as u32) as usize] - mag).abs();
        if d < best_d {
            best_d = d;
            best = y;
        }
    }
    best
}

/// Which class each post index (`>= 2`) belongs to, from the partition layout.
fn post_classes(fl: &Floor1) -> Vec<usize> {
    let mut classes = vec![usize::MAX; fl.x_list.len()];
    let mut idx = 2;
    for &pc in &fl.partition_class {
        let c = pc as usize;
        for _ in 0..fl.class_dimensions[c] {
            classes[idx] = c;
            idx += 1;
        }
    }
    classes
}

/// Fit + encode a floor-1 for one channel, emit its bits, and return the reconstructed
/// curve (length `m`). `spectrum` is the channel's `m` MDCT coefficients.
pub fn fit_and_encode_floor(
    bw: &mut BitWriter,
    target_mag: &[f32],
    fl: &Floor1,
    codebooks: &[Codebook],
    m: usize,
) -> Result<Vec<f32>> {
    let range = RANGES[(fl.multiplier - 1) as usize];
    let b = 32 - (range as u32 - 1).leading_zeros();
    let nposts = fl.x_list.len();
    let classes = post_classes(fl);

    // Fit each post to the (masking-threshold) target magnitude at its bin.
    let target: Vec<i32> = (0..nposts)
        .map(|i| {
            let bin = (fl.x_list[i] as usize).min(m - 1);
            db_index_for(target_mag[bin], fl.multiplier, range).min(range as u32 - 1) as i32
        })
        .collect();

    // Incremental differential coding: mirrors compute_amplitude but derives each coded
    // residual from the fit, then commits the reconstructed final_y + step2 flags.
    let mut floor1_y = vec![0u32; nposts];
    let mut subclass = vec![0usize; nposts];
    let mut final_y = vec![0i32; nposts];
    let mut step2 = vec![false; nposts];
    final_y[0] = target[0];
    final_y[1] = target[1];
    step2[0] = true;
    step2[1] = true;
    floor1_y[0] = target[0] as u32;
    floor1_y[1] = target[1] as u32;

    for i in 2..nposts {
        let (li, lx) = low_neighbor(&fl.x_list, i);
        let (hi_i, hx) = high_neighbor(&fl.x_list, i);
        let predicted =
            render_point(lx, final_y[li] as u32, hx, final_y[hi_i] as u32, fl.x_list[i]) as i32;
        let raw = encode_val(target[i], predicted, range);
        let (val, sc) = fit_val_to_books(fl, codebooks, classes[i], raw);
        floor1_y[i] = val;
        subclass[i] = sc;
        if val > 0 {
            step2[li] = true;
            step2[hi_i] = true;
            step2[i] = true;
            final_y[i] = decode_val(val as i32, predicted, range).clamp(0, range - 1);
        } else {
            final_y[i] = predicted.clamp(0, range - 1);
        }
    }

    // Emit: used flag, posts 0/1, then per-partition masterbook cval + subclass post values.
    bw.write(1, 1);
    bw.write(floor1_y[0], b);
    bw.write(floor1_y[1], b);
    let mut idx = 2;
    for &pc in &fl.partition_class {
        let c = pc as usize;
        let cdim = fl.class_dimensions[c] as usize;
        let cbits = fl.class_subclasses[c] as u32;
        if cbits > 0 {
            let mut cval = 0u32;
            for p in 0..cdim {
                cval |= (subclass[idx + p] as u32) << (p as u32 * cbits);
            }
            write_entry(bw, &codebooks[fl.class_masterbooks[c] as usize], cval)?;
        }
        for p in 0..cdim {
            let book = fl.subclass_books[c][subclass[idx + p]];
            if book >= 0 {
                write_entry(bw, &codebooks[book as usize], floor1_y[idx + p])?;
            }
        }
        idx += cdim;
    }

    Ok(synthesize_curve(&final_y, &step2, fl, m))
}

fn write_entry(bw: &mut BitWriter, book: &Codebook, e: u32) -> Result<()> {
    let (cw, len) = book.encode(e);
    if len == 0 {
        return Err(Error::invalid("vorbis floor: tried to emit an unused codebook entry"));
    }
    bw.write(cw, len as u32);
    Ok(())
}

/// Render the floor curve from the final post values + step2 flags (lewton's
/// `floor_one_curve_synthesis`), mapped through the inverse-dB table.
fn synthesize_curve(final_y: &[i32], step2: &[bool], fl: &Floor1, n: usize) -> Vec<f32> {
    // Posts sorted by x.
    let mut order: Vec<usize> = (0..fl.x_list.len()).collect();
    order.sort_by_key(|&i| fl.x_list[i]);

    let mult = fl.multiplier as i32;
    let mut idx = vec![0u32; 0];
    let mut lx = 0u32;
    let mut ly = (final_y[order[0]] * mult) as u32;
    let mut hx = 0u32;
    let mut hy = 0u32;
    for &oi in order.iter().skip(1) {
        if step2[oi] {
            hy = (final_y[oi] * mult) as u32;
            hx = fl.x_list[oi];
            render_line(lx, ly, hx, hy, &mut idx);
            lx = hx;
            ly = hy;
        }
    }
    if (hx as usize) < n {
        render_line(hx, hy, n as u32, hy, &mut idx);
    } else if hx as usize > n {
        idx.truncate(n);
    }
    idx.truncate(n);
    idx.into_iter()
        .map(|i| FLOOR1_INV_DB[(i as usize).min(255)])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Differential coding must round-trip for every (predicted, target) within range.
    #[test]
    fn encode_val_roundtrips() {
        for &range in &RANGES {
            for predicted in 0..range {
                for target in 0..range {
                    let val = encode_val(target, predicted, range);
                    let back = decode_val(val as i32, predicted, range).clamp(0, range - 1);
                    assert_eq!(back, target, "range={range} predicted={predicted} target={target}");
                }
            }
        }
    }

    #[test]
    fn neighbors_match_examples() {
        let v = [1u32, 4, 2, 3, 6, 5];
        assert_eq!(low_neighbor(&v, 3), (2, 2));
        assert_eq!(low_neighbor(&v, 4), (1, 4));
        assert_eq!(high_neighbor(&v, 2), (1, 4));
        assert_eq!(high_neighbor(&v, 5), (4, 6));
    }
}
