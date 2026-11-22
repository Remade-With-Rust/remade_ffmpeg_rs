//! ICC profiles: header, tag table, parsing, encoding and built-in
//! constructors.

use std::collections::HashMap;
use std::sync::Arc;

use crate::bytes::{Reader, Writer};
use crate::color::{bradford, mat_mul, q15f16, rgb_to_xyz_matrix, Mat3, Xy, Xyz, D50, D65_XY};
use crate::curve::Curve;
use crate::error::{Error, Result};
use crate::lut::{AbLut, Lut};
use crate::md5::md5;
use crate::tag::{LocalizedString, Tag};
use crate::transform::Intent;

/// Most tags a profile may carry (`LittleCMS` caps at 100; real profiles use
/// far fewer). Bounds the tag-table walk on hostile input.
const MAX_TAGS: usize = 1024;

/// A four-byte tag signature such as `*b"A2B0"`.
pub type TagSignature = [u8; 4];

/// Profile/device class (header bytes 12..16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileClass {
    /// `scnr`: input device (camera, scanner).
    Input,
    /// `mntr`: display.
    Display,
    /// `prtr`: output device (printer, press).
    Output,
    /// `link`: device link.
    Link,
    /// `abst`: abstract (PCS to PCS).
    Abstract,
    /// `spac`: colour space conversion.
    ColorSpace,
    /// `nmcl`: named colour.
    NamedColor,
    /// Any other signature.
    Other([u8; 4]),
}

impl ProfileClass {
    fn from_sig(s: [u8; 4]) -> Self {
        match &s {
            b"scnr" => Self::Input,
            b"mntr" => Self::Display,
            b"prtr" => Self::Output,
            b"link" => Self::Link,
            b"abst" => Self::Abstract,
            b"spac" => Self::ColorSpace,
            b"nmcl" => Self::NamedColor,
            _ => Self::Other(s),
        }
    }

    const fn sig(self) -> [u8; 4] {
        match self {
            Self::Input => *b"scnr",
            Self::Display => *b"mntr",
            Self::Output => *b"prtr",
            Self::Link => *b"link",
            Self::Abstract => *b"abst",
            Self::ColorSpace => *b"spac",
            Self::NamedColor => *b"nmcl",
            Self::Other(s) => s,
        }
    }
}

/// Data colour space or PCS (header bytes 16..24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorSpace {
    /// `XYZ `.
    Xyz,
    /// `Lab `.
    Lab,
    /// `GRAY`.
    Gray,
    /// `RGB `.
    Rgb,
    /// `CMYK`.
    Cmyk,
    /// Any other signature (kept for round-tripping; not transformable).
    Other([u8; 4]),
}

impl ColorSpace {
    fn from_sig(s: [u8; 4]) -> Self {
        match &s {
            b"XYZ " => Self::Xyz,
            b"Lab " => Self::Lab,
            b"GRAY" => Self::Gray,
            b"RGB " => Self::Rgb,
            b"CMYK" => Self::Cmyk,
            _ => Self::Other(s),
        }
    }

    const fn sig(self) -> [u8; 4] {
        match self {
            Self::Xyz => *b"XYZ ",
            Self::Lab => *b"Lab ",
            Self::Gray => *b"GRAY",
            Self::Rgb => *b"RGB ",
            Self::Cmyk => *b"CMYK",
            Self::Other(s) => s,
        }
    }

    /// Channel count, for the colour spaces this crate transforms.
    #[must_use]
    pub const fn channels(self) -> Option<usize> {
        match self {
            Self::Gray => Some(1),
            Self::Xyz | Self::Lab | Self::Rgb => Some(3),
            Self::Cmyk => Some(4),
            Self::Other(_) => None,
        }
    }
}

/// The 128-byte profile header. `size` is not stored: [`Profile::encode`]
/// computes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// Preferred CMM signature.
    pub cmm: [u8; 4],
    /// Encoded version, e.g. `0x0440_0000` for 4.4.
    pub version: u32,
    /// Device class.
    pub class: ProfileClass,
    /// Data colour space.
    pub color_space: ColorSpace,
    /// Profile connection space (`Xyz` or `Lab`).
    pub pcs: ColorSpace,
    /// Creation date and time: year, month, day, hours, minutes, seconds.
    pub date: [u16; 6],
    /// Primary platform signature.
    pub platform: [u8; 4],
    /// Profile flags.
    pub flags: u32,
    /// Device manufacturer signature.
    pub manufacturer: [u8; 4],
    /// Device model signature.
    pub model: u32,
    /// Device attributes.
    pub attributes: u64,
    /// Default rendering intent.
    pub rendering_intent: u32,
    /// PCS illuminant (D50 in practice).
    pub illuminant: Xyz,
    /// Profile creator signature.
    pub creator: [u8; 4],
    /// Profile ID as read (an MD5 checksum in v4; [`Profile::encode`]
    /// recomputes it for v4 and writes zeros for v2).
    pub profile_id: [u8; 16],
}

/// A parsed (or constructed) ICC profile: its header and tags in file
/// order. Tags whose type this crate does not interpret are carried as
/// [`Tag::Raw`], so `Profile::new_from_slice(&p.encode())` equals `p`.
#[derive(Debug, Clone)]
pub struct Profile {
    /// The header.
    pub header: Header,
    tags: Vec<(TagSignature, Arc<Tag>)>,
}

/// Two profiles are equal when their headers (ignoring the profile ID,
/// which is a checksum of one particular encoding) and tags are equal.
impl PartialEq for Profile {
    fn eq(&self, other: &Self) -> bool {
        let mut a = self.header.clone();
        let mut b = other.header.clone();
        a.profile_id = [0; 16];
        b.profile_id = [0; 16];
        a == b
            && self.tags.len() == other.tags.len()
            && self
                .tags
                .iter()
                .zip(&other.tags)
                .all(|((sa, ta), (sb, tb))| sa == sb && (Arc::ptr_eq(ta, tb) || ta == tb))
    }
}

impl Profile {
    /// Parses a profile from its bytes.
    ///
    /// Every offset, size and count is bounds-checked against the input, so
    /// malformed or hostile data returns [`Error::Malformed`] and never
    /// panics. Memory held by the result is bounded by a small multiple of
    /// the input length: tag elements are decoded only from bytes that are
    /// present, tags sharing an offset are decoded once, and the total
    /// size of decoded tags is capped at four times the profile size.
    ///
    /// # Errors
    /// [`Error::Malformed`] for structural problems, [`Error::Unsupported`]
    /// for ICC versions other than 2 and 4.
    pub fn new_from_slice(data: &[u8]) -> Result<Self> {
        let all = Reader::new(data);
        if data.len() < 132 {
            return Err(Error::Malformed("shorter than header and tag count"));
        }
        let mut h = all;
        let size = h.u32("header size")? as usize;
        if size < 132 || size > data.len() {
            return Err(Error::Malformed("header size field"));
        }
        let r = all.sub(0, size, "profile")?;
        let mut h = r;
        h.seek(4, "header")?;
        let cmm = h.sig("cmm")?;
        let version = h.u32("version")?;
        let major = version >> 24;
        if !(2..=4).contains(&major) {
            return Err(Error::Unsupported("ICC version other than 2 or 4"));
        }
        let class = ProfileClass::from_sig(h.sig("class")?);
        let color_space = ColorSpace::from_sig(h.sig("color space")?);
        let pcs = ColorSpace::from_sig(h.sig("pcs")?);
        let mut date = [0u16; 6];
        for d in &mut date {
            *d = h.u16("date")?;
        }
        if &h.sig("magic")? != b"acsp" {
            return Err(Error::Malformed("missing acsp signature"));
        }
        let platform = h.sig("platform")?;
        let flags = h.u32("flags")?;
        let manufacturer = h.sig("manufacturer")?;
        let model = h.u32("model")?;
        let attributes = h.u64("attributes")?;
        let rendering_intent = h.u32("intent")?;
        let illuminant = Xyz::new(
            h.s15f16("illuminant")?,
            h.s15f16("illuminant")?,
            h.s15f16("illuminant")?,
        );
        let creator = h.sig("creator")?;
        let mut profile_id = [0u8; 16];
        profile_id.copy_from_slice(h.bytes(16, "profile id")?);
        let header = Header {
            cmm,
            version,
            class,
            color_space,
            pcs,
            date,
            platform,
            flags,
            manufacturer,
            model,
            attributes,
            rendering_intent,
            illuminant,
            creator,
            profile_id,
        };

        h.seek(128, "tag count")?;
        let count = h.u32("tag count")? as usize;
        if count > MAX_TAGS || count * 12 > size - 132 {
            return Err(Error::Malformed("tag count"));
        }
        let mut cache: HashMap<(usize, usize), Arc<Tag>> = HashMap::new();
        let mut budget = size.saturating_mul(4);
        let mut tags: Vec<(TagSignature, Arc<Tag>)> = Vec::with_capacity(count);
        for _ in 0..count {
            let sig = h.sig("tag signature")?;
            let off = h.u32("tag offset")? as usize;
            let len = h.u32("tag size")? as usize;
            if len < 8 {
                return Err(Error::Malformed("tag smaller than its type header"));
            }
            let body = r.sub(off, len, "tag offset/size past end of profile")?;
            if tags.iter().any(|(s, _)| *s == sig) {
                continue; // first occurrence wins, as in LittleCMS
            }
            let tag = if let Some(t) = cache.get(&(off, len)) {
                Arc::clone(t)
            } else {
                budget = budget
                    .checked_sub(len)
                    .ok_or(Error::Malformed("tag data exceeds 4x profile size"))?;
                let t = Arc::new(Tag::parse(&body)?);
                cache.insert((off, len), Arc::clone(&t));
                t
            };
            tags.push((sig, tag));
        }
        Ok(Self { header, tags })
    }

    /// Serialises the profile. Tag data is 4-byte aligned, identical tags
    /// share one copy, and v4 profiles get their MD5 profile ID computed.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let hint = 132
            + 12 * self.tags.len()
            + self
                .tags
                .iter()
                .map(|(_, t)| t.size_hint() + 3)
                .sum::<usize>();
        let mut w = Writer {
            buf: Vec::with_capacity(hint),
        };
        w.zeros(128);
        w.u32(self.tags.len() as u32);
        let table_at = w.len();
        w.zeros(12 * self.tags.len());
        let mut placed: Vec<(usize, usize, usize)> = Vec::new(); // (tag idx, offset, size)
        for (i, (_, tag)) in self.tags.iter().enumerate() {
            let dup = placed.iter().find(|&&(j, _, _)| {
                let other = &self.tags[j].1;
                Arc::ptr_eq(other, tag) || **other == **tag
            });
            let (off, len) = if let Some(&(_, o, l)) = dup {
                (o, l)
            } else {
                w.pad4();
                let o = w.len();
                tag.encode(&mut w);
                (o, w.len() - o)
            };
            placed.push((i, off, len));
        }
        w.pad4();
        for (k, (i, off, len)) in placed.iter().enumerate() {
            let p = table_at + 12 * k;
            w.buf[p..p + 4].copy_from_slice(&self.tags[*i].0);
            w.buf[p + 4..p + 8].copy_from_slice(&(*off as u32).to_be_bytes());
            w.buf[p + 8..p + 12].copy_from_slice(&(*len as u32).to_be_bytes());
        }
        let total = w.len() as u32;
        let mut hw = Writer::default();
        let hd = &self.header;
        hw.u32(total);
        hw.sig(hd.cmm);
        hw.u32(hd.version);
        hw.sig(hd.class.sig());
        hw.sig(hd.color_space.sig());
        hw.sig(hd.pcs.sig());
        for &d in &hd.date {
            hw.u16(d);
        }
        hw.sig(*b"acsp");
        hw.sig(hd.platform);
        hw.u32(hd.flags);
        hw.sig(hd.manufacturer);
        hw.u32(hd.model);
        hw.u64(hd.attributes);
        hw.u32(hd.rendering_intent);
        hw.s15f16(hd.illuminant.x);
        hw.s15f16(hd.illuminant.y);
        hw.s15f16(hd.illuminant.z);
        hw.sig(hd.creator);
        hw.zeros(16); // profile ID, filled below
        hw.zeros(128 - hw.len());
        w.buf[..128].copy_from_slice(&hw.buf);
        if hd.version >> 24 >= 4 {
            let mut tmp = w.buf.clone();
            tmp[44..48].fill(0);
            tmp[64..68].fill(0);
            tmp[84..100].fill(0);
            let id = md5(&tmp);
            w.buf[84..100].copy_from_slice(&id);
        }
        w.buf
    }

    // ---------------------------------------------------------------- tags

    /// The tag with signature `sig`.
    #[must_use]
    pub fn tag(&self, sig: TagSignature) -> Option<&Tag> {
        self.tags.iter().find(|(s, _)| *s == sig).map(|(_, t)| &**t)
    }

    /// All tags in file order.
    pub fn tags(&self) -> impl Iterator<Item = (TagSignature, &Tag)> {
        self.tags.iter().map(|(s, t)| (*s, &**t))
    }

    /// Sets (replacing) or appends a tag.
    pub fn set_tag(&mut self, sig: TagSignature, tag: Tag) {
        self.set_tag_arc(sig, Arc::new(tag));
    }

    fn set_tag_arc(&mut self, sig: TagSignature, tag: Arc<Tag>) {
        if let Some(e) = self.tags.iter_mut().find(|(s, _)| *s == sig) {
            e.1 = tag;
        } else {
            self.tags.push((sig, tag));
        }
    }

    /// Removes a tag, returning it.
    pub fn remove_tag(&mut self, sig: TagSignature) -> Option<Tag> {
        let i = self.tags.iter().position(|(s, _)| *s == sig)?;
        let (_, t) = self.tags.remove(i);
        Some(Arc::try_unwrap(t).unwrap_or_else(|a| (*a).clone()))
    }

    /// Major and minor version, e.g. `(4, 4)`.
    #[must_use]
    pub fn version(&self) -> (u8, u8) {
        let v = self.header.version;
        ((v >> 24) as u8, ((v >> 20) & 0xf) as u8)
    }

    /// The profile description (`desc`).
    #[must_use]
    pub fn description(&self) -> Option<String> {
        self.tag(*b"desc").and_then(Tag::as_text)
    }

    /// The copyright string (`cprt`).
    #[must_use]
    pub fn copyright(&self) -> Option<String> {
        self.tag(*b"cprt").and_then(Tag::as_text)
    }

    /// Sets the description, as `mluc` for v4 profiles and
    /// `textDescriptionType` for v2.
    pub fn set_description(&mut self, text: &str) {
        let tag = self.text_tag(text, true);
        self.set_tag(*b"desc", tag);
    }

    /// Sets the copyright string, as `mluc` for v4 and `textType` for v2.
    pub fn set_copyright(&mut self, text: &str) {
        let tag = self.text_tag(text, false);
        self.set_tag(*b"cprt", tag);
    }

    fn text_tag(&self, text: &str, description: bool) -> Tag {
        if self.header.version >> 24 >= 4 {
            Tag::Mluc(vec![LocalizedString::en_us(text)])
        } else if description {
            Tag::Description {
                ascii: text
                    .bytes()
                    .map(|b| if b.is_ascii() { b } else { b'?' })
                    .collect(),
                unicode_language: 0,
                unicode: Vec::new(),
                script_code: 0,
                script: Vec::new(),
            }
        } else {
            Tag::Text(
                text.bytes()
                    .map(|b| if b.is_ascii() { b } else { b'?' })
                    .collect(),
            )
        }
    }

    fn xyz_tag(&self, sig: TagSignature) -> Option<Xyz> {
        match self.tag(sig)? {
            Tag::Xyz(v) => v.first().copied(),
            _ => None,
        }
    }

    pub(crate) fn curve_tag(&self, sig: TagSignature) -> Option<&Curve> {
        match self.tag(sig)? {
            Tag::Curve(c) => Some(c),
            _ => None,
        }
    }

    /// The media white point tag (`wtpt`) as stored.
    #[must_use]
    pub fn media_white_point(&self) -> Option<Xyz> {
        self.xyz_tag(*b"wtpt")
    }

    /// The chromatic adaptation matrix (`chad`), row-major.
    #[must_use]
    pub fn chromatic_adaptation(&self) -> Option<[[f64; 3]; 3]> {
        match self.tag(*b"chad")? {
            Tag::S15Fixed16(v) if v.len() >= 9 => {
                Some([[v[0], v[1], v[2]], [v[3], v[4], v[5]], [v[6], v[7], v[8]]])
            }
            _ => None,
        }
    }

    /// The device's own (unadapted) white: `chad`⁻¹ applied to `wtpt` when
    /// a `chad` tag is present, otherwise `wtpt` itself. For a v4 sRGB
    /// display profile this recovers D65 from the D50-adapted `wtpt`.
    #[must_use]
    pub fn native_white_point(&self) -> Option<Xyz> {
        let w = self.media_white_point()?;
        match self
            .chromatic_adaptation()
            .and_then(|m| crate::color::mat_inv(&m))
        {
            Some(inv) => Some(Xyz::from_array(crate::color::mat_vec(&inv, w.to_array()))),
            None => Some(w),
        }
    }

    /// The media white point `LittleCMS` uses for absolute colorimetric
    /// scaling: `wtpt`, except D50 when absent and for v2 display profiles.
    pub(crate) fn absolute_white_point(&self) -> Xyz {
        match self.media_white_point() {
            None => D50,
            Some(_)
                if self.header.version < 0x0400_0000
                    && self.header.class == ProfileClass::Display =>
            {
                D50
            }
            Some(w) if w.y > 0.0 => w,
            Some(_) => D50,
        }
    }

    /// The red, green and blue colorants (`rXYZ`, `gXYZ`, `bXYZ`).
    #[must_use]
    pub fn colorants(&self) -> Option<[Xyz; 3]> {
        Some([
            self.xyz_tag(*b"rXYZ")?,
            self.xyz_tag(*b"gXYZ")?,
            self.xyz_tag(*b"bXYZ")?,
        ])
    }

    /// `true` for an RGB matrix/TRC profile or a grey TRC profile.
    #[must_use]
    pub fn is_matrix_shaper(&self) -> bool {
        match self.header.color_space {
            ColorSpace::Gray => self.curve_tag(*b"kTRC").is_some(),
            ColorSpace::Rgb => {
                self.colorants().is_some()
                    && self.curve_tag(*b"rTRC").is_some()
                    && self.curve_tag(*b"gTRC").is_some()
                    && self.curve_tag(*b"bTRC").is_some()
            }
            _ => false,
        }
    }

    pub(crate) fn lut_tag(&self, sig: TagSignature) -> Result<Option<&Lut>> {
        match self.tag(sig) {
            None => Ok(None),
            Some(Tag::Lut(l)) => Ok(Some(l)),
            Some(_) => Err(Error::Unsupported("LUT tag of an uninterpreted type")),
        }
    }

    /// The device-to-PCS LUT used for `intent`: `A2B0`/`A2B1`/`A2B2`
    /// (absolute colorimetric uses `A2B1`), falling back to `A2B0`, as
    /// `LittleCMS` does. `None` when the profile has no such tag.
    ///
    /// # Errors
    /// [`Error::Unsupported`] when the tag exists but is not a LUT type this
    /// crate interprets (such as `multiProcessElementType`).
    pub fn a2b(&self, intent: Intent) -> Result<Option<&Lut>> {
        let sig = [*b"A2B0", *b"A2B1", *b"A2B2", *b"A2B1"][intent as usize];
        match self.lut_tag(sig)? {
            Some(l) => Ok(Some(l)),
            None => self.lut_tag(*b"A2B0"),
        }
    }

    /// The PCS-to-device LUT used for `intent` (`B2A0`/`B2A1`/`B2A2`,
    /// absolute colorimetric using `B2A1`), falling back to `B2A0`.
    ///
    /// # Errors
    /// As [`Profile::a2b`].
    pub fn b2a(&self, intent: Intent) -> Result<Option<&Lut>> {
        let sig = [*b"B2A0", *b"B2A1", *b"B2A2", *b"B2A1"][intent as usize];
        match self.lut_tag(sig)? {
            Some(l) => Ok(Some(l)),
            None => self.lut_tag(*b"B2A0"),
        }
    }

    /// `LittleCMS`'s `cmsIsCLUT`: the exact tag for `intent` exists (no
    /// fallback).
    pub(crate) fn has_exact_lut(&self, intent: Intent, input: bool) -> bool {
        let sig = if input {
            [*b"A2B0", *b"A2B1", *b"A2B2", *b"A2B1"][intent as usize]
        } else {
            [*b"B2A0", *b"B2A1", *b"B2A2", *b"B2A1"][intent as usize]
        };
        matches!(self.tag(sig), Some(Tag::Lut(_)))
    }

    /// `LittleCMS`'s `cmsIsIntentSupported`.
    pub(crate) fn intent_supported(&self, intent: Intent, input: bool) -> bool {
        self.has_exact_lut(intent, input) || self.is_matrix_shaper()
    }

    /// The black point of this profile used as a source with `intent`, in
    /// D50 XYZ (`LittleCMS`'s `cmsDetectBlackPoint`). Zero when undefined
    /// (absolute colorimetric, device-link and abstract profiles).
    #[must_use]
    pub fn detect_black_point(&self, intent: Intent) -> Xyz {
        crate::pipeline::detect_black_point(self, intent)
    }

    /// The black point of this profile used as a destination with
    /// `intent`, in D50 XYZ (`LittleCMS`'s `cmsDetectDestinationBlackPoint`,
    /// the Adobe round-trip algorithm for LUT-based outputs).
    #[must_use]
    pub fn detect_destination_black_point(&self, intent: Intent) -> Xyz {
        crate::pipeline::detect_destination_black_point(self, intent)
    }

    // ---------------------------------------------------------- constructors

    pub(crate) fn blank(class: ProfileClass, color_space: ColorSpace, pcs: ColorSpace) -> Self {
        Self {
            header: Header {
                cmm: [0; 4],
                version: 0x0440_0000,
                class,
                color_space,
                pcs,
                date: [2026, 1, 1, 0, 0, 0],
                platform: [0; 4],
                flags: 0,
                manufacturer: [0; 4],
                model: 0,
                attributes: 0,
                rendering_intent: 0,
                illuminant: Xyz::new(q15f16(D50.x), q15f16(D50.y), q15f16(D50.z)),
                creator: [0; 4],
                profile_id: [0; 16],
            },
            tags: Vec::new(),
        }
    }

    /// An ICC v4 RGB matrix/TRC display profile from a white point,
    /// primaries and one tone curve shared by the three channels. The
    /// colorants are Bradford-adapted to D50 and a `chad` tag records the
    /// adaptation; all values are rounded to `s15Fixed16Number`, so the
    /// profile equals its own encoding parsed back.
    ///
    /// # Errors
    /// [`Error::InvalidTransform`] when the primaries are degenerate.
    pub fn new_rgb(white: Xy, primaries: [Xy; 3], trc: Curve) -> Result<Self> {
        let m = rgb_to_xyz_matrix(white, primaries)
            .ok_or(Error::InvalidTransform("degenerate primaries"))?;
        let chad = bradford(white.to_xyz(), D50);
        let adapted: Mat3 = mat_mul(&chad, &m);
        let mut p = Self::blank(ProfileClass::Display, ColorSpace::Rgb, ColorSpace::Xyz);
        p.set_description("RGB (rusty_icc)");
        p.set_copyright("No copyright, use freely");
        p.set_tag(*b"wtpt", Tag::Xyz(vec![quant_xyz(D50)]));
        p.set_tag(
            *b"chad",
            Tag::S15Fixed16(chad.iter().flatten().map(|&v| q15f16(v)).collect()),
        );
        for (i, sig) in [*b"rXYZ", *b"gXYZ", *b"bXYZ"].into_iter().enumerate() {
            let c = Xyz::new(adapted[0][i], adapted[1][i], adapted[2][i]);
            p.set_tag(sig, Tag::Xyz(vec![quant_xyz(c)]));
        }
        let trc = Arc::new(Tag::Curve(trc));
        for sig in [*b"rTRC", *b"gTRC", *b"bTRC"] {
            p.set_tag_arc(sig, Arc::clone(&trc));
        }
        Ok(p)
    }

    /// An ICC v4 grey display profile with white point `white` and tone
    /// curve `trc` (`kTRC`).
    #[must_use]
    pub fn new_gray(white: Xy, trc: Curve) -> Self {
        let chad = bradford(white.to_xyz(), D50);
        let mut p = Self::blank(ProfileClass::Display, ColorSpace::Gray, ColorSpace::Xyz);
        p.set_description("Gray (rusty_icc)");
        p.set_copyright("No copyright, use freely");
        p.set_tag(*b"wtpt", Tag::Xyz(vec![quant_xyz(D50)]));
        p.set_tag(
            *b"chad",
            Tag::S15Fixed16(chad.iter().flatten().map(|&v| q15f16(v)).collect()),
        );
        p.set_tag(*b"kTRC", Tag::Curve(trc));
        p
    }

    /// sRGB (IEC 61966-2-1): BT.709 primaries, D65, the sRGB curve.
    #[must_use]
    pub fn new_srgb() -> Self {
        let mut p = Self::new_rgb(
            D65_XY,
            [
                Xy::new(0.64, 0.33),
                Xy::new(0.30, 0.60),
                Xy::new(0.15, 0.06),
            ],
            Curve::srgb(),
        )
        .unwrap_or_else(|_| Self::blank(ProfileClass::Display, ColorSpace::Rgb, ColorSpace::Xyz));
        p.set_description("sRGB (rusty_icc)");
        p
    }

    /// Display P3: DCI-P3 primaries, D65, the sRGB curve.
    #[must_use]
    pub fn new_display_p3() -> Self {
        let mut p = Self::new_rgb(
            D65_XY,
            [
                Xy::new(0.680, 0.320),
                Xy::new(0.265, 0.690),
                Xy::new(0.150, 0.060),
            ],
            Curve::srgb(),
        )
        .unwrap_or_else(|_| Self::blank(ProfileClass::Display, ColorSpace::Rgb, ColorSpace::Xyz));
        p.set_description("Display P3 (rusty_icc)");
        p
    }

    /// ITU-R BT.2020: BT.2020 primaries, D65, the BT.709/BT.2020 curve.
    #[must_use]
    pub fn new_rec2020() -> Self {
        let mut p = Self::new_rgb(
            D65_XY,
            [
                Xy::new(0.708, 0.292),
                Xy::new(0.170, 0.797),
                Xy::new(0.131, 0.046),
            ],
            Curve::rec709(),
        )
        .unwrap_or_else(|_| Self::blank(ProfileClass::Display, ColorSpace::Rgb, ColorSpace::Xyz));
        p.set_description("Rec. 2020 (rusty_icc)");
        p
    }

    /// The identity L*a*b* profile (D50), as `LittleCMS`'s
    /// `cmsCreateLab4Profile`: a v4 abstract `Lab `/`Lab ` profile whose
    /// `A2B0` and `B2A0` are identity curves. Used with
    /// [`Layout::Lab`](crate::Layout::Lab) to feed or read L*a*b* values.
    #[must_use]
    pub fn new_lab_identity() -> Self {
        let mut p = Self::blank(ProfileClass::Abstract, ColorSpace::Lab, ColorSpace::Lab);
        p.set_description("Lab identity (rusty_icc)");
        p.set_copyright("No copyright, use freely");
        p.set_tag(*b"wtpt", Tag::Xyz(vec![quant_xyz(D50)]));
        let id = || AbLut {
            inputs: 3,
            outputs: 3,
            a: None,
            clut: None,
            m: None,
            matrix: None,
            b: vec![Curve::gamma(1.0), Curve::gamma(1.0), Curve::gamma(1.0)],
        };
        p.set_tag(*b"A2B0", Tag::Lut(Lut::AToB(id())));
        p.set_tag(*b"B2A0", Tag::Lut(Lut::BToA(id())));
        p
    }
}

fn quant_xyz(v: Xyz) -> Xyz {
    Xyz::new(q15f16(v.x), q15f16(v.y), q15f16(v.z))
}
