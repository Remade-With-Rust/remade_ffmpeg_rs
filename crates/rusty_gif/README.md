# rusty_gif

[![crates.io](https://img.shields.io/crates/v/rusty_gif?logo=rust)](https://crates.io/crates/rusty_gif) [![docs.rs](https://img.shields.io/docsrs/rusty_gif?logo=docsdotrs)](https://docs.rs/rusty_gif) [![CI](https://github.com/remade-with-rust/rusty_gif/actions/workflows/ci.yml/badge.svg)](https://github.com/remade-with-rust/rusty_gif/actions/workflows/ci.yml) [![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/remade-with-rust/rusty_gif/blob/main/LICENSE-MIT) [![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/Remade-With-Rust) [![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network/)

> "Cost should scale with color diversity, not resolution." A pure-Rust GIF
> decoder + encoder whose quantizer routes by a color census — exact palette,
> frequency-weighted k-means, or budget-sampled NeuQuant, chosen per frame.
> Measured against FFmpeg 8.1.2's `palettegen`/`paletteuse` best path:
> **1.5–2.5× faster encode at higher PSNR on 8/8 benchmark frames**, decode
> up to 2.5× faster. `#![forbid(unsafe_code)]`.

**Encoding video or images end-to-end? You want the facade —
[`rff`](https://crates.io/crates/rff) / [`rff-cli`](https://crates.io/crates/rff-cli)**
(drop-in `ffmpeg`-style pipeline). Depend on `rusty_gif` directly when you are
working with GIF itself — decoding frames, building palettes, or writing GIFs
from raw pixels.

Part of **[Remade With Rust](https://github.com/Remade-With-Rust)** by **[Mata Network](https://www.mata.network/)**.

---

## Why it's `forbid(unsafe_code)`

A GIF decoder parses LZW off hostile input; a GIF encoder walks every pixel of
untrusted dimensions. That is exactly where the C implementations grew their
CVEs, so this crate is `#![forbid(unsafe_code)]` end to end — the LZW layer is
the pure-Rust [`weezl`](https://crates.io/crates/weezl), and every kernel in
the quantizer is safe integer/float Rust that the compiler auto-vectorizes.

## Content-routed quantization

`Frame::from_rgb`/`from_rgba` price palette building by the frame's **color
census**, not its pixel count (real frames measured 2–88× unique-color
redundancy):

| Route | Fires when | What runs |
|-------|-----------|-----------|
| **Exact** | ≤256 unique colors | Exact palette — lossless, no quantizer |
| **Histogram k-means** | ≤65,536 unique | Median-cut init + frequency-weighted k-means over the *unique colors* (iterations tier 3/2/1 by diversity) — directly minimizes the frequency-weighted squared error that PSNR measures |
| **Budget NeuQuant** | >65,536 unique | The classic NeuQuant net, trained on a constant 125k-sample budget instead of every pixel |

Every decision is observable and overridable — no silent fallbacks:

| Env var | Effect |
|---------|--------|
| `RUSTY_GIF_TRACE=1` | Print the chosen route + census per frame to stderr |
| `RUSTY_GIF_Q_POLICY` | Force a policy: `auto` (default), `upstream` (pre-fork NeuQuant-on-every-pixel, kept as the byte-identical oracle), `kmeans:<iters>`, `unique:<cap>`, `budget:<pixels>`. Unknown values panic. |

Benchmarked on 8 real-content frames (CIF→1080p, pinned CPU time, interleaved
A/B, 15 rounds) against FFmpeg 8.1.2's high-quality
`palettegen`/`paletteuse dither=none` path: **0.40–0.67× its CPU with PSNR
above it on every frame** (+0.2 to +4.1 dB), 15/15 paired wins per frame.

## Modules

| Item | What's in it |
|------|-------------|
| `Decoder` / `DecodeOptions` | High-level streaming decode — frames out as indexed, RGBA, or raw, with `MemoryLimit` guards |
| `streaming_decoder` | The low-level push decoder underneath (`StreamingDecoder`, per-block `Decoded` events) |
| `Encoder` | Container writing: global/local palettes, frame delays, `Repeat`, extensions |
| `Frame::from_rgb*` / `from_rgba*` | True-color → indexed conversion through the content-routed quantizer above |
| `neuquant` | The vendored NeuQuant net (formerly the `color_quant` crate), public for direct palette work |

## Features

| Feature | Default | Effect |
|---------|---------|--------|
| `color_quant` | ✔ | The quantizer behind `Frame::from_rgb*`/`from_rgba*` (vendored — no external dependency) |
| `std` | ✔ | Std I/O plumbing (`Read`/`Write` encode/decode) |
| `raii_no_panic` | ✔ | Encoder drop never panics on write failure |

## Install

```
cargo add rusty_gif
```

```rust
// Encode: quantize an RGB frame to a single-frame GIF.
let (width, height) = (4u16, 4u16);
let rgb = vec![0u8; width as usize * height as usize * 3];
let frame = rusty_gif::Frame::from_rgb(width, height, &rgb);
let mut out = Vec::new();
{
    let mut encoder = rusty_gif::Encoder::new(&mut out, width, height, &[]).unwrap();
    encoder.write_frame(&frame).unwrap();
}

// Decode it back.
let mut options = rusty_gif::DecodeOptions::new();
options.set_color_output(rusty_gif::ColorOutput::RGBA);
let mut decoder = options.read_info(std::io::Cursor::new(out)).unwrap();
let first = decoder.read_next_frame().unwrap().unwrap();
assert_eq!((first.width, first.height), (width, height));
```

## Where this sits

| Crate | Role |
|-------|------|
| [`rff`](https://crates.io/crates/rff) / [`rff-cli`](https://crates.io/crates/rff-cli) | the ffmpeg-style pipeline — **most users want this** |
| [`rff-codec-gif`](https://crates.io/crates/rff-codec-gif) | the pipeline's GIF codec adapter |
| [`rff-format-gif`](https://crates.io/crates/rff-format-gif) | the pipeline's GIF container layer |
| **[`rusty_gif`](https://crates.io/crates/rusty_gif)** | **← you are here** — the standalone GIF codec |

`rusty_gif` is a performance fork of
[image-rs/image-gif](https://github.com/image-rs/image-gif) with
[`color_quant`](https://github.com/image-rs/color_quant)'s NeuQuant vendored
in-tree — see [NOTICE.md](NOTICE.md) for attribution and
[UPSTREAM-CHANGES.md](UPSTREAM-CHANGES.md) for every change against upstream,
including the measurement gates each one shipped under.

## The Remade With Rust ecosystem

**Remade With Rust** is an initiative by **[Mata Network](https://www.mata.network/)** to rebuild essential C and C++ tools in Rust — for the memory safety, the predictable performance, and the freedom of a permissive license. Each project is a reimplementation, not a fork: same wire protocols and file formats, new code you can actually depend on. No copyleft. No surprises.

| Project | What it is |
|---------|-----------|
| 🎬 **[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** | **Our FFmpeg alternative.** Drop-in `ffmpeg` and `ffprobe` binaries — demux → decode → filter → encode → mux, rebuilt as composable Rust crates with **zero GPL/LGPL**. Apache-2.0. `rusty_gif` is its GIF codec. |
| 🧠 **[FFAI](https://github.com/Remade-With-Rust/FFAI)** | **Our sister project: media _for_ AI.** "The AI media toolkit, remade with rust." Embedded ASR + TTS (**Mercury**), OCR (**Carmenta**) and vision-language captioning (**Argus**) behind an ffmpeg-style, swap-by-name architecture — no Python, no CUDA. MIT OR Apache-2.0. |
| 🌐 **[Mata Network](https://www.mata.network/)** | **The home page.** _"Stop sacrificing your privacy for convenience."_ Sovereign, self-hostable privacy infrastructure — wallet & identity, password manager, contact manager, and a browser extension that stops information leaking as you browse. Remade With Rust is its open-source arm. |

→ All projects: **[github.com/Remade-With-Rust](https://github.com/Remade-With-Rust)**

## License

MIT OR Apache-2.0, matching upstream — see
[LICENSE-MIT](LICENSE-MIT) / [LICENSE-APACHE](LICENSE-APACHE).
`src/neuquant.rs` additionally retains the original Piston Developers and
Anthony Dekker NeuQuant notices verbatim, as its MIT license requires.
