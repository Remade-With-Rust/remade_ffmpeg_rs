# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-wav-v0.6.3...rff-format-wav-v0.7.0) - 2026-11-21

### Fixed

- *(packaging)* ship the Apache-2.0 LICENSE text in every published crate

## [0.6.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-wav-v0.6.2...rff-format-wav-v0.6.3) - 2026-11-19

### Fixed

- *(wav)* resolve an extensible SubFormat only from a GUID family that carries a format tag

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-wav-v0.6.1...rff-format-wav-v0.6.2) - 2026-11-19

### Added

- *(audio)* G.711 and ADPCM decoders, extended WAV, FLAC in Ogg (#125 phase A)

### Other

- keep each file's line endings as on main
- rustfmt the phase A/B audio crates

## [0.3.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-wav-v0.3.0...rff-format-wav-v0.3.1) - 2026-10-23

### Added

- *(wav)* stream the WAV muxer on seekable outputs ([#28](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/28))
