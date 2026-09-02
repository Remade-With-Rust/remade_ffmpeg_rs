use alloc::boxed::Box;
use alloc::fmt;
use alloc::string::String;
use core::error::Error as CoreError;
use core::result;
#[cfg(feature = "std")]
use std::io::Error as IoError;

use crate::decode::ColorTransform;

pub type Result<T> = result::Result<T, Error>;

/// An enumeration over JPEG features (currently) unsupported by this library.
///
/// Support for features listed here may be included in future versions of this library.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnsupportedFeature {
    /// Hierarchical JPEG.
    Hierarchical,
    /// JPEG using arithmetic entropy coding instead of Huffman coding.
    ArithmeticEntropyCoding,
    /// Sample precision in bits. 8 bit sample precision is what is currently supported in non-lossless coding process.
    SamplePrecision(u8),
    /// Number of components in an image. 1, 3 and 4 components are currently supported.
    ComponentCount(u8),
    /// An image can specify a zero height in the frame header and use the DNL (Define Number of
    /// Lines) marker at the end of the first scan to define the number of lines in the frame.
    DNL,
    /// Subsampling ratio.
    SubsamplingRatio,
    /// A subsampling ratio not representable as an integer.
    NonIntegerSubsamplingRatio,
    /// Colour transform
    ColorTransform(ColorTransform),
}

/// Errors that can occur while decoding a JPEG image.
#[derive(Debug)]
pub enum Error {
    /// The image is not formatted properly. The string contains detailed information about the
    /// error.
    Format(String),
    /// The image makes use of a JPEG feature not (currently) supported by this library.
    Unsupported(UnsupportedFeature),
    /// An I/O error occurred while decoding the image.
    #[cfg(feature = "std")]
    Io(IoError),
    /// The data ended before the image did. Raised in place of an
    /// `Io(UnexpectedEof)` on every path, with or without `std`, so a
    /// receiver can tell a truncated frame from a corrupt one.
    UnexpectedEof,
    /// An internal error occurred while decoding the image.
    Internal(Box<dyn CoreError + Send + Sync + 'static>), //TODO: not used, can be removed with the next version bump
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Format(ref desc) => write!(f, "invalid JPEG format: {}", desc),
            Error::Unsupported(ref feat) => write!(f, "unsupported JPEG feature: {:?}", feat),
            #[cfg(feature = "std")]
            Error::Io(ref err) => err.fmt(f),
            Error::UnexpectedEof => write!(f, "unexpected end of JPEG data"),
            Error::Internal(ref err) => err.fmt(f),
        }
    }
}

impl CoreError for Error {
    fn source(&self) -> Option<&(dyn CoreError + 'static)> {
        match *self {
            #[cfg(feature = "std")]
            Error::Io(ref err) => Some(err),
            Error::Internal(ref err) => Some(&**err),
            _ => None,
        }
    }
}

#[cfg(feature = "std")]
impl From<IoError> for Error {
    fn from(err: IoError) -> Error {
        Error::Io(err)
    }
}
