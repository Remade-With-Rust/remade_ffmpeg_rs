# rusty_vorbis

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A pure-Rust Ogg Vorbis **encoder** — to our knowledge **the first pure-Rust,
permissively-licensed Vorbis encoder**: Rust has had Vorbis decoders for years
(lewton, Symphonia), but every encoder was an FFI binding to libvorbis. Zero
dependencies, no C, no FFI, Apache-2.0. Vorbis itself is patent-free and
royalty-free everywhere.

- **The pipeline**: window → N/4-FFT forward MDCT → Bark-scale masking-threshold
  floor (a real psychoacoustic model, not an energy envelope) → forward channel
  coupling + point stereo → rate-distortion residue VQ, emitting an embedded
  libvorbis setup header. Quality is the familiar Vorbis `-q` knob (−1..=10).
- **Validated output**: packets decode in ffmpeg and in
  [lewton](https://crates.io/crates/lewton) (the decoder oracle the encoder was
  built against, brick by brick); the residue-class shortlist speed lever is
  PEAQ-validated perceptually neutral (ΔODG ≤ 0.03).
- **Honest performance**: ~5.3× faster than libvorbis wall-clock on stereo music
  via frame-parallel encode (a 24-core figure — libvorbis is single-threaded per
  stream); single-threaded we are ~1.4× behind libvorbis.
- **Encoder-only by design**: decoding is intentionally out of scope — use
  [`lewton`](https://crates.io/crates/lewton) for that.

## Encode

```rust
use rusty_vorbis::{Error, VorbisEncoder, VorbisEncoderConfig, quality01_from_vorbis_q};

fn main() -> Result<(), Error> {
    // 2 s of a 440 Hz sine, stereo 44.1 kHz, interleaved f32 in [-1, 1].
    let sr = 44_100u32;
    let pcm: Vec<f32> = (0..2 * sr)
        .flat_map(|i| {
            let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin();
            [s, s] // L, R
        })
        .collect();

    let mut enc = VorbisEncoder::new(VorbisEncoderConfig {
        quality: quality01_from_vorbis_q(4.0), // the Vorbis `-q:a` scale, −1..=10
        ..VorbisEncoderConfig::default()
    });
    enc.push_pcm_f32(&pcm, 2, sr)?; // also: push_pcm_s16 for i16 input
    enc.finish(); // all blocks encode in parallel across cores on the next pull

    // The first three packets are the ident/comment/setup headers; the rest are
    // audio packets with granule pts — hand them to an Ogg muxer in this order.
    let mut packets = Vec::new();
    loop {
        match enc.next_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => break, // flushed and fully drained
            Err(e) => return Err(e),
        }
    }
    println!("{} packets ({} headers + audio)", packets.len(), 3);
    Ok(())
}
```

The pull calls follow FFmpeg's EAGAIN/EOF drain protocol: `Err(Error::Again)`
means "feed more input", `Err(Error::Eof)` means the flushed stream is fully
drained. Lower-level building blocks (the `setup` header parser and encode-side
codebooks, per-block `frame::encode_long_packet` / `frame::encode_stream_bs`,
the `mdct` filterbank, the `psy` masking model, the LSB-first `BitWriter`) are
public too.

## Part of Remade With Rust

This crate is the standalone Vorbis engine of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Also check out our
sister project **[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for
an AI-first world — and the rest of
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**, including
the sibling codec crates
[`rusty_h264`](https://crates.io/crates/rusty_h264),
[`rusty_vp9`](https://crates.io/crates/rusty_vp9),
[`rusty_mp3`](https://crates.io/crates/rusty_mp3),
[`rusty_aac`](https://crates.io/crates/rusty_aac),
[`rusty-opus`](https://crates.io/crates/rusty-opus), and the
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
