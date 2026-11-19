//! Typed tag values.

use crate::tags::Type;

/// The decoded value of one IFD entry: a vector of the entry's field type.
///
/// Unlike image-tiff's `Value` (one enum node per element), arrays stay
/// packed, so a 3 MB ICC profile is one `Vec<u8>` and not three million enum
/// values. Use the `into_*`/`as_*` helpers for type-agnostic access.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// `BYTE` (1).
    Byte(Vec<u8>),
    /// `ASCII` (2), lossily decoded, trailing NULs removed. Embedded NULs
    /// (multi-string values) are kept.
    Ascii(String),
    /// `SHORT` (3).
    Short(Vec<u16>),
    /// `LONG` (4).
    Long(Vec<u32>),
    /// `RATIONAL` (5) as `[numerator, denominator]`.
    Rational(Vec<[u32; 2]>),
    /// `SBYTE` (6).
    SByte(Vec<i8>),
    /// `UNDEFINED` (7): opaque bytes (ICC profiles, EXIF blobs...).
    Undefined(Vec<u8>),
    /// `SSHORT` (8).
    SShort(Vec<i16>),
    /// `SLONG` (9).
    SLong(Vec<i32>),
    /// `SRATIONAL` (10) as `[numerator, denominator]`.
    SRational(Vec<[i32; 2]>),
    /// `FLOAT` (11).
    Float(Vec<f32>),
    /// `DOUBLE` (12).
    Double(Vec<f64>),
    /// `IFD` (13): 32-bit IFD offsets.
    Ifd(Vec<u32>),
    /// `LONG8` (16, BigTIFF).
    Long8(Vec<u64>),
    /// `SLONG8` (17, BigTIFF).
    SLong8(Vec<i64>),
    /// `IFD8` (18, BigTIFF): 64-bit IFD offsets.
    Ifd8(Vec<u64>),
}

impl Value {
    /// The TIFF field type of this value.
    #[must_use]
    pub fn field_type(&self) -> Type {
        match self {
            Self::Byte(_) => Type::Byte,
            Self::Ascii(_) => Type::Ascii,
            Self::Short(_) => Type::Short,
            Self::Long(_) => Type::Long,
            Self::Rational(_) => Type::Rational,
            Self::SByte(_) => Type::SByte,
            Self::Undefined(_) => Type::Undefined,
            Self::SShort(_) => Type::SShort,
            Self::SLong(_) => Type::SLong,
            Self::SRational(_) => Type::SRational,
            Self::Float(_) => Type::Float,
            Self::Double(_) => Type::Double,
            Self::Ifd(_) => Type::Ifd,
            Self::Long8(_) => Type::Long8,
            Self::SLong8(_) => Type::SLong8,
            Self::Ifd8(_) => Type::Ifd8,
        }
    }

    /// Number of elements (for `Ascii`, the byte length plus the NUL).
    #[must_use]
    pub fn count(&self) -> usize {
        match self {
            Self::Byte(v) | Self::Undefined(v) => v.len(),
            Self::Ascii(s) => s.len() + 1,
            Self::Short(v) => v.len(),
            Self::Long(v) | Self::Ifd(v) => v.len(),
            Self::Rational(v) => v.len(),
            Self::SByte(v) => v.len(),
            Self::SShort(v) => v.len(),
            Self::SLong(v) => v.len(),
            Self::SRational(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Double(v) => v.len(),
            Self::Long8(v) | Self::Ifd8(v) => v.len(),
            Self::SLong8(v) => v.len(),
        }
    }

    /// All elements as `u64`, for any unsigned integer type (`BYTE`, `SHORT`,
    /// `LONG`, `IFD`, `LONG8`, `IFD8`). `None` for other types.
    #[must_use]
    pub fn to_u64_vec(&self) -> Option<Vec<u64>> {
        Some(match self {
            Self::Byte(v) | Self::Undefined(v) => v.iter().map(|&x| u64::from(x)).collect(),
            Self::Short(v) => v.iter().map(|&x| u64::from(x)).collect(),
            Self::Long(v) | Self::Ifd(v) => v.iter().map(|&x| u64::from(x)).collect(),
            Self::Long8(v) | Self::Ifd8(v) => v.clone(),
            _ => return None,
        })
    }

    /// All elements as `u32` (unsigned types whose values fit).
    #[must_use]
    pub fn to_u32_vec(&self) -> Option<Vec<u32>> {
        self.to_u64_vec()?
            .into_iter()
            .map(|x| u32::try_from(x).ok())
            .collect()
    }

    /// All elements as `u16` (unsigned types whose values fit).
    #[must_use]
    pub fn to_u16_vec(&self) -> Option<Vec<u16>> {
        self.to_u64_vec()?
            .into_iter()
            .map(|x| u16::try_from(x).ok())
            .collect()
    }

    /// The single element of an unsigned integer value.
    #[must_use]
    pub fn to_u64(&self) -> Option<u64> {
        match self.to_u64_vec()?.as_slice() {
            [x] => Some(*x),
            _ => None,
        }
    }

    /// The single element of an unsigned integer value, if it fits `u32`.
    #[must_use]
    pub fn to_u32(&self) -> Option<u32> {
        self.to_u64().and_then(|x| u32::try_from(x).ok())
    }

    /// All elements as `f64`, for every numeric type (rationals divided out).
    #[must_use]
    pub fn to_f64_vec(&self) -> Option<Vec<f64>> {
        Some(match self {
            Self::Byte(v) | Self::Undefined(v) => v.iter().map(|&x| f64::from(x)).collect(),
            Self::Short(v) => v.iter().map(|&x| f64::from(x)).collect(),
            Self::Long(v) | Self::Ifd(v) => v.iter().map(|&x| f64::from(x)).collect(),
            Self::Long8(v) | Self::Ifd8(v) => v.iter().map(|&x| x as f64).collect(),
            Self::SByte(v) => v.iter().map(|&x| f64::from(x)).collect(),
            Self::SShort(v) => v.iter().map(|&x| f64::from(x)).collect(),
            Self::SLong(v) => v.iter().map(|&x| f64::from(x)).collect(),
            Self::SLong8(v) => v.iter().map(|&x| x as f64).collect(),
            Self::Float(v) => v.iter().map(|&x| f64::from(x)).collect(),
            Self::Double(v) => v.clone(),
            Self::Rational(v) => v
                .iter()
                .map(|r| f64::from(r[0]) / f64::from(r[1]))
                .collect(),
            Self::SRational(v) => v
                .iter()
                .map(|r| f64::from(r[0]) / f64::from(r[1]))
                .collect(),
            Self::Ascii(_) => return None,
        })
    }

    /// The bytes of a `BYTE`, `UNDEFINED` or `ASCII` value.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Byte(v) | Self::Undefined(v) => Some(v),
            Self::Ascii(s) => Some(s.as_bytes()),
            _ => None,
        }
    }

    /// The string of an `ASCII` value.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Ascii(s) => Some(s),
            _ => None,
        }
    }

    /// Serialize the elements little-endian, as the encoder writes them.
    pub(crate) fn to_le_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Byte(v) | Self::Undefined(v) => out.extend_from_slice(v),
            Self::Ascii(s) => {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Self::Short(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::Long(v) | Self::Ifd(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::Rational(v) => out.extend(v.iter().flatten().flat_map(|x| x.to_le_bytes())),
            Self::SByte(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::SShort(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::SLong(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::SRational(v) => out.extend(v.iter().flatten().flat_map(|x| x.to_le_bytes())),
            Self::Float(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::Double(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::Long8(v) | Self::Ifd8(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            Self::SLong8(v) => out.extend(v.iter().flat_map(|x| x.to_le_bytes())),
        }
        out
    }

    /// Decode `count` elements of type `ty` from `bytes` (already exactly
    /// `count * ty.size()` long) in the given byte order.
    pub(crate) fn decode(ty: Type, bytes: &[u8], big_endian: bool) -> Self {
        macro_rules! arr {
            ($t:ty, $n:expr) => {
                bytes
                    .chunks_exact($n)
                    .map(|c| {
                        let a: [u8; $n] = c.try_into().unwrap_or([0; $n]);
                        if big_endian {
                            <$t>::from_be_bytes(a)
                        } else {
                            <$t>::from_le_bytes(a)
                        }
                    })
                    .collect::<Vec<$t>>()
            };
        }
        match ty {
            Type::Byte => Self::Byte(bytes.to_vec()),
            Type::Undefined => Self::Undefined(bytes.to_vec()),
            Type::Ascii => {
                let mut end = bytes.len();
                while end > 0 && bytes[end - 1] == 0 {
                    end -= 1;
                }
                Self::Ascii(String::from_utf8_lossy(&bytes[..end]).into_owned())
            }
            Type::SByte => Self::SByte(bytes.iter().map(|&b| b as i8).collect()),
            Type::Short => Self::Short(arr!(u16, 2)),
            Type::SShort => Self::SShort(arr!(i16, 2)),
            Type::Long => Self::Long(arr!(u32, 4)),
            Type::Ifd => Self::Ifd(arr!(u32, 4)),
            Type::SLong => Self::SLong(arr!(i32, 4)),
            Type::Float => Self::Float(arr!(f32, 4)),
            Type::Double => Self::Double(arr!(f64, 8)),
            Type::Long8 => Self::Long8(arr!(u64, 8)),
            Type::Ifd8 => Self::Ifd8(arr!(u64, 8)),
            Type::SLong8 => Self::SLong8(arr!(i64, 8)),
            Type::Rational => {
                let v = arr!(u32, 4);
                Self::Rational(v.chunks_exact(2).map(|c| [c[0], c[1]]).collect())
            }
            Type::SRational => {
                let v = arr!(i32, 4);
                Self::SRational(v.chunks_exact(2).map(|c| [c[0], c[1]]).collect())
            }
        }
    }
}
