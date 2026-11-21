# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_image-v0.1.0...rusty_image-v0.1.1) - 2026-11-20

### Fixed

- *(rusty_image)* probe reads PAM headers followed by binary pixels

### Other

- *(rusty_image)* QOI encode tests green's LUMA range before DIFF
- *(rusty_image)* HDR encode quantiser without a clamp; branch-free RLE scan
- *(rusty_image)* QOI encode tests DIFF/LUMA fit with one compare each
- *(rusty_image)* QOI encode per-op and per-branch census
- *(rusty_image)* record a third refuted QOI encode shape
- *(rusty_tiff, rusty_image)* remove the last avoidable codec copies
- *(rusty_image)* record the refuted QOI worst-case output reservation
- *(rusty_image)* flat HDR scanlines convert straight into the output row
- *(rusty_image)* QOI decode dispatches on range tests, not a jump table
- *(rusty_image)* red/blue swaps in two instructions a pixel
- *(rusty_image)* TGA swizzles while expanding; encode with one swizzle copy
- *(rusty_image)* flat/old-RLE HDR scanlines read as a slice
- *(rusty_image)* BMP 8-bit palette, RLE8 and 32-bit bitfield decode
- *(rusty_image)* QOI on packed pixels with a one-multiply hash
- *(rusty_image)* HDR encode without a float copy or per-row Vecs; direct-row decode
