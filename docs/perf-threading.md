# Performance: multithreaded VP9 decode (design + plan)

Status: **design / in progress.** The decoder is single-threaded today. This is
the plan to parallelize it — pursued in small, individually-verified steps so the
315/315 libvpx conformance never regresses.

## Why, and how much

Profiling ([benchmarks.md](benchmarks.md)) puts ~94% of decode time in **inter
prediction (~53%)** and the **loop filter (~41%)** — both already AVX2.
Out-optimizing FFmpeg's decade-tuned kernels is slow, incremental work.
Threading is the higher-leverage lever, and VP9 is built for it: **tile columns
decode independently** (separate bool decoder, disjoint column ranges, no
cross-tile reads — intra at a tile's left edge treats it as unavailable). Our
benchmark clip already carries **4 tile columns**.

**Amdahl reality (4 tiles):**
- Inter pred + intra + tokens + transform (~59%) happen *during* tile decode →
  parallelizable. The loop filter (~41%) runs *after*, across tile boundaries.
- Threading tile decode only: `0.59/4 + 0.41 ≈ 1.8×`. The serial loop filter caps
  it.
- Threading the loop filter too (separate effort): up to `~4×`.

So the full win is **two** parallelization efforts; tile-decode first (~1.8×),
loop filter second (toward ~4×).

## State separation (the crux, under "no `unsafe`")

Per-tile-column **mutable** state — each worker needs its own:
`left_ctx`, `left_seg`, `dqcoeff`, `token_cache`, the bool decoder, its slice of
`above_ctx`/`above_seg`, its column range of the output planes + `mi` grid, and
its own backward-adaptation `counts`.

**Shared, read-only** across workers: the frame-context probabilities (`fc`),
reference frames (`Arc<RefFrame>` — already `Send + Sync`), dequant tables, and
the frame header.

The disjoint-but-interleaved plane writes (column ranges in a row-major `Vec`)
are the reason we can't just `split_at_mut`. The safe answer: each worker
decodes into its **own full-frame-coordinate scratch** (writing only its column
range, reading its own range + the shared refs), then a single-threaded **merge**
copies each worker's column strip + `mi` range into the frame, and sums the
`counts`. No `unsafe`, threads via `std::thread::scope`.

## First cut: clone-per-tile — built, verified, **rejected on perf**

The pragmatic first cut cloned the whole `Reconstructor` per tile column
(`#[derive(Clone)]`), decoded each on `std::thread::scope`, and merged the pixel
strips + `mi`/seg ranges + `CountAdd`-summed counts back. It was **bit-exact —
11/11 conformance vectors, including the multi-tile `tile-4x4`/`tile_1x*`** — so
the whole parallel-decode-and-merge mechanism is proven correct.

But it was **~3× slower** on the 4-tile 720p clip (105 vs ~315–390 Mpixels/s).
The deep clone copies the full frame state — padded planes (~5 MB) + the
mode-info grid (~1 MB) — **~25 MB per frame**, which costs more than the
parallelism saves at these frame sizes. Reverted; the lesson is the deliverable:
**a tile worker must not own a full-frame copy.**

## Second cut: no-clone fork — built, verified, **also slower**

Removed the deep copy: each worker shares the read-only state (cloned-cheap
`fc`/`refs`) but gets **fresh, lazily-zeroed scratch** (planes / `mi` / contexts)
instead of a copied full frame, then merges its column strip back. Still
**bit-exact (11/11)**, and faster than the clone — but still **~1.7× slower than
serial, at every frame size**:

| | 720p 4-tile | 1080p 4-tile |
|---|---|---|
| serial | 350 | 302 |
| clone-per-tile | 105 | — |
| no-clone fork | 185 | 181 |

(Mpixels/s, i7-14650HX.) The overhead is **per-pixel, not per-frame**, so it
doesn't amortize on bigger frames: the worker faults in a full frame of fresh
buffer, and the merge copies a full frame back — roughly **2× the memory traffic
of serial**, and VP9 decode is memory-bandwidth-bound.

## Conclusion: safe tile threading doesn't pay off for VP9

Two honest attempts, both bit-exact, both slower. The wall is structural:

1. **Amdahl** — the loop filter (~41%) runs serially after tile decode, capping
   tile-decode threading at **~1.8×** even with *zero* overhead.
2. **The safe (no-`unsafe`) tax** — without a shared-buffer disjoint write,
   every worker needs its own frame buffer + a merge copy, and that ~2× memory
   traffic erases the ≤1.8× ceiling.

Winning would require **`unsafe`** (threads writing disjoint columns of one
shared buffer, no per-worker buffer, no merge) — which breaks the project's
"no `unsafe` in the tree" promise — *plus* threading the loop filter, *plus* a
thread/buffer pool. Not worth trading the project's defining property for a
sub-2× that fights memory bandwidth.

**Recommended pivot:** put the (now-restored) conformance gate behind the
**inter-prediction + loop-filter SIMD kernels** instead — that's the 94%, it
helps *every* stream (not just multi-tile), and it needs no `unsafe` and no
threading. Single-threaded-but-memory-safe stays the honest decode story.

## Verification

The conformance gate (`crates/rff/tests/vp9_conformance.rs`) stays **bit-exact**
(11/11 tile-focused set; 315/315 with the full set) for any decoder change, and
`cargo bench -p rff-codec-vp9` records the number. A change that moves one
output pixel is a bug, not an optimization.
