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
while fully conformant, is currently slower than libvpx single-thread. 0.2
closes part of that gap: **1.5–2.0× faster encode and 1.09–1.23× faster
decode** than 0.1, with byte-identical output (see [Performance](#performance)).

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

## Performance

### What changed in 0.2

An instruction-count campaign, measured with callgrind on a deterministic count
rather than a clock, worked fifteen separate parts of the codec. Every step was
gated on **identical output**: the same decoded frames (hashed) on four streams
and the same encoded bitstream on five encodes, plus the unit suite.

Wall clock against the 0.1 sources, single thread, pinned to one core,
ABBA-interleaved, minimum per arm, both built the same way (rusty_alloc,
thin LTO, AVX2 kernels by runtime dispatch), on an i7-14650HX under WSL2:

| workload | 0.1 | 0.2 | speed-up | pairs won |
|---|---:|---:|---:|---|
| encode foreman CIF, 10 frames, crf 32, speed 3 | 708 ms | 417 ms | **1.70×** | 12/12 |
| encode akiyo CIF, 10 frames, crf 32, speed 3 | 351 ms | 174 ms | **2.02×** | 12/12 |
| encode mobile CIF, 3 frames, crf 32, speed 0 | 1,292 ms | 727 ms | **1.78×** | 8/8 |
| encode bus CIF, 10 frames, crf 32, speed 5 | 650 ms | 438 ms | **1.48×** | 12/12 |
| decode park_joy 1080p (libvpx stream), ×3 | 1,186 ms | 1,064 ms | **1.11×** | 16/16 |
| decode 720p (libvpx stream), ×4 | 962 ms | 885 ms | **1.09×** | 16/16 |
| decode mobile CIF, high-rate (our stream), ×8 | 722 ms | 588 ms | **1.23×** | 16/16 |

The decode timings include hashing every output byte, which the benchmark does
to prove identical output. That hashing is the same in both arms, so it shrinks
the measured ratio. Instructions retired fell by **17.6–34.4%** on decode and
**68.5–85.4%** on encode. Callgrind counts a memset one instruction per byte,
which inflates the encode figure: measured from after the first two changes,
which removed a 17 KB zero-fill per transform block, the encode reduction is
31–45%.

- **Per-block scratch** is taken from the encoder, not re-allocated and zeroed for every transform block.
- **Forward ADST/DCT** runs as flat-table matrix products with AVX2 row and column passes.
- **Inverse transform** kernels now carry `#[target_feature]`. Without it, their intrinsics were out-of-line calls.
- **Motion compensation** filters a whole block per call, 16 samples a step, and gathers frame-edge tiles a row at a time.
- **Loop filter** has one kernel per edge width and orientation, chosen per frame, and skips the 16-wide sums when no lane needs them.
- **Encoder residual and distortion** are computed in one fused pass, with frame-level SSE kernels for the loop-filter level search.
- **Sub-8×8 motion search** scores 4×4 sub-pel candidates in one u8 pass and no longer scores overlapping candidates twice.
- **Coefficient decoding** indexes its tables without bounds checks and inlines the bool decoder.
- **Intra prediction** fills and copies whole rows.

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
