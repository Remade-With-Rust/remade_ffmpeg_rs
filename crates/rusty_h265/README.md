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