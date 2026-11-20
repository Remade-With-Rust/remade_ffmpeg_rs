//! Deflate (compression 8 and 32946) through flate2's pure-Rust zlib-rs
//! backend. Both codes carry a zlib-wrapped stream.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::error::{TiffError, TiffResult};

/// Inflate into `out`, which must be filled exactly.
#[cfg(test)]
pub(crate) fn decode(data: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut v = Vec::new();
    decode_append(data, &mut v, out.len())?;
    out.copy_from_slice(&v);
    Ok(())
}

/// Inflate exactly `n` bytes onto the end of `out`, straight into its spare
/// capacity (no zero-fill first). Capacity is reserved for exactly `n`
/// more, so a decompression bomb costs at most `n` bytes; anything the
/// inflater writes past `n` (only possible if the allocator handed back
/// more capacity than asked) is truncated away.
pub(crate) fn decode_append(data: &[u8], out: &mut Vec<u8>, n: usize) -> TiffResult<()> {
    out.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
    let base = out.len();
    let target = base + n;
    // A strip whose 2-byte zlib header is damaged (bad method, failed check
    // bits, or a preset dictionary no TIFF writer uses) is inflated as the
    // raw deflate stream behind it, as libtiff-free readers such as
    // image-tiff do: a bit flip in the header no longer loses pixels the
    // stream itself still holds. Intact headers take the checked zlib path.
    let (mut d, data) = if data.len() >= 2 && !zlib_header_ok(data[0], data[1]) {
        (Decompress::new(false), &data[2..])
    } else {
        (Decompress::new(true), data)
    };
    loop {
        if out.len() >= target {
            out.truncate(target);
            return Ok(());
        }
        let in_before = d.total_in();
        let out_before = out.len();
        let ip = in_before as usize;
        let status = match d.decompress_vec(&data[ip..], out, FlushDecompress::None) {
            Ok(s) => s,
            // The trailing Adler-32 is checked only after the whole payload
            // has inflated: a failed check with every byte already out
            // ("incorrect data check" on a damaged trailer) keeps the data,
            // as the `tiff` crate does. Any other failure is an error.
            Err(_) if out.len() >= target => {
                out.truncate(target);
                return Ok(());
            }
            Err(e) => return Err(TiffError::format(format!("Deflate: {e}"))),
        };
        let progressed = d.total_in() != in_before || out.len() != out_before;
        if out.len() >= target {
            out.truncate(target);
            return Ok(());
        }
        match status {
            Status::StreamEnd => {
                return Err(TiffError::format(format!(
                    "Deflate stream ended after {} of {n} bytes",
                    out.len() - base
                )))
            }
            Status::Ok | Status::BufError => {
                if !progressed {
                    return Err(TiffError::format(format!(
                        "Deflate data ended after {} of {n} bytes",
                        out.len() - base
                    )));
                }
                if out.len() == out.capacity() {
                    // Capacity exhausted below the target cannot happen with
                    // the reservation above, but never spin if it does.
                    out.try_reserve_exact(target - out.len())
                        .map_err(|_| TiffError::limits("cannot allocate"))?;
                }
            }
        }
    }
}

/// Deflate `data` into a zlib stream at `level` (0-9).
pub(crate) fn encode(data: &[u8], level: u32) -> Vec<u8> {
    let mut c = Compress::new(Compression::new(level.min(9)), true);
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    loop {
        let ip = c.total_in() as usize;
        if out.capacity() - out.len() < 64 {
            out.reserve(out.capacity().max(4096));
        }
        let st = c
            .compress_vec(&data[ip..], &mut out, FlushCompress::Finish)
            .unwrap_or(Status::StreamEnd);
        if st == Status::StreamEnd {
            break;
        }
    }
    out
}

/// RFC 1950 header check: method 8 with a window of at most 32 KB, check
/// bits making `CMF * 256 + FLG` a multiple of 31, and no preset dictionary.
fn zlib_header_ok(cmf: u8, flg: u8) -> bool {
    cmf & 0x0f == 8
        && cmf >> 4 <= 7
        && (u16::from(cmf) * 256 + u16::from(flg)) % 31 == 0
        && flg & 0x20 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_adler32_keeps_the_data() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 241) as u8).collect();
        let mut enc = encode(&data, 6);
        let l = enc.len();
        enc[l - 1] ^= 0x5a;
        let mut out = vec![0u8; data.len()];
        decode(&enc, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn damaged_zlib_header_falls_back_to_raw_deflate() {
        let data: Vec<u8> = (0..50_000u32)
            .map(|i| (i % 253) as u8 ^ (i >> 7) as u8)
            .collect();
        let enc = encode(&data, 6);
        assert!(zlib_header_ok(enc[0], enc[1]));
        let mut recovered = 0;
        for bit in 0..16 {
            let mut bad = enc.clone();
            bad[bit / 8] ^= 1 << (bit % 8);
            let mut out = vec![0u8; data.len()];
            // Either the flip still leaves a valid header (then the zlib path
            // runs and may legitimately fail) or the raw fallback recovers.
            if !zlib_header_ok(bad[0], bad[1]) {
                decode(&bad, &mut out).unwrap();
                assert_eq!(out, data, "bit {bit}");
                recovered += 1;
            }
        }
        assert!(recovered >= 12, "{recovered}");
    }

    #[test]
    fn round_trip() {
        let data: Vec<u8> = (0..100_000u32)
            .map(|i| (i % 251) as u8 ^ (i >> 9) as u8)
            .collect();
        for level in [0, 1, 6, 9] {
            let enc = encode(&data, level);
            let mut out = vec![0u8; data.len()];
            decode(&enc, &mut out).unwrap();
            assert_eq!(out, data);
        }
    }

    #[test]
    fn short_and_garbage() {
        let enc = encode(&[1, 2, 3], 6);
        let mut out = vec![0u8; 10];
        assert!(decode(&enc, &mut out).is_err());
        assert!(decode(&[0x78, 0x9c, 0xff, 0xff, 0xff], &mut out).is_err());
        assert!(decode(&[], &mut out).is_err());
    }
}
