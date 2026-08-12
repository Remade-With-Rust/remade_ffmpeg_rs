# rusty_webp

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)

Pure-Rust WebP codec for **remade_ffmpeg_rs** — a performance/feature fork of
[image-rs/image-webp](https://github.com/image-rs/image-webp) v0.2.4
(MIT OR Apache-2.0, preserved; see [NOTICE.md](NOTICE.md)). No C, no FFI,
`#![forbid(unsafe_code)]`.

- **Decode**: VP8 (lossy) and VP8L (lossless); animated WebP (all frames,
  composited, with per-frame durations); fancy-bilinear or simple chroma
  upsampling; native YUV 4:2:0 output for lossy stills (`read_yuv420`) so
  video pipelines skip the RGB round-trip entirely.
- **Encode**: lossless VP8L with real compression machinery — greedy
  hash-chain LZ77 backward references, color cache (size chosen per image by
  entropy estimate), per-tile predictor selection (14 modes, 16-px tiles)
  with a content dispatch that skips the search where LZ77 already wins.
  A runs-only fast tier remains available via `EncoderParams`.

## Performance

Measured 2026-08-12 against FFmpeg 8.1.2 (libwebp encoder, native VP8/VP8L
decoder) on Windows x86-64: pinned single core, child **CPU time**, arms
ABBA-interleaved, N = 21–31 pairs, null-arm floor ≤ 5%, corpus of real
content (Derf video frames 1080p–8K, screen captures, alpha, odd sizes).
Decoded output is verified **bit-exact** against FFmpeg on every corpus image.

| axis | vs FFmpeg |
|---|---|
| lossy (VP8) decode, 8K | **1.24–1.29× faster**, output bit-exact |
| lossless (VP8L) decode, 8K | **~2.15× faster** |
| animated WebP decode | all frames (FFmpeg's native decoder errors on animations) |
| lossless encode speed | **1.6–2.8× faster** than libwebp default effort (m4) |
| lossless encode size, photos | +4…13% vs libwebp m4; **smaller than libwebp m0 everywhere** |
| lossless encode size, screen/repeated content | up to **39% smaller than libwebp m4** |

The decode wins come from batched branch-free loop-filter kernels, a
block-scoped fast session in the boolean decoder, and a DC-only IDCT path —
each gated bit-exact against the scalar originals, which remain in-tree as
oracles. Lossy (VP8) *encoding* is not implemented yet.

## License

MIT OR Apache-2.0, same as upstream. See `LICENSE-MIT` / `LICENSE-APACHE`
and [NOTICE.md](NOTICE.md) for attribution.

## Part of Remade With Rust

This crate is one layer of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg. Also see the sibling
codec crates [`rusty_h264`](https://crates.io/crates/rusty_h264),
[`rusty_vp9`](https://crates.io/crates/rusty_vp9),
[`rusty_mp3`](https://crates.io/crates/rusty_mp3),
[`rusty_aac`](https://crates.io/crates/rusty_aac), and
[`rusty-opus`](https://crates.io/crates/rusty-opus).
