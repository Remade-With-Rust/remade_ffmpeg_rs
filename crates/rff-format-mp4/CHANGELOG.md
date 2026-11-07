# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.4.4...rff-format-mp4-v0.4.5) - 2026-10-29

### Fixed

- *(mp4)* keep the public fmp4 API backward-compatible
- *(mp4)* keep AAC priming edit list on remux; B-frame timing for DASH/fMP4

## [0.4.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.4.3...rff-format-mp4-v0.4.4) - 2026-10-28

### Fixed

- *(mp4,mkv)* write B-frame timing as FFmpeg does ([#60](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/60))

### Other

- Merge branch 'pr45' into fix/60-bframe-timing

## [0.3.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.3.0...rff-format-mp4-v0.3.1) - 2026-10-23

### Added

- *(mp4)* stream mdat to seekable outputs, moov at the end ([#28](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/28))
