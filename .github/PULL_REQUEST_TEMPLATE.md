<!-- Thanks for contributing to remade_ffmpeg_rs! -->

## What this changes

A short description of the change and why.

Closes #<!-- issue number, if any -->

## Checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace` is clean (no new warnings)
- [ ] `cargo fmt --all` applied
- [ ] `cargo deny check` passes — **no new copyleft / non-permissive dependency**
      and the default build stays **pure Rust** (no C/FFI; the only sanctioned
      exception is the opt-in `h264-asm` hand-written assembly)
- [ ] New behavior is covered by a test; codec/format work is validated against
      reference output (bit-exact or an FFmpeg round-trip) where practical
- [ ] Docs updated if this changes user-facing behavior (README /
      `docs/compatibility.md`)

## Notes for reviewers

<!-- Anything tricky, trade-offs, follow-ups, or things you want a close look at. -->
