# Changelog — rusty_mp3

Security-relevant changes are listed first in each release, under **Security**.
Releases before 0.9.1 are described in the README's per-version sections.

## Unreleased

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
- Test suite clean under Miri, `cargo careful`, ASan + LeakSanitizer, TSan and
  MSan.

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
