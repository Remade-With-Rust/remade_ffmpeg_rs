static CONST1: i64 = 20091;
static CONST2: i64 = 35468;

pub(crate) fn idct4x4(block: &mut [i32]) {
    // The intermediate results may overflow the types, so we stretch the type.
    fn fetch(block: &[i32], idx: usize) -> i64 {
        i64::from(block[idx])
    }

    // Perform one lenght check up front to avoid subsequent bounds checks in this function
    assert!(block.len() >= 16);

    for i in 0usize..4 {
        let a1 = fetch(block, i) + fetch(block, 8 + i);
        let b1 = fetch(block, i) - fetch(block, 8 + i);

        let t1 = (fetch(block, 4 + i) * CONST2) >> 16;
        let t2 = fetch(block, 12 + i) + ((fetch(block, 12 + i) * CONST1) >> 16);
        let c1 = t1 - t2;

        let t1 = fetch(block, 4 + i) + ((fetch(block, 4 + i) * CONST1) >> 16);
        let t2 = (fetch(block, 12 + i) * CONST2) >> 16;
        let d1 = t1 + t2;

        block[i] = (a1 + d1) as i32;
        block[4 + i] = (b1 + c1) as i32;
        block[4 * 3 + i] = (a1 - d1) as i32;
        block[4 * 2 + i] = (b1 - c1) as i32;
    }

    for i in 0usize..4 {
        let a1 = fetch(block, 4 * i) + fetch(block, 4 * i + 2);
        let b1 = fetch(block, 4 * i) - fetch(block, 4 * i + 2);

        let t1 = (fetch(block, 4 * i + 1) * CONST2) >> 16;
        let t2 = fetch(block, 4 * i + 3) + ((fetch(block, 4 * i + 3) * CONST1) >> 16);
        let c1 = t1 - t2;

        let t1 = fetch(block, 4 * i + 1) + ((fetch(block, 4 * i + 1) * CONST1) >> 16);
        let t2 = (fetch(block, 4 * i + 3) * CONST2) >> 16;
        let d1 = t1 + t2;

        block[4 * i] = ((a1 + d1 + 4) >> 3) as i32;
        block[4 * i + 3] = ((a1 - d1 + 4) >> 3) as i32;
        block[4 * i + 1] = ((b1 + c1 + 4) >> 3) as i32;
        block[4 * i + 2] = ((b1 - c1 + 4) >> 3) as i32;
    }
}

/// [`idct4x4`] specialised for blocks whose only nonzero coefficient is the
/// DC: every output collapses to `(dc + 4) >> 3`. The common case for coded
/// blocks at web quality levels, and ~free next to the 32-multiply full
/// transform.
pub(crate) fn idct4x4_dc(block: &mut [i32]) {
    assert!(block.len() >= 16);
    let v = (block[0] + 4) >> 3;
    block[..16].fill(v);
}

/// [`idct4x4`] specialised for blocks whose only nonzero coefficients are
/// the first three in zigzag order: the DC and the lowest horizontal and
/// vertical frequencies (raster indices 0, 1 and 4). The vertical pass then
/// yields one varying column and one constant column, so the whole
/// transform is four multiplies instead of sixteen (libwebp's
/// `TransformAC3`). Same results as [`idct4x4`], including its wrap to
/// `i32` between the passes.
pub(crate) fn idct4x4_ac3(block: &mut [i32]) {
    assert!(block.len() >= 16);
    let mul1 = |x: i64| x + ((x * CONST1) >> 16);
    let mul2 = |x: i64| (x * CONST2) >> 16;

    let dc = i64::from(block[0]);
    let (x1, x4) = (i64::from(block[1]), i64::from(block[4]));
    let (c4, d4) = (mul2(x4), mul1(x4));
    let (c1, d1) = (mul2(x1), mul1(x1));

    // column 0 after the vertical pass; column 1 is `x1` in every row
    let column = [dc + d4, dc + c4, dc - c4, dc - d4];
    for (row, v) in block[..16].chunks_exact_mut(4).zip(column) {
        let v = i64::from(v as i32);
        row[0] = ((v + d1 + 4) >> 3) as i32;
        row[1] = ((v + c1 + 4) >> 3) as i32;
        row[2] = ((v - c1 + 4) >> 3) as i32;
        row[3] = ((v - d1 + 4) >> 3) as i32;
    }
}

// 14.3
pub(crate) fn iwht4x4(block: &mut [i32]) {
    // Perform one length check up front to avoid subsequent bounds checks in this function
    assert!(block.len() >= 16);

    for i in 0usize..4 {
        let a1 = block[i] + block[12 + i];
        let b1 = block[4 + i] + block[8 + i];
        let c1 = block[4 + i] - block[8 + i];
        let d1 = block[i] - block[12 + i];

        block[i] = a1 + b1;
        block[4 + i] = c1 + d1;
        block[8 + i] = a1 - b1;
        block[12 + i] = d1 - c1;
    }

    for block in block.chunks_exact_mut(4) {
        let a1 = block[0] + block[3];
        let b1 = block[1] + block[2];
        let c1 = block[1] - block[2];
        let d1 = block[0] - block[3];

        let a2 = a1 + b1;
        let b2 = c1 + d1;
        let c2 = a1 - b1;
        let d2 = d1 - c1;

        block[0] = (a2 + 3) >> 3;
        block[1] = (b2 + 3) >> 3;
        block[2] = (c2 + 3) >> 3;
        block[3] = (d2 + 3) >> 3;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three-coefficient transform must agree with the full one on
    /// every block it is used for, including extreme coefficient values.
    #[test]
    fn ac3_idct_matches_full() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 32) as i32
        };
        for case in 0..50_000 {
            let mut block = [0i32; 16];
            for k in [0usize, 1, 4] {
                let r = next();
                block[k] = match case % 5 {
                    0 => r % 4096,
                    1 => r % 400_000,
                    2 => r,
                    3 => [i32::MIN, i32::MAX, 0, -1][(r & 3) as usize],
                    _ => {
                        if r & 1 == 0 {
                            0
                        } else {
                            r % 2048
                        }
                    }
                };
            }
            let (mut a, mut b) = (block, block);
            idct4x4_ac3(&mut a);
            idct4x4(&mut b);
            assert_eq!(a, b, "block {block:?}");
        }
    }
}
