# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-heif-v0.9.1...rff-format-heif-v0.9.2) - 2026-11-26

### Added

- *(heif)* HEIF/HEIC still-image decode (rff-format-heif) ([#233](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/233))

### Added

- HEIF/HEIC still-image decoding: `hvc1` primary images, `grid` (tiles decoded
  in parallel, cropped to the output size), `iden`, `pred`-coded items;
  `clap`/`irot`/`imir` in `ipma` order; alpha (`auxl` + `prem`); `colr` nclx
  (VUI fallback) and ICC; Exif and XMP; depth maps and thumbnails; an rff
  demuxer (`heic`, `heif`, `hif`). (#137)
