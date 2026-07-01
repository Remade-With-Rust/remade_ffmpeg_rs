# Security Policy

`remade_ffmpeg_rs` parses untrusted media and fetches untrusted URLs, so we take
input-handling bugs seriously. It is also **pre-1.0 and not yet independently
audited** — treat it accordingly in production.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report privately through GitHub's **"Report a vulnerability"** button on the
repository's **Security** tab
([open a draft advisory](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/security/advisories/new)).
That opens a private advisory only the maintainers can see.

Please include:

- the affected component (a demuxer/decoder, the `rff-io` HTTP/TLS client, the
  server, …) and version / commit,
- a minimal reproducer — ideally the input file or URL, or a crashing fuzz case,
- the impact you observed (crash / hang / out-of-bounds read / memory growth / RCE).

We aim to acknowledge within **5 business days** and to agree on a disclosure
timeline from there. We will credit reporters who want it.

## Scope

In scope — the things that touch untrusted input:

- **Demuxers and decoders** — panics, hangs, unbounded allocation, or any memory
  unsafety on malformed input. (We treat a panic on hostile input as a bug; see the
  always-on `crates/rff/tests/demuxer_fuzz.rs` + `crates/rff/tests/fuzz_robustness.rs`
  and the coverage-guided `crates/rff/fuzz/` targets.)
- **`rff-io`** — the HTTP/HTTPS client against hostile servers (oversized
  responses, redirect loops, slow-loris, TLS issues).
- **`rff-server`** — authentication/authorization bypass, path traversal.

Out of scope:

- The optional `rff-ui` front-end (excluded from the published binaries).
- The opt-in C `openh264` path (`--features h264-openh264`); the default build is
  pure Rust.
- Denial of service that requires a genuinely enormous but well-formed input
  (i.e. "decoding a 100 GB file uses a lot of RAM").

## Known robustness caveats

- **AVIF decode (`rav1d`).** The AVIF decoder runs the external
  [`rav1d`](https://github.com/memorysafety/rav1d) AV1 decoder. On malformed AV1,
  rav1d's `validate_input!` calls `debug_abort()` → `std::process::abort()` — but
  **only under `cfg(debug_assertions)`** (debug builds), which `catch_unwind` cannot
  contain. **Release builds return `Err` and are unaffected** (verified by a release
  fuzz run). We pre-validate the AV1 sample at our boundary (`crates/rff-codec-avif`),
  but if you decode untrusted AVIF in a *debug* build or CI, sandbox that path. It is a
  controlled abort (availability only), not memory unsafety — no RCE.

## Supported versions

Until 1.0, only the **latest `main` / most recent release** receives security
fixes. There is no back-porting to older `0.0.x` tags.
