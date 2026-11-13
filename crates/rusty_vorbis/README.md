# rusty_vorbis

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A pure-Rust Ogg Vorbis **encoder and decoder** — to our knowledge **the first
pure-Rust, permissively-licensed Vorbis encoder** (every other encoder is an
FFI binding to libvorbis), plus a decoder that matches FFmpeg and is faster
than the existing pure-Rust decoders. Zero dependencies, no C, no FFI,
Apache-2.0. Vorbis itself is patent-free and royalty-free everywhere.

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
- **A fast, faithful decoder**: `decode::Decoder` covers all of Vorbis I
  (floor 0/1, residue 0/1/2, VQ lookup types 1/2, submaps, coupling, block
  switching, up to 255 channels) and matches FFmpeg 8.1 to a max |diff| of
  3e-8 on a libvorbis corpus (q0–q10, 1–8 channels, 8–96 kHz). Single-threaded
  it is **1.5–1.6x faster than symphonia 0.5.5** and **2.5–3.1x faster than
  lewton 0.10.2** (see *Decode* below).

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

## Decode

The decoder is packet level and Ogg-agnostic: build it from the three header
packets (identification, comment, setup), then feed audio packets in stream
order. Output is `f32`, interleaved into your buffer (or planar via
`decode` + `channel`), with no per-packet allocation.

```rust
use rusty_vorbis::decode::Decoder;

fn decode(headers: [&[u8]; 3], packets: &[Vec<u8>]) -> rusty_vorbis::Result<Vec<f32>> {
    let mut dec = Decoder::new(headers[0], headers[1], headers[2])?;
    let mut buf = vec![0.0f32; dec.max_frames_per_packet() * dec.channels()];
    let mut pcm = Vec::new();
    for p in packets {
        let frames = dec.decode_interleaved(p, &mut buf)?;
        pcm.extend_from_slice(&buf[..frames * dec.channels()]);
    }
    Ok(pcm) // trim the end to the final granule position (the container's job)
}
```

- **Semantics** match lewton (and FFmpeg's totals): the first audio packet
  yields 0 frames, every later one the samples its window completes — which
  is `prev_blocksize/4 + blocksize/4` except that a long block followed by a
  short one returns its `(long - short)/4` tail one packet early. For end
  trimming against a granule, count with `packet_blocksize` (the granule
  convention), not with the per-packet output.
  Channels come out in **Vorbis order** (e.g. 5.1 is FL C FR RL RR LFE), not
  WAV/FFmpeg order — reorder in your adapter if needed.
- **Robust**: malformed packets return `Err` and leave the decoder state
  untouched; hostile setup headers cannot request unbounded memory (codebook,
  VQ and Huffman-table allocations are capped). Fuzzed in the debug profile.
- **Correctness gate** (`tests/corpus.rs`, `tests/synthetic.rs`): identical
  frame counts and max |diff| <= 3e-8 (SNR >= 135 dB) vs FFmpeg on libvorbis
  streams; vs lewton on 300 generated setups exercising every floor / residue
  / lookup / mapping feature. Three lewton defects surfaced along the way
  (wrong fast IMDCT at block size 64/128, swapped coupling roles when the
  magnitude channel number is above the angle's, only 7 of 8 residue cascade
  books read); this decoder follows the spec in each.
- **Speed** (60 s files, in-process, packets demuxed once outside the timed
  loop, one core pinned + High priority, arms interleaved ABBA, 24 rounds,
  median of paired ratios, 24/24 paired wins in every row; null arm within
  2%):

  | file | vs symphonia 0.5.5 | vs lewton 0.10.2 |
  |---|---:|---:|
  | stereo 44.1 kHz q3 | 1.63x faster | 3.04x faster |
  | stereo 44.1 kHz q5 | 1.61x faster | 3.05x faster |
  | stereo 44.1 kHz q8 | 1.55x faster | 3.11x faster |
  | 5.1 48 kHz q5 | 1.52x faster | 2.48x faster |

  How: a 64-bit buffered bit reader kept in registers through each residue
  partition, multi-level lookup-table Huffman decode, residue decoded straight
  into the spectra, floor-1 curves rendered as constant-gain runs, and a
  power-of-two DCT-IV IMDCT with AVX/NEON FFT twins. `--features profile`
  prints the per-stage breakdown (`examples/vorbisdec.rs`).

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
