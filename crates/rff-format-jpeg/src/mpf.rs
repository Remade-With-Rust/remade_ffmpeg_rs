//! JPEG Multi-Picture Format (CIPA DC-007): the container layer.
//!
//! An MPF file is a JPEG whose first image carries an `APP2` segment that
//! starts `MPF\0`. After that identifier sits a small TIFF structure (byte
//! order mark, IFD offset, one IFD): the **MP Index IFD**, whose `MPEntry`
//! table lists every image in the file as 16-byte records of attribute
//! (format, type, dependency flags), size, offset and two dependent-image
//! numbers. Offsets count from the TIFF byte-order mark (the "MP Endian"
//! field) of that `APP2` segment; the first image is the entry whose offset
//! is 0. The other images are whole JPEG files appended after the first
//! image's `EOI`.
//!
//! This module only finds, lists and writes the images: it reads no pixels
//! and interprets no gain-map metadata. It is `no_std`-friendly (`core` and
//! `alloc` only, no I/O), takes a byte slice and returns byte ranges, and is
//! written to take hostile input: every offset is bounds-checked, every loop
//! advances, and nothing is allocated beyond what the input itself can
//! justify (the entry table is capped by the `APP2` segment it lives in).
//!
//! - [`locate`] finds the images of a file (by its MP Index, or, without one,
//!   by the [`TrailingPolicy`]).
//! - [`parse_index`] parses an MP Index IFD on its own.
//! - [`write`] builds an MPF file from whole JPEG images.
//!
//! The writer lays out the index exactly as FFmpeg 9.1's `jpeg_mpf` muxer
//! does (big-endian, the three tags, entries in input order, segments spliced
//! after `SOI`/`APP0`/`APP1`), so the two produce byte-identical files for the
//! same input.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

/// The identifier that opens an MPF `APP2` segment.
pub const MPF_IDENTIFIER: &[u8; 4] = b"MPF\0";
/// The identifier of an ISO 21496-1 gain-map `APP2` segment (NUL included).
pub const ISO21496_IDENTIFIER: &[u8] = b"urn:iso:std:iso:ts:21496:-1\0";
/// The identifier of an XMP `APP1` segment (NUL included).
const XMP_IDENTIFIER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// The Adobe/Google `hdrgm` XMP namespace a gain map's XMP declares.
const HDRGM_NAMESPACE: &[u8] = b"http://ns.adobe.com/hdr-gain-map/1.0/";

/// Most images one MP Index can list: its entry table has to fit in a
/// single `APP2` segment (65 535 bytes, of which 56 are the fixed part).
pub const MAX_IMAGES: usize = (u16::MAX as usize - FIXED_INDEX_LEN) / ENTRY_LEN;

/// One `MPEntry` record: attribute, size, offset, two dependent numbers.
const ENTRY_LEN: usize = 16;
/// Length of the index segment without its entry table: the length field,
/// `MPF\0`, the TIFF header, the IFD with three tags and the next-IFD link.
const FIXED_INDEX_LEN: usize = 2 + 4 + 8 + 2 + 3 * 12 + 4;

const TAG_MPF_VERSION: u16 = 0xB000;
const TAG_NUMBER_OF_IMAGES: u16 = 0xB001;
const TAG_MP_ENTRY: u16 = 0xB002;
const TIFF_LONG: u16 = 4;
const TIFF_UNDEFINED: u16 = 7;

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const TEM: u8 = 0x01;
const APP0: u8 = 0xE0;
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;

/// Individual Image Attribute: this image has dependent images.
pub const ATTR_DEPENDENT_PARENT: u32 = 0x8000_0000;
/// Individual Image Attribute: this image is a dependent image.
pub const ATTR_DEPENDENT_CHILD: u32 = 0x4000_0000;
/// Individual Image Attribute: the representative image of the file.
pub const ATTR_REPRESENTATIVE: u32 = 0x2000_0000;
/// Individual Image Attribute: image data format (0 = JPEG).
pub const ATTR_FORMAT_MASK: u32 = 0x0700_0000;
/// Individual Image Attribute: the MP Type code.
pub const ATTR_TYPE_MASK: u32 = 0x00FF_FFFF;

/// An MP Type code (CIPA DC-007 Table 4, plus the ISO 21496-1 gain map).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MpType(pub u32);

impl MpType {
    pub const UNDEFINED: MpType = MpType(0x00_0000);
    pub const LARGE_THUMBNAIL_VGA: MpType = MpType(0x01_0001);
    pub const LARGE_THUMBNAIL_FULL_HD: MpType = MpType(0x01_0002);
    pub const LARGE_THUMBNAIL_4K: MpType = MpType(0x01_0003);
    pub const LARGE_THUMBNAIL_8K: MpType = MpType(0x01_0004);
    pub const LARGE_THUMBNAIL_16K: MpType = MpType(0x01_0005);
    pub const PANORAMA: MpType = MpType(0x02_0001);
    pub const DISPARITY: MpType = MpType(0x02_0002);
    pub const MULTI_ANGLE: MpType = MpType(0x02_0003);
    pub const PRIMARY: MpType = MpType(0x03_0000);
    pub const ORIGINAL: MpType = MpType(0x04_0000);
    pub const GAIN_MAP: MpType = MpType(0x05_0000);

    const NAMED: [(MpType, &'static str, &'static str); 12] = [
        (MpType::UNDEFINED, "undefined", "Undefined"),
        (
            MpType::LARGE_THUMBNAIL_VGA,
            "large_thumbnail_vga",
            "Large Thumbnail (VGA equivalent)",
        ),
        (
            MpType::LARGE_THUMBNAIL_FULL_HD,
            "large_thumbnail_full_hd",
            "Large Thumbnail (Full-HD equivalent)",
        ),
        (
            MpType::LARGE_THUMBNAIL_4K,
            "large_thumbnail_4k",
            "Large Thumbnail (4K equivalent)",
        ),
        (
            MpType::LARGE_THUMBNAIL_8K,
            "large_thumbnail_8k",
            "Large Thumbnail (8K equivalent)",
        ),
        (
            MpType::LARGE_THUMBNAIL_16K,
            "large_thumbnail_16k",
            "Large Thumbnail (16K equivalent)",
        ),
        (MpType::PANORAMA, "panorama", "Multi-Frame Image (Panorama)"),
        (
            MpType::DISPARITY,
            "disparity",
            "Multi-Frame Image (Disparity)",
        ),
        (
            MpType::MULTI_ANGLE,
            "multi_angle",
            "Multi-Frame Image (Multi-Angle)",
        ),
        (MpType::PRIMARY, "primary", "Baseline MP Primary Image"),
        (MpType::ORIGINAL, "original", "Original Preservation Image"),
        (MpType::GAIN_MAP, "gain_map", "Gain Map Image"),
    ];

    /// The 24-bit type code.
    pub const fn code(self) -> u32 {
        self.0 & ATTR_TYPE_MASK
    }

    /// Short machine name (`primary`, `disparity`, `gain_map`, ...), or
    /// `None` for a code the specification does not define.
    pub fn name(self) -> Option<&'static str> {
        Self::NAMED
            .iter()
            .find(|(t, _, _)| t.code() == self.code())
            .map(|&(_, n, _)| n)
    }

    /// Human-readable name, as the specification words it.
    pub fn description(self) -> &'static str {
        Self::NAMED
            .iter()
            .find(|(t, _, _)| t.code() == self.code())
            .map_or("Unknown MP Type", |&(_, _, d)| d)
    }

    /// Parse a machine name ([`MpType::name`]) or a hex code (`0x020002`).
    pub fn from_name(s: &str) -> Option<MpType> {
        let s = s.trim();
        if let Some(&(t, _, _)) = Self::NAMED.iter().find(|(_, n, _)| *n == s) {
            return Some(t);
        }
        let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
        u32::from_str_radix(hex, 16)
            .ok()
            .filter(|c| *c <= ATTR_TYPE_MASK)
            .map(MpType)
    }

    /// Types that only exist as dependent images of a parent (the large
    /// thumbnails, the original preservation image, the gain map).
    pub const fn is_dependent_kind(self) -> bool {
        matches!(self.code() & 0xFF_0000, 0x01_0000 | 0x04_0000 | 0x05_0000)
    }
}

impl fmt::Display for MpType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => f.write_str(n),
            None => write!(f, "0x{:06x}", self.code()),
        }
    }
}

/// Byte order of an MP Index IFD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    /// `MM` (Motorola).
    Big,
    /// `II` (Intel).
    Little,
}

impl ByteOrder {
    fn u16(self, b: &[u8], at: usize) -> Option<u16> {
        let s: [u8; 2] = b.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(match self {
            ByteOrder::Big => u16::from_be_bytes(s),
            ByteOrder::Little => u16::from_le_bytes(s),
        })
    }

    fn u32(self, b: &[u8], at: usize) -> Option<u32> {
        let s: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(match self {
            ByteOrder::Big => u32::from_be_bytes(s),
            ByteOrder::Little => u32::from_le_bytes(s),
        })
    }
}

/// One `MPEntry` record, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpEntry {
    /// Individual Image Attribute (flags, format, MP Type).
    pub attribute: u32,
    /// Image size in bytes.
    pub size: u32,
    /// Offset of the image's `SOI` from the MP Endian field; 0 for the first
    /// image.
    pub offset: u32,
    /// 1-based entry numbers of up to two dependent images (0 = none).
    pub dependents: [u16; 2],
}

impl MpEntry {
    /// The MP Type code of the attribute.
    pub const fn mp_type(&self) -> MpType {
        MpType(self.attribute & ATTR_TYPE_MASK)
    }

    /// Image data format: 0 is JPEG, the only one the specification defines.
    pub const fn format(&self) -> u32 {
        (self.attribute & ATTR_FORMAT_MASK) >> 24
    }
}

/// A parsed MP Index IFD.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MpIndex {
    pub byte_order: ByteOrder,
    /// The `MPFVersion` tag (`0100`), when present.
    pub version: Option<[u8; 4]>,
    /// The first `NumberOfImages` entries of the `MPEntry` table.
    pub entries: Vec<MpEntry>,
}

/// Why an MPF structure could not be read or written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MpfError {
    /// The data does not start with a JPEG `SOI` marker.
    NotJpeg,
    /// A marker or segment of the JPEG header is malformed or truncated.
    BadMarker,
    /// The TIFF header of the MP Index is not `MM\0*` / `II*\0`.
    BadTiffHeader,
    /// The IFD offset, tag count or a tag's data points outside the segment.
    BadIfd,
    /// `NumberOfImages` or `MPEntry` is missing or of the wrong type.
    MissingTag,
    /// `NumberOfImages` is 0 or claims more entries than `MPEntry` holds.
    BadImageCount,
    /// No entry describes the first image (offset 0), or two do.
    BadFirstImage,
    /// Writing: no images were given.
    NoImages,
    /// Writing: more images than one MP Index can list ([`MAX_IMAGES`]).
    TooManyImages,
    /// Writing: image `n` is not a whole JPEG with a parsable header.
    BadImage(usize),
    /// Writing: more than one gain map, or more than two dependent images.
    TooManyDependents,
    /// Writing: an image size or offset does not fit the 32-bit fields.
    TooLarge,
}

impl fmt::Display for MpfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MpfError::NotJpeg => f.write_str("not a JPEG (no SOI marker)"),
            MpfError::BadMarker => f.write_str("malformed or truncated JPEG marker segment"),
            MpfError::BadTiffHeader => f.write_str("MP Index: bad TIFF header"),
            MpfError::BadIfd => f.write_str("MP Index: IFD out of bounds"),
            MpfError::MissingTag => {
                f.write_str("MP Index: missing or malformed NumberOfImages/MPEntry")
            }
            MpfError::BadImageCount => f.write_str("MP Index: invalid number of images"),
            MpfError::BadFirstImage => {
                f.write_str("MP Index: no (or a duplicate) entry for the first image")
            }
            MpfError::NoImages => f.write_str("no images to write"),
            MpfError::TooManyImages => {
                write!(
                    f,
                    "more than {MAX_IMAGES} images (one MP Index cannot list them)"
                )
            }
            MpfError::BadImage(n) => write!(f, "image {n} is not a well-formed JPEG"),
            MpfError::TooManyDependents => {
                f.write_str("too many dependent images (at most one gain map and two dependents)")
            }
            MpfError::TooLarge => f.write_str("image too large for MPF's 32-bit offsets"),
        }
    }
}

impl core::error::Error for MpfError {}

/// Parse an MP Index IFD: `tiff` is the `APP2` payload after `MPF\0`, from
/// the MP Endian field to the end of the segment. Every structure must lie
/// inside it.
///
/// Like FFmpeg, an `MPEntry` table longer than `NumberOfImages` records is
/// accepted (the excess is ignored), a shorter one is not.
pub fn parse_index(tiff: &[u8]) -> Result<MpIndex, MpfError> {
    let order = match tiff.get(..4) {
        Some(b"MM\0*") => ByteOrder::Big,
        Some(b"II*\0") => ByteOrder::Little,
        _ => return Err(MpfError::BadTiffHeader),
    };
    let ifd = order.u32(tiff, 4).ok_or(MpfError::BadIfd)? as usize;
    let count = order.u16(tiff, ifd).ok_or(MpfError::BadIfd)? as usize;
    // The whole tag array must be present: 12 bytes per tag.
    let tags_end = ifd
        .checked_add(2)
        .and_then(|t| t.checked_add(count.checked_mul(12)?))
        .ok_or(MpfError::BadIfd)?;
    if tags_end > tiff.len() {
        return Err(MpfError::BadIfd);
    }

    let mut version = None;
    let mut number: Option<u32> = None;
    let mut entry_data: Option<Range<usize>> = None;
    for t in 0..count {
        let at = ifd + 2 + t * 12;
        let tag = order.u16(tiff, at).ok_or(MpfError::BadIfd)?;
        let kind = order.u16(tiff, at + 2).ok_or(MpfError::BadIfd)?;
        let n = order.u32(tiff, at + 4).ok_or(MpfError::BadIfd)?;
        match tag {
            TAG_MPF_VERSION if version.is_none() && kind == TIFF_UNDEFINED && n == 4 => {
                version = tiff
                    .get(at + 8..at + 12)
                    .and_then(|v| <[u8; 4]>::try_from(v).ok());
            }
            TAG_NUMBER_OF_IMAGES if number.is_none() => {
                if kind != TIFF_LONG || n != 1 {
                    return Err(MpfError::MissingTag);
                }
                number = order.u32(tiff, at + 8);
            }
            TAG_MP_ENTRY if entry_data.is_none() => {
                if kind != TIFF_UNDEFINED {
                    return Err(MpfError::MissingTag);
                }
                let len = n as usize;
                // Values of up to 4 bytes sit in the tag itself; a real entry
                // table (16 bytes per image) never does, but read it right.
                let start = if len <= 4 {
                    at + 8
                } else {
                    order.u32(tiff, at + 8).ok_or(MpfError::BadIfd)? as usize
                };
                let end = start.checked_add(len).ok_or(MpfError::BadIfd)?;
                if end > tiff.len() {
                    return Err(MpfError::BadIfd);
                }
                entry_data = Some(start..end);
            }
            _ => {}
        }
    }

    let (Some(number), Some(data)) = (number, entry_data) else {
        return Err(MpfError::MissingTag);
    };
    let number = number as usize;
    // `data` lies inside `tiff`, so this bounds the allocation by the input.
    if number == 0 || number > data.len() / ENTRY_LEN {
        return Err(MpfError::BadImageCount);
    }
    let table = &tiff[data];
    let entries = (0..number)
        .map(|i| {
            let at = i * ENTRY_LEN;
            // In bounds: number * 16 <= table.len() was checked above.
            let r32 = |o: usize| order.u32(table, at + o).unwrap_or(0);
            let r16 = |o: usize| order.u16(table, at + o).unwrap_or(0);
            MpEntry {
                attribute: r32(0),
                size: r32(4),
                offset: r32(8),
                dependents: [r16(12), r16(14)],
            }
        })
        .collect();
    Ok(MpIndex {
        byte_order: order,
        version,
        entries,
    })
}

/// What the header of a JPEG image (`SOI` up to its first scan) carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Header {
    /// Payload of the first `APP2` segment that starts `MPF\0`, and how many
    /// such segments there are.
    mpf: Option<Range<usize>>,
    mpf_count: usize,
    /// Payload of the first ISO 21496-1 `APP2` segment.
    iso21496: Option<Range<usize>>,
    /// The image carries an XMP packet, and that packet names `hdrgm`.
    has_xmp: bool,
    has_hdrgm: bool,
    /// Whole `APP2` segments (marker to payload end) that start `MPF\0` or
    /// the ISO 21496-1 identifier: what a writer may need to replace.
    replaceable: Vec<(Range<usize>, bool)>,
    /// Where an MPF writer inserts its segments: the first segment that is
    /// not `APP0`/`APP1`, or the first scan (CIPA DC-007 §5.2, FFmpeg).
    splice: Option<usize>,
    /// The walk reached entropy-coded data (`SOS` or a standalone marker).
    complete: bool,
}

/// Walk the marker segments of the JPEG at the start of `data` until its
/// first scan. A walk that runs off the end of the data stops there with
/// what it found (`complete == false`); a malformed marker is an error.
fn walk_header(data: &[u8]) -> Result<Header, MpfError> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != SOI {
        return Err(MpfError::NotJpeg);
    }
    let mut h = Header::default();
    let mut i = 2usize;
    loop {
        let pos = i;
        match data.get(i) {
            None => return Ok(h),
            Some(0xFF) => {}
            Some(_) => return Err(MpfError::BadMarker),
        }
        i += 1;
        while data.get(i) == Some(&0xFF) {
            i += 1; // fill bytes
        }
        let Some(&marker) = data.get(i) else {
            return Ok(h);
        };
        i += 1;
        if marker == SOS || marker == TEM || (0xD0..=EOI).contains(&marker) {
            h.splice.get_or_insert(pos);
            h.complete = true;
            return Ok(h);
        }
        let Some(len) = data.get(i..i + 2) else {
            return Ok(h);
        };
        let len = u16::from_be_bytes([len[0], len[1]]) as usize;
        if len < 2 {
            return Err(MpfError::BadMarker);
        }
        let payload = i + 2..i + len;
        if payload.end > data.len() {
            return Ok(h);
        }
        let body = &data[payload.clone()];
        match marker {
            APP0 => {}
            APP1 => {
                if body.starts_with(XMP_IDENTIFIER) {
                    h.has_xmp = true;
                    h.has_hdrgm |= contains(body, HDRGM_NAMESPACE);
                }
            }
            _ => {
                h.splice.get_or_insert(pos);
                if marker == APP2 {
                    if body.starts_with(MPF_IDENTIFIER) {
                        h.mpf_count += 1;
                        h.mpf.get_or_insert(payload.clone());
                        h.replaceable.push((pos..payload.end, false));
                    } else if body.starts_with(ISO21496_IDENTIFIER) {
                        h.iso21496.get_or_insert(payload.clone());
                        h.replaceable.push((pos..payload.end, true));
                    }
                }
            }
        }
        i = payload.end;
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// End (exclusive) of the JPEG image that starts at `start`: the byte after
/// its `EOI`, following marker segments by their lengths and entropy-coded
/// data byte by byte (so a thumbnail inside `APP1` is skipped, and
/// progressive files with many scans are followed to the real end). `None`
/// when the image is malformed or has no `EOI` before the end of `data`.
/// Linear in the image size.
pub fn jpeg_end(data: &[u8], start: usize) -> Option<usize> {
    if data.get(start..start.checked_add(2)?)? != [0xFF, SOI] {
        return None;
    }
    let mut i = start + 2;
    loop {
        if *data.get(i)? != 0xFF {
            return None;
        }
        while data.get(i + 1) == Some(&0xFF) {
            i += 1;
        }
        let marker = *data.get(i + 1)?;
        i += 2;
        match marker {
            EOI => return Some(i),
            TEM | 0xD0..=0xD7 => continue,
            0x00 | SOI => return None,
            _ => {}
        }
        let len = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
        if len < 2 {
            return None;
        }
        i = i.checked_add(len)?;
        if marker == SOS {
            // Entropy-coded data: runs to the next marker that is neither a
            // stuffed 0x00 nor a restart marker (fill bytes may precede it).
            loop {
                let ff = i + data.get(i..)?.iter().position(|&b| b == 0xFF)?;
                let mut j = ff + 1;
                while data.get(j) == Some(&0xFF) {
                    j += 1;
                }
                match *data.get(j)? {
                    0x00 | 0xD0..=0xD7 => i = j + 1,
                    _ => {
                        i = j - 1; // the last fill byte is the marker's 0xFF
                        break;
                    }
                }
            }
        }
    }
}

/// What to do with a JPEG that has no usable MP Index but more JPEG images
/// after its `EOI` (an index stripped by an editor, a writer that never
/// wrote one, an Ultra HDR file that only has the XMP container directory).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrailingPolicy {
    /// One image: the whole file (what FFmpeg 9.1 does).
    Ignore,
    /// Split: the first image is `SOI..EOI`, and every complete JPEG that
    /// follows it back to back (zero padding between them is skipped) is an
    /// image of its own. Data after the last one that is not a JPEG is not
    /// part of any image. A file with nothing after its `EOI` but non-JPEG
    /// data is still one image covering the whole file.
    #[default]
    Split,
}

/// How a [`Layout`]'s images were found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutSource {
    /// A plain JPEG: one image, the whole file.
    Single,
    /// The MP Index.
    Index,
    /// No usable MP Index: split at `EOI` ([`TrailingPolicy::Split`]).
    Trailing,
}

/// One image of an MPF file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocatedImage {
    /// Its bytes in the file.
    pub range: Range<usize>,
    /// Its `MPEntry` number in the index, when it came from one.
    pub entry: Option<usize>,
    /// Individual Image Attribute (0 when found without an index).
    pub attribute: u32,
    /// MP Type: the attribute's, or [`MpType::PRIMARY`] for the first image
    /// and [`MpType::UNDEFINED`] for the rest when found without an index.
    pub mp_type: MpType,
    /// 1-based dependent entry numbers, from the index.
    pub dependents: [u16; 2],
    /// The type was inferred, not read (libultrahdr's untyped gain maps).
    pub type_inferred: bool,
    /// The file ends before the image does: `range` is what there is.
    pub truncated: bool,
}

/// Something [`locate`] tolerated instead of failing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The `MPEntry` it concerns, if any.
    pub entry: Option<usize>,
    pub message: String,
}

/// The images of a JPEG/MPF file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub source: LayoutSource,
    /// The images, in index order (FFmpeg's stream order). Never empty.
    pub images: Vec<LocatedImage>,
    /// Which of [`images`](Layout::images) is the first individual image
    /// (the one at the start of the file): 0 except in an index that lists
    /// another entry first.
    pub first: usize,
    /// The parsed MP Index, when the file has a usable one.
    pub index: Option<MpIndex>,
    /// The first image carries ISO 21496-1 gain-map metadata.
    pub has_gain_map_metadata: bool,
    pub warnings: Vec<Warning>,
}

impl Layout {
    /// The image indices sorted by file position (the order the bytes come).
    pub fn file_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.images.len()).collect();
        order.sort_by_key(|&i| (self.images[i].range.start, i));
        order
    }
}

/// Find the images of a JPEG / MPF file.
///
/// With a usable MP Index, the images are its JPEG entries, in entry order,
/// as FFmpeg 9.1 reads them: the first image is the entry with offset 0,
/// the others sit at MP Endian + offset; an image that overruns the next
/// one is cut where the next starts; with ISO 21496-1 metadata in the first
/// image and exactly two images, an untyped second image is the gain map
/// (libultrahdr writes those). Where FFmpeg would fail or hand out garbage,
/// this degrades instead, with a [`Warning`]: an entry that points past the
/// end of the file or at something that is not a JPEG `SOI` is dropped; an
/// image the file cuts short is kept as far as it goes; an index that is
/// malformed, or lists no first image, is treated as absent.
///
/// Without a usable index, `policy` decides ([`TrailingPolicy`]).
///
/// Errors only when `data` is not a JPEG at all.
pub fn locate(data: &[u8], policy: TrailingPolicy) -> Result<Layout, MpfError> {
    let mut warnings = Vec::new();
    // A header whose marker structure breaks (stray bytes between segments,
    // which libjpeg skips) has no index we can trust, but is still a JPEG.
    let header = match walk_header(data) {
        Ok(h) => h,
        Err(MpfError::NotJpeg) => return Err(MpfError::NotJpeg),
        Err(e) => {
            warnings.push(Warning {
                entry: None,
                message: alloc::format!("{e}; no MP Index read"),
            });
            Header::default()
        }
    };
    let has_gain_map_metadata = header.iso21496.is_some();
    if header.mpf_count > 1 {
        warnings.push(Warning {
            entry: None,
            message: "duplicate MP Index segment; using the first".into(),
        });
    }

    if let Some(payload) = header.mpf.clone() {
        let tiff_base = payload.start + MPF_IDENTIFIER.len();
        match parse_index(&data[tiff_base..payload.end]) {
            Ok(index) => {
                match from_index(
                    data,
                    tiff_base,
                    &index,
                    has_gain_map_metadata,
                    &mut warnings,
                ) {
                    Ok((images, first)) => {
                        return Ok(Layout {
                            source: LayoutSource::Index,
                            images,
                            first,
                            index: Some(index),
                            has_gain_map_metadata,
                            warnings,
                        })
                    }
                    Err(e) => warnings.push(Warning {
                        entry: None,
                        message: alloc::format!("{e}; ignoring the MP Index"),
                    }),
                }
            }
            Err(e) => warnings.push(Warning {
                entry: None,
                message: alloc::format!("{e}; ignoring the MP Index"),
            }),
        }
    }

    let images = match policy {
        TrailingPolicy::Ignore => None,
        TrailingPolicy::Split => split_trailing(data),
    };
    let (source, images) = match images {
        Some(images) => (LayoutSource::Trailing, images),
        None => (
            LayoutSource::Single,
            alloc::vec![LocatedImage {
                range: 0..data.len(),
                entry: None,
                attribute: 0,
                mp_type: MpType::PRIMARY,
                dependents: [0; 2],
                type_inferred: false,
                truncated: false,
            }],
        ),
    };
    Ok(Layout {
        source,
        images,
        first: 0,
        index: None,
        has_gain_map_metadata,
        warnings,
    })
}

fn from_index(
    data: &[u8],
    tiff_base: usize,
    index: &MpIndex,
    has_gain_map_metadata: bool,
    warnings: &mut Vec<Warning>,
) -> Result<(Vec<LocatedImage>, usize), MpfError> {
    let mut images: Vec<LocatedImage> = Vec::new();
    let mut first: Option<usize> = None;
    for (k, e) in index.entries.iter().enumerate() {
        let warn = |warnings: &mut Vec<Warning>, m: &str| {
            warnings.push(Warning {
                entry: Some(k),
                message: m.into(),
            })
        };
        if e.format() != 0 {
            warn(warnings, "not a JPEG image (unsupported format); skipped");
            continue;
        }
        let start = if e.offset == 0 {
            if first.is_some() {
                return Err(MpfError::BadFirstImage);
            }
            first = Some(images.len());
            0
        } else {
            match tiff_base.checked_add(e.offset as usize) {
                Some(s) if s < data.len() => s,
                _ => {
                    warn(warnings, "offset points past the end of the file; skipped");
                    continue;
                }
            }
        };
        if data.get(start..start + 2) != Some(&[0xFF, SOI][..]) {
            warn(warnings, "offset does not point at a JPEG SOI; skipped");
            continue;
        }
        let available = data.len() - start;
        let mut len = e.size as usize;
        // Some writers leave the size of the first image 0, and a damaged
        // size can end it before its own header does: find its real end.
        let short = |len: usize| {
            e.offset == 0 && walk_header(&data[..len.min(available)]).map_or(true, |h| !h.complete)
        };
        if len == 0 || short(len) {
            len = jpeg_end(data, start).map_or(available, |end| end - start);
        }
        let truncated = len > available;
        if truncated {
            warn(warnings, "image runs past the end of the file; truncated");
            len = available;
        }
        images.push(LocatedImage {
            range: start..start + len,
            entry: Some(k),
            attribute: e.attribute,
            mp_type: e.mp_type(),
            dependents: e.dependents,
            type_inferred: false,
            truncated,
        });
    }
    let first = first.ok_or(MpfError::BadFirstImage)?;

    // libultrahdr writes gain maps with no MP Type; FFmpeg accepts them.
    if has_gain_map_metadata && images.len() == 2 {
        let other = &mut images[1 - first];
        if other.mp_type == MpType::UNDEFINED {
            other.mp_type = MpType::GAIN_MAP;
            other.type_inferred = true;
        }
    }

    // An image may not run into the next one: cut it where the next starts.
    let mut order: Vec<usize> = (0..images.len()).collect();
    order.sort_by_key(|&i| (images[i].range.start, images[i].entry));
    for w in order.windows(2) {
        let next_start = images[w[1]].range.start;
        let img = &mut images[w[0]];
        if next_start < img.range.end {
            img.range.end = next_start;
            warnings.push(Warning {
                entry: img.entry,
                message: "image overlaps the next one; truncated".into(),
            });
        }
    }
    let before = images.len();
    images.retain(|img| !img.range.is_empty());
    if images.len() != before {
        warnings.push(Warning {
            entry: None,
            message: "dropped images that start where another starts".into(),
        });
    }
    // Only the first image can start at 0 (every other offset is past the
    // MP Index), so this finds it again after the drops.
    let first = images
        .iter()
        .position(|i| i.range.start == 0)
        .ok_or(MpfError::BadFirstImage)?;
    Ok((images, first))
}

/// [`TrailingPolicy::Split`]: `None` when nothing but non-JPEG data (or
/// nothing at all) follows the first image's `EOI`.
fn split_trailing(data: &[u8]) -> Option<Vec<LocatedImage>> {
    let first_end = jpeg_end(data, 0)?;
    let mut images = alloc::vec![LocatedImage {
        range: 0..first_end,
        entry: None,
        attribute: 0,
        mp_type: MpType::PRIMARY,
        dependents: [0; 2],
        type_inferred: false,
        truncated: false,
    }];
    let mut pos = first_end;
    while images.len() < MAX_IMAGES {
        while data.get(pos) == Some(&0) {
            pos += 1;
        }
        if data.get(pos..pos + 3) != Some(&[0xFF, SOI, 0xFF][..]) {
            break;
        }
        let (end, truncated) = match jpeg_end(data, pos) {
            Some(end) => (end, false),
            None => (data.len(), true),
        };
        images.push(LocatedImage {
            range: pos..end,
            entry: None,
            attribute: 0,
            mp_type: MpType::UNDEFINED,
            dependents: [0; 2],
            type_inferred: false,
            truncated,
        });
        pos = end;
    }
    (images.len() > 1).then_some(images)
}

/// One image to [`write`].
#[derive(Clone, Copy, Debug)]
pub struct WriteImage<'a> {
    /// A whole JPEG file.
    pub data: &'a [u8],
    /// Its MP Type; `None` picks one: [`MpType::PRIMARY`] for the first
    /// image, [`MpType::GAIN_MAP`] for an image that carries ISO 21496-1
    /// metadata, [`MpType::UNDEFINED`] otherwise.
    pub mp_type: Option<MpType>,
    /// Set the representative-image flag.
    pub representative: bool,
}

impl<'a> WriteImage<'a> {
    /// An image whose type [`write`] picks.
    pub fn new(data: &'a [u8]) -> WriteImage<'a> {
        WriteImage {
            data,
            mp_type: None,
            representative: false,
        }
    }
}

/// The GContainer XMP directory FFmpeg writes into the primary image of an
/// Ultra HDR file, around the gain map's length.
const GCONTAINER_HEAD: &str = concat!(
    "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Adobe XMP Core 5.1.2\">\n",
    "  <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
    "    <rdf:Description\n",
    "     xmlns:Container=\"http://ns.google.com/photos/1.0/container/\"\n",
    "     xmlns:Item=\"http://ns.google.com/photos/1.0/container/item/\"\n",
    "     xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\"\n",
    "     hdrgm:Version=\"1.0\">\n",
    "      <Container:Directory>\n",
    "        <rdf:Seq>\n",
    "          <rdf:li rdf:parseType=\"Resource\">\n",
    "            <Container:Item\n",
    "             Item:Semantic=\"Primary\"\n",
    "             Item:Mime=\"image/jpeg\"/>\n",
    "          </rdf:li>\n",
    "          <rdf:li rdf:parseType=\"Resource\">\n",
    "            <Container:Item\n",
    "             Item:Semantic=\"GainMap\"\n",
    "             Item:Mime=\"image/jpeg\"\n",
    "             Item:Length=\"",
);
const GCONTAINER_TAIL: &str = concat!(
    "\"/>\n",
    "          </rdf:li>\n",
    "        </rdf:Seq>\n",
    "      </Container:Directory>\n",
    "    </rdf:Description>\n",
    "  </rdf:RDF>\n",
    "</x:xmpmeta>",
);

/// Build an MPF file from whole JPEG images, the first being the primary.
///
/// The MP Index (big-endian, version `0100`, one entry per image in input
/// order) is spliced into the first image after its `SOI`/`APP0`/`APP1`
/// segments; the other images follow it unchanged. Large thumbnails, the
/// original preservation image and the gain map are recorded as dependent
/// images of the first image. For a gain map (an image carrying ISO 21496-1
/// metadata) the first image also gets what Ultra HDR readers look for, as
/// FFmpeg 9.1 writes it: the ISO 21496-1 version segment and, when the gain
/// map has `hdrgm` XMP and the first image has no XMP of its own, the
/// GContainer directory. An MP Index (or ISO 21496-1 segment being
/// replaced) already in the first image is dropped, so re-muxing an MPF file
/// does not stack stale indexes; the MP Attribute IFDs inside the other
/// images are kept. A single image is written back unchanged.
pub fn write(images: &[WriteImage<'_>]) -> Result<Vec<u8>, MpfError> {
    let n = images.len();
    if n == 0 {
        return Err(MpfError::NoImages);
    }
    if n > MAX_IMAGES {
        return Err(MpfError::TooManyImages);
    }
    let mut headers = Vec::with_capacity(n);
    for (i, img) in images.iter().enumerate() {
        let h = walk_header(img.data).map_err(|_| MpfError::BadImage(i))?;
        if !h.complete {
            return Err(MpfError::BadImage(i));
        }
        headers.push(h);
    }
    let types: Vec<MpType> = images
        .iter()
        .zip(&headers)
        .enumerate()
        .map(|(i, (img, h))| {
            img.mp_type.unwrap_or(if i == 0 {
                MpType::PRIMARY
            } else if gain_map_version(img.data, h).is_some() {
                MpType::GAIN_MAP
            } else {
                MpType::UNDEFINED
            })
        })
        .collect();

    // Dependents of the first image.
    let mut dependents: Vec<usize> = Vec::new();
    let mut gain_map: Option<usize> = None;
    for (i, t) in types.iter().enumerate().skip(1) {
        if !t.is_dependent_kind() {
            continue;
        }
        if *t == MpType::GAIN_MAP {
            if gain_map.is_some() {
                return Err(MpfError::TooManyDependents);
            }
            gain_map = Some(i);
        }
        dependents.push(i);
    }
    if dependents.len() > 2 {
        return Err(MpfError::TooManyDependents);
    }
    if n == 1 {
        return Ok(images[0].data.to_vec());
    }

    // The segments spliced into the first image.
    let primary = images[0].data;
    let ph = &headers[0];
    let iso_version = gain_map.and_then(|g| gain_map_version(images[g].data, &headers[g]));
    let xmp: Option<String> = gain_map
        .filter(|&g| headers[g].has_hdrgm && !ph.has_xmp)
        .map(|g| alloc::format!("{GCONTAINER_HEAD}{}{GCONTAINER_TAIL}", images[g].data.len()));
    let xmp_len = xmp
        .as_ref()
        .map_or(0, |x| 2 + 2 + XMP_IDENTIFIER.len() + x.len());
    let index_len = 2 + FIXED_INDEX_LEN + ENTRY_LEN * n;
    let iso_len = iso_version.map_or(0, |_| 2 + 2 + ISO21496_IDENTIFIER.len() + 4);
    let splice = ph.splice.ok_or(MpfError::BadImage(0))?;
    // Stale segments to drop: every MP Index, and the ISO 21496-1 version
    // segment when a new one replaces it.
    let dropped: Vec<Range<usize>> = ph
        .replaceable
        .iter()
        .filter(|(_, iso)| !iso || iso_version.is_some())
        .map(|(r, _)| r.clone())
        .collect();
    let dropped_before_splice: usize = dropped
        .iter()
        .filter(|r| r.end <= splice)
        .map(|r| r.len())
        .sum();
    let dropped_total: usize = dropped.iter().map(|r| r.len()).sum();

    let primary_len = primary.len() - dropped_total + xmp_len + index_len + iso_len;
    // The MP Endian field: past the XMP segment, the APP2 marker and length,
    // and the identifier.
    let tiff_base = splice - dropped_before_splice + xmp_len + 2 + 2 + MPF_IDENTIFIER.len();
    let mut starts = Vec::with_capacity(n);
    let mut sizes = Vec::with_capacity(n);
    let mut at = 0usize;
    for (i, img) in images.iter().enumerate() {
        let len = if i == 0 { primary_len } else { img.data.len() };
        starts.push(at);
        sizes.push(u32::try_from(len).map_err(|_| MpfError::TooLarge)?);
        at = at.checked_add(len).ok_or(MpfError::TooLarge)?;
    }

    let mut out = Vec::with_capacity(at);
    // The first image, up to the splice point (minus dropped segments).
    let keep = |out: &mut Vec<u8>, range: Range<usize>| {
        let mut pos = range.start;
        for d in dropped
            .iter()
            .filter(|d| d.start >= range.start && d.end <= range.end)
        {
            out.extend_from_slice(&primary[pos..d.start]);
            pos = d.end;
        }
        out.extend_from_slice(&primary[pos..range.end]);
    };
    keep(&mut out, 0..splice);
    if let Some(x) = &xmp {
        out.extend_from_slice(&[0xFF, APP1]);
        out.extend_from_slice(&((xmp_len - 2) as u16).to_be_bytes());
        out.extend_from_slice(XMP_IDENTIFIER);
        out.extend_from_slice(x.as_bytes());
    }
    // The MP Index.
    out.extend_from_slice(&[0xFF, APP2]);
    out.extend_from_slice(&((index_len - 2) as u16).to_be_bytes());
    out.extend_from_slice(MPF_IDENTIFIER);
    debug_assert_eq!(out.len(), tiff_base);
    out.extend_from_slice(b"MM\0*");
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(&3u16.to_be_bytes());
    for (tag, kind, count, value) in [
        (TAG_MPF_VERSION, TIFF_UNDEFINED, 4u32, *b"0100"),
        (TAG_NUMBER_OF_IMAGES, TIFF_LONG, 1, (n as u32).to_be_bytes()),
        (
            TAG_MP_ENTRY,
            TIFF_UNDEFINED,
            (ENTRY_LEN * n) as u32,
            (FIXED_INDEX_LEN as u32 - 6).to_be_bytes(),
        ),
    ] {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&value);
    }
    out.extend_from_slice(&0u32.to_be_bytes()); // no MP Attribute IFD
    for (i, ((img, ty), (&start, &size))) in images
        .iter()
        .zip(&types)
        .zip(starts.iter().zip(&sizes))
        .enumerate()
    {
        let mut attr = ty.code();
        if img.representative {
            attr |= ATTR_REPRESENTATIVE;
        }
        let deps: &[usize] = if i == 0 { &dependents } else { &[] };
        if !deps.is_empty() {
            attr |= ATTR_DEPENDENT_PARENT;
        }
        if dependents.contains(&i) {
            attr |= ATTR_DEPENDENT_CHILD;
        }
        let offset = if i == 0 {
            0
        } else {
            u32::try_from(start - tiff_base).map_err(|_| MpfError::TooLarge)?
        };
        out.extend_from_slice(&attr.to_be_bytes());
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(&offset.to_be_bytes());
        for j in 0..2 {
            // 1-based entry numbers; 0 means none.
            let d = deps.get(j).map_or(0, |&d| d as u16 + 1);
            out.extend_from_slice(&d.to_be_bytes());
        }
    }
    if let Some(v) = iso_version {
        out.extend_from_slice(&[0xFF, APP2]);
        out.extend_from_slice(&((iso_len - 2) as u16).to_be_bytes());
        out.extend_from_slice(ISO21496_IDENTIFIER);
        out.extend_from_slice(&v);
    }
    keep(&mut out, splice..primary.len());
    debug_assert_eq!(out.len(), primary_len);
    for img in &images[1..] {
        out.extend_from_slice(img.data);
    }
    Ok(out)
}

/// The two 16-bit version fields of an image's ISO 21496-1 segment.
fn gain_map_version(data: &[u8], h: &Header) -> Option<[u8; 4]> {
    let p = h.iso21496.clone()?;
    let at = p.start + ISO21496_IDENTIFIER.len();
    data.get(at..at + 4)
        .filter(|_| at + 4 <= p.end)
        .and_then(|v| v.try_into().ok())
}

#[cfg(test)]
mod tests;
