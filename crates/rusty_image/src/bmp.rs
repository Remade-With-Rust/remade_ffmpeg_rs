//! Windows bitmap (BMP / DIB).
//!
//! Decodes every layout Windows writes: 1, 2, 4 and 8-bit palettes, 16-bit
//! (5-5-5 and `BI_BITFIELDS`, 5-6-5 included), 24-bit, 32-bit (`BI_RGB` and
//! `BI_BITFIELDS`, with or without an alpha mask), RLE4 and RLE8 with delta
//! and end-of-line escapes, bottom-up and top-down, behind a core,
//! `BITMAPINFOHEADER` or V2-V5 header. Headerless DIBs (the clipboard's
//! `CF_DIB` / `CF_DIBV5`, and ICO entries) go through [`decode_dib`].
//!
//! Output layouts follow the `image` crate exactly: RGBA when the file has an
//! alpha mask, RGB otherwise (a 32-bit `BI_RGB` file's fourth byte is not
//! alpha by the specification and is dropped, as there).

use crate::read::Reader;
use crate::{Error, Image, PixelFormat, Result};

const CORE: u32 = 12;
const INFO: u32 = 40;
const V2: u32 = 52;
const V3: u32 = 56;
const V4: u32 = 108;
const V5: u32 = 124;

/// The largest width or height accepted (as in `image`).
const MAX_SIDE: i32 = 0xFFFF;

static LUT3: [u8; 8] = [0, 36, 73, 109, 146, 182, 219, 255];
static LUT4: [u8; 16] = [
    0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255,
];
static LUT5: [u8; 32] = [
    0, 8, 16, 25, 33, 41, 49, 58, 66, 74, 82, 90, 99, 107, 115, 123, 132, 140, 148, 156, 165, 173,
    181, 189, 197, 206, 214, 222, 230, 239, 247, 255,
];
static LUT6: [u8; 64] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 45, 49, 53, 57, 61, 65, 69, 73, 77, 81, 85, 89, 93,
    97, 101, 105, 109, 113, 117, 121, 125, 130, 134, 138, 142, 146, 150, 154, 158, 162, 166, 170,
    174, 178, 182, 186, 190, 194, 198, 202, 206, 210, 215, 219, 223, 227, 231, 235, 239, 243, 247,
    251, 255,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Field {
    shift: u32,
    len: u32,
}

impl Field {
    fn from_mask(mask: u32, max_len: u32) -> Result<Field> {
        if mask == 0 {
            return Ok(Field { shift: 0, len: 0 });
        }
        let mut shift = mask.trailing_zeros();
        let mut len = (!(mask >> shift)).trailing_zeros();
        if len != mask.count_ones() {
            return Err(Error::invalid("bmp: non-contiguous bitfield mask"));
        }
        if len + shift > max_len {
            return Err(Error::invalid("bmp: bitfield mask wider than the pixel"));
        }
        if len > 8 {
            shift += len - 8;
            len = 8;
        }
        Ok(Field { shift, len })
    }

    fn read(self, data: u32) -> u8 {
        let d = data >> self.shift;
        match self.len {
            1 => ((d & 1) * 0xff) as u8,
            2 => ((d & 3) * 0x55) as u8,
            3 => LUT3[(d & 7) as usize],
            4 => LUT4[(d & 15) as usize],
            5 => LUT5[(d & 31) as usize],
            6 => LUT6[(d & 63) as usize],
            7 => (((d & 0x7f) << 1) | ((d & 0x7f) >> 6)) as u8,
            _ => (d & 0xff) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Fields {
    r: Field,
    g: Field,
    b: Field,
    a: Field,
}

impl Fields {
    fn from_masks(r: u32, g: u32, b: u32, a: u32, max_len: u32) -> Result<Fields> {
        let f = Fields {
            r: Field::from_mask(r, max_len)?,
            g: Field::from_mask(g, max_len)?,
            b: Field::from_mask(b, max_len)?,
            a: Field::from_mask(a, max_len)?,
        };
        if f.r.len == 0 || f.g.len == 0 || f.b.len == 0 {
            return Err(Error::invalid("bmp: missing bitfield mask"));
        }
        Ok(f)
    }
}

/// A bitfield layout as lookup tables: channel `c` of pixel `v` is
/// `tab[c][(v >> shift[c]) as u8 & mask[c]]`. A table holds the channel's
/// expansion to eight bits ([`Field::read`]) for every value of the field,
/// so a pixel is three or four shift-mask-load-store steps, where
/// `Field::read` chose among eight expansions by the field's width for
/// every channel of every pixel (63 Ir a 16-bit pixel).
struct FieldTables {
    shift: [u32; 4],
    mask: [u8; 4],
    tab: [[u8; 256]; 4],
}

impl FieldTables {
    fn new(f: &Fields) -> FieldTables {
        let mut t = FieldTables {
            shift: [0; 4],
            mask: [0; 4],
            tab: [[0; 256]; 4],
        };
        for (c, field) in [f.r, f.g, f.b, f.a].into_iter().enumerate() {
            if field.len == 0 {
                // No such channel (only alpha can be absent): every pixel
                // reads entry 0, opaque.
                t.tab[c][0] = 0xff;
                continue;
            }
            t.shift[c] = field.shift;
            // 1 to 8 bits (`from_mask` narrows wider fields to their top 8).
            t.mask[c] = (0xffu32 >> (8 - field.len)) as u8;
            let unit = Field {
                shift: 0,
                len: field.len,
            };
            for v in 0..=u32::from(t.mask[c]) {
                t.tab[c][v as usize] = unit.read(v);
            }
        }
        t
    }

    /// One row: `S`-byte little-endian pixels (2 or 4) to `C` channels.
    /// (Four parallel arrays read in step: an index is the plain way to
    /// say it.)
    #[inline(always)]
    #[allow(clippy::needless_range_loop)]
    fn row<const S: usize, const C: usize>(&self, dst: &mut [u8], src: &[u8]) {
        for (px, s) in dst.chunks_exact_mut(C).zip(src.chunks_exact(S)) {
            let v = if S == 2 {
                u32::from(u16::from_le_bytes([s[0], s[1]]))
            } else {
                u32::from_le_bytes([s[0], s[1], s[2], s[3]])
            };
            for c in 0..C {
                px[c] = self.tab[c][usize::from((v >> self.shift[c]) as u8 & self.mask[c])];
            }
        }
    }
}

const RGB555: Fields = Fields {
    r: Field { shift: 10, len: 5 },
    g: Field { shift: 5, len: 5 },
    b: Field { shift: 0, len: 5 },
    a: Field { shift: 0, len: 0 },
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Palette,
    Rgb16,
    Rgb24,
    Rgb32,
    Rgba32,
    Rle8,
    Rle4,
    Bitfields16,
    Bitfields32,
}

/// Where a DIB comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A `.bmp` file: `BM` file header, then the DIB.
    File,
    /// A packed DIB with no file header (`CF_DIB`, `CF_DIBV5`).
    Dib,
    /// An ICO/CUR entry: a packed DIB, always RGBA, with a doubled height.
    Ico,
}

struct Header {
    width: usize,
    height: usize,
    top_down: bool,
    bit_count: u16,
    kind: Kind,
    alpha: bool,
    fields: Option<Fields>,
    palette: Option<Box<[[u8; 3]; 256]>>,
    data_offset: usize,
}

fn read_header(r: &mut Reader, mode: Mode) -> Result<Header> {
    let mut data_offset = 0usize;
    if mode == Mode::File {
        if r.array::<2>()? != *b"BM" {
            return Err(Error::invalid("bmp: no BM signature"));
        }
        r.skip(8)?;
        data_offset = r.u32le()? as usize;
    }
    let header_offset = r.pos();
    let header_size = r.u32le()?;
    let header_end = header_offset + header_size as usize;
    match header_size {
        CORE | INFO | V2 | V3 | V4 | V5 => {}
        s if s < CORE => return Err(Error::invalid(format!("bmp: header of {s} bytes"))),
        s => return Err(Error::unsupported(format!("bmp: header of {s} bytes"))),
    }
    let mut alpha = mode == Mode::Ico;
    let (width, height, top_down, bit_count, kind, colors_used);
    if header_size == CORE {
        width = i32::from(r.u16le()?);
        height = i32::from(r.u16le()?);
        top_down = false;
        if r.u16le()? != 1 {
            return Err(Error::invalid("bmp: more than one plane"));
        }
        bit_count = r.u16le()?;
        kind = match bit_count {
            1 | 4 | 8 => Kind::Palette,
            24 => Kind::Rgb24,
            n => return Err(Error::invalid(format!("bmp: {n}-bit core header"))),
        };
        colors_used = 0;
    } else {
        width = r.i32le()?;
        let mut h = r.i32le()?;
        if width < 0 {
            return Err(Error::invalid("bmp: negative width"));
        }
        if width > MAX_SIDE || h > MAX_SIDE {
            return Err(Error::unsupported(format!(
                "bmp: {width}x{h} is past the {MAX_SIDE}-pixel limit"
            )));
        }
        if h == i32::MIN {
            return Err(Error::invalid("bmp: invalid height"));
        }
        let mut td = false;
        if h < 0 {
            h = -h;
            td = true;
        }
        height = h;
        top_down = td;
        if r.u16le()? != 1 {
            return Err(Error::invalid("bmp: more than one plane"));
        }
        bit_count = r.u16le()?;
        let compression = r.u32le()?;
        if top_down && compression != 0 && compression != 3 {
            return Err(Error::invalid("bmp: compressed top-down bitmap"));
        }
        kind = match (compression, bit_count) {
            (0, 1 | 2 | 4 | 8) => Kind::Palette,
            (0, 16) => Kind::Rgb16,
            (0, 24) => Kind::Rgb24,
            (0, 32) if alpha => Kind::Rgba32,
            (0, 32) => Kind::Rgb32,
            (1, 8) => Kind::Rle8,
            (2, 4) => Kind::Rle4,
            (3, 16) => Kind::Bitfields16,
            (3, 32) => Kind::Bitfields32,
            (0..=3, n) => {
                return Err(Error::invalid(format!(
                    "bmp: {n}-bit with compression {compression}"
                )))
            }
            (4, _) => return Err(Error::unsupported("bmp: JPEG compression")),
            (5, _) => return Err(Error::unsupported("bmp: PNG compression")),
            (11..=13, _) => return Err(Error::unsupported("bmp: CMYK")),
            (c, _) => return Err(Error::invalid(format!("bmp: compression {c}"))),
        };
        r.skip(12)?;
        colors_used = r.u32le()?;
        r.skip(4)?;
    }
    if width <= 0 || height <= 0 {
        return Err(Error::invalid("bmp: empty image"));
    }

    let mut fields = None;
    let mut mask_bytes = 0;
    if matches!(kind, Kind::Bitfields16 | Kind::Bitfields32) {
        let rm = r.u32le()?;
        let gm = r.u32le()?;
        let bm = r.u32le()?;
        let am = if matches!(header_size, V3 | V4 | V5) {
            r.u32le()?
        } else {
            0
        };
        let max = if kind == Kind::Bitfields16 { 16 } else { 32 };
        fields = Some(Fields::from_masks(rm, gm, bm, am, max)?);
        if am != 0 {
            alpha = true;
        }
        // INFO, V4 and V5 headers are followed by the three masks again
        // (the packed-DIB colour table), whatever the header holds.
        if matches!(header_size, INFO | V4 | V5) {
            mask_bytes = 12;
        }
    }
    // Where the colour table would start. A file with masks only inside a
    // V4/V5 header can end before this point (a tiny image whose pixels sit
    // at the header's end); that only matters when something is read here.
    let after_header = header_end + mask_bytes;

    let mut palette = None;
    if matches!(kind, Kind::Palette | Kind::Rle4 | Kind::Rle8) {
        r.seek(after_header)?;
        let per = if header_size == CORE { 3 } else { 4 };
        let max = 1usize << bit_count;
        let n = match colors_used {
            0 => max,
            c if c as usize > max => {
                return Err(Error::invalid(format!(
                    "bmp: {c} palette entries for {bit_count}-bit pixels"
                )))
            }
            c => c as usize,
        };
        let raw = r.bytes(n * per)?;
        let mut p = Box::new([[0u8; 3]; 256]);
        for (i, e) in raw.chunks_exact(per).enumerate() {
            p[i] = [e[2], e[1], e[0]];
        }
        palette = Some(p);
    }
    if mode != Mode::File {
        // Pixels follow the colour table (or the masks).
        data_offset = if palette.is_some() {
            r.pos()
        } else {
            after_header
        };
    }
    let mut height = height as usize;
    if mode == Mode::Ico {
        // The height counts the AND mask too.
        height /= 2;
        if height == 0 {
            return Err(Error::invalid("bmp: empty icon entry"));
        }
    }
    Ok(Header {
        width: width as usize,
        height,
        top_down,
        bit_count,
        kind,
        alpha,
        fields,
        palette,
        data_offset,
    })
}

/// Decode a `.bmp` file.
pub fn decode(data: &[u8]) -> Result<Image> {
    decode_mode(data, Mode::File).map(|(img, _)| img)
}

/// Decode a packed DIB: a `BITMAPINFOHEADER` / V4 / V5 / core header, the
/// palette or masks, then the pixels, with no `BM` file header. This is what
/// the Windows clipboard hands over as `CF_DIB` and `CF_DIBV5`.
pub fn decode_dib(data: &[u8]) -> Result<Image> {
    decode_mode(data, Mode::Dib).map(|(img, _)| img)
}

/// Decode an ICO/CUR entry's DIB; also returns where its pixel data ended
/// (the AND mask follows).
pub(crate) fn decode_ico_dib(data: &[u8]) -> Result<(Image, usize)> {
    decode_mode(data, Mode::Ico)
}

fn decode_mode(data: &[u8], mode: Mode) -> Result<(Image, usize)> {
    let mut r = Reader::new(data);
    let h = read_header(&mut r, mode)?;
    let format = if h.alpha {
        PixelFormat::Rgba8
    } else {
        PixelFormat::Rgb8
    };
    let mut img = Image::zeroed(h.width as u32, h.height as u32, format)?;
    r.seek(h.data_offset)?;
    match h.kind {
        Kind::Palette => read_palette_pixels(&mut r, &h, &mut img.pixels)?,
        Kind::Rle8 | Kind::Rle4 => read_rle(&mut r, &h, &mut img.pixels)?,
        _ => read_direct(&mut r, &h, &mut img.pixels)?,
    }
    Ok((img, r.pos()))
}

/// The order rows are stored in: index `i` of the file is output row `row(i)`.
fn row(h: &Header, i: usize) -> usize {
    if h.top_down {
        i
    } else {
        h.height - 1 - i
    }
}

fn read_palette_pixels(r: &mut Reader, h: &Header, buf: &mut [u8]) -> Result<()> {
    let ch = if h.alpha { 4 } else { 3 };
    let pal = h.palette.as_deref().expect("palette kinds carry a palette");
    let bits = usize::from(h.bit_count);
    let row_bytes = (bits * h.width).div_ceil(32) * 4;
    let stride = h.width * ch;
    // A padded palette (alpha 0xff): a pixel is one 4-byte store. With three
    // channels the fourth byte lands on the next pixel's first byte, which
    // that pixel overwrites; a row's last pixel takes a 3-byte store.
    let mut pal4 = [[0u8, 0, 0, 0xff]; 256];
    for (q, p) in pal4.iter_mut().zip(pal.iter()) {
        q[..3].copy_from_slice(p);
    }
    if bits == 8 {
        // One index per byte.
        for i in 0..h.height {
            let src = &r.bytes(row_bytes)?[..h.width];
            let y = row(h, i);
            let dst = &mut buf[y * stride..(y + 1) * stride];
            if ch == 4 {
                for (px, &idx) in dst.chunks_exact_mut(4).zip(src) {
                    px.copy_from_slice(&pal4[usize::from(idx)]);
                }
            } else if let Some((&last, head)) = src.split_last() {
                let mut o = 0;
                for &idx in head {
                    dst[o..o + 4].copy_from_slice(&pal4[usize::from(idx)]);
                    o += 3;
                }
                dst[o..o + 3].copy_from_slice(&pal4[usize::from(last)][..3]);
            }
        }
        return Ok(());
    }
    // 1, 2 or 4 bits, each with its own copy of the loop so the pixels per
    // byte and every shift are constants.
    match (bits, ch) {
        (1, 3) => low_bit_rows::<1, 3>(r, h, buf, &pal4),
        (1, _) => low_bit_rows::<1, 4>(r, h, buf, &pal4),
        (2, 3) => low_bit_rows::<2, 3>(r, h, buf, &pal4),
        (2, _) => low_bit_rows::<2, 4>(r, h, buf, &pal4),
        (_, 3) => low_bit_rows::<4, 3>(r, h, buf, &pal4),
        _ => low_bit_rows::<4, 4>(r, h, buf, &pal4),
    }
}

/// Rows of 1, 2 or 4-bit palette indices (`BITS`, most significant first) as
/// `CH`-byte pixels (3, or 4 with alpha 0xff).
///
/// The row is walked a source byte at a time: its 8, 4 or 2 pixels go to
/// fixed offsets of one exactly-sized output cell, so there is no byte
/// index, shift computation or bounds check per pixel (25 Ir a pixel
/// before), and the alpha of an RGBA row comes with the padded palette
/// entry instead of from a pass over the whole image first. The pixels of a
/// row's last, partly used byte are written one at a time. Out of line, one
/// function per depth and layout, so the 8-bit loop in the caller keeps its
/// registers.
#[inline(never)]
fn low_bit_rows<const BITS: usize, const CH: usize>(
    r: &mut Reader,
    h: &Header,
    buf: &mut [u8],
    pal4: &[[u8; 4]; 256],
) -> Result<()> {
    let per_byte = 8 / BITS;
    let mask = (1u8 << BITS) - 1;
    let row_bytes = (BITS * h.width).div_ceil(32) * 4;
    let stride = h.width * CH;
    for i in 0..h.height {
        let src = r.bytes(row_bytes)?;
        let y = row(h, i);
        let dst = &mut buf[y * stride..(y + 1) * stride];
        let mut cells = dst.chunks_exact_mut(per_byte * CH);
        for (cell, &b) in cells.by_ref().zip(src) {
            for k in 0..per_byte {
                let e = &pal4[usize::from((b >> (8 - BITS * (k + 1))) & mask)];
                if CH == 4 || k + 1 < per_byte {
                    cell[k * CH..k * CH + 4].copy_from_slice(e);
                } else {
                    cell[k * CH..k * CH + 3].copy_from_slice(&e[..3]);
                }
            }
        }
        let tail = cells.into_remainder();
        if let Some(&b) = src.get(h.width / per_byte).filter(|_| !tail.is_empty()) {
            for (k, px) in tail.chunks_exact_mut(CH).enumerate() {
                let e = &pal4[usize::from((b >> (8 - BITS * (k + 1))) & mask)];
                px.copy_from_slice(&e[..CH]);
            }
        }
    }
    Ok(())
}

/// Uncompressed 16, 24 and 32-bit rows.
///
/// Each layout has a row loop of its own, out of line ([`direct_rows`]).
/// Inlined into `decode_mode` as one loop, the output buffer was a local
/// `Vec` instead of a `&mut [u8]` argument, so LLVM could not tell it from
/// the input and the 32-bit BGRA word loop did not vectorise (9.1 Ir a
/// pixel, 4.2 out of line); the caller's reader sat behind a pointer, so
/// every row's `bytes` and `skip` loaded, checked and stored its position;
/// the layout was chosen again for every row, and the per-pixel tests of
/// the shared loops (`ch == 4`, the kind) stayed inside; and any change to
/// one layout's loop re-rolled the registers of all the others.
#[inline(never)]
fn read_direct(r: &mut Reader, h: &Header, buf: &mut [u8]) -> Result<()> {
    let ch = if h.alpha { 4 } else { 3 };
    let (bytes_pp, pad) = match h.kind {
        Kind::Rgb16 | Kind::Bitfields16 => (2, h.width % 2 * 2),
        Kind::Rgb24 => (3, (4 - (h.width * 3) % 4) % 4),
        _ => (4, 0),
    };
    let fields = match h.kind {
        Kind::Rgb16 => Some(RGB555),
        _ => h.fields,
    };
    // Standard 8-8-8(-8) masks are plain byte moves.
    let std_masks = fields.is_some_and(|f| {
        bytes_pp == 4
            && f.r == Field { shift: 16, len: 8 }
            && f.g == Field { shift: 8, len: 8 }
            && f.b == Field { shift: 0, len: 8 }
            && (f.a.len == 0 || f.a == Field { shift: 24, len: 8 })
    });
    let std_alpha = std_masks && fields.is_some_and(|f| f.a.len != 0);
    let g = RowGeom {
        height: h.height,
        top_down: h.top_down,
        n: h.width * bytes_pp,
        pad,
        stride: h.width * ch,
    };
    let data = r.rest();
    let used = match (h.kind, ch) {
        (Kind::Rgb24, 3) => direct_rows(data, buf, g, crate::swizzle::swap_rb3)?,
        (Kind::Rgb32, 3) => direct_rows(data, buf, g, |dst, src| {
            for (px, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                px.copy_from_slice(&[s[2], s[1], s[0]]);
            }
        })?,
        (Kind::Bitfields32, 4) if std_masks => {
            // BGRA -> RGBA as one word: swap bytes 0 and 2, and force alpha
            // opaque (decided once, not per pixel) when the file has no
            // alpha mask.
            let opaque = if std_alpha { 0 } else { 0xff00_0000u32 };
            direct_rows(data, buf, g, move |dst, src| {
                for (px, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                    let v = v.swap_bytes().rotate_right(8) | opaque;
                    px.copy_from_slice(&v.to_le_bytes());
                }
            })?
        }
        (Kind::Bitfields32, _) if std_masks => direct_rows(data, buf, g, |dst, src| {
            for (px, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                px.copy_from_slice(&[s[2], s[1], s[0]]);
            }
        })?,
        // 24 and 32-bit `BI_RGB` rows of an icon entry, which carries alpha.
        (Kind::Rgb24, _) => direct_rows(data, buf, g, |dst, src| {
            for (px, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                px.copy_from_slice(&[s[2], s[1], s[0], 0xff]);
            }
        })?,
        (Kind::Rgb32, _) => direct_rows(data, buf, g, |dst, src| {
            for (px, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                px.copy_from_slice(&[s[2], s[1], s[0], 0xff]);
            }
        })?,
        (Kind::Rgba32, 3) => direct_rows(data, buf, g, |dst, src| {
            for (px, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                px.copy_from_slice(&[s[2], s[1], s[0]]);
            }
        })?,
        (Kind::Rgba32, _) => direct_rows(data, buf, g, |dst, src| {
            for (px, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                px.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
        })?,
        _ => {
            let f = fields.expect("bitfield kinds carry masks");
            read_bitfield_rows(data, buf, g, bytes_pp, ch, &f)?
        }
    };
    r.skip(used)
}

/// Where the stored rows of an uncompressed image go: `n` bytes of pixels
/// then `pad` bytes of padding per stored row, `stride` bytes per output
/// row, the first stored row at the bottom unless `top_down`.
#[derive(Clone, Copy)]
struct RowGeom {
    height: usize,
    top_down: bool,
    n: usize,
    pad: usize,
    stride: usize,
}

/// Every stored row converted into its output row by `f`. Reads from the
/// start of `data`; returns how many bytes the rows took. Out of line, one
/// copy per layout.
#[inline(never)]
fn direct_rows(
    data: &[u8],
    buf: &mut [u8],
    g: RowGeom,
    mut f: impl FnMut(&mut [u8], &[u8]),
) -> Result<usize> {
    let mut r = Reader::new(data);
    for i in 0..g.height {
        let src = r.bytes(g.n)?;
        r.skip(g.pad)?;
        let y = if g.top_down { i } else { g.height - 1 - i };
        f(&mut buf[y * g.stride..(y + 1) * g.stride], src);
    }
    Ok(r.pos())
}

/// The rows of a 16-bit image, or of a 32-bit one whose masks are not the
/// standard 8-8-8(-8), through [`FieldTables`]. Reads from the start of
/// `data`; returns how many bytes the rows took.
#[inline(never)]
fn read_bitfield_rows(
    data: &[u8],
    buf: &mut [u8],
    g: RowGeom,
    bytes_pp: usize,
    ch: usize,
    f: &Fields,
) -> Result<usize> {
    let t = FieldTables::new(f);
    let mut r = Reader::new(data);
    for i in 0..g.height {
        let src = r.bytes(g.n)?;
        r.skip(g.pad)?;
        let y = if g.top_down { i } else { g.height - 1 - i };
        let dst = &mut buf[y * g.stride..(y + 1) * g.stride];
        match (bytes_pp, ch) {
            (2, 3) => t.row::<2, 3>(dst, src),
            (2, _) => t.row::<2, 4>(dst, src),
            (_, 3) => t.row::<4, 3>(dst, src),
            _ => t.row::<4, 4>(dst, src),
        }
    }
    Ok(r.pos())
}

#[inline(never)]
fn read_rle(r: &mut Reader, h: &Header, buf: &mut [u8]) -> Result<()> {
    if h.kind == Kind::Rle4 {
        read_rle_as::<true>(r, h, buf)
    } else {
        read_rle_as::<false>(r, h, buf)
    }
}

/// RLE4 (`RLE4`) or RLE8 rows. One out-of-line copy per kind, so neither
/// kind's loop shares registers with the other's (a change to the RLE8
/// stores alone moved the RLE4 rows by +1.9% while they shared one body).
#[inline(never)]
fn read_rle_as<const RLE4: bool>(r: &mut Reader, h: &Header, buf: &mut [u8]) -> Result<()> {
    // The stream is consumed as a slice that shrinks from the front: every
    // operation starts with two bytes, taken with one length test, where two
    // `Reader::u8` calls were an overflow check and a range check each.
    let all = r.rest();
    let mut data = all;
    rle_rows::<RLE4>(&mut data, h, buf)?;
    r.skip(all.len() - data.len())
}

#[inline(always)]
fn rle_rows<const RLE4: bool>(data: &mut &[u8], h: &Header, buf: &mut [u8]) -> Result<()> {
    let ch = if h.alpha { 4 } else { 3 };
    let pal = h.palette.as_deref().expect("RLE kinds carry a palette");
    let rle4 = RLE4;
    let stride = h.width * ch;
    let corrupt = || Error::invalid("bmp: corrupt RLE data");
    // Zero pixels [from, width) of file row `ri`.
    let zero_from = |buf: &mut [u8], ri: usize, from: usize| {
        let y = row(h, ri);
        if from < h.width {
            buf[y * stride + from * ch..(y + 1) * stride].fill(0);
        }
    };
    // The palette padded to four bytes with a zero: a pixel is one 4-byte
    // store. With four channels that leaves alpha at the zero it had
    // (RLE never sets alpha); with three, the zero lands on the first byte
    // of the next pixel, which the same packet then writes, so only a
    // packet's last pixel needs a 3-byte store (the byte after it may
    // belong to a pixel a delta skips, or to the row decoded before).
    let mut pal4 = [[0u8; 4]; 256];
    for (q, p) in pal4.iter_mut().zip(pal.iter()) {
        q[..3].copy_from_slice(p);
    }

    let mut ri = 0;
    while ri < h.height {
        let mut px = 0usize;
        let mut x = 0usize;
        // Byte offset of this file row in `buf`, recomputed only when a
        // delta moves to another row (not per pixel, not per run).
        let mut rb = row(h, ri) * stride;
        loop {
            let Some((&[control, arg], rest)) = data.split_first_chunk::<2>() else {
                return Err(Error::Truncated);
            };
            *data = rest;
            if control == 0 {
                match arg {
                    0 => {
                        zero_from(buf, ri, px);
                        break;
                    }
                    1 => {
                        zero_from(buf, ri, px);
                        for rest in ri + 1..h.height {
                            zero_from(buf, rest, 0);
                        }
                        return Ok(());
                    }
                    2 => {
                        let Some((&[dx, dy], rest)) = data.split_first_chunk::<2>() else {
                            return Err(Error::Truncated);
                        };
                        *data = rest;
                        if dy > 0 {
                            zero_from(buf, ri, px);
                            for _ in 1..dy {
                                ri += 1;
                                if ri >= h.height {
                                    return Err(corrupt());
                                }
                                zero_from(buf, ri, 0);
                            }
                            ri += 1;
                            if ri >= h.height {
                                return Err(corrupt());
                            }
                            rb = row(h, ri) * stride;
                            px = 0;
                            for _ in 0..x {
                                if px >= h.width {
                                    return Err(corrupt());
                                }
                                let y = row(h, ri);
                                buf[y * stride + px * ch..y * stride + px * ch + ch].fill(0);
                                px += 1;
                            }
                        }
                        for _ in 0..dx {
                            if px >= h.width {
                                return Err(corrupt());
                            }
                            let y = row(h, ri);
                            buf[y * stride + px * ch..y * stride + px * ch + ch].fill(0);
                            px += 1;
                        }
                        x += usize::from(dx);
                    }
                    n => {
                        let n = usize::from(n);
                        let mut len = if rle4 { n.div_ceil(2) } else { n };
                        len += len & 1;
                        let Some((idx, rest)) = data.split_at_checked(len) else {
                            return Err(Error::Truncated);
                        };
                        *data = rest;
                        // A literal running past the row is corrupt (the
                        // whole decode fails, so no partial row matters).
                        if n > h.width - px {
                            return Err(corrupt());
                        }
                        if rle4 {
                            // Two pixels a byte, high nibble first, into the
                            // packet's own range of the row (it was a row
                            // offset computed and a 3-byte range-checked
                            // copy per nibble).
                            let o = rb + px * ch;
                            let dst = &mut buf[o..o + n * ch];
                            let mut q = 0;
                            'nib: for &b in idx {
                                for nib in [b >> 4, b & 15] {
                                    let c = &pal4[usize::from(nib)];
                                    if q + 4 > dst.len() {
                                        if q < dst.len() {
                                            dst[q..].copy_from_slice(&c[..3]);
                                        }
                                        break 'nib;
                                    }
                                    dst[q..q + 4].copy_from_slice(c);
                                    q += ch;
                                }
                            }
                            px += n;
                        } else {
                            let mut o = rb + px * ch;
                            for &i in &idx[..n] {
                                buf[o..o + 3].copy_from_slice(&pal[usize::from(i)]);
                                o += ch;
                            }
                            px += n;
                        }
                        x += n;
                    }
                }
            } else {
                let index = arg;
                let n = usize::from(control);
                if rle4 {
                    // The two nibbles alternate. A run past the row is
                    // corrupt (RLE8 clamps, below).
                    if n > h.width - px {
                        return Err(corrupt());
                    }
                    let c = [pal4[usize::from(index >> 4)], pal4[usize::from(index & 15)]];
                    let o = rb + px * ch;
                    let dst = &mut buf[o..o + n * ch];
                    let (mut q, mut k) = (0, 0);
                    while q + 4 <= dst.len() {
                        dst[q..q + 4].copy_from_slice(&c[k & 1]);
                        q += ch;
                        k += 1;
                    }
                    if q < dst.len() {
                        dst[q..].copy_from_slice(&c[k & 1][..3]);
                    }
                    px += n;
                } else {
                    // A run longer than the row is clamped to it (ImageMagick
                    // writes such files).
                    let take = n.min(h.width - px);
                    // Runs average about two pixels in real files: a plain
                    // offset loop, no per-run slice or chunk setup (that
                    // shape measured +15% Ir on the RLE8 fixture).
                    let c = pal[usize::from(index)];
                    let mut o = rb + px * ch;
                    for _ in 0..take {
                        buf[o..o + 3].copy_from_slice(&c);
                        o += ch;
                    }
                    px += take;
                }
                x += n;
            }
        }
        ri += 1;
    }
    Ok(())
}

// ── encode ───────────────────────────────────────────────────────────────────

/// The layout of `image` as a bottom-up DIB: its header fields and the size
/// of its pixel array. The pixels themselves are written straight into the
/// output after the headers by [`dib_pixels`] (building them in their own
/// Vec first and then appending it was a second full copy).
struct Dib {
    bit_count: u16,
    compression: u32,
    palette: Vec<u8>,
    row_bytes: usize,
    pixels_len: usize,
}

fn dib_body(image: &Image) -> Result<Dib> {
    let (w, h) = (image.width as usize, image.height as usize);
    if w == 0 || h == 0 || image.width > MAX_SIDE as u32 || image.height > MAX_SIDE as u32 {
        return Err(Error::unsupported(format!(
            "bmp: {}x{} (1 to {MAX_SIDE} pixels a side)",
            image.width, image.height
        )));
    }
    let (bit_count, compression, bpp) = match image.format {
        PixelFormat::L8 => (8u16, 0u32, 1usize),
        PixelFormat::Rgb8 => (24, 0, 3),
        PixelFormat::Rgba8 => (32, 3, 4),
        other => {
            return Err(Error::unsupported(format!(
                "bmp: encoding {other:?} (L8, Rgb8 and Rgba8 are stored without loss)"
            )))
        }
    };
    let row_bytes = (w * bpp).div_ceil(4) * 4;
    let palette = if image.format == PixelFormat::L8 {
        (0..=255u8).flat_map(|v| [v, v, v, 0]).collect()
    } else {
        Vec::new()
    };
    Ok(Dib {
        bit_count,
        compression,
        palette,
        row_bytes,
        pixels_len: row_bytes * h,
    })
}

/// Append `image`'s bottom-up pixel array (rows padded to 4 bytes) to `out`.
fn dib_pixels(image: &Image, d: &Dib, out: &mut Vec<u8>) {
    let (w, h) = (image.width as usize, image.height as usize);
    let ch = image.format.channels();
    let bpp = usize::from(d.bit_count) / 8;
    let base = out.len();
    out.resize(base + d.pixels_len, 0);
    let pixels = &mut out[base..];
    for y in 0..h {
        let src = &image.pixels[y * w * ch..(y + 1) * w * ch];
        let dst = &mut pixels[(h - 1 - y) * d.row_bytes..][..w * bpp];
        match image.format {
            PixelFormat::L8 => dst.copy_from_slice(src),
            PixelFormat::Rgb8 => crate::swizzle::swap_rb3(dst, src),
            _ => crate::swizzle::swap_rb4(dst, src),
        }
    }
}

const MASKS_RGBA: [u32; 4] = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000];

fn info_header(out: &mut Vec<u8>, size: u32, image: &Image, d: &Dib) {
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&d.bit_count.to_le_bytes());
    out.extend_from_slice(&d.compression.to_le_bytes());
    out.extend_from_slice(&(d.pixels_len as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // 72 dpi
    out.extend_from_slice(&2835u32.to_le_bytes());
    let colors = (d.palette.len() / 4) as u32;
    out.extend_from_slice(&colors.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

/// The V4/V5 fields after the INFO part: masks, colour space, endpoints and
/// gammas (and for V5 the intent and an empty profile).
fn v4_v5_tail(out: &mut Vec<u8>, v5: bool) {
    for m in MASKS_RGBA {
        out.extend_from_slice(&m.to_le_bytes());
    }
    out.extend_from_slice(b"BGRs"); // LCS_sRGB, little-endian 'sRGB'
    out.extend_from_slice(&[0u8; 36 + 12]); // endpoints, gamma
    if v5 {
        out.extend_from_slice(&4u32.to_le_bytes()); // LCS_GM_IMAGES
        out.extend_from_slice(&[0u8; 12]); // profile data, size, reserved
    }
}

/// Encode a `.bmp` file: `L8` as an 8-bit grey palette, `Rgb8` as 24-bit,
/// `Rgba8` as 32-bit `BI_BITFIELDS` with an alpha mask behind a
/// `BITMAPV4HEADER` (so the alpha survives, in `image` and in Windows).
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    let d = dib_body(image)?;
    let header = if image.format == PixelFormat::Rgba8 {
        V4
    } else {
        INFO
    };
    let offset = 14 + header as usize + d.palette.len();
    let total = offset + d.pixels_len;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(offset as u32).to_le_bytes());
    info_header(&mut out, header, image, &d);
    if header == V4 {
        v4_v5_tail(&mut out, false);
    }
    out.extend_from_slice(&d.palette);
    dib_pixels(image, &d, &mut out);
    Ok(out)
}

/// Which clipboard DIB to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DibKind {
    /// `CF_DIB`: a `BITMAPINFOHEADER`. `Rgba8` is written as 32-bit
    /// `BI_BITFIELDS`, but this header has no alpha mask, so readers see RGB.
    Dib,
    /// `CF_DIBV5`: a `BITMAPV5HEADER` with an alpha mask; `Rgba8` keeps its
    /// alpha.
    DibV5,
}

/// Encode a packed DIB for the clipboard (`CF_DIB` / `CF_DIBV5`). With
/// `BI_BITFIELDS`, the three colour masks follow the header as the
/// clipboard's packed-DIB layout expects, even after a V5 header that also
/// holds them, which is what Windows and [`decode_dib`] read.
pub fn encode_dib(image: &Image, kind: DibKind) -> Result<Vec<u8>> {
    let d = dib_body(image)?;
    let header = match kind {
        DibKind::Dib => INFO,
        DibKind::DibV5 => V5,
    };
    let mut out = Vec::with_capacity(header as usize + 12 + d.palette.len() + d.pixels_len);
    info_header(&mut out, header, image, &d);
    if header == V5 {
        v4_v5_tail(&mut out, true);
    }
    if d.compression == 3 {
        for m in &MASKS_RGBA[..3] {
            out.extend_from_slice(&m.to_le_bytes());
        }
    }
    out.extend_from_slice(&d.palette);
    dib_pixels(image, &d, &mut out);
    Ok(out)
}

/// Write an ICO entry's DIB: 32-bit `BI_RGB` (alpha in the fourth byte, as
/// icons store it) with a doubled height and an AND mask from the alpha.
pub(crate) fn encode_ico_dib(image: &Image) -> Result<Vec<u8>> {
    let rgba = match image.format {
        PixelFormat::Rgba8 => image.pixels.clone(),
        PixelFormat::Rgb8 => image
            .pixels
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 0xff])
            .collect(),
        other => {
            return Err(Error::unsupported(format!(
                "ico: encoding {other:?} (Rgb8 and Rgba8)"
            )))
        }
    };
    let (w, h) = (image.width as usize, image.height as usize);
    let mut out = Vec::new();
    out.extend_from_slice(&INFO.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&((h * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let mask_row = w.div_ceil(32) * 4;
    out.extend_from_slice(&((w * h * 4 + mask_row * h) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for y in (0..h).rev() {
        for p in rgba[y * w * 4..(y + 1) * w * 4].chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    for y in (0..h).rev() {
        let mut row = vec![0u8; mask_row];
        for x in 0..w {
            if rgba[(y * w + x) * 4 + 3] == 0 {
                row[x / 8] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table form of a bitfield layout gives every pixel the bytes
    /// `Field::read` gives it: every contiguous mask of a 16 and a 32-bit
    /// pixel, in each channel position, with and without an alpha channel.
    #[test]
    fn field_tables_match_field_read() {
        let mut seed = 0x2545_f491u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let other = Field { shift: 3, len: 5 };
        let none = Field { shift: 0, len: 0 };
        for bits in [16u32, 32] {
            for len in 1..=bits {
                for shift in 0..=bits - len {
                    let x = Field::from_mask((u32::MAX >> (32 - len)) << shift, bits).unwrap();
                    for which in 0..5 {
                        let mut f = Fields {
                            r: other,
                            g: other,
                            b: other,
                            a: if which == 4 { none } else { other },
                        };
                        match which {
                            0 => f.r = x,
                            1 => f.g = x,
                            2 => f.b = x,
                            3 => f.a = x,
                            _ => {}
                        }
                        let t = FieldTables::new(&f);
                        let s = (bits / 8) as usize;
                        let src: Vec<u8> = (0..64).flat_map(|_| next().to_le_bytes()).collect();
                        for ch in [3usize, 4] {
                            let mut got = vec![0u8; src.len() / s * ch];
                            match (s, ch) {
                                (2, 3) => t.row::<2, 3>(&mut got, &src),
                                (2, _) => t.row::<2, 4>(&mut got, &src),
                                (_, 3) => t.row::<4, 3>(&mut got, &src),
                                _ => t.row::<4, 4>(&mut got, &src),
                            }
                            for (px, p) in got.chunks_exact(ch).zip(src.chunks_exact(s)) {
                                let v = if s == 2 {
                                    u32::from(u16::from_le_bytes([p[0], p[1]]))
                                } else {
                                    u32::from_le_bytes([p[0], p[1], p[2], p[3]])
                                };
                                let a = if f.a.len != 0 { f.a.read(v) } else { 0xff };
                                let want = [f.r.read(v), f.g.read(v), f.b.read(v), a];
                                assert_eq!(px, &want[..ch], "{bits}-bit {f:?} pixel {v:#x}");
                            }
                        }
                    }
                }
            }
        }
    }
}
