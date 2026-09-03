# rff-format-rtp

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **`rtp`** input format for **remade_ffmpeg_rs**: timed frames as received
by [`rff-io`](https://crates.io/crates/rff-io)'s `rtp://` reader. Our own code.

- `rff -i rtp://@:5004 out.mp4` — the reader reassembles **RFC 6184 H.264**
  (single NAL, STAP-A, FU-A) or **RFC 2435 JPEG** (tables in-band or from
  the RFC's `Q` tables, Annex K Huffman tables regenerated) from RTP packets;
  this demuxer turns each frame into a packet with `pts` on the 90 kHz RTP
  clock (unwrapped past 2³²), dimensions from the first SPS or `SOF0`.
- The payload type picks the codec: 26 is JPEG, a dynamic type (96..127) is
  H.264; `rtp://@:5004?pt=96` pins it, `?timeout=SECONDS` sets the idle
  timeout that ends the stream.
- Loss is reported, not guessed: a sequence gap inside a frame drops that
  frame; the next frame start resynchronises.
- Pure Rust, no C/FFI. ffmpeg's `-f rtp` output is the oracle
  (`rff-io/tests/rtp_ffmpeg.rs`).

## Usage

```rust
use rff_format::FormatRegistry;
use rff_core::Error;

fn main() -> Result<(), Error> {
    let mut formats = FormatRegistry::new();
    rff_format_rtp::register(&mut formats);

    let reader = rff_io::rtp::RtpReader::bind("rtp://@:5004")?;
    let mut demuxer = formats.open_demuxer(reader.format_name(), Box::new(reader))?;
    for stream in demuxer.read_header()? {
        println!("stream {}: {:?} {}x{}", stream.index, stream.codec_id, stream.width, stream.height);
    }
    Ok(())
}
```

Part of [remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs).
