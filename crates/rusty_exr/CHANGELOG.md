# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_exr-v0.2.2...rusty_exr-v0.2.3) - 2026-11-28

### Added

- *(exr)* decode into caller buffers, encode from borrowed planes; at or ahead of exr 1.74.2 on Effect's paths ([#254](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/254))

## [0.2.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_exr-v0.2.1...rusty_exr-v0.2.2) - 2026-11-28

### Other

- *(exr)* DWA decode faster than OpenEXR 3.5.5 single-threaded, byte-identical to 0.2.1 (-29..-39% Ir, 0.77-0.88x OpenEXR wall) ([#249](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/249))

## [0.2.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_exr-v0.2.0...rusty_exr-v0.2.1) - 2026-11-27

### Other

- *(exr)* SIMD twins, kernel work and buffer reuse for rusty_exr, byte-identical (decode up to -59%, encode up to -66% Ir) ([#244](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/244))

## [0.2.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_exr-v0.1.0...rusty_exr-v0.2.0) - 2026-11-27

### Added

- *(exr)* B44/B44A/DWAA/DWAB and tiled encoding, byte-identical to exr ([#241](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/241))

## [0.1.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/releases/tag/rusty_exr-v0.1.0) - 2026-11-26

### Added

- *(exr)* rusty_exr — complete OpenEXR decoder + lossless scanline encoder, rff-codec-exr / rff-format-exr ([#236](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/236))
