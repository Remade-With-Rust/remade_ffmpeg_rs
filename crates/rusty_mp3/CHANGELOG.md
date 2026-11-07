# Changelog — rusty_mp3

All notable changes to this crate. Security-relevant changes are listed first in
each release, under **Security**. The project follows
[Semantic Versioning](https://semver.org/): from 1.0.0 the public API is stable;
decoder output is bit-exact and stable; encoder output may change in minor
releases when quality improves. Engineering notes for releases before 1.0.0
are in [`docs/history.md`](docs/history.md).

## [1.0.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_mp3-v1.0.4...rusty_mp3-v1.0.5) - 2026-10-24

### Other

- Merge pull request #78 from Remade-With-Rust/release-plz-2026-10-24T09-31-44Z

## [1.0.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_mp3-v1.0.3...rusty_mp3-v1.0.4) - 2026-10-24

### Fixed

- *(ci)* stop the vet-import release loop: exclude supply-chain/ from the published mp3/aac crates

## [1.0.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_mp3-v1.0.2...rusty_mp3-v1.0.3) - 2026-10-24

### Other

- clippy -D warnings clean for the new decode paths
- fuse long-block requantization into the Huffman decode
- do the long-block IMDCT window, mirror and overlap-add in registers
- expand the synthesis matrixing in registers, straight into the FIFO
- decprof streams its decode instead of collecting every frame
- run stereo synthesis for both channels at once, written interleaved
- requantize band scales from an exact 2^(q/4) table
- put the decode stage clock behind a `profile` feature
- decode Huffman pairs/quads through one flat table + 64-bit bit cache

## [1.0.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_mp3-v1.0.1...rusty_mp3-v1.0.2) - 2026-10-22

### Other

- *(vet)* refresh mp3/aac imports for the 0.2.4 release and rusty_alloc-api 2.2.6
- Merge pull request #36 from Remade-With-Rust/ci/vet-imports

## [1.0.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_mp3-v1.0.0...rusty_mp3-v1.0.1) - 2026-10-17

### Other

- rusty_alloc-api 2.2.5; rff-cli and rff-server 0.3.0

## 1.0.0 — 2026-10-04

First stable release. The public API is now covered by Semantic Versioning, and
the crate has passed a full security-hardening audit
([`docs/plans/use-protection-please.md`](docs/plans/use-protection-please.md)).

### Security

- **Encoder: hostile PCM could panic the process.** Input far beyond full scale
  (integer-valued "f32" samples, random bit patterns) could not be quantised under
  the frame budget, and the bit-reservoir assembly `expect`-panicked (56 of 3,000
  hostile encodes). Samples are now sanitised at the push boundary (NaN → 0,
  everything else clamped to `MAX_INPUT_AMPLITUDE` = 8× full scale — identity on
  all ordinary input), and the assembly degrades instead of panicking.
- **Encoder: unbounded memory growth on sample-rate changes.** The psychoacoustic
  band-model cache leaked a ~2.4 KB model on every change of sample rate, on every
  thread. Found by the `encode` fuzz target under LeakSanitizer; now a fixed
  per-process table of nine models.
- Xing/Info header totals saturate instead of wrapping on streams over 4 GiB.

### Hardening (no output change)

- Threat model (`docs/threat-model.md`), `UNSAFE.md`, a SAFETY comment on every
  `unsafe` block; ring-buffer SIMD kernels mask their own index.
- cargo-fuzz harness (`fuzz/`): decoder, frame-level decoder, encoder, and
  pipelined-vs-serial equivalence targets, seeded.
- Supply chain: `cargo vet` 14/14 fully audited, `cargo audit` and `cargo deny`
  clean on the crate's closure (`tools/hardening/standalone_supply_chain.sh`).
- clippy pedantic + nursery clean under `-D warnings`; narrowing casts are
  lint-enforced in every module that parses untrusted bytes.
- Test suite clean under Miri (86 tests), `cargo careful`, ASan +
  LeakSanitizer, TSan and MSan; property tests for every documented invariant
  (`tests/properties.rs`); a CI gate runs on every change, plus daily fuzzing
  and advisory scans.

### Added

- `Mp3Encoder::push_pcm_s16le` / `push_pcm_f32le`: push the container's
  little-endian bytes directly.
- `MAX_INPUT_AMPLITUDE`.
- `Mp3EncoderConfig` is now `Copy`.

### Performance

- The encoder converts and deinterleaves input straight into a one-frame buffer
  (no whole-file staging): whole-input push peak memory 13.5× lower.
- Decoder front end allocation-free (6.02 → 1.02 allocations per frame); encoder
  steady state 12.03 → 4.02.

### Documentation

- README rewritten as a product overview; release engineering notes moved to
  `docs/history.md`; new `CHANGELOG.md`, `UNSAFE.md`, threat model.

## 0.9.0 and earlier

See the [crates.io version history](https://crates.io/crates/rusty_mp3/versions)
and [`docs/history.md`](docs/history.md).
