# rff-format-mjpeg

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **Motion JPEG** containers for **remade_ffmpeg_rs**: what a camera sends.
Our own code.

- **`mjpeg`** — a raw stream of concatenated JPEG frames (`.mjpeg`, `.mjpg`),
  demux + mux. No timing in the stream, so packets are counted at 25 fps
  like ffmpeg's raw MJPEG demuxer.
- **`mpjpeg`** — MJPEG over HTTP, `multipart/x-mixed-replace`: what an ESP32
  CameraWebServer, a `rusty_esp_video` device or any IP camera pushes, so
  `rff -i http://device/stream out.mp4` works. Demux + mux. Parts with an
  `X-Timestamp` header (microseconds) get real timestamps; `Content-Length`
  is honoured when present and the JPEG marker grammar delimits the part when
  it is not.
- One frame splitter for both, walking marker segments rather than scanning
  for `FF D9` — an EXIF thumbnail is a whole JPEG inside `APP1` and a byte
  scan would end the frame there. Progressive scans and restart markers are
  handled by the grammar.
- Pure Rust, no C/FFI. Hostile input errors; it does not panic.

## Usage

```rust
use rff_format::FormatRegistry;
use rff_core::Error;

fn main() -> Result<(), Error> {
    let mut formats = FormatRegistry::new();
    rff_format_mjpeg::register(&mut formats);

    let file = std::fs::File::open("camera.mjpeg").expect("open input");
    let mut demuxer = formats.open_demuxer("mjpeg", Box::new(file))?;
    for stream in demuxer.read_header()? {
        println!("stream {}: {:?} {}x{}", stream.index, stream.codec_id, stream.width, stream.height);
    }
    Ok(())
}
```

Part of [remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs).
