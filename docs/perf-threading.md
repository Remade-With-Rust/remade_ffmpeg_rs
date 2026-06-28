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

## Next: no-clone state-extraction

Split `Reconstructor` into shared read-only (`fc`, `refs`, dq, header) borrowed
by all workers, and a small per-tile `TileState` (bool decoder, left/above
contexts, `dqcoeff`/`token_cache`, counts) threaded through the decode methods.
Workers write into **tile-width scratch** (≈1/Ntiles the memory, with a
column-offset coordinate translation) — *not* a full-frame buffer — then merge.
This is more refactor (≈15 method signatures) but removes the copy that sank the
first cut.

## Verification

Every step: the conformance gate (`crates/rff/tests/vp9_conformance.rs`) stays
**bit-exact** (315/315 with the full set; 11/11 with the tile-focused set), and
the `cargo bench -p rff-codec-vp9` number is recorded. Threading that changes a
single output pixel is a bug, not an optimization.
