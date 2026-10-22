//! Floor decode (packet side) and curve synthesis, multiplied straight into the
//! channel's spectrum.
//!
//! Floor 1 follows lewton 0.10.2's `floor_one_decode` /
//! `floor_one_curve_compute_amplitude` / `floor_one_curve_synthesis` exactly —
//! including its integer semantics on out-of-range post values (wrapping `u32`
//! / `i32` arithmetic, clamping only after all posts are predicted), so a
//! hostile packet never panics and a valid one is bit-exact — but with the
//! neighbour search precomputed at setup and the rendered line multiplied into
//! the spectrum in place (no floor vector).
//!
//! Floor 0 (LSP) follows lewton's `floor_zero_decode` / `floor_zero_compute_curve`.

use super::bits::BitReader;
use super::header::{Codebook, Floor0, Floor1};

/// Floor-1 inverse-dB table (Vorbis I §10.1); identical to lewton's
/// `FLOOR1_INVERSE_DB_TABLE` and the encoder's [`crate::floor::FLOOR1_INV_DB`].
use crate::floor::FLOOR1_INV_DB;

/// Outcome of a channel's floor decode.
pub(crate) enum FloorDecode {
    /// Nonzero flag clear, or end of packet inside the floor: channel is silent.
    Unused,
    Used,
    /// Spec "undecodable packet" (floor 0 book number out of range, or a book
    /// without a VQ lookup).
    BadPacket,
}

/// Decode floor-1 post values into `y` (length >= `x_list.len()`).
pub(crate) fn floor1_decode(
    fl: &Floor1,
    books: &[Codebook],
    br: &mut BitReader,
    y: &mut [u32],
) -> FloorDecode {
    if !br.read_bool() || br.eop() {
        return FloorDecode::Unused;
    }
    y[0] = br.read(fl.ybits);
    y[1] = br.read(fl.ybits);
    if br.eop() {
        return FloorDecode::Unused;
    }
    let mut k = 2;
    for &c in &fl.partition_class {
        let c = c as usize;
        let cdim = fl.class_dims[c];
        let cbits = u32::from(fl.class_subclasses[c]);
        let csub = (1u32 << cbits) - 1;
        let mut cval = 0u32;
        if cbits > 0 {
            let book = &books[fl.class_masterbook[c] as usize];
            if !book.usable() {
                return FloorDecode::Unused;
            }
            cval = book.decode(br);
            if br.eop() {
                return FloorDecode::Unused;
            }
        }
        for _ in 0..cdim {
            let b = fl.subclass_books[c][(cval & csub) as usize];
            cval >>= cbits;
            y[k] = if b >= 0 {
                let book = &books[b as usize];
                if !book.usable() {
                    return FloorDecode::Unused;
                }
                let v = book.decode(br);
                if br.eop() {
                    return FloorDecode::Unused;
                }
                v
            } else {
                0
            };
            k += 1;
        }
    }
    FloorDecode::Used
}

/// lewton's `render_point`, with its release-mode wrapping semantics.
#[inline]
fn render_point(x0: u32, y0: u32, x1: u32, y1: u32, x: u32) -> u32 {
    let dy = (y1 as i32).wrapping_sub(y0 as i32);
    let adx = x1.wrapping_sub(x0);
    let ady = dy.wrapping_abs() as u32;
    let err = ady.wrapping_mul(x.wrapping_sub(x0));
    let off = err.checked_div(adx).unwrap_or(0);
    if dy < 0 {
        y0.wrapping_sub(off)
    } else {
        y0.wrapping_add(off)
    }
}

/// Multiply `spec[x0..min(x1, len)]` by the inverse-dB curve of the line from
/// `(x0, y0)` to `(x1, y1)` (lewton's `render_line`; `y` in 0..=255).
#[inline]
fn render_line_mul(x0: usize, y0: i32, x1: usize, y1: i32, spec: &mut [f32]) {
    let end = x1.min(spec.len());
    if x0 >= end {
        return;
    }
    let dy = y1 - y0;
    let adx = (x1 - x0) as i32;
    let base = dy / adx;
    let sy = base + if dy < 0 { -1 } else { 1 };
    let ady = dy.abs() - base.abs() * adx;
    let mut y = y0;
    let mut err = 0i32;
    let seg = &mut spec[x0..end];
    seg[0] *= FLOOR1_INV_DB[(y & 255) as usize];
    for s in &mut seg[1..] {
        err += ady;
        if err >= adx {
            err -= adx;
            y += sy;
        } else {
            y += base;
        }
        *s *= FLOOR1_INV_DB[(y & 255) as usize];
    }
}

/// Floor-1 curve synthesis multiplied into `spec` (length `n/2`).
pub(crate) fn floor1_apply(fl: &Floor1, y: &[u32], spec: &mut [f32]) {
    let np = fl.x_list.len();
    let range = fl.range;
    let mut fy = [0u32; 65];
    let mut step2 = [false; 65];
    fy[0] = y[0];
    fy[1] = y[1];
    step2[0] = true;
    step2[1] = true;
    for i in 2..np {
        let lo = fl.low[i] as usize;
        let hi = fl.high[i] as usize;
        let predicted =
            render_point(fl.x_list[lo], fy[lo], fl.x_list[hi], fy[hi], fl.x_list[i]) as i32;
        let val = y[i] as i32;
        let highroom = range.wrapping_sub(predicted);
        let lowroom = predicted;
        let room = highroom.min(lowroom).wrapping_mul(2);
        if val > 0 {
            step2[lo] = true;
            step2[hi] = true;
            step2[i] = true;
            fy[i] = if val >= room {
                if highroom > lowroom {
                    predicted.wrapping_add(val).wrapping_sub(lowroom)
                } else {
                    predicted
                        .wrapping_sub(val)
                        .wrapping_add(highroom)
                        .wrapping_sub(1)
                }
            } else {
                let t = if val % 2 == 1 { -val - 1 } else { val };
                predicted.wrapping_add(t >> 1)
            } as u32;
        } else {
            fy[i] = predicted as u32;
        }
    }
    for v in &mut fy[..np] {
        *v = (*v).min(range as u32 - 1);
    }
    let mult = fl.multiplier as i32;
    let mut lx = 0usize;
    let mut ly = fy[fl.sorted[0] as usize] as i32 * mult;
    let mut hx = 0usize;
    let mut hy = 0i32;
    for &s in &fl.sorted[1..] {
        let s = s as usize;
        if step2[s] {
            hy = fy[s] as i32 * mult;
            hx = fl.x_list[s] as usize;
            render_line_mul(lx, ly, hx, hy, spec);
            lx = hx;
            ly = hy;
        }
    }
    if hx < spec.len() {
        let g = FLOOR1_INV_DB[(hy & 255) as usize];
        for s in &mut spec[hx..] {
            *s *= g;
        }
    }
}

/// Decode floor-0 LSP coefficients (stored as `cos(coefficient)`, like
/// lewton) into `coeffs`; returns the amplitude, or the special case.
pub(crate) fn floor0_decode(
    fl: &Floor0,
    books: &[Codebook],
    br: &mut BitReader,
    coeffs: &mut Vec<f32>,
) -> Result<u64, FloorDecode> {
    let amplitude = br.read_u64(fl.amp_bits);
    if br.eop() || amplitude == 0 {
        return Err(FloorDecode::Unused);
    }
    let booknum = br.read(fl.book_bits) as usize;
    if br.eop() {
        return Err(FloorDecode::Unused);
    }
    let Some(&bi) = fl.books.get(booknum) else {
        return Err(FloorDecode::BadPacket);
    };
    let book = &books[bi as usize];
    if !book.usable() {
        return Err(FloorDecode::BadPacket);
    }
    let order = fl.order as usize;
    coeffs.clear();
    let mut last = 0.0f32;
    loop {
        let e = book.decode(br);
        // lewton's order: end of packet inside the codeword first (floor
        // unused), only then "book has no VQ lookup" (undecodable packet).
        if br.eop() {
            return Err(FloorDecode::Unused);
        }
        if !book.has_vq {
            return Err(FloorDecode::BadPacket);
        }
        let mut last_new = last;
        for &v in book.row(e) {
            coeffs.push((last + v).cos());
            last_new = v;
            if coeffs.len() == order {
                return Ok(amplitude);
            }
        }
        last += last_new;
        if coeffs.len() >= order {
            return Ok(amplitude);
        }
    }
}

/// Floor-0 curve synthesis (lewton's `floor_zero_compute_curve`) multiplied
/// into `spec`; `cos_omega` has `spec.len()` entries.
pub(crate) fn floor0_apply(
    fl: &Floor0,
    coeffs: &[f32],
    amplitude: u64,
    cos_omega: &[f32],
    spec: &mut [f32],
) {
    let order = fl.order as usize;
    let denom = ((1u64 << fl.amp_bits) - 1) as f32;
    let lfv_common = amplitude as f32 * fl.amp_offset as f32 / denom;
    let n = spec.len().min(cos_omega.len());
    let mut i = 0;
    while i < n {
        let cw = cos_omega[i];
        let (mut p, mut q) = if order & 1 == 1 {
            (1.0 - cw * cw, 0.25)
        } else {
            ((1.0 - cw) / 2.0, (1.0 + cw) / 2.0)
        };
        let mut j = 1;
        while j < order {
            let pm = coeffs[j] - cw;
            p *= 4.0 * pm * pm;
            j += 2;
        }
        let mut j = 0;
        while j < order {
            let qm = coeffs[j] - cw;
            q *= 4.0 * qm * qm;
            j += 2;
        }
        let lfv = (0.115_129_25 * (lfv_common / (p + q).sqrt() - fl.amp_offset as f32)).exp();
        // Runs of equal cos(omega) share the value. Always advance at least one
        // bin: a degenerate header (rate or bark map size 0) yields NaN here.
        spec[i] *= lfv;
        i += 1;
        while i < n && cos_omega[i] == cw {
            spec[i] *= lfv;
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// lewton 0.10.2 `floor_one_curve_compute_amplitude` + `floor_one_curve_synthesis`,
    /// transcribed (O(n^2) neighbour search, full floor vector) as the oracle.
    fn lewton_floor1(fl: &Floor1, y: &[u32], n: usize) -> Vec<f32> {
        let x = &fl.x_list;
        let low = |i: usize| -> (usize, u32) {
            let mut best: Option<(usize, u32)> = None;
            for (k, &v) in x[..i].iter().enumerate() {
                if v < x[i] && best.is_none_or(|b| v > b.1) {
                    best = Some((k, v));
                }
            }
            best.unwrap()
        };
        let high = |i: usize| -> (usize, u32) {
            let mut best: Option<(usize, u32)> = None;
            for (k, &v) in x[..i].iter().enumerate() {
                if v > x[i] && best.is_none_or(|b| v < b.1) {
                    best = Some((k, v));
                }
            }
            best.unwrap()
        };
        let range = fl.range;
        let mut step2 = vec![true, true];
        let mut fy = vec![y[0], y[1]];
        for i in 2..x.len() {
            let (lo, lx) = low(i);
            let (hi, hx) = high(i);
            let predicted = render_point(lx, fy[lo], hx, fy[hi], x[i]) as i32;
            let val = y[i] as i32;
            let highroom = range - predicted;
            let lowroom = predicted;
            let room = highroom.min(lowroom) * 2;
            if val > 0 {
                step2[lo] = true;
                step2[hi] = true;
                step2.push(true);
                fy.push(if val >= room {
                    if highroom > lowroom {
                        predicted + val - lowroom
                    } else {
                        predicted - val + highroom - 1
                    }
                } else {
                    predicted + (if val % 2 == 1 { -val - 1 } else { val } >> 1)
                } as u32);
            } else {
                fy.push(predicted as u32);
                step2.push(false);
            }
        }
        for v in &mut fy {
            *v = (*v).min(range as u32 - 1);
        }
        let mut sorted: Vec<(usize, u32)> = x.iter().copied().enumerate().collect();
        sorted.sort_by_key(|a| a.1);
        let mut floor: Vec<u32> = Vec::new();
        let render_line = |x0: u32, y0: u32, x1: u32, y1: u32, v: &mut Vec<u32>| {
            let dy = y1 as i32 - y0 as i32;
            let adx = x1 as i32 - x0 as i32;
            let base = dy / adx;
            let mut y = y0 as i32;
            let mut err = 0;
            let sy = base + if dy < 0 { -1 } else { 1 };
            let ady = dy.abs() - base.abs() * adx;
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
        };
        let (mut hx, mut lx, mut hy) = (0u32, 0u32, 0u32);
        let mut ly = fy[sorted[0].0] * fl.multiplier;
        for &(idx, xv) in &sorted[1..] {
            if step2[idx] {
                hy = fy[idx] * fl.multiplier;
                hx = xv;
                render_line(lx, ly, hx, hy, &mut floor);
                lx = hx;
                ly = hy;
            }
        }
        if hx < n as u32 {
            render_line(hx, hy, n as u32, hy, &mut floor);
        } else if hx > n as u32 {
            floor.truncate(n);
        }
        floor
            .into_iter()
            .map(|i| FLOOR1_INV_DB[i as usize])
            .collect()
    }

    fn make_floor(x_list: Vec<u32>, multiplier: u32) -> Floor1 {
        let n = x_list.len();
        let mut sorted: Vec<u8> = (0..n as u8).collect();
        sorted.sort_by_key(|&i| x_list[i as usize]);
        let mut low = vec![0u8; n];
        let mut high = vec![0u8; n];
        for i in 2..n {
            let (mut lo, mut hi, mut lx, mut hx) = (0, 1, 0, u32::MAX);
            for k in 0..i {
                let xk = x_list[k];
                if xk < x_list[i] && xk >= lx {
                    lo = k;
                    lx = xk;
                }
                if xk > x_list[i] && xk < hx {
                    hi = k;
                    hx = xk;
                }
            }
            low[i] = lo as u8;
            high[i] = hi as u8;
        }
        let range = [256, 128, 86, 64][(multiplier - 1) as usize];
        Floor1 {
            partition_class: Vec::new(),
            class_dims: [0; 16],
            class_subclasses: [0; 16],
            class_masterbook: [0; 16],
            subclass_books: [[-1; 8]; 16],
            multiplier,
            range,
            ybits: 8,
            x_list,
            low,
            high,
            sorted,
        }
    }

    #[test]
    fn floor1_matches_lewton_transcription() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for case in 0..3000 {
            let rangebits = 4 + next(7) as u32;
            let nposts = 2 + next(20) as usize;
            let mut xs = vec![0u32, 1 << rangebits];
            while xs.len() < nposts.min((1 << rangebits) as usize) {
                let v = next(1 << rangebits) as u32;
                if !xs.contains(&v) {
                    xs.push(v);
                }
            }
            let mult = 1 + next(4) as u32;
            let fl = make_floor(xs, mult);
            let range = fl.range as u64;
            // Values within the legal coded range (no wrap-around).
            let y: Vec<u32> = (0..fl.x_list.len())
                .map(|i| {
                    if i < 2 {
                        next(range) as u32
                    } else {
                        next(range) as u32 * (next(3) as u32 / 2)
                    }
                })
                .collect();
            let n = [16usize, 32, 64, 128, 1024][next(5) as usize];
            let want = lewton_floor1(&fl, &y, n);
            let mut spec = vec![1.0f32; n];
            floor1_apply(&fl, &y, &mut spec);
            assert_eq!(spec, want, "case {case}: x={:?} y={y:?} n={n}", fl.x_list);
        }
    }
}
