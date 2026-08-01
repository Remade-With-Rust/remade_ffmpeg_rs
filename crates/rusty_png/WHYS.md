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
