# NOTICE: attribution for `rusty_svgtree`

The crate is Apache-2.0 (`LICENSE`). Parts of it are derived from the
[resvg project](https://github.com/linebender/resvg) (resvg and usvg 0.48.1,
Copyright the Resvg Authors), which is licensed `Apache-2.0 OR MIT`; it is used
here under Apache-2.0. This crate is not endorsed by or affiliated with the
resvg project.

| here | derived from | what was changed |
|---|---|---|
| `src/filter.rs` | usvg `tree/filter.rs` | plain data types, no `strict-num` |
| `src/filter_convert.rs` | usvg `parser/filter.rs` | this crate's document; cycles broken, numbers clamped, work bounded |
| `src/filter_render.rs` | resvg `filter/mod.rs` | results dropped after their last use, dead primitives skipped, memory and work budgets |
| `src/filter_kernels.rs` | resvg `filter/*.rs` | same output, other algorithms (see the module docs) |
| `src/text.rs` | usvg `tree/text.rs` | plain data types, a font provider trait |
| `src/text_convert.rs` | usvg `parser/text.rs`, `parser/svgtree/text.rs` (the latter in `src/svgtree.rs`) | this crate's document; linear position lookup, bounded |
| `src/text_layout.rs` | usvg `text/layout.rs`, `text/flatten.rs` | fonts through the trait; indexed lookups; bounded |
| `src/fonts.rs`, `src/colr.rs` | usvg `text/mod.rs`, `text/flatten.rs`, `text/colr.rs` | behind the trait; own SVG output; bounded |

Attributions resvg's filter code carries, kept for what descends from it:

- The recursive Gaussian blur follows resvg's `iir_blur.rs`, which is based on
  <http://www.getreuer.info/home/gaussianiir> ("Simplified BSD License"), by
  Pascal Getreuer; the method is Alvarez and Mazorra, "Signal and Image
  Restoration using Shock Filters and Anisotropic Diffusion", SIAM J. on
  Numerical Analysis 31 (2), 1994.
- The box blur's widths follow resvg's `box_blur.rs`, which is based on
  <https://github.com/fschutt/fastblur> (MIT).
- The sRGB / linear RGB tables follow an idea of librsvg's; the values are the
  rounded sRGB transfer functions.
- The turbulence noise is the algorithm of the `feTurbulence` reference code
  in the SVG specification (W3C).
