# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vp9-v0.8.4...rff-codec-vp9-v0.9.0) - 2026-11-24

### Fixed

- signal full colour range in AVIF/VP9/H.264; setrange at every bit depth (#213, #214)
- *(cli)* convert RGB to the encoder's pixel format automatically; honour -pix_fmt ([#179](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/179))

## [0.8.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vp9-v0.8.3...rff-codec-vp9-v0.8.4) - 2026-11-23

### Other

- update Cargo.toml dependencies

## [0.8.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vp9-v0.8.2...rff-codec-vp9-v0.8.3) - 2026-11-22

### Other

- update Cargo.toml dependencies

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vp9-v0.6.3...rff-codec-vp9-v0.7.0) - 2026-11-21

### Fixed

- *(packaging)* ship the Apache-2.0 LICENSE text in every published crate

## [0.6.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vp9-v0.5.0...rff-codec-vp9-v0.6.0) - 2026-11-18

### Added

- *(vp9)* [**breaking**] keyframe interval; VP9_* environment switches behind a feature ([#119](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/119))

## [0.4.7](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vp9-v0.4.6...rff-codec-vp9-v0.4.7) - 2026-11-13

### Other

- update Cargo.toml dependencies

## [0.2.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vp9-v0.2.3...rff-codec-vp9-v0.2.4) - 2026-10-17

### Other

- update Cargo.toml dependencies
