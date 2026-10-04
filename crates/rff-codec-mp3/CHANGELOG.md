# Changelog — rff-codec-mp3

All notable changes to this crate. Security-relevant changes are listed first in
each release, under **Security**. The project follows
[Semantic Versioning](https://semver.org/).

## 1.0.0 — 2026-10-04

First stable release, depending on `rusty_mp3` 1.x.

### Security

- **A frame's sample count could panic or exhaust memory.** `send_frame`
  trusted `AudioFrame::samples`: a frame with no sample plane panicked, and a
  count larger than the plane forced an allocation of the claimed size. The
  plane is now authoritative: only the whole samples it holds are encoded, and
  a frame without a plane is rejected with `InvalidData`. Output is unchanged for
  every well-formed frame.

### Hardening

- `#![forbid(unsafe_code)]`; fuzz targets for the `Decoder` and `Encoder` trait
  paths; property and differential tests (the decoded plane equals
  `rusty_mp3`'s PCM bit for bit); clippy pedantic + nursery clean; tests clean
  under Miri, ASan, TSan, MSan and `cargo careful`.

### Changed

- Input frames are passed to the encoder as bytes, without intermediate sample
  buffers.
- README rewritten; the earlier claim of quality parity with LAME is replaced by
  the measured comparison in `rusty_mp3`'s README.

## 0.2.2 and earlier

See the [crates.io version history](https://crates.io/crates/rff-codec-mp3/versions).
