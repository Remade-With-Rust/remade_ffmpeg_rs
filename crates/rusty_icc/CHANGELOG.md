# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_icc-v0.1.2...rusty_icc-v0.1.3) - 2026-11-21

### Other

- Merge pull request #180 from Remade-With-Rust/perf/loss-rows-icc
- *(rusty_icc)* Lab encode and decode run as one op each
- *(rusty_icc)* grid build walks its points with an odometer
- *(rusty_icc)* grid build tabulates CLUT cells per axis value
- *(rusty_icc)* sRGB-shaped forward curves in line in non-shaper chains
- *(rusty_icc)* sRGB-shaped inverse curves in line in non-shaper chains
- *(rusty_icc)* LUT table curves evaluated in line in the f32 chain
- *(rusty_icc)* perceptual twins of the float CMYK, proof and build workloads

## [0.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_icc-v0.1.1...rusty_icc-v0.1.2) - 2026-11-21

### Other

- speed numbers in five image crate READMEs; rusty_tiff needs flate2 1.1.11
- *(rusty_icc)* README section for the second performance pass
- *(rusty_icc)* grey 8-bit sources read a 256-entry output table
- *(rusty_icc)* 8- and 16-bit outputs skip the clamp the cast already does
- *(rusty_icc)* integer device-link grid loops are compiled in the library
- *(rusty_icc)* sRGB-shaped f32 matrix/TRC pairs evaluate their curves in line
- *(rusty_icc)* CLUT front ends for 3 and 4 inputs, kernels out of line
- *(rusty_icc)* 3- and 4-input CLUT front ends and fixed-width kernels [draft, unmeasured]
- *(rusty_icc)* unit-gamma LUT curve stages as one clamp op [candidate win, not final]
- *(rusty_icc)* tetrahedral lookups read four corners on fixed-width lanes
- *(rusty_icc)* grid builds evaluate the device curves once per axis value
- *(rusty_icc)* shaper tables are built once per distinct curve
- *(rusty_icc)* CLUT grid cells are found with one cvttsd2si
- *(rusty_icc)* sampled curves find their segment with one cvttsd2si
- *(rusty_icc)* the general f32 loop is compiled in the library, in place
- *(rusty_icc)* f32 pixels between two matrix/TRC profiles run one fused loop
- *(rusty_icc)* the WSL bench copy keeps its Cargo.lock
- *(rusty_icc)* black-point search builds its round-trip sides once
- *(rusty_icc)* inverse sRGB-shaped curves keep their break value

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
