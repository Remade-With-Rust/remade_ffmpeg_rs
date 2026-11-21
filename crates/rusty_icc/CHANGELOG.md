# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_icc-v0.1.0...rusty_icc-v0.1.1) - 2026-11-20

### Other

- *(rusty_icc)* matrix-shaper output index via a 32-bit conversion
- *(rusty_icc)* performance pass results and the new side-by-side
- *(rusty_icc)* rustfmt the perf-pass sources
- *(rusty_icc)* 8-bit grid lookups read a per-value axis table
- *(rusty_icc)* bulk big-endian writers for LUT tables and CLUT data
- *(rusty_icc)* Profile::encode reserves its size up front
- *(rusty_icc)* skip libm pow for exponent 1.0 (identity curves)
- *(rusty_icc)* Chain::eval on whole [f64; 16] arrays, no memcpy
- *(rusty_icc)* CLUT eval writes into the caller's array, no memcpy
- *(rusty_icc)* device-link grid loop in its own function
- *(rusty_icc)* monomorphised matrix-shaper loop with fixed-size table rows
- *(rusty_icc)* tetrahedral CLUT kernel reads corners as slices
- *(rusty_icc)* truncate instead of floor() in sampled-curve evaluation
- *(rusty_icc)* truncate instead of floor() in CLUT interpolation
