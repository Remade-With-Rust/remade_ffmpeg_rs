# rff-resample

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

Audio sample-rate conversion for **remade_ffmpeg_rs** — FFmpeg's `swresample`
equivalent: a windowed-sinc polyphase FIR resampler in safe Rust, with **SSE2
kernels gated bit-exact against a kept scalar oracle**.

- **Polyphase FIR** when the reduced rate ratio has a small denominator (the common 44.1↔48 kHz family); a general interpolating path otherwise.
- **SSE2-accelerated** inner FIR with a scalar twin retained as the oracle — `force_scalar_oracle()` switches to it so a test can assert the two agree.
- Weights derived in `f64` for accuracy, stored and applied in `f32` (2× SIMD width, per-tap error ~2⁻²⁴) — gated at **>90 dB SNR** against the `f64` oracle.
- **Streaming** — `process` any chunking you like, `finish` to drain the filter tail.

## Usage

```rust
fn main() {
    // 44.1 kHz -> 48 kHz, stereo interleaved f32.
    let mut r = rff_resample::Resampler::new(44_100, 48_000, 2);

    let input = vec![0.0f32; 4410 * 2]; // 100 ms
    let mut out = r.process(&input);    // feed any chunking
    out.extend(r.finish());             // drain the FIR tail

    println!("{} in -> {} out @ {} Hz", input.len(), out.len(), r.out_rate());
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
