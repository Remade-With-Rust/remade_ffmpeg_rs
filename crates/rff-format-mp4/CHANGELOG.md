# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.6.3...rff-format-mp4-v0.7.0) - 2026-11-21

### Added

- *(rff-format-mp4)* rotation, dOps/av1C/vpcC, stored table + movie timescale, udta, spooled faststart; fix >=64 kHz rates and 32-bit offsets ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

### Other

- Merge pull request #157 from Remade-With-Rust/fix/licenses-alac
- *(rff-format-mp4, rff-format-mkv)* list the #114 features in the crate READMEs

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.6.1...rff-format-mp4-v0.6.2) - 2026-11-19

### Fixed

- *(mp4)* a cut-short file reads its samples, then ends (#125 phase E)

### Other

- keep each file's line endings as on main

## [0.6.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.6.0...rff-format-mp4-v0.6.1) - 2026-11-18

### Added

- *(mp4)* faststart and caller edit lists ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))
- *(mp4)* mux HEVC (hvc1 + hvcC), MJPEG (jpeg) and MP3 (mp4a, 0x6B) ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))
- *(mp4)* random access, raw samples and avcC/hvcC extradata ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

## [0.6.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.5.0...rff-format-mp4-v0.6.0) - 2026-11-18

### Fixed

- *(mp4)* keep a track's start delay as an empty edit ([#115](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/115))

### Other

- *(mp4)* hop within the read buffer instead of seeking
- *(format)* demuxers skip discarded streams at the source (Demuxer::set_discard)

## [0.5.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp4-v0.4.8...rff-format-mp4-v0.5.0) - 2026-11-14

### Added

- *(mp4)* QuickTime MOV muxer, ProRes/DNx, PCM in MOV, timecode, colour/HDR, fragmented reads ([#110](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/110))

### Fixed

- *(mp4)* keep PCM out of the mp4 muxer; it stays a mov feature

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
