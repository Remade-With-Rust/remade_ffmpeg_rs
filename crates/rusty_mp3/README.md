# rusty_mp3

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A pure-Rust MP3 (MPEG-1/2/2.5 Audio Layer III) **decoder and encoder**. Zero
dependencies, no C, no FFI, Apache-2.0. MP3's patents expired in 2017 — the
format is royalty-free everywhere.

- **Decoder**: full MPEG-1/2/2.5 Layer III — bit reservoir, all stereo modes
  (including mid/side and intensity), alias reduction, hybrid IMDCT, polyphase
  synthesis. **Bit-exact against FFmpeg** on our conformance corpus.
- **Encoder**: to our knowledge the **first pure-Rust MP3 encoder on
  crates.io** — existing options are FFI bindings to LAME. MPEG-1/2/2.5, CBR
  and VBR, mono/stereo/joint (mid/side) stereo, psychoacoustic model with
  transient block switching, and a **bit reservoir** (default-on for MPEG-1 CBR
  ≤ 256 kbps) that puts it at PEAQ parity with LAME across 96–256 kbps on the
  content we've measured. Known gap: the reservoir is disabled at 320 kbps and
  for MPEG-2/2.5 (fixed-frame path is used there instead).

## Decode

```rust
use rusty_mp3::{Mp3Decoder, Error};

fn main() -> Result<(), Error> {
    let bytes = std::fs::read("input.mp3").expect("read input");

    let mut dec = Mp3Decoder::new();
    dec.push(&bytes); // feed any chunking you like; the decoder frame-syncs
    dec.flush();      // signal end of input

    let mut pcm = Vec::new(); // interleaved f32 in [-1, 1]
    loop {
        match dec.next_frame() {
            Ok(frame) => {
                println!("{} Hz, {} ch", frame.sample_rate, frame.channels);
                pcm.extend_from_slice(&frame.samples);
            }
            Err(Error::Again) => break, // need more input (streaming)
            Err(Error::Eof) => break,   // flushed and fully drained
            Err(e) => return Err(e),
        }
    }
    println!("decoded {} samples", pcm.len());
    Ok(())
}
```

## Encode

```rust
use rusty_mp3::{Mp3Encoder, Mp3EncoderConfig, Error};

fn main() -> Result<(), Error> {
    // 2 s of a 440 Hz sine, mono 44.1 kHz.
    let sr = 44100u32;
    let pcm: Vec<f32> = (0..2 * sr)
        .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin())
        .collect();

    let mut enc = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: 192, // 0 = default (128); snapped to the valid table
        vbr_quality: None, // Some(rusty_mp3::vbr_quality_index(3.0)) for VBR -q:a 3
    });
    enc.push_pcm_f32(&pcm, 1, sr)?; // also: push_pcm_s16 for i16 input
    enc.finish(); // tail padding, reservoir assembly, Xing/Info header

    let mut mp3 = Vec::new();
    while let Ok(packet) = enc.next_packet() {
        mp3.extend_from_slice(&packet);
    }
    std::fs::write("out.mp3", mp3).expect("write output");
    Ok(())
}
```

The pull calls follow FFmpeg's EAGAIN/EOF drain protocol: `Err(Error::Again)`
means "feed more input", `Err(Error::Eof)` means the flushed stream is fully
drained. Lower-level building blocks (frame-level `Mp3Decode`/`Mp3Encode`,
`header::FrameHeader`, bit I/O, ISO tables) are public too.

## Performance

Measured on a real 6:53 stereo 44.1 kHz music track (412.9 s, 15,806 frames) at
CBR 192 kbps.

**0.4.0** caches the psychoacoustic model's FFT twiddle factors — they depend
only on the transform size, but were rebuilt on every call, which cost ~1.26 M
`cos`/`sin` evaluations across the track to produce ten distinct values — and
reuses the mid/side scratch across frames instead of reallocating it:

|                                     | before   | after        |
| ----------------------------------- | -------- | ------------ |
| encode CPU (median of 41 pairs)     | 7,047 ms | **6,766 ms** |
| allocations per frame               | 32.06    | **21.06**    |
| zero-filled allocations / 800 frames | 8,000    | **2**        |

**1.045× faster encode**, 33/41 paired wins, z = 3.90. The output is
byte-identical across the change (same md5 over the full track), so both arms
are provably doing the same work rather than one of them doing less. Decode is
untouched at 5.04 allocations per frame — its per-block path was already
allocation-free.

Method: pinned to one core at High priority, CPU time rather than wall,
arms ABBA-interleaved, 41 pairs, with a null arm (the same binary against
itself) reading 1.017 as the session's resolution floor. This is a
same-binary-family delta, not a cross-implementation ratio.

The allocation counts are reproducible with the bundled instrument, which
counts through whichever global allocator the binary sets:

```sh
cargo run -p rusty_mp3 --release --example allocaudit -- 800 192
```

## Part of Remade With Rust

This crate is the standalone MP3 engine of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Also check out our
sister project **[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for
an AI-first world — and the rest of
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**, including
the sibling codec crates
[`rusty_h264`](https://crates.io/crates/rusty_h264),
[`rusty_vp9`](https://crates.io/crates/rusty_vp9),
[`rusty_aac`](https://crates.io/crates/rusty_aac),
[`rusty-opus`](https://crates.io/crates/rusty-opus), [`rusty_vorbis`](https://crates.io/crates/rusty_vorbis), and the
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
