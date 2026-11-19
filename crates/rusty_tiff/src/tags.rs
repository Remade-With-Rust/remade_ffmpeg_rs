//! Tag numbers, field types and the enumerated values the decoder interprets.
//!
//! Any tag number can be read: those without a name here come through as
//! [`Tag::Unknown`], so DNG, GeoTIFF or private tags are just tags.

macro_rules! tags {
    ($( $(#[$m:meta])* $name:ident = $val:expr, )*) => {
        /// A TIFF tag number. Named variants cover baseline TIFF 6, the common
        /// extensions, EXIF/GPS pointers, GeoTIFF and the DNG tags; every other
        /// number is [`Tag::Unknown`].
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum Tag {
            $( $(#[$m])* #[allow(missing_docs)] $name, )*
            /// Any tag number without a named variant.
            Unknown(u16),
        }

        impl Tag {
            /// The tag for a raw tag number.
            #[must_use]
            pub fn from_u16(n: u16) -> Self {
                match n {
                    $( $val => Tag::$name, )*
                    other => Tag::Unknown(other),
                }
            }

            /// The raw tag number.
            #[must_use]
            pub fn to_u16(self) -> u16 {
                match self {
                    $( Tag::$name => $val, )*
                    Tag::Unknown(n) => n,
                }
            }
        }
    };
}

tags! {
    NewSubfileType = 254,
    SubfileType = 255,
    ImageWidth = 256,
    ImageLength = 257,
    BitsPerSample = 258,
    Compression = 259,
    PhotometricInterpretation = 262,
    Threshholding = 263,
    FillOrder = 266,
    DocumentName = 269,
    ImageDescription = 270,
    Make = 271,
    Model = 272,
    StripOffsets = 273,
    Orientation = 274,
    SamplesPerPixel = 277,
    RowsPerStrip = 278,
    StripByteCounts = 279,
    MinSampleValue = 280,
    MaxSampleValue = 281,
    XResolution = 282,
    YResolution = 283,
    PlanarConfiguration = 284,
    PageName = 285,
    XPosition = 286,
    YPosition = 287,
    T4Options = 292,
    T6Options = 293,
    ResolutionUnit = 296,
    PageNumber = 297,
    TransferFunction = 301,
    Software = 305,
    DateTime = 306,
    Artist = 315,
    HostComputer = 316,
    Predictor = 317,
    WhitePoint = 318,
    PrimaryChromaticities = 319,
    ColorMap = 320,
    TileWidth = 322,
    TileLength = 323,
    TileOffsets = 324,
    TileByteCounts = 325,
    SubIfds = 330,
    InkSet = 332,
    ExtraSamples = 338,
    SampleFormat = 339,
    SMinSampleValue = 340,
    SMaxSampleValue = 341,
    JpegTables = 347,
    YCbCrCoefficients = 529,
    YCbCrSubSampling = 530,
    YCbCrPositioning = 531,
    ReferenceBlackWhite = 532,
    Xmp = 700,
    CfaRepeatPatternDim = 33421,
    CfaPattern = 33422,
    Copyright = 33432,
    ModelPixelScale = 33550,
    ModelTiepoint = 33922,
    ModelTransformation = 34264,
    Photoshop = 34377,
    ExifIfd = 34665,
    IccProfile = 34675,
    GeoKeyDirectory = 34735,
    GeoDoubleParams = 34736,
    GeoAsciiParams = 34737,
    GpsIfd = 34853,
    InteropIfd = 40965,
    GdalMetadata = 42112,
    GdalNodata = 42113,
    DngVersion = 50706,
    DngBackwardVersion = 50707,
    UniqueCameraModel = 50708,
    LocalizedCameraModel = 50709,
    CfaPlaneColor = 50710,
    CfaLayout = 50711,
    LinearizationTable = 50712,
    BlackLevelRepeatDim = 50713,
    BlackLevel = 50714,
    BlackLevelDeltaH = 50715,
    BlackLevelDeltaV = 50716,
    WhiteLevel = 50717,
    DefaultScale = 50718,
    DefaultCropOrigin = 50719,
    DefaultCropSize = 50720,
    ColorMatrix1 = 50721,
    ColorMatrix2 = 50722,
    CameraCalibration1 = 50723,
    CameraCalibration2 = 50724,
    ReductionMatrix1 = 50725,
    ReductionMatrix2 = 50726,
    AnalogBalance = 50727,
    AsShotNeutral = 50728,
    AsShotWhiteXy = 50729,
    BaselineExposure = 50730,
    BaselineNoise = 50731,
    BaselineSharpness = 50732,
    BayerGreenSplit = 50733,
    LinearResponseLimit = 50734,
    CameraSerialNumber = 50735,
    LensInfo = 50736,
    ChromaBlurRadius = 50737,
    AntiAliasStrength = 50738,
    DngPrivateData = 50740,
    CalibrationIlluminant1 = 50778,
    CalibrationIlluminant2 = 50779,
    BestQualityScale = 50780,
    ActiveArea = 50829,
    MaskedAreas = 50830,
    ForwardMatrix1 = 50964,
    ForwardMatrix2 = 50965,
    OpcodeList1 = 51008,
    OpcodeList2 = 51009,
    OpcodeList3 = 51022,
    NoiseProfile = 51041,
}

/// The field type of an IFD entry (TIFF 6 plus the BigTIFF 8-byte types).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum Type {
    Byte = 1,
    Ascii = 2,
    Short = 3,
    Long = 4,
    Rational = 5,
    SByte = 6,
    Undefined = 7,
    SShort = 8,
    SLong = 9,
    SRational = 10,
    Float = 11,
    Double = 12,
    Ifd = 13,
    Long8 = 16,
    SLong8 = 17,
    Ifd8 = 18,
}

impl Type {
    /// The type for a raw type code, or `None` for a code TIFF does not define.
    #[must_use]
    pub fn from_u16(n: u16) -> Option<Self> {
        Some(match n {
            1 => Self::Byte,
            2 => Self::Ascii,
            3 => Self::Short,
            4 => Self::Long,
            5 => Self::Rational,
            6 => Self::SByte,
            7 => Self::Undefined,
            8 => Self::SShort,
            9 => Self::SLong,
            10 => Self::SRational,
            11 => Self::Float,
            12 => Self::Double,
            13 => Self::Ifd,
            16 => Self::Long8,
            17 => Self::SLong8,
            18 => Self::Ifd8,
            _ => return None,
        })
    }

    /// Size in bytes of one element of this type.
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            Self::Byte | Self::Ascii | Self::SByte | Self::Undefined => 1,
            Self::Short | Self::SShort => 2,
            Self::Long | Self::SLong | Self::Float | Self::Ifd => 4,
            Self::Rational
            | Self::SRational
            | Self::Double
            | Self::Long8
            | Self::SLong8
            | Self::Ifd8 => 8,
        }
    }
}

/// Values of the `Compression` tag (259).
pub mod compression {
    /// No compression.
    pub const NONE: u16 = 1;
    /// CCITT modified Huffman run-length encoding (1-D, no EOLs, byte-aligned rows).
    pub const CCITT_RLE: u16 = 2;
    /// CCITT T.4 (Group 3) fax, 1-D or 2-D per `T4Options`.
    pub const CCITT_FAX3: u16 = 3;
    /// CCITT T.6 (Group 4) fax.
    pub const CCITT_FAX4: u16 = 4;
    /// Lempel-Ziv-Welch.
    pub const LZW: u16 = 5;
    /// Old-style (TIFF 6 section 22) JPEG. Not decoded.
    pub const OLD_JPEG: u16 = 6;
    /// New-style JPEG (TIFF Technical Note 2), shared tables in `JPEGTables`.
    pub const JPEG: u16 = 7;
    /// Adobe Deflate (zlib).
    pub const DEFLATE: u16 = 8;
    /// Macintosh PackBits.
    pub const PACKBITS: u16 = 32773;
    /// The pre-standard Deflate code; same zlib stream as [`DEFLATE`].
    pub const OLD_DEFLATE: u16 = 32946;
}

/// Values of the `PhotometricInterpretation` tag (262).
pub mod photometric {
    /// Greyscale, 0 is white.
    pub const WHITE_IS_ZERO: u16 = 0;
    /// Greyscale, 0 is black.
    pub const BLACK_IS_ZERO: u16 = 1;
    /// RGB.
    pub const RGB: u16 = 2;
    /// Palette colour through `ColorMap`.
    pub const PALETTE: u16 = 3;
    /// Transparency mask.
    pub const TRANSPARENCY_MASK: u16 = 4;
    /// Separated, usually CMYK.
    pub const SEPARATED: u16 = 5;
    /// YCbCr.
    pub const YCBCR: u16 = 6;
    /// CIE L*a*b*.
    pub const CIELAB: u16 = 8;
    /// ICC L*a*b*.
    pub const ICCLAB: u16 = 9;
    /// ITU L*a*b*.
    pub const ITULAB: u16 = 10;
    /// DNG colour filter array.
    pub const CFA: u16 = 32803;
    /// DNG linear raw.
    pub const LINEAR_RAW: u16 = 34892;
}

/// Values of the `Predictor` tag (317).
pub mod predictor {
    /// No prediction.
    pub const NONE: u16 = 1;
    /// Horizontal differencing.
    pub const HORIZONTAL: u16 = 2;
    /// Floating-point byte-plane differencing.
    pub const FLOATING_POINT: u16 = 3;
}

/// Values of the `SampleFormat` tag (339).
pub mod sample_format {
    /// Unsigned integer.
    pub const UINT: u16 = 1;
    /// Two's-complement signed integer.
    pub const INT: u16 = 2;
    /// IEEE floating point.
    pub const IEEEFP: u16 = 3;
    /// Undefined; decoded as unsigned.
    pub const VOID: u16 = 4;
}

/// Values of the `ExtraSamples` tag (338).
pub mod extra_samples {
    /// Unspecified data.
    pub const UNSPECIFIED: u16 = 0;
    /// Associated (premultiplied) alpha.
    pub const ASSOCIATED_ALPHA: u16 = 1;
    /// Unassociated (straight) alpha.
    pub const UNASSOCIATED_ALPHA: u16 = 2;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trip() {
        for n in 0..=u16::MAX {
            assert_eq!(Tag::from_u16(n).to_u16(), n);
        }
        assert_eq!(Tag::from_u16(34675), Tag::IccProfile);
        assert_eq!(Tag::from_u16(65000), Tag::Unknown(65000));
    }
}
