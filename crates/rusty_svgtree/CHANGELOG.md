# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.2.1...rusty_svgtree-v0.3.0) - 2026-11-28

### Added

- *(rusty_svgtree)* tree view for vector writers (glyph runs, font data, render_node, Image::decode) ([#256](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/256))

## [0.2.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.2.0...rusty_svgtree-v0.2.1) - 2026-11-28

### Other

- update Cargo.toml dependencies

## [0.2.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.7...rusty_svgtree-v0.2.0) - 2026-11-25

### Added

- *(rusty_svgtree)* text, a usvg-style tree and Tree::to_svg_string
- *(rusty_svgtree)* GIF, WebP and nested SVG images, svgz input, gradient and pattern context paints, `Tree::skipped()`
- *(rusty_svgtree)* SVG filters, pixel-identical to resvg on its test suite

### Changed (breaking)

- `Options` gains fields (`fonts`, `font_family`, `max_svgz_bytes`, `max_filter_work`) and is no longer `UnwindSafe`; build it with `..Default::default()`
- `Node`, `ImageKind` and `Error` are `#[non_exhaustive]`: matches need a `_` arm
- defaults: `max_depth` is 1024 and `max_canvas_pixels` is unlimited

### Other

- Merge branch 'feat/svg-filters-text' into feat/svg-images-fixes
- *(rusty_svgtree)* shorter filter chain in debug builds

## [0.1.7](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.6...rusty_svgtree-v0.1.7) - 2026-11-23

### Other

- bump rusty_alloc-api to 2.6.0 ([#204](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/204))

## [0.1.6](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.5...rusty_svgtree-v0.1.6) - 2026-11-22

### Other

- updated the following local packages: rusty_raster

## [0.1.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.4...rusty_svgtree-v0.1.5) - 2026-11-22

### Other

- update Cargo.toml dependencies

## [0.1.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.3...rusty_svgtree-v0.1.4) - 2026-11-22

### Other

- updated the following local packages: rusty_raster

## [0.1.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.2...rusty_svgtree-v0.1.3) - 2026-11-21

### Other

- updated the following local packages: rusty_raster

## [0.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.1...rusty_svgtree-v0.1.2) - 2026-11-21

### Other

- Merge pull request #177 from Remade-With-Rust/perf/loss-rows-svgtree
- *(rusty_svgtree)* draw a one-path clip straight into a coverage mask (vein: clip paths drawn through an RGBA scratch layer, win 2)
- *(rusty_svgtree)* apply a pixel-aligned rectangle clip without a mask (vein: clip paths drawn through an RGBA scratch layer, win 1)
- *(rusty_svgtree)* build a huge path at about its final size (vein: copies of a huge path while it is parsed, win 2)
- *(rusty_svgtree)* borrow element ids in the id map
- *(rusty_svgtree)* keep the stroke-bounding-box stroker across paths
- *(rusty_svgtree)* build shapes in one reused builder, copied out at their size
- *(rusty_svgtree)* borrow attribute values from the XML DOM (vein: copies of a huge path while it is parsed, win 1)
- *(rusty_svgtree)* svgveins `both` mode, the whole per-document cost

## [0.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_svgtree-v0.1.0...rusty_svgtree-v0.1.1) - 2026-11-21

### Other

- *(rusty_svgtree)* absolute README links so they work on crates.io
- speed numbers in five image crate READMEs; rusty_tiff needs flate2 1.1.11
- *(rusty_svgtree)* convert decoded images in one pass (vein: render layers, clips and image glue, win 3)
- *(rusty_svgtree)* quick `trim` for text with nothing to trim (vein: attribute storage and lookup, win 3)
- *(rusty_svgtree)* composite only the part of a layer that holds anything (vein: render layers, clips and image glue, win 2)
- *(rusty_svgtree)* read the XML attributes of an element in one pass (vein: tree building and conversion, win 3)
- *(rusty_svgtree)* the build passes allocate once (vein: tree building and conversion, win 2)
- *(rusty_svgtree)* convert an unrequired group straight into its parent (vein: tree building and conversion, win 1)
- *(rusty_svgtree)* scan number digits over a local slice (vein: number and path-data parsing, win 3)
- *(rusty_svgtree)* per-element attribute presence sets (vein: attribute storage and lookup, win 2)
- *(rusty_svgtree)* intern attribute names (vein: attribute storage and lookup, win 1)
- *(rusty_svgtree)* decode base64 image data four digits at a time (vein: render layers, clips and image glue, win 1)
- *(rusty_svgtree)* stream `style` attribute declarations (vein: CSS matching, win 3)
- *(rusty_svgtree)* apply style-sheet declarations without cloning them (vein: CSS matching, win 2)
- *(rusty_svgtree)* index style-sheet rules by subject (vein: CSS matching, win 1)
- *(rusty_svgtree)* CSS rule index (vein: CSS matching) - NOT compiled, NOT measured
- *(rusty_svgtree)* stream path data into the builder (vein: number and path-data parsing, win 2)
- *(rusty_svgtree)* parse numbers while scanning them (vein: number and path-data parsing, win 1)
