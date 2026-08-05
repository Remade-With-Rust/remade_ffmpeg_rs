# WHYS — rusty_png

The measured descent that created this fork. Rules: every "why" is closed by a
number that was taken, never by a mechanism that can be explained. Refuted
hypotheses keep their number so they do not come back.

Machine: i7-14650HX, 24 logical CPUs, Windows 11. Reference: FFmpeg 8.1.2
(gyan.dev full build, static zlib). Harness: pinned to core 2, priority High,
on-core cycles via `QueryProcessCycleTime`, ABBA-interleaved, median AND
min-of-N, paired win-rate + z. **Null-arm floor 2.0–2.3%** — nothing smaller is
a result.

---

## D6a — is the instrument sound? (run FIRST)

- ASKED: can `TotalProcessorTime` resolve a PNG transcode?
- MEASURED: no. Every reading was an exact multiple of **15.625 ms** (the
  Windows scheduler tick); the rff startup arm read a literal **0 ms**.
- ANSWER: replaced with `QueryProcessCycleTime`. Reconciliation check later
  confirmed the replacement: implied clock 2.32–2.67 GHz for *both* binaries and
  2.42 GHz on a CPU-bound control, against a 2.2 GHz base part.
- STATUS: closed.

## D6b — resolution floor

- MEASURED: ffmpeg-vs-ffmpeg null arm `ratio_med 1.0199`, z = +1.09;
  rff-vs-rff `ratio_med 1.0233`, z = +1.53. Both correctly inconclusive.
- ANSWER: **floor ≈ 2.0–2.3%**.
- STATUS: closed.

## D6c — fixed per-invocation overhead

- MEASURED (16×16 PNG, full process, ~no codec work): rff **21 Mcyc**, ffmpeg
  **94.6 Mcyc**; oracle **16.8 Mcyc**. ffmpeg's launch is 4.5× ours.
- ANSWER: any row whose residual does not clear 3× the overhead being subtracted
  is reported as `startup-dominated`, never as a ratio. On the small corpus
  images that is most rows.
- STATUS: closed. This is why the corpus grew a 4K mosaic and 4–6× tall inputs.

## D1 — is there a gap, at matched settings?

- MEASURED: corpus total rff 31.76 MB vs ffmpeg 26.43 MB = **+20.1% larger**,
  while being several times faster.
- ANSWER: not one gap — the two ship at **different operating points on the same
  curve**. Comparing defaults prices a configuration difference as a codec
  difference.
- STATUS: closed.

## D2 — which configuration?

- MEASURED (read from both sources): upstream `png` →
  `Compression::Fast` (fdeflate) + `FilterType::Sub` + `NonAdaptive`, because
  `Info::with_size` defaults there and `rff-codec-png` overrides none of it.
  ffmpeg → `-pred paeth` + zlib `Z_DEFAULT_COMPRESSION` (level 6).
- STATUS: closed.

## D2b — does the sign flip across content? (the dispatch question)

- MEASURED, our bytes vs ffmpeg's, same pixels:
  | image | class | vs ff `sub L1` | vs ff default |
  |---|---|---:|---:|
  | park_joy_1080p | photo, noisy | **−11.4%** | +16.5% |
  | FourPeople_720p | photo, static | **−10.5%** | +34.4% |
  | mobile_cif | photo, detail | +0.6% | +25.8% |
  | ui_flat | graphics, flat | +24.1% | +653.9% |
- ANSWER: **the sign flips.** Against ffmpeg's low-effort configs we win on
  photographic content and lose badly on graphics. A sign-flip is a dispatch
  signal, not a mean to average away.
- STATUS: closed → drives the content-adaptive default, not a "fix".

## D3 — where does the encode cost actually sit? ★ THE FORK'S REASON

- ASKED: at a MATCHED SIZE (not a matched flag), who is faster?
- MEASURED, park_joy tall (8.3 MPx), single core, codec-only Mcyc, sizes exact:
  | config | Mcyc | bytes |
  |---|---:|---:|
  | ffmpeg default — paeth + **zlib** L6 | 1,472 | 14,837,375 |
  | ours `default/up` — **miniz_oxide** | 3,827 | 13,910,262 |
  | ours `best/up` — **miniz_oxide** max | 6,543 | 13,647,420 |
- ANSWER: we emit 6.2–8.0% smaller files for **2.6–4.4× the CPU**. PNG filtering
  is a minor share at those settings; the **DEFLATE stream** is the hot spot.
  Upstream routes `Default`/`Best` through `flate2` → `miniz_oxide`
  (`encoder.rs:1719-1720`).
- CLASSIFICATION: not a kernel defect and not a PNG defect — a **backend**
  choice. Routes to the deflate backend, not to `codec-vectorize-kernel`.
- CONFIDENCE: high — sizes are deterministic, and the speed rows quoted are the
  ones that cleared the startup filter.
- STATUS: closed → **brick 1 = `zlib-rs` backend**.

## D4 — REFUTED hypotheses (kept so they do not return)

- **H1: ffmpeg's CLI threading inflated its cycle count, since
  `QueryProcessCycleTime` sums all threads.** REFUTED twice: peak thread count
  read 1, and ffmpeg pinned to one core cost *fewer* cycles than unpinned
  (tax 0.93–0.97×), i.e. confinement was not penalising it.
- **H2: the cycle counter disagrees with ffmpeg's own `-benchmark`, so one
  instrument is broken.** REFUTED — that was an error of mine, not the
  instrument: I compared `-benchmark` on the `-f null` job (156 ms, discards
  output) against cycles from the `-f rawvideo` job (1,084 Mcyc, writes 24 MB).
  Two different jobs. Reconciled, both instruments agree at ~2.3–2.7 GHz.
- **H3: our decoder is ~2.5× faster than ffmpeg's** (from probe P3, which read
  the other way). P3 was NOT symmetric — it charged ffmpeg for process launch,
  demux and file read while timing our side in-process with none of those. The
  symmetric re-run is what decides this; do not quote P3.

## D2b — the unexplained encode residue (SOLVED)

- ASKED: at the shipped `Fast`/`Sub` default the profiler left **11–40%** of
  encode unattributed (gfx_terminal 40.1%, ducks 18.1%, park_joy 17.9%), while
  `Default`/`Best` left only 0.4–3.5%. What is it?
- D6 FIRST — is it the profiler's own tax? Priced: 4,320 rows × 2 scopes × 2
  `Instant` reads ≈ 17,280 calls ≈ **0.5 ms**, against a residue of **15.2 ms**.
  Tax is ~3% of the residue. It is real work. Closed.
- MEASURED: two regions had no scope — fdeflate's `finish()` (outside the
  per-row loop) and `write_zlib_encoded_idat`. Scoping both collapsed the
  residue to **2.9–3.7%**:

  | image | filter | deflate | **enc.chunk** | enc.finish | residue |
  |---|---:|---:|---:|---:|---:|
  | park_joy | 4.1% | 77.0% | **16.1%** | 0.0% | 2.9% |
  | ducks_take_off | 4.1% | 76.7% | **16.0%** | 0.0% | 3.2% |
  | gfx_terminal | 7.2% | 77.0% | **12.1%** | 0.0% | 3.7% |
  | gfx_uiart | 7.4% | 82.1% | **7.6%** | 0.0% | 2.9% |

- ANSWER: the residue was **`write_zlib_encoded_idat`** — CRC32 plus the IDAT
  write — at 7.6–16.1% of encode. `enc.finish` reads **0.0%**: that hypothesis
  was refuted, cheaply, by one scope.
- **D3 — which op inside it?** Split further: `enc.crc` = 1.630 ms for 17.28 MB
  = **10.6 GB/s**, i.e. the hardware CRC32 path is working and is not the
  problem. The remainder is `write_all`: 10.14 ms for 17.28 MB = **1.70 GB/s**,
  far under memcpy — the signature of `Vec` reallocation growth.
- **D5 — ceiling probe before building.** Pre-sizing the output `Vec` took the
  stage from 14.755 ms to 6.247 ms (**−58%**).
- **REBUILD GATE — and it FAILED at the level above.** Paired ABBA A/B of the
  fixed binary against a pre-change binary, output byte-identical throughout:
  **1.017× / 0.982× / 0.983× / 0.974×** at 0.4–2 MPx and **1.010×** at 8.3 MPx.
  Every one inside the 2.0–2.3% null-arm floor.
- VERDICT: **kept, but NOT as a speedup.** It removes genuinely redundant
  copying and cannot alter output, so it stays; the whole-pipeline effect is
  unmeasurable and must not be quoted. Reverting the *claim*, not the code.
  Recorded as "delta sat inside the noise", not "measured worse".
- LESSON: the −58% came from two single un-interleaved runs. The stage profiler
  is fine for ATTRIBUTION (which stage owns the time) and untrustworthy for
  DELTAS; those need the paired harness.
- STATUS: closed. Consequence for the roadmap below: with the residue named,
  encode is **77–82% deflate at `Fast`** and **94–99.5% at quality**, so nothing
  outside DEFLATE can move the standing benchmark.

## D3a — parallel DEFLATE (BUILT)

- ASKED: with DEFLATE at 77–99.5% of encode and ffmpeg's zlib 1.09–1.45× ahead
  single-threaded, what actually moves the standing benchmark?
- ARITHMETIC FIRST (prune before building): Amdahl on a 97% parallel stage gives
  ~6.6× at 8 threads — far more than the 1.20× needed for parity. **The speedup
  was never the risk.** The size cost was, and it is deterministic, so it was
  measured before a line of threading was written.
- MEASURED (pessimistic bound — independent streams, no dictionary priming):

  | filtered | bytes/block | size delta |
  |---|---|---|
  | 24.9 MB | 1.04 MB | **+0.11%** |
  | 2.35 MB | 98 KB | +1.64% |
  | 1.44 MB | 60 KB | **+7.44%** |

- ANSWER: the cost tracks **bytes per block**, not block count. So blocks are
  *sized* (`PAR_MIN_BLOCK` = 1 MiB), never counted, and an image too small to
  yield two of them stays serial and pays nothing.
- BUILT AND MEASURED (level 6, zlib-rs, 8 workers):

  | image | filtered | serial | parallel | speedup | size |
  |---|---|---|---|---|---|
  | park_joy 8.3 MPx | 24.9 MB | 698 ms | 148 ms | **4.71×** | +0.03% |
  | blue_sky 8.3 MPx | 24.9 MB | 873 ms | 162 ms | **5.40×** | +0.04% |
  | gfx_uiart 3.9 MPx | 11.7 MB | 190 ms | 29 ms | **6.53×** | +0.05% |
  | gfx_chart 0.5 MPx | 1.44 MB | 5.1 ms | **1 block** | 1.04× | **+0.00%** |

  gfx_chart is the row that matters: it refuses to split, so the +7.44% never
  happens.
- GATED: the split stream is valid zlib and round-trips byte-for-byte at
  1/2/3/4/8 workers *decoded by flate2, not by our own decoder*; end-to-end, a
  PNG encoded through the parallel path decodes to identical pixels, with an
  assertion that a split actually occurred so a silent serial fallback cannot
  pass as success.
- STATUS: closed. Opt-in (`parallel` + `set_parallel`), because it changes the
  compressed bytes — never the pixels.

## D2c — the fixed default is an unfinished dispatch (SOLVED)

- ASKED: `Fast`/`Sub` is faster *and* smaller than every ffmpeg
  `-compression_level 1` config on photographs, and **+130.1%** against ffmpeg's
  default across nine real graphics assets (up to +1409% on a chart). What signal
  separates them?
- MEASURED — fraction of horizontally repeated pixels (DEFLATE exploits LZ77
  matches, so this is the cheapest honest proxy), sampled over ~64 rows:

  | class | signal |
  |---|---|
  | photographic (9 Derf frames) | 0.0366 – **0.2037** |
  | real graphics (9 assets) | **0.5312** – 0.9790 |

  Nothing lands between 0.204 and 0.531, so the 0.35 threshold sits in an **empty
  band**, not on a fitted boundary.
- CONFIG CHOSEN BY CORPUS TOTAL, not by counting per-image winners (which were
  spread across `best/sub`, `best/up`, `best/paeth`, `default/adaptive`):

  | config | total vs ffmpeg | worst image | encode time |
  |---|---|---|---|
  | fast/sub (shipped) | +130.1% | +1409.0% | 451 ms |
  | **default + adaptive** | **−2.4%** | **+0.7%** | **502 ms** |
  | best + adaptive | −6.3% | −3.3% | 3,638 ms |

  `best` buys 3.9 more points for **8.1×** the time — a bad default however good
  the number looks in isolation.
- RESULT end to end: graphics corpus **5,443,716 → 2,372,444 B (−56.4%)**, i.e.
  **+115.6% → −6.1% vs ffmpeg**; photographs **byte-identical** (the dispatch
  correctly does not fire); 13/13 lossless; an explicit `-compression_level` /
  `-pred` still overrides the dispatch.
- BUG FOUND BY THE UNIT TEST, not by measurement: `PLTE` is 3 *incompressible*
  bytes per entry, and on a 64×40 40-colour frame indexing cost **more** than it
  saved (252 B vs 188 B). Small inputs now encode both candidates and keep the
  smaller; above 1 MB of raw data the palette is ≤768 B and the check is skipped.
- STATUS: closed.

## Correctness findings (these outrank the descent)

1. RGB path clean: 30/30 cross-checks pixel-exact — our PNG decoded by ffmpeg,
   ffmpeg's PNG decoded by us, and our self round-trip, on all 10 images.
2. **16-bit is silently reduced to 8-bit.** `STRIP_16` is unconditional and
   `transform_row_strip16` keeps the **high byte** (`v >> 8`) instead of
   rounding. Measured: 34.0% of bytes differ by 1 LSB from both ffmpeg's decode
   and the original, where ffmpeg was exact.
3. **Grayscale and palette are expanded to RGB(A)** and re-emitted that way:
   gray 259,303 B → 924,819 B (**+257%**); a 64-colour pal8 graphic
   6,522 B → 73,232 B (**+1023%**). Pixels still match; the file inflates.

---

## ROI ledger

Ranked by **measured gap × achievability**, not by how interesting the work is.
Every "prize" below is a number already taken; nothing is ranked on a hunch.

### The two standing facts that set the ranking

- **Decode: we are already 2.67–2.95× AHEAD of ffmpeg** (six real images, 15/15
  paired wins each, z = 3.87, two independent framings agreeing). ⇒ **Do not
  spend optimisation effort on decode.** Any decode work has a prize of roughly
  zero because we are not behind.
- **Encode: the whole gap lived in DEFLATE, not in PNG.** Brick 1 moved
  `default/up` on park_joy from 4,192 → 1,637 Mcyc — a 2.56× whole-encode win
  from swapping *only* the backend, which is only possible if deflate was the
  large majority of encode time.

### Ranked bricks

| rank | brick | measured prize | cost | status |
|---|---|---|---|---|
| **1** | **Expose compression/filter/adaptive** (`rff-codec-png` + CLI) | Unlocks everything below. Today the CLI reaches **no** operating point but one: `-pred` does not exist and `-compression_level` is routed to *audio*. On real graphics this is the difference between **+130.1%** and **−5.7%** vs ffmpeg. | ~zero perf work; plumbing only | **do first — it is a GATE, not an optimisation.** Bricks 1 and 3 deliver nothing to a user until this lands |
| **2** | **`zlib-rs` deflate backend** | `Compression::Default` **1.68–2.72× faster**, size neutral (+1.3%…−3.0%). Closes encode-at-matched-size from **2.6× behind → ~1.1× behind** ffmpeg (1,637 vs 1,472 Mcyc) while staying **6.0% smaller**. Pure Rust, no C. | one feature flag | **MEASURED, ready.** ⚠ sign flip at `Best` — see below |
| **3** | **Grayscale / palette passthrough** | gray **+257%**, pal8 **+1023%** file inflation today. Structural: we expand to RGB(A) on decode and re-emit that way. Largest single size win in the corpus. | adapter-level, no kernel work | designed, not built |
| **4** | **Content-adaptive default** | The best config is **content-dependent and measured so**: `best/up` on charts, `best/sub` on screenshots, `default/sub/ad` on diagrams, `best/paeth` on UI art. Our one fixed default is +130.1% on real graphics. | needs a cheap content signal | blocked on brick 1 (the gate) |
| **5** | **16-bit without truncation** | 34.0% of bytes off by 1 LSB, bit depth silently halved. Correctness, not speed. | small | designed |
| — | ~~decode optimisation~~ | **prize ≈ 0 — we are 2.8× ahead** | — | **explicitly not doing** |

### ⚠ Open sign flip (brick 2)

At `Compression::Best`, zlib-rs **lost** on the synthetic flat-graphics image —
0.68× speed (z = −3.87, a real result) while producing a **10.85% smaller**
file. It is buying compression with time. Photographic content showed no such
flip (1.68–2.36× faster).

Per the standing rule a sign flip is a dispatch signal, not a mean to average —
but this one is **not yet admissible**, because it was seen only on synthetic
content. `brick1_realgfx.ps1` reproduces the A/B on the nine real graphics
assets. If it holds there, `Best` needs a per-content backend choice; if it does
not, it was an artefact of flat synthetic fills and zlib-rs goes on unconditionally.

### Brick gates

| # | brick | gate |
|---|---|---|
| 0 | vendor upstream verbatim | ✅ 600/600 byte-identical vs `png` 0.17.16 (20 images × 30 configs) |
| 1 | expose knobs | every operating point reachable from the CLI; default output byte-identical to today |
| 2 | `zlib-rs` | ✅ size neutral at equal level; ✅ speed win outside the 2.3% floor; ⚠ real-graphics `Best` A/B outstanding |
| 3 | gray/palette passthrough | pixels identical, colour type preserved, size strictly down |
| 4 | content-adaptive default | **no image regresses** vs today's default; decided on real content only |
| 5 | 16-bit | exact vs ffmpeg on a TRUE 16-bit source (not one up-converted from 8-bit) |

---

## Correction: the encode number was measured by transcoding

*Recorded after the fact. The figures above this line were what the instrument
said at the time; this is what a better instrument said later.*

Adding the public **CLIC professional** corpus produced a run in our favour —
1.04–1.34× per core at matched filter and level — that **contradicted our own
published 0.69–0.92×**. A result in our favour that contradicts a published
number gets checked harder, not accepted, so the Derf fixture was re-run under
the same instrument. It also moved: 0.83–0.90× → 1.06–1.21×.

Both corpora moving the same way is not a content effect. It is the method.
Both arms were invoked as `-i image.png -c:v png`, so **both arms decoded and
then encoded**. Our decode is ~2.6× ffmpeg's. A decode win was being reported in
a row labelled ENCODE, on both corpora, which is why they agreed.

Re-measured with **neither arm decoding** — ours reads `.rgb24` and calls the
encoder, ffmpeg reads the same `.rgb24` through the rawvideo demuxer, Up filter
non-adaptive, level 6, one thread, launch subtracted:

| corpus | encode-only, per core | size |
|---|---|---|
| CLIC professional photographs (n=7) | **0.94–1.05×**, median 0.97× | −0.2% |
| Derf video frames (n=4) | **0.86–0.91×**, median 0.87× | −0.3% |

And decode measured **alone**, same instrument: **2.55–2.89×**, median 2.60×
(n=5 admissible; smaller images refuse because ffmpeg's decode work falls under
3× its own launch overhead).

So the published 0.69–0.92× was **directionally right and numerically stale** —
we have since moved to parity on photographs — and the corpus split is real but
small. Neither correction came from the codec changing during the check.

**The lesson, and it is the second time this session:** for a codec with a large
win in one direction, *any* pipeline that runs both directions will smuggle that
win into the other one's row. Measuring encode means feeding raw pixels in.
`-i x.png -c:v png` is a **transcode**, and it is a legitimate number — it is
just never the encode number.

---

## Encoder optimisation sweep: four dead ends and one real result

Went looking for slow functions in the encoder after the corrected benchmark
showed ffmpeg still ahead per core. Recorded in full because four of these look
obviously worth trying, and the next person will try them again otherwise.

**First, where the time actually is.** Stage profile, park_joy 1920x4320:

| config | filter | deflate | chunk | crc |
|---|---|---|---|---|
| `fast`/sub | 4.1% | **77.9%** | 15.2% | 2.6% |
| `fast`/sub/adaptive | 22.9% | **64.3%** | 10.9% | 4.4% |
| `default`/sub/adaptive | 2.9% | **95.1%** | 1.6% | 0.3% |
| `best`/up | 0.2% | **98.7%** | 1.0% | 0.1% |

At the matched-ffmpeg operating point (level 6, non-adaptive) everything that is
ours totals **3.0%**, against a 14% deficit. Zeroing all of our own code cannot
close that gap. It is a deflate gap, and deflate is zlib-rs.

**Per-filter, same frame:** up 8.58 GB/s | none 5.92 | sub 5.88 | avg 5.91 |
paeth 3.00. none/sub/avg touch the same streams and land within 1% of each
other — that is read+write at ~12 GB/s, i.e. **memory-bound and finished**.

### Refuted

1. **Batch the per-row deflate calls.** The serial path calls the compressor
   twice per row and one call carries a single byte, so 8,640 calls for a
   4,320-row image. Measured: per-row-two-calls **816.8 ms**, per-row-one-call
   1015.8, batched-256 KB 829.8, single-call 836.5. The shipped pattern is the
   *fastest* of the four, and the one arm that only removed calls got worse.
2. **SIMD `sum_buffer`.** Called 4x per row in adaptive mode, sum of absolute
   deviations, textbook. It already runs at **24.83 GB/s**; a u32 accumulator
   got 8.52, u16 20.24, 4-lane u32 10.82, fixed-array 5.24. LLVM's output for
   the existing u64-over-`chunks_exact(32)` form beats every rewrite offered.
3. **Branchless Paeth.** The three-way `if/else if/else` select looked like the
   reason Paeth runs at half the rate of the other filters. Rewrote it as
   min/eq + AND/OR masks (verified equal on all 2^24 triples) with
   `inline(always)`. Whole-process A/B: inside a control arm that itself swung
   0.878-1.019x. Same-process ABBA at full 24.9 MB size, which reproduces the
   encoder (3.51 GB/s vs 3.00): **0.966x median, 1.002x min, 7/15 wins,
   z = -0.26**. A tie. Paeth *is* genuinely slower than the others, but the
   branchy select is not why.
4. **Pre-size the internal deflate buffer.** It grows from zero by doubling to
   ~17 MB. Pre-reserving measured 0.843x-1.139x — inside noise — and over-
   reserves badly on graphics, where output can be 2% of input. Reverted. This
   is the same class as the `emit` pre-size in `rff-codec-png`, which is kept
   with the same honest caveat.

### Kept

**The whole-frame clone in `rff-codec-png::encode_png`.** It did
`vf.planes[0].clone()` whenever the source rows were already tight — the common
case — duplicating a buffer it already held. Now `Cow::Borrowed`.

Speed is *also* inside noise (0.924-1.124x, median 1.003x at shipped defaults),
so it is **not** a speedup and must not be quoted as one. What it does buy is
measured and well outside noise: **peak working set down 23-28%**, exactly one
frame — 101.0 -> 76.1 MB on park_joy, 86.4 -> 62.6 on blue_sky, 102.2 -> 78.5 on
ducks_take_off, 56.9 -> 43.2 on FourPeople. Output byte-identical, 80 tests pass.

### The standing conclusion

Our own encoder code is at its ceiling: the filters are memory-bound, the
adaptive sum is already optimal, and the buffer handling is dominated by
first-touch page faults on freshly allocated multi-megabyte buffers. **The
remaining gap to ffmpeg is DEFLATE and nothing else.** Two levers are left, and
both are structural rather than micro:

- **zlib-rs vs C zlib.** 96-99% of encode at quality levels. Not our code.
- **Streaming IDAT.** The encoder builds the entire compressed stream in a temp
  `Vec` and then copies all of it into the writer, because an IDAT carries its
  length in front of its payload. Encoding to `io::sink()` — identical
  compression, no second buffer — is **9.5-19.3% faster at `fast`**. Capturing
  it means emitting fixed-size IDAT chunks as compression proceeds, which is
  spec-legal and what libpng does, but changes the file's chunk layout and would
  break the byte-identical-to-upstream gate. That is a design decision, not an
  optimisation.

---

## Streamed IDAT: the double-buffer is gone

The previous entry left one structural lever open, and this is it.

`write_image_data` accumulated the ENTIRE compressed stream in a `Vec` and only
then copied all of it into the caller's writer — 17 MB built and then copied on
an 8.3 MPx frame. The reason was real: an IDAT chunk carries its length ahead of
its payload, so the total had to be known before anything could be written.

Emitting **fixed 256 KiB IDAT chunks** removes the need to know the total at
all — each chunk's length is known the moment its buffer fills. PNG permits any
number of IDATs and decoders concatenate their payloads, so this is a container
choice, not a format one.

### What it cost and what it bought

| | |
|---|---|
| peak memory, level 6 | **-16% to -19%** (park_joy 71.1 -> 57.8 MB) |
| peak memory, cumulative with the clone fix | **-39% to -40%** at this config (park_joy 94.9 -> 57.6 MB) — see the correction at the end of this file |
| speed | 0.981x - 1.062x, **median 1.023x — inside noise, not claimed** |
| file size | **+0.0045%** (12 bytes of framing per 256 KiB) |
| DEFLATE payload | **byte-for-byte unchanged** (14,636,550 B both ways) |
| upstream decodes ours to source pixels | **330/330** |

**The speed estimate in the previous entry was wrong, and worth saying why.**
It quoted 9.5-19.3% from an `io::sink()` probe measured at `Fast` — but `Fast`
is precisely the path that CANNOT stream, because it compresses, then compares
the finished size against `StoredOnlyCompressor`'s bound and re-encodes in
stored mode if compression lost. Measured before relying on it: fdeflate
expands uniform random bytes **1.3686x** and the fallback genuinely fires, so
dropping it would make incompressible images ~37% larger. Where streaming does
apply (`Default`/`Best`), the job is 95-99% DEFLATE, so removing ~11 ms of
buffering is ~2% and unmeasurable. Ceiling probes have to be run on the path
that will actually receive the fix.

### Three things that were nearly silent bugs

1. **Parallel DEFLATE.** The streaming check sits in FRONT of the `match`, so an
   early return would have routed every multi-threaded encode down the serial
   path — disabling a 2.11-3.06x feature with no test failing, because the
   output would still be correct. `par_active` is checked explicitly; the gate
   is that `-threads 8` output stays byte-identical to before AND still emits a
   single IDAT.
2. **APNG.** An animation frame has an fcTL that must precede it, and frames
   after the first are fdAT rather than IDAT. Streaming needs its destination
   ready before compression starts, so both keep the buffered path.
3. **`Write::flush`.** Wrappers call it at arbitrary points; honouring it by
   emitting a chunk would scatter short IDATs through the stream. It forwards to
   the inner writer and nothing else — the trailing partial chunk goes out in
   `finish`, which is explicit because `Drop` cannot report an I/O error.

---

## Streamed IDAT, part 2: the parallel path

The previous entry left parallel DEFLATE on the buffered path, on the grounds
that it "joins its workers' blocks into one buffer". That was true but it was
not a reason — the join was itself avoidable.

The layout this module builds is `[header] [blocks in order] [Adler-32]`. The
header is a function of `level` alone and the checksum is over the
*uncompressed* input, so **nothing in the stream depends on the total compressed
size** and the whole thing can go out front-to-back. Workers are joined in
order and each block is written and dropped as it arrives — joining in order is
not a scheduling constraint (the threads run concurrently either way), it is
what lets the bytes leave in stream order without staging them first.

What was actually there was worse than one buffer. The returning form made
**three** full-size copies of the compressed data:

1. each worker's own `Vec`,
2. `raw`, concatenating all of them,
3. `assemble`, copying `raw` again to put two header bytes in front,

and the encoder then copied the result into the writer. `assemble` is now
`zlib_header`, returning `[u8; 2]`, and (2) and (3) are gone. The workers'
buffers remain — bounding those means bounding concurrency, which is the
feature.

| | |
|---|---|
| peak memory, `-threads 4/8` | **-8% to -11%** (park_joy 95.2 -> 84.5 MB) |
| IDAT payload | **byte-identical at 2, 4 and 8 threads** (sha256 match) |
| speed, cycles and wall | **inside a ±9% null-arm floor — not claimed** |

### The measurement notes, because two instruments lied first

- The standing harness **pins to one core**. That is right for every
  single-core comparison in this project and wrong here: it ran all eight
  workers on one core. The A/B stayed controlled (both arms pinned alike) but
  the wall figures described nothing real.
- `Start-Process -Wait` reported ~1010 ms for four images of very different
  sizes. A number that does not move with the work is the instrument, not the
  result. `-PassThru` + `WaitForExit` gave 245-324 ms and tracked image size.
- Unpinned, streamed looked 1.017-1.073x faster on 4/4 images — then the
  **null arm** (same binary in both slots) spread 0.966-1.089x. The effect was
  smaller than the floor, so it is recorded as no result. Its absolute times
  also drifted from the treatment run's, which is the reason to run one.

**Standing tally for this whole sweep: six speed hypotheses, six results inside
noise; three memory results, all measurable and reproducible.** Peak working set
for a level-6 encode went 94.9 -> 57.6 MB serial and 118.7 -> 85.1 MB parallel,
both measured with the SAME configuration at each end.
The encoder was never spending its time where the space was being wasted.


---

## Correction: a memory figure that chained two different configurations

The two entries above originally reported **-37% to -44%**, from
`park_joy 101.0 -> 57.8 MB`. That number was never measured. The 101.0 came
from the clone-fix run, which used **default settings** (`Fast`); the 57.8 came
from the streaming run, which used **`-compression_level 6`**. Subtracting one
from the other compares two different jobs and silently credits the change with
the difference between compression levels.

Re-measured with the same configuration at both ends — the binary from before
any of this work against the binary after all of it:

| configuration | before | after | |
|---|---|---|---|
| `-compression_level 6`, 1 thread | 94.9 MB | 57.6 MB | **-39%** |
| `-compression_level 6`, `-threads 8` | 118.7 MB | 85.1 MB | **-28%** |
| default (`Fast`) | 101.1 MB | 77.3 MB | **-24%** |

So the honest range is **24-40%, depending on configuration**, not 37-44%.
The old figure happened to be about right for level-6 single-thread and
overstated both `Fast` and multi-threaded — `Fast` gains least because it cannot
stream at all, which the chained number completely hid.

**The lesson is narrower than "measure carefully" and worth stating exactly:**
every individual measurement in this campaign was a valid A/B, because each one
held its configuration fixed across its own two arms. The error appeared only
when a *before* from one run was paired with an *after* from another. Deltas do
not compose across runs unless the configuration is identical — and a cumulative
claim spanning several changes needs its own end-to-end measurement, not
arithmetic on the individual ones.
