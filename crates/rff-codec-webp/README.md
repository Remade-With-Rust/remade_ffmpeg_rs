# rff-codec-webp

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **WebP** codec adapter for **remade_ffmpeg_rs**, backed by our own
pure-Rust [`rusty_webp`](https://crates.io/crates/rusty_webp) (a
performance/feature fork of image-rs/image-webp 0.2.4).

- **Decode** VP8 (lossy), VP8L (lossless), and **animated WebP** (all frames,
  millisecond pts). Still lossy images without alpha decode straight to their
  native YUV 4:2:0 planes — video pipelines skip the RGB round-trip, and the
  output is **bit-exact with FFmpeg's** webp decoder.
- **Encode** lossless (VP8L) with LZ77 backward references, color cache, and
  per-tile predictor selection.
- MIT/Apache-2.0 backing crate; WebP is royalty-free.

Measured vs FFmpeg 8.1.2 (pinned CPU time, ABBA-interleaved, real-content
corpus, decoded output verified bit-exact): lossy decode **1.24–1.29× faster**,
lossless decode **~2.15× faster**, lossless encode **1.6–2.8× faster** than
libwebp's default effort at +4–13% size on photos (and up to 39% *smaller* on
screen content). FFmpeg's native decoder errors on animated WebP; this crate
decodes every frame. Lossy encoding is not implemented yet.

## Usage

```rust
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error};

fn main() -> Result<(), Error> {
    let mut codecs = CodecRegistry::new();
    rff_codec_webp::register(&mut codecs);

    // Now reachable by id or by FFmpeg-style name.
    let _decoder = codecs.find_decoder(CodecId::Webp)?;
    let codec = codecs.by_id(CodecId::Webp).expect("just registered");
    println!("{} — decode: {}, encode: {}", codec.long_name, codec.can_decode(), codec.can_encode());
    Ok(())
}
```

## Part of Remade With Rust

This crate is one layer of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Most users want
the [`remade-ffmpeg`](https://crates.io/crates/remade-ffmpeg) engine facade or the
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
