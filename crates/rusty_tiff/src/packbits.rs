//! Macintosh PackBits run-length coding.

use crate::error::{TiffError, TiffResult};

/// Decode into `out`, which must be filled exactly; extra input is ignored.
#[cfg(test)]
pub(crate) fn decode(data: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut v = Vec::new();
    decode_append(data, &mut v, out.len())?;
    out.copy_from_slice(&v);
    Ok(())
}

/// Runs this long or shorter are written byte by byte: TIFF PackBits data
/// is mostly short runs (16-bit samples give 2-3 byte runs), and a libc
/// `memset`/`memcpy` call per run cost more than the bytes (4.6 M memset
/// calls at 22 instructions each on one 16-bit 1.5 Mpx image).
const INLINE_RUN: usize = 16;

/// Append exactly `n` decoded bytes to `out`; extra input is ignored.
/// Nothing is zero-filled first, and `out` grows by at most `n`. (Writing
/// by index into a zero-filled buffer instead measured +4.5% on 16-bit and
/// +17% on 8-bit: this appends straight into the image.)
pub(crate) fn decode_append(data: &[u8], out: &mut Vec<u8>, n: usize) -> TiffResult<()> {
    out.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
    let target = out.len() + n;
    let mut i = 0usize;
    // A strip whose data runs out early keeps what it decoded and is
    // zero-filled to length, as the `tiff` crate does (damaged files often
    // lose a strip's last byte or two).
    while out.len() < target {
        let Some(&h) = data.get(i) else {
            out.resize(target, 0);
            return Ok(());
        };
        i += 1;
        let h = h as i8;
        if h >= 0 {
            let run = h as usize + 1;
            // A literal cut off by the end of the data copies what is there.
            let src = &data[i..data.len().min(i + run)];
            let take = src.len().min(target - out.len());
            if take <= INLINE_RUN {
                for &b in &src[..take] {
                    out.push(b);
                }
            } else {
                out.extend_from_slice(&src[..take]);
            }
            i += run;
        } else if h != -128 {
            let run = (1 - i32::from(h)) as usize;
            let Some(&b) = data.get(i) else {
                out.resize(target, 0);
                return Ok(());
            };
            i += 1;
            let take = run.min(target - out.len());
            if take == 2 {
                // The commonest repeat on 16-bit data: one fixed 2-byte
                // append instead of two pushes.
                out.extend_from_slice(&[b, b]);
            } else if take <= INLINE_RUN {
                for _ in 0..take {
                    out.push(b);
                }
            } else {
                out.resize(out.len() + take, b);
            }
        }
    }
    Ok(())
}

/// Encode one row (PackBits runs must not cross rows per TIFF 6; the caller
/// passes rows separately).
pub(crate) fn encode_row(row: &[u8], out: &mut Vec<u8>) {
    let n = row.len();
    let mut i = 0;
    while i < n {
        // Repeat run?
        // A slice-iterator scan here measured the same (-0.6M on 16-bit,
        // +0.1M on 8-bit): the loop's bounds checks are not the cost.
        let mut r = 1;
        while i + r < n && r < 128 && row[i + r] == row[i] {
            r += 1;
        }
        if r >= 2 {
            // Header and byte as one fixed 2-byte append (one capacity check).
            out.extend_from_slice(&[(1 - r as i32) as i8 as u8, row[i]]);
            i += r;
            continue;
        }
        // Literal run until a repeat of 2+ starts or 128 bytes.
        let start = i;
        i = literal_end(row, start + 1, n.min(start + 128));
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&row[start..i]);
    }
}

/// The first `j` in `from..lim` where `row[j] == row[j + 1]` (a repeat
/// starts), or `lim`.
///
/// Eight positions per step: XOR the eight bytes at `j` with the eight at
/// `j + 1`, and a zero byte marks a repeat. In `(x - 0x01..) & !x & 0x80..`
/// a borrow can flag a byte above a real zero but never below one, so the
/// lowest flagged byte is the first repeat. A byte-at-a-time scan here was
/// most of the encoder's cost (about 8 Ir a byte).
#[inline]
fn literal_end(row: &[u8], from: usize, lim: usize) -> usize {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let n = row.len();
    let mut j = from;
    while j + 9 <= n && j + 8 <= lim {
        let a = u64::from_le_bytes(row[j..j + 8].try_into().unwrap());
        let b = u64::from_le_bytes(row[j + 1..j + 9].try_into().unwrap());
        let x = a ^ b;
        let z = x.wrapping_sub(LO) & !x & HI;
        if z != 0 {
            return j + (z.trailing_zeros() / 8) as usize;
        }
        j += 8;
    }
    while j < lim {
        if j + 1 < n && row[j] == row[j + 1] {
            return j;
        }
        j += 1;
    }
    lim
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_end_matches_byte_scan() {
        let mut seed = 5u32;
        for len in 0..300usize {
            let row: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    // Small alphabet so repeats land everywhere, incl. 0x80/0x01 bytes.
                    [0u8, 1, 0x80, 0x81, 0xff, 7][(seed >> 16) as usize % 6]
                })
                .collect();
            for from in 0..=len {
                for lim in from..=len.min(from + 140) {
                    let mut want = from;
                    while want < lim && !(want + 1 < len && row[want] == row[want + 1]) {
                        want += 1;
                    }
                    assert_eq!(literal_end(&row, from, lim), want, "{len} {from} {lim}");
                }
            }
        }
    }

    #[test]
    fn short_strip_zero_fills() {
        let row: Vec<u8> = (0..300u32)
            .map(|i| if i % 40 < 10 { 9 } else { (i * 13) as u8 })
            .collect();
        let mut enc = Vec::new();
        encode_row(&row, &mut enc);
        for cut in 1..=3 {
            let mut out = Vec::new();
            decode_append(&enc[..enc.len() - cut], &mut out, row.len()).unwrap();
            assert_eq!(out.len(), row.len());
            let good = out.iter().zip(&row).take_while(|(a, b)| a == b).count();
            assert!(good >= row.len() - 130, "cut {cut}: {good}");
            assert!(
                out[good..].iter().all(|&b| b == 0) || good + 3 >= row.len(),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn round_trip() {
        let row: Vec<u8> = (0..1000u32)
            .map(|i| if i % 50 < 20 { 7 } else { (i * 31) as u8 })
            .collect();
        let mut enc = Vec::new();
        encode_row(&row, &mut enc);
        let mut out = vec![0u8; row.len()];
        decode(&enc, &mut out).unwrap();
        assert_eq!(out, row);
    }

    #[test]
    fn spec_example() {
        // Apple's TN1023 example.
        let enc = [
            0xFE, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0xF7,
            0xAA,
        ];
        let want = [
            0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let mut out = vec![0u8; want.len()];
        decode(&enc, &mut out).unwrap();
        assert_eq!(out, want);
    }
}
