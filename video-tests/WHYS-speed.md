# Six-Whys descent — why is rff-codec-vp9 slower (and worse) than ffmpeg's VP9?

**Unknown.** The `video-tests` analyzer at default settings shows our encoder behind
ffmpeg/libvpx on both speed and quality across the corpus. Find the real cause, down to
the functions and primitives, and fix them.

Run 2026-07-27, on `vp9-encoder-inter-bricks` at `7931876`.

Per `codec-six-whys-unknowns`: **depth 6 runs first.** Every level is closed by a
measurement, never by a mechanism I can explain.

---

## D6 — is the measurement sound?  (run FIRST)

### D6a — do both arms do identical work?  **NO.**

- **ASKED:** the `speed` pass runs "every arm at its own defaults". Are those defaults
  comparable?
- **MEASURED:** read the two configurations out of the code rather than the README.
  - ours (`Vp9Encoder::default`, `encoder.rs:364`): `qindex 64` (constant quality,
    equivalent to crf 16), `speed 3`, `lag 0` (no ALT-REF).
  - libvpx (`_ref_libvpx/_harness/vp9enc.c:88`): `vpx_codec_enc_config_default` =
    **256 kbps VBR**, `cpu_used 0`, `lag 25` (ALT-REF on).

  Four simultaneous confounds: rate-control *mode*, operating *point*, speed *preset*,
  and *lookahead*. The resulting output is not comparable:

  | clip | ours bytes / PSNR | libvpx bytes / PSNR | bits ratio | wall ratio |
  |---|---:|---:|---:|---:|
  | akiyo_cif | 29,502 / 43.21 dB | 32,690 / 45.11 dB | 0.90x | ours **1.34x faster** |
  | mobile_cif | 488,992 / 39.71 | 94,152 / 31.08 | **5.19x** | ours 1.14x faster |
  | city_4cif | 1,181,053 / 40.71 | 30,789 / 30.21 | **38.4x** | ours 2.96x slower |
  | park_joy | 7,771,678 / 40.90 | 108,255 / 25.55 | **71.8x** | ours 1.18x slower |

- **ANSWER:** on park_joy we emit **72x the bits at +15.3 dB** and take only 1.18x the
  time. That is not a speed comparison. The headline "we are slower and worse" is, at
  minimum, badly contaminated — and on the two clips where the rates are within 5x we are
  *faster*.
- **CONFIDENCE:** high — the configurations are read from source, the bytes and PSNR are
  measured in-process frame-by-frame.
- **SPAWNED:** D1 (re-measure at a matched operating point).
- **STATUS:** closed. Built `analyzer pareto`: both arms pinned to constant quality, the
  same cq-level, the same speed preset and the same lookahead.

### D6b — what is the noise floor?  **+/-10%.**

- **ASKED:** how large must a speed delta be before it is a result on this machine?
- **MEASURED:** `analyzer noise` — a NULL ARM: arms A and B are the identical encoder at
  identical settings, ABBA-interleaved, best-of-5 each (FRAMES=20, crf 32, speed 3):

  | clip | A best | B best | delta | worst/best |
  |---|---:|---:|---:|---:|
  | akiyo_cif | 832.8 ms | 749.7 ms | **-9.98%** | 1.40x |
  | foreman_cif | 1609.8 ms | 1536.7 ms | -4.54% | 1.16x |
  | mobile_cif | 3568.0 ms | 3655.4 ms | +2.45% | 1.15x |

- **ANSWER:** up to **10%** between two identical encoders. Any wall-clock A/B below that
  is a coin flip. Verdicts on smaller effects must come from the deterministic per-stage
  profiler (cycle counts), not from wall time.
- **CONFIDENCE:** high — reproduces the 2026-07-26 VP9 null-arm finding on this box.
- **STATUS:** closed. Adopted as the standing gate for every claim below.

---

## D1 — is the gap real at MATCHED settings?  **YES, and larger than the default table showed.**

- **ASKED:** with both arms in constant-quality mode at the same cq ladder, the same
  speed preset and the same lookahead, what is the speed and rate difference at the
  SAME reconstructed quality?
- **MEASURED:** `analyzer pareto`, crf {20,32,43,55} x speed 0 x lag 0, 20 frames,
  best-of-3, PSNR measured in-process frame-by-frame. Interpolated to the midpoint of
  each clip's PSNR overlap (`iso.py`, no extrapolation):

  | clip | PSNR | ours ms | libvpx ms | slower | ours kb/s | libvpx kb/s | bits |
  |---|---:|---:|---:|---:|---:|---:|---:|
  | akiyo_cif | 39.97 | 1528 | 291 | **5.25x** | 119 | 82 | +45.8% |
  | foreman_cif | 36.49 | 2889 | 1109 | **2.61x** | 439 | 295 | +48.8% |
  | mobile_cif | 33.58 | 4316 | 2168 | **1.99x** | 1647 | 1173 | +40.4% |
  | bus_cif | 34.08 | 3944 | 1755 | **2.25x** | 1081 | 902 | +19.9% |
  | **geomean** | | | | **2.80x** | | | **+38.2%** |

- **ANSWER:** dominated on both axes — ~2.8x slower AND ~38% more bits. The premise is
  correct. Note the default-settings table had reported us *faster* on akiyo and mobile;
  it was comparing our speed-3 against libvpx's cpu-used-0.
- **CONFIDENCE:** high. Deterministic encodes; the ratio ordering reproduces under the
  profiler (akiyo 3.11x, foreman 1.75x, mobile 1.52x on TOTAL).
- **STATUS:** closed.

## D2 — which stage owns it, in ABSOLUTE ms?

- **ASKED:** at crf 32 / speed 0 / lag 0, where does the 2025 ms akiyo delta sit?
- **MEASURED:** `analyzer stages` with `MATCH_CRF` (added, so both arms profile the same
  operating point — previously the encoder pass profiled two workloads 5-72x apart).
  Ours TOTAL 2984.6 ms vs libvpx 959.6 ms:

  | stage | ours | libvpx | delta | ours calls | libvpx calls |
  |---|---:|---:|---:|---:|---:|
  | orchestration/glue | 790.4 | 59.3 | **+731** | - | - |
  | fwd_tx + quantize | 509.9 | 57.1 | **+453** | 2,144,144 | 296,313 |
  | sub8x8 | 459.4 | 28.7 | **+431** | 20,493 | 5,916 |
  | int_search | 417.1 | 89.0 | **+328** | 26,882 | 73,735 |
  | interp_8tap | 205.0 | 49.8 | +155 | 1,154,902 | 427,317 |
  | snap_restore | 115.4 | - | +115 | 289,466 | - |
  | invtx_recon | 63.3 | 0.6 | +63 | 226,998 | 7,971 |

- **ANSWER:** the top four are ~1943 ms of the 2025 ms delta. The largest is the
  **unscoped residue**, and it is real work, not the instrument: the profiler reports
  `_INSTRUMENT_TAX` 457.6 ms separately and `_DECISION_WALL - TOTAL` = 457.5 ms matches
  it, so the tax is already outside `TOTAL` from which glue is derived.
- **CONFIDENCE:** high.
- **SPAWNED:** D3a (what IS the glue?), D3b (why 7.2x the tx-pipeline calls?),
  D3c (sub8x8), D3d (int_search 12.8x per call).
- **STATUS:** closed.

## D3a/D4a — the glue is a per-tx-block memset  **(FIRST CAUSE, fixed)**

- **ASKED:** 790 ms over 2,144,144 tx blocks is 369 ns per block of unnamed work. What?
- **MEASURED:** read `encode_tx_block`. It declared FIVE fixed-size stack buffers -
  `residual`, `coeffs`, `levels`, `dqcoeff` (`[0i32; 1024]`) and `token_cache`
  (`[0u8; 1024]`) = **~17 KB zero-initialised per call**, of which a 4x4 luma block uses
  320 bytes. 17 KB at L1 store bandwidth is ~340 ns — within noise of the 369 ns
  observed. Every buffer is fully written over `[..n]` before being read over `[..n]`,
  so the zero-init is dead work.
- **ANSWER:** confirmed. This is the 2026-07-17 "fixed-size stack array in a hot path is
  a hidden per-call memset" law, at 2.1M calls per 20-frame CIF encode.
- **FIX:** moved the five buffers into a boxed `TxScratch` owned by the encoder and
  `mem::take`-n for the duration of the call (a pointer move, and it keeps the
  `&self.src` / `&mut self.rec` borrows legal); restored at both exits. Also row-sliced
  the residual and recon-SSE loops, which were 2-D indexed and recomputed
  `(y0+y)*stride + x0+x` per pixel across two different strides.
- **GATE:** bitstreams BYTE-IDENTICAL on akiyo/foreman/mobile with `VP9_TX_MEMSET=1`
  (the oracle arm re-zeroes the scratch, reproducing the old cost exactly); 157 tests pass.
- **STATUS:** closed, landed.

## D6c — the rebuild's own measurement trap (logged because it nearly shipped a lie)

- Two `stages` runs of the SAME binary, minutes apart, differed by **2.5x on every
  bucket uniformly** - including `int_search` and `interp_8tap`, which the change cannot
  touch, at IDENTICAL call counts. That is CPU frequency state, not code.
- A sequential arm-A-then-arm-B comparison then read arm B (which does strictly MORE
  work) as *faster* on mobile - an impossible result, and the tell that the method was
  broken.
- **RULE ADOPTED:** every brick below is judged by `abba.sh` - ABBA-interleaved paired
  rounds in one loop, reported as a paired win rate with a z-score, plus best and median.
  A stale-binary check comes first: the analyzer is its OWN cargo workspace, so
  `cargo build -p rff-codec-vp9` in the main workspace does NOT rebuild it. That
  invalidated one byte-identity gate before it was caught.

## D3b — WHY 7.2x the transform calls?  **We are not slower per primitive; we run more of them.**

- **ASKED:** is the tx-pipeline delta a per-call cost problem or a call-count problem?
- **MEASURED:** per-call, from the SAME stages invocation (so one clock):

  | primitive | ours ns/call | libvpx ns/call | ratio |
  |---|---:|---:|---:|
  | fwd_tx + quantize | 237.8 | 192.6 (`fwd_tx+quant`) | 1.23x |
  | coef_cost | 60.7 | - | - |
  | intra_pred | 146.0 | 61.1 | 2.4x |

  but the CALL COUNTS are 2,144,144 vs 296,313 = **7.2x**. Per 4x4 luma position per
  frame that is ~17 transform evaluations for us against ~2.3 for libvpx.
- **ANSWER:** our primitives are roughly at parity (1.2-2.4x, and the transform itself is
  within 25%). The gap is **RD trial multiplicity** — libvpx reaches a decision after ~2
  transforms per position, we take ~17. This is the "an encoder that PROVES exactness
  where the reference ESTIMATES pays a structural 3-4x" law: libvpx's `model_rd_*`
  variance model and `rd_less_than_thresh` early-outs avoid the transform entirely for
  most candidates, most aggressively on static content — which is exactly where our gap
  is widest (akiyo 5.25x, mobile 1.99x).
- **CONFIDENCE:** high on the counts and the ratio; medium on the attribution of libvpx's
  mechanism (read from its stage shape and known source, not instrumented per-decision).
- **IMPLICATION:** the remaining speed gap is NOT a vectorisation or kernel problem.
  Closing it means cutting trials, which changes the bitstream and therefore belongs to
  `codec-search-skip-gate` / `codec-content-adaptive-dispatch` with a BD-rate gate — a
  campaign, not a brick. Do NOT spend more effort hand-optimising these kernels.
- **STATUS:** closed as a finding; the fix is out of scope for a byte-identical pass.

## D5b — second memset, on the compound path  (fixed)

- **MEASURED:** `pred_sse_compound` declared `[0u16; 64*64]` = **8 KB zero-init per
  call**, and it runs per compound candidate whenever `-lag` is active (compound is
  default-on at speed <= 3 with ARF). Same bug class as `TxScratch`.
- **FIX:** thread-local reused buffer; the ref-0 pass writes all of `[..w*h]` before the
  ref-1 pass blends and before the SSE loop reads, so reuse is sound. Row-sliced the SSE
  loop as well.
- **GATE:** 16/16 ladder points (4 clips x 4 CRFs, lag 25) identical in BOTH bytes and
  PSNR; 157 tests pass.

## Quality — the gap is NOT explained by ALT-REF

- **MEASURED:** the same iso-quality Pareto with lag 25 on BOTH arms:

  | | bits gap (geomean) | speed gap (geomean) |
  |---|---:|---:|
  | lag 0 both arms | +38.2% | 2.80x |
  | lag 25 both arms | +37.1% | **4.68x** |

- **ANSWER (quality):** with ARF *disabled on both sides* we still need +38% more bits at
  matched PSNR. So a large core-encoder efficiency gap exists independently of ALT-REF,
  and the campaign's working hypothesis ("the gap is almost entirely ALT-REF") does not
  survive a matched-settings measurement. **CAVEAT — the lag-25 row is inconclusive on
  quality: the clips are 20 frames, so a 25-frame lookahead never fills.** Re-run at
  FRAMES >= 60 before drawing any ARF conclusion. The lag-0 row has no such problem and
  is the trustworthy one.
- **ANSWER (speed):** enabling lookahead costs US 2.5x (akiyo 1528 -> 3788 ms) and libvpx
  only 1.5x (291 -> 430 ms). Our ARF/compound path is disproportionately expensive; that
  is a real, separate target.

## Rebuild — climbing back up through the gates

| level | gate | result |
|---|---|---|
| D5 primitive | oracle A/B, ABBA-paired | akiyo 16/16 z=+4.00; foreman 8/8; mobile 8/8; +ARF 8/8 |
| D4 magnitude | best-of-N per arm | -15.6% / -18.0% / -16.5%; **-22.9% with ARF+compound** |
| D3 correctness | bitstream compare | byte-identical, 3 clips (lag 0) + 16/16 ladder points (lag 25) |
| D2 suite | `cargo test -p rff-codec-vp9` | 157 pass, 0 fail |
| D1 workspace | `cargo build -p rff-cli` | clean |

### A closing measurement caveat, recorded so it is not re-litigated

The post-fix `pareto` run reported a geomean **3.23x** against the pre-fix run's **2.80x**
— i.e. it looked like a regression. It is not: in that run OUR times rose on three clips
while LIBVPX's fell 2-12%, which no change of ours can cause. The `pareto` ms column
carries the same +/-10-40% drift as everything else on this box; only the ABBA-paired
test controls for it, and it said -16..-23% over 32 paired rounds.

What *did* reproduce exactly across both runs is the **bits** column
(+45.8 / +48.8 / +40.4 / +19.9%, identical to the digit) — which is an independent
confirmation that the change altered no output.

**Therefore:** quote the speed gap as "roughly 3x at matched settings" (the honest
resolution of this harness), quote brick values only from `abba.sh`, and never compare ms
across two `pareto` invocations.

## Remaining ranked targets (for the next session)

1. **RD trial multiplicity — 7.2x libvpx's transform calls.** The dominant cause and NOT
   a kernel problem. Needs a trial-count gate (`codec-search-skip-gate`) or a model-based
   early-out, BD-rate-gated. Biggest prize, biggest risk.
2. **`sub8x8`** — 459 ms vs libvpx's 28.7 ms on akiyo; we enter sub-8x8 search on ~65% of
   8x8 blocks against libvpx's ~19%, and each entry costs more. A partition-descent gate
   is the natural shape (the null arm — whole 8x8 — usually wins on static content).
3. **`int_search`** — 12.8x libvpx's per-call cost (exhaustive vs diamond at speed 0).
   Previously measured as +2.05% BD for 1.18x; re-price it now that the profile has moved.
4. **Our ARF/compound path costs 2.5x where libvpx's costs 1.5x** — a separate, unexplored
   target surfaced by the lag-25 run.
5. **Re-run the ARF quality comparison at FRAMES >= 60** so the 25-frame lookahead fills.
