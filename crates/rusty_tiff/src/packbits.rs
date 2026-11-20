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
/// Nothing is zero-filled first, and `out` grows by at most `n`.
pub(crate) fn decode_append(data: &[u8], out: &mut Vec<u8>, n: usize) -> TiffResult<()> {
    out.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
    let target = out.len() + n;
    let mut i = 0usize;
    while out.len() < target {
        let Some(&h) = data.get(i) else {
            return Err(TiffError::format(format!(
                "PackBits data ended after {} of {n} bytes",
                n - (target - out.len())
            )));
        };
        i += 1;
        let h = h as i8;
        if h >= 0 {
            let run = h as usize + 1;
            let src = data
                .get(i..i + run)
                .ok_or_else(|| TiffError::format("PackBits literal run past end of data"))?;
            let take = run.min(target - out.len());
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
            let &b = data
                .get(i)
                .ok_or_else(|| TiffError::format("PackBits repeat past end of data"))?;
            i += 1;
            let take = run.min(target - out.len());
            if take <= INLINE_RUN {
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
        let mut r = 1;
        while i + r < n && r < 128 && row[i + r] == row[i] {
            r += 1;
        }
        if r >= 2 {
            out.push((1 - r as i32) as i8 as u8);
            out.push(row[i]);
            i += r;
            continue;
        }
        // Literal run until a repeat of 2+ starts or 128 bytes.
        let start = i;
        i += 1;
        while i < n && i - start < 128 {
            if i + 1 < n && row[i] == row[i + 1] {
                break;
            }
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&row[start..i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
