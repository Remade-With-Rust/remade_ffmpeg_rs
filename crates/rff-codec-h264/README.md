# rff-codec-h264

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **H.264 / AVC** video codec adapter for **remade_ffmpeg_rs**, backed by
[`rusty_h264`](https://crates.io/crates/rusty_h264) — pure Rust end to end.
Registers `h264` for both decode and encode.

- **Pure Rust, no C/FFI, no assembler** — `rusty_h264` 0.12's SIMD is portable Rust (SSE2/AVX2/NEON), on with the `asm` feature (`h264-asm` on the facade); `--no-default-features` is the scalar path.
- **`-preset fast|medium|slow`** and **`-profile baseline|main|high`**. `-profile baseline` is Constrained Baseline with CAVLC, one reference, no B-frames, no lookahead and no scene cut — the configuration a `rusty_esp_video` device runs — so `rff -i in.y4m -c:v h264 -profile baseline -preset fast -g 30 -b:v 500k -qp 28 out.264` is the host oracle for a chip's stream. `-g`, `-b:v` and `-qp` map straight onto the encoder.
- The default configuration runs `rusty_h264`'s lookahead (mb-tree over a GOP); the adapter turns its batched output into one packet per frame with the right `pts`. Baseline is unbuffered.
- **Patent note:** H.264/AVC is patent-relevant. No patent licence is granted or implied; royalties are the responsibility of whoever distributes or commercially deploys a product incorporating it. See the [patents section](https://github.com/Remade-With-Rust/remade_ffmpeg_rs#patents).
- A C/FFI alternative (Cisco openh264) exists in-tree as `rff-codec-openh264` but is **deliberately unpublished** and off by default — this crate is the pure-Rust path.

## Usage

```rust
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error};

fn main() -> Result<(), Error> {
    let mut codecs = CodecRegistry::new();
    rff_codec_h264::register(&mut codecs);

    // Now reachable by id or by FFmpeg-style name.
    let _decoder = codecs.find_decoder(CodecId::H264)?;
    let codec = codecs.by_id(CodecId::H264).expect("just registered");
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
