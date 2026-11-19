//! Pure-Rust ICC colour management.
//!
//! - Parse ICC v2 and v4 profiles ([`Profile::new_from_slice`]): matrix/TRC
//!   RGB, grey TRC, XYZ and L*a*b* PCS, `lut8`/`lut16`, v4 `mAB `/`mBA `,
//!   `A2B0..2`/`B2A0..2` selected by intent, `wtpt`/`chad`, `desc`/`mluc`.
//! - Transform `u8`, `u16` and `f32` pixels between RGB, CMYK, grey and
//!   L*a*b*, with or without alpha ([`Transform`]), with the four rendering
//!   intents and `LittleCMS`-style black-point compensation.
//! - Soft-proof ([`Transform::new_proofing`]) and gamut-check
//!   ([`GamutCheck`]) against a proofing profile.
//! - Write profiles ([`Profile::encode`]), including built-in sRGB, Display
//!   P3, Rec. 2020, custom primaries/curve RGB, grey and L*a*b* identity.
//!
//! ```
//! use rusty_icc::{Intent, Layout, Options, Profile, Transform};
//!
//! let srgb = Profile::new_srgb();
//! let p3 = Profile::new_from_slice(&Profile::new_display_p3().encode())?;
//! let t = Transform::new(
//!     &srgb,
//!     Layout::Rgba,
//!     &p3,
//!     Layout::Rgba,
//!     Intent::RelativeColorimetric,
//!     Options::default(),
//! )?;
//! let src = [255u8, 0, 0, 128];
//! let mut dst = [0u8; 4];
//! t.transform(&src, &mut dst)?;
//! assert_eq!(dst[3], 128); // alpha passes through
//! assert!(dst[0] < 255 && dst[1] > 0); // sRGB red sits inside P3
//! # Ok::<(), rusty_icc::Error>(())
//! ```
//!
//! The library forbids `unsafe`, has no dependencies, and never declares a
//! global allocator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bytes;
pub mod color;
mod curve;
mod error;
mod lut;
mod md5;
mod pipeline;
mod profile;
mod tag;
mod transform;

pub use curve::Curve;
pub use error::{Error, Result};
pub use lut::{AbLut, Clut, LegacyLut, Lut};
pub use profile::{ColorSpace, Header, Profile, ProfileClass, TagSignature};
pub use tag::{LocalizedString, Tag};
#[doc(hidden)]
pub use transform::SampleKind;
pub use transform::{GamutCheck, Intent, Layout, Options, Sample, Transform};
