# rff-codec-aac

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **AAC-LC** codec adapter for **remade_ffmpeg_rs**, backed by the in-house
pure-Rust [`rusty_aac`](https://crates.io/crates/rusty_aac) crate — no C, no
FFI. Registers `aac` for both decode and encode.

- **Decoder** with the full AAC-LC feature set — short blocks, M/S and intensity stereo, PNS, TNS — **bit-exact against FFmpeg**.
- **Encoder** with a Bark-scale psychoacoustic model, bitrate rate control, transient block switching, M/S stereo and MP4 `esds` config. **FFmpeg decodes our output at unity.**
- **~450× realtime — roughly 6× faster than FFmpeg's own AAC encoder** (FFmpeg's is single-threaded), via frame-parallel encoding, an N/4-point-FFT MDCT, a two-phase rate loop, cached psychoacoustic tables and AVX2 (+ opt-in AVX-512) quantize kernels. Single-thread it still edges FFmpeg (~1.15×).
- **Patent note:** AAC is patent-relevant. This is the largely-expired AAC-LC corner (no HE-AAC); no patent licence is granted or implied. See the [patents section](https://github.com/Remade-With-Rust/remade_ffmpeg_rs#patents).

## Usage

```rust
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error};

fn main() -> Result<(), Error> {
    let mut codecs = CodecRegistry::new();
    rff_codec_aac::register(&mut codecs);

    // Now reachable by id or by FFmpeg-style name.
    let _decoder = codecs.find_decoder(CodecId::Aac)?;
    let codec = codecs.by_id(CodecId::Aac).expect("just registered");
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
