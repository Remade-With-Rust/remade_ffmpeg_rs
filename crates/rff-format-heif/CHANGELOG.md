# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- HEIF/HEIC still-image decoding: `hvc1` primary images, `grid` (tiles decoded
  in parallel, cropped to the output size), `iden`, `pred`-coded items;
  `clap`/`irot`/`imir` in `ipma` order; alpha (`auxl` + `prem`); `colr` nclx
  (VUI fallback) and ICC; Exif and XMP; depth maps and thumbnails; an rff
  demuxer (`heic`, `heif`, `hif`). (#137)
