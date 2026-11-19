# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-aiff-v0.6.2...rff-format-aiff-v0.6.3) - 2026-11-19

### Fixed

- *(aiff)* match compression types without regard to case, and read 23ni / 42ni

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-aiff-v0.6.1...rff-format-aiff-v0.6.2) - 2026-11-19

### Added

- *(format)* AIFF / AIFF-C demuxer, rff-format-aiff (#125 phase B)

### Other

- keep each file's line endings as on main
- rustfmt the phase A/B audio crates
