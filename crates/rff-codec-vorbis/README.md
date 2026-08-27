# rff-codec-vorbis

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **Vorbis** codec adapter for **remade_ffmpeg_rs** — decode via the pure-Rust
[`lewton`](https://crates.io/crates/lewton), encode via our in-house
[`rusty_vorbis`](https://crates.io/crates/rusty_vorbis), *the first
permissively-licensed Vorbis encoder in Rust*. No C, no FFI.

- **Encoder** — window → N/4-FFT MDCT → Bark-scale masking floor → channel coupling + point stereo → rate-distortion residue VQ, emitting an embedded libvorbis setup header. Driven with `-q:a 0–9`.
- **FFmpeg decodes our output**, validated packet-exact against `lewton` and libvorbis.
- **~5.3× faster than libvorbis wall-clock** (stereo music, 24 cores) via frame-parallel encoding over a structure-of-arrays + AVX2 residue-VQ search and an energy-bucket class shortlist (PEAQ-validated perceptually neutral, ΔODG ≤ 0.03). Per-thread it is ~1.4× behind libvorbis, down from 4.7×.
- Vorbis is royalty-free.

## Usage

```rust
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error};

fn main() -> Result<(), Error> {
    let mut codecs = CodecRegistry::new();
    rff_codec_vorbis::register(&mut codecs);

    // Now reachable by id or by FFmpeg-style name.
    let _decoder = codecs.find_decoder(CodecId::Vorbis)?;
    let codec = codecs.by_id(CodecId::Vorbis).expect("just registered");
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
