# rusty_h265 — the Great Gate inventory: every content-adaptive route

**Measured 2026-09-05.** Every route below is instrumented with a population
counter (`RH265_CENSUS=1`) and measured across a corpus chosen to cover the
capability classes, not just the common one.

## What a gate means in a DECODER

The Great Gate doc (§3) draws the line, and it changes how every row here is
judged:

> Encoder rows are decision functions (gates trade quality/speed, judged by
> per-clip BD). **Decoder rows are throughput functions — bit-exact by law; gates
> are pure speed, judged by |z| > 2 paired CPU time.** Decode-side content
> dispatch is mostly *by construction*, because the bitstream declares the
> content; the remaining decode gates are **capability × population**: *a
> population of streams served by a slow path is a missing kernel.*

So a decoder gate has no quality dimension at all. The canonical form
`GATE := (unit, signal, threshold, arms, fallback, ledger)` collapses:

- **signal** — a bitstream property, already parsed. Free, exact, no estimation.
- **threshold** — an exact predicate, never a percentile. The stream *declares*
  which arm is correct; we are choosing an implementation, not a trade.
- **arms** — implementations that must agree **bit-exactly**. That is the gate.
- **ledger** — the population, per content class. A route with no population is
  unreachable code; a population with no fast arm is a missing kernel.

The corpus is the instrument: a route looks cold from any single stream.

## The corpus

| stream | class it covers |
|---|---|
| `in_to_tree_720p_8bit` | natural video, inter, 8-bit (x265 medium) |
| `IPRED_A_docomo_2` | all-intra |
| `TILES_A_Cisco_2` | tiles |
| `WPP_A_ericsson_MAIN10_2` | wavefronts + 10-bit |
| `IPCM_A_NEC_3` | PCM blocks |
| `WP_A/B_Toshiba_3` | explicit weighted prediction |
| `LS_A/B_Orange` | lossless (transquant bypass) |
| `DBLK_A_MAIN10_VIXS_4` | transform-skip-heavy, 10-bit |
| `SAO_A_MediaTek_4`, `TSCL_A_VIDYO_5` | SAO, temporal scalability |

## The inventory

Population is *calls*, per stream.

### A. Motion compensation — routed by fractional position (§8.5.3.3.3)

| route | arms | in_to_tree | TILES_A | WP_A | WPP_A (10-bit) |
|---|---|---:|---:|---:|---:|
| A1 fractional position | full-pel / H-only / V-only / 2-D | 5,111 / 38,456 / 36,956 / 175,979 | 407 / 119,759 / 97,109 / 457,281 | **21,631** / 6,672 / 3,910 / 13,339 | 24 / 11,216 / 3,883 / 16,154 |
| A2 footprint | interior slice / edge-padded copy | `MC_EDGE_PAD` | | | |
| A3 prediction write | uni / bi / fp-uni / fp-bi / **mixed fp-bi** / weighted | 11,267 / 65,595 / 616 / 1,575 / 3,441 / 0 | | 0 / 0 / 0 / 0 / 0 / **45,552** | |
| A4 shift1 | zero-shift (8-bit) / shifted | 256,502 / 0 | 674,556 / 0 | 45,552 / 0 | 0 / **31,277** |
| A5 tap count | luma 8-tap / chroma 4-tap | by construction | | | |

**A1 is the clearest content axis in the decoder.** `WP_A` takes full-pel on 47 %
of its blocks; `TILES_A` on 0.06 %. Same code, three orders of magnitude apart —
which is why the full-pel identity (a rectangle copy) was worth building at all.

### B. Residual — routed by transform kind, size and sparsity

| route | arms | in_to_tree | IPRED | DBLK_A | LS_B |
|---|---|---:|---:|---:|---:|
| B1 transform kind | bypass / **skip** / DST / DCT | 0 / 0 / 7,084 / 41,008 | 0 / 1,125 / 52,520 / 72,987 | 0 / **10,757** / 2,350 / 3,440 | **248,989** / 161 |
| B2 block size | 4 / 8 / 16 / 32 | 12,552 / 18,305 / 9,903 / 7,332 | 80,982 / 34,305 / 9,973 / 1,372 | 16,547 total | |
| B3 scaling list | signalled / flat | 0 / 48,092 | 0 / 126,632 | | |
| B4 sparsity (live ÷ block area) | last-significant bound | **32.4 %** | 12.6 % | 94.6 % | |

**B4 is the most valuable route in the decoder, and it is not a branch.** The
last-significant-coefficient position bounds the transform to a rectangle: on
`WP_A` only **0.42 %** of each block is live, on `TILES_A` 5.1 %, on natural video
32 %. That gate spares between two thirds and 99.6 % of the transform arithmetic
depending on content.

### C. Intra prediction

| route | arms | in_to_tree | IPRED | TILES_A |
|---|---|---:|---:|---:|
| C1 mode | planar / DC / angular | 5,513 / 2,862 / 15,034 | 67,952 / 47,065 / 166,038 | |
| C2 angular direction | mode ≥ 18 row-major / < 18 transposed | 6,630 / 8,404 | 106,616 / 59,422 | 47,875 / 34,763 |
| C3 `ifact == 0` | pure copy / 2-tap filter | inside the kernel | | |
| C4 reference smoothing | filtered / plain | 1,092 / 10,919 | 14,736 / 122,709 | 3,112 / 51,174 |
| C5 reference availability | all present / substitution needed | 7,286 / **16,123** | 86,777 / **194,278** | 32,042 / **81,082** |
| C6 DC + residual | fused / filled | 667 / 2,195 | 5,921 / 41,144 | 641 / 11,359 |

**C5 corrects a claim made earlier in this campaign.** When the `substitute`
short-circuit was built, its comment said "in the interior of a picture nothing
[is missing]". The population says the opposite: **69–72 % of intra blocks need
substitution**, consistently across every class. The short-circuit is still
correct and still worth having — it fires on the other 28–31 % — but the
description was wrong, and substitution is the *common* path, not the exception.

### D. Loop filters

| route | arms | in_to_tree | IPRED | WP_A |
|---|---|---:|---:|---:|
| D1 SAO type | off / band / edge | 36,710 / 524 / 5,966 | 2,918 / … | 5,881 / … |
| D2 SAO interior | interior kernel / per-sample | 6,026 / 0 | 3,112 / 0 | 164 / 0 |
| D3 picture bypass | any bypass sample → whole picture scalar | 0 | 0 | 0 |
| D4 deblock luma | filtered edges | 437,526 | 339,920 | 143,175 |
| D5 deblock chroma | bS == 2 edges only | instrumented | | |

### E. Picture-level capability

| route | in_to_tree | TILES_A | WPP_A |
|---|---:|---:|---:|
| E1 tiles | 0 | **100** | 0 |
| E2 wavefronts | **60** | 0 | **48** |
| E3 bit depth | 8-bit ×60 | 8-bit ×100 | **10-bit ×48** |

### F. ISA — the one route that is not content

Runtime CPU detection (scalar / SSE2 / AVX2), cached once. Every kernel carries
`*_SCALAR` and `*_SIMD` counters. **On every stream in the corpus, every
`_SCALAR` counter reads 0** — no kernel silently falls back.

## What the ledger found

### Two missing arms — populations served by a slow path

**1. Explicit weighted prediction (§8.5.3.3.4.3) had no kernel at all.**

`weighted_write` was a scalar per-sample loop — multiply, shift, add, clamp. On
`WP_A_Toshiba_3` it serves **every one of 33.9 M motion-compensated samples**:
`PUT_UNI` and `PUT_BI` both read 0 there, because the weighted path takes all of
it.

Both forms turned out to be one `pmaddwd` each. Uni pairs `(x, 1)` against
`(w, round)` — the spec bounds the weight to `(1 << denom) + [−128, 127]` with
`denom ≤ 7`, so both multiplicands fit `i16`. Bi is better still: interleaving the
two predictions puts `x` and `z` in adjacent lanes, so `(x, z)·(w0, w1)` is the
entire weighted sum in one instruction.

**1.182×, 19/21, z = 3.71** on a 2,560-frame weighted workload, and 1.062–1.125
instructions per output against roughly 8 scalar. (A first reading of 1.300× came
from the raw 78 ms stream — under six timer quanta. The longer workload is the
number; the short one was inflated by its own resolution.)

**2. Transform skip (§8.6.2) had no kernel.**

A shift-add-shift over the block, scalar. It looks negligible on natural video —
0 blocks on `in_to_tree` — and it is **65 % of every transform block on
`DBLK_A_MAIN10_VIXS_4`** (10,757 of 16,547). Those blocks skip the DCT entirely,
so that loop *is* their whole inverse transform.

**0.500 instructions per output** (8 per 16 samples) against ~5 scalar, a 10×
reduction. On a 320-frame workload the clock agrees but barely: **1.014×, 15/19,
z = 2.52** — a verdict, and exactly the ~1.5 % the arithmetic predicted before the
kernel was written. (The raw 16 ms stream returned p25/p75 of 0.500/2.000: ±1
timer tick of nothing.)

### A dead counter that produced a wrong prune

`PUT_WEIGHTED` was **declared and never incremented anywhere**. Reading it as 0, a
decision was recorded earlier in this campaign that a weighted-prediction kernel
would be unreachable code and should not be built.

That is the "a flat arm is not evidence until you know the arm is wired" law
(§10) turned on the census itself. The counter was not measuring a cold path; it
was measuring nothing. The fixed counter reads 45,552 on `WP_A`.

A second instance of the same class, caught during this campaign: the census
predicate for the new weighted arm was computed *before* the `RH265_SCALAR_GATE`
switch was consulted, so the scalar arm still reported `SIMD`. **A census
predicate must match its dispatch exactly, switch included** — otherwise the
instrument reports an arm that never ran.

### A corpus gap that looked like a feature ceiling

`RT_TX_BYPASS` reads 0 on eight of the eleven streams and **248,989 on
`LS_B_Orange_4`**. Lossless coding is not rare in general — it is absent from the
streams anyone reaches for first. That is the great-gate law directly: corpus-
neutral on a feature with a known physical premise is a corpus gap. No arm is
needed here (bypass is a no-op by construction), but the same blind spot hid the
weighted-prediction kernel for the whole campaign.

### A third missing arm: neutral weights on the expensive path

Wiring the dead `PUT_WEIGHTED` counter exposed something bigger than the kernel
it was hiding. On the **mainstream** 720p bench stream it reads **110,604 of
187,466 prediction writes — 59 %**. x265 emits a `pred_weight_table` for P slices
by default, and a table in the slice header routes *every* prediction in that
slice through §8.5.3.3.4.3, whatever the values are.

Those values are almost always the identity, and with `w = 1 << denom, o = 0` the
weighted form collapses to the default one exactly:

```text
  uni:  ((x·2^d + 2^(d+s−1)) >> (d+s)) + 0  ==  (x + 2^(s−1)) >> s
  bi:   (2^d(x+z) + 2^(d+s)) >> (d+s+1)     ==  ((x+z) + 2^s) >> (s+1)
```

term for term `put_uni` and `put_bi`. `neutral_weights()` detects it and routes
accordingly. The population moves as designed:

| counter | before | after |
|---|---:|---:|
| `PUT_WEIGHTED_SIMD` | 110,604 | **7,239** |
| `PUT_UNI_SIMD` | 11,267 | **110,131** |
| `MC_FULLPEL_UNI` | 616 | **5,117** |

The full-pel fast path re-opens as a side effect — it is gated on "no weighting"
and had therefore been closed for every P slice.

**And the clock cannot see it: 1.000×, z = 0.24.** 110,604 calls moved from a
1.125 instructions/output kernel to a 0.406 one and the time did not change.
`put_uni` is one load and one store per sixteen samples; it is memory-bound, and
instructions removed from a memory-bound loop do not convert into time. Kept
anyway — it is exact, strictly less work, and it re-opens a route — but recorded
as **deterministically smaller, not measurably faster**.

## What the gating actually bought

Measured, paired, pinned CPU time, ABBA, on workloads long enough to clear the
timer:

| content class | effect | verdict |
|---|---|---|
| weighted prediction (`wp_x10`, 2,560 frames) | **1.182×** (19/21, z = 3.71) | ✅ |
| transform-skip-heavy (`dblk_x40`, 320 frames) | **1.014×** (15/19, z = 2.52) | ✅ marginal |
| mainstream inter (`in_to_tree`, 60 frames) | 1.014× (18/28, **z = 1.51**) | ❌ inside noise |

**The mainstream row is the honest one and it did not survive N.** At 21 pairs it
read z = 3.50 and looked like a verdict; at 31 pairs it fell to z = 1.51. The
higher-N run is the better estimate, and the campaign's effect on ordinary content
is **not distinguishable from noise** — even though the route census proves 59 %
of its prediction writes were moved to a cheaper kernel.

That is the expected shape, not a disappointment: decoder gates are
capability × population, and these populations are concentrated in specific
content classes. The value delivered here was mostly **diagnostic** — a dead
counter that had produced a wrong prune, and 59 % of mainstream writes sitting on
a path nobody had looked at.

## Silent zeros — the hunt this campaign forced

A zero is the most dangerous value in an instrumented decoder, because a value
nobody wrote is indistinguishable from a value that was measured as zero. This
campaign produced two of them within hours of each other, so the class got swept
deliberately. Findings, worst first:

**1. The conformance gate could pass having decoded nothing.** `HEVC_ONLY=typo`
selected no streams, and every assertion after it is of the form "nothing went
wrong" — all trivially true on an empty run. The gate went **green in 0.01 s**,
indistinguishable from 147/147 to anything reading an exit code. `assert!(n > 0)`
now makes the work a precondition of passing.

**2. The SEI self-check could rot silently.** `assert_eq!(sei_pass, sei_seen)` is
`0 == 0` if the decoded-picture-hash SEI ever stopped being parsed. The strongest
self-check in the decoder — 6,183 per-picture hashes across the corpus — would
have reported success while verifying nothing. Now floored at `sei_seen > 50`.

**3. A missing corpus was a green CI build.** Both conformance tests skip when
`hevc-vectors/` is absent. `HEVC_REQUIRE_VECTORS=1` now turns that skip into a
failure, so a broken fetch cannot masquerade as a pass.

**4. Two dead census counters.** `PUT_WEIGHTED` (which caused a wrong prune — see
above) and `RT_INTRA_ANG_COPY`, the second one introduced *by this very campaign*,
hours after writing up the first. Both now caught by
`every_census_counter_is_actually_incremented`, a test that reads the source and
fails on any counter declared without a `bump`/`route`/`arm` site.

**5. A counter that lied by aliasing.** The SAO-type route was instrumented onto
the *kernel's* counters, so `SAO_BAND_SIMD` read 224 on `LS_B_Orange_4` where the
band kernel runs exactly **zero** times (the picture-level bypass flag sends every
sample to the scalar path). A counter reporting work that never happened fails in
the same direction as one reporting nothing. Given dedicated counters.

**6. A census predicate that disagreed with its dispatch.** The weighted kernel's
`simd` flag was computed before its bring-up switch was consulted, so the scalar
arm still reported `SIMD`. Now guarded by
`census_predicates_consult_the_same_switches_as_the_dispatch`.

Checked and **not** defects, recorded so they are not re-litigated: the discarded
`sps_changed` (§C.5.2.2 permits either reading; the comment says which the
conformance md5s follow), `output_size()`'s `saturating_sub` (the oversized
conformance window is already rejected at parse per §7.4.3.2.1), and
`found.unwrap_or(0)` in `substitute` (the availability guard above it makes `None`
unreachable).

## Confirmation: every gate mutation-tested

A guard that cannot fail is itself a silent zero, so each one was verified by
**reintroducing the defect and confirming it fires**, then restoring. Run
2026-09-06.

| # | gate | mutation applied | fired? |
|---|---|---|---|
| 1 | dead census counter | added `MUTATION_TEST_DEAD_COUNTER` to `counters!` | ✅ named it exactly |
| 2 | census predicate ↔ dispatch | dropped `!scalar_gate()` from one `let simd =` | ✅ reported `pixel.rs:1093` |
| 3 | missing corpus in CI | renamed `hevc-vectors/` away, set `HEVC_REQUIRE_VECTORS=1` | ✅ failed; skipped cleanly without the flag |
| 4 | SEI self-check vacuity | forced `dec.verify_sei = false` | ✅ "only 0 pictures carried a … SEI" |
| 5 | vacuous conformance pass | `HEVC_ONLY=nomatch` | ✅ "no streams matched" |
| 6 | allocator provenance | rebuilt without `--features bench-alloc` | ✅ pinbench refused to produce a table |

Corpus restored to 147 streams, no mutation residue, bench binary back to
`alloc=rusty isa=AVX2`.

### The last one was a law the tooling did not obey

Guard 6 was still open until this pass. `pinbench.ps1` *documented* the
rusty_alloc requirement in a comment, and a comment is what failed to stop this
campaign being measured under the system allocator in the first place
(`codec-measurement` §13: a law in a document that the tooling does not obey is
a law that will be broken).

The decoder binary now reports `alloc=` and `isa=` in its stats line, and the
harness refuses to time a build whose provenance is wrong. That also closes the
stale-binary trap from earlier in the campaign, where a `--no-default-features`
verification build silently replaced the benchmark binary and the next
measurement read 66 % slow without anything complaining.

### Standing state

- **147/147 JCT-VC HEVC_v1 bit-exact**, SEI picture hashes **6,183/6,183**.
- 32 decoder tests, 18 kernel oracle tests, all green.
- **Zero scalar-arm populations** on every stream in the capability corpus — no
  kernel silently falls back.

## Status

- **Routes inventoried: 24**, across five stages plus the ISA dispatch.
- **Gated (population measured, arms bit-exact, fast arm reached): 23.**
- **B3 scaling lists is the one incomplete row** — only the flat arm has a
  population on this corpus. The signalled arm exists and the conformance suite
  exercises it, but it needs a stream in the standing corpus before it can honestly
  be called gated.
- **Missing arms found and built: 2** (weighted prediction, transform skip).
- **Gate: all 147 JCT-VC HEVC_v1 streams bit-exact**, 30 decoder tests, 18 kernel
  oracle tests — after both new arms.

`RH265_SCALAR_GATE=1` puts both new arms back on their scalar twins, so either can
be A/B'd inside one binary on the content class whose population justified it.

## The narrow-arithmetic sweep — kernels that are correct only by a distant check

A SIMD twin often computes in a narrower type than the scalar twin it must
match. That is usually fine and deliberate. It becomes a defect when the
narrowing is sound only because of a bound asserted **somewhere else entirely**,
and no test in the tree can reach the violating case.

This class has a signature worth naming, because every ordinary gate passes:

* the scalar oracle is `i32` and stays right;
* `*_matches_scalar` sweeps the bit depths the corpus contains, so it agrees;
* the 147-stream conformance gate is Main / Main 10 only, so it agrees;
* and the wrong answer appears only on content nobody can currently produce.

So it is not caught by testing harder — only by auditing the arithmetic width
against the bound, and then making the kernel ENFORCE the bound itself.

### What the sweep found

Every kernel in `rusty_h265-accel`, by narrowing:

| kernel | arithmetic | bound | verdict |
|---|---|---|---|
| `planar` | `i32` throughout (`madd_epi16`, `mullo_epi32`), `packs` only at the end | pixels ≤ max | safe — same width as scalar |
| `transform_skip` | `i32` (`slli/add/sra_epi32`) | — | safe |
| `weighted_uni/bi` | `pmaddwd` → `i32` | — | safe |
| `fir_h` / `fir_v` | `pmaddwd` → `i32` | — | safe |
| `add_residual`, `put_uni`, `put_bi`, `add_residual_const` | saturating `packs_epi32` + `adds_epi16` | final clamp to `[0, max]` | safe — see below |
| `copy_shift` | `sll_epi16` | 14-bit intermediate by design | safe at every depth |
| **`angular`, `angular_t`** | **`mullo_epi16`** | **`31·max ≤ 32767`** | **guarded** |
| **`sao_band`, `sao_edge`** | **`pshufb` over `i8` entries** | **`0 ≤ off+32 ≤ 255`** | **guarded** |

**Saturation is benign when the following clamp is tighter.** `add_residual`
narrows an `i32` residual with `packs_epi32` and adds with `adds_epi16`, both
saturating at ±32767 — then clamps to `[0, max]` with `max ≤ 1023`. Anything
that saturated was already outside the clamp, so it lands identically. Worth
stating, because it looks like the same defect and is not.

### Defect 1 — the angular `i16` multiply

`mullo_epi16` keeps the low 16 bits, and `f·(a−b)` reaches `31·max`:

| bit depth | max | `31·max` | fits `i16`? |
|---|---:|---:|---|
| 8 | 255 | 7,905 | yes |
| 10 | 1,023 | 31,713 | yes — **3% of headroom** |
| 12 | 4,095 | 126,945 | no, wraps |

The scalar twin computes in `i32`; the **NEON** twin widens with `vmull_s16`.
So above 10 bits only the x86 arms are wrong — the same stream decoding
differently on two architectures.

### Defect 2 — the SAO `pshufb` table, and a guard that was itself wrong

`ctu.rs` implements the spec's bit-depth scaling in full
(`offset_abs << (BitDepth − Min(BitDepth,10))`), so the parser already produces
±124 offsets at 12-bit while the kernels carry offsets as **bytes**.

The first guard I wrote tested the `i8` range — and the test caught it. `pshufb`
returns bytes, and the kernel ORs `0x8000` into the index so the odd byte reads
zero, which means the entry comes back as an **unsigned** byte and is un-biased
with `sub_epi16`. An entry of `−32` is `0xE0`, reads back as `224`, and
`224 − 32 = 192` — every affected pixel off by exactly 256. The real bound is
`0 ≤ offset + 32 ≤ 255`, i.e. offsets no more negative than `−32`.

At 10-bit the most negative offset is `−31`. **One unit of margin**, and nothing
had ever said so.

*A guard written from the same reasoning that produced the bug inherits the
bug.* The only reason this one was caught is that its test forces the condition
and compares against the oracle, rather than asserting the guard's own logic.

### The fix: the kernel refuses rather than wraps

Both preconditions now extend the existing `ok` gate, which already falls back
to the scalar twin — so enabling RExt would cost speed on those kernels, not
correctness. The check tests the **actual values** (`offsets_fit_lut`) rather
than a bit-depth proxy, keeping the precondition and its check the same
statement. Because census reads the same `ok`, the arm counters stay honest
automatically.

Tests sweep bit depths 8/10/12/14 with offsets scaled exactly as `ctu.rs` scales
them, and each asserts it actually REACHED the failing condition
(`stressed > 0`) — a test that silently never reaches the case is the thing that
let both defects live.

### Mutation results

`tools/hevc/mutate_guards.py` removes each guard and confirms a test goes red:

| guard removed | killed by |
|---|---|
| `offsets_fit_lut(band)` | `sao_is_exact_at_rext_offset_magnitudes` |
| `offsets_fit_lut(offs)` | `sao_is_exact_at_rext_offset_magnitudes` |
| `angular_i16_is_exact(max)` | `angular_is_exact_at_rext_bit_depths` |

**The harness needed a fix before its verdict could be trusted.** cargo
fingerprints by mtime, and swapping a file back and forth fast enough leaves the
timestamp looking unchanged, so the crate is not rebuilt. In a mutation harness
that means the mutated code never ran and the guard reads **SURVIVED** — "not a
gate" — when it was simply never tested. It first showed up as the inverse (a
restored file still testing red), which is the lucky direction. `write_and_touch`
now bumps mtime forward on every write. A mutation harness must never report a
verdict on a build it cannot prove happened.
