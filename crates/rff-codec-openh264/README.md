# rff-codec-openh264

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A **TEMPORARY, opt-in** H.264 codec adapter for **remade_ffmpeg_rs** backed by
Cisco's C [openh264](https://github.com/cisco/openh264) library via
[`openh264`](https://crates.io/crates/openh264)/`openh264-sys2`.

> **⚠ This is the project's only C/FFI crate, and it is NOT pure Rust.**
> Everything else in remade_ffmpeg_rs is pure Rust with no C dependency. You
> almost certainly want [`rff-codec-h264`](https://crates.io/crates/rff-codec-h264)
> instead — the in-house pure-Rust path, which is the default.

- **Why it exists:** a cross-check and fallback against the pure-Rust H.264
  codec, useful when bringing up or debugging the in-house implementation.
- **Off by default.** It is reachable only through `rff`'s `h264-openh264`
  feature; a default build never compiles it and never needs a C toolchain.
- **Why it is published at all:** `rff` declares it as an *optional* dependency,
  and crates.io must be able to resolve optional dependencies — an unpublished
  one would make `rff` itself unpublishable.
- **Build requirement:** a working C toolchain, since `openh264-sys2` compiles
  Cisco's C sources.
- **Patent note:** H.264/AVC is patent-relevant, and Cisco's binary-licence
  arrangement for openh264 does **not** transfer when you build from source. No
  patent licence is granted or implied. See the
  [patents section](https://github.com/Remade-With-Rust/remade_ffmpeg_rs#patents).

## Usage

```rust
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error};

fn main() -> Result<(), Error> {
    let mut codecs = CodecRegistry::new();
    rff_codec_openh264::register(&mut codecs);

    let _decoder = codecs.find_decoder(CodecId::H264)?;
    Ok(())
}
```

Through the engine, enable the feature instead of calling `register` yourself:

```sh
cargo build -p rff-cli --features h264-openh264
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
