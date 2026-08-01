# rff-cli

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The command-line front-ends of **remade_ffmpeg_rs** — a ground-up,
permissively-licensed Rust rebuild of FFmpeg. Installs `rff` and `rffprobe`,
which speak the FFmpeg flags you already know.

- **Familiar flags** — `-i`, `-c:v`, `-c:a`, `-b:v`, `-b:a`, `-q:a`, `-crf`, `-f`, `-y`/`-n`, `-map`, `-vf`, `-filter_complex`, `-frames:v`, `-codecs`, `-formats`.
- **Pure Rust on the default path** — no C, no FFI, no copyleft. Apache-2.0.
- **Optional drop-in names** — `--features drop-in-names` additionally installs executables named `ffmpeg` and `ffprobe`. Off by default, because installing them shadows a real FFmpeg on your `PATH` and that should be your explicit choice.
- **`--features https`** adds `https://` streaming input on a pure-Rust rustls stack.
- A thin shell over the [`rff`](https://crates.io/crates/rff) engine — anything the CLI does is available as a library call.

## Install

```sh
cargo install rff-cli

# List what this build supports — just like FFmpeg:
rff -codecs
rff -formats

# Inspect a file:
rffprobe input.mp4

# Encode audio with an in-house, pure-Rust encoder:
rff -i input.wav -c:a aac -b:a 128k -y output.m4a
rff -i input.wav -c:a flac -y output.flac
rff -i input.wav -c:a vorbis -q:a 4 -y output.ogg

# Transcode video:
rff -i input.mp4 -c:v vp9 -crf 32 -y output.webm
```

Want the drop-in `ffmpeg`/`ffprobe` names so existing scripts work unchanged?

```sh
cargo install rff-cli --features drop-in-names
```

> **Build prerequisite — `nasm`.** The default build enables `h264-asm`
> (`rusty_h264`'s hand-written SIMD kernels), which assembles with
> [`nasm`](https://nasm.us). Install it (`winget install NASM` /
> `brew install nasm` / `apt install nasm`), or use
> `--no-default-features` for the pure-Rust scalar H.264 path.

**Trademark.** This is an independent, clean-room reimplementation, **not
affiliated with, endorsed by, or derived from the source code of the FFmpeg
project**. "FFmpeg" is a trademark of Fabrice Bellard. The optional
`ffmpeg`/`ffprobe` executable names exist solely for command-line compatibility;
the product is **remade_ffmpeg_rs**.

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
