# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_jpx-v0.1.1...rusty_jpx-v0.1.2) - 2026-11-28

### Other

- lenient mode that reports what it conceals; 109-pass segments as OpenJPEG reads them ([#260](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/260))

## [0.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_jpx-v0.1.0...rusty_jpx-v0.1.1) - 2026-11-25

### Other

- *(rusty_jpx)* timings against OpenJPEG measured again
- *(rusty_jpx)* unsafe code lives in the kernels module; lint fixes
- *(rusty_jpx)* every kernel against its scalar twin, the MQ decoder against the book
- *(rusty_jpx)* columns sweep with running pointers away from the edges
- *(rusty_jpx)* no byte-wise tile-part gathering, palette clone or packet lists
- *(rusty_jpx)* buffers from zeroed memory instead of a zero fill
- *(rusty_jpx)* tile buffers kept, and cleared only under missing code-blocks
- *(rusty_jpx)* codeword segments read where they are
- *(rusty_jpx)* a 9/7 tile that is the whole image becomes the output in place
- *(rusty_jpx)* wavelet buffers kept between levels and tiles
- *(rusty_jpx)* one loop for the per-sample kernels, two registers a turn
- *(rusty_jpx)* 9/7 rows without a scaling pass
- *(rusty_jpx)* MQ interval test on the subtraction's own flags
- *(rusty_jpx)* MQ contexts carry their successors' offset
- *(rusty_jpx)* context numbers as an enumeration
- *(rusty_jpx)* tier-1 columns as one mask each, samples in scan order
- *(rusty_jpx)* MQ decoder on 64-bit registers
- *(rusty_jpx)* kernels for dequantization, colour transforms and level shift
- *(rusty_jpx)* wavelet kernels over AVX2, SSE2 and NEON, columns in one sweep
