# AAC quality plan — closing the four measured gaps

Target: the four content bands where the 0.4.0 measurement puts us behind
`ffmpeg -c:a aac`. Every number below is from the published PEAQ run
(`tools/quality/aac_vs_ffmpeg.py`, FFmpeg 8.1.2, null-arm ceiling +0.14).

| band | measured Δ (ours − ffmpeg, ODG) | status |
|---|---|---|
| **Percussive** | −1.18 / −1.73 / −1.96 / −2.22 (64/96/128/192k) | worst gap; **worsens with bitrate** |
| **Stereo** | piano −1.28, guitar −0.87 (mean over 4 rates) | behind at every rate |
| **Mono music, ≤96k** | −1.29 / −1.02 / −1.92 at 64k | behind low, **ahead ≥128k** |
| **Tonal** | −0.03 | parity — the cheapest band to win |

Two structural notes before any of it:

- **Percussive gets *worse* as bitrate rises** (−1.18 → −2.22). That is not a
  bit-shortage signature; extra bits are being spent somewhere they do not help.
  It points at a *tool* gap (pre-echo control), not an allocation gap.
- **Mono music inverts at ~110–120 kbps.** Above it we reach transparency and
  beat ffmpeg. So the machinery is sound and the low-rate loss is specifically an
  allocation problem, not a quantizer-quality problem.

---

## Phase 0 — Re-measure what is ALREADY BUILT, under PEAQ (do this first)

**Highest value per unit of effort in the entire plan, and it needs no new
encoder code.** Six arms are implemented, tested, and off by default. All were
judged on NMR — our own psy model — and NMR is provably unable to judge two of
them:

| arm | built? | NMR verdict | why NMR may be wrong |
|---|---|---|---|
| A9 level-invariant transient detector | ✅ | −6.10 on percussive @128k | NMR-valid, but never PEAQ-confirmed |
| A3 TNS | ✅ | +13.3 (looks terrible) | **NMR is blind to it** — TNS shapes noise *in time*; NMR has no intra-frame time resolution |
| A6 PNS | ✅ | +0.19 (looks bad) | **NMR is blind to it** — PNS randomizes *phase* at matched energy; NMR compares coefficients directly |
| A7 intensity stereo | ✅ | −0.006 mean, worst **+0.000** | only arm to pass worst-class; measured on one stereo clip |
| A2 tonality SMR | ✅ | −0.312 (best mean) | NMR-valid; needs per-class dispatch |
| A1 short-block psy | ✅ | refuted (needs grouping) | NMR-valid; refutation stands |

**Action:** extend `tools/quality/aac_vs_ffmpeg.py` to accept an arm
configuration and run the same 13-clip × 4-bitrate sweep per arm, scored by PEAQ.
This is a small change — the harness already encodes through the CLI, so it needs
CLI flags for the arm knobs plus a loop.

**Why this comes first:** A3 and A6 are *exactly* the tools that address two of
the four target bands (TNS → percussive, PNS → noisy/low-rate). Both are built.
Both currently look bad on a metric that cannot see their benefit. It is entirely
possible that two of the four gaps close with flags that already exist. Writing
new code before running this risks solving a problem twice.

**Gate:** per-class PEAQ table per arm, worst class ≤ 0, plus the calculator
harvest (`aacharvest`) with a **pinned** clock — the current harvest's null arm
is 7.6% median, which cannot resolve any of these.

---

## Band 1 — Percussive (−1.18 → −2.22, worst and widening)

### Root causes, in confidence order

1. **No TNS in the shipping path (A3 — BUILT, default-off).** TNS is *the* AAC
   pre-echo tool and the main reason AAC beats MP3 on transients. Its encoder
   side exists and its filter inversion is verified exact
   (`a3_tns_inverts_exactly`). ffmpeg ships TNS on by default. **This is the
   single most likely explanation for the whole gap**, and it explains the
   widening-with-bitrate shape: more bits spent on a smeared representation do not
   fix smear.
2. **The transient detector is off (A9 — BUILT, default-off).** The shipping
   detector's `avg > 1e-3` guard is absolute, so it flags the percussive class
   **zero times at any level**. Those frames code as long blocks and take the full
   pre-echo hit. A9 measured **−6.10 NMR on percussive @128k**.
3. **Short blocks still code with flat scalefactors (A1 + arm 1a).** A1 alone was
   *refuted*: with one window group a scalefactor spans all eight windows, so the
   mask is built from their sum and hands quiet pre-attack windows an
   attack-sized mask. **Window grouping (arm 1a) is a hard prerequisite**, not a
   follow-up.

### Work, in order

1. PEAQ-measure **A9** and **A3** (Phase 0). Bank whichever pass.
2. If they do not close it: build **arm 1a, window grouping** — real
   `window_group_length`, per-group sections/scalefactors/spectrum in the emit
   path. Then re-enable A1 on top and re-measure.
3. Only then consider TNS on short blocks (per-window filters).

**Gate:** percussive class ≤ 0 vs ffmpeg at all four rates, and the *shape*
inverted — the delta should improve with bitrate, not worsen.

---

## Band 2 — Stereo (−0.87 to −1.28, behind everywhere)

### Root causes

1. **★ The bit budget is split evenly by channel count.**
   [`encode.rs:2968`](../crates/rusty_aac/src/encode.rs#L2968):
   ```rust
   let per_channel = (frame_budget / self.channels.max(1)).saturating_sub(7);
   ```
   Mid and side receive **identical budgets regardless of content**. On correlated
   stereo the side channel carries a small fraction of the energy and needs a small
   fraction of the bits; giving it half starves mid, which is what the listener
   mostly hears. This is a confirmed code-level defect, it is cheap to fix, and it
   plausibly accounts for much of the gap on its own.
   **Fix:** allocate between the two channels by perceptual demand (their
   perceptual-entropy ratio, which `AacSignals::pe` already computes), with a
   floor so side never starves entirely.
2. **Intensity stereo is off (A7 — BUILT, passes worst-class).** Already
   implemented as a genuine 3-arm dispatch `{L/R, M/S, IS}` that decides before
   M/S and vetoes it. ffmpeg ships intensity stereo. Bank it in Phase 0.
3. **The M/S decision has no rate or perceptual term (A10).** Currently
   `E_M·E_S < E_L·E_R` — pure energy. No bit cost, no binaural masking level
   difference. It cannot know that M/S which saves no bits is not worth taking.

### Work, in order

1. **Fix the channel bit split** (highest confidence, smallest change).
2. Bank **A7** on PEAQ (Phase 0).
3. Add the rate term to the **A10** M/S objective: choose M/S only when it
   measurably reduces coded bits at equal distortion, not merely when the energy
   product says so.

**Gate:** stereo classes ≤ 0 at all four rates, on both real stereo clips *and*
the synthetic wide-stereo class (the anti-class for A7 — image collapse must not
be the price of the win).

---

## Band 3 — Mono music at ≤96 kbps (−1.0 to −1.9 at 64k)

We already **win** here at ≥128k, so this is narrowly an allocation problem.

### Evidence

- **Rate undershoot is real but insufficient as an explanation.** At the 64k
  target we emit 61.0 kbps and ffmpeg emits 68.6 — a ~12% bit advantage to
  ffmpeg. That does not by itself buy 1.3 ODG.
- **REFUTED: bandwidth is not the lever.** I expected ffmpeg to band-limit harder
  at low rates. Measured average spectra say otherwise — at 64k the 99.5% rolloff
  is 14.9 kHz (ours) vs 14.3 kHz (ffmpeg) on guitar, and identical on piano and
  vocal. Ours is very slightly *wider*. **Do not build arm A11 for this band**;
  the hypothesis is dead.

### Root causes

1. **No bit reservoir, no VBR (A5).** The rate loop searches one global base
   against a **constant per-frame budget**. Frames that cannot use their budget
   waste it; frames that need more cannot borrow. At 64k that is precisely when it
   hurts. Note the threading constraint: the encoder is frame-parallel and a
   reservoir is sequential state — decide the model (serialize the rate stage vs
   chunk-local reservoir) *before* fitting anything.
2. **No outer distortion loop (A12).** Per-band offsets are computed once,
   analytically, and never iterated against the per-band threshold. ffmpeg's
   `aacenc` has a real two-loop. At high rates this barely matters (we win); at low
   rates, where the offsets must be right first time, it does.

### Work, in order

1. **A12 distortion loop** — iterate per-band scalefactors against per-band
   threshold inside the rate loop. Sequence it before A5: it changes the objective
   A5 would otherwise be fitted against.
2. **A5 reservoir**, PE-driven, with the threading model decided up front.

**Gate:** 64k and 96k classes ≤ 0 on the three real mono clips, **without
regressing the ≥128k win** — that win is currently our strongest public claim.

---

## Band 4 — Tonal (−0.03, at parity — the cheapest win)

### Root cause

**A2 tonality-adaptive SMR is built and is the best-scoring arm** (−0.312 mean on
NMR, with wins of −3.94 on wide stereo and −2.81 on quiet/dynamic). Its failures
are a clean **per-class sign flip** — wins tonal/stereo/quiet, loses
noisy/percussive — which is a dispatch signal, not a mean loss.

One refutation already banked: pinning the noise end of the SMR pair to the
shipped 18 dB, which *looks* strictly safer, measured **worse** (worst class
+0.72 → +2.19). An SMR pair is a bit-allocation balance, not two independent
protections — raising the tonal end only buys precision because lowering the
noise end frees the bits to pay for it. Keep the textbook 5.5 dB pair and route
by content class instead.

### Work

1. PEAQ-measure A2 (Phase 0).
2. Harvest with `aacharvest --arm a2` and fit the dispatch in the
   **gate-calculator** — a depth ≤ 3 tree on the content-signal vector
   (`tonality`, `pe`, `rolloff_hz`, `bitrate_kbps` are all already emitted).
3. Transcribe the winning branch in the house style; ledger it.

This band is also the designated **Prometheus symbolic-leaf** target: the branch
stays a calculator-discovered threshold tree, and the leaf becomes a discovered
closed-form `SMR = f(tonality, bark)` law.

**Gate:** tonal ≤ 0 *and* no new sign flip introduced on the noisy classes.

---

## Cross-cutting infrastructure (blocks banking, not building)

1. **★ Pin the clock.** The current harvest's timing null arm is **7.6% median /
   15.3% worst** — it cannot resolve any arm in this plan. Until a pinned-CPU run
   replaces that column, `--attest-full-stack` cannot honestly be passed and
   *nothing* is bankable by the calculator, however good the PEAQ number looks.
2. **Grow the real corpus.** Three real clips (piano, guitar, vocal) carry the
   headline; percussive and speech evidence is currently **synthetic only**, and
   `codec-measurement` §9 is explicit that synthetic content misdirects. Real
   percussive and real speech clips are needed before the two widest gaps can be
   called closed. `tools/quality/fetch_corpus.sh` is the place to add them.
3. **Keep NMR for the inner loop, PEAQ for verdicts.** NMR is valid for A1, A2,
   A7, A9 and is far faster; it is invalid for A3 and A6. Never bank on it.

---

## Sequence

```
Phase 0   PEAQ-measure the 6 built arms          <- no new code, may close 2 bands
          pin the clock (blocks all banking)
   |
   +-- Percussive : bank A9 + A3, else build grouping (1a) -> A1
   +-- Stereo     : fix the channel bit split, bank A7, then A10 rate term
   +-- Mono <=96k : A12 distortion loop, then A5 reservoir
   +-- Tonal      : fit the A2 dispatch in the calculator
   |
   ...re-run the full ffmpeg comparison; update the README standing.
```

**Expected order of payoff:** percussive (largest gap, tools already built) →
stereo (one confirmed code defect + one built arm) → tonal (cheap, at parity) →
mono low-rate (most genuine new engineering).

**The exit criterion is unchanged and per-class:** worst content band ≤ 0 versus
ffmpeg, verified per class at four operating points, never on an average.
