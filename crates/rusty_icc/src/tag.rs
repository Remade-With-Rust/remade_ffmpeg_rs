//! Tag element types.

use crate::bytes::{Reader, Writer};
use crate::color::Xyz;
use crate::curve::Curve;
use crate::error::{Error, Result};
use crate::lut::Lut;

/// One localized string of a `multiLocalizedUnicodeType` tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalizedString {
    /// ISO 639-1 language code, e.g. `*b"en"`.
    pub language: [u8; 2],
    /// ISO 3166-1 country code, e.g. `*b"US"`.
    pub country: [u8; 2],
    /// The text as UTF-16 code units.
    pub text: Vec<u16>,
}

impl LocalizedString {
    /// An `en`/`US` record holding `text`.
    #[must_use]
    pub fn en_us(text: &str) -> Self {
        Self {
            language: *b"en",
            country: *b"US",
            text: text.encode_utf16().collect(),
        }
    }
}

/// A decoded tag element. Types this crate does not interpret are kept
/// byte-for-byte as [`Tag::Raw`] so profiles round-trip.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Tag {
    /// `XYZType`: one or more XYZ values.
    Xyz(Vec<Xyz>),
    /// `curveType` or `parametricCurveType`.
    Curve(Curve),
    /// `lut8Type`, `lut16Type`, `lutAToBType` or `lutBToAType`.
    Lut(Lut),
    /// `textType` (ASCII, without the terminating NUL).
    Text(Vec<u8>),
    /// `textDescriptionType` (ICC v2).
    Description {
        /// The ASCII description, without the terminating NUL.
        ascii: Vec<u8>,
        /// Unicode language code.
        unicode_language: u32,
        /// The Unicode description as UTF-16 code units, without a NUL.
        unicode: Vec<u16>,
        /// `ScriptCode` code.
        script_code: u16,
        /// `ScriptCode` description bytes (at most 67).
        script: Vec<u8>,
    },
    /// `multiLocalizedUnicodeType` (ICC v4).
    Mluc(Vec<LocalizedString>),
    /// `s15Fixed16ArrayType`.
    S15Fixed16(Vec<f64>),
    /// `signatureType`.
    Signature([u8; 4]),
    /// Any other type: the complete element, type signature included.
    Raw(Vec<u8>),
}

fn strip_nul(b: &[u8]) -> Vec<u8> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end].to_vec()
}

fn strip_nul16(b: Vec<u16>) -> Vec<u16> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let mut b = b;
    b.truncate(end);
    b
}

impl Tag {
    /// Parses the element in `r` (exactly the tag's bytes).
    pub(crate) fn parse(r: &Reader<'_>) -> Result<Self> {
        let mut h = *r;
        let sig = h.sig("tag type")?;
        match &sig {
            b"XYZ " => {
                h.skip(4, "XYZ reserved")?;
                let n = (r.len() - 8) / 12;
                if n == 0 {
                    return Err(Error::Malformed("empty XYZ tag"));
                }
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let x = h.s15f16("XYZ")?;
                    let y = h.s15f16("XYZ")?;
                    let z = h.s15f16("XYZ")?;
                    v.push(Xyz::new(x, y, z));
                }
                Ok(Self::Xyz(v))
            }
            b"curv" | b"para" => Ok(Self::Curve(Curve::parse(&mut r.clone())?)),
            b"mft1" => Ok(Self::Lut(Lut::parse_legacy(&mut r.clone(), false)?)),
            b"mft2" => Ok(Self::Lut(Lut::parse_legacy(&mut r.clone(), true)?)),
            b"mAB " => Ok(Self::Lut(Lut::parse_ab(r, true)?)),
            b"mBA " => Ok(Self::Lut(Lut::parse_ab(r, false)?)),
            b"text" => {
                h.skip(4, "text reserved")?;
                let rest = h.bytes(r.len() - 8, "text")?;
                Ok(Self::Text(strip_nul(rest)))
            }
            b"desc" => {
                h.skip(4, "desc reserved")?;
                let n = h.u32("desc ascii count")? as usize;
                let ascii = strip_nul(h.bytes(n, "desc ascii")?);
                // The Unicode and ScriptCode parts are often truncated in
                // the wild; read what is present.
                let mut unicode_language = 0;
                let mut unicode = Vec::new();
                let mut script_code = 0;
                let mut script = Vec::new();
                if let Ok(lang) = h.u32("desc unicode language") {
                    unicode_language = lang;
                    if let Ok(cnt) = h.u32("desc unicode count") {
                        unicode = strip_nul16(h.u16_vec(cnt as usize, "desc unicode")?);
                        if let Ok(code) = h.u16("desc scriptcode") {
                            script_code = code;
                            if let Ok(cnt) = h.u8("desc scriptcode count") {
                                let cnt = usize::from(cnt).min(67);
                                script = h.bytes(cnt, "desc scriptcode").unwrap_or(&[]).to_vec();
                            }
                        }
                    }
                }
                Ok(Self::Description {
                    ascii,
                    unicode_language,
                    unicode,
                    script_code,
                    script,
                })
            }
            b"mluc" => {
                h.skip(4, "mluc reserved")?;
                let n = h.u32("mluc count")? as usize;
                let rec = h.u32("mluc record size")? as usize;
                if rec < 12 {
                    return Err(Error::Malformed("mluc record size"));
                }
                let table = n.checked_mul(rec).ok_or(Error::Malformed("mluc count"))?;
                if table > r.len() {
                    return Err(Error::Malformed("mluc count"));
                }
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    let mut e = *r;
                    e.seek(16 + i * rec, "mluc record")?;
                    let lang = e.bytes(2, "mluc language")?;
                    let country = e.bytes(2, "mluc country")?;
                    let len = e.u32("mluc length")? as usize;
                    let off = e.u32("mluc offset")? as usize;
                    let mut s = r.sub(off, len, "mluc string")?;
                    let text = s.u16_vec(len / 2, "mluc string")?;
                    v.push(LocalizedString {
                        language: [lang[0], lang[1]],
                        country: [country[0], country[1]],
                        text: strip_nul16(text),
                    });
                }
                Ok(Self::Mluc(v))
            }
            b"sf32" => {
                h.skip(4, "sf32 reserved")?;
                let n = (r.len() - 8) / 4;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(h.s15f16("sf32")?);
                }
                Ok(Self::S15Fixed16(v))
            }
            b"sig " => {
                h.skip(4, "sig reserved")?;
                Ok(Self::Signature(h.sig("sig")?))
            }
            _ => Ok(Self::Raw(r.clone().bytes(r.len(), "tag")?.to_vec())),
        }
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        match self {
            Self::Xyz(v) => {
                w.sig(*b"XYZ ");
                w.u32(0);
                for x in v {
                    w.s15f16(x.x);
                    w.s15f16(x.y);
                    w.s15f16(x.z);
                }
            }
            Self::Curve(c) => c.encode(w),
            Self::Lut(l) => l.encode(w),
            Self::Text(t) => {
                w.sig(*b"text");
                w.u32(0);
                w.bytes(t);
                w.u8(0);
            }
            Self::Description {
                ascii,
                unicode_language,
                unicode,
                script_code,
                script,
            } => {
                w.sig(*b"desc");
                w.u32(0);
                w.u32(ascii.len() as u32 + 1);
                w.bytes(ascii);
                w.u8(0);
                w.u32(*unicode_language);
                if unicode.is_empty() {
                    w.u32(0);
                } else {
                    w.u32(unicode.len() as u32 + 1);
                    for &c in unicode {
                        w.u16(c);
                    }
                    w.u16(0);
                }
                w.u16(*script_code);
                let n = script.len().min(67);
                w.u8(n as u8);
                let mut s = [0u8; 67];
                s[..n].copy_from_slice(&script[..n]);
                w.bytes(&s);
            }
            Self::Mluc(v) => {
                let start = w.len();
                w.sig(*b"mluc");
                w.u32(0);
                w.u32(v.len() as u32);
                w.u32(12);
                let mut off = 16 + 12 * v.len();
                for s in v {
                    w.bytes(&s.language);
                    w.bytes(&s.country);
                    w.u32((s.text.len() * 2) as u32);
                    w.u32(off as u32);
                    off += s.text.len() * 2;
                }
                for s in v {
                    for &c in &s.text {
                        w.u16(c);
                    }
                }
                debug_assert_eq!(w.len() - start, off);
            }
            Self::S15Fixed16(v) => {
                w.sig(*b"sf32");
                w.u32(0);
                for &x in v {
                    w.s15f16(x);
                }
            }
            Self::Signature(s) => {
                w.sig(*b"sig ");
                w.u32(0);
                w.sig(*s);
            }
            Self::Raw(b) => w.bytes(b),
        }
    }

    /// The text of a `desc`/`cprt`-style tag: the `en`/`US` record (else the
    /// first) of an `mluc`, the ASCII part of a `desc`, or a `text`.
    #[must_use]
    pub fn as_text(&self) -> Option<String> {
        match self {
            Self::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
            Self::Description { ascii, unicode, .. } => {
                if ascii.is_empty() && !unicode.is_empty() {
                    Some(String::from_utf16_lossy(unicode))
                } else {
                    Some(String::from_utf8_lossy(ascii).into_owned())
                }
            }
            Self::Mluc(v) => {
                let pick = v
                    .iter()
                    .find(|s| &s.language == b"en" && &s.country == b"US")
                    .or_else(|| v.first())?;
                Some(String::from_utf16_lossy(&pick.text))
            }
            _ => None,
        }
    }
}
