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
    r.seek(header_end + mask_bytes)?;

    let mut palette = None;
    if matches!(kind, Kind::Palette | Kind::Rle4 | Kind::Rle8) {
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
        data_offset = r.pos();
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
    if ch == 4 {
        buf.chunks_exact_mut(4).for_each(|p| p[3] = 0xff);
    }
    let bits = usize::from(h.bit_count);
    let row_bytes = (bits * h.width).div_ceil(32) * 4;
    let stride = h.width * ch;
    for i in 0..h.height {
        let src = r.bytes(row_bytes)?;
        let y = row(h, i);
        let dst = &mut buf[y * stride..(y + 1) * stride];
        let per_byte = 8 / bits;
        let mask = ((1u16 << bits) - 1) as u8;
        for (x, px) in dst.chunks_exact_mut(ch).enumerate() {
            let byte = src[x / per_byte];
            let shift = 8 - bits * (x % per_byte + 1);
            let idx = (byte >> shift) & mask;
            px[..3].copy_from_slice(&pal[idx as usize]);
        }
    }
    Ok(())
}

fn read_direct(r: &mut Reader, h: &Header, buf: &mut [u8]) -> Result<()> {
    let ch = if h.alpha { 4 } else { 3 };
    let stride = h.width * ch;
    let (bytes_pp, pad) = match h.kind {
        Kind::Rgb16 | Kind::Bitfields16 => (2, h.width % 2 * 2),
        Kind::Rgb24 => (3, (4 - (h.width * 3) % 4) % 4),
        _ => (4, 0),
    };
    let fields = match h.kind {
        Kind::Rgb16 => Some(RGB555),
        _ => h.fields,
    };
    for i in 0..h.height {
        let src = r.bytes(h.width * bytes_pp)?;
        r.skip(pad)?;
        let y = row(h, i);
        let dst = &mut buf[y * stride..(y + 1) * stride];
        for (px, s) in dst.chunks_exact_mut(ch).zip(src.chunks_exact(bytes_pp)) {
            match h.kind {
                Kind::Rgb24 | Kind::Rgb32 | Kind::Rgba32 => {
                    px[0] = s[2];
                    px[1] = s[1];
                    px[2] = s[0];
                    if ch == 4 {
                        px[3] = if h.kind == Kind::Rgba32 { s[3] } else { 0xff };
                    }
                }
                _ => {
                    let f = fields.expect("bitfield kinds carry masks");
                    let v = if bytes_pp == 2 {
                        u32::from(u16::from_le_bytes([s[0], s[1]]))
                    } else {
                        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
                    };
                    px[0] = f.r.read(v);
                    px[1] = f.g.read(v);
                    px[2] = f.b.read(v);
                    if ch == 4 {
                        px[3] = if f.a.len != 0 { f.a.read(v) } else { 0xff };
                    }
                }
            }
        }
    }
    Ok(())
}

fn read_rle(r: &mut Reader, h: &Header, buf: &mut [u8]) -> Result<()> {
    let ch = if h.alpha { 4 } else { 3 };
    let pal = h.palette.as_deref().expect("RLE kinds carry a palette");
    let rle4 = h.kind == Kind::Rle4;
    let stride = h.width * ch;
    let corrupt = || Error::invalid("bmp: corrupt RLE data");
    // Zero pixels [from, width) of file row `ri`.
    let zero_from = |buf: &mut [u8], ri: usize, from: usize| {
        let y = row(h, ri);
        if from < h.width {
            buf[y * stride + from * ch..(y + 1) * stride].fill(0);
        }
    };
    let set = |buf: &mut [u8], ri: usize, px: usize, idx: u8| {
        let y = row(h, ri);
        let o = y * stride + px * ch;
        buf[o..o + 3].copy_from_slice(&pal[idx as usize]);
    };

    let mut ri = 0;
    while ri < h.height {
        let mut px = 0usize;
        let mut x = 0usize;
        loop {
            let control = r.u8()?;
            if control == 0 {
                match r.u8()? {
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
                        let dx = r.u8()?;
                        let dy = r.u8()?;
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
                        let idx = r.bytes(len)?;
                        if rle4 {
                            let mut left = n;
                            'nib: for &b in idx {
                                for nib in [b >> 4, b & 15] {
                                    if left == 0 {
                                        break 'nib;
                                    }
                                    if px >= h.width {
                                        return Err(corrupt());
                                    }
                                    set(buf, ri, px, nib);
                                    px += 1;
                                    left -= 1;
                                }
                            }
                        } else {
                            for &i in idx.iter().take(n) {
                                if px >= h.width {
                                    return Err(corrupt());
                                }
                                set(buf, ri, px, i);
                                px += 1;
                            }
                        }
                        x += n;
                    }
                }
            } else {
                let index = r.u8()?;
                let n = usize::from(control);
                if rle4 {
                    let mut left = n;
                    'run: loop {
                        for nib in [index >> 4, index & 15] {
                            if left == 0 {
                                break 'run;
                            }
                            if px >= h.width {
                                return Err(corrupt());
                            }
                            set(buf, ri, px, nib);
                            px += 1;
                            left -= 1;
                        }
                    }
                } else {
                    // A run longer than the row is clamped to it (ImageMagick
                    // writes such files).
                    let take = n.min(h.width - px);
                    for _ in 0..take {
                        set(buf, ri, px, index);
                        px += 1;
                    }
                }
                x += n;
            }
        }
        ri += 1;
    }
    Ok(())
}

// ── encode ───────────────────────────────────────────────────────────────────

/// The pixel array of `image` as a bottom-up DIB, and its header fields.
struct Dib {
    bit_count: u16,
    compression: u32,
    palette: Vec<u8>,
    pixels: Vec<u8>,
}

fn dib_body(image: &Image) -> Result<Dib> {
    let (w, h) = (image.width as usize, image.height as usize);
    if w == 0 || h == 0 || image.width > MAX_SIDE as u32 || image.height > MAX_SIDE as u32 {
        return Err(Error::unsupported(format!(
            "bmp: {}x{} (1 to {MAX_SIDE} pixels a side)",
            image.width, image.height
        )));
    }
    let ch = image.format.channels();
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
    let mut pixels = vec![0u8; row_bytes * h];
    for y in 0..h {
        let src = &image.pixels[y * w * ch..(y + 1) * w * ch];
        let dst = &mut pixels[(h - 1 - y) * row_bytes..][..w * bpp];
        match image.format {
            PixelFormat::L8 => dst.copy_from_slice(src),
            PixelFormat::Rgb8 => {
                for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(3)) {
                    d.copy_from_slice(&[s[2], s[1], s[0]]);
                }
            }
            _ => {
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
                }
            }
        }
    }
    let palette = if image.format == PixelFormat::L8 {
        (0..=255u8).flat_map(|v| [v, v, v, 0]).collect()
    } else {
        Vec::new()
    };
    Ok(Dib {
        bit_count,
        compression,
        palette,
        pixels,
    })
}

const MASKS_RGBA: [u32; 4] = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000];

fn info_header(out: &mut Vec<u8>, size: u32, image: &Image, d: &Dib) {
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&d.bit_count.to_le_bytes());
    out.extend_from_slice(&d.compression.to_le_bytes());
    out.extend_from_slice(&(d.pixels.len() as u32).to_le_bytes());
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
    let total = offset + d.pixels.len();
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
    out.extend_from_slice(&d.pixels);
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
    let mut out = Vec::with_capacity(header as usize + 12 + d.palette.len() + d.pixels.len());
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
    out.extend_from_slice(&d.pixels);
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
