# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.4.4...rff-format-mkv-v0.4.5) - 2026-10-29

### Fixed

- *(mkv)* DefaultDuration packet durations; drop a truncated final block

## [0.4.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.4.3...rff-format-mkv-v0.4.4) - 2026-10-28

### Fixed

- *(mp4,mkv)* write B-frame timing as FFmpeg does ([#60](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/60))

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.3.1...rff-format-mkv-v0.4.0) - 2026-10-24

### Fixed

- *(mkv,vorbis,opus)* trim the stream end by Matroska DiscardPadding

## [0.3.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.3.0...rff-format-mkv-v0.3.1) - 2026-10-23

### Added

- *(mkv)* stream Matroska/WebM to seekable outputs ([#28](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/28))
