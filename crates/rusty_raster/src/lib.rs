//! `rusty_raster` is a pure-Rust 2D rasteriser with a
//! [tiny-skia](https://github.com/linebender/tiny-skia)-shaped API: anti-aliased
//! fill and stroke (every join and cap, dashes, hairlines), even-odd and
//! non-zero fill rules, linear, two-point-conical and sweep gradients, Porter-Duff
//! and SVG blend modes, alpha and luminance masks, and pixmap drawing with
//! nearest, bilinear and bicubic sampling.
//!
//! It is a hardened fork of tiny-skia 0.12.0 and tiny-skia-path 0.12.0, which
//! are themselves a port of a subset of [Skia](https://skia.org/). The code keeps
//! their BSD-3-Clause notices (see `LICENSE-BSD-3-Clause` and `NOTICE.md`). What
//! the fork changes:
//!
//! - `#![forbid(unsafe_code)]` and no dependencies: the SIMD lane types are
//!   scalar arrays that reproduce tiny-skia's SSE2 lane semantics, and
//!   `bytemuck`, `arrayref`, `arrayvec`, `strict-num`, `log` and `png` are gone
//!   (PNG goes through the house `rusty_png` behind the default `png-format`
//!   feature).
//! - Hostile geometry is rejected or clamped instead of panicking: NaN and
//!   infinite coordinates, huge paths and transforms, zero-size targets.
//! - `PixmapMut::from_bytes` works on any borrowed `&mut [u8]` of the right
//!   length.
//!
//! ```
//! use rusty_raster::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};
//!
//! let mut pixmap = Pixmap::new(64, 64).unwrap();
//! let mut paint = Paint::default();
//! paint.set_color(Color::from_rgba8(50, 127, 150, 200));
//! paint.anti_alias = true;
//! let path = PathBuilder::from_circle(32.0, 32.0, 20.0).unwrap();
//! pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
//! assert_ne!(pixmap.data()[(32 * 64 + 32) * 4 + 3], 0);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_copy_implementations)]
#![warn(missing_debug_implementations)]
#![allow(clippy::approx_constant)]
#![allow(clippy::clone_on_copy)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::comparison_chain)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::wrong_self_convention)]
#![allow(clippy::eq_op)]
#![allow(clippy::neg_cmp_op_on_partial_ord)]
#![allow(clippy::upper_case_acronyms)]

extern crate alloc;

mod alpha_runs;
mod arrayvec;
mod blend_mode;
mod blitter;
mod color;
mod edge;
mod edge_builder;
mod edge_clipper;
mod fixed_point;
mod geom;
mod line_clipper;
mod mask;
mod math;
mod path64;
mod path_geometry;
mod pipeline;
mod pixmap;
mod scan;
mod shaders;
mod wide;

mod painter;
pub mod path; // Keep it under `pixmap` for a better order in the docs.

pub use blend_mode::BlendMode;
pub use color::{Color, ColorSpace, ColorU8, PremultipliedColor, PremultipliedColorU8};
pub use color::{ALPHA_OPAQUE, ALPHA_TRANSPARENT, ALPHA_U8_OPAQUE, ALPHA_U8_TRANSPARENT};
pub use mask::{Mask, MaskType};
pub use painter::{FillRule, Paint};
pub use pixmap::{Pixmap, PixmapMut, PixmapRef, BYTES_PER_PIXEL};
pub use shaders::{FilterQuality, GradientStop, PixmapPaint, SpreadMode};
pub use shaders::{LinearGradient, Pattern, RadialGradient, Shader, SweepGradient};

pub use crate::path::{IntRect, IntSize, NonZeroRect, Point, Rect, Size, Transform};
pub use crate::path::{LineCap, LineJoin, Stroke, StrokeDash};
pub use crate::path::{Path, PathBuilder, PathSegment, PathSegmentsIter, PathStroker};

/// An integer length that is guarantee to be > 0
type LengthU32 = core::num::NonZeroU32;
