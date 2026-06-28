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

Three bit-exact attempts (each gated by the conformance suite), and what each
measured:

| Attempt | Result | Kept? |
|---------|--------|-------|
| Tile-column threading (clone, then no-clone fork) | **~1.7–3× *slower*** — per-worker buffer + merge is ~2× the memory traffic | no |
| Compound (averaged) inter SIMD | **~1.8%** (closed a 100%-scalar gap) | yes |
| `madd_epi16` core convolution | **~0%** (best-of-N tied with `mullo`) | no |

The pattern is the lesson: **VP9 decode here is memory-bandwidth-bound, not
compute-bound.** Inter prediction is dominated by *copying reference pixels*
(integer-MV blocks) and the loop filter by *moving pixels* — so SIMD-ing the
*arithmetic* (madd, compound) barely moves the needle, and adding threads just
adds memory traffic. The ~5–6× gap to FFmpeg is mostly structural (memory
layout/prefetch + decades of tuning), not a missing multiply trick.

**Conclusion:** the decoder is near its practical single-thread ceiling for a
safe pure-Rust implementation. Further compute-level SIMD is low-value; the only
real levers left are algorithmic memory-traffic reduction (hard) — or accepting
the gap, since *bit-exact + memory-safe* is the honest story. `target-cpu=native`
still buys ~30% on the non-kernel glue, for free.

## Caveats

- One machine, one synthetic clip, single-thread, wall-clock medians.
- Compared against ffmpeg's **native** VP9 decoder (faster than libvpx) — the
  decoder users actually run.
- Re-captured per release and as the decoder is optimized.

## Not yet benchmarked

MP3 / AAC / AVIF decode, and the encode paths — same methodology, added as they
are tuned.
