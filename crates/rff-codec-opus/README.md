# rff-codec-opus

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **Opus** codec adapter for **remade_ffmpeg_rs**, backed by our own
[`rusty-opus`](https://crates.io/crates/rusty-opus) — a BSD-3-Clause performance
fork of the pure-Rust `opus-rs` port of libopus. No C, no FFI. Registers `opus`
for both decode and encode.

- **Faster than `libopus` per core, on speech *and* music** — 1.0–1.5× single-thread (fair, one thread each), via three **byte-identical AVX2 SILK kernels** (LPC short-prediction, warped autocorrelation, and the cross-state NSQ shaping filter) plus an O(n²)-copy fix in the encode wrapper.
- **Frame-parallel encoding** takes wall-clock to **2–4× faster** than libopus, which is single-threaded per stream. Chunked and state-primed, gated **PEAQ-neutral** (ΔODG ≤ 0.03).
- **FFmpeg decodes our output at unity.** The decoder passes all 12 official RFC 6716/8251 test vectors.
- Knobs: `-b:a`, `-compression_level`, `-opus_parallel`.
- Opus is royalty-free.

## Usage

```rust
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error};

fn main() -> Result<(), Error> {
    let mut codecs = CodecRegistry::new();
    rff_codec_opus::register(&mut codecs);

    // Now reachable by id or by FFmpeg-style name.
    let _decoder = codecs.find_decoder(CodecId::Opus)?;
    let codec = codecs.by_id(CodecId::Opus).expect("just registered");
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
