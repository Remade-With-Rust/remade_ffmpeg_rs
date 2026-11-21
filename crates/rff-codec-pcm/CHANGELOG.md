# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-pcm-v0.6.3...rff-codec-pcm-v0.7.0) - 2026-11-21

### Fixed

- *(packaging)* ship the Apache-2.0 LICENSE text in every published crate

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-pcm-v0.6.1...rff-codec-pcm-v0.6.2) - 2026-11-19

### Added

- *(audio)* G.711 and ADPCM decoders, extended WAV, FLAC in Ogg (#125 phase A)

### Other

- keep each file's line endings as on main
- rustfmt the phase A/B audio crates

## [0.5.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-pcm-v0.4.8...rff-codec-pcm-v0.5.0) - 2026-11-14

### Added

- *(core)* [**breaking**] video metadata, block additions, copy-only codecs and wider PCM for FilmCraft ([#110](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/110))
