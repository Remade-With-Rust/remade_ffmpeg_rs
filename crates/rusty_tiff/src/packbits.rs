//! Macintosh PackBits run-length coding.

use crate::error::{TiffError, TiffResult};

/// Decode into `out`, which must be filled exactly; extra input is ignored.
pub(crate) fn decode(data: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut i = 0usize;
    let mut pos = 0usize;
    while pos < out.len() {
        let Some(&h) = data.get(i) else {
            return Err(TiffError::format(format!(
                "PackBits data ended after {pos} of {} bytes",
                out.len()
            )));
        };
        i += 1;
        let h = h as i8;
        if h >= 0 {
            let n = h as usize + 1;
            let src = data
                .get(i..i + n)
                .ok_or_else(|| TiffError::format("PackBits literal run past end of data"))?;
            let take = n.min(out.len() - pos);
            out[pos..pos + take].copy_from_slice(&src[..take]);
            pos += take;
            i += n;
        } else if h != -128 {
            let n = (1 - i32::from(h)) as usize;
            let &b = data
                .get(i)
                .ok_or_else(|| TiffError::format("PackBits repeat past end of data"))?;
            i += 1;
            let take = n.min(out.len() - pos);
            out[pos..pos + take].fill(b);
            pos += take;
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
