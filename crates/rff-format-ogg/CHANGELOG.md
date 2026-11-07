# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-ogg-v0.3.1...rff-format-ogg-v0.4.0) - 2026-10-24

### Fixed

- *(ogg)* verify page CRCs; drop bad pages and resync like FFmpeg

## [0.3.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-ogg-v0.2.4...rff-format-ogg-v0.3.0) - 2026-10-22

### Fixed

- *(opus)* honour OpusHead pre-skip/gain and trim to the final granule
- *(ogg,vorbis)* trim the stream end to the final granule position
- *(vorbis)* decode to f32 instead of quantising to s16

### Other

- *(clippy)* clear the warnings in the crates this fix touches
