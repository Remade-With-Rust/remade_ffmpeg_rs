# Contributing to remade_ffmpeg_rs

Thanks for your interest! This is a young, pre-1.0 project — contributions,
bug reports, and reference test cases are all welcome.

## Ground rules (the project's invariants)

These are non-negotiable and CI-enforced; a PR that breaks them won't merge:

- **Pure Rust by default — no C/C++ FFI in the default build.** The *one*
  sanctioned exception is the opt-in `h264-asm` feature (hand-written assembly
  in an isolated crate; still no C). The opt-in `h264-openh264` feature is a
  deliberate, clearly-labelled C fallback and is never on by default.
- **No copyleft, ever.** Every dependency must be permissively licensed
  (Apache-2.0 / MIT / BSD / ISC / Zlib / Unicode-3.0, etc.). This is gated by
  [`deny.toml`](deny.toml) via `cargo deny`. Adding a GPL/LGPL/MPL crate — even
  transitively — is a hard fail.
- **Codecs are validated against a reference.** New or changed codec/format work
  should be checked against upstream output — bit-exact against a conformance
  suite where one exists, otherwise an FFmpeg round-trip in the test suite.
- **Hostile input must not panic.** Demuxers/decoders parse untrusted bytes; a
  panic is a bug (see `crates/rff/tests/demuxer_fuzz.rs` and `crates/rff/fuzz/`).

## Getting set up

```sh
git clone https://github.com/Remade-With-Rust/remade_ffmpeg_rs
cd remade_ffmpeg_rs
# Default build needs `nasm` (h264-asm); or use --no-default-features.
cargo build
cargo test --workspace
```

See the README's *Building from source* for the `nasm` requirement and the
`--no-default-features` / `--features https` alternatives.

## Before you open a PR

Run these locally — CI runs the same:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets    # no new warnings
cargo test --workspace
cargo deny check                          # licenses + bans
```

Then fill in the PR template checklist. Keep changes focused; one logical change
per PR. Match the surrounding code's style and comment density.

## Adding a codec or format

It's a first-class extension point — no engine-core changes needed:

- implement the `Decoder`/`Encoder` (codec) or `Demuxer`/`Muxer` (format) trait
  in a new `rff-codec-*` / `rff-format-*` crate,
- `register(...)` it with the engine,
- add it to the workspace + [`docs/compatibility.md`](docs/compatibility.md),
- include reference-validated tests.

New dependencies must clear `cargo deny`. If a codec only has a copyleft or
C-only implementation available, it can't go in the default tree — open an issue
to discuss before starting.

## Reporting issues

- **Bugs / features:** use the issue templates.
- **Security vulnerabilities:** do **not** open a public issue — follow
  [SECURITY.md](SECURITY.md) (private GitHub advisory).

## License

By contributing, you agree your contributions are licensed under the project's
[Apache-2.0](LICENSE) license.
