# rusty_svgtree

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/crates/rusty_svgtree/LICENSE)
[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)

**A pure-Rust SVG parser and renderer.** `Tree::from_data` parses SVG into a
simplified render tree (the shape of usvg's: styles resolved, lengths in user
units, `use` expanded, paint servers in user space); `render` draws it with
the house [`rusty_raster`](https://crates.io/crates/rusty_raster) rasteriser. XML comes from the
house [`rusty_xml`](https://crates.io/crates/rusty_xml) parser, embedded PNG,
JPEG, GIF and WebP from the house codecs, embedded SVG and svgz from this crate. `#![forbid(unsafe_code)]`, no C.

Part of **[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)**
by **[Mata Network](https://www.mata.network/)**.


## Against resvg and nanosvg (C), side by side

`../rusty_raster/bench-c/` renders an SVG corpus with this crate, resvg 0.48 and nanosvg. The corpus is 105 files: 34 feature tests and 71 real SVGs from a Windows Program Files tree, each fitted to a 1024 px box.

- **Pixels:**
  - 98 of the 105 files are identical to resvg; the other 7 are 36-55 dB.
  - 7 files use `<filter>` or `<text>`, which were not implemented when this was measured.
  - 1 file differs only by JPEG decoder rounding.
- **nanosvg:** its median PSNR against both this crate and resvg is 27 dB. It has no CSS, clip paths, masks or `preserveAspectRatio`, so it serves only as a speed reference.
- **Speed:** measured over the 98 files where this crate and resvg produce identical pixels, so both do the same work. Parse and render the whole set, minimum of 11 pinned, ABBA-interleaved rounds.

| | rusty_svgtree | resvg 0.48 (Rust) | nanosvg (C) |
|---|---:|---:|---:|
| time | 992 ms | **518 ms** | **481 ms** |
| relative to rusty_svgtree | 1.00x | 1.91x faster | 2.06x faster |

On 0.1.0 this crate was about half the speed of both. Part of the gap was `rusty_raster`'s scalar lanes, which ran 0.70-0.95x of tiny-skia. Over the full 105 files the totals look reversed (resvg spends seconds on filters this crate skips), so that comparison is not reported as a speed result. This table was measured before the speed pass below and has not been re-run.

### Speed against resvg (after 0.1.0)

A 15-commit instruction-count pass on this crate ([PR #170](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/170)), measured on top of the `rusty_raster` speed pass ([PR #166](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/166)). Output is byte-identical to 0.1.0 on 1,255 real SVGs and on all 1,695 files of the resvg v0.45.1 test suite. Ratios are resvg's time divided by this crate's; above 1 means this crate is faster.

| workload | 0.1.0 | now | resvg 0.48.1 | now vs resvg | now vs 0.1.0 |
|---|---:|---:|---:|---:|---:|
| **400 UI icons at 48 px** | 23.4 ms | 20.4 ms | 20.6 ms | **0.996 (level)** | 1.150 |
| 71 illustrations at 512 px | 187 ms | 132 ms | 139 ms | 1.038 | 1.450 |
| 13 large documents (0.4-1.2 MB) at 512 px | 344 ms | 244 ms | 271 ms | 1.126 | 1.398 |

On 0.1.0 these rows ran at 0.72-0.87x of resvg. Parsing alone (`Tree::from_data`) is now 1.057x / 1.002x / 1.294x of usvg 0.48.1 on the same three sets (0.1.0: 0.855 / 0.741 / 0.750). **Memory is still higher than resvg's on extreme paths:** a path with a million segments peaks at 34 MB, down from 131 MB in 0.1.0, against resvg's 24 MB. Method: an independent run by a downstream team ([PR #170 comment](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/170#issuecomment-7059394686)), PR head `dbe48b7` with `rusty_raster` at `3e747a1`, one process per arm per round pinned to one P-core at High priority, 20 rounds in alternating order, the same allocator in every arm, null arm 1.005.

## What is verified

**Oracle: resvg 0.48.1** (dev-dependency; numbers measured with this crate on
`rusty_raster` and resvg on tiny-skia 0.12). Every SVG is rendered by both stacks at the same
size and the premultiplied RGBA8 buffers are compared byte for byte
(`cargo test -p rusty_svgtree --release --test oracle -- --nocapture`).

| corpus | renders | pixel-identical | the rest |
|---|---:|---:|---|
| feature corpus, 34 files (`tests/corpus/`) at scales 0.66, 1, 2.5 | 102 | **99** | 3 = the JPEG file: max channel diff 23, 13% of pixels, PSNR 55.37 dB min. rusty_jpeg and resvg's zune-jpeg round the IDCT/upsampling differently. |
| 220 real-world SVGs shipped in crates.io packages in the local cargo registry (Inkscape, Illustrator, diagrams.net, plotters, docs diagrams), fit to 800 px (`RSVG_EXTRA_DIR=<dir>`) | 220 | **216** | 4 = one logo (four copies). It uses a gradient whose `gradientTransform` is inherited through `href`. The spec says to inherit it, and we do. usvg 0.48.1 drops it: `convert_linear` reads the attribute with `resolve_transform`, which looks only at the node itself (PSNR 25.29 dB). |

The feature corpus covers every basic shape, all path commands (arcs
converted exactly as resvg's kurbo does), fill rules, every join and cap,
miter limits, dashes, nested transforms, `viewBox` meet/slice, absolute and
relative units, `<style>` sheets (type, class, id, attribute, child,
descendant, `:first-child`, `!important`, `@media` skipped), `style`
attributes, `inherit`, `currentColor`, opacity at every level, linear and
radial gradients (units, spread, `href` inheritance, degenerate stops),
`use`/`symbol`/nested `svg`, clip paths (nested, `objectBoundingBox`,
`clip-rule`), luminance and alpha masks, `mix-blend-mode`, embedded PNG and
JPEG, user-space and `objectBoundingBox` patterns, markers (orient, units,
`viewBox`, `context-stroke`), `switch`/`systemLanguage`, `display`,
`visibility`, `shape-rendering`, `paint-order`, CSS colour syntaxes and an
Illustrator export with internal entities.

**Hostile input: 25 tests** (`tests/hostile.rs`), each must error or clamp,
never panic or allocate without bound: self, mutual and 3-cycle `use`, a
2000-deep `use` chain, exponential `use` amplification (10^9 nodes requested;
`ElementsLimitReached` after the node cap), gradient `href` cycles, 1/2/3-step
clip-path cycles, mask cycles, pattern self-reference, giant `width`/`height`,
giant and tiny `viewBox`, 100k nested elements, 200 nested opacity layers
under a 4 MiB layer budget, malformed XML, malformed numbers/paths/transforms,
an XML entity bomb, an external entity, a PNG header claiming 100000 x 100000
pixels, a 10^8-unit pattern tile, a 141,421-unit line dashed every 0.001
units, a pathological style
sheet, CSS descendant-combinator backtracking (over 60 s with the matching
budget removed, 0.21 s with it), one `objectBoundingBox` clip path shared by
20,000 elements (over 60 s with the conversion budget removed, 2.93 s with
it), coordinates that overflow inside the unwrapping rect and size helpers
inherited from tiny-skia-path, and 2,040 deterministic byte-mutations of the
feature corpus. During development, 8,480 more mutations of the real-world
files were run once against tiny-skia; none panicked. The tiny-`viewBox`
stroke case panics resvg 0.48.1 itself (tiny-skia `blit_anti_h` index out of
range); on `rusty_raster`, which clips every span, it renders, and all 25
tests pass.

No speed numbers are claimed yet.

## Filters

Every SVG 1.1 filter primitive plus `feDropShadow` and the CSS filter
functions (`blur()`, `drop-shadow()`, `grayscale()`, ...), with filter and
primitive regions, `filterUnits` / `primitiveUnits`,
`color-interpolation-filters`, `href` inheritance and filter lists. The
filters are in the tree (`Group::filters`, types in the `filter` module) and
`render` applies them. Pixel-identical to resvg 0.48.1 on the resvg test
suite's `filters` files (the 7 that differ there draw text labels).

Two things are deliberately not as in resvg: `feDisplacementMap` applies
`scale` once and reads the map unpremultiplied (resvg applies it twice), and a
huge `feMorphology` radius costs no more than a small one.
`BackgroundImage`, `BackgroundAlpha`, `FillPaint` and `StrokePaint` inputs
read as `SourceGraphic`, as in resvg.

## Text

Text is always in the tree: `Node::Text` keeps the chunks, spans, fonts,
paints and positions. To draw it, turn on the `text` feature and give
`Options::fonts` a `text::FontProvider`, a trait with five methods (select a
font, a fallback, metrics, shape a run, draw a glyph). The layout on top of it
(bidi, fallback, `textPath`, `textLength`, spacing, baselines, decorations,
vertical writing, SVG, bitmap and `COLR` glyphs) is this crate's, and the
result is `Text::flattened`, ordinary paths. Documents cannot load fonts.

| feature | default | adds |
|---|---|---|
| (none) | yes | text nodes in the tree, not drawn (`Tree::skipped().text` counts them) |
| `text` | no | layout through a font callback: `kurbo`, `unicode-bidi`, `unicode-script`, `unicode-vo` |
| `fonts` | no | `Fonts`, a provider over font files: `fontdb`, `harfrust`, `skrifa` |

There is no house font stack yet
([thoth#6](https://github.com/Remade-With-Rust/thoth/issues/6)), which is why
the third-party one is behind a feature that is never on by default.

With `fonts` and the same font files as resvg 0.48.1, all 379 files of the
resvg suite's `text` category are pixel-identical, and so is every other file
of the suite except four (JPEG rounding, at most 3 levels).

```rust,ignore
let mut fonts = rusty_svgtree::Fonts::new();
fonts.load_system_fonts();
let opt = rusty_svgtree::Options {
    fonts: Some(std::sync::Arc::new(fonts)),
    ..Default::default()
};
```

## Drawing the tree elsewhere (a PDF, a vector surface)

The tree is a public, read-only view of the resolved document (after CSS,
`use` expansion and unit resolution), the stage a usvg tree gives krilla-svg.
A writer walks `Tree::root()`: groups (transform, opacity, blend mode,
isolation, clip path, mask, filters), paths (`Path::data().segments()` in
user space, fill, stroke), paints (colour, linear and radial gradients and
patterns, all already in user space: `objectBoundingBox` is folded into their
transforms), images (encoded bytes, so JPEG passes through; `ImageKind::Svg`
is a nested `Tree`) and text. Structs have accessor methods; `Node`,
`ImageKind`, `filter::Kind` and the new types are `#[non_exhaustive]`.

| what | needs | for |
|---|---|---|
| `Text::glyph_runs()`: shaped glyphs (font id, glyph id, placement, the source text of the cluster) and the decoration paths | `text` | text kept as text, with `ToUnicode` |
| `FontProvider::font_data()`, `FontData`, `Tree::fonts()` | `text` | embedding the fonts: a provided method, `None` unless the provider gives the file |
| `Fonts`' `font_data` (the file's bytes, read once, shared) | `fonts` | |
| `render_node(&Tree, &Node, Transform, &mut PixmapMut)` | none | rasterising what a writer cannot express (a filtered group), as `resvg::render_node` |
| `Image::decode(max_pixels)` | `raster-images` (default) | premultiplied RGBA8 pixels; `None` without the feature |

With `default-features = false, features = ["text"]` and an application's
own `FontProvider` in `Options::fonts`, glyph runs and `font_data` work
without `fontdb` or a system-font scan. A provider's `FontId` is the key to
de-duplicate fonts on: the same provider and id are the same face in every
tree parsed with it (nested `ImageKind::Svg` trees included). A run's
`variations` (plus `opsz` from `optical_size`) are the axis values its glyphs
were shaped and drawn with.

## Writing SVG

`Tree::to_svg_string` writes the tree back with every style resolved. All 1718
files of the resvg suite that parse draw the same pixels after a round trip,
with text as paths; with `WriteOptions { preserve_text: true }` so do the 379
text files.

## Usage

```rust
use rusty_raster::{Pixmap, Transform};

let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
    <circle cx="10" cy="10" r="8" fill="tomato"/></svg>"#;
let tree = rusty_svgtree::Tree::from_data(svg, &rusty_svgtree::Options::default())?;
let size = tree.size().to_int_size();
let mut pixmap = Pixmap::new(size.width(), size.height()).unwrap();
rusty_svgtree::render(&tree, Transform::identity(), &mut pixmap.as_mut());
```

`examples/svg2png.rs` is a command-line renderer:
`cargo run -p rusty_svgtree --release --example svg2png -- in.svg out.png 2`.

## Limits

All in `Options`, all with safe defaults: `max_nodes` (500,000 elements after
`use` expansion), `max_depth` (1024), `max_image_pixels` (64 Mi),
`max_canvas_pixels` (none: a document's size allocates nothing, and `render`
draws at the size of the caller's pixmap), `max_svgz_bytes` (256 MiB
decompressed), `max_layer_bytes` (1 GiB held at once by group layers and
clip/mask/filter scratch; a group past it is skipped) and `max_filter_work`
(2^33 pixel operations per `render`; an element whose filter would exceed it
is not drawn). Fixed internal work budgets
bound style-sheet matching, `use` cycle checks and the re-conversion of shared
clip paths, patterns and markers. `use` reference cycles are
skipped (resvg instead fails the whole document); clip-path, mask and pattern
cycles are broken. Embedded documents share the limits of the one around them
and nest 8 deep. Only `data:` image URIs are loaded: no file or network
access. `Tree::skipped` says what was left out.

## Not done yet

- No external `href`s (by design), no animation.
- A house font stack: see Text.

## License

Apache-2.0.
