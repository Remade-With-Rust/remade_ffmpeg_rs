# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-caf-v0.6.2...rff-format-caf-v0.6.3) - 2026-11-19

### Added

- *(caf)* map MPEG audio (.mp1/.mp2/.mp3) and FLAC, with the STREAMINFO from the cookie

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-caf-v0.6.1...rff-format-caf-v0.6.2) - 2026-11-19

### Added

- *(audio)* ALAC decoder and CAF demuxer, rff-codec-alac + rff-format-caf (#125 phase C)

### Other

- keep each file's line endings as on main
