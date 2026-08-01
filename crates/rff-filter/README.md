# rff-filter

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

Video frame filters for **remade_ffmpeg_rs** — FFmpeg's `libavfilter`, scoped
to the graph shapes the CLI actually exposes: a linear `-vf` chain and an
`overlay` composite for `-filter_complex`.

- **`FilterChain::parse`** — parses FFmpeg's `-vf` syntax (`scale=320:240,crop=w:h:x:y`) into a chain you can `apply` to each decoded `VideoFrame`.
- **`scale`** and **`crop`** filters today; the `Filter` trait is the extension point for more.
- **`output_dims`** — resolve the post-chain frame size without decoding anything, which is what the encoder needs at open time.
- **`overlay`** — composite one frame over another at `(x, y)`, backing `-filter_complex` overlay.
- Empty spec = pass-through, so callers don't special-case "no filters".

## Usage

```rust
use rff_filter::FilterChain;
use rff_core::{Error, PixelFormat, VideoFrame};

fn main() -> Result<(), Error> {
    // Exactly the string `ffmpeg -vf` takes.
    let mut chain = FilterChain::parse("scale=320:240,crop=160:120:80:60")?;

    // Resolve the output size before opening the encoder.
    let (w, h) = chain.output_dims(1920, 1080);
    println!("encoder should be opened at {w}x{h}");

    let frame = VideoFrame {
        width: 1920,
        height: 1080,
        format: PixelFormat::Yuv420p,
        planes: vec![vec![0u8; 1920 * 1080], vec![0u8; 960 * 540], vec![0u8; 960 * 540]],
        strides: vec![1920, 960, 960],
        pts: Some(0),
    };
    let _filtered = chain.apply(frame)?;
    Ok(())
}
```

## Part of Remade With Rust

This crate is one layer of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Most users want
the [`rff`](https://crates.io/crates/rff) engine facade or the
[`rff-cli`](https://crates.io/crates/rff-cli) binaries rather than this crate
directly.

Also check out our sister project
**[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for an AI-first
world — and the rest of
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**, including
the standalone codec crates
[`rusty_h264`](https://crates.io/crates/rusty_h264),
[`rusty_vp9`](https://crates.io/crates/rusty_vp9),
[`rusty_mp3`](https://crates.io/crates/rusty_mp3),
[`rusty_aac`](https://crates.io/crates/rusty_aac),
[`rusty-opus`](https://crates.io/crates/rusty-opus),
[`rusty_vorbis`](https://crates.io/crates/rusty_vorbis), and the
[rusty-av1-toolkit](https://github.com/Remade-With-Rust/rusty-av1-toolkit) forks.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## License

Apache-2.0. See the workspace
[LICENSE](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE).
