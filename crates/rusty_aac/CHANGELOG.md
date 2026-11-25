# Changelog — rusty_aac

All notable changes to this crate. Security-relevant changes are listed first in
each release, under **Security**. The project follows
[Semantic Versioning](https://semver.org/): from 1.0.0 the public API is stable;
decoder output is stable (gated against the ISO/IEC 14496-26 conformance suite);
encoder output may change in minor releases when quality improves.

## [2.2.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.1.6...rusty_aac-v2.2.0) - 2026-11-25

### Added

- *(aac)* spend a high-rate frame's spare bits on the bands that carry the signal

### Fixed

- *(aac)* surplus shaping with one channel of the pair silent
- *(aac)* keep room for TNS data in a block at the decoder buffer's limit

## [2.1.6](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.1.5...rusty_aac-v2.1.6) - 2026-11-24

### Fixed

- *(decode)* window irregular `window_sequence` transitions per ISO/IEC 14496-3 §4.6.11: each frame's half is windowed by its own sequence and the two are summed, instead of one slope chosen from the previous block ([#208](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/issues/208), [#209](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/209)). ISO conformance stream `al03_48` goes from max error 2,800 LSB (RMS 88) to 0.004 LSB; all 24 `al03*` streams now match their ISO references. The other 401 ISO LC/Main/LTP streams decode byte-identically. Encoder output is unchanged.

## [2.1.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.1.4...rusty_aac-v2.1.5) - 2026-11-23

### Other

- update Cargo.toml dependencies

## [2.1.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.1.3...rusty_aac-v2.1.4) - 2026-11-22

### Other

- update Cargo.toml dependencies

## [2.1.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.1.2...rusty_aac-v2.1.3) - 2026-11-22

### Other

- move the 2.2 low-rate knobs into AacTuning (semver-compatible)
- aac encoder: section trellis prices books with #189's one-walk BandCost
- Merge remote-tracking branch 'origin/main' into feat/aac-lowrate-quality
- aac encoder: clippy -D warnings clean (section trellis, bandwidth docs)
- aac encoder: enable the low-rate tools by default
- aac encoder: mask SMR as a setting (default 18 dB, unchanged)
- aac encoder: frame-demand bit allocation within reservoir segments (switch, default off)
- aac encoder: coding bandwidth policy (switch, default off)
- aac encoder: high-passed transient detector (switch, default off)
- aac encoder: section trellis for codebook selection (switch, default off)

## [2.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.1.1...rusty_aac-v2.1.2) - 2026-11-21

### Fixed

- *(rusty_aac)* narrow-band SNR floor so tone-over-noise SNR rises with bitrate
- *(rusty_aac)* busy 5.1 channels keep their level; bitrate lands on target ([#129](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/129))

### Other

- Merge pull request #167 from Remade-With-Rust/fix/aac-psy-snr
- *(rusty_aac)* rate loops hand back the coding they already did

## [2.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.1.0...rusty_aac-v2.1.1) - 2026-11-19

### Fixed

- *(aac)* a hearing floor in the mask, and pooled multichannel budgets ([#129](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/129))

### Other

- *(aac)* backticks in the #129 test-module doc

## [2.1.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.0.2...rusty_aac-v2.1.0) - 2026-11-18

### Added

- *(aac)* streaming encode; priming and padding for the edit list ([#111](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/111))

## [2.0.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.0.1...rusty_aac-v2.0.2) - 2026-11-13

### Other

- update Cargo.toml dependencies

## [2.0.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v2.0.0...rusty_aac-v2.0.1) - 2026-10-24

### Fixed

- *(ci)* stop the vet-import release loop: exclude supply-chain/ from the published mp3/aac crates

## [2.0.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v1.1.2...rusty_aac-v2.0.0) - 2026-10-24

### Other

- Merge branch 'main' into perf/aac-decode

## [1.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v1.1.1...rusty_aac-v1.1.2) - 2026-10-22

### Other

- *(vet)* refresh mp3/aac imports for the 0.2.4 release and rusty_alloc-api 2.2.6
- Merge pull request #36 from Remade-With-Rust/ci/vet-imports

## [1.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_aac-v1.1.0...rusty_aac-v1.1.1) - 2026-10-17

### Other

- rusty_alloc-api 2.2.5; rff-cli and rff-server 0.3.0
- *(rff-codec-aac)* H-37/H-12 evidenced by CI run 37272275669 and the 1.1.0 release

## 1.1.0 — 2026-10-04

A security-hardening and performance release. The crate has passed a full
hardening audit ([`docs/plans/use-protection-please.md`](docs/plans/use-protection-please.md)).
Decoder output is bit-identical to 1.0.1 on every conforming stream.

### Security

- **Decoder: a fill element could panic the host (overflow-checked builds).** A
  fill element with an escaped count of zero computed `0 - 1`; any build with
  overflow checks (debug builds, and release profiles that enable them) panicked
  on one crafted byte. Found by fuzzing.
- **Decoder: stale state after a failed block could index out of bounds.** A block
  that failed part-way left its elements marked present with half-updated state; a
  later block rendered that mix and indexed past the spectrum (a panic, not memory
  corruption — bounds-checked). Elements are now cleared after a failed block, and
  TNS data is only applied to the window layout it was parsed for. Found by fuzzing.
- **Decoder: AAC-LTP with 960-sample frames could index out of bounds.** That
  combination is now refused at configuration time with `Error::Unsupported`.
  Found by fuzzing.
- **Encoder: hostile PCM produced oversized blocks.** ±Inf or 1e30 samples drove
  every band to maximal escape codes and the encoder emitted a block larger than
  the decoder input buffer (and than ADTS can frame). Input is now sanitised at the
  push boundary — NaN becomes silence, samples are clamped to ±8× full scale
  ([`encode::MAX_INPUT_AMPLITUDE`]) — and every block is capped at the decoder
  buffer (6144 bits per channel), even at `bitrate_bps = u32::MAX`. Ordinary audio
  encodes byte-identically.
- The internal mixed-radix DCT plan cache no longer grows per distinct length (a
  fixed table replaces a map of leaked plans).
- The AVX2 / AVX-512 quantize kernels' length precondition is now enforced at the
  call site, not assumed.

### Performance

- Decode: about 7–9% fewer instructions (LC, HE-AAC v1, HE-AAC v2) from removing
  redundant copies and zero fills — a constant-length bit-window load, exact SBR
  `X_low` writes, persistent PS and QMF buffers, a bit-reversed QMF analysis input,
  short-window synthesis reading in place, no per-frame window-grouping
  allocations. Encode: 3–5% fewer, from borrowing input blocks and filling MDCT
  buffers in place.

### Added

- `BitReader::read_u8`, `read_u16`, `read_i32`: typed reads that narrow by
  construction.
- `encode::MAX_INPUT_AMPLITUDE`.
- `#[must_use]` on pure functions; `# Errors` / `# Panics` documentation on every
  public fallible function.

### Hardening (no output change)

- Threat model (`docs/threat-model.md`) and `UNSAFE.md`; a SAFETY comment on every
  `unsafe` block and a `# Safety` section on every `unsafe fn`.
- cargo-fuzz harness (`fuzz/`, five targets) with seeds from the ISO suite; every
  past crasher is a regression test (`tests/regressions.rs`).
- Property tests (`tests/properties.rs`): decoder robustness, ADTS / ASC round
  trips, encode→decode shape, push-shape invariance, hostile PCM, LOAS ≡ raw.
- Supply chain: `cargo vet` 14/14 fully audited, `cargo audit` and `cargo deny`
  clean on the crate's closure; CycloneDX SBOM per release.
- clippy pedantic + nursery clean under `-D warnings`; narrowing casts are
  lint-enforced in every module and function that parses untrusted bytes.
- CI (`aac-hardening`): the ISO/IEC 14496-26 differential gate, fuzz corpus replay,
  daily fuzzing and advisory scan, weekly Miri, MSRV 1.85.

## 1.0.1 — 2026-10-04

Decode-speed release: fifteen instruction-count-proven kernel changes, each
bit-identical to 1.0.0 (LC −16%, HE-AAC v1 −30%, HE-AAC v2 −23% instructions).

## 1.0.0 — 2026-10-04

First stable release: the whole MPEG-4 AAC family short of USAC decodes (Main,
LTP, HE-AAC v1/v2, ER AAC-LC/LTP, AAC-LD, AAC-ELD, 960-sample frames, PCE layouts,
LATM), checked against FFmpeg on the ISO/IEC 14496-26 conformance suite.
