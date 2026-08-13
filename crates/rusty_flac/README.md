# rusty_flac

[![crates.io](https://img.shields.io/crates/v/rusty_flac?logo=rust)](https://crates.io/crates/rusty_flac)
[![docs.rs](https://img.shields.io/docsrs/rusty_flac?logo=docsdotrs)](https://docs.rs/rusty_flac)
[![CI](https://github.com/remade-with-rust/rusty_flac/actions/workflows/ci.yml/badge.svg)](https://github.com/remade-with-rust/rusty_flac/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)

> **rusty_flac** is a ground-up, pure-**Rust** FLAC **encoder and decoder**:
> zero dependencies, no C, no FFI. It **encodes 17% faster and decodes 23%
> faster than FFmpeg** while producing **smaller files on every benchmarked
> content class** — and every stream is verified **losslessly interoperable in
> both directions** (FFmpeg decodes ours bit-exact; we decode FFmpeg's
> bit-exact). The four SIMD kernels are the only `unsafe` in the crate, and
> each one is gated **bit-identical** against the scalar twin it ships with.

Part of **[Remade With Rust](https://github.com/Remade-With-Rust)** by
**[Mata Network](https://www.mata.network/)** — the FLAC codec inside
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)**,
our memory-safe FFmpeg alternative, alongside
**[FFAI](https://github.com/Remade-With-Rust/FFAI)**, the AI media toolkit.
[Jump to the ecosystem ↓](#the-remade-with-rust-ecosystem)

---

## ⚡ The headline

A pure-Rust lossless audio codec that beats the C reference on **speed and
compression at the same time**, without giving up a byte of interoperability:

- **Faster than FFmpeg on both sides.** Encode at the maximum compression
  level runs **0.83× FFmpeg's CPU time** (17.3% faster); decode runs **0.77×**
  (22.7% faster). Measured pinned, CPU-time, ABBA-interleaved, 31 pairs —
  **31/31 wins on both, z = 5.57** (method below).
- **Smaller output on every gate clip.** Across the standing gate matrix
  (music / sine / noise / pink noise / silence × 16-bit and 24-bit × two
  levels), rusty_flac's files are smaller than FFmpeg's on **20 of 20**
  combos — −0.4…−4.6% on real music, −87% on silence.
- **Lossless and interoperable, proven both directions.** Every gate clip
  round-trips bit-exact: FFmpeg decodes our streams to the exact source PCM,
  and we decode FFmpeg's streams to the exact source PCM. STREAMINFO carries
  the spec's MD5 signature, so `flac -t` verifies our output too.
- **Zero dependencies.** No C, no FFI, no external crates — the bit I/O, CRCs,
  MD5, LPC analysis, Rice coding and all four stereo modes are in this crate.
  `unsafe` exists only inside four AVX2 kernels (autocorrelation, Rice
  parameter sums, fixed-order estimation, LPC residual), each runtime-detected
  with a scalar twin kept as oracle and fallback, gated **bit-identical** by
  tests — on any other CPU you get the same bytes from safe Rust.

| | FFmpeg flac (C) | **rusty_flac (Rust)** |
|---|---|---|
| C/C++ in the dependency tree | all of it | **none** |
| Dependencies | libavcodec/libavutil | **zero** |
| Encode CPU (level 8) | 1.00× | **0.83× — 17.3% faster** |
| Decode CPU | 1.00× | **0.77× — 22.7% faster** |
| Output size, 20-combo gate matrix | baseline | **smaller on 20/20** |
| Lossless interop | — | **both directions, bit-exact, gated** |
| License | LGPL/GPL | **Apache-2.0** (embed freely) |

### Performance (single core, this machine)

Measured against FFmpeg 8.1.2's native `flac` encoder/decoder over a 180 s
44.1 kHz stereo music clip (guitar / piano / vocal corpus):

| workload | rusty_flac | FFmpeg | ratio |
|---|---:|---:|---:|
| **Encode**, `-compression_level 8` | **672 ms** | 813 ms | **0.83× — 31/31 wins, z = 5.57** |
| **Decode** (FFmpeg-encoded stream) | **266 ms** | 344 ms | **0.77× — 31/31 wins, z = 5.57** |

Size at matched level, the standing gate table (excerpt):

| clip | level | rusty_flac | FFmpeg | delta |
|---|---|---:|---:|---:|
| stereo guitar (s24) | L8 | 539,087 | 565,122 | **−4.6%** |
| stereo piano (s16) | L8 | 334,330 | 346,299 | **−3.5%** |
| pink noise (s24) | L8 | 1,057,412 | 1,065,512 | **−0.8%** |
| vocal (s16) | L8 | 839,291 | 847,315 | **−0.9%** |
| silence (s16) | L8 | 1,173 | 9,137 | **−87.2%** |

<sub>**Method** (the discipline is the point): both binaries pinned to one
core at High priority, measuring **CPU time** not wall (a busy box counts
descheduled time against wall), arms **ABBA-alternated**, 31 pairs, reported
as a paired win-rate with a z-score. Work parity is checked (both arms code
the identical PCM; outputs are verified lossless before anything is timed),
and a null arm (FFmpeg vs itself) bounds the noise floor. Cross-implementation
ratios are quoted at N = 31 because smaller N provably drifts — an earlier
N = 15 read of the same quantity moved 5 points by N = 31.</sub>

<sub>**Where the speed comes from** (each brick landed byte-identical or
size-gated): an accumulator bit writer and table CRCs; cached apodization
windows; an exact bottom-up sum-merged Rice partition planner (O(15n) once,
replacing a 15-parameter scan per partition per order); estimate-gated
realization — fixed order by one-pass |residual| sums, window winner by
Levinson estimate, stereo mode by per-arm estimates, with safety margins that
realize runners-up when estimates are close (total size cost of all gating:
**+0.09%**, still below FFmpeg); exact AVX2 kernels with bit-identity gate
tests; an unrolled MD5; a word-at-a-time bit reader with fused Rice reads; and
pre-sized write-by-index decode buffers.</sub>

## What is this?

`rusty_flac` encodes and decodes FLAC in pure Rust — the full subset in both
directions: FIXED and LPC prediction (orders to 12), all four stereo modes
(independent, left/side, right/side, mid/side), **wasted-bits** detection,
Rice and **Rice2** partitioned residuals with escape codes, 8/16/24-bit encode
and 4–32-bit decode, up to 8 channels, CRC-8/CRC-16 verification, and the
STREAMINFO MD5 audio signature. There is no C in the dependency tree — in
fact there are **no dependencies at all**. It is a reimplementation of the
format, not a wrapper, and it is Apache-2.0: embed it in closed-source
software with no copyleft obligations.

Correctness is enforced by a standing gate matrix, not by hope: every content
class × bit depth × channel count × compression level must round-trip exactly
through **two independent decoders** (our own and claxon), and the FFmpeg
interop gate proves both encode and decode against the reference in both
directions. The gates found real bugs before release — a spec-invalid
STREAMINFO on sub-16-sample streams and a missing Rice2 path among them —
which is exactly what they are for.

## The Remade With Rust ecosystem

<!-- ORG BOILERPLATE — keep identical across repos -->

**Remade With Rust** is an initiative by **[Mata Network](https://www.mata.network/)**
to rebuild essential C and C++ tools in Rust — for the memory safety, the
predictable performance, and the freedom of a permissive license. Each project
is a reimplementation, not a fork: same wire protocols and file formats, new
code you can actually depend on.

We build the core to production grade and open-source it so the community can
extend it. No copyleft. No surprises. Just the tools we rely on, made faster and
safer.

| Project | What it is |
|---|---|
| 🎬 **[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** | **Our FFmpeg alternative.** Drop-in `ffmpeg` and `ffprobe` binaries — demux → decode → filter → encode → mux, rebuilt as composable Rust crates with **zero GPL/LGPL**. Apache-2.0. `rusty_flac` is its FLAC codec. |
| 🧠 **[FFAI](https://github.com/Remade-With-Rust/FFAI)** | **Our sister project: media *for* AI.** "The AI media toolkit, remade with rust." Embedded ASR + TTS (**Mercury**), OCR (**Carmenta**) and vision-language captioning (**Argus**) behind an ffmpeg-style, swap-by-name architecture — no Python, no CUDA. MIT OR Apache-2.0. |
| 🌐 **[Mata Network](https://www.mata.network/)** | **The home page.** *"Stop sacrificing your privacy for convenience."* Sovereign, self-hostable privacy infrastructure — wallet & identity, password manager, contact manager, and a browser extension that stops information leaking as you browse. Remade With Rust is its open-source arm. |

→ All projects: **[github.com/Remade-With-Rust](https://github.com/Remade-With-Rust)**

<!-- /ORG BOILERPLATE -->

## Features

**Encoder** (every stream verified lossless via two independent decoders +
FFmpeg, with the STREAMINFO MD5 signature `flac -t` checks):

- **FIXED (orders 0–4) and LPC prediction (orders to 12)** with Levinson-
  Durbin analysis over two Tukey apodization windows, libFLAC-style
  coefficient quantization with error feedback, and exact-arithmetic residuals
  the decoder inverts losslessly.
- **All four stereo modes** — independent, left/side, right/side, mid/side —
  chosen per block by exact realized cost, with cheap per-arm estimates
  pruning the arms that can't win before any expensive work happens.
- **Wasted-bits detection**: content quantized coarser than its container
  (16-bit audio in a 24-bit file is common in the wild) has its zero LSBs
  shifted out per subframe — worth **8 bits per sample** on such material,
  and the difference between −4.6% and +94% against FFmpeg on our gate clip.
- **Rice + Rice2 partitioned residuals** (partition orders to 8, parameters
  to k = 30), planned by an exact bottom-up sum merge — every partition order
  and both methods are costed exactly, in one pass over the residual.
- **`-compression_level 0..8`** maps to the LPC order searched; CONSTANT and
  VERBATIM subframes handle silence and incompressible blocks.
- **8/16/24-bit**, up to **8 channels**, any sample rate; interleaved or
  planar `i32` input, plus zero-copy-friendly `s16le`/`f32le` byte ingest.
- **`EncodeStats`** wiring-audit counters on every decision path — a corpus
  run proves no path is silently dead and no fallback is silently hot.

**Decoder** (validated sample-exact against claxon and FFmpeg on every gate
clip, in-house and reference-encoded):

- Full subset: CONSTANT / VERBATIM / FIXED / LPC subframes, wasted bits, all
  four stereo assignments, Rice + Rice2 with escape codes, 4–32-bit sample
  sizes, variable tail blocks.
- **CRC-8 header and CRC-16 frame verification** on every frame (table-driven,
  so the check is nearly free); malformed input returns typed errors, never
  panics.
- Streaming per-frame API (`Decoder::next_frame` appends planar `i32`) or the
  one-call `decode()` convenience.
- Fast by construction: a word-at-a-time left-aligned bit reader, fused
  single-refill Rice reads, const-order LPC restore, and pre-sized
  write-by-index buffers (no per-push bookkeeping, no per-frame re-zeroing).

**Shared:**

- **Zero dependencies, no C, no FFI.** Bit I/O, CRC-8/16, MD5 — all in-crate.
- **`unsafe` is confined to four AVX2 kernels**, each runtime-detected
  (`is_x86_feature_detected!`), each with a scalar twin kept as oracle and
  fallback, each gated **bit-identical** by a dedicated test. Integer kernels
  are exact by construction; the two float kernels are engineered to be exact
  (deterministic reduction order; FMA only where every intermediate is an
  integer below 2⁵³, range-guarded against the decoder's truncation).
- Every speed brick landed **byte-identical** (verified against the previous
  binary) or, where the search space changed, size-gated on the corpus with
  the interop gates green.

## Install

```sh
cargo add rusty_flac
```

or in `Cargo.toml`:

```toml
[dependencies]
rusty_flac = "0.1"
```

**Dropping it into `remade_ffmpeg`:** already done — `rff-codec-flac` in
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs) is a
thin adapter over this crate, so `rff -i in.wav out.flac` and FLAC decode in
the CLI run exactly this code.

## Quick start

```rust
// Encode: 2 channels, 16-bit, 44.1 kHz.
let mut enc = rusty_flac::Encoder::new(44_100, 2, 16).unwrap();
enc.set_compression_level(8);
enc.push_planar(&[&left, &right]).unwrap();   // or push_interleaved / push_s16le_bytes
let flac: Vec<u8> = enc.finish();

// Decode the whole stream back to planar i32 — bit-exact.
let (info, channels) = rusty_flac::decode(&flac).unwrap();
assert_eq!(info.sample_rate, 44_100);
assert_eq!(channels[0], left);
assert_eq!(channels[1], right);
```

Streaming decode, one frame at a time:

```rust
let mut dec = rusty_flac::Decoder::new(&flac).unwrap();
let info = dec.streaminfo().clone();
let mut out: Vec<Vec<i32>> = (0..info.channels).map(|_| Vec::new()).collect();
while let Some(_block_len) = dec.next_frame(&mut out).unwrap() {
    // out accumulates planar samples; drain it here for true streaming.
}
```

Wiring-audit stats (how did the encoder actually code this stream?):

```rust
let mut enc = rusty_flac::Encoder::new(44_100, 2, 16).unwrap();
enc.push_planar(&[&left, &right]).unwrap();
let (flac, stats) = enc.finish_with_stats();
println!("LPC {} / FIXED {} / mid-side {}", stats.sub_lpc, stats.sub_fixed, stats.stereo_mid_side);
```

## Architecture

```
src/
  lib.rs      public API: Encoder, Decoder, decode(), StreamInfo, EncodeStats
  encode.rs   analysis + coding: windows, autocorrelation, Levinson, quantize,
              fixed/LPC residuals, Rice/Rice2 partition planning, stereo modes,
              wasted bits, the AVX2 kernels + their scalar twins
  decode.rs   streaming frame decoder: subframes, residuals, stereo undo, CRCs
  bitio.rs    MSB-first accumulator bit writer + word-refill bit reader
  crc.rs      table-driven CRC-8 (0x07) and CRC-16 (0x8005)
  md5.rs      unrolled RFC 1321 MD5 for the STREAMINFO audio signature
tests/
  gates.rs    the standing content-type gate matrix (two-decoder oracle)
examples/
  flacbench.rs  in-process encode/decode bench + stats (runs under rusty_alloc)
```

## Benchmarking & the gates

Everything quoted above is reproducible from the repo:

```sh
# Unit + kernel-identity + content-type gate matrix (two independent decoders):
cargo test --release

# In-process bench + wiring-audit stats over raw PCM
# (produce input with: ffmpeg -i in.wav -f s16le in.raw):
cargo run --release --example flacbench -- in.raw 2 16 8 10
RUSTY_FLAC_TIMING=1 cargo run --release --example flacbench -- in.raw 2 16 8 1
```

In the `remade_ffmpeg_rs` workspace, `tools/flac_gate.ps1` runs the FFmpeg
interop gate: every clip × level is encoded by both sides, decoded by the
*other* side, hash-compared against the source PCM, and size-compared with a
0.5% tolerance — the run fails if we are ever larger. The pinned ABBA
CPU-time harness that produced the N = 31 speed verdicts lives alongside it.

## Platform support

| Platform | Status |
|---|---|
| Windows | ✅ builds + tests |
| Linux | ✅ builds + tests |
| macOS | ✅ builds + tests |

The AVX2 kernels are runtime-detected — no build flags, no `nasm`, no ISA
floor. On any CPU without AVX2 (or any non-x86 target) the scalar twins run
and produce the same bytes.

## Roadmap

- [x] Bitstream core: framing, UTF-8 frame numbers, CRC-8/16, STREAMINFO + MD5
- [x] FIXED + LPC prediction, two-window Levinson analysis, error-feedback
      coefficient quantization
- [x] Partitioned Rice residuals with an exact one-pass sum-merged planner
- [x] All four stereo modes with estimate-gated arm realization
- [x] **Wasted-bits** support (encode + decode)
- [x] **Rice2** (5-bit parameters, k ≤ 30) with per-level method choice
- [x] In-house decoder: full subset, CRC-verified, claxon dropped
- [x] Exact AVX2 kernels (autocorrelation, Rice sums, fixed-order estimate,
      FMA LPC residual) with scalar twins + bit-identity gates
- [x] Standing gates: content matrix × two decoders; FFmpeg interop both
      directions; size-parity tolerance
- [x] **Faster than FFmpeg on encode (0.83×) and decode (0.77×) with smaller
      output on 20/20 gate combos**
- [ ] Streaming encode API (fixed-latency block push, bounded memory)
- [ ] 32-bit encode (FLAC 1.4 extension; decode already handles it)
- [ ] Seek-table and metadata (Vorbis comment / picture) blocks
- [ ] ffmpeg-style level→work-point mapping for L0–L5 (today the level knob
      maps to LPC order only, so our L0 is ~10% smaller but slower than
      FFmpeg's fixed-only L0)

## License

Apache-2.0 — see [LICENSE](LICENSE). No GPL/LGPL anywhere in the dependency
tree — there is no dependency tree.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

**[Mata Network](https://www.mata.network/)** builds sovereign, self-hostable
privacy infrastructure — *"stop sacrificing your privacy for convenience"*:
wallet & identity, a password manager, a contact manager, and a browser
extension that stops your information leaking as you browse.

**Remade With Rust** is our open-source home for the permissively-licensed
building blocks that work depends on — including
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs) (the
FFmpeg alternative) and [FFAI](https://github.com/Remade-With-Rust/FFAI) (the
AI media toolkit).

→ **[www.mata.network](https://www.mata.network/)**

<!-- /ORG BOILERPLATE -->
