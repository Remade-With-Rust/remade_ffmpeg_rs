# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.3.0...remade-ffmpeg-v0.3.1) - 2026-10-23

### Added

- *(format)* seekable muxer output (SeekOutput / open_muxer_seekable), files open seekable

## [0.3.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.2.4...remade-ffmpeg-v0.3.0) - 2026-10-22

### Added

- *(vorbis)* decode with the in-house rusty_vorbis decoder, not lewton

### Fixed

- *(opus)* honour OpusHead pre-skip/gain and trim to the final granule
- *(ogg,vorbis)* trim the stream end to the final granule position

## [0.2.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.2.3...remade-ffmpeg-v0.2.4) - 2026-10-17

### Other

- update Cargo.toml dependencies
