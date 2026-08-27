# rff-format

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The container abstraction layer of **remade_ffmpeg_rs** — the `Demuxer` and
`Muxer` traits, the `Stream` descriptor and the `FormatRegistry` that every
container crate registers into. FFmpeg's `libavformat` core, minus the
containers themselves.

- **`Demuxer` / `Muxer` traits** — read `Packet`s out of a byte stream, or write them into one; generic over any `Read + Send` / `Write + Send`.
- **`FormatRegistry`** — resolve a container by name, by file extension, or by **content probe** (`ProbeFn` scoring, the same idea as FFmpeg's probe scores).
- **`Stream`** — per-stream descriptor: index, `CodecId`, timebase, and codec parameters.
- **`Format`** — the registration record with optional demuxer/muxer factories, so a container can be read-only (e.g. Matroska today).
- Adding a container is one `register(&mut registry)` call — no engine-core changes.

## Usage

```rust
use rff_format::FormatRegistry;
use rff_core::Error;

fn main() -> Result<(), Error> {
    let mut formats = FormatRegistry::new();

    // Each rff-format-* crate contributes one line like this.
    rff_format_wav::register(&mut formats);

    // Resolve by name, by extension, or by sniffing the first bytes.
    assert!(formats.by_name("wav").is_some());
    assert!(formats.by_extension("wav").is_some());

    let file = std::fs::File::open("input.wav").expect("open input");
    let mut demuxer = formats.open_demuxer("wav", Box::new(file))?;
    for stream in demuxer.read_header()? {
        println!("stream {}: {:?}", stream.index, stream.codec_id);
    }
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
