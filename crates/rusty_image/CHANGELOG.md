# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_image-v0.1.1...rusty_image-v0.1.2) - 2026-11-21

### Fixed

- *(rusty_image)* recognise an uncompressed true-colour TGA with no footer

### Other

- Merge pull request #171 from Remade-With-Rust/perf/veins-rusty-image
- *(rusty_image)* ICO entry DIB sized once and written in place
- *(rusty_image)* ICO/CUR encode without image clones or the validation copy
- *(rusty_image)* 32-bit icon and cursor entries through the word swizzle
- *(rusty_image)* BMP/DIB encoder rows, one out-of-line loop per format
- *(rusty_image)* HDR flat scanlines converted in step with their input
- *(rusty_image)* BMP RLE8 literals and long runs as word stores
- *(rusty_image)* BMP RLE4 runs and literals as word stores, each RLE kind in its own function
- *(rusty_image)* BMP RLE read as a shrinking slice in a function of its own
- *(rusty_image)* ICO/CUR AND mask applied a byte at a time
- *(rusty_image)* BMP bitfield layouts through per-channel tables
- *(rusty_image)* BMP direct layouts each get a row loop of their own, out of line
- *(rusty_image)* BMP 1/2/4-bit palettes a source byte at a time
- *(rusty_image)* TGA bottom-up rows placed directly
- *(rusty_image)* TGA 15/16-bit pixels straight to RGB
- *(rusty_image)* HDR new-style RLE decoded into component rows (sweep and wall check owed)
- *(rusty_image)* HDR orientation settled per scanline, not per pixel
- *(rusty_image)* HDR exponent scale from a const table
- *(rusty_image)* 16-bit PNM encode as one copy and an in-place byte swap
- *(rusty_image)* TGA 8-bit colour map as a table of finished pixels
- *(rusty_image)* PBM pixels from a byte table, and no scaling pass for PBM
- *(rusty_image)* PNM ASCII rasters parsed over a slice iterator
- WIP perf(rusty_image): PNM maxval scaling without roundf or fptosi (NOT YET GATED)
- *(rusty_image)* allow needless_range_loop on the HDR flat-row writer

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
