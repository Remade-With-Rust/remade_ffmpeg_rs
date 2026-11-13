# rusty_h265

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A pure-Rust **HEVC / H.265 video decoder** — no C, no FFI, no build scripts,
zero dependencies, `#![forbid(unsafe_code)]`, Apache-2.0. Written from
ITU-T H.265 with the JCT-VC HM reference decoder (BSD-3) as the transcribable
source, and gated on the **JCT-VC HEVC_v1 conformance suite** (147 streams,
published YUV md5 + the decoded-picture-hash SEI of every picture).

Scope (v1): **Main, Main 10 and Main Still Picture** — 4:2:0, 8 and 10 bit,
every version-1 tool: CABAC, all intra modes, merge/AMVP/TMVP, weighted
prediction, PCM, lossless, transform skip, scaling lists, sign data hiding,
deblocking, SAO, slices, dependent slice segments, tiles and wavefront
parallel processing. Range extensions, screen content coding and the
layered profiles are parsed and refused with a named `Error::Unsupported`.

## Decoding

The API mirrors FFmpeg's send/receive convention: push NAL units (or whole
Annex-B chunks) in, pull frames out in output order; `Error::Again` means
"feed more input".

```rust
use rusty_h265::{Decoder, Error};

fn decode(annexb: &[u8]) -> Result<(), Error> {
    let mut dec = Decoder::new();
    dec.push_annexb(annexb, None)?;   // any number of NAL units, e.g. one access unit
    dec.flush();                      // end of stream
    while let Ok(frame) = dec.next_frame() {
        // frame.picture.planes[0..3] are Y/Cb/Cr u16 planes at the coded size;
        // frame.width / frame.height and picture.crop give the display window.
        println!("{}x{} poc {} {}-bit", frame.width, frame.height, frame.poc, frame.bit_depth());
    }
    Ok(())
}
```

`Decoder::verify_sei = true` checks every decoded picture against its
decoded-picture-hash SEI (MD5 / CRC / checksum) and counts mismatches in
`Decoder::stats` — the decoder carries its own conformance gate.

The `rusty_h265` binary is the conformance-harness front end:
`rusty_h265 <in.bit> <out.yuv> [--verify-sei]` writes the cropped pictures as
planar 4:2:0 (`u8`, or `u16` little-endian above 8 bits) in output order.

## Performance

Single-thread decode against FFmpeg 8.1.2's native `hevc` decoder
(`-threads 1`). Both arms pinned to one core at High priority, timed by their
own self-reported decode time, ABBA-interleaved, 15 pairs per stream, on an
i7-14650HX. Ours is built as it ships: rusty_alloc, AVX2 kernels by runtime
dispatch, everything else for the portable x86-64 baseline.

| stream | FFmpeg 1T | rusty_h265 | × FFmpeg | paired verdict |
|---|---:|---:|---:|---|
| x265-encoded 20 s 720p30 clip | 1,199 ms | 1,461 ms | **0.82×** | 0/15, z = −3.87 |
| JCT-VC 720p, 60 frames | 150 ms | 172 ms | **0.86×** | 0/15, z = −3.87 |
| all-intra 720p (`ipred_x24`) | 1,759 ms | 2,159 ms | **0.82×** | 0/15, z = −3.87 |

FFmpeg measured against itself reads 0.95×, so differences inside ~5% are
noise. On the JCT-VC streams the picture is wider: weighted prediction runs
**1.37× faster** than FFmpeg (20/21 pairs, z = +4.15), and deblocking-heavy and
10-bit content are at parity.

### What changed since 0.6

An instruction-count campaign, measured with callgrind on a deterministic
count rather than a clock, cut **34.0%** of the instructions on the x265 clip
and **36.7%** on all-intra content. Every step was gated bit-exact on all 147
JCT-VC conformance streams and every decoded-picture-hash SEI. The wall clock
agrees, against the 0.6 decoder on the same harness (11/11 pairs each, z = 3.32):

| stream | 0.6 | this release | speed-up |
|---|---:|---:|---:|
| x265-encoded 20 s 720p30 clip | 2,122 ms | 1,539 ms | **1.37×** |
| JCT-VC 720p, 60 frames | 260 ms | 186 ms | **1.39×** |
| all-intra 720p (`ipred_x24`) | 3,244 ms | 2,169 ms | **1.50×** |

- **SAO** runs one whole-CTB kernel when all eight neighbours are usable, instead of a per-sample border ring.
- **Inverse transform** runs one block kernel per size with both passes inside the SIMD boundary, and dequantization happens at the coefficient write.
- **Deblocking strength** is written at parse time by the CU, PU and TU that own each edge; the frame-wide edge scans are gone.
- **Deblocking** skips eight zero strengths at a time and computes its decisions inside the kernel.
- **Intra reference gathering** answers availability once per block and copies spans, not samples.
- **Motion compensation** folds the uni/bi output stage into the last interpolation pass.
- **Per-4x4 metadata** lives in one packed record, filled once per coding unit, instead of six maps.

## Conformance

`cargo test -p rusty_h265 --release` runs the unit tests and, when
`hevc-vectors/` is present (fetched by `scripts/fetch-hevc-vectors.sh`), the
corpus gates. The full md5 verdict comes from the decoder-agnostic harness in
`tools/hevc/` (see its README for the standing table and the method).

## Sources and clean-room line

- ITU-T H.265 (the specification) and the HM reference software (BSD-3):
  the only sources code was transcribed from. `tools/hevc/hm-oracle.patch`
  is the instrumentation used to diff boundary strengths and filter
  decisions against HM during bring-up.
- FFmpeg's `hevc` decoder, libde265 and hpvcd were used only as black-box
  output oracles; no LGPL source was opened.

## License

Apache-2.0. HEVC is subject to patents held by many parties; this crate
implements the standard and makes no licensing representation.