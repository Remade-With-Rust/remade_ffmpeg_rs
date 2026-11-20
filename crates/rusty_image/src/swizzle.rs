//! Red/blue exchange between BGR(A) file order and RGB(A) image order.
//!
//! A 3-byte pixel loaded as a little-endian word is `b | g << 8 | r << 16`
//! (plus the next byte on top); `swap_bytes() >> 8` is then exactly
//! `r | g << 8 | b << 16`. A 4-byte BGRA word becomes RGBA with
//! `swap_bytes().rotate_right(8)`. Both are two instructions a pixel, where
//! assembling the pixel byte by byte cost about eight.

/// Copy 3-byte pixels from `src` to `dst` exchanging the first and third
/// byte (BGR <-> RGB). Both hold the same whole number of pixels.
///
/// Every pixel but the last is one 4-byte load and one 4-byte store: the
/// fourth byte written lands on the next pixel's first byte, which that
/// pixel's own store then overwrites. The last pixel, with no room for a
/// fourth byte, is written byte by byte.
#[inline(never)]
pub(crate) fn swap_rb3(dst: &mut [u8], src: &[u8]) {
    let len = dst.len().min(src.len());
    let mut i = 0;
    while i + 4 <= len {
        let w: [u8; 4] = src[i..i + 4].try_into().unwrap();
        let v = u32::from_le_bytes(w).swap_bytes() >> 8;
        dst[i..i + 4].copy_from_slice(&v.to_le_bytes());
        i += 3;
    }
    if i + 3 <= len {
        dst[i] = src[i + 2];
        dst[i + 1] = src[i + 1];
        dst[i + 2] = src[i];
    }
}

/// Copy 4-byte pixels from `src` to `dst` exchanging the first and third
/// byte (BGRA <-> RGBA).
#[inline]
pub(crate) fn swap_rb4(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        d.copy_from_slice(&v.swap_bytes().rotate_right(8).to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_byte_swaps() {
        for n in 0..40usize {
            let src3: Vec<u8> = (0..n * 3).map(|i| (i * 37 + 11) as u8).collect();
            let mut want = src3.clone();
            for p in want.chunks_exact_mut(3) {
                p.swap(0, 2);
            }
            let mut got = vec![0xee; n * 3];
            swap_rb3(&mut got, &src3);
            assert_eq!(got, want, "3 x {n}");

            let src4: Vec<u8> = (0..n * 4).map(|i| (i * 53 + 7) as u8).collect();
            let mut want = src4.clone();
            for p in want.chunks_exact_mut(4) {
                p.swap(0, 2);
            }
            let mut got = vec![0xee; n * 4];
            swap_rb4(&mut got, &src4);
            assert_eq!(got, want, "4 x {n}");
        }
    }
}
