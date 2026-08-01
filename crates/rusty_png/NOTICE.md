# NOTICE — attribution for `rusty_png`

`rusty_png` is a **performance fork** of the pure-Rust
[`png`](https://crates.io/crates/png) crate by the image-rs project, vendored
in-tree and carried forward here.

- **Upstream:** <https://github.com/image-rs/image-png>
- **Forked from:** `png` **0.17.16**
- **Upstream authors:** The image-rs Developers
- **Upstream license:** MIT OR Apache-2.0 — unchanged in this fork.

The original `LICENSE-MIT` and `LICENSE-APACHE` are preserved verbatim
alongside this file, as is the upstream changelog (`UPSTREAM-CHANGES.md`) and
the upstream README (`UPSTREAM-README.md`). This fork is **not** endorsed by or
affiliated with the image-rs project.

This is a reimplementation-by-derivation, not a clean-room rewrite: the code
here descends directly from image-png and remains under image-png's dual
MIT/Apache-2.0 terms. Both are compatible with this workspace's Apache-2.0
licence and carry no copyleft, which is what the `cargo-deny` gate enforces.

## Why the fork exists

Benchmarking `rff-codec-png` against FFmpeg's PNG codec (single core, pinned,
ABBA-interleaved, on-core cycles; corpus of real Derf/xiph frames plus real
screenshots/charts/logos) found two things that upstream cannot address for us:

1. **At a matched output size the encoder is 2.6–4.4× slower than ffmpeg's.**
   On an 8.3 MPx frame, ffmpeg's default (paeth + zlib L6) took 1,472 Mcyc for
   14,837,375 B; this crate's `default/up` took 3,827 Mcyc for 13,910,262 B and
   `best/up` 6,543 Mcyc for 13,647,420 B. We produce 6.2–8.0% smaller files and
   pay 2.6–4.4× the CPU. PNG filtering is a minor share of that — the DEFLATE
   stream is the hot spot, and upstream routes it through `miniz_oxide`.

2. **The one operating point upstream defaults to is not the one we want.**
   `Compression::Fast` + `FilterType::Sub` + non-adaptive filtering is a strong
   point on photographic content — faster *and* smaller than every ffmpeg
   `-compression_level 1` configuration — and a poor one on graphics, where it
   ran +654% against ffmpeg's default. The right default is content-dependent.

See `WHYS.md` for the measured descent behind both claims.

## Changes from upstream

Every entry must state the gate it passed. Nothing lands here on reasoning alone.

| # | change | gate | status |
|---|---|---|---|
| 0 | vendored `png` 0.17.16 verbatim; renamed lib to `rusty_png`; edition 2018 → 2021 | byte-identical PNG output vs the registry crate on the full benchmark corpus | baseline |
