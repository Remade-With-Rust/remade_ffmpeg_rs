# rff-format-exr

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **OpenEXR** single-image container for **remade_ffmpeg_rs**: an `.exr`
file is its own codec stream, so the demuxer hands the whole file to
[`rff-codec-exr`](https://crates.io/crates/rff-codec-exr) as one packet and
the muxer writes it straight out. Probed by the EXR magic number; size comes
from the header (read with `rusty_exr`).
