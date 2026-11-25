# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.17](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.16...rff-cli-v0.4.17) - 2026-11-25

### Added

- *(rff-cli)* -show_chapters, -map_chapters, -map N:d ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

## [0.4.16](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.15...rff-cli-v0.4.16) - 2026-11-24

### Fixed

- signal full colour range in AVIF/VP9/H.264; setrange at every bit depth (#213, #214)
- *(cli)* convert RGB to the encoder's pixel format automatically; honour -pix_fmt ([#179](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/179))

## [0.4.15](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.14...rff-cli-v0.4.15) - 2026-11-23

### Other

- update Cargo.toml dependencies

## [0.4.14](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.13...rff-cli-v0.4.14) - 2026-11-22

### Other

- update Cargo.lock dependencies

## [0.4.13](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.12...rff-cli-v0.4.13) - 2026-11-22

### Other

- update Cargo.toml dependencies

## [0.4.12](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.11...rff-cli-v0.4.12) - 2026-11-22

### Other

- update Cargo.lock dependencies

## [0.4.11](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.10...rff-cli-v0.4.11) - 2026-11-22

### Other

- update Cargo.lock dependencies

## [0.4.10](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.9...rff-cli-v0.4.10) - 2026-11-22

### Added

- *(jpeg)* JPEG Multi-Picture Format (MPF) demux and mux

### Other

- Merge pull request #184 from Remade-With-Rust/feat/jpeg-mpf

## [0.4.9](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.8...rff-cli-v0.4.9) - 2026-11-21

### Other

- update Cargo.lock dependencies

## [0.4.8](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.7...rff-cli-v0.4.8) - 2026-11-21

### Other

- update Cargo.lock dependencies

## [0.4.7](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.6...rff-cli-v0.4.7) - 2026-11-21

### Added

- *(avif)* default-on `av1-asm` feature so rff-cli builds without NASM ([#172](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/172))

## [0.4.6](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.5...rff-cli-v0.4.6) - 2026-11-21

### Fixed

- *(packaging)* ship the Apache-2.0 LICENSE text in every published crate

### Other

- Merge pull request #157 from Remade-With-Rust/fix/licenses-alac

## [0.4.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.4...rff-cli-v0.4.5) - 2026-11-20

### Other

- update Cargo.lock dependencies

## [0.4.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.3...rff-cli-v0.4.4) - 2026-11-19

### Other

- update Cargo.lock dependencies

## [0.4.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.2...rff-cli-v0.4.3) - 2026-11-19

### Other

- update Cargo.lock dependencies

## [0.4.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.1...rff-cli-v0.4.2) - 2026-11-19

### Other

- update Cargo.lock dependencies

## [0.4.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.4.0...rff-cli-v0.4.1) - 2026-11-18

### Added

- *(mp4)* faststart and caller edit lists ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.16...rff-cli-v0.4.0) - 2026-11-18

### Fixed

- *(cli)* [**breaking**] -vn/-an/-sn/-dn drop streams; unknown -c names are an error (#116, #117)

## [0.3.16](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.15...rff-cli-v0.3.16) - 2026-11-14

### Other

- update Cargo.lock dependencies

## [0.3.15](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.14...rff-cli-v0.3.15) - 2026-11-14

### Other

- update Cargo.lock dependencies

## [0.3.14](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.13...rff-cli-v0.3.14) - 2026-11-13

### Other

- update Cargo.toml dependencies

## [0.3.13](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.12...rff-cli-v0.3.13) - 2026-11-08

### Other

- update Cargo.lock dependencies

## [0.3.12](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.11...rff-cli-v0.3.12) - 2026-11-07

### Other

- update Cargo.lock dependencies

## [0.3.11](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.10...rff-cli-v0.3.11) - 2026-10-29

### Other

- update Cargo.lock dependencies

## [0.3.10](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.9...rff-cli-v0.3.10) - 2026-10-28

### Other

- update Cargo.lock dependencies

## [0.3.9](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.8...rff-cli-v0.3.9) - 2026-10-24

### Other

- update Cargo.lock dependencies

## [0.3.8](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.7...rff-cli-v0.3.8) - 2026-10-24

### Other

- update Cargo.lock dependencies

## [0.3.7](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.6...rff-cli-v0.3.7) - 2026-10-24

### Other

- update Cargo.lock dependencies

## [0.3.6](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.5...rff-cli-v0.3.6) - 2026-10-24

### Other

- update Cargo.lock dependencies

## [0.3.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.4...rff-cli-v0.3.5) - 2026-10-24

### Other

- update Cargo.lock dependencies

## [0.3.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.3...rff-cli-v0.3.4) - 2026-10-23

### Other

- update Cargo.lock dependencies

## [0.3.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.2...rff-cli-v0.3.3) - 2026-10-22

### Other

- update Cargo.lock dependencies

## [0.3.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-cli-v0.3.1...rff-cli-v0.3.2) - 2026-10-17

### Other

- update Cargo.toml dependencies
