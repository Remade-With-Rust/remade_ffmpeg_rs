# `video-tests` — the fixed speed corpus and the function-level analyzer

A pinned set of **real** source video plus a harness that measures our in-house
video codecs against the reference implementation ffmpeg actually ships, function
by function, for both the encoder and the decoder. The point is repeatability:
the same pixels, the same settings, every run, so two measurements taken weeks
apart are comparable — and per-function attribution, so an optimisation is chosen
from data rather than a hunch.

**VP9 is live today.** AV1 follows the same three-pass shape.

## What "default settings" means here

Every arm runs at *its own* defaults, which is what the comparison is for:

| arm | encode | decode |
|---|---|---|
| `ours` | `rff-vp9` in-process through the public codec registry, no options set | same |
| `libvpx` | `vpx_codec_enc_config_default` + `--threads 1` | `vpx_codec_dec_init`, `--threads 1` |
| `ffmpeg` | `ffmpeg -i clip.y4m -c:v libvpx-vp9 -threads 1 out.ivf` | `ffmpeg -c:v vp9` (the native ffvp9 decoder) |

Only `--threads 1` is imposed, on every arm, because per-function attribution
against a single-threaded codec core is meaningless otherwise.

Note that `libvpx` and `ffmpeg` produce *different* bitstreams from the same
source: ffmpeg's wrapper resolves its own defaults (bitrate, GOP length) on top
of libvpx's. Both are reported. Neither is wrong; they are two different
front-ends' idea of "default".

## Layout

```
video-tests/
  manifest.tsv        # name, dims, fps, frame count, class, byte size, hash
  fetch_clips.sh      # reproducible corpus fetch (HTTP range, not full downloads)
  clips/              # optional local copy; by default the rs_h264 corpus is reused
  analyzer/           # the Rust harness (own workspace, pure Rust, path deps)
  run_analysis.sh     # drives the three passes end to end
  results/            # speed.tsv, stages.tsv, REPORT.md (gitignored)
```

## The corpus

20 clips, five resolution rungs x seven content classes, from Xiph's Derf
collection (the standard codec-research corpus).

| rung | clips | frames |
|---|---|---|
| QCIF 176x144 | akiyo, foreman | 120 |
| CIF 352x288 | akiyo, foreman, mobile, bus, tempete, football | 120 |
| 4CIF 704x576 | city, crew, harbour, soccer | 120 |
| 720p 1280x720 | shields, stockholm, in_to_tree, FourPeople | 60 |
| 1080p 1920x1080 | ducks_take_off, park_joy, crowd_run, blue_sky | 60 |

Classes: `static` (skip/entropy dominated) · `medium` · `pan` (motion-search
heavy) · `detail` (residual/transform heavy) · `complex` · `fastmotion` (worst
case for ME) · `smooth`.

**The pixels are shared with the `rs_h264` checkout** (`../../rs_h264/video-tests/clips`)
rather than duplicated. That is deliberate: VP9, AV1 and H.264 numbers are then
taken on byte-identical input and can be compared across repositories. Set
`CLIPS_DIR` to point elsewhere, or run `fetch_clips.sh` to make a local copy.

By default each clip is capped at **30 frames** (`FRAMES=N`, `FRAMES=0` for whole
clips) — enough to cross every resolution rung in minutes rather than hours.

## The libvpx reference

Built from source **outside this repo** (`../_ref_libvpx`) and driven as external
processes. Nothing here compiles C or C++; the codec crates stay pure Rust.

```sh
cd ../_ref_libvpx
python instrument.py     # plant the rdtsc stage taps (idempotent)
bash build.sh            # vp9enc.exe   + vp9dec.exe        (STOCK — throughput arm)
bash build.sh prof       # vp9enc-prof.exe + vp9dec-prof.exe (TAPPED — breakdown arm)
```

`build.sh` stands in for `make` (not installed on this machine) and mirrors
libvpx's own object lists and per-ISA pattern rules for the configured target:
`x86_64-win64-gcc`, SIMD through AVX2, runtime CPU detect, single-threaded,
8-bit only.

**Two binaries on purpose.** Measuring throughput on the instrumented build would
tax libvpx with overhead our own profiler-off runs don't pay.

**Why a local libvpx at all, when ffmpeg is right there.** ffmpeg's shipped binary
is fully stripped, so it yields a total and nothing else. This build is the same
library with symbols and taps. It earns the right to stand in for ffmpeg's copy:
at matched settings it produces a **byte-identical bitstream** (114,487 bytes on
`foreman_cif`, both) at **matching speed** (3.97 s encode-loop vs ffmpeg's 4.44 s
process wall, which includes ~0.4 s of startup and y4m demux). Its milliseconds
are therefore ffmpeg's milliseconds, and unlike ffmpeg's it can be attributed.

## Running the analysis

```sh
bash video-tests/run_analysis.sh                          # whole corpus
CLIPS=foreman_cif,mobile_cif bash video-tests/run_analysis.sh   # a subset
FRAMES=0 bash video-tests/run_analysis.sh                 # whole clips
```

Three passes, because throughput and per-function breakdown cannot come from the
same run — the rdtsc scopes inflate wall time on both sides:

1. `speed` (profilers **off**) → `results/speed.tsv`
2. `stages` (profilers **on**) → `results/stages.tsv`
3. `report` (merge) → `results/REPORT.md`

Unlike the rs_h264 harness, our side needs only **one** build: both Rust
profilers are runtime-gated (`prof::set_enabled`), so the speed pass leaves the
taps costing a single predictable relaxed load and the stages pass turns them on.

## Reading the numbers honestly

* **Exclusive vs inclusive.** Our decoder's profiler and both libvpx profilers are
  *exclusive self-time*: a stack charges every cycle to exactly one function, so
  the buckets sum to the measured wall and the percentages ARE "share of
  process". Our *encoder's* profiler predates this and is nested-*inclusive*, so
  the report emits parents twice — once as measured (`incl`) and once as
  `name(self)` with the scoped children subtracted. **The `self` rows are the ones
  that pair with the reference.**
* **The residue is a bucket, not a rounding error.** `orchestration/glue` (encoder)
  and `other/glue` (decoder) are real unscoped work. Our decoder charges glue only
  *inside* a frame decode — time between frames goes to a discarded sink, so the
  residue can't quietly absorb the caller's own loop.
* **The profiler measures itself.** Every scope costs ~2 `rdtsc` reads, and the
  encode path opens hundreds of thousands to millions of them. Both sides subtract
  the per-scope latency from each bucket, and report `_INSTRUMENT_TAX` — scope
  count times the cost measured on *this* machine — next to the residue. When the
  residue matches the tax, there is no hidden work left, only the instrument.
* **Aggregation is by sum of milliseconds**, never by averaging per-clip
  percentages, which would weight a 25 ms QCIF encode the same as a 30 s 1080p one.
* **Timing is best-of-N; stage tables are median-of-N.** The fastest pass is the
  least disturbed by the scheduler, but taking the minimum of each stage
  independently would produce a table summing to less than any real pass took.
* **External binaries are timed by their own reported codec-loop time**, not
  process wall clock. Startup is 10–20 ms here, which would swamp a 25 ms QCIF
  encode. ffmpeg, which reports no such figure, is timed net of a measured
  do-nothing invocation.
* **Quality is computed in-process**, frame index against frame index, after
  decoding both encoders' output with the same external ffmpeg. ffmpeg's own
  `psnr`/`ssim` filters pair by *timestamp*, and a container time base that
  disagrees with the source frame rate misaligns the streams and reports a large,
  entirely fictitious quality loss.
* **`adapt_probs` reads zero on libvpx streams.** libvpx defaults to
  `frame_parallel_decoding_mode = 1`, which disables backward adaptation. That is
  the encoder's default, not a missing tap.
* **AVX-512 is compiled but never dispatched.** This CPU (Raptor Lake) has it fused
  off, so the runtime detector selects AVX2 — exactly as it does inside ffmpeg's
  libvpx.
