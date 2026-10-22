# Changelog — rff-codec-aac

All notable changes to this crate. Security-relevant changes are listed first in
each release, under **Security**. The project follows
[Semantic Versioning](https://semver.org/).

## [1.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-aac-v1.1.1...rff-codec-aac-v1.1.2) - 2026-10-22

### Other

- *(vet)* refresh mp3/aac imports for the 0.2.4 release and rusty_alloc-api 2.2.6
- Merge pull request #36 from Remade-With-Rust/ci/vet-imports

## [1.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-aac-v1.1.0...rff-codec-aac-v1.1.1) - 2026-10-17

### Other

- rusty_alloc-api 2.2.5; rff-cli and rff-server 0.3.0

## 1.1.0 — 2026-10-04

Requires `rusty_aac` 1.1.0, and inherits its security fixes and speed-ups.

### Security

- **Encoder: a frame's `samples` claim could panic the host or exhaust memory.**
  `send_frame` trusted the `AudioFrame`'s `samples` count: an empty `planes`
  panicked, a short plane was indexed past its end, and an inflated claim sized
  an allocation to it. The planes are now the authority — only the whole sample
  frames every plane holds are encoded; a missing plane, or fewer planes than
  channels for planar input, is an error. Consistent frames encode exactly as
  before.
- A `b` (bitrate) option beyond `u32` saturates instead of truncating.

### Hardening

- `#![forbid(unsafe_code)]`; clippy pedantic + nursery clean under `-D warnings`.
- Threat model for the adapter's own surface (`docs/threat-model.md`).
- cargo-fuzz harness (`fuzz/`: `decode_packets`, `encode_frames`); property and
  hostile-shape tests; a differential test against `rusty_aac` on real and
  mutated streams.
- Supply chain: `cargo vet` 9/9, `cargo audit` and `cargo deny` clean on the
  crate's closure; CycloneDX SBOM per release.

## 1.0.1 — 2026-10-04

Requires `rusty_aac` 1.0.1 (decode-speed release).

## 1.0.0 — 2026-10-04

First stable release over `rusty_aac` 1.0.0: LC / Main / LTP / HE-AAC v1 + v2 /
LD / ELD decode, AAC-LC encode.
