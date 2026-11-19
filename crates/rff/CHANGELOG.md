# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.6.1...remade-ffmpeg-v0.6.2) - 2026-11-19

### Added

- *(opus)* decode multichannel Opus, mapping families 1 and 255
- *(audio)* ALAC decoder and CAF demuxer, rff-codec-alac + rff-format-caf (#125 phase C)
- *(format)* AIFF / AIFF-C demuxer, rff-format-aiff (#125 phase B)

### Other

- keep each file's line endings as on main

## [0.6.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.6.0...remade-ffmpeg-v0.6.1) - 2026-11-18

### Added

- *(mp4)* faststart and caller edit lists ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

## [0.6.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.5.0...remade-ffmpeg-v0.6.0) - 2026-11-18

### Fixed

- *(mp4)* keep a track's start delay as an empty edit ([#115](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/115))
- *(cli)* [**breaking**] -vn/-an/-sn/-dn drop streams; unknown -c names are an error (#116, #117)

### Other

- *(format)* demuxers skip discarded streams at the source (Demuxer::set_discard)

## [0.5.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.4.8...remade-ffmpeg-v0.5.0) - 2026-11-14

### Added

- *(mp4)* QuickTime MOV muxer, ProRes/DNx, PCM in MOV, timecode, colour/HDR, fragmented reads ([#110](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/110))

### Other

- *(targets)* vp9 now stream-copies into mp4 (vp09 + vpcC, #110)

## [0.4.8](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.4.7...remade-ffmpeg-v0.4.8) - 2026-11-14

### Other

- *(fuzz)* print the findings before failing

## [0.4.7](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.4.6...remade-ffmpeg-v0.4.7) - 2026-11-13

### Other

- update Cargo.toml dependencies

## [0.4.6](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.4.5...remade-ffmpeg-v0.4.6) - 2026-11-07

### Other

- updated the following local packages: rff-codec-hevc

## [0.4.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/remade-ffmpeg-v0.4.0...remade-ffmpeg-v0.4.1) - 2026-10-24

### Other

- updated the following local packages: rff-codec-aac, rff-codec-mp3

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
