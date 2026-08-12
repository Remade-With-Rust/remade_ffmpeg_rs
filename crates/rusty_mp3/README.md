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
  ≤ 256 kbps). Quality is **behind LAME by ~0.7 ODG at 192 kbps and ~1.1 at
  128 kbps** on a three-clip corpus, and the gap is content-dependent — ~0.3 on
  tonal material, 1.4–1.8 on transients. See [Quality](#quality). Known gaps:
  the reservoir is disabled at 320 kbps and for MPEG-2/2.5 (fixed-frame path is
  used there instead); the per-band distortion loop is effectively inert, so we
  pick a global gain per granule where LAME shapes noise per band; and the
  psychoacoustic model has no short-block thresholds.

## Quality

PEAQ ODG at matched **actual** bitrate, on a three-clip corpus — tonal guitar,
dense piano, transient clicks. Per clip, because the mean hides the thing that
matters:

| CBR 192 kbps | guitar | piano | clicks | mean |
| ------------ | ------ | ----- | ------ | ---- |
| LAME | +0.01 | +0.11 | −1.14 | −0.34 |
| ours | −0.28 | −0.37 | −2.53 | −1.06 |
| **gap** | **0.29** | **0.48** | **1.39** | **0.72** |

| CBR 128 kbps | guitar | piano | clicks | mean |
| ------------ | ------ | ----- | ------ | ---- |
| LAME | −0.90 | +0.06 | −1.32 | −0.72 |
| ours | −1.28 | −1.07 | −3.06 | −1.80 |
| **gap** | **0.37** | **1.13** | **1.75** | **1.08** |

ODG runs 0 (imperceptible) to −4 (very annoying).

**The gap is content-dependent, and transients are where we lose.** On tonal
guitar we are ~0.3 ODG behind LAME; on percussive material we are 1.4–1.8
behind. Two known causes, both structural rather than tuning: the per-band
distortion loop keeps iteration 0 in effectively every granule, so we choose a
global gain per granule where LAME shapes noise per band; and the
psychoacoustic model produces no short-block masking thresholds, so the block
type that exists to control pre-echo is the one with the weakest model behind
it.

Sweeping the model's one masking constant across a 6× range moves tonal content
by 0.002 ODG, which is the arithmetic confirming the above: the constant is not
what is binding.

*Earlier revisions of this README quoted a 0.29–0.46 ODG gap. That was measured
on the guitar clip alone, which is our best content; the corpus numbers above
supersede it. An earlier revision also claimed PEAQ parity with LAME, which
predated measuring against it at matched bitrate.*

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

### VBR correctness — 0.5.0

**If you use `-q:a` / `vbr_quality`, upgrade.** Every release up to and
including 0.4.1 produced VBR streams that FFmpeg rejects (`invalid new backstep
-1`) and that decode to noise. Three stacked defects:

- the masking thresholds (FFT power domain) were compared directly against
  quantization noise (MDCT domain), scales ~10⁴ apart — so the gain search
  saturated at the coarsest setting for 97.5% of granules and every quality
  setting produced the same ~39 kbps;
- the quality scale was inverted (NMR ≥ 1 means noise *at or above* the masking
  threshold, so even the best setting asked for audible noise);
- with those fixed, quality could demand more bits than the largest legal frame
  holds, and the overflow corrupted the bit-reservoir back-pointer.

Measured on 60 s of real guitar, ours at each `-q:a`:

| `-q:a` | kbps | SNR | FFmpeg decode |
| ------ | ---- | --- | ------------- |
| 0 | 315.1 | 54.47 dB | clean |
| 4 | 300.3 | 47.34 dB | clean |
| 9 | 143.8 | 9.01 dB | clean |

CBR is unaffected and byte-identical across the change.

**VBR quality — fixed in 0.6.0.** Through 0.5.1 the VBR path ran its own
noise-to-mask gain search, and PEAQ measured it **3.5 ODG behind LAME** at
matched bitrate — worse at 268 kbps than the CBR path managed at 192, because
the criterion was dimensionless but unanchored. `-q:a` is now a target average
bitrate that drives the **same two-loop quantizer CBR uses**.

PEAQ ODG at matched *actual* bitrate (matching `-q:a` between encoders does not
match the rate, so it proves nothing):

| point | ours | LAME | gap |
| ----- | ---- | ---- | --- |
| 192 kbps | −0.44 | +0.01 | 0.45 |
| 128 kbps | −1.36 | −0.90 | 0.46 |
| 80 kbps | −3.07 | −2.77 | 0.30 |
| *(0.5.1, 200 kbps)* | *−3.50* | *+0.01* | *3.51* |

So VBR now sits in the same 0.3–0.5 ODG band as CBR rather than three ODG
adrift, and `-q:a` lands where users expect (q=0 ≈ 245 kbps, q=5 ≈ 130,
q=9 ≈ 65). Closing the remaining ~0.4 ODG is ordinary encoder tuning and
applies to CBR and VBR alike.

Short blocks are covered by the same budget, so they no longer need a separate
path.

### Decode — vs FFmpeg, at matched CPU

Both sides decode the same 27.5-minute stream and **discard the output**, pinned
to the same physical cores, wall clock, arms alternated, 15 pairs:

| cores each | rusty_mp3 | FFmpeg | result |
| ---------- | --------- | ------ | ------ |
| 1 physical | 1,952 ms | 2,431 ms | **1.24× faster** (15/15, z = −3.87) |
| 2 physical | **1,017 ms** | 1,455 ms | **1.39× faster** (15/15, z = −3.87) |
| *2, our serial build (control)* | *1,807 ms* | *1,445 ms* | *1.30× slower* |

We also use **less total CPU**: 1,875 ms against FFmpeg's 2,125 ms on one core.
The win is not bought with extra work.

The control row is the one that makes the two-core number trustworthy: our
*serial* build on the same two cores reads `cpu/wall` 0.95 — it cannot use the
second core and loses. Only the pipelined build converts the budget, at
`cpu/wall` 1.78. So the gain is real concurrency, not scheduling luck.

Three asymmetries had to be removed before any of this was visible, and one was
ours: FFmpeg's CLI uses ~2 cores even with `-threads 1` (`cpu/wall` 1.96); our
CLI wrote 582 MB while `-f null -` writes nothing; and our own profiler hashed
every sample, 17% of its own runtime, work FFmpeg never did. A CLI-to-CLI
comparison with those in place read 1.20× *behind* — it was measuring the output
path, not the codec.

### Decode — 0.5.0 (SIMD)### Decode — 0.5.0 (SIMD)

Two AVX kernels in the synthesis filterbank, both **bit-identical** to their
scalar twins (each output owns a lane and accumulates in the original order;
separate mul+add, never FMA). Runtime-detected, with the scalar paths kept as
oracles and as the fallback.

| kernel | share of the win |
| ------ | ---------------- |
| matrixing (`matrixing_avx`) | **1.162×** whole decode, 31/31 pairs, z = 5.57 |
| windowing (`window_avx`) | 1.019×, 23/31 pairs, z = 2.69 |

The gap between those two is the useful part: same stage, same instruction set,
same effort, 16.2% versus 1.9%. Auto-vectorization had produced **0 packed ops
against 1698 scalar ones** in this kernel, but "the stage is hot" was still not
enough to aim a kernel — it took the split *within* the stage.

### Decode — 0.4.1

Three structural changes, measured on a real 27.5-minute stereo stream encoded
by LAME (a decoder benchmarked on its own encoder's output skips paths that
encoder never emits, so provenance matters):

| brick | change | effect |
| ----- | ------ | ------ |
| bit reader | `peek(n)` loaded eight bytes and shifted, instead of looping once per bit | huffman 954 → 706 ms |
| synthesis | V FIFO addressed circularly instead of shifted (a 960-float memmove per pass, ~17.5 GB per track), plus a transposed window loop so the operands are contiguous | synthesis 1008 → 857 ms |
| IMDCT | exact kernel symmetry — half the dot products are derived, 648 → 324 MACs per subband | imdct share 31.9% → 19.9% |

**1.185× faster decode overall**, 39/41 paired wins, z = 5.78, against a null
arm of 1.006. Measured directly rather than by chaining the per-brick ratios,
which would have overstated it as 1.233×.

All three are **bit-identical**, not merely close. The IMDCT one is the
surprise: halving the work costs no precision because the symmetries
(`cos36[17−n][k] == −cos36[n][k]`, `cos36[53−n][k] == +cos36[n][k]`, and the
same pair in the 12-point short-block kernel) hold *exactly* in the stored f32
tables, and IEEE multiplication and round-to-nearest are sign-symmetric — so a
mirrored sum is exactly the negation of the computed one.

Verified byte-identical over a 15-stream corpus spanning joint/true-L-R/mono,
MPEG-1 (44.1/48/32 kHz), MPEG-2 (22.05/24 kHz — the 576-sample granule path),
MPEG-2.5 (11.025 kHz), 128–320 kbps CBR plus VBR, and four content classes.
Short blocks are 15–37% of granules there; **mixed** blocks are 0% on every
stream because LAME never emits them, so that path is gated separately against
a dense reference implementation instead of being assumed covered.

```sh
cargo run -p rusty_mp3 --release --example decprof -- input.mp3
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
