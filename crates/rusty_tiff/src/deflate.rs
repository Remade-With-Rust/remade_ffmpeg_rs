//! Deflate (compression 8 and 32946) through flate2's pure-Rust zlib-rs
//! backend. Both codes carry a zlib-wrapped stream.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::error::{TiffError, TiffResult};

/// Inflate into `out`, which must be filled exactly.
#[cfg(test)]
pub(crate) fn decode(data: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut v = Vec::new();
    decode_append(data, &mut v, out.len(), &mut None)?;
    out.copy_from_slice(&v);
    Ok(())
}

/// Inflate exactly `n` bytes onto the end of `out`, straight into its spare
/// capacity (no zero-fill first: flate2 1.1.11 and later hand zlib-rs the
/// spare capacity uninitialised; 1.1.9 zero-filled all of it on every call,
/// which is why the manifest asks for 1.1.11). Capacity is reserved for
/// exactly `n` more, so a decompression bomb costs at most `n` bytes;
/// anything the inflater writes past `n` (only possible if the allocator
/// handed back more capacity than asked) is truncated away.
///
/// `state` is the inflater, kept for all the chunks of an image and reset
/// for each one: building a new one per chunk allocates and clears its
/// state and window every time (6.5% of a decode with default 8 KB strips).
pub(crate) fn decode_append(
    data: &[u8],
    out: &mut Vec<u8>,
    n: usize,
    state: &mut Option<Decompress>,
) -> TiffResult<usize> {
    out.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
    let base = out.len();
    let target = base + n;
    // A strip whose 2-byte zlib header is damaged (bad method, failed check
    // bits, or a preset dictionary no TIFF writer uses) is inflated as the
    // raw deflate stream behind it, as libtiff-free readers such as
    // image-tiff do: a bit flip in the header no longer loses pixels the
    // stream itself still holds. Intact headers take the checked zlib path.
    let (zlib, data) = if data.len() >= 2 && !zlib_header_ok(data[0], data[1]) {
        (false, &data[2..])
    } else {
        (true, data)
    };
    let d = match state {
        Some(d) => {
            d.reset(zlib);
            d
        }
        None => state.insert(Decompress::new(zlib)),
    };
    loop {
        if out.len() >= target {
            out.truncate(target);
            return Ok(n);
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
                return Ok(n);
            }
            // A failed check on a payload that did not fully inflate keeps
            // what did and zero-fills the rest, as the `tiff` crate does.
            Err(e) if e.to_string().contains("data check") => {
                let real = out.len() - base;
                out.resize(target, 0);
                return Ok(real);
            }
            Err(e) => return Err(TiffError::format(format!("Deflate: {e}"))),
        };
        let progressed = d.total_in() != in_before || out.len() != out_before;
        if out.len() >= target {
            out.truncate(target);
            return Ok(n);
        }
        match status {
            // The stream (or the strip's data) ended short of the rows: keep
            // what inflated and zero-fill the rest, like a short LZW or
            // PackBits strip.
            Status::StreamEnd => {
                let real = out.len() - base;
                out.resize(target, 0);
                return Ok(real);
            }
            Status::Ok | Status::BufError => {
                if !progressed {
                    let real = out.len() - base;
                    out.resize(target, 0);
                    return Ok(real);
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
#[cfg(test)]
pub(crate) fn encode(data: &[u8], level: u32) -> Vec<u8> {
    DeflateEncoder::new(level).encode(data)
}

/// A zlib compressor reused across strips and tiles.
///
/// `Compress::new` zero-initialises the whole zlib state (window, hash
/// chains, pending buffer: about 500 KB) and a default strip is about 8 KB,
/// so building one per strip spent 40% of a Deflate encode in `memset`.
/// `reset` returns the same state to a fresh stream, and the output is the
/// same bytes a new compressor would produce.
pub(crate) struct DeflateEncoder {
    c: Compress,
}

impl DeflateEncoder {
    pub(crate) fn new(level: u32) -> Self {
        Self {
            c: Compress::new(Compression::new(level.min(9)), true),
        }
    }

    /// Deflate one strip or tile into a zlib stream.
    pub(crate) fn encode(&mut self, data: &[u8]) -> Vec<u8> {
        self.c.reset();
        encode_with(&mut self.c, data)
    }
}

fn encode_with(c: &mut Compress, data: &[u8]) -> Vec<u8> {
    // zlib's compressBound: the stream never exceeds it, so the buffer never
    // reallocates (half the input, as before, grew 1,041 times on a photo).
    let n = data.len();
    let mut out = Vec::with_capacity(n + (n >> 12) + (n >> 14) + (n >> 25) + 13 + 64);
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
    fn inflater_reused_across_chunks() {
        // One inflater for every chunk of an image: intact headers, damaged
        // headers (raw fallback), a failed stream and a short one, in any
        // order, each give what a fresh inflater gives.
        let a: Vec<u8> = (0..30_000u32)
            .map(|i| (i % 249) as u8 ^ (i >> 8) as u8)
            .collect();
        let b: Vec<u8> = (0..9_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let ea = encode(&a, 6);
        let mut eb = encode(&b, 1);
        eb[0] ^= 0x01; // damaged header: inflated as raw deflate
        assert!(!zlib_header_ok(eb[0], eb[1]));
        let short = encode(&[9, 8, 7], 6);
        let garbage = [0x78, 0x9c, 0xff, 0xff, 0xff];
        let chunks: [(&[u8], usize); 6] = [
            (&ea, a.len()),
            (&eb, b.len()),
            (&garbage, 64),
            (&short, 16),
            (&eb, b.len()),
            (&ea, a.len()),
        ];
        let mut state = None;
        for (data, n) in chunks {
            let mut fresh = Vec::new();
            let want = decode_append(data, &mut fresh, n, &mut None);
            let mut kept = vec![0x55u8; 3];
            let got = decode_append(data, &mut kept, n, &mut state);
            match (want, got) {
                (Ok(w), Ok(g)) => {
                    assert_eq!(w, g);
                    assert_eq!(&kept[..3], &[0x55; 3]);
                    assert_eq!(kept[3..], fresh[..]);
                }
                (Err(_), Err(_)) => {}
                (w, g) => panic!("fresh {w:?} kept {g:?}"),
            }
        }
    }

    #[test]
    fn short_and_garbage() {
        // A stream shorter than the strip keeps what it holds, zero-filled
        // (as the `tiff` crate does); an empty strip reads as zeros.
        let enc = encode(&[1, 2, 3], 6);
        let mut out = vec![0xaa; 10];
        decode(&enc, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 0, 0, 0, 0, 0, 0, 0]);
        decode(&[], &mut out).unwrap();
        assert_eq!(out, [0; 10]);
        // Garbage is still an error.
        assert!(decode(&[0x78, 0x9c, 0xff, 0xff, 0xff], &mut out).is_err());
    }
}
