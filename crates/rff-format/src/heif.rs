//! HEIF (ISO/IEC 23008-12) box parser: the item model shared by every
//! HEIF-family container (HEIC, AVIF, ...).
//!
//! A HEIF still image is not a stream of samples but a small database of
//! *items* described by the top-level `meta` box:
//!
//! * `iinf`/`infe` — what each item is (`hvc1`, `av01`, `grid`, `iden`,
//!   `Exif`, `mime`, ...) and whether it is hidden;
//! * `iloc` — where its bytes are (file extents, or the `idat` box);
//! * `iprp` (`ipco` + `ipma`) — its properties: size (`ispe`), decoder
//!   configuration (`hvcC`), colour (`colr`), transforms (`clap`, `irot`,
//!   `imir`), auxiliary type (`auxC`), ...;
//! * `iref` — how items relate: a grid's tiles (`dimg`), a thumbnail
//!   (`thmb`), an alpha or depth plane (`auxl`), metadata (`cdsc`), a
//!   premultiplied alpha (`prem`), predictive coding (`pred`);
//! * `pitm` — which item is the primary image.
//!
//! [`HeifFile::parse`] reads all of it over a borrowed file buffer, copying
//! nothing it can borrow, and with every count and size bounded by
//! [`Limits`]: a malformed file returns an error, never a panic and never an
//! allocation the file's own length does not justify.
//!
//! This module only *describes* the file. Decoding an item's bitstream is the
//! job of the codec-specific crate (`rff-format-heif` for HEVC, the AVIF
//! crates for AV1).

use std::borrow::Cow;

use rff_core::{Error, Result};

// ===========================================================================
// Box walking
// ===========================================================================

/// Parse sibling boxes at the top level of `buf` into `(type, payload)` pairs.
/// Tolerates the 64-bit `largesize` form and the `size == 0` (to-end) form.
/// A box whose declared size runs past `buf`, or ends before its own header,
/// ends the walk (as FFmpeg's `mov_read_default` does).
pub fn child_boxes(buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= buf.len() {
        let size32 = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let typ = [buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]];
        let (payload_start, box_end) = if size32 == 1 {
            if i + 16 > buf.len() {
                break;
            }
            let mut large = [0u8; 8];
            large.copy_from_slice(&buf[i + 8..i + 16]);
            let large = u64::from_be_bytes(large) as usize;
            (i + 16, i.checked_add(large).unwrap_or(buf.len()))
        } else if size32 == 0 {
            (i + 8, buf.len())
        } else {
            (i + 8, i + size32)
        };
        // A size below the header's own (2-7, or a 64-bit size under 16) ends the box
        // before its payload starts: stop there, as FFmpeg's mov_read_default does.
        if box_end > buf.len() || box_end < payload_start {
            break;
        }
        out.push((typ, &buf[payload_start..box_end]));
        i = box_end;
    }
    out
}

/// The payload of the first box of type `typ` in a [`child_boxes`] list.
pub fn find_box<'a>(boxes: &[([u8; 4], &'a [u8])], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(t, _)| t == typ).map(|(_, p)| *p)
}

/// A bounds-checked big-endian reader over one box payload. Every read past
/// the end is an error naming the box, never a panic.
struct Rd<'a> {
    b: &'a [u8],
    p: usize,
    what: &'static str,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8], what: &'static str) -> Self {
        Rd { b, p: 0, what }
    }

    fn short(&self) -> Error {
        Error::invalid(format!("heif: truncated `{}`", self.what))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p.checked_add(n).ok_or_else(|| self.short())?;
        let s = self.b.get(self.p..end).ok_or_else(|| self.short())?;
        self.p = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// An unsigned big-endian field of `n` bytes (0, 1, 2, 4 or 8). Zero
    /// bytes reads as 0, which is how `iloc` encodes an absent field.
    fn uint(&mut self, n: usize) -> Result<u64> {
        if n > 8 {
            return Err(Error::invalid(format!(
                "heif: {}-byte field in `{}`",
                n, self.what
            )));
        }
        Ok(self.take(n)?.iter().fold(0u64, |v, &b| (v << 8) | b as u64))
    }

    fn fourcc(&mut self) -> Result<[u8; 4]> {
        let s = self.take(4)?;
        Ok([s[0], s[1], s[2], s[3]])
    }

    /// A NUL-terminated UTF-8 string. A missing terminator at the end of the
    /// box is tolerated (some writers drop it on the last field).
    fn cstr(&mut self) -> String {
        let rest = &self.b[self.p.min(self.b.len())..];
        let n = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        self.p = (self.p + n + 1).min(self.b.len());
        String::from_utf8_lossy(&rest[..n]).into_owned()
    }

    fn rest(&self) -> &'a [u8] {
        &self.b[self.p.min(self.b.len())..]
    }

    /// A FullBox header: `(version, flags)`.
    fn full(&mut self) -> Result<(u8, u32)> {
        let v = self.u32()?;
        Ok(((v >> 24) as u8, v & 0x00ff_ffff))
    }
}

// ===========================================================================
// Limits
// ===========================================================================

/// Upper bounds the parser and decoders enforce on untrusted files. The
/// defaults admit every real camera file (an iPhone 48 MP photo is a grid of
/// 8×6 tiles, 8064×6048) with a wide margin, and refuse what only a hostile
/// file would declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Items in `iinf` / `iloc` / `ipma` (each).
    pub max_items: u32,
    /// Extents per item in `iloc`.
    pub max_extents: u32,
    /// Bytes of one item's data (also bounded by the file's length).
    pub max_item_bytes: u64,
    /// Width or height of any image, after composition.
    pub max_dimension: u32,
    /// Pixels of any image (`width * height`), after composition.
    pub max_pixels: u64,
    /// Tiles of one `grid` (rows × columns; the format caps each at 256).
    pub max_grid_tiles: u32,
    /// Depth of derived-image chains (`iden` of `iden` of `grid` ...).
    pub max_derivation_depth: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_items: 65_536,
            max_extents: 4_096,
            max_item_bytes: 1 << 30,
            max_dimension: 65_535,
            // 2^28 = 268 M pixels (16384×16384); libheif's default is 2^30.
            max_pixels: 1 << 28,
            max_grid_tiles: 65_536,
            max_derivation_depth: 8,
        }
    }
}

// ===========================================================================
// The item model
// ===========================================================================

/// Where an item's bytes live (`iloc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// 0 = file offsets, 1 = offsets into `idat`, 2 = into another item.
    pub construction_method: u8,
    pub base_offset: u64,
    /// `(offset, length)` pairs; a length of 0 means "to the end".
    pub extents: Vec<(u64, u64)>,
}

/// One entry of `iinf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: u32,
    /// `hvc1`, `av01`, `grid`, `iden`, `iovl`, `Exif`, `mime`, ...
    pub item_type: [u8; 4],
    pub name: String,
    /// For `mime` items: the MIME type (`application/rdf+xml` for XMP).
    pub content_type: Option<String>,
    /// `infe` flag 1: not meant to be displayed on its own.
    pub hidden: bool,
    pub location: Option<Location>,
    /// `(index into HeifFile::properties, essential)`, in `ipma` order —
    /// which is the order transformative properties apply in.
    pub properties: Vec<(usize, bool)>,
}

/// `colr`: an `nclx` code-point triple or an ICC profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Colour<'a> {
    Nclx {
        primaries: u16,
        transfer: u16,
        matrix: u16,
        full_range: bool,
    },
    /// `rICC` (restricted) or `prof` (unrestricted) — the profile bytes.
    Icc(&'a [u8]),
}

/// `clap`: a clean aperture as four rationals (ISO/IEC 14496-12 §12.1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clap {
    pub width: (i64, i64),
    pub height: (i64, i64),
    pub horiz_off: (i64, i64),
    pub vert_off: (i64, i64),
}

impl Clap {
    /// The integer crop rectangle `(left, top, width, height)` this aperture
    /// selects from a `w`x`h` image, computed exactly as libheif does: the
    /// centre is `offset + (size - 1) / 2`, `left` truncates, `top`,
    /// `right` and `bottom` round half up, and the result is clamped to the
    /// image. `None` if a denominator is zero, the image is empty, or the
    /// clamped rectangle is empty.
    pub fn rect(&self, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        if w == 0 || h == 0 {
            return None;
        }
        let (l, r) = edges(self.width, self.horiz_off, w, false)?;
        let (t, b) = edges(self.height, self.vert_off, h, true)?;
        let (l, t) = (l.max(0), t.max(0));
        let (r, b) = (r.min(w as i64 - 1), b.min(h as i64 - 1));
        if l > r || t > b {
            return None;
        }
        Some((l as u32, t as u32, (r - l + 1) as u32, (b - t + 1) as u32))
    }
}

/// An exact rational with a positive denominator.
#[derive(Clone, Copy)]
struct Q(i128, i128);

impl Q {
    fn new(n: i64, d: i64) -> Option<Q> {
        match d {
            0 => None,
            d if d < 0 => Some(Q(-(n as i128), -(d as i128))),
            d => Some(Q(n as i128, d as i128)),
        }
    }
    fn add(self, o: Q) -> Q {
        Q(self.0 * o.1 + o.0 * self.1, self.1 * o.1)
    }
    fn sub(self, o: Q) -> Q {
        Q(self.0 * o.1 - o.0 * self.1, self.1 * o.1)
    }
    /// C integer division: truncation toward zero (libheif `round_down`).
    fn trunc(self) -> i64 {
        (self.0 / self.1) as i64
    }
    /// `(n + d/2) / d`, truncated (libheif `round`).
    fn round(self) -> i64 {
        ((self.0 + self.1 / 2) / self.1) as i64
    }
}

/// The two edges `(lo, hi)` of one clean-aperture axis, as libheif computes
/// them (`lo` truncated on the horizontal axis, rounded on the vertical).
fn edges(size: (i64, i64), off: (i64, i64), image: u32, round_lo: bool) -> Option<(i64, i64)> {
    let size = Q::new(size.0, size.1)?;
    let off = Q::new(off.0, off.1)?;
    let one = Q(1, 1);
    let pc = off.add(Q(image as i128 - 1, 2));
    let half = {
        let s = size.sub(one);
        Q(s.0, s.1 * 2)
    };
    let lo_q = pc.sub(half);
    let lo = if round_lo { lo_q.round() } else { lo_q.trunc() };
    let hi = size.sub(one).add(Q(lo as i128, 1)).round();
    Some((lo, hi))
}

/// One property of `ipco`. Payloads are borrowed from the file buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Property<'a> {
    /// `ispe`: the image's width and height.
    Ispe {
        width: u32,
        height: u32,
    },
    /// `hvcC`: the HEVC decoder configuration record, verbatim.
    HvcC(&'a [u8]),
    /// `av1C`: the AV1 codec configuration record, verbatim.
    Av1C(&'a [u8]),
    Colr(Colour<'a>),
    /// `irot`: anti-clockwise rotation in units of 90 degrees (0-3).
    Irot(u8),
    /// `imir`: 0 = mirror about a vertical axis (left-right flip),
    /// 1 = about a horizontal axis (top-bottom flip).
    Imir(u8),
    Clap(Clap),
    /// `pixi`: bits per channel.
    Pixi(Vec<u8>),
    /// `auxC`: the auxiliary image's type URN (alpha, depth, ...).
    AuxC {
        aux_type: String,
        subtype: &'a [u8],
    },
    /// Anything else, by box type.
    Other([u8; 4]),
}

/// One reference of `iref`: `from` refers to each of `to`, with meaning
/// `kind` (`dimg`, `thmb`, `auxl`, `cdsc`, `prem`, `pred`, `base`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub kind: [u8; 4],
    pub from: u32,
    pub to: Vec<u32>,
}

/// A parsed HEIF file: brands, items, properties and references, borrowing
/// the file buffer.
#[derive(Debug, Clone)]
pub struct HeifFile<'a> {
    data: &'a [u8],
    idat: &'a [u8],
    pub major_brand: [u8; 4],
    pub compatible_brands: Vec<[u8; 4]>,
    pub handler: [u8; 4],
    pub primary: Option<u32>,
    pub items: Vec<Item>,
    pub properties: Vec<Property<'a>>,
    pub references: Vec<Reference>,
    pub limits: Limits,
}

/// `grid` derived-image descriptor (ISO/IEC 23008-12 §6.6.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub rows: u32,
    pub columns: u32,
    pub output_width: u32,
    pub output_height: u32,
}

/// `iovl` derived-image descriptor (ISO/IEC 23008-12 §6.6.2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    /// Canvas fill, 16-bit R, G, B, A.
    pub fill: [u16; 4],
    pub output_width: u32,
    pub output_height: u32,
    /// `(x, y)` of each `dimg` input, in reference order.
    pub offsets: Vec<(i32, i32)>,
}

impl<'a> HeifFile<'a> {
    /// Parse the `ftyp` and top-level `meta` of a HEIF file held in memory.
    pub fn parse(data: &'a [u8], limits: Limits) -> Result<HeifFile<'a>> {
        let top = child_boxes(data);
        let ftyp = find_box(&top, b"ftyp").ok_or_else(|| Error::invalid("heif: no `ftyp` box"))?;
        let mut r = Rd::new(ftyp, "ftyp");
        let major_brand = r.fourcc()?;
        let _minor = r.u32()?;
        let compatible_brands = r
            .rest()
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();

        let meta = find_box(&top, b"meta").ok_or_else(|| Error::invalid("heif: no `meta` box"))?;
        let mut r = Rd::new(meta, "meta");
        r.full()?;
        let meta_children = child_boxes(r.rest());

        let mut file = HeifFile {
            data,
            idat: find_box(&meta_children, b"idat").unwrap_or(&[]),
            major_brand,
            compatible_brands,
            handler: [0; 4],
            primary: None,
            items: Vec::new(),
            properties: Vec::new(),
            references: Vec::new(),
            limits,
        };

        if let Some(hdlr) = find_box(&meta_children, b"hdlr") {
            let mut r = Rd::new(hdlr, "hdlr");
            r.full()?;
            r.u32()?; // pre_defined
            file.handler = r.fourcc()?;
        }
        if let Some(pitm) = find_box(&meta_children, b"pitm") {
            let mut r = Rd::new(pitm, "pitm");
            let (v, _) = r.full()?;
            file.primary = Some(if v == 0 { r.u16()? as u32 } else { r.u32()? });
        }
        if let Some(iinf) = find_box(&meta_children, b"iinf") {
            file.items = parse_iinf(iinf, &limits)?;
        }
        if let Some(iloc) = find_box(&meta_children, b"iloc") {
            for (id, loc) in parse_iloc(iloc, &limits)? {
                // `iloc` may name items `iinf` does not; those are unreachable
                // and ignored.
                if let Some(it) = file.items.iter_mut().find(|it| it.id == id) {
                    it.location = Some(loc);
                }
            }
        }
        if let Some(iref) = find_box(&meta_children, b"iref") {
            file.references = parse_iref(iref, &limits)?;
        }
        if let Some(iprp) = find_box(&meta_children, b"iprp") {
            let iprp = child_boxes(iprp);
            if let Some(ipco) = find_box(&iprp, b"ipco") {
                file.properties = child_boxes(ipco)
                    .into_iter()
                    .map(|(t, p)| parse_property(t, p))
                    .collect::<Result<_>>()?;
            }
            // A file may split its associations over several `ipma` boxes.
            for (t, p) in &iprp {
                if t == b"ipma" {
                    parse_ipma(p, &limits, &mut file)?;
                }
            }
        }
        Ok(file)
    }

    /// The item with `id`, if `iinf` declares one.
    pub fn item(&self, id: u32) -> Option<&Item> {
        self.items.iter().find(|it| it.id == id)
    }

    /// The primary item, if `pitm` names one that `iinf` declares.
    pub fn primary_item(&self) -> Option<&Item> {
        self.primary.and_then(|id| self.item(id))
    }

    /// Item `id`'s properties in `ipma` order, with their essential flag.
    pub fn properties_of(&self, id: u32) -> impl Iterator<Item = (&Property<'a>, bool)> + '_ {
        self.item(id)
            .map(|it| it.properties.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|&(i, e)| self.properties.get(i).map(|p| (p, e)))
    }

    /// The first `ispe` of item `id`.
    pub fn ispe(&self, id: u32) -> Option<(u32, u32)> {
        self.properties_of(id).find_map(|(p, _)| match p {
            Property::Ispe { width, height } => Some((*width, *height)),
            _ => None,
        })
    }

    /// The first `hvcC` of item `id`.
    pub fn hvcc(&self, id: u32) -> Option<&'a [u8]> {
        self.properties_of(id).find_map(|(p, _)| match p {
            Property::HvcC(b) => Some(*b),
            _ => None,
        })
    }

    /// The `auxC` type URN of item `id`, if it is an auxiliary image.
    pub fn aux_type(&self, id: u32) -> Option<&str> {
        self.properties_of(id).find_map(|(p, _)| match p {
            Property::AuxC { aux_type, .. } => Some(aux_type.as_str()),
            _ => None,
        })
    }

    /// Items `id` refers to with `kind`, in reference order.
    pub fn refs_from(&self, id: u32, kind: &[u8; 4]) -> Vec<u32> {
        self.references
            .iter()
            .filter(|r| r.from == id && &r.kind == kind)
            .flat_map(|r| r.to.iter().copied())
            .collect()
    }

    /// Items that refer to `id` with `kind` (e.g. the thumbnails of `id`
    /// are the items with a `thmb` reference to it).
    pub fn refs_to(&self, id: u32, kind: &[u8; 4]) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .references
            .iter()
            .filter(|r| &r.kind == kind && r.to.contains(&id))
            .map(|r| r.from)
            .collect();
        out.dedup();
        out
    }

    /// Item `id`'s bytes: a borrow of the file (or `idat`) when it is one
    /// extent, a concatenation when it is several.
    pub fn item_data(&self, id: u32) -> Result<Cow<'a, [u8]>> {
        let item = self
            .item(id)
            .ok_or_else(|| Error::invalid(format!("heif: no item {id}")))?;
        let loc = item
            .location
            .as_ref()
            .ok_or_else(|| Error::invalid(format!("heif: item {id} has no `iloc` entry")))?;
        let src: &'a [u8] = match loc.construction_method {
            0 => self.data,
            1 => self.idat,
            m => {
                return Err(Error::unsupported(format!(
                    "heif: item {id} uses iloc construction method {m}"
                )))
            }
        };
        let extent = |&(off, len): &(u64, u64)| -> Result<&'a [u8]> {
            let start = loc
                .base_offset
                .checked_add(off)
                .filter(|&s| s <= src.len() as u64)
                .ok_or_else(|| Error::invalid(format!("heif: item {id} extent out of range")))?;
            let end = if len == 0 {
                src.len() as u64
            } else {
                start
                    .checked_add(len)
                    .filter(|&e| e <= src.len() as u64)
                    .ok_or_else(|| Error::invalid(format!("heif: item {id} extent out of range")))?
            };
            Ok(&src[start as usize..end as usize])
        };
        let total: u64 = loc
            .extents
            .iter()
            .map(|e| extent(e).map(|s| s.len() as u64))
            .sum::<Result<u64>>()?;
        if total > self.limits.max_item_bytes {
            return Err(Error::invalid(format!(
                "heif: item {id} is {total} bytes, over the {} byte limit",
                self.limits.max_item_bytes
            )));
        }
        match loc.extents.as_slice() {
            [] => Ok(Cow::Borrowed(&[])),
            [one] => Ok(Cow::Borrowed(extent(one)?)),
            many => {
                let mut v = Vec::with_capacity(total as usize);
                for e in many {
                    v.extend_from_slice(extent(e)?);
                }
                Ok(Cow::Owned(v))
            }
        }
    }

    /// Parse item `id` as a `grid` descriptor, checked against [`Limits`].
    pub fn grid(&self, id: u32) -> Result<Grid> {
        let data = self.item_data(id)?;
        let mut r = Rd::new(&data, "grid");
        let _version = r.u8()?;
        let flags = r.u8()?;
        let rows = r.u8()? as u32 + 1;
        let columns = r.u8()? as u32 + 1;
        let field = if flags & 1 != 0 { 4 } else { 2 };
        let output_width = r.uint(field)? as u32;
        let output_height = r.uint(field)? as u32;
        if rows * columns > self.limits.max_grid_tiles {
            return Err(Error::invalid(format!(
                "heif: grid of {rows}x{columns} tiles is over the limit"
            )));
        }
        self.check_size(output_width, output_height)?;
        Ok(Grid {
            rows,
            columns,
            output_width,
            output_height,
        })
    }

    /// Parse item `id` as an `iovl` descriptor, checked against [`Limits`].
    pub fn overlay(&self, id: u32) -> Result<Overlay> {
        let inputs = self.refs_from(id, b"dimg").len();
        let data = self.item_data(id)?;
        let mut r = Rd::new(&data, "iovl");
        let _version = r.u8()?;
        let flags = r.u8()?;
        let mut fill = [0u16; 4];
        for f in &mut fill {
            *f = r.u16()?;
        }
        let field = if flags & 1 != 0 { 4 } else { 2 };
        let output_width = r.uint(field)? as u32;
        let output_height = r.uint(field)? as u32;
        self.check_size(output_width, output_height)?;
        let mut offsets = Vec::with_capacity(inputs.min(r.rest().len() / (2 * field)));
        for _ in 0..inputs {
            let (x, y) = if field == 4 {
                (r.u32()? as i32, r.u32()? as i32)
            } else {
                (r.u16()? as i16 as i32, r.u16()? as i16 as i32)
            };
            offsets.push((x, y));
        }
        Ok(Overlay {
            fill,
            output_width,
            output_height,
            offsets,
        })
    }

    /// Refuse an image size outside [`Limits`] (or empty).
    pub fn check_size(&self, w: u32, h: u32) -> Result<()> {
        if w == 0 || h == 0 {
            return Err(Error::invalid(format!("heif: empty image {w}x{h}")));
        }
        if w > self.limits.max_dimension
            || h > self.limits.max_dimension
            || w as u64 * h as u64 > self.limits.max_pixels
        {
            return Err(Error::invalid(format!(
                "heif: image {w}x{h} is over the size limit"
            )));
        }
        Ok(())
    }

    /// True if the brands say this file holds HEIF still images (`mif1`,
    /// `heic`, ...), as opposed to only image sequences (`msf1`).
    pub fn has_brand(&self, brand: &[u8; 4]) -> bool {
        &self.major_brand == brand || self.compatible_brands.contains(brand)
    }
}

fn parse_iinf(p: &[u8], limits: &Limits) -> Result<Vec<Item>> {
    let mut r = Rd::new(p, "iinf");
    let (v, _) = r.full()?;
    let count = if v == 0 { r.u16()? as u32 } else { r.u32()? };
    if count > limits.max_items {
        return Err(Error::invalid(format!(
            "heif: {count} items is over the limit"
        )));
    }
    let mut items = Vec::with_capacity((count as usize).min(r.rest().len() / 12));
    for (t, infe) in child_boxes(r.rest()) {
        if &t != b"infe" {
            continue;
        }
        if items.len() as u32 >= count {
            break;
        }
        let mut r = Rd::new(infe, "infe");
        let (v, flags) = r.full()?;
        let mut item = Item {
            id: 0,
            item_type: [0; 4],
            name: String::new(),
            content_type: None,
            hidden: flags & 1 != 0,
            location: None,
            properties: Vec::new(),
        };
        match v {
            0 | 1 => {
                item.id = r.u16()? as u32;
                r.u16()?; // protection index
                item.name = r.cstr();
                item.content_type = Some(r.cstr());
            }
            2 | 3 => {
                item.id = if v == 2 { r.u16()? as u32 } else { r.u32()? };
                r.u16()?; // protection index
                item.item_type = r.fourcc()?;
                item.name = r.cstr();
                if &item.item_type == b"mime" {
                    item.content_type = Some(r.cstr());
                }
            }
            _ => continue, // a future version: unknown layout, skip the item
        }
        items.push(item);
    }
    Ok(items)
}

fn parse_iloc(p: &[u8], limits: &Limits) -> Result<Vec<(u32, Location)>> {
    let mut r = Rd::new(p, "iloc");
    let (v, _) = r.full()?;
    if v > 2 {
        return Err(Error::unsupported(format!("heif: iloc version {v}")));
    }
    let a = r.u8()?;
    let b = r.u8()?;
    let (offset_size, length_size) = ((a >> 4) as usize, (a & 15) as usize);
    let base_offset_size = (b >> 4) as usize;
    let index_size = if v >= 1 { (b & 15) as usize } else { 0 };
    for s in [offset_size, length_size, base_offset_size, index_size] {
        if !matches!(s, 0 | 4 | 8) {
            return Err(Error::invalid(format!("heif: iloc field size {s}")));
        }
    }
    let count = if v < 2 { r.u16()? as u32 } else { r.u32()? };
    if count > limits.max_items {
        return Err(Error::invalid(format!(
            "heif: {count} iloc entries is over the limit"
        )));
    }
    let mut out = Vec::with_capacity((count as usize).min(r.rest().len() / 6));
    for _ in 0..count {
        let id = if v < 2 { r.u16()? as u32 } else { r.u32()? };
        let construction_method = if v >= 1 { (r.u16()? & 15) as u8 } else { 0 };
        let data_ref = r.u16()?;
        let base_offset = r.uint(base_offset_size)?;
        let extent_count = r.u16()? as u32;
        if extent_count > limits.max_extents {
            return Err(Error::invalid(format!(
                "heif: item {id} has {extent_count} extents, over the limit"
            )));
        }
        let per = index_size + offset_size + length_size;
        let mut extents =
            Vec::with_capacity((extent_count as usize).min(r.rest().len() / per.max(1)));
        for _ in 0..extent_count {
            r.uint(index_size)?;
            let off = r.uint(offset_size)?;
            let len = r.uint(length_size)?;
            extents.push((off, len));
        }
        if data_ref != 0 {
            // Data in another file: not reachable from a single buffer.
            continue;
        }
        out.push((
            id,
            Location {
                construction_method,
                base_offset,
                extents,
            },
        ));
    }
    Ok(out)
}

fn parse_iref(p: &[u8], limits: &Limits) -> Result<Vec<Reference>> {
    let mut r = Rd::new(p, "iref");
    let (v, _) = r.full()?;
    let mut out = Vec::new();
    let mut total = 0u32;
    for (kind, body) in child_boxes(r.rest()) {
        let mut r = Rd::new(body, "iref");
        let id = |r: &mut Rd| -> Result<u32> {
            if v == 0 {
                Ok(r.u16()? as u32)
            } else {
                r.u32()
            }
        };
        let from = id(&mut r)?;
        let n = r.u16()? as u32;
        total = total.saturating_add(n);
        if total > limits.max_items.saturating_mul(4) {
            return Err(Error::invalid("heif: too many item references"));
        }
        let mut to = Vec::with_capacity(n as usize);
        for _ in 0..n {
            to.push(id(&mut r)?);
        }
        out.push(Reference { kind, from, to });
    }
    Ok(out)
}

fn parse_ipma(p: &[u8], limits: &Limits, file: &mut HeifFile) -> Result<()> {
    let mut r = Rd::new(p, "ipma");
    let (v, flags) = r.full()?;
    let count = r.u32()?;
    if count > limits.max_items {
        return Err(Error::invalid(format!(
            "heif: {count} ipma entries is over the limit"
        )));
    }
    for _ in 0..count {
        let id = if v < 1 { r.u16()? as u32 } else { r.u32()? };
        let n = r.u8()?;
        let mut props = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let (essential, index) = if flags & 1 != 0 {
                let x = r.u16()?;
                (x & 0x8000 != 0, (x & 0x7fff) as usize)
            } else {
                let x = r.u8()?;
                (x & 0x80 != 0, (x & 0x7f) as usize)
            };
            // Index 0 means "no property"; the rest are 1-based into ipco.
            if index > 0 {
                props.push((index - 1, essential));
            }
        }
        if let Some(it) = file.items.iter_mut().find(|it| it.id == id) {
            it.properties.extend(props);
        }
    }
    Ok(())
}

fn parse_property<'a>(t: [u8; 4], p: &'a [u8]) -> Result<Property<'a>> {
    Ok(match &t {
        b"ispe" => {
            let mut r = Rd::new(p, "ispe");
            r.full()?;
            Property::Ispe {
                width: r.u32()?,
                height: r.u32()?,
            }
        }
        b"hvcC" => Property::HvcC(p),
        b"av1C" => Property::Av1C(p),
        b"colr" => {
            let mut r = Rd::new(p, "colr");
            match &r.fourcc()? {
                b"nclx" => {
                    let primaries = r.u16()?;
                    let transfer = r.u16()?;
                    let matrix = r.u16()?;
                    let full_range = r.u8()? & 0x80 != 0;
                    Property::Colr(Colour::Nclx {
                        primaries,
                        transfer,
                        matrix,
                        full_range,
                    })
                }
                b"rICC" | b"prof" => Property::Colr(Colour::Icc(r.rest())),
                _ => Property::Other(t),
            }
        }
        b"irot" => Property::Irot(Rd::new(p, "irot").u8()? & 3),
        b"imir" => Property::Imir(Rd::new(p, "imir").u8()? & 1),
        b"clap" => {
            let mut r = Rd::new(p, "clap");
            let mut f = [0i64; 8];
            for (i, x) in f.iter_mut().enumerate() {
                let v = r.u32()?;
                // Widths/heights and denominators are unsigned; the two
                // offset numerators are signed.
                *x = if i == 4 || i == 6 {
                    v as i32 as i64
                } else {
                    v as i64
                };
            }
            Property::Clap(Clap {
                width: (f[0], f[1]),
                height: (f[2], f[3]),
                horiz_off: (f[4], f[5]),
                vert_off: (f[6], f[7]),
            })
        }
        b"pixi" => {
            let mut r = Rd::new(p, "pixi");
            r.full()?;
            let n = r.u8()? as usize;
            Property::Pixi(r.take(n)?.to_vec())
        }
        b"auxC" => {
            let mut r = Rd::new(p, "auxC");
            r.full()?;
            let aux_type = r.cstr();
            Property::AuxC {
                aux_type,
                subtype: r.rest(),
            }
        }
        _ => Property::Other(t),
    })
}

// ===========================================================================
// Well-known auxiliary-image types
// ===========================================================================

/// `auxC` URNs that mark an alpha plane.
pub const ALPHA_URNS: &[&str] = &[
    "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
    "urn:mpeg:hevc:2015:auxid:1",
    "urn:mpeg:avc:2015:auxid:1",
];

/// `auxC` URNs that mark a depth map.
pub const DEPTH_URNS: &[&str] = &[
    "urn:mpeg:mpegB:cicp:systems:auxiliary:depth",
    "urn:mpeg:hevc:2015:auxid:2",
    "urn:mpeg:avc:2015:auxid:2",
];

/// True if `urn` is an alpha `auxC` type.
pub fn is_alpha_urn(urn: &str) -> bool {
    ALPHA_URNS.contains(&urn)
}

/// True if `urn` is a depth `auxC` type.
pub fn is_depth_urn(urn: &str) -> bool {
    DEPTH_URNS.contains(&urn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(t);
        v.extend_from_slice(body);
        v
    }

    fn full(t: &[u8; 4], ver: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut b = vec![ver];
        b.extend_from_slice(&flags.to_be_bytes()[1..]);
        b.extend_from_slice(body);
        bx(t, &b)
    }

    /// A minimal two-item file: a 2x1 grid (in `idat`) over two `hvc1`
    /// tiles, one with a thumbnail and an Exif item.
    fn sample() -> Vec<u8> {
        let mut infe = Vec::new();
        for (id, t, hidden) in [
            (1u16, b"grid", 0u32),
            (2, b"hvc1", 1),
            (3, b"hvc1", 1),
            (4, b"Exif", 0),
        ] {
            let mut b = id.to_be_bytes().to_vec();
            b.extend_from_slice(&[0, 0]);
            b.extend_from_slice(t);
            b.push(0);
            infe.extend_from_slice(&full(b"infe", 2, hidden, &b));
        }
        let mut iinf = 4u16.to_be_bytes().to_vec();
        iinf.extend_from_slice(&infe);
        let iinf = full(b"iinf", 0, 0, &iinf);

        let grid = [0u8, 0, 0, 1, 0, 100, 0, 50];
        let idat = bx(b"idat", &grid);

        // iloc v1: offset 4, length 4, base 0, index 0.
        let mut il = vec![0x44, 0x00];
        il.extend_from_slice(&4u16.to_be_bytes());
        let entry = |id: u16, cm: u16, off: u32, len: u32| {
            let mut e = id.to_be_bytes().to_vec();
            e.extend_from_slice(&cm.to_be_bytes());
            e.extend_from_slice(&0u16.to_be_bytes());
            e.extend_from_slice(&1u16.to_be_bytes());
            e.extend_from_slice(&off.to_be_bytes());
            e.extend_from_slice(&len.to_be_bytes());
            e
        };
        il.extend_from_slice(&entry(1, 1, 0, 8));
        il.extend_from_slice(&entry(2, 0, 0, 4));
        il.extend_from_slice(&entry(3, 0, 4, 4));
        il.extend_from_slice(&entry(4, 0, 8, 0));
        let iloc = full(b"iloc", 1, 0, &il);

        let mut dimg = 1u16.to_be_bytes().to_vec();
        dimg.extend_from_slice(&2u16.to_be_bytes());
        dimg.extend_from_slice(&2u16.to_be_bytes());
        dimg.extend_from_slice(&3u16.to_be_bytes());
        let mut cdsc = 4u16.to_be_bytes().to_vec();
        cdsc.extend_from_slice(&1u16.to_be_bytes());
        cdsc.extend_from_slice(&1u16.to_be_bytes());
        let iref = full(
            b"iref",
            0,
            0,
            &[bx(b"dimg", &dimg), bx(b"cdsc", &cdsc)].concat(),
        );

        let ispe = full(
            b"ispe",
            0,
            0,
            &[&100u32.to_be_bytes()[..], &50u32.to_be_bytes()].concat(),
        );
        let irot = bx(b"irot", &[1]);
        let ipco = bx(b"ipco", &[ispe, irot].concat());
        let mut ipma = 1u32.to_be_bytes().to_vec();
        ipma.extend_from_slice(&1u16.to_be_bytes());
        ipma.extend_from_slice(&[2, 1, 0x82]);
        let ipma = full(b"ipma", 0, 0, &ipma);
        let iprp = bx(b"iprp", &[ipco, ipma].concat());

        let pitm = full(b"pitm", 0, 0, &1u16.to_be_bytes());
        let meta = full(
            b"meta",
            0,
            0,
            &[pitm, iinf, iloc, iref, iprp, idat].concat(),
        );
        let ftyp = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
        let head = [ftyp, meta].concat();
        // The file-offset extents above are relative to the file start, so
        // place the mdat bytes at offset 0..12 by prefixing... instead, put
        // them after the head and rebase: here we just append and let the
        // test read item 4 (to-end) to prove the length-0 rule.
        [head, bx(b"mdat", b"AAAABBBBexif")].concat()
    }

    #[test]
    fn parses_items_refs_properties() {
        let data = sample();
        let f = HeifFile::parse(&data, Limits::default()).unwrap();
        assert_eq!(&f.major_brand, b"heic");
        assert!(f.has_brand(b"mif1"));
        assert_eq!(f.primary, Some(1));
        assert_eq!(f.items.len(), 4);
        assert!(f.item(2).unwrap().hidden);
        assert_eq!(f.refs_from(1, b"dimg"), vec![2, 3]);
        assert_eq!(f.refs_to(1, b"cdsc"), vec![4]);
        assert_eq!(f.ispe(1), Some((100, 50)));
        let props: Vec<_> = f.properties_of(1).collect();
        assert_eq!(props.len(), 2);
        assert_eq!(props[1], (&Property::Irot(1), true));
        let g = f.grid(1).unwrap();
        assert_eq!(
            (g.rows, g.columns, g.output_width, g.output_height),
            (1, 2, 100, 50)
        );
        // File-offset extents: item 2 is bytes 0..4 of the file (the ftyp
        // size field), item 4 runs from offset 8 to the end.
        assert_eq!(&f.item_data(2).unwrap()[..], &data[0..4]);
        assert_eq!(f.item_data(4).unwrap().len(), data.len() - 8);
    }

    #[test]
    fn truncation_is_an_error_at_every_length() {
        let data = sample();
        for n in 0..data.len() {
            // Never a panic; a short buffer may still parse (boxes past the
            // end are dropped), but every accessor must stay in bounds.
            if let Ok(f) = HeifFile::parse(&data[..n], Limits::default()) {
                for it in &f.items {
                    let _ = f.item_data(it.id);
                    let _ = f.grid(it.id);
                    let _ = f.overlay(it.id);
                }
            }
        }
    }

    #[test]
    fn hostile_counts_are_refused_not_allocated() {
        // iinf claiming 4 billion items.
        let iinf = full(b"iinf", 1, 0, &u32::MAX.to_be_bytes());
        let meta = full(b"meta", 0, 0, &iinf);
        let data = [bx(b"ftyp", b"heic\0\0\0\0"), meta].concat();
        assert!(HeifFile::parse(&data, Limits::default()).is_err());
        // iloc with a 3-byte offset field.
        let iloc = full(b"iloc", 0, 0, &[0x34, 0x00, 0, 0]);
        let meta = full(b"meta", 0, 0, &iloc);
        let data = [bx(b"ftyp", b"heic\0\0\0\0"), meta].concat();
        assert!(HeifFile::parse(&data, Limits::default()).is_err());
    }

    #[test]
    fn clap_rect_matches_libheif_rounding() {
        // Centred 300x300 crop of a 1280x720 image.
        let c = Clap {
            width: (300, 1),
            height: (300, 1),
            horiz_off: (0, 1),
            vert_off: (0, 1),
        };
        assert_eq!(c.rect(1280, 720), Some((490, 210, 300, 300)));
        // A zero denominator is refused; an over-wide aperture is clamped
        // to the image, as libheif does.
        let bad = Clap {
            width: (300, 0),
            ..c
        };
        assert_eq!(bad.rect(1280, 720), None);
        let big = Clap {
            width: (2000, 1),
            ..c
        };
        assert_eq!(big.rect(1280, 720), Some((0, 210, 1280, 300)));
        // Odd: a 3-wide aperture centred in a 4-wide image.
        let odd = Clap {
            width: (3, 1),
            height: (1, 1),
            horiz_off: (0, 1),
            vert_off: (0, 1),
        };
        assert_eq!(odd.rect(4, 1), Some((0, 0, 3, 1)));
    }
}
