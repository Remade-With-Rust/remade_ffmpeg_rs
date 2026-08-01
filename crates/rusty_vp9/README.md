# rusty_vp9

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A pure-Rust **VP9 video decoder + encoder** — no C, no FFI, no build scripts,
zero dependencies, Apache-2.0. The decoder is **bit-exact against all 315
official libvpx conformance vectors** (profiles 0–3, 8/10/12-bit, with AVX2 and
NEON fast paths) and is fuzzed to never panic on malformed input. The encoder
has RDO partition/mode search, CBR and two-pass rate control, golden/ALT-REF
reference groups with temporal filtering, compound prediction, and speed
presets — its output is validated pixel-exact against libvpx and ffmpeg.
Honest numbers: the encoder is younger than libvpx (roughly +36% bitrate at a
matched operating point today, and actively being optimized), and the decoder,
while fully conformant, is currently slower than libvpx single-thread.

## Decoding

The API mirrors FFmpeg's send/receive convention: push coded packets in, pull
frames out; `Error::Again` means "feed more input", `Error::Eof` means the
stream is drained.

```rust
use rusty_vp9::{Error, Vp9Decoder};

fn decode(packets: &[Vec<u8>]) -> Result<(), Error> {
    let mut dec = Vp9Decoder::new();
    for (i, pkt) in packets.iter().enumerate() {
        // A packet may be a superframe (hidden ALT-REF + shown frame); the
        // decoder splits it internally.
        dec.push(pkt, Some(i as i64))?;
        loop {
            match dec.next_frame() {
                Ok(frame) => {
                    // frame.planes = Y, U, V; frame.strides in bytes;
                    // frame.bit_depth / frame.subsampling_x / _y give the format.
                    println!("{}x{} @ {} bit", frame.width, frame.height, frame.bit_depth);
                }
                Err(Error::Again) => break, // feed more input
                Err(e) => return Err(e),
            }
        }
    }
    dec.flush();
    while dec.next_frame().is_ok() {} // drain
    Ok(())
}
```

## Encoding

8-bit YUV 4:2:0 in, coded VP9 packets out (ready for an IVF/WebM muxer).

```rust
use rusty_vp9::{Error, Vp9Encoder, Vp9EncoderConfig};

fn encode() -> Result<(), Error> {
    let (w, h) = (320u32, 240u32);
    let y = vec![128u8; (w * h) as usize];
    let u = vec![128u8; ((w / 2) * (h / 2)) as usize];
    let v = vec![128u8; ((w / 2) * (h / 2)) as usize];

    let mut enc = Vp9Encoder::default();
    enc.configure(&Vp9EncoderConfig {
        crf: Some(32),          // constant quality; or bitrate_bps for rate control
        lag: Some(8),           // ALT-REF lookahead groups
        speed: Some(3),         // 0 = best quality .. 6 = fastest
        ..Default::default()
    })?;

    enc.push_frame(
        [y.as_slice(), u.as_slice(), v.as_slice()],
        [w as usize, (w / 2) as usize, (w / 2) as usize],
        w,
        h,
    )?;
    enc.flush();
    loop {
        match enc.next_packet() {
            Ok(pkt) => println!("{} bytes, keyframe={}", pkt.data.len(), pkt.keyframe),
            Err(Error::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
```

The bitstream toolbox is public too: `parse_uncompressed_header` /
`FrameHeader`, `consume_compressed_header`, and the `BitReader` /
`BoolDecoder` primitives.

## Part of Remade With Rust

This crate is the standalone VP9 engine of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** —
a ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on top of pure-Rust codecs, with no copyleft anywhere in
the tree. Also check out our sister project
**[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for an AI-first
world — and the org page
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

Sibling engines from the same effort:
[`rusty_h264`](https://crates.io/crates/rusty_h264) (H.264/AVC),
[`rusty_mp3`](https://crates.io/crates/rusty_mp3) (MP3),
[`rusty_aac`](https://crates.io/crates/rusty_aac) (AAC-LC),
[`rusty-opus`](https://crates.io/crates/rusty-opus) (Opus), [`rusty_vorbis`](https://crates.io/crates/rusty_vorbis) (Vorbis), and the
[rusty-av1-toolkit](https://github.com/Remade-With-Rust/rusty-av1-toolkit) (AV1).

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## License

Apache-2.0. VP9 itself is an open, **royalty-free** codec published by Google,
so there are no per-unit licensing fees to use it.
