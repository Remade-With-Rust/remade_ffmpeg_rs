# rusty_webp

[![crates.io](https://img.shields.io/crates/v/rusty_webp?logo=rust)](https://crates.io/crates/rusty_webp)
[![docs.rs](https://img.shields.io/docsrs/rusty_webp?logo=docsdotrs)](https://docs.rs/rusty_webp)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/Remade-With-Rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)

> **The pure-Rust WebP codec.** VP8 (lossy) + VP8L (lossless) + animated WebP
> decoding, and a lossless VP8L encoder with real compression machinery — a
> performance/feature fork of
> [image-rs/image-webp](https://github.com/image-rs/image-webp) that decodes
> lossy WebP **faster than FFmpeg, bit-exact**, with no C, no FFI, and
> `#![forbid(unsafe_code)]`.

**Most users want the facade —
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)'s
`rff` CLI or the [`rff-codec-webp`](https://crates.io/crates/rff-codec-webp)
adapter.** Depend on this crate directly to encode or decode WebP
programmatically without the engine around it.

Part of **[Remade With Rust](https://github.com/Remade-With-Rust)** by
**[Mata Network](https://www.mata.network/)**.

---

## Measured against FFmpeg

These ratios were measured on 0.1. 0.2 is faster again on every axis below
(see [What changed in 0.2](#what-changed-in-02)) with the same decoded
pixels and the same encoded bytes; they have not been re-measured against
FFmpeg since.

FFmpeg 8.1.2 (libwebp encoder, native VP8/VP8L decoder), pinned single core,
child **CPU time**, ABBA-interleaved pairs, N = 21–31, null-arm floor ≤ 5%,
real-content corpus (Derf video frames 1080p–8K, screen captures, alpha, odd
sizes). Decoded output verified **bit-exact against FFmpeg on every corpus
image** — the speed never buys a different picture.

| Axis | vs FFmpeg |
|---|---|
| Lossy (VP8) decode, 8K | **1.24–1.29× faster**, bit-exact |
| Lossless (VP8L) decode, 8K | **~2.15× faster** |
| Animated WebP decode | **all frames** — FFmpeg's native decoder errors on animated files |
| Lossless encode, speed | **1.6–2.8× faster** than libwebp's default effort (m4) |
| Lossless encode, photos | +4…13% vs libwebp m4 — **smaller than libwebp m0 (its fastest) everywhere** |
| Lossless encode, screen / repeated content | up to **39% smaller** than libwebp m4 |

Where the decode speed comes from — each brick gated **bit-exact** against the
scalar originals, which stay in-tree as oracles: batched branch-free 16-lane
loop-filter kernels (the stage was 49% of decode; ~6× faster), a block-scoped
fast session in the boolean decoder (one state snapshot + commit per
coefficient block instead of per read), and a DC-only IDCT path.

Where the compression comes from: greedy hash-chain LZ77 backward references,
a color cache sized per image by entropy estimate, and per-tile predictor
selection (14 modes, 16-px tiles) behind a run-rate content dispatch that
skips the search where LZ77 already models the redundancy.

## What changed in 0.2

An instruction-count campaign over the whole crate: 45 changes, each
measured on its own with callgrind and gated on output. Still images decode
**bit-exact against libwebp 1.5.0** before and after on a libwebp-encoded
corpus (lossy at three qualities, lossless photo / screen / palette, alpha),
and the lossless encoder writes **byte-identical files**. Still no `unsafe`.

0.1 against 0.2, one pinned core, interleaved pairs (0.2 won 10 of 10 on
every row):

| Workload | Instructions | Wall clock |
|---|---:|---:|
| Lossy decode, 1080p, q40 / q75 / q92 | −42% / −39% / −36% | **1.31–1.36× faster** |
| Lossy + alpha | −45% | **1.41×** |
| Lossy thumbnail (160×128) | −38% | **1.38×** |
| Lossless decode, photo / screen / code | −19% / −32% / −28% | **1.12–1.16×** |
| Animated, lossy / lossless | −47% / −43% | **1.54× / 1.52×** |
| Lossless encode, photo | −69% | **2.1–2.2×** |
| Lossless encode, screen / code | −41% / −43% | **1.30–1.34×** |

Timings are whole-process (file read and an output hash included), so the
codec's own gain is a little larger than the wall column shows.

Where it came from:

- **Lossy decode.** The loop filter loaded and stored its taps one
  bounds-checked pixel at a time (31% of a 1080p decode); they are slices
  now. Coefficient tokens are read as straight-line code instead of a
  generic tree walk, with a branchless sign read, a table-driven
  renormalisation and whole-macroblock decoder sessions. YUV→RGB runs 16
  pixels at a time in 16-bit lanes, and a three-coefficient inverse
  transform joins the DC-only one.
- **Lossless decode.** Colour-transform blocks whose multipliers are zero
  (most of them in libwebp's output) are skipped, the colour-cache key is
  one byte-swapped load, predictors run over runs of blocks, and RGB
  output is converted 32 pixels at a time.
- **Alpha and animation.** The alpha plane is unfiltered a row at a time,
  straight from the decoded pixels; frames blend row by row and reuse
  their buffers.
- **Lossless encode.** The predictor search costs each mode over a tile row
  of bytes instead of calling a predictor per pixel per mode; the five
  colour-cache candidates share one histogram pass.

**Behaviour change (a fix):** in animated WebP, fully opaque pixels of
alpha-blended frames decoded one level too dark in red, green and blue.
0.2 copies opaque pixels as libwebp does, and both animations in the
corpus now match libwebp's frame-by-frame output exactly. Still images are
unaffected.

## What's in the box

| Capability | Status |
|---|---|
| VP8 (lossy) decode | ✅ — plus native YUV 4:2:0 output (`read_yuv420`) so video pipelines skip the RGB round-trip; fancy-bilinear (dwebp-default) or simple chroma upsampling for RGB output |
| VP8L (lossless) decode | ✅ |
| Alpha (ALPH), ICC / EXIF / XMP | ✅ |
| Animated WebP decode | ✅ — composited canvas frames with per-frame durations |
| VP8L (lossless) encode | ✅ — LZ77 + color cache + per-tile predictors; a runs-only fastest tier stays reachable via `EncoderParams` |
| VP8 (lossy) encode | ❌ not yet |

## Features

| Feature | Default | Effect |
|---|:---:|---|
| `profile` | — | Per-stage decode timing printed to stderr (entropy / predict+IDCT / loop filter). Measurement builds only — never ship it. |

## Install

```
cargo add rusty_webp
```

```rust
use std::io::Cursor;
use rusty_webp::{ColorType, WebPDecoder, WebPEncoder};

// Decode: RGB(A) for image use, or native YUV 4:2:0 for video pipelines.
let mut decoder = WebPDecoder::new(Cursor::new(&webp_bytes))?;
if let Some(yuv) = decoder.read_yuv420()? {
    // Still lossy image: BT.601 limited-range planes, zero-copy.
    let (y, u, v) = (yuv.y, yuv.u, yuv.v);
} else {
    let mut rgb = vec![0; decoder.output_buffer_size().unwrap()];
    decoder.read_image(&mut rgb)?;
}

// Encode (lossless VP8L).
let mut out = Vec::new();
WebPEncoder::new(&mut out).encode(&rgb_pixels, width, height, ColorType::Rgb8)?;
```

## Where this sits

| Crate | Role |
|---|---|
| [`rusty_webp`](https://crates.io/crates/rusty_webp) | ← you are here — the codec: VP8/VP8L decode, VP8L encode |
| [`rff-codec-webp`](https://crates.io/crates/rff-codec-webp) | the engine adapter (frames ↔ packets) |
| [`rff-format-webp`](https://crates.io/crates/rff-format-webp) | the container (de)muxer |
| [`rff`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs) | the full engine + drop-in `ffmpeg`/`ffprobe` CLI |

## The Remade With Rust ecosystem

**Remade With Rust** is an initiative by [Mata Network](https://www.mata.network/)
to rebuild essential C and C++ tools in Rust — for the memory safety, the
predictable performance, and the freedom of a permissive license. Each project
is a reimplementation, not a fork: same wire protocols and file formats, new
code you can actually depend on. No copyleft. No surprises.

| Project | What it is |
|---|---|
| 🎬 [remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs) | Our FFmpeg alternative. Drop-in `ffmpeg` and `ffprobe` binaries — demux → decode → filter → encode → mux, rebuilt as composable Rust crates with zero GPL/LGPL. Apache-2.0. `rusty_webp` is its WebP codec. |
| 🧠 [FFAI](https://github.com/Remade-With-Rust/FFAI) | Our sister project: media for AI. "The AI media toolkit, remade with rust." Embedded ASR + TTS (Mercury), OCR (Carmenta) and vision-language captioning (Argus) behind an ffmpeg-style, swap-by-name architecture — no Python, no CUDA. MIT OR Apache-2.0. |
| 🌐 [Mata Network](https://www.mata.network/) | The home page. "Stop sacrificing your privacy for convenience." Sovereign, self-hostable privacy infrastructure — wallet & identity, password manager, contact manager, and a browser extension that stops information leaking as you browse. Remade With Rust is its open-source arm. |

→ All projects: [github.com/Remade-With-Rust](https://github.com/Remade-With-Rust)

## License

MIT OR Apache-2.0, same as upstream — see `LICENSE-MIT` / `LICENSE-APACHE`,
and [NOTICE.md](NOTICE.md) for attribution to
[image-rs/image-webp](https://github.com/image-rs/image-webp) v0.2.4, the
project this crate forks.
