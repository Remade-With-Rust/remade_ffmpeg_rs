//! QOI, the Quite OK Image format (specification 1.0).
//!
//! Decodes to `Rgb8` or `Rgba8` per the header's channel count, as the `qoi`
//! crate (which `image` uses) does; encodes `Rgb8` and `Rgba8`.

use crate::read::Reader;
use crate::{Error, Image, PixelFormat, Result};

const OP_INDEX: u8 = 0x00;
const OP_DIFF: u8 = 0x40;
const OP_LUMA: u8 = 0x80;
const OP_RUN: u8 = 0xc0;
const OP_RGB: u8 = 0xfe;
const OP_RGBA: u8 = 0xff;
const END: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];

/// [`hash_u32`] of a pixel as bytes (for the spec-formula test).
#[cfg(test)]
fn hash(p: [u8; 4]) -> usize {
    hash_u32(u32::from_le_bytes(p))
}

/// The QOI colour hash `(r*3 + g*5 + b*7 + a*11) % 64` of a pixel packed
/// little-endian (r in the low byte), with one multiply: the four bytes are
/// spread into 16-bit lanes (r, b, g, a from the bottom) and multiplied by a
/// constant whose lanes hold the weights in reverse, so lane 3 of the product
/// is exactly the weighted sum. No lane can carry into the next (the largest
/// lane sum is 255 * 26 = 6630), so the low six bits of lane 3 are the hash.
#[inline]
fn hash_u32(v: u32) -> usize {
    let v = u64::from(v);
    let w = (v & 0x00ff_00ff) | ((v & 0xff00_ff00) << 24);
    ((w.wrapping_mul((3 << 48) | (7 << 32) | (5 << 16) | 11) >> 48) & 63) as usize
}

/// Add two packed pixels byte by byte, each byte wrapping on its own (no
/// carry crosses into the next channel).
#[inline]
fn add_bytes(a: u32, b: u32) -> u32 {
    ((a & 0x7f7f_7f7f) + (b & 0x7f7f_7f7f)) ^ ((a ^ b) & 0x8080_8080)
}

/// QOI_OP_DIFF's low six bits -> the packed per-channel addend
/// (`dr`, `dg`, `db` each biased by 2; alpha unchanged).
const DIFF_ADD: [u32; 64] = {
    let mut t = [0u32; 64];
    let mut i = 0;
    while i < 64 {
        let (r, g, b) = ((i as u32 >> 4) & 3, (i as u32 >> 2) & 3, i as u32 & 3);
        t[i] = (r.wrapping_sub(2) & 0xff)
            | ((g.wrapping_sub(2) & 0xff) << 8)
            | ((b.wrapping_sub(2) & 0xff) << 16);
        i += 1;
    }
    t
};

/// Decode a QOI file.
pub fn decode(data: &[u8]) -> Result<Image> {
    let mut r = Reader::new(data);
    if r.array::<4>()? != *b"qoif" {
        return Err(Error::invalid("qoi: no qoif magic"));
    }
    let width = r.u32be()?;
    let height = r.u32be()?;
    let channels = r.u8()?;
    let colorspace = r.u8()?;
    if width == 0 || height == 0 {
        return Err(Error::invalid("qoi: empty image"));
    }
    if colorspace > 1 {
        return Err(Error::invalid("qoi: colour space"));
    }
    let format = match channels {
        3 => PixelFormat::Rgb8,
        4 => PixelFormat::Rgba8,
        n => return Err(Error::invalid(format!("qoi: {n} channels"))),
    };
    let len = crate::byte_len(width, height, format)?;
    let data = r.rest();
    let pixels = if channels == 3 {
        decode_ops::<3>(data, len)?
    } else {
        decode_ops::<4>(data, len)?
    };
    Image::new(width, height, format, pixels)
}

/// The op stream into `len` bytes of `CH`-channel pixels. Reads go through
/// `data.get`, so a cut-off stream is an error.
///
/// The output is pre-sized and every pixel is one fixed 4-byte store: for
/// three channels the fourth byte lands on the next pixel's first byte, which
/// that pixel then overwrites (pixels are written strictly in order and every
/// position below `len` is written), and only the last pixel, with no room
/// for four bytes, takes a 3-byte store. A run's length is counted in bytes,
/// so no division by the channel count per op.
fn decode_ops<const CH: usize>(data: &[u8], len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    // Pixels packed little-endian (r in the low byte): an index op is one
    // load, the hash reads the word directly and a pixel is one store.
    let mut index = [0u32; 64];
    let mut px = 0xff00_0000u32;
    let mut pos = 0usize;
    let mut o = 0usize;
    while o < len {
        let b = *data.get(pos).ok_or(Error::Truncated)?;
        pos += 1;
        // Dispatch as a chain of range tests on the 2-bit tag. A `match`
        // here compiled to an indirect jump table, and on 3-channel data
        // its target mispredicted on 59% of ops (callgrind branch sim:
        // 797K of 1.36M), which Ir cannot see.
        let end = if b < OP_DIFF {
            px = index[usize::from(b & 0x3f)];
            o + CH
        } else if b < OP_LUMA {
            px = add_bytes(px, DIFF_ADD[usize::from(b & 0x3f)]);
            o + CH
        } else if b < OP_RUN {
            let b2 = *data.get(pos).ok_or(Error::Truncated)?;
            pos += 1;
            let dg = (b & 0x3f).wrapping_sub(32);
            let dr = dg.wrapping_sub(8).wrapping_add(b2 >> 4);
            let db = dg.wrapping_sub(8).wrapping_add(b2 & 0x0f);
            px = add_bytes(
                px,
                u32::from(dr) | (u32::from(dg) << 8) | (u32::from(db) << 16),
            );
            o + CH
        } else if b < OP_RGB {
            // OP_RUN: the same pixel, up to 62 times, never past the end.
            (o + (usize::from(b & 0x3f) + 1) * CH).min(len)
        } else if b == OP_RGB {
            let c = data.get(pos..pos + 3).ok_or(Error::Truncated)?;
            px = (px & 0xff00_0000)
                | u32::from(c[0])
                | (u32::from(c[1]) << 8)
                | (u32::from(c[2]) << 16);
            pos += 3;
            o + CH
        } else {
            let c = data.get(pos..pos + 4).ok_or(Error::Truncated)?;
            px = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            pos += 4;
            o + CH
        };
        index[hash_u32(px)] = px;
        let bytes = px.to_le_bytes();
        while o < end {
            if let Some(d) = out.get_mut(o..o + 4) {
                d.copy_from_slice(&bytes);
            } else {
                out[o..o + CH].copy_from_slice(&bytes[..CH]);
            }
            o += CH;
        }
    }
    Ok(out)
}

/// Encode `Rgb8` or `Rgba8` as QOI (sRGB colour space flag).
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    let ch = match image.format {
        PixelFormat::Rgb8 => 3usize,
        PixelFormat::Rgba8 => 4,
        other => {
            return Err(Error::unsupported(format!(
                "qoi: encoding {other:?} (Rgb8, Rgba8)"
            )))
        }
    };
    if image.width == 0 || image.height == 0 {
        return Err(Error::unsupported("qoi: empty image"));
    }
    // Reserving the worst case (CH + 1 bytes a pixel) to avoid the one
    // realloc an RGBA photo that encodes larger than its pixels pays measured
    // worse (Ir +3.5M RGB / +2M RGBA) and tied on the clock, so the input
    // size stays the guess.
    let mut out = Vec::with_capacity(14 + image.pixels.len() + 8);
    out.extend_from_slice(b"qoif");
    out.extend_from_slice(&image.width.to_be_bytes());
    out.extend_from_slice(&image.height.to_be_bytes());
    out.push(ch as u8);
    out.push(0);
    if ch == 3 {
        encode_ops::<3>(&image.pixels, &mut out);
    } else {
        encode_ops::<4>(&image.pixels, &mut out);
    }
    out.extend_from_slice(&END);
    Ok(out)
}

/// The op stream for `CH`-channel pixels, appended to `out`. The channel
/// count is a constant, so building the pixel is a load, not a per-pixel
/// branch. A run still pending at the end is flushed after the loop rather
/// than tested for on every pixel.
///
/// Pixels are packed little-endian (r in the low byte), as in the decoder:
/// the equality tests, the hash and the index are single-word operations.
/// The per-pixel body is a local macro over plain locals: as a method on a
/// state struct the 256-byte index kept the whole struct in memory, so
/// `prev` and `run` went through it every pixel (measured +6% Ir at 24-bit).
/// The op choice stays branchy: computing every candidate encoding, picking
/// one by value selects and always storing 8 bytes into a pre-sized buffer
/// (to dodge op mispredicts, 645K of 4.4M on RGB data in callgrind's branch
/// sim) measured worse on both instruments: Ir RGB -28.5% -> -10.3%, RGBA
/// -> +2.7%; wall 0.70 s -> 0.82 s RGB, 0.28 s -> 0.59 s RGBA.
/// Keeping the branches but writing into a pre-sized, zero-filled buffer by
/// index (no Vec capacity checks) also lost: Ir RGB -30.8% -> -31.5% but
/// RGBA -46.4% -> -35.5%; wall 1.01 -> 1.06 s RGB, 0.36 -> 0.47 s RGBA.
fn encode_ops<const CH: usize>(pixels: &[u8], out: &mut Vec<u8>) {
    let mut index = [0u32; 64];
    let mut prev = 0xff00_0000u32;
    let mut run = 0u8;
    macro_rules! pixel {
        ($px:expr) => {{
            let px: u32 = $px;
            if px == prev {
                run += 1;
                if run == 62 {
                    out.push(OP_RUN | 61);
                    run = 0;
                }
                continue;
            }
            if run > 0 {
                out.push(OP_RUN | (run - 1));
                run = 0;
            }
            let h = hash_u32(px);
            if index[h] == px {
                out.push(OP_INDEX | h as u8);
            } else {
                index[h] = px;
                let [r, g, b, a] = px.to_le_bytes();
                let [pr, pg, pb, pa] = prev.to_le_bytes();
                if a == pa {
                    let dr = r.wrapping_sub(pr) as i8;
                    let dg = g.wrapping_sub(pg) as i8;
                    let db = b.wrapping_sub(pb) as i8;
                    let (dr_dg, db_dg) = (dr.wrapping_sub(dg), db.wrapping_sub(dg));
                    if (-2..=1).contains(&dr) && (-2..=1).contains(&dg) && (-2..=1).contains(&db) {
                        out.push(
                            OP_DIFF
                                | ((dr + 2) as u8) << 4
                                | ((dg + 2) as u8) << 2
                                | (db + 2) as u8,
                        );
                    } else if (-32..=31).contains(&dg)
                        && (-8..=7).contains(&dr_dg)
                        && (-8..=7).contains(&db_dg)
                    {
                        out.extend_from_slice(&[
                            OP_LUMA | (dg + 32) as u8,
                            ((dr_dg + 8) as u8) << 4 | (db_dg + 8) as u8,
                        ]);
                    } else {
                        out.extend_from_slice(&[OP_RGB, r, g, b]);
                    }
                } else {
                    out.extend_from_slice(&[OP_RGBA, r, g, b, a]);
                }
            }
            prev = px;
        }};
    }
    if CH == 4 {
        for p in pixels.chunks_exact(4) {
            pixel!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]));
        }
    } else {
        let n = pixels.len() / 3;
        for i in 0..n {
            let at = i * 3;
            pixel!(if let Some(q) = pixels.get(at..at + 4) {
                // One 4-byte load; the fourth byte (the next pixel's red)
                // is replaced by opaque alpha.
                u32::from_le_bytes([q[0], q[1], q[2], q[3]]) | 0xff00_0000
            } else {
                u32::from_le_bytes([pixels[at], pixels[at + 1], pixels[at + 2], 255])
            });
        }
    }
    if run > 0 {
        out.push(OP_RUN | (run - 1));
    }
}

#[cfg(test)]
mod tests {
    use super::{add_bytes, hash, DIFF_ADD};

    #[test]
    fn packed_byte_add_wraps_per_channel() {
        let mut s = 0x1234_5679u32;
        for _ in 0..200_000 {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let t = s.rotate_left(11) ^ 0xa5a5_5a5a;
            let (a, b) = (s.to_le_bytes(), t.to_le_bytes());
            let want = [
                a[0].wrapping_add(b[0]),
                a[1].wrapping_add(b[1]),
                a[2].wrapping_add(b[2]),
                a[3].wrapping_add(b[3]),
            ];
            assert_eq!(add_bytes(s, t).to_le_bytes(), want);
        }
        for i in 0..64u8 {
            let d = DIFF_ADD[usize::from(i)].to_le_bytes();
            assert_eq!(d[0], ((i >> 4) & 3).wrapping_sub(2));
            assert_eq!(d[1], ((i >> 2) & 3).wrapping_sub(2));
            assert_eq!(d[2], (i & 3).wrapping_sub(2));
            assert_eq!(d[3], 0);
        }
    }

    #[test]
    fn one_multiply_hash_matches_the_spec_formula() {
        let mut s = 0x9e37_79b9u32;
        for i in 0..200_000u32 {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let p = if i < 4 {
                [255u8 * (i as u8 & 1), 255, 255, 255]
            } else {
                s.to_le_bytes()
            };
            let want = (usize::from(p[0]) * 3
                + usize::from(p[1]) * 5
                + usize::from(p[2]) * 7
                + usize::from(p[3]) * 11)
                % 64;
            assert_eq!(hash(p), want, "{p:?}");
        }
        assert_eq!(hash([255; 4]), (255 * 26) % 64);
    }
}
