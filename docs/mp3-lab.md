# The MP3 Encoder Lab — Repeatable Experiment Framework

A harness to **track every encoder brick** and **tune the experimental ones on
the fly**. It pairs with [mp3-encoder-plan.md](mp3-encoder-plan.md): the plan is
the prose; the lab is the running, queryable version of the same brick list plus
the machinery to experiment on each one.

It is **opt-in** behind the `lab` Cargo feature — zero weight on the default
build (the published `ffmpeg`/`ffprobe` binaries never compile it).

## Why a framework, not just unit tests

The encoder lives in **two regimes**, and the harness is built to keep them
separate:

| Regime | Bricks | Right answer? | How the lab scores it |
|---|---|---|---|
| **Conformance** | Foundation, analysis, coding (N*, L*, B*) | **Yes** — one correct output | bit-exact round-trip through our decoder → `max_abs_err == 0` |
| **Experimental** | the psychoacoustic brain (Q*, parts of C*/R*) | **No** — perceptual trade-offs | sweep variants, track PSNR / noise-to-mask over the corpus |

Conformance bricks get a pass/fail gate. Experimental bricks get a *measurement
you can compare across variants and over time*. Same backbone for both:
deterministic corpus, named variants, repeatable reports.

## The pieces

All under [`crates/rff-codec-mp3/src/lab/`](../crates/rff-codec-mp3/src/lab/):

- **`bricks.rs`** — the canonical manifest of all 35 bricks (id, phase, class,
  verification regime, status). The slice order *is* the build order, so
  `next_unbuilt()` always points at what to lay next. It's typed code, so it
  can't drift from the build the way a side doc would.
- **`signals.rs`** — a deterministic corpus (tones, two-tone masking, sweep,
  white noise, a transient torture case, DC). Generated from formulas + a seeded
  LCG: **no binary fixtures, no `rand`, no wall-clock** → identical on every
  machine and every run.
- **`metrics.rs`** — scores output vs. a reference: `max_abs_err` (0 ⇒ bit-exact),
  `rmse`, `psnr_db`.
- **`variant.rs`** — named parameter presets per brick. The "modify on the fly"
  knob: add a row to a brick's variant table to test a new setting.
- **`quantizer.rs`** — brick **N4**, the seed experiment, fully runnable today.
- **`experiment.rs`** — picks a brick + variant, runs the corpus, returns a
  **repeatable** `Report` (same inputs ⇒ identical numbers, byte for byte).

## Using it

```sh
cd crates/rff-codec-mp3
F="--features lab --example mp3lab"

cargo run -q $F -- bricks            # status table of every brick + the tally
cargo run -q $F -- next              # the next brick to build, in order
cargo run -q $F -- corpus            # list the test signals
cargo run -q $F -- variants N4       # a brick's variants
cargo run -q $F -- run N4 iso        # run an experiment → console + JSON log

# Override a parameter on the fly — no recompile, no code edit:
cargo run -q $F -- run N4 naive --bias 0.0 --step 0.0008
```

`run` writes `lab-results/<brick>-<variant>.json`. Because reports are
deterministic, those files **diff cleanly** — commit them to watch a metric move
as you tune, or regenerate them at will.

Example `run N4 iso` output:

```
experiment: N4 / iso
params: step=0.001 bias=0.0946 max_level=8191

  tone-1000hz        maxerr=0.003727  rmse=0.001694  psnr=55.42 dB
  ...
  ── mean ──         maxerr=0.003992  rmse=0.001354  psnr=57.37 dB
```

## Adding a brick to the lab (the standard ceremony)

When you implement a real brick, plug it in the same way N4 is wired:

1. **Implement** the brick (in `encode/…`, the production path).
2. **Variant table** — in a lab module, declare
   `static VARIANTS: &[Preset<YourCfg>]` of the settings worth comparing.
3. **`eval`** — a function `(YourCfg, &Signal) -> Metrics` that runs the brick and
   scores it. For conformance bricks this is the round-trip through the decoder
   (expect `max_abs_err == 0`); for quality bricks it's PSNR / NMR.
4. **Dispatch** — add a `match` arm in `experiment::run` keyed on the brick id.
5. **Status** — flip the brick's `Status` in `bricks.rs` (`Todo → Impl →
   Verified`).

That's the whole loop. The corpus, metrics, reporting, repeatability, and CLI are
already there; a new brick only supplies its variants + how to score it.

## Repeatability guarantees

- **Deterministic corpus** — no randomness that isn't seeded, no clock, no files.
- **Pure reports** — `run(brick, variant, overrides)` is a function of its inputs;
  a test (`quantizer_experiment_is_repeatable`) asserts byte-identical JSON across
  runs.
- **Graceful on unbuilt bricks** — running a `todo!()` brick returns an error
  naming the next buildable brick, never a panic.
- **Isolated** — the whole harness is behind `--features lab`; it cannot affect
  the shipped binaries or pull a single extra dependency (`cargo-deny` stays
  green).

## Status today

`cargo run -q --features lab --example mp3lab -- bricks`:
**35 bricks — 3 verified ✓ (N1–N3, reused from decode) · 1 stub ◐ (N4) · 31
todo**. Next to build: **N4**, then the Foundation generators (N5–N7) and Floor 1.
