# rff

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The engine facade of **remade_ffmpeg_rs** — a ground-up, permissively-licensed
Rust rebuild of FFmpeg. This crate wires every built-in codec and container into
one `Engine` and exposes the high-level **transcode** and **probe** API that the
`ffmpeg`/`ffprobe` CLI and the HTTP server are thin shells over.

**This is the crate to depend on if you want to embed the toolkit in an
application.** Apache-2.0, no copyleft anywhere in the dependency tree
(CI-enforced with `cargo-deny`), and no C/FFI on the default path.

- **`Engine::new()`** — every built-in codec and container registered, ready to use. Its `codecs`/`formats` registries are public, which is how `ffmpeg -codecs` and `-formats` are implemented.
- **`transcode::run`** — a declarative `TranscodeSpec`: inputs, outputs, per-stream codec + options, `-map` stream selection, `-vf` filter graphs, `-filter_complex` overlay, `-frames:v` limits, overwrite policy.
- **`probe::probe`** — container and per-stream metadata for a file, the `ffprobe` core.
- **Video**: VP9, H.264/AVC, AV1, AVIF, PNG, MJPEG, GIF, WebP, JPEG XL (decode), raw video.
- **Audio**: Opus, AAC-LC, MP3, Vorbis, FLAC, PCM — several with in-house encoders that beat their C counterparts (see the [benchmarks](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/docs/benchmarks.md)).
- **Containers**: MP4/MOV, Matroska/WebM, AVI, MPEG-TS, FLV, HLS output, Ogg, WAV, FLAC, MP3, IVF, Y4M, AVIF, and the single-image wrappers.
- **Pre-1.0 and not yet independently audited.** APIs and codec coverage are still moving.

## Usage

```rust
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;
use rff_core::{CodecId, Dictionary, Error};

fn main() -> Result<(), Error> {
    let engine = Engine::new();

    // What `ffmpeg -i input.wav -c:a aac -b:a 128k -y output.m4a` builds.
    let mut options = Dictionary::default();
    options.set("b", "128k");

    let spec = TranscodeSpec {
        inputs: vec![InputSpec { path: "input.wav".into(), format: None }],
        outputs: vec![OutputSpec {
            path: "output.m4a".into(),
            audio_codec: Some(StreamCodec { codec: CodecId::Aac, options }),
            overwrite: true,
            ..Default::default()
        }],
    };

    let report = rff::transcode::run(&engine, &spec)?;
    println!("{} packets written", report.packets_written);

    // And the ffprobe side:
    let info = rff::probe::probe(&engine, "output.m4a")?;
    println!("{} — {} stream(s)", info.format_name, info.streams.len());
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
