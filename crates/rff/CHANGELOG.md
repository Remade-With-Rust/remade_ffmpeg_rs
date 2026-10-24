# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.3.1...remade-ffmpeg-v0.4.0) - 2026-10-24

### Other

- Merge pull request #67 from Remade-With-Rust/fix/ogg-page-crc
- Merge pull request #70 from Remade-With-Rust/test/fuzz-watchdog-34
- *(rff)* fuzz_decode watchdog warns at 4 s, aborts at 20 s, and its messages survive the abort ([#34](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/34))
- release

## [0.3.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.3.1...remade-ffmpeg-v0.3.2) - 2026-10-24

### Other

- updated the following local packages: rff-codec-aac, rff-codec-mp3

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
