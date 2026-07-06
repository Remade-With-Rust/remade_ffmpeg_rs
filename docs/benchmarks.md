# Benchmarks

Reproducible numbers, reported honestly — including when they aren't flattering.
The README's headline table states *targets*; this page is the *measurements*.

## How to reproduce

Single machine, single-thread, wall-clock:

```sh
# ours
cargo bench -p rff-codec-vp9

# ffmpeg's native VP9 decoder, same clip, same frame count (60 × 50 = 3000)
ffmpeg -threads 1 -benchmark -stream_loop 49 \
       -i crates/rff-codec-vp9/benches/data/vp9_720p.ivf -f null -
```

Clip: 1280×720 4:2:0 VP9, 60 frames (libvpx-vp9 `-crf 32`), committed at
`crates/rff-codec-vp9/benches/data/vp9_720p.ivf`. Machine: Intel Core
i7-14650HX. Throughput = frames × pixels ÷ time; absolute numbers are
machine-specific — the **ratio** is the point.

## VP9 decode

| Decoder | fps | Mpixels/s | vs ffmpeg (1T) |
|---------|----:|----------:|---------------:|
| **remade_ffmpeg_rs** — default release | 342 | 315 | **0.16×** |
| **remade_ffmpeg_rs** — `-C target-cpu=native` | 445 | 410 | 0.21× |
| ffmpeg native vp9, 1 thread *(reference)* | 2132 | 1965 | 1.0× |
| ffmpeg native vp9, multi-thread | 6186 | 5701 | — |

**The honest read:** our VP9 decoder is **bit-exact** (315/315 official libvpx
conformance vectors) and **memory-safe**, but **~5–6× slower** than ffmpeg's
decades-tuned native decoder, single-thread. That's a respectable *starting*
point for a young pure-Rust decoder — not parity, and we don't claim it.

**Where the time goes (profiled, this clip):**

| Phase | Share | Already SIMD? |
|-------|------:|---------------|
| Inter prediction (sub-pixel motion comp) | **~53%** | yes (AVX2) |
| Loop filter (deblocking) | **~41%** | yes (AVX2) |
| Inverse transform | ~2% | no (scalar) |
| Intra prediction | ~2% | no (scalar) |
| Entropy / token decode | ~2% | no (scalar) |

The surprising, measured result: **motion compensation and the loop filter are
~94% of decode** — and both already use hand-written AVX2. The scalar paths
(transform / intra / entropy) are a rounding error.

### Optimization attempts — and what they revealed

Four bit-exact attempts (each gated by the conformance suite), and what each
measured:

| Attempt | Result | Kept? |
|---------|--------|-------|
| Tile-column threading (clone, then no-clone fork) | **~1.7–3× *slower*** — per-worker buffer + merge is ~2× the memory traffic | no |
| Compound (averaged) inter SIMD | **~1.8%** (closed a 100%-scalar gap) | yes |
| `madd_epi16` core convolution | **~0%** (best-of-N tied with `mullo`) | no |
| `target-cpu=native` / `-v2` / `-v3` build flags | **~0%** (interleaved best-of-N tied with default; `native` if anything slightly *slower*) | no |

The pattern is the lesson: **VP9 decode here is memory-bandwidth-bound, not
compute-bound.** Inter prediction is dominated by *copying reference pixels*
(integer-MV blocks) and the loop filter by *moving pixels* — so SIMD-ing the
*arithmetic* (madd, compound) barely moves the needle, telling the compiler to
use newer instructions (`target-cpu`) does nothing, and adding threads just adds
memory traffic. The ~5–6× gap to FFmpeg is mostly structural (memory
layout/prefetch + decades of tuning), not a missing multiply trick.

> **Correction.** An earlier draft of this doc claimed `target-cpu=native` buys
> "a free ~30%." That was wrong — an artifact of a single cold-build-vs-warm-build
> comparison. Careful *interleaved* best-of-N puts native within noise of the
> default (often slower). A compute-codegen flag can't speed up a memory-bound
> workload; the phantom win was thermal drift. Lesson recorded in Caveats.

**Conclusion:** the decoder is at its practical single-thread ceiling for a safe
pure-Rust implementation. There is no free build-flag or compute-SIMD lever left;
the only real levers are algorithmic memory-traffic reduction (hard) — or
accepting the gap, since *bit-exact + memory-safe* is the honest story.

## Vorbis encode

72 s of concatenated CC0/PD music (piano + guitar), best-of-7 (parallel) / best-of-5
(single), 24-core box. Realtime = clip duration ÷ encode time. libvorbis is
single-threaded per stream by design.

| Encoder · setting | bitrate | parallel (24-core) | single-thread |
|---|---:|---:|---:|
| **rff** −q:a 4 | 50 kb/s | **650× RT** | 98× RT |
| **rff** −q:a 6 | 85 kb/s | 537× RT | 74× RT |
| **rff** −q:a 8 | 139 kb/s | 443× RT | 60× RT |
| ffmpeg libvorbis −q:a 5 *(ref)* | 147 kb/s | 86× RT | 87× RT |
| ffmpeg libvorbis −q:a 7 *(ref)* | 204 kb/s | 84× RT | — |

At a matched ~140 kb/s: **parallel 443× vs 86× → ~5.2× faster**; **single-thread 60×
vs 87× → ~1.45× slower**. The frame-parallel encode is the lever libvorbis can't
answer; the single-thread deficit is our rate-distortion residue classifier doing more
work than libvorbis's cheaper heuristic (closed from 4.7× to ~1.4× via an energy-bucket
class shortlist).

### Quality (PEAQ ODG)

Encode → **ffmpeg** decode → PEAQ (numpy_PEAQ, validated to the MATLAB reference
ODG −3.875) with signed sample-accurate alignment. ODG scale [−4, 0]; **higher is
better**, 0 = transparent. At matched bitrate **libvorbis leads** — its psychoacoustic
model allocates bits better.

| clip | rate | rff ODG | libvorbis ODG |
|---|---:|---:|---:|
| piano (tonal) | ~85 kb/s | −2.25 | **−1.53** |
| piano (tonal) | ~120 kb/s | −0.87 | **−0.25** |
| guitar (transient) | ~135 kb/s | −1.4 (interp.) | **−0.91** |

- **libvorbis is ~0.6–0.7 ODG ahead on tonal content** at every matched bitrate.
- **Guitar shows a pre-echo dip:** rff's ODG *sags* near 50–70 kb/s (−3.8) even as
  waveform correlation *rises* (0.80 → 0.97). That's the long-block window smearing
  each attack backwards in time — audible pre-echo PEAQ penalises. **Block switching**
  (short blocks over transients) is implemented and TDAC-proven; enabling it (`VORBIS_BS`)
  lifts guitar +0.14 ODG. Env-gated pending a tuned detector + streaming integration.

Reproduce: `tools/quality/` (`fetch_corpus.sh`, `peaq_align.py`, `setup_peaq.py`).
Honest summary: **decisively faster (5–6× parallel), competitive single-thread (~1.4×
slower), behind on perceptual quality** (libvorbis's two decades of tuning lead).

## Caveats

- One machine, one synthetic clip, single-thread, wall-clock medians.
- Compared against ffmpeg's **native** VP9 decoder (faster than libvpx) — the
  decoder users actually run.
- **Cross-build comparisons need *interleaved* best-of-N.** Separate process
  invocations drift ±5–10% with thermal/scheduling state; a single
  before-vs-after run will manufacture phantom "wins" (we cited a phantom +30%
  from `target-cpu=native` before measuring properly). Alternate the two builds
  round-by-round and take the min — only a gap that survives that is real.
- Re-captured per release and as the decoder is optimized.

## Not yet benchmarked

MP3 / AAC / AVIF decode, and the remaining encode paths — same methodology, added as
they are tuned. (VP9 decode + Vorbis encode measured above.)
