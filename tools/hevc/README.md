# tools/hevc — the HEVC conformance harness and the north-star bench

Phase-0 instruments of [docs/plans/rusty_hevc.md](../../docs/plans/rusty_hevc.md).
Everything here is decoder-agnostic on purpose: it scored the off-the-shelf
Rust candidates before a line of `rusty_hevc` existed, and it will score
`rusty_hevc` the same way.

## Corpus (H0.1)

```sh
bash scripts/fetch-hevc-vectors.sh            # all 147 JCT-VC HEVC_v1 streams, ~130 MB
bash scripts/fetch-hevc-vectors.sh hevc-vectors IPRED_A_docomo_2 WPP_A_ericsson_MAIN_2
```

Source: `https://www.itu.int/wftp3/av-arch/jctvc-site/bitstream_exchange/draft_conformance/HEVC_v1/`.
Flat layout in `hevc-vectors/` (gitignored): `<name>.bit`, `<name>.yuv.md5`
(bare 32-hex of the whole decoded YUV, output order, u8 / u16-LE), `<name>.txt`,
`<name>.sha256`. The yuv / trace payloads inside the zips are never extracted.

## Harness (H0.2)

```sh
python tools/hevc/conform.py --decoder ffmpeg --self-test        # run this FIRST
python tools/hevc/conform.py --decoder ffmpeg --json hevc-vectors/results/ffmpeg.json
python tools/hevc/conform.py --decoder "../_hevc_score/target/release/_hevc_score.exe rust_h265" \
    --json hevc-vectors/results/rust_h265.json --ref-json hevc-vectors/results/ffmpeg.json
```

Two verdicts per stream:

- **md5** — MD5 of the decoder's whole YUV output vs the published md5. The
  primary gate; order-sensitive (output order must be right too).
- **sei** — the decoded-picture-hash SEI (payload 132; MD5 / CRC / checksum per
  plane, parsed straight out of the bitstream) vs the same hash of every output
  picture. Every output picture must match one declared hash; the SEI set may be
  larger (pictures with `pic_output_flag = 0`, dropped RASL pictures). Skipped
  (`n/a`) when the stream is cropped — the SEI covers the full coded picture and
  external decoders emit the cropped one — or carries no hash SEI.

`--self-test` decodes a known-good stream (must pass) and a byte-corrupted copy
of it (must fail) — a harness that cannot fail is measuring nothing. It caught
its own first two bugs (ffmpeg needs `-f hevc` for a `.bit` file; `-fps_mode`
must follow `-i`).

A decoder plugs in as any command taking `<in.bit> <out.yuv>` and writing
planar 4:2:0, 8-bit as `u8`, >8-bit as `u16` little-endian, in output order.
`../_hevc_score` (a scratch crate outside the workspace) wraps `rust_h265` and
`hpvcd` that way.

## North-star bench

```powershell
powershell -File tools/hevc/pinbench.ps1 -Stream hevc-vectors/bench/in_to_tree_720p_8bit.hevc -Rounds 7
```

The **C/C++ north star is ffmpeg's native `hevc` decoder** (`libavcodec/hevc`,
`-threads 1`): it is the decoder users actually run, it is faster than the HM
reference, and it is present on every machine we bench on. HM (BSD-3) is the
*conformance* reference and the source we transcribe from, not a speed target.

Bench streams (`hevc-vectors/bench/`, regenerable): the first 60 frames of
Derf's `in_to_tree_420_720p50` (real content, slow dolly into foliage) encoded
with `libx265 -preset medium -crf 24`, once 8-bit and once `-pix_fmt yuv420p10le`:

```sh
ffmpeg -i in_to_tree_720p50_60f.y4m -c:v libx265 -preset medium -crf 24 \
       -x265-params log-level=error:frame-threads=1 -f hevc in_to_tree_720p_8bit.hevc
```

Method (codec-measurement): each run pinned to one core at High priority, **CPU
time** not wall, arms ABBA-interleaved, best-of-N and median, a null arm
(reference vs itself) for the noise floor. The Rust arms include writing the
YUV to disk (ffmpeg's arm discards to `-f null`), a fixed per-run cost that
flatters ffmpeg slightly; subtract it with a null-write arm before quoting a
sub-10 % difference.

### Standing (2026-09-05, i7-14650HX, ffmpeg 8.1.2)

After the kernel campaign (`docs/plans/rusty_hevc_kernels.md`); every arm below
was re-measured in one session at 9 rounds, so the ratios share one denominator.

| arm | 8-bit 720p best ms | Mpx/s | × ref | 10-bit 720p best ms | Mpx/s | × ref |
|---|---:|---:|---:|---:|---:|---:|
| **ffmpeg hevc, 1 thread (north star)** | 297 | **186** | 1.00 | 297 | **186** | 1.00 |
| ffmpeg hevc, null arm | 297 | 186 | 1.00 | 313 | 177 | 1.05 |
| **`rusty_h265` (ours, SIMD kernels)** | **1,016** | **54** | **3.42** | **1,141** | **48** | **3.84** |
| `rust_h265` 0.1.0 (our scalar origin) | 2,078 | 27 | 7.00 | 1,891 | 29 | 6.37 |
| `hpvcd` 0.3.2, 1 thread (SSE/AVX on) | 1,656 | 33 | 5.58 | 1,563 | 35 | 5.26 |

60 frames = 55.3 Mpx per arm. The 10-bit null arm read 1.05×, so treat ~5 % as
that column's floor; the 8-bit null arm's best equalled the reference's, below
the timer's 1 ms resolution at best-of-9. Work-count parity is printed per run
(`frames=60 errors=0 pictures=60 slices=60` on every round).

Two things this table says that the per-brick numbers do not:

- **We are now 1.63× (8-bit) / 1.37× (10-bit) faster than `hpvcd`**, the mature
  C++ decoder with its own SSE/AVX kernels — having started out *slower* than it
  on both. That comparison is what made forking `hpvcd` the alternative plan.
- **The gap to ffmpeg is ~3.4–3.8×, down from ~7×.** ffmpeg's hevc decoder is
  hand-written assembly across essentially every stage, so what is left is the
  distance from good intrinsics to good asm, not a missing kernel.

Conformance results for the same three decoders: `hevc-vectors/results/*.txt`.


## Conformance verdict (H0.4, 2026-09-05 — JCT-VC HEVC_v1, 147 streams, whole-YUV md5)

```sh
python tools/hevc/verdict.py --ref hevc-vectors/results/ffmpeg.json,hevc-vectors/results/ffmpeg_extra.json     hevc-vectors/results/rust_h265.json hevc-vectors/results/rust_h265_extra.json     hevc-vectors/results/hpvcd.json hevc-vectors/results/hpvcd_extra.json --markdown
```

| class | ffmpeg | rust_h265 | hpvcd |
|---|---:|---:|---:|
| AMP | 5/5 | 5/5 | 5/5 |
| AMVP | 3/3 | 1/3 | 3/3 |
| BUMPING | 1/1 | 0/1 | 1/1 |
| CAINIT | 8/8 | 6/8 | 8/8 |
| CIP | 3/3 | 0/3 | 3/3 |
| CONFWIN | 1/1 | 0/1 | 1/1 |
| DBLK | 8/8 | 0/8 | 8/8 |
| DELTAQP | 3/3 | 0/3 | 3/3 |
| DSLICE | 3/3 | 1/3 | 3/3 |
| ENTP | 3/3 | 1/3 | 3/3 |
| EXT | 1/1 | 0/1 | 1/1 |
| FILLER | 1/1 | 0/1 | 1/1 |
| HRD | 1/1 | 0/1 | 1/1 |
| INITQP | 2/2 | 0/2 | 2/2 |
| IPCM | 5/5 | 3/5 | 5/5 |
| IPRED | 3/3 | 3/3 | 3/3 |
| LS | 2/2 | 0/2 | 2/2 |
| LTRPSPS | 1/1 | 0/1 | 1/1 |
| MAXBINS | 3/3 | 3/3 | 3/3 |
| MERGE | 7/7 | 1/7 | 7/7 |
| MVCLIP | 1/1 | 0/1 | 1/1 |
| MVDL1ZERO | 1/1 | 0/1 | 1/1 |
| MVEDGE | 1/1 | 0/1 | 1/1 |
| NOOUTPRIOR | 2/2 | 0/2 | 2/2 |
| NUT | 0/1 | 0/1 | 1/1 |
| OPFLAG | 3/3 | 0/3 | 3/3 |
| PICSIZE | 4/4 | 0/4 | 4/4 |
| PMERGE | 5/5 | 0/5 | 5/5 |
| POC | 1/1 | 1/1 | 1/1 |
| PPS | 1/1 | 0/1 | 1/1 |
| PS | 1/1 | 0/1 | 1/1 |
| RAP | 2/2 | 0/2 | 2/2 |
| RPLM | 2/2 | 0/2 | 2/2 |
| RPS | 5/6 | 0/6 | 6/6 |
| RQT | 7/7 | 7/7 | 7/7 |
| SAO | 8/8 | 4/8 | 8/8 |
| SAODBLK | 0/2 | 0/2 | 2/2 |
| SDH | 1/1 | 0/1 | 1/1 |
| SLICES | 1/1 | 0/1 | 1/1 |
| SLIST | 4/4 | 2/4 | 4/4 |
| SLPPLP | 1/1 | 0/1 | 1/1 |
| STRUCT | 2/2 | 2/2 | 2/2 |
| TILES | 2/2 | 0/2 | 2/2 |
| TMVP | 1/1 | 0/1 | 1/1 |
| TSCL | 2/2 | 0/2 | 2/2 |
| TSKIP | 1/1 | 1/1 | 1/1 |
| TSUNEQBD | 0/1 | 0/1 | 1/1 |
| TUSIZE | 1/1 | 1/1 | 1/1 |
| VPSID | 1/1 | 0/1 | 1/1 |
| VPSSPSPPS | 0/1 | 0/1 | 1/1 |
| WP | 4/4 | 0/4 | 4/4 |
| WPP | 12/12 | 0/12 | 12/12 |
| **TOTAL md5** | **141/147 (95.9%)** | **42/147 (28.6%)** | **147/147 (100.0%)** |

```
rust_h265: output byte-identical to ffmpeg on 42/147; 105 md5 failures = 25 no output + 48 with decode errors + 32 silent mismatches; SEI per-picture pass 29/81

hpvcd: output byte-identical to ffmpeg on 141/147; 0 md5 failures = 0 no output + 0 with decode errors + 0 silent mismatches; SEI per-picture pass 100/100

ffmpeg (reference) fails 6: NUT_A_ericsson_5, RPS_D_ericsson_6, SAODBLK_A_MainConcept_4, SAODBLK_B_MainConcept_4, TSUNEQBD_A_MAIN10_Technicolor_2, VPSSPSPPS_A_MainConcept_1
```

ffmpeg's six failures are its own documented deviations, not harness faults
(hpvcd matches the published md5 on all six): it drops pictures on reserved
NAL types (`NUT_A`) and missing references (`RPS_D`), refuses unequal
luma/chroma bit depth (`TSUNEQBD`), and differs on non-rectangular-slice
deblocking/SAO and parameter-set re-activation (`SAODBLK_A/B`,
`VPSSPSPPS_A`). **The north star is a speed reference, not a 100 % conformance
oracle** — the published md5 plus the SEI hash are.

Verdict per the plan's rule: **fork `hpvcd`** (100 %, BSD-3 OR Apache-2.0,
zero dependencies); `rust_h265` is not a base. Details and what the fork
changes in the plan: `docs/plans/rusty_hevc.md` §8 Q2 and §9.

## Our decoder (`crates/rusty_h265`)

```sh
cargo build --release -p rusty_h265
python tools/hevc/conform.py --decoder "$PWD/target/release/rusty_h265.exe"     --json hevc-vectors/results/rusty_h265.json --ref-json hevc-vectors/results/ffmpeg.json
cargo test -p rusty_h265 --release          # the same gate, in process
```

**Standing verdict (2026-09-05): `TOTAL md5 147/147 (100.0 %)  sei 100/100`** — every
class green, including the six streams ffmpeg 8.1.2 fails (`NUT_A`, `RPS_D`,
`SAODBLK_A/B`, `TSUNEQBD`, `VPSSPSPPS_A`).

| decoder | md5 | SEI picture hashes |
|---|---:|---:|
| **rusty_h265 (ours)** | **147/147** | **100/100** |
| hpvcd 0.3.2 | 147/147 | 100/100 |
| ffmpeg 8.1.2 `hevc` | 141/147 | 94/100 |
| rust_h265 0.1.0 | 42/147 | 10/100 |

### Speed (H6.3 shape, not yet optimised)

`powershell -File tools/hevc/pinbench.ps1 -Stream hevc-vectors/bench/in_to_tree_720p_8bit.hevc -Rounds 5`,
2026-09-05, i7-14650HX, 720p 60 frames, pinned core, CPU time, ABBA:

| arm | best ms | Mpx/s | × ref |
|---|---:|---:|---:|
| ffmpeg `hevc` 1T (north star) | 344 | 161 | 1.00 |
| ffmpeg 1T (null arm) | 359 | 154 | 1.05 |
| **rusty_h265 (ours)** | **4,172** | **13** | **12.1** |
| rust_h265 0.1.0 | 2,313 | 24 | 6.7 |
| hpvcd 0.3.2, 1 thread | 2,156 | 26 | 6.3 |

That is the honest starting point of a conformance-first decoder: every plane
is `u16` even for 8-bit content, every prediction unit allocates its
interpolation buffers, and there is not one SIMD kernel. Phase 6 of the plan
(after the fuzz work) is where those get measured and fixed — under
`codec-measurement` rules, with the scalar path kept as the oracle.

### The HM oracle

`tools/hevc/hm-oracle.patch` applies to a fresh clone of the JCT-VC HM
(`git clone https://vcgit.hhi.fraunhofer.de/jvet/HM.git ../_ref_hm`) and adds
env-gated dumps that made every reconstruction desync a one-line diff:

| variable | prints |
|---|---|
| `HM_DBG_BS` | per-edge boundary strength, β/tC, the d/dp/dq decision and the p/q line samples |
| `HM_DBG_BS2` | the inputs to the boundary-strength rule (transform-edge flag, intra flags, cbfs) |
| `HM_DBG_CU` | one line per coding unit: position, size, QP, part mode, intra mode or MV/refidx |
| `HM_NO_DBK`, `HM_NO_SAO` | skip deblocking / SAO, to isolate the stage |

Our decoder mirrors them: `RH265_DBG_CU` (identical format — diff the two
files), `RH265_DBG_SUB` (substream entry points vs the natural byte position),
`RH265_TRACE` (pictures, slices, DPB output), `RH265_NO_LF`, `RH265_NO_SAO`.
Build HM with clang: `cmake -S .. -B . -G Ninja -DCMAKE_BUILD_TYPE=Release
-DCMAKE_CXX_FLAGS=-w` (clang 22 rejects one `|` on bools in `TComDataCU.h`;
the patch fixes it).
