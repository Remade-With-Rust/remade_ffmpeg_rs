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

## Step plan (each step keeps 315/315 green)

1. **Extract** the per-tile-column decode (`decode_tiles`' inner loop) into a unit
   that takes explicit per-tile state instead of `self` fields — still called
   serially. Verify: identical output, 315/315 unchanged.
2. **Run the units on `std::thread::scope`** with per-tile scratch + the merge
   step. Verify: 315/315 unchanged; measure speedup on the 4-tile clip.
3. **Thread the loop filter** (by column strips, with boundary handling) — the
   second ~2× toward the ~4× ceiling.
4. A `threads` knob (default = available parallelism, `1` = today's path) and a
   benchmark row.

## Verification

Every step: the full libvpx conformance set stays **315/315 bit-exact**, and the
`cargo bench -p rff-codec-vp9` number is recorded. Threading that changes a
single output pixel is a bug, not an optimization.
