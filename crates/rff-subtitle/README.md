# rff-subtitle

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

Shared text-subtitle helpers for **remade_ffmpeg_rs** — cue timing parse and
format, and a minimal `Cue` type. Used by the SubRip and WebVTT containers,
which differ mostly in their timestamp separator.

- **`parse_timestamp` / `format_timestamp`** — `HH:MM:SS,mmm` (SubRip) and `HH:MM:SS.mmm` (WebVTT) from one implementation, parameterised by the separator.
- **`parse_cue_timing`** — the `start --> end` line, returning milliseconds.
- **`Cue`** — start, end and text, plus `parse_cues` for a whole document.
- Zero dependencies beyond [`rff-core`](https://crates.io/crates/rff-core).

## Usage

```rust
fn main() {
    // SubRip uses ',' before the milliseconds; WebVTT uses '.'.
    assert_eq!(rff_subtitle::format_timestamp(3_661_500, ','), "01:01:01,500");
    assert_eq!(rff_subtitle::format_timestamp(3_661_500, '.'), "01:01:01.500");

    let (start, end) = rff_subtitle::parse_cue_timing("00:00:01,000 --> 00:00:04,000")
        .expect("well-formed timing line");
    println!("cue runs {start} ms .. {end} ms");

    for cue in rff_subtitle::parse_cues("1\n00:00:01,000 --> 00:00:04,000\nHello.\n") {
        println!("[{}..{}] {}", cue.start_ms, cue.end_ms, cue.text);
    }
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
