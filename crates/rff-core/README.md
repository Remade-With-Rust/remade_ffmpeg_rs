# rff-core

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The dependency-free core of **remade_ffmpeg_rs** — the media primitives every
other layer is written against: frames, packets, codec/pixel/sample enums,
rationals, dictionaries and the shared error type. FFmpeg's `libavutil`, in safe
Rust with a single dependency (`thiserror`).

- **Frames** — `VideoFrame` (planar/packed pixels, `PixelFormat`), `AudioFrame` (interleaved `f32`, `SampleFormat`), unified by the `Frame` enum.
- **Packets** — `Packet` with PTS/DTS, duration, stream index and `PacketFlags` (keyframe et al.).
- **Identifiers** — `CodecId`, `MediaType`, `PixelFormat`, `SampleFormat`.
- **Plumbing** — `Rational` timebases, `Dictionary` for FFmpeg-style `-key value` options, and `Error`/`Result` with the `Again`/`Eof` drain protocol every codec follows.
- No I/O, no codecs, no allocator tricks — just the vocabulary types, so every downstream crate agrees on what a frame is.

## Usage

```rust
use rff_core::{AudioFrame, CodecId, Error, MediaType, Packet, Rational, SampleFormat};

fn main() -> Result<(), Error> {
    // A 20 ms stereo frame at 48 kHz, interleaved f32.
    // `samples` counts samples PER CHANNEL; `planes` holds the raw bytes —
    // one plane for interleaved formats, one per channel for planar ones.
    let frame = AudioFrame {
        sample_rate: 48_000,
        channels: 2,
        format: SampleFormat::F32,
        planes: vec![vec![0u8; 960 * 2 * 4]],
        samples: 960,
        pts: Some(0),
    };
    println!("{} samples/ch, {} ch", frame.samples, frame.channels);

    // Timebases are exact rationals, never floats.
    let tb = Rational::new(1, 48_000);
    println!("timebase {}/{}", tb.num, tb.den);

    assert_eq!(CodecId::Opus.media_type(), MediaType::Audio);
    let _pkt = Packet::default();
    Ok(())
}
```

`Error::Again` means "feed more input" and `Error::Eof` means "flushed and fully
drained" — the same EAGAIN/EOF contract FFmpeg uses, and the one every
`rff-codec-*` and `rff-format-*` crate implements.

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
