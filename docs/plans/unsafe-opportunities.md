# Unsafe Optimization Opportunities — Codec Inner Loops

> **Status:** READY — target catalog populated from a live code survey (2026-08-05),
> extended same day with the three owned sibling repos: `../rs_h264`,
> `../rusty-av1-toolkit`, `../rs_AV2ed`.
>
> Built on the `rusty-unsafe-optimizations` skill
> (`~/.claude/skills/rusty-unsafe-optimizations/`: `SKILL.md`,
> `references/polars-patterns.md`, `references/safety-contracts.md`).
>
> **Prime directive: measurement first.** No unsafe is written until a profile
> proves where time goes *today*, on this box, in `--release`. Stale profiles
> lie (sister-project example: recorded profile said 62% detector; re-measured
> reality was 5.6% detector / 93.8% recognition). This repo has its own version
> of the same lesson: `docs/benchmarks.md` records a phantom 30% win from a
> non-interleaved measurement, and `rusty_vp9/src/transform.rs:764-776` records
> that the decode-stage wall carries ±7–14% noise — larger than a real kernel win.

## The five patterns (from the skill)

| # | Pattern | Mechanism | Codec shape |
|---|---------|-----------|-------------|
| P1 | Unchecked indexing | `get_unchecked(_mut)` | coefficient/table access in transforms, entropy symbol tables, clipped MV lookups |
| P2 | Uninitialized buffers | `with_capacity` + `spare_capacity_mut` + `set_len` | output planes, residual blocks, decoded sample buffers written exactly once |
| P3 | Raw-pointer sharing | custom `Send`/`Sync` wrapper | tile/slice parallelism over one plane; shared constant tables |
| P4 | TrustedLen | exact-size `collect`/`extend` | packing a known count of symbols/samples |
| P5 | Zero-copy reinterpret | `transmute` / `from_raw_parts` / bytemuck | planar↔interleaved views, i16↔bytes for entropy coding, u16→u8 storage domain |

---

# Part 1 — Target catalog

## Eligibility rules

A function is **in** only if all of these hold:

1. The hot loop is **our Rust** — not an FFI wrapper, not an out-of-repo
   git/registry dependency.
2. It profiles at **≥5% of end-to-end stage runtime** on the pinned corpus
   input — verified fresh, not from memory or an old plan doc.
3. A **bit-exact oracle** exists (or can be stood up cheaply) for the
   containing stage.
4. The win survives the **noise floor** (Part 2, Gate B).

A target profiling under 5% is recorded in the ledger as **CLOSED-COLD** — a
successful outcome, not a failure.

## Scope verdicts (which codecs are in)

| Codec / crate | Verdict | Why |
|---|---|---|
| **rusty_vp9** (decoder + encoder) | **IN — primary video target** | 31k LoC of our Rust; committed profile + conformance gate exist |
| **rusty_jpeg** (decoder + encoder) | ✅ **CLOSED 2026-08-02** | 13 entries audited, **1 real** — and that one was fixed in safe Rust + SIMD, no `unsafe`. See the two closed sections below. |
| **rusty_mp3** (decoder + encoder) | **IN** | Zero unsafe today; dense scalar DSP; strong bit-exact gates |
| **rusty_aac** (encoder + decoder) | **IN** | Has AVX2/AVX-512 quantize kernels already; decoder IMDCT is O(N²) (algorithmic fix first) |
| **rusty_vorbis** (encoder) | **IN** | Already uses `get_unchecked`+AVX2 in `brute_cost`; more of the same surface |
| **rff-codec-flac** (encoder) | **IN** | Real in-repo encoder; claxon = independent lossless oracle |
| **rff-resample** | **IN** | Own FIR kernels + the only proper audio A/B bench in the repo |
| **rff-codec-png** (wrapper glue) | ✅ **CLOSED 2026-08-06** | 3 entries, **1 real** — and it was a data-structure fix in safe Rust (3.55x whole-CLI, byte-identical). See Part 4. |
| **rff-codec-jpeg** (wrapper glue) | IN (minor) | gray→RGB expansion |
| **rff-core `AudioFrame` byte planes** (cross-cutting) | IN (P5, structural) | 319 per-sample `from_le_bytes`/`to_le_bytes` sites across 7 crates |
| **rusty_h264 (owned repo `../rs_h264`)** | **IN — owned; forbid covenant being LIFTED (owner decision 2026-08-05)** | 5 crates, ~38k LoC, published to crates.io. The `unsafe_code = "forbid"` covenant (4 Cargo.toml lint blocks + a cfg_attr in common) is being removed, unlocking in-place P1–P5. Honest sizing from the repo's own ledgers: the covenant mostly blocked a *portability* win (generalizing the accel-only `MeCtx` to the default build, ~25% of ME) — decoder P1 is comprehensively refuted, and the decoder's largest cost (40.3% unnamed glue) must be *named* before any pattern can attack it. Ranked top-10 in the section below. |
| **rusty_av1e / rusty_av1d (owned repo `../rusty-av1-toolkit`)** | **IN — owned** | rav1e fork (76k LoC, ~68 own commits) + rav1d fork (55k LoC). **2026-08-05: rff now enables `asm` + `threading` on native targets** (`rff-codec-avif/Cargo.toml` target-gated; wasm32 stays no-asm/no-threading). The repos' asm-ON optimization ledgers therefore describe rff's native build again; the no-asm concerns below apply only to the wasm32 path. |
| **rav2d + rusty_av2e (owned repo `../rs_AV2ed`)** | **IN — richest greenfield; hygiene first** | AV2 decoder (22.6k LoC hand-written AV2 code) + AV2 encoder (15.4k LoC AV2 delta). The ENTIRE AV2 delta is fresh, bounds-checked safe Rust on `Vec<i32>` planes (one i32 per 8-bit sample); zero SIMD reachable from AV2 paths. Blockers before ANY measurement: per-sample `env::var` in `Plane::at`, release `eprintln!` guards, rav2d has **no `.git`** and **no profiler**. |
| rusty_png | **BLOCKED (policy)** | `#![forbid(unsafe_code)]` at `crates/rusty_png/src/lib.rs:62` is a deliberate crate promise (see `pardeflate.rs:144-148`). Lifting it is a project decision, not a code change. If ever lifted: `filter.rs` unfilter arms (~40 `try_into().unwrap()` per-chunk sites), `transform.rs:83`, `palette.rs:93` — benches + CRC-golden conformance already exist. Note: inflate/deflate live in external crates (`fdeflate`, `zlib-rs`), not here. |
| rff-codec-h264 / rff-codec-avif adapters | OUT (adapters) | 318-line / 605-line glue; the codecs themselves are the owned repos above. |
| rff-codec-opus DSP | **OUT of this plan (owned but separate campaign)** | `rusty-opus 0.1.24` is a registry dep; the fork at `../rusty-opus` is ours but already carries byte-identical AVX2 SILK/CELT kernels and its own optimization ledger. In-repo scope here = adapter sample conversion only (see AudioFrame row). |
| rff-codec-openh264, rff-codec-webp/gif/jxl/avif wrappers, rff-codec-pcm | **OUT** | FFI shims / pure adapters; no own inner loops. (`rff-codec-pcm`'s two full-buffer clones per frame are an `Arc`/`Bytes` fix, not unsafe.) |
| rff-filter | OUT of this campaign | Video-only filters; no surveyed hot-loop complaint yet — needs its own Phase 0 before admission. |

## Function-level targets

Column "profile" = last known share of its stage; **every row still requires a
fresh Phase 0 profile before attack** (eligibility rule 2).

### rusty_vp9 — decoder

Committed stage profile (`docs/benchmarks.md:38-48`, 720p): inter prediction
~53%, loop filter ~41%, inverse transform ~2%, intra ~2%, entropy ~2%. The
doc's own conclusion: **memory-bandwidth-bound, not compute-bound** → P2/P3/P5
outrank P1 here.

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| **u16 plane storage for 8-bit content** | `src/decode.rs:986,1005`; `RefPlane.buf: &[u16]` `src/inter.rs:116` | **P5 (structural, highest ceiling)** | 2× memory-traffic multiplier on the 94% (MC + loop filter). Encoder already proves the u8 shadow domain works (`ref8_active` `encode/frameenc.rs:4785`, `sad8x8_u8` `inter.rs:510`). A redesign, not a `get_unchecked`. |
| **Tile-column decode threading** | `decode_tiles` `src/decode.rs:1501` (fully serial today) | **P3** | `docs/perf-threading.md` records two bit-exact *safe* threading attempts, both slower (3× and 1.7×), and states the fix in writing: "threads writing disjoint columns of one shared buffer". The merge half (`CountAdd::merge`, `decode.rs:189`) is already built and conformance-proven. Amdahl ceiling ~1.8× unless loop filter threads too. |
| `RefPlane::px` + scalar `predict_block` fallback | `src/inter.rs:128`, scalar bodies `:1598-1667` | P1 | `.get(...).copied().unwrap_or(0)` per tap, 8 taps/pixel — but AVX2/NEON already own interior+edge-tiled blocks; profile the residual fallback share first. |
| MC edge-tile gather | `src/inter.rs:1543-1551` | P1, P2 | `(w+7)*(h+7)` bounds-checked writes into a thread-local `[u16; 72*72]`, fully overwritten. |
| `scaled_predict_block` scratch | `src/inter.rs:1696` | P2 | `vec![0u16; int_h*w]` per call, fully overwritten (cold path — likely CLOSED-COLD). |
| `filter_edge8` scalar twin + `scatter` | `src/loopfilter.rs:642` (gather `:670-676`), `:1016` | P1 | 64 bounds-checked strided loads per edge; AVX2 twin exists at `:1383` — scalar is the non-AVX2 path only. |
| `decode_coefs` per-block memsets | `src/token.rs:110-111` | P2 | Two memsets up to 1024 i32 + 1024 u8 per transform block; only `eob` entries ever read. |
| `decode_coefs` body + `get_coef_context` | `src/token.rs:113-183`, `:52` | P1 | Densest double-indirect bounds-checked loads in entropy — but entropy is ~2% of decode: expect **CLOSED-COLD**; refute cheaply, record it. |
| `BoolDecoder::fill` | `src/bits.rs:99` | P1, P5 | Fast path: range-check + 8-byte staging copy where `read_unaligned::<u64>` would do. `read_bool` itself is already tight — P1 dead there, record as refuted. |
| `inverse_transform_add_rows` scalar column pass | `src/transform.rs:1057-1074` | P1 | Strided gather + RMW per pixel on the ADST/fallback path; stage is ~2% → likely CLOSED-COLD. |
| Intra predictors via `d()`/`g()`/`a()` | `src/predict.rs:67,71,32` (all 10 predictors) | P1 | Cleanest uniform P1 surface in the crate — but ~2% stage: CLOSED-COLD unless profile disagrees. |
| `crop_frame` | `src/lib.rs:919-938` | P2, P4, P5 | Per-row `extend(map)` u16→u8 narrowing, 3 planes/frame; TrustedLen extend into spare capacity. |

### rusty_vp9 — encoder

**No committed encoder stage profile** — Phase 0 is mandatory before choosing.
Profiler buckets already exist (`src/encode/prof.rs:27-62`, `VP9_PROF2=1`).

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| Trellis/token/quantize zeroed-scratch cluster | `encode/tokens.rs:767,805,816-817,879,883,945-951`; `encode/quantize.rs:259-262,312-315` | **P2 (densest cluster)** | `vec![0i32; n*n]` × many, some inside candidate loops. Precedent: `TxScratch` reuse (`frameenc.rs:935`, comment calls the 17 KB zero-init "the largest single unattributed bucket"), A/B'd via `VP9_TX_MEMSET=1`. |
| `quantize` prologue memsets | `encode/quantize.rs:36-37` | P2 | Two fills up to 1024 i32 per transform block, only `eob` written. |
| `quantize_scan_loop` | `encode/quantize.rs:69` | P1 | Index-from-scan-table scatter; AVX2 twin is default for n≥16 — this is the fallback/oracle, keep scalar twin regardless. |
| `block_sad` edge path | `encode/frameenc.rs:4946-4957` | P1 | Interior already raw-ptr (`:4933`); edge path fully bounds-checked per pixel. |
| `sad4x4_scalar` | `encode/frameenc.rs:7497` | P1 | No AVX2 twin exists (8x8 has one at `:7457`). |
| `search_mv` / `predicted_sad` / `pred_sse` | `frameenc.rs:5014`, `:4965`, `:5574` | P1, P2 | MotionSearch bucket; profile first. |
| Snapshot planes | `frameenc.rs:1059,1073-1075` | P2 | Thread-local pool already exists (`:322-380`) and is benched by `examples/poolbench.rs` — extend, don't duplicate. |
| `run_partition_decision` tile threading | `frameenc.rs:3576-3607` | P3 | Currently `self.clone()` per tile — the exact cost that killed the decoder's safe threading. Candidate: raw-ptr shared read state. |
| `fdct16x16`/`fdct32x32`, `forward_2d_matrix` | `encode/transform.rs:403,680,795` | P1 | Only 8×8 has an AVX2 kernel. |

> ### What auditing the first codec on this list taught us
>
> `rusty_jpeg` was worked end to end on 2026-08-02. **13 entries, 1 real.** Before
> spending time on any remaining crate here, run these two checks — each is one
> command and together they killed 9 of the 13:
>
> 1. **Does the code execute on the path we ship?** Count invocations, don't
>    assume. The entry described as "the densest bounds-check-per-byte site in
>    the image crates" was *correct* about the check count and measured
>    **0 calls/frame**, because the pipeline takes a different output path.
> 2. **Did the compiler already elide the check?** Emit the assembly and count
>    `panic_bounds_check` per symbol:
>    `cargo rustc --release -p <crate> --lib -- --emit asm`.
>    Entries reasoning "the mask proves the index, so the check is pure tax" are
>    usually right about the premise and wrong about the conclusion — *because*
>    the mask proves it, the check is not there.
>
> And when an entry survives both: **try safe Rust first.** The one real entry
> here was fixed by hoisting a dead edge case (1.14x, byte-identical) and then by
> SIMD (1.25x) — `unsafe` was never needed. Reach for it when a bound genuinely
> cannot be proven, not when it merely has not been.

### rusty_jpeg — decoder ✅ CLOSED 2026-08-02

> **MEASURED 2026-08-02 — four of these five decoder entries are DEAD, and the
> counts that killed them each took one command.** See `crates/rusty_jpeg/WHYS.md`
> D1g. Verdicts are inline below; the table is left intact so the reasoning that
> produced each candidate is still readable.
>
> - **`upsample_rows` per frame: 0 planar / 1,080 packed.** `decode_planes`
>   returns before `compute_image`, which owns the upsampler, the whole-frame
>   `vec![0u8; ..]` AND the colour-convert tails. `rff-codec-jpeg` uses
>   `decode_planar()` for every ordinary 4:4:4/4:2:2/4:2:0 colour JPEG. The
>   "densest bounds-check-per-byte site" claim is CORRECT (H2V1 emits 14 checks,
>   H2V2 12 — the most of any function in the crate) and irrelevant: it does not
>   execute.
> - **`decode_block` emits ZERO bounds checks.** The entry reasons that the
>   `& 63` masks prove the index so the check is "pure tax" — the premise is
>   right and the conclusion inverted: *because* the masks prove it, LLVM has
>   already removed the check. Nothing to reclaim.

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| ~~**Upsampler row loops**~~ **DEAD — 0 exec** | `src/decode/upsampler.rs:188-192` (H2V1), `:257-263` (H2V2), `:222-224` (H1V2) | **P1** | 5 bounds checks per output pair; loop bounds `1..width-1` already prove all five. Densest bounds-check-per-byte site in the image crates. |
| ~~**Per-scanline line-buffer alloc**~~ **DEAD — 0 exec** | `upsampler.rs:73` | **P2** | `vec![vec![0u8; …]; ncomp]` allocated + zeroed *per output row*, fully overwritten. Reuse or spare-capacity. |
| ~~**Whole-frame output zeroing**~~ **DEAD — 0 exec** | `src/decode/worker/mod.rs:179`, `worker/rayon.rs:202` | **P2** | Full RGB frame `vec![0u8; …]` then every byte written. Precedent: `immediate.rs:60-107` recycled-plane memset skip with `RUSTY_JPEG_ABLATE=planezero` as A/B arm. |
| ~~Coefficient stores in entropy decode~~ **DEAD — 0 checks emitted** | `src/decode/decoder.rs:1564,1592,1690,1714-1716` | P1 | `coefficients[UNZIGZAG[i & 63] as usize & 63]` — the masks already prove the index; check is pure tax. ~362k symbols/1080p frame. |
| ~~Huffman LUT lookups~~ **negligible — 1 check** | `src/decode/huffman.rs:40,75` | P1 | Index masked to `1<<LUT_BITS` — provably in range. |
| ~~Block `try_into().unwrap()` slicing~~ **negligible** | `src/decode/worker/immediate.rs:141,155` | P5 | `&[i16]`→`&[i16;64]` via pointer cast; ~49k/frame. |
| ~~`color_convert_line_*` scalar tails~~ **DEAD — 0 exec** | `decoder.rs:1865-1876` (`.skip()` on 4-deep zip), `color_no_convert` `:1916-1924` (unwrap per byte) | P1, P4 | Arch kernels own the bulk; the tail zip + per-byte unwrap are the leftovers. |
| ~~Scalar IDCT fallback~~ **DEAD — SSSE3 always present** | `src/decode/idct.rs:377-390` | P1 | Only when SSSE3 absent — profile before touching. |

### rusty_jpeg — encoder ✅ CLOSED 2026-08-02

> **MEASURED 2026-08-02 — `get_block` is the one entry in the JPEG backlog that
> was real, and the fix needed no `unsafe`.** It emits 23 bounds checks and is
> **13.05% of encode**, the largest named encoder stage. But the cost is the
> per-sample `.min()` clamps, which are loop-invariant except for blocks
> overhanging the right/bottom edge — and they are what stop the compiler proving
> the index. Hoisting the edge test to block level and slicing each row turns 256
> checks into 2, in safe Rust: **encode 24.3 -> 21.4 ms min-of-9, non-overlapping
> ranges, byte-identical output**. SHIPPED.
>
> `quantize_zz` / `quantize_block_scalar` emits **zero** bounds checks, and the
> AVX2/NEON twins own the path regardless — dead entry.
>
> **Closed out 2026-08-02.** Final tally for the JPEG backlog: **13 entries, 1
> real.** Nine were dead (the code does not execute on the shipped path, or the
> compiler had already elided the check), three were probed and landed inside
> noise, and one — `get_block` — was genuine at ~13% of encode.
>
> **That one needed no `unsafe` either.** It went safe-Rust first (hoist the
> dead edge clamps: 1.14x, byte-identical), then SIMD (`cvtepu8_epi16` for luma,
> `maddubs` for the 4:2:0 chroma box filter: a further **1.25x**, z 4.20, gated
> byte-for-byte over 81 encodes). Its double-run share then fell from a verdict
> to the null floor.
>
> The real encoder win of the campaign was not on this list at all: the AC
> zero-run scan, found by decomposing the residue rather than by reading code
> for bounds checks (**1.28x**, z 3.36). Lesson recorded in
> `~/.claude/skills/rusty-unsafe-optimizations` — read the emitted assembly
> before trusting any entry in a list like this one.

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| ~~`get_block` box-average chroma~~ ✅ **DONE — safe Rust + SIMD, 1.14x then 1.25x** | `src/encode/encoder.rs:1687-1717` | P1 | 4-deep nest, per-sample `.min()` clamp + bounds check; hoist clamp then unchecked. |
| ~~Row-padding loop~~ **not pursued — below resolution** | `encoder.rs:1323-1329` | P2 | `channel.push(channel[len-1])` per padding byte. |
| ~~`BitWriter` output~~ **REFUTED — probed, inside noise (z 0.26)** | `src/encode/writer.rs:200,349,408` | P2 | spare-capacity + `set_len` on the output vec vs per-byte push. |
| ~~`HuffmanTable::get_for_value`~~ **negligible** | `src/encode/huffman.rs:230` | P1 | 256-entry LUT by u8 — check provably dead. |
| ~~`quantize_zz` scalar~~ **DEAD — 0 checks; SIMD owns the path** | `src/encode/quantization.rs:353,386` | P1 | AVX2/NEON twins + oracle tests already exist — extend the same seam. |

### rusty_mp3 — decoder

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| **`BitReader::peek`** | `src/bitio.rs:41-51` | **P1, P5 (top decoder lever)** | One bit at a time, `.get()` + `unwrap_or(0)` per bit, under every Huffman codeword. u64 big-endian refill + unchecked. Gate: `lut_matches_linear_for_every_codeword` (`decode/huffman.rs:393`). |
| **`synthesis::polyphase`** | `src/decode/synthesis.rs:106-113` | **P1** | 4 bounds checks × 9216 taps per granule/channel. Gate: `fast_matrixing_matches_dense`. |
| `synthesis::matrixing_fast` | `synthesis.rs:66-73` | P1 | 512 mults/pass × 18. |
| `imdct::hybrid` | `src/decode/imdct.rs:95-125` | P1 | 648 MACs per subband × 32. |
| Huffman decode + LUT | `src/decode/huffman.rs:101,277-304` | P1 | Peek index provably `lut_bits`-wide. |
| `requantize::apply` long-block fill | `src/decode/requantize.rs:45` | P2 | `out.fill(0)` then fully rewritten for long blocks (short blocks need the zero — keep). |
| `Reservoir::assemble` | `src/decode/reservoir.rs:19-28` | not-unsafe (memory-copies) | Per-frame alloc + double-reverse collect; fixed ring buffer fix. Route to codec-memory-copies, record here for completeness. |

### rusty_mp3 — encoder

Quantize is 62.7% of encode per the in-code `perf001` note (`encode/quantize.rs:81`).

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| `quantize_with_sf` | `encode/quantize.rs:186-189` | P1 | 3 arrays indexed per line, per rate-loop trial. |
| `band_noise` (+ hoist its per-line `OnceLock` LUT fetch) | `quantize.rs:47-50, 211-215` | P1 + redundancy | Hoist the slice first (like `requantize.rs` does), then unchecked. |
| `best_pair_table` / `estimate_bits` / `cost` | `encode/huffman.rs:196-251, :116, :444` | P1 | Already redundancy-optimized — remaining cost is bounds-check tax. Oracles: `best_pair_table_hist_matches_ref`, `estimate_matches_emitted_bits`, `cost_matches_encoded_bits`. |
| `filterbank::analyze` | `encode/filterbank.rs:67-82` | P1 | 512 + 2048 MACs × 18 passes. |
| `mdct::forward` | `encode/mdct.rs:127-157` | P1 | Gates: `*_reconstruct_exactly` tests. |
| `fft` / `power_spectrum` | `encode/fft.rs:11,75` | P1, P2 | Butterflies + two fully-written `vec![0f32]`. Oracle: `hoisted_matches_inline`. |
| `stereo::mid_side` | `encode/stereo.rs:16-17` | P2 | Two fully-overwritten zeroed vecs. |
| ⚠ `xrpow` | `quantize.rs:94` | P1 only | **AVX2 twin already tried and PRUNED** (`perf003`, `:86-93`) — auto-vectorizes. Do not retry SIMD; unchecked-only is still open. |

### rusty_aac

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| **`dsp::imdct` (decoder)** | `src/dsp.rs:32` | **Algorithmic FIRST, then P1/P2** | O(N²) direct IMDCT (2048×1024 per channel per frame) while `mdct_fast` (`:190`) exists for the forward direction with no inverse twin. Biggest single number on the audio board — an FFT-based inverse belongs before any unsafe. |
| `HuffBook::decode` (decoder) | `src/huffman.rs:52-63` | LUT first, then P1 | O(maxlen × count) linear scan per codeword, no peek table; `rusty_mp3`'s `LutSlot` is the proven in-repo template. |
| `BitReader::read_bits` | `src/bits.rs:35-44` | P1, P5 | Bit-at-a-time; u64 refill candidate. |
| `quantize_band_scalar` / `Xpow::new` scalar | `src/encode.rs:645, :494-500` | P1, P2 | AVX2/AVX-512 twins exist; scalar fallback + the fully-overwritten `pow`/`sign` vecs (`:484-485`) remain. |
| `best_codebook_for_band` + `spectral_bits`/`tuple_index` | `encode.rs:518, :104, :78` | P1 | Hottest encoder search: 11 codebooks × band width, all bounds-checked. |
| `estimate_bits` | `encode.rs:817-823` | P1 | LUT index already clamped to `MAX_QUANT`. |
| `analyze_long`/`analyze_short` windowing | `encode.rs:271-297` | P2, P1 | Fully-overwritten zeroed frames. |
| `mdct_fast` fold/rotation | `dsp.rs:206-227` | P1, P2 | Three fully-written zeroed vecs per call. |
| `decode::synthesize`/`short_frame` overlap-add | `src/decode.rs:196-235` | P1, P2 | Merge the two `0..FRAME_LEN` loops, spare-capacity the frame buffer. |
| `decode::interleave` | `decode.rs:754-759` | P4 | Exact-count pushes after `with_capacity` — textbook TrustedLen. |
| 🐛 **`xpow_avx2` scalar tail** | `encode.rs:737-741` | **BUG — fix now, independent of campaign** | Missing `i += 1` → infinite loop for any length not a multiple of 4 (currently unreachable; still a landmine). |

### rusty_vorbis (encoder)

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| `quantize_vector` lattice path | `src/setup.rs:227-243` | P1 | O(dim × levels) per vector, "called hundreds of thousands of times per stream". |
| `vq_pass` / `cascade_cost` | `src/frame.rs:94-104, :111-129` | P1, P2 | 8 `vq_pass`es per class per partition; `work.clear()+extend` → uninit copy. |
| `encode_residue2` interleave | `frame.rs:143-148, :209` | P2 | Fully-overwritten `vec![0.0]` + a `clone` per submap. |
| `mdct_forward` fold/rotation | `src/mdct.rs:207-232` | P1, P2 | Same shape as AAC's; oracle `table_mdct_matches_direct`. |
| `brute_cost_scalar` | `setup.rs:283-300` | keep as oracle | AVX2 twin already exists with a bit-identity gate (`brute_quantize_matches_reference` `:1105`) — house template for the audio crates. |
| ⚠ `argmin_entry` | `setup.rs:349` | refuted | Comment `:305`: vectorized scan measured slower on 80–220-entry books. Don't retry. |

### rff-codec-flac (encoder)

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| **`rice_bits`/`best_rice`/`plan_partitions`** | `src/encode.rs:420-510` | **Redundancy FIRST, then P1** | Residual walked ~135× (15 k-values × up to 9 orders); a single histogram pass over zigzag magnitudes yields all 15 costs at once. Oracle: claxon losslessness. |
| `lpc_residual` | `encode.rs:638-646` | P1, P4 | Order-32 dot product per sample; exact-count pushes. |
| `autocorrelation` / `levinson` | `encode.rs:556-571, :576` | P1, P2 | Per-call collects + zeroed vecs. |
| `fixed_residual` | `encode.rs:402-410` | P2 | Two fresh Vecs per order tried. |
| `ingest` sample conversion | `encode.rs:215-262` | P5 | Part of the AudioFrame item below. |

### rff-resample

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| **`dot_width` dispatch** | `src/lib.rs:287` | redundancy FIRST | `is_x86_feature_detected!` per output sample per channel — hoist to `OnceLock` (pattern: `rusty_aac::has_avx2`). Do this before any unsafe; it may close the gap alone. |
| `run_poly` inner | `lib.rs:196-231` | P1, P2 | Per-output bounds-checked window/weight slices; per-sample `push` after computed reserve → spare-capacity. |
| History front-drain | `lib.rs:226-228, :270` | memory-copies | `drain(0..keep_from)` per run — ring buffer / `copy_within`. |
| N-channel deinterleave | `lib.rs:168-170` | P1 | `i % ch` integer divide per sample. |
| `dot_width_avx2` | `lib.rs:301-333` | polish | WIDTH==32 → the `while i+32<=WIDTH` loop is one iteration; unroll/const-fold. No NEON mirror exists. |

### Cross-cutting — AudioFrame byte planes (P5, structural)

`rff-core/src/frame.rs:28` stores audio as `Vec<Vec<u8>>` of little-endian
bytes → **319** per-sample `from_le_bytes`/`to_le_bytes` sites across `rff`
(`transcode.rs:210-225`), `rff-codec-opus` (`lib.rs:48-56,113,403`),
`rff-codec-aac`, `rff-codec-mp3`, `rff-codec-vorbis`, `rff-codec-flac`.

- Fast path: `align_to::<f32>()` / bytemuck with scalar fallback — **never** a
  raw `&[u8]`→`&[f32]` transmute (`Vec<u8>` is 1-aligned; the alignment check
  is the safety contract).
- Durable fix: typed/aligned plane representation in `AudioFrame` — a design
  change to propose separately, not to smuggle in via this campaign.

### rff-codec-png / rff-codec-jpeg wrapper glue (minor)

| Function | Location | Pattern(s) |
|---|---|---|
| ~~`pack_indices`~~ **CLOSED-COLD** | `rff-codec-png/src/lib.rs:342-348` | 0.5–16 ms, and only does real work below 8 bits/index (≤16 colours); a plain `to_vec` otherwise. |
| ~~`analyse` per-pixel HashMap probe~~ ✅ **DONE — 19.91x on the scan, 3.55x whole-CLI** | `rff-codec-png/src/lib.rs:280-308` | data-structure fix, not unsafe — **the row was exactly right**. Open-addressed table + fused single pass. |
| gray→RGB expansions (~~png **CLOSED-COLD** 2–12 ms~~; jpeg open) | `rff-codec-jpeg/src/lib.rs:135-137`, ~~`rff-codec-png/src/lib.rs:88-112`~~ | P2 |

### rusty_h264 — owned repo `../rs_h264` (post-forbid campaign, 2026-08-05)

**Decision context:** the owner is removing `unsafe_code = "forbid"` from the
codec crates (4 `[lints.rust]` blocks + the `cfg_attr` in
`common/src/lib.rs:17`, which is already conditional for the `profile` feature
— the precedent for a feature-gated relaxation). A dedicated survey mined the
repo's ledgers for what the covenant actually blocked. **Honest sizing before
spending the covenant:**

1. The best-documented prize is a **portability win, not a new seam**: H-15
   already collected ~10% (bus 1.134×, mobile 1.088×) by putting `MeCtx` in
   accel — but `MeCtx::new` declines without AVX2, without `--features asm`,
   and for sub-8×8, so **the default published build banks none of it**.
   Re-implementing the MeCtx shape natively in the encoder generalizes a
   proven brick to every build (~23 ns/eval of glue ≈ 25% of ME ≈ 15 ms/24f,
   sized by H-14 R2, `WHYS-speed-gap.md:1786-1787`).
2. The decoder's largest cost is **40.3% "unnamed glue"** (H-50,
   `WHYS-speed-gap.md:2969-2996`) with every surrounding kernel at the
   rdtsc instrumentation floor — **no P1–P5 pattern can be aimed at it until
   a tap session names it**. That decomposition is the real next decoder
   action, and it precedes any unsafe.
3. The ISA lever the covenant was blamed for is measured at **≤4.3%**
   (H-42 `-C target-cpu=native` ceiling, z=2.33) — real, cheap, not a headline.

| # | Opportunity | Location | Pattern(s) | Sizing / status | Gate |
|---|---|---|---|---|---|
| 1 | **Encoder-native `MeCtx`** — validate plane geometry once, raw-offset per eval, over the safe/`wide` kernels, serving the default (non-accel) build | `encoder/src/mb16.rs:2207-2246` (construction), `:1617-1694` (dispatch ladder it replaces); mirror of `accel/src/mectx.rs` | P1+P5 | ★ top prize; ~25% of ME on default builds; the exact thing `WHYS:1759-1762` says required lifting forbid | `mectx_matches_safe_path`-style oracle + `bench/conf_matrix.sh` + full-encode hash (H-15's own gate list) |
| 2 | **`save_mb` reuse + uninit fill** — `MbState` is 12 Vecs, allocated fresh per RDO trial (~5/MB) + a fresh `BitWriter` per trial; `save_mb_into` exists but 3 of 4 call sites don't use it | `mb16.rs:4390,4411,5102` → `:4552-4596` | safe reuse FIRST, then P2 | quality preset is 93% RDO trials; small repeated allocs, so the big-alloc refutation does NOT apply | encoder hash (byte-identical by construction) + `rd_skip_conformance` |
| 3 | **`load_mb`/`save_mb_into` const-width row copies** — 384 element-wise strided stores + 384 pushes per trial | `mb16.rs:4599-4632, 4561-4596` | **safe** `copy_from_slice` | H-17 proved this exact codegen trap (16× precedent) and fixed pred-buf, not these | same as #2 |
| 4 | **`mb_ssd` restructure** — 384 px per trial, i64-multiply blocks vectorization | `mb16.rs:4314-4334` | P1 + accumulate-width fix | genuinely unswept (zero prior mention) | must be **exactly** equal (SSD feeds mode decisions) — hash gate + `conf_matrix.sh` |
| 5 | **Encoder CABAC `renorm` branchless** (port decoder's H-35 shape) | `encoder/src/cabac.rs:102-138` | **safe** | ~3-5% of encode (H-16 sizing); queued and un-built; needs no unsafe | `engine_roundtrip_many` + `cabac_roundtrip.rs` + conf_matrix CABAC arm |
| 6 | **AVX2 multiversioning** (`#[target_feature]` + runtime detect) on named hot safe fns | per `WHYS-decoder-perf.md:1005-1012` option 3 | unsafe fn + dispatch | capped at ~4.3% total; only unlocked by lifting forbid | ffmpeg byte-identical 9/9 + `fuzz_no_panic` + pinned paired bench |
| 7 | `satd_px` uninit `blocks` | `mb16.rs:5507` | P2 (`MaybeUninit`, init `[..nbx*nby]`) | default-build only (accel bypasses it); 1 KiB per eval ×468 evals/MB | `satd_*_compare.rs` oracles + hash |
| 8 | Encoder scan8 sentinel cache (kills `mv_neighbors_*` 4-branch guards) | `mb16.rs:1558-1604`; plan `docs/x264-structural-port.md:91-116` | **safe** layout | decoder side pre-refuted at 0.3%; encoder side never measured — **tap the ceiling first** | prof tap, then hash + conf_matrix |
| 9 | `peek_bits` u64 window | `common/src/bit_reader.rs:123-151` | safe (wider load) | 4-byte form already banked (movbe 0→8); low | `peek_bits_matches_zero_fill_reference` + fuzz + ffmpeg 9/9 |
| 10 | Drop per-MB aligned source copy via `AlignedBytes` source plane | `mb16.rs:2174-2183` | P5 (bytemuck, precedent `common/src/aligned.rs`) | low (256 B amortized over 20-50 evals) | `sad_satd_family_matches_reference` + hash |

⚠ Standing hazards: the CAVLC reject guards (`common/src/cavlc.rs:616,695,708`)
and the CABAC zero-fill-past-end contract are **security-load-bearing**
(`fuzz_no_panic` gate) — never remove them for speed. Note items 3, 5, 8, 9
are safe-Rust fixes — the repo's own discipline (safe restructure first,
`WHYS-decoder-perf.md:1439-1441`) applies even with forbid lifted.

### rusty_av1e / rusty_av1d — owned repo `../rusty-av1-toolkit`

**Pre-work ladder — status after the 2026-08-05 rff feature change:**

1. ~~Decide `threading` / enable asm~~ **DONE** — rff now builds
   `rusty_av1e` with `["asm","threading"]` and `rusty_av1d` with `["asm"]` on
   non-wasm targets (`rff-codec-avif/Cargo.toml`; AVIF roundtrip green; CI
   already installs nasm on all three legs). The ledgers' asm-ON numbers and
   gate baselines (encoder SHA256/FNV, decoder md5) are valid for rff's
   native build as-is.
2. **Un-`#[cold]` the no-asm fallbacks** — now a **wasm32-path** improvement
   only: `mc.rs:295,405,499`, `cdef.rs:196`, `transform/forward.rs:70`,
   `transform/inverse.rs:1632` (+ rav1d's `#[inline(never)]` on
   `put_8tap_rust`/`prep_8tap_rust`/`itx_1d`). Still one line per site and
   byte-identical, but re-ranked below the native-path targets.
3. With asm ON, the native-build hot-loop reality matches the ledgers again:
   MSAC is asm, MC/CDEF/LRF/transforms are asm — so the native P1 surface
   shrinks to what the ledgers left open (B2/B8, quantize gather/scatter,
   `PlaneRegion::index`) and the P2 rows below. **Re-run Phase 0 anyway** —
   eligibility rule 2 — but expect the asm-ON profile shape.

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| **`rav1d_msac_decode_symbol_adapt_rust`** | `rusty_av1d/src/msac.rs:388` | **P1 (top decoder target)** | Replaces the SSE2 asm that was ~40% of decode; in rff's build this pure-Rust loop is the hottest scalar function. Invariants (`n_symbols < 16`, `val <= n_symbols`) already asserted — the `get_unchecked` contract is pre-written. No microbench exists — build one first. |
| **`PlaneRegion::index`/`index_mut`** | `rusty_av1e/src/tiling/plane_region.rs:411,513` | **P1 (cross-cutting)** | Hard release `assert!` on every row access inside `put_8tap`, `prep_8tap`, `mc_avg`, `cdef_filter_block`, SAD/SATD row iteration, every predictor. Add `row_unchecked` for kernels that already assert bounds at entry. |
| LR/SGR scratch | `rusty_av1d/src/looprestoration.rs:394,400` (wiener: 2×27,300 elts/LR unit), `:653`; `rusty_av1e/src/lrf.rs:637-646, 856-862` (per stripe / per RDO candidate) | **P2 (largest memsets)** | All fully overwritten. |
| `get_satd` per-chunk buf | `rusty_av1e/src/dist.rs:194` | P2 | `[0i32; 64]` zeroed per 8×8 chunk of every SATD. |
| `put_8tap`/`prep_8tap` intermediate | `rusty_av1e/src/mc.rs:360,462` | P2 | `[i16; 8*135]` = 2,160 B zeroed per subpel MC call, fully written. |
| Quantize gather/scatter | `rusty_av1e/src/quantize/mod.rs:348-372` | P1 | Already branchless (Q2 brick, −35%); remaining per-iteration cost is exactly the two scan-table bounds checks. Racecar twin must stay byte-identical. |
| `write_coeffs_lv_map` path | `rusty_av1e/src/context/block_unit.rs:2139` (+ `get_txb_ctx:479`, `encode_coeff_signs:2582`, `update_cdf` in `ec.rs`) | P1, P2 | 48.6% of encode (asm-on figure), 192M `symbol_with_update` calls. Ledger bricks B2/B8 never attempted; `get_unchecked` on the CDF row never tried. |
| `varpart::build_var_tree` (ours) | `rusty_av1e/src/varpart.rs:85,110` | P2 | 16 KB zeroed per SB + 3 heap allocs per SB. |
| `tempfilter` ARNR planes (ours) | `rusty_av1e/src/tempfilter.rs:325-327` | P2 | Per-block `Plane::new` per neighbour. |
| `deblock.rs` (whole file) | `rusty_av1e/src/deblock.rs:147-1250` | P1 (unexplored) | 1,668 L pure Rust with **no asm twin in any build** — least-explored hot file in the repo. |
| `inv_txfm_add` scratch / `itx_1d` butterflies | `rusty_av1d/src/itx.rs:98,307`; `itx_1d.rs` | P2 / P1 | Up to 16 KB zeroed per TX block; `itx_1d` is `deny(unsafe_code)` — P1 there means module-policy discussion or routing. |
| `DisjointMut` | `rusty_av1d/src/disjoint_mut.rs` | P3 **already built** | The exact raw-pointer-sharing pattern with debug overlap checking. Don't rebuild; residual P1 = release-mode range checks in `index()`/`index_mut()`. |

### rav2d + rusty_av2e — owned repo `../rs_AV2ed`

**Tier −1 — hygiene, before any measurement is admissible (all byte-identical):**

1. `Plane::at` (`rav2d/src/av2_frame.rs:33-41`) does **`env::var("ATDBG")` per
   pixel read** — and it is *the* accessor under every MC tap. Remove/gate it.
2. Per-SB `env::var("MSBT")` (`obu.rs:5310`); audit the ~200 `env::var` +
   `eprintln!` probes (114 in `av2_recon.rs` alone) for hot-path sites.
3. Release-mode `eprintln!` length guards in `residual_add`
   (`av2_itx.rs:443-445`) and `mc_translate` (`av2_inter.rs:82-84`).
4. **Put `rav2d` under git** (it has no `.git`) — no optimization campaign on
   an unversioned tree.

**Tier 0 — instrument:** rav2d has no profiler at all; the encoder's
`prof.rs` covers only the AV1 pipeline (zero `prof::scope` in the AV2 leaf
path `encoder.rs:5300-7000` / `av2/tile.rs`). Port the `rusty_av1e/src/prof.rs`
pattern before Phase 0.

| Function | Location | Pattern(s) | Evidence |
|---|---|---|---|
| GDF per-row alloc | `rav2d/src/av2_gdf.rs:278` AND `rusty_av2e/src/av2/gdf.rs:278` | **P2 (severe)** | `vec![[0i32; LUT_IDX_NUM]; blk_width]` allocated **inside the row loop** of the learned deblocking filter, both crates. |
| Per-TX `levels` allocs | `rav2d/src/av2_coef.rs:337` + 4 siblings; `rusty_av2e/src/av2/coef_ctx.rs:287,447,595,792,913`; `av2/tile.rs:4147` | **P2** | `vec![0i8; …]` heap alloc + zero per TX block. Ledger B4 says: the win is the **allocation**, not the fill — hoist/reuse, don't eob-proportion the memset. |
| MC intermediates | `rav2d/src/av2_inter.rs:99,169`; `rusty_av2e/src/av2/inter.rs:67,145,283,413,504,519-520` | P2 | `vec![0i32; midh*w]` per MC call. |
| Encoder AV2 leaf alloc cluster | `rusty_av2e/src/encoder.rs:5391-6640` (**144 `vec![0…]` sites**, incl. `Vec<Vec<f64>>` at `:5570,6360`); RD contracts `measure_quad16_bits:4457`, `measure_option_bits:4537` (owned Vec triples per candidate); `dequant_block` returns `Vec<i32>` per block (`av2/itx.rs:177`) | **P2** | Allocation-dominated leaf path. |
| `inv_txfm_2d` scratch | `rav2d/src/av2_itx.rs:336` | P2 | `[0i32; 32*32]` = 4 KB stack zeroed per TX regardless of size. |
| Whole-plane clones between filter stages | `rav2d/src/av2_frame.rs:1250,1438,1638,1680,1862,1870` | P2 / memory-copies | Full-frame `.clone()` per stage. |
| **`get_lo_ctx_2d_luma` stencil** | `rav2d/src/av2_coef.rs:86` (+ HV/chroma variants `:458,657,851`); mirror `rusty_av2e/src/av2/coef_ctx.rs:58` | **P1 (proven-hot shape)** | 5 bounds-checked neighbour reads per coefficient — the exact AV2 twin of the B7a stencil that took **−70%** (AVX2 kernel, `entropy-bricks.md`) in the AV1 encoder. Highest-confidence P1 in the AV2 repos. |
| MC 8-tap loops | `rav2d/src/av2_inter.rs:104-132` | P1, P4 | Per-tap `clamp` + `Plane::at`; hoist clamp via edge-extended borders, then unchecked. |
| `inv_dct_1d` / `inv_dst_1d` matmuls | `av2_itx.rs:157, 207` | P1 | Running-`m` index LLVM can't prove; serves 10 TX types. |
| `lr_filter_luma` | `rav2d/src/av2_lr.rs:271-279 (6×6×3 stencil), :330-343 (13/18-tap)` | P1 | Every sample through a `px` closure with clamp + 3-way stripe branch. |
| CDEF / CCSO / blends / `residual_add` | `av2_filter.rs:97,152,282`; `av2_inter.rs:218-292`; `av2_itx.rs:449-467` | P1, P4 | Textbook per-pixel loops currently blocked by bounds checks. |
| **`Vec<i32>` planes → u16/u8** | `rav2d/src/av2_frame.rs:13` (+ encoder mirror) | **P5/structural (biggest ceiling)** | 4× memory traffic vs u8 and the reason no inherited SIMD is reachable. A layout job (codec-cache-tiles), not an unsafe job — schedule as its own campaign. |
| P3 status | `rav2d/src/disjoint_mut.rs` present but unused by AV2 | P3 (later) | AV2 decode is `thread_local!`+`RefCell` single-threaded by construction. When threading lands, `DisjointMut` is the ready seam. |

## Priority order (expected value × reach; byte-identical free wins first)

**Tier A — free, byte-identical, no unsafe (do before anything else):**

1. **rav2d hygiene** — remove the per-pixel `env::var("ATDBG")` in `Plane::at`, per-SB `MSBT`, release `eprintln!` guards; put rav2d under git; add a profiler. Likely dwarfs every other AV2 cost.
2. ~~rusty-av1-toolkit no-asm ladder~~ **largely DONE 2026-08-05** — rff now ships asm+threading AV1 on native targets. Remaining: un-`#[cold]` the six fallbacks for the wasm32 path (low priority).
3. **rff-resample dispatch hoist**, **rusty_aac `imdct` algorithmic fix** (O(N²)→FFT), **flac rice histogram-pass** — safe algorithmic/redundancy wins that precede unsafe on their crates.

**Tier B — the unsafe campaign proper:**

4. **rusty_vp9 / P5 u16→u8 plane domain** — attacks the memory-bound 94% at its root.
5. **rusty_vp9 / P3 tile-column decode** — safe alternatives already refuted in writing; merge machinery exists.
6. **rs_AV2ed P2 cluster** (GDF per-row alloc, per-TX `levels`, MC intermediates, encoder leaf allocs) + **`get_lo_ctx_2d_luma` stencil** (the B7a twin, −70% precedent).
7. **rusty_av1d `rav1d_msac_decode_symbol_adapt_rust` (P1)** + **rusty_av1e `PlaneRegion` unchecked rows (P1)** — the two highest-leverage AV1 no-asm sites; gate baselines re-derived first.
8. ~~**rusty_jpeg upsamplers (P1) + per-scanline allocs (P2)**~~ ✅ **CLOSED — DEAD.**
   Both live in `compute_image`, which the planar output path returns before
   reaching: `upsample_rows` counts **0/frame** on every ordinary colour JPEG.
   The "dense per-byte cost" was real and never executed.
9. **rusty_mp3 `BitReader::peek` u64 refill (P1/P5)** — sits under every Huffman codeword.
10. **rusty_vp9 encoder P2 zeroed-scratch cluster** — precedent + A/B mechanism (`VP9_TX_MEMSET`) already in place.
11. **rs_h264 post-forbid #1-#4**: encoder-native `MeCtx` (the proven ~25%-of-ME brick, generalized to the default build), `save_mb` reuse→P2, `load_mb` const-width copies (safe), `mb_ssd`. Precondition for decoder work: the 40.3%-glue tap session.
12. **rusty-av1-toolkit P2** (wiener/SGR scratch, `get_satd` buf, 8-tap intermediates), **rusty_mp3 `polyphase` (P1)**.
13. ~~**rusty_jpeg entropy coefficient stores (P1)**~~ ✅ **CLOSED — the compiler
    had already elided them (0 `panic_bounds_check` emitted in `decode_block`).**
    AAC quantize/codebook search (P1), vorbis vq loops, resample P1/P2 remain.
14. **AudioFrame P5** — widest reach, but propose the representation change first.

**Tier C — structural campaigns (own plans, not single bricks):**
AV2 `Vec<i32>`→u16/u8 planes; AV1/AV2 threading (P3 via existing `DisjointMut`); `rusty_av1e` deblock.rs exploration.

## Pre-refuted levers — do NOT retry without new evidence

| Lever | Where recorded |
|---|---|
| Safe tile threading in VP9 decode (per-worker buffers + merge) — 1.7–3× slower | `docs/perf-threading.md` |
| VP9 compute-SIMD / tile threading beyond current — decoder at single-thread ceiling | memory: vp9-perf-memory-bound; `docs/benchmarks.md` |
| `rusty_mp3::xrpow` AVX2 twin — auto-vectorizes, pruned | `encode/quantize.rs:86-93` (perf003) |
| `rusty_vorbis::argmin_entry` vectorized scan — slower on small books | `setup.rs:305` |
| rayon-by-default in rusty_jpeg — 1.32–1.91× slower, deliberately off | `rusty_jpeg/Cargo.toml` |
| VP9 `BoolDecoder::read_bool` P1 — no indexing to uncheck | `src/bits.rs:133` |
| VP9 `idct*/iadst*` P1 — fixed-size arrays, LLVM already elides | `src/transform.rs:40-493` |
| **rs_h264 decoder P1** — `get_unchecked` over hot grids/planes, interleaved A/B, FLAT (96.7→94.0 Mpx/s); "NOT cache and NOT bounds-checks" | `rs_h264/docs/decode-locality-plan.md:14,70-72` |
| rs_h264 cache/tile layout — cache probe showed throughput RISES past L2; Phases 1-3 ruled out (`neighbors` = 0.3%) | `decode-locality-plan.md:5-21` |
| rs_h264 SIMD DCT batching — ~3% slower; rustc autovectorizes the scalar | `rs_h264/memory/transform-batching-regresses.md` |
| rs_h264 asm SAD for fast preset — `abs_diff` sum already lowers to psadbw | `rs_h264/docs/satd-asm-ledger.md:26` |
| rs_h264 `-C target-cpu=native` — non-shippable (SIGILL elsewhere); ceiling since measured at ~4.3% (H-42) | `docs/WHYS-decoder-perf.md:999-1012`; `WHYS-speed-gap.md:2753-2761` |
| **rs_h264 LAW: `vec![0; n]` for a BIG buffer is not a memset** — fresh large allocs get OS-pre-zeroed pages; pad_plane memset-elimination REFUTED (3/7, 0.972), reverted. Kills `build_hpel_planes`×4, `clamp_plane` "P2" ideas | `WHYS-speed-gap.md:2460-2464` |
| rs_h264 decoder `store()` scatter — inside the flat get_unchecked sweep AND priced at 11 ns/call, the rdtsc floor (H-50) | `WHYS-speed-gap.md:2987-2992` |
| rs_h264 decoder CABAC u64 refill, branchless decision, packed ctx — **all three already shipped in safe Rust** (H-34/H-35/`& 127` proof); nothing left | `decoder/src/cabac.rs:117-198` |
| rs_h264 `peek_bits` unchecked load — safe `get(range)`+`from_be_bytes` already compiles to the unchecked form (movbe 0→8); "No unsafe required" recorded verbatim | `WHYS-decoder-perf.md:1432-1445` |
| rs_h264 CAVLC/CABAC bit-writer micro-opt — capped ~5%, CABAC output rewrite measured FLAT | `memory/encode-phase-breakdown.md:13-15`; `WHYS:1829-1840` |
| rav1e/av2e ledger B4 — eob-proportional `levels` fill, FLAT ("small-array data movement ≠ redundancy", 3 codecs confirm; the alloc is the cost, not the fill) | `docs/entropy-bricks.md` both repos |
| rav1e/av2e F1 — `update_cdf` split-loop, FLAT (LLVM already emits cmov) | same |
| rav1e/av2e F2 — `fc_log` dedup side-table, **+6-7% REGRESSION, do-not-retry any side-table variant** | same |
| rav1e/av2e Q2-twopass — hoisted `divu_pair`, +15% regression | same |
| rav1e Q1 eob-scan (0.9%), LF-rate-hoist (below noise), LF-alloc ("allocs are a red herring" at 0.1%) | `rusty_av1e/docs/entropy-bricks.md` |
| ⚠ rav1d ledger rows (levels fill 3.8 ms, `get_lo_ctx` SIMD + direct-index flat, dequant vectorize flat, `refmvs_find` micro-opt) — **measured with asm MSAC ON; stale for rff's no-asm build.** One re-measure each is permitted; do not iterate past that without new evidence | `rusty_av1d/docs/decode-bricks.md` |

---

# Part 2 — The uniform, repeatable process

Run this identical sequence for every target row in Part 1. One target = one
branch = one ledger entry. Never batch two targets into one measurement.

## Phase 0 — Fresh profile (per codec, before picking any target)

1. Build the **right binary** in `--release`. For CLI-driven runs:
   `cargo build --release -p rff-cli` (NOT `-p rff` — `rff` is a library and
   never relinks `ffmpeg.exe`; verify the exe mtime before trusting any run).
   Prefer in-process harnesses over the CLI wherever they exist — file I/O
   swamps the codec (`rusty_jpeg/tests/profile_jpeg.rs` documents ~1.5 s of
   disk write vs ~200 ms of codec).
2. Profile per stage on the pinned corpus input (Part 3 table) using the
   crate's existing instrument:
   - `rusty_vp9` decode: `VP9_DPROF=1` (exclusive self-time, rdtsc)
   - `rusty_vp9` encode: `VP9_PROF2=1` (nested-inclusive)
   - `rusty_jpeg`: `--features profile` (+ `--features counters` in a
     SEPARATE binary — never both at once)
   - `rusty_png`: `--features profile`
   - `rusty_mp3`: always-on `prof` buckets via the `#[ignore]`d
     `profile_encode_dense` / `profile_decode` drivers
   - `rusty_aac`: `profile_encode_hotpath` (`encode.rs:2118`)
   - `rff-resample`: `tests/bench_resample.rs`
   - No profiler yet (flac, vorbis frame-level, wrappers): add minimal
     stage buckets first, modeled on `rusty_png/src/prof.rs` — that IS the
     Phase 0 work for those crates.
3. Rank stages by **absolute cost** (ms per clip), not ratio.
4. Write the ranked profile into the ledger, dated. Profiles older than the
   current campaign are treated as fiction — including the ones quoted in
   Part 1 of this document.

## Phase 1 — Noise floor (Gate B prerequisite)

1. Run the baseline against itself ≥5 times. Record min/max/median. (Observed
   here: ±7–14% on VP9 decode wall; 17% between identical configs on another
   box.) Any candidate win smaller than this spread is a coin flip.
2. If the stage wall is too noisy to resolve the target, drop to an
   in-process rdtsc microbench with a synthetic corpus —
   `transform.rs::profile_inverse_transform` and `inter.rs::mc_microbench`
   are the templates. The microbench decides the kernel; the stage wall
   decides Gate C.
3. Decision rule: best-of-N (N≥5), **non-overlapping ranges**
   (new.worst < old.best), interleaved ABBA within one process where
   possible — `rff-codec-vp9/examples/poolbench.rs` is the template, and
   `video-tests/abba.sh` (paired win rate, |z| > 2) for env-knob A/Bs.

## Phase 2 — Target selection

1. Pick the highest-absolute-cost stage whose hot loop is our Rust and ≥5%.
2. Identify which of P1–P5 applies. Write the invariant down before touching
   code. If you cannot state the invariant precisely, the target is not
   eligible.
3. Cheap refutations first: check the disasm / a microbench — if LLVM already
   elides the bounds checks (fixed-size arrays, iterator rewrites, masked
   indices it can see through), P1 is dead on arrival. Record and move on.

## Phase 3 — Attack

1. **Safe twin stays.** Keep the safe implementation next to the unsafe one,
   selectable at runtime (env oracle switch like `RFF_MP3_XRPOW=powf` /
   `force_scalar_oracle()` in rff-resample) or via feature. The safe twin is
   the oracle for the differential test — this is already house style
   (`*_matches_scalar` family in rusty_vp9, `avx2_pair_matches_ssse3_twice`
   in rusty_jpeg).
2. Write the unsafe kernel in the crate's **single kernel module** — use the
   existing seam where one exists (`rusty_jpeg` `decode/arch/` + encoder
   `Operations` trait; `rusty_vp9` kernel files; `rusty_aac`/`rusty_vorbis`
   SIMD sites). Create `src/kernels.rs` only where no seam exists. Callers go
   through a safe wrapper; public API never requires `unsafe`.
3. Every `unsafe` block carries a contract comment from
   `references/safety-contracts.md` — the invariant AND who upholds it.
   Where an input-magnitude bound is part of the contract, add an exhaustive
   bound-proof test in the style of `idct8x8_avx2_bound_is_safe`
   (`rusty_vp9/src/transform.rs:1655`) with a byte-identical scalar fallback
   above the bound.
4. Smallest possible unsafe block. Prefer `bytemuck`/`align_to` over raw
   `transmute` for P5.
5. Respect crate policies: `rusty_jpeg`'s `platform_independent` feature must
   keep compiling with unsafe forbidden (safe twin behind the same cfg);
   `rusty_png` stays untouched unless its `forbid(unsafe_code)` promise is
   explicitly revisited with the user.

## Phase 4 — Validate (all gates, in order; any failure = revert)

| Gate | What | Pass criterion |
|------|------|----------------|
| **A. Bit-exactness** | Run the codec's oracle gate (Part 3 table) | Byte-identical output vs reference. A faster decoder that differs by one sample is a regression. Runs BEFORE any timing. |
| **B. Best-of-N timing** | Best-of-N (N≥5), interleaved | Non-overlapping ranges AND delta > Phase 1 noise floor |
| **C. Chain, not link** | Re-time one level above the change (stage profiler + end-to-end bench on the corpus input) | Stage-level and end-to-end time did not regress. Op-level wins routinely die here (a 1.56× matmul win once made the downstream softmax 82% slower). |
| **D. Safety audit** | Differential test safe-twin vs unsafe-twin on randomized + adversarial inputs (sparse paths, mismatched strides, edge sizes — per the `avx2_pair_matches_ssse3_twice` house style); Miri on the kernel's unit tests where Miri can run it (needs `cargo +nightly miri` — toolchain is pinned stable 1.95.0, no miri component; cargo-fuzz/ASan is unavailable on this Windows/MSVC box, so Miri + differential tests carry the load) | Identical outputs; no UB findings; contract comment matches the final code |
| **E. Concentration** | `grep -c unsafe` per file | All new unsafe lives in the designated kernel module(s); count recorded in the ledger entry |

## Phase 5 — Ledger

Append one entry per target to Part 4:
`date | crate | function | pattern(s) | profile % before | result (SHIPPED x% / FLAT-REVERTED / CLOSED-COLD <5% / REFUTED) | commit`.
Reverted and refuted attempts are recorded too — a refuted lever is knowledge
(see the pre-refuted table in Part 1).

---

# Part 3 — Per-codec oracles, corpus, profilers, benches

| Codec | Bit-exact oracle (Gate A) | Command | Pinned corpus | Profiler | Bench harness |
|---|---|---|---|---|---|
| rusty_vp9 decode | libvpx per-frame MD5, 315/315 vectors | `VP9_VECTORS_DIR=vp9-vectors cargo test -p rff --test vp9_conformance --release -- --ignored --nocapture` | `vp9-vectors/` (17 pairs) + `rff-codec-vp9/benches/data/vp9_720p.ivf` | `VP9_DPROF=1` | `cargo bench -p rff-codec-vp9` (fps/Mpx-s, best+median) |
| rusty_vp9 encode | dual gate: `*_roundtrips_bit_exact` in-crate + IVF external decode; recon gate `VP9_RECON_CHECK` | `cargo test -p rusty_vp9 --release` | `video-tests/manifest.tsv` 20-clip Derf corpus | `VP9_PROF2=1` | `examples/speedbench.rs`, `examples/poolbench.rs` (ABBA), `video-tests/run_analysis.sh` |
| rusty_jpeg | `avx2_pair_matches_ssse3_twice` + `*_matches_scalar` family + planar/progressive tests; whole-decode via `tests/profile_jpeg.rs` | `cargo test -p rusty_jpeg --release` (+ `--test profile_jpeg -- --ignored --nocapture`) | `tests/fixtures/progressive_libjpeg.jpg` + `examples/corpus_sweep.rs` foreign corpus | `--features profile` / `--features counters` (separate binaries); `RUSTY_JPEG_ABLATE` arms | `examples/decode_bench.rs`, `entropy_probe.rs` |
| rusty_png (if unblocked) | CRC-of-decode vs golden `results*.txt`, 194-entry pngsuite | `cargo test -p rusty_png` | `tests/{pngsuite,pngsuite-extra,bugfixes,animated}` | `--features profile` | `cargo bench -p rusty_png --features benchmarks` (criterion) |
| rusty_mp3 | `lut_matches_linear_for_every_codeword`, `fast_matrixing_matches_dense`, `conformance_corpus_round_trips` (+ external FFmpeg/LAME via `MP3_ENC_DIR`) | `cargo test -p rusty_mp3 --release` | `corpus/` WAVs + `lab::signals` deterministic corpus | always-on `decode::prof` / `encode::prof` + `#[ignore]` drivers | `examples/mp3lab.rs` (`--features lab`) |
| rusty_aac | MDCT-vs-direct oracle, roundtrip-through-decoder tests, external ffmpeg via `emit_*_for_external_check` | `cargo test -p rusty_aac --release` (+ `-- --ignored` emitters) | `corpus/` WAVs | `profile_encode_hotpath` | none — add per Phase 0 |
| rusty_vorbis | `brute_quantize_matches_reference` (AVX2 bit-identity), `table_mdct_matches_direct`, lewton cross-decode | `cargo test -p rusty_vorbis --release && cargo test -p rff-codec-vorbis` | `corpus/` WAVs | test-only `frame::prof` | none — add per Phase 0 |
| rff-codec-flac | claxon losslessness (independent decoder), `flac_roundtrip`, ratio vs ffmpeg | `cargo test -p rff --test flac_roundtrip` (+ `flac_baseline -- --ignored`) | `corpus/` WAVs | none — add per Phase 0 | none — add per Phase 0 |
| rff-resample | `polyphase_matches_scalar_oracle` (+ `force_scalar_oracle()` switch), length cross-check | `cargo test -p rff-resample --release` | synthetic in-test (24 s 44.1→48k) | n/a (single stage) | `tests/bench_resample.rs` (best-of-7) — the audio A/B template |
| AudioFrame P5 | end-to-end pipeline tests (`audio_pipeline.rs`, `resample_pipeline.rs`, codec roundtrips) — byte-identical output files pre/post | `cargo test -p rff --release` | pipeline fixtures | n/a | per consuming codec |
| rusty_h264 (`../rs_h264`) | `bench/conf_matrix.sh` (encode → ffmpeg-decode AND self-decode, recon **byte-identical**); `RH_CABAC_TRACE` symbol oracle vs instrumented openh264; `tests/fuzz_no_panic.rs`; `tests/satd_avg_compare.rs` style `*_matches_scalar` for new accel kernels | in `rs_h264`: `bash bench/conf_matrix.sh`; `cargo test -p rusty_h264-decoder` | 34-stream decode corpus; `video-tests/manifest.tsv` (pixels shared with this repo's corpus) | `--features profile` (28-stage, rdtsc) + `#[ignore]` `profile_{encode,decode}` tests + ceiling-probe examples | `bench/` crate + pinned PS harnesses (`pinbench.ps1` …); no criterion, deliberate |
| rusty_av1e (`../rusty-av1-toolkit`) | Byte-identical bitstream vs stock rav1e (SHA256 gate + FNV in `tests/profile_encode.rs stage_breakdown`); racecar off/on/stock SHA-equal; `nz_map_area_kernel_matches_scalar`; encode→decode via aom/dav1d (`decode_test*` features). ⚠ recorded baselines are asm-build values — **re-derive under `--no-default-features` first** | `cargo test` + `stage_breakdown` FNV; CI legs `no-asm-tests` (`RAV1E_CPU_TARGET=rust`) | deterministic synth clip in `tests/profile_encode.rs` (+ `RAW=` for real I420) | `--features profile` (`RAV1E_PROF`), ~35 stages | criterion `benches/{bench,dist,mc,plane,predict,rdo,transform}.rs` (`--features bench`) |
| rusty_av1d (`../rusty-av1-toolkit`) | Decode md5 vs dav1d (`--muxer md5 --threads 1`), dav1d-test-data vectors (8/10/12-bit), Argon streams (`tests/dav1d_argon.bash`). ⚠ checkasm only validates asm-vs-Rust — for Rust-only changes the md5 gate is the sole oracle; re-derive baseline no-asm | `cargo test`; md5 runs per `docs/decode-bricks.md` recipe | `tests/dav1d-test-data/` | `--features profile` (18 stages) | **none** — no criterion; build an MSAC microbench as Phase 0 work |
| rav2d (`../rs_AV2ed`) | `bench/conformance/run.sh` — byte-for-byte `cmp` of YUV vs dav2d oracle (`ORACLE=avmdec` for 4:2:2/4:4:4); `bench/m1/` per-symbol dif/rng/cnt trace; per-stage pixel oracles (`run_deblock_verify` etc.); in-file scalar-twin unit tests (`mc_translate_matches_dav2d` …) | `bash bench/conformance/run.sh` | `bench/conformance/corpus/*.ivf` (gen.sh + mksrc.py deterministic) | **none — add as Tier 0** (port `rusty_av1e/src/prof.rs`) | shell-level only (`bench/speedtest.sh`) — add in-process bench as Tier 0 |
| rusty_av2e (`../rs_AV2ed`) | Dual gate: avmdec decodes the stream AND rav2d recon matches avmdec md5 (`encoder.rs:9672`); byte-identical FNV hash + 547-test suite; CDF table-provenance asserts vs rav2d; `check_asm` scalar-twin pattern for any new SIMD | `cargo test`; `tests/profile_encode.rs` instruments | deterministic `synth_plane` clip (env knobs `W,H,FRAMES,SPEED,…`) | `--features profile` — **but zero coverage of the AV2 leaf path; instrument it first** | criterion benches (AV1-path kernels); `bench_encode` best-of-N |
| Quality backstop (audio, when a change could touch output) | PEAQ ODG via `tools/quality/` (external oracle; NMR self-metric for iteration) | `tools/quality/corpus_eval.sh` | `corpus/` | — | — |

**Standing repo gates that must stay green regardless of target:**
CI `conformance` job (VP9 vectors, every PR) · CI `fuzz` (50k malformed
streams) · `cargo test --workspace --exclude rff-ui`.

---

# Part 4 — Ledger

| date | crate | function | pattern | profile % before | result | commit |
|---|---|---|---|---|---|---|
| 2026-08-02 | rusty_jpeg | (13 entries) | P1/P2/P5 | — | **1 real, 9 DEAD, 3 in noise** — the one real fix needed no `unsafe` | see closed sections |
| 2026-08-06 | rff-codec-png | `analyse` + Indexed lut | data-structure (**not** unsafe) | 222.8 ms of a 320 ms encode on the one image that hits it | **SHIPPED 3.55x** whole-CLI, byte-identical | swept into `8691d0f` |
| 2026-08-06 | rff-codec-png | `pack_indices` | P1 + strength-reduce | 0.5–16 ms, and only below 8 bits/index | **CLOSED-COLD** | — |
| 2026-08-06 | rff-codec-png | gray→RGB expansion | P2 | 2–12 ms vs an 80–700 ms encode | **CLOSED-COLD** | — |

### 2026-08-06 — `rff-codec-png` closed. Three entries, one real, no `unsafe`.

The same shape as the JPEG audit, and the same two checks did the work.

**Check 1 — does it execute, and on what content?** The plan sizes `analyse` as
a per-pixel `HashMap` probe. A synthetic ≤256-colour image put it at **69 ms**,
which was the wrong target: on the **real** corpus almost everything trips the
256-colour bail within a few hundred pixels and costs **~0.03 ms**. Exactly one
real image does not — `gfx_diagram`, 5.35 MPx with **141 colours** — where the
probe runs for all 5.35 M pixels. *Synthetic content chose the wrong image;
only the real corpus found it.*

**Check 2 — had LLVM already elided the bounds checks?** The entire crate emits
**six** `panic_bounds_check`, all inside `send_frame` (every candidate inlines
into it). The P1 framing was dead on arrival, exactly as it was for 9 of JPEG's
13 entries.

**What was actually expensive was the data structure** — which is what the plan
row predicted, verbatim: *"data-structure fix, not unsafe"*.

1. `HashMap` uses SipHash-1-3 — keyed, DoS-resistant — for a 4-byte key in a
   table that never exceeds 256 entries.
2. `analyse` built an index map, discarded it, and the `Indexed` branch then
   built a **second** map and probed it once per pixel: two full walks.

| on `gfx_diagram` (5,351,301 px) | ms |
|---|---|
| shipped `analyse` (HashMap) | 109.2 |
| shipped second pass (lut probe) | 113.6 |
| **= total shipped** | **222.8** |
| fused single pass, open-addressed table | **11.2** (**19.91×**) |

Fix: a fixed 1024-slot open-addressed table (`u32` key, `u8` value, Fibonacci
multiply-shift hash) and `analyse` emits the per-pixel indices from the same
pass. No allocation, **no `unsafe`** — ≤256 live entries in 1024 slots keeps the
load factor under 25%.

Gates: output **byte-identical on 9/9** real graphics images; per-pixel indices
proved identical to the HashMap version by differential test; whole-CLI 15 ABBA
pairs, pinned, on-core cycles — `gfx_diagram` **670.6 → 189.0 Mcyc (3.55×,
z 3.87)**, `gfx_uiart` 1.07× (z 3.36), `gfx_logo` 1.07× (z 2.32), `gfx_chart` /
`gfx_terminal` inside noise, and a **photograph unchanged at 1.00×** (no
regression where `analyse` bails early).

**Running tally across both audited codecs: 16 entries, 2 real, 0 `unsafe`
written.** Both real wins were data-structure or loop-shape fixes in safe Rust.

### `rusty_png` — still policy-blocked, but now sized

The crate's `#![forbid(unsafe_code)]` was not touched. Fresh stage profile
(2026-08-06, `--features profile`, real corpus) for whoever revisits it:

| stage | photographic | graphics |
|---|---|---|
| **decode `unfilter`** | 30.0–40.5% | **53.7–66.3%** |
| decode `inflate` | 50.3–63.9% | 7.5–27.6% |
| decode `transform` | 3.8–7.4% | 6.5–28.1% |
| encode `deflate` | 77–99.5% | 77–99% |
| encode `filter` | 0.2–3.1% | 0.5–7.4% |

So the plan's instinct was right about **where**: `filter.rs`'s unfilter arms
are genuinely 30–66% of decode and are the only PNG target that clears the 5%
bar by a wide margin. Two caveats before spending the covenant:

- **We are already 2.67–2.95× faster than FFmpeg at decode.** Prize is small.
- `inflate`/`deflate` — the majority of both directions — live in `fdeflate` and
  `zlib-rs`, **outside this repo**, so the covenant buys nothing there.


**Incidental findings from the survey (act on independently):**
- 🐛 `rusty_aac::xpow_avx2` scalar tail missing `i += 1` (`encode.rs:737-741`) — infinite loop for non-multiple-of-4 lengths; currently unreachable but should be fixed now.
- `rff-resample::dot_width` re-runs `is_x86_feature_detected!` per output sample (`lib.rs:287`) — safe one-line hoist, likely a free win before any unsafe.
- `rusty_mp3::Reservoir::assemble` per-frame alloc + double-reverse (`decode/reservoir.rs:19-28`) — codec-memory-copies territory.
- 🐛 `rav2d::Plane::at` calls `env::var("ATDBG")` **per pixel read** (`av2_frame.rs:33-41`); per-SB `env::var("MSBT")` at `obu.rs:5310`; release `eprintln!` guards in `residual_add`/`mc_translate`. Tier −1 of the AV2 section.
- ⚠ `rav2d` is **not under version control** (no `.git`) — initialize before touching it.
- `rusty_av1e` upstream latent bug: `tx_domain_rate=true` + `tx_domain_distortion=false` panics at `rdo.rs:454` (`temporal_rdo()` checks only `tx_domain_distortion` while `rdo_type` becomes `TxDistEstRate` on `use_tx_domain_rate` alone).
- ~~rff's `rusty_av1e` dep disables `threading`~~ **RESOLVED 2026-08-05**: asm+threading enabled for native targets in `rff-codec-avif/Cargo.toml` (wasm32 keeps the no-asm base; AVIF roundtrip green).
- ⚠ **rff pins `rusty_h264 = "0.2"` and resolves 0.2.1 from crates.io, but the owned repo `../rs_h264` is at workspace version 0.7.0** — the caret can never cross 0.x minors, so rff ships a five-minors-old h264 codec. Bump `rff-codec-h264/Cargo.toml:21` (and re-run its conformance/roundtrip gates) when ready.
- `rusty_h264` facade defaults `asm` on, but its build.rs silently skips asm when nasm is absent — verify which arm a benchmark binary actually took before trusting numbers.
- ⚠ rav1d's `asm` feature is now live in rff's decode path — the "decode path is 100% safe Rust" claim in the root Cargo.toml comment no longer holds for native targets (comment updated).
