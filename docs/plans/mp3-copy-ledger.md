# MP3 memory-copy ledger

`codec-memory-copies` over both CLI MP3 paths (WAV→MP3 encode, MP3→WAV decode),
2026-10-04, branch `mp3-memory-copies`. Every copy and per-frame allocation found
between the container bytes and the codec, what was done about it, and the
evidence. Workload: a 412 s stereo 44.1 kHz track (72.8 MB WAV, 9.9 MB MP3 @192k).

## Step 0 — is it the plumbing?

`RFF_PHASES=1` (wall time per transcode phase) against the core profilers on the
same input, before any change:

| path | CLI | core codec | gap |
|---|---:|---:|---:|
| WAV→MP3 | encode phase 4.24–4.87 s | encprof stages 3.40 s | ~0.8 s around the encoder |
| MP3→WAV | decode phase 0.53–0.63 s | decprof stages 0.42 s | ~0.1 s in the adapter |
| MP3→WAV | conform 86–103 ms, mux 54 ms (to a pipe) | — | all plumbing |

(The mux phase read 0.6–1.5 s writing the WAV to disk; to a pipe it is 54 ms, so
that was I/O on a 99%-full drive, not copies.)

## The list

Ranked by bytes moved at the real scale. **Resolved** = removed and proven
byte-identical; evidence is the deterministic counter where the effect is below the
clock's floor, and a pinned ABBA A/B (`tools/bench/pinab.py`, null arm first) where
it is above.

| # | site | copy | resolution | evidence |
|---|---|---|---|---|
| E1 | adapter `send_frame` → `push_pcm_s16`/`f32` | plane bytes → `Vec<i16>`/`Vec<f32>` → `Vec<f32>` (collect) → per-sample `push` into a whole-file per-channel buffer, no reserve | **resolved** (4445ad7): `push_pcm_s16le`/`f32le` take the bytes; one pass converts + deinterleaves into a one-frame staging buffer, encoding as each frame fills | whole-push: 110.9 → 42.1 MB requested, 33.1 → 0.3 MB carried by realloc, peak live +52.2 → +3.9 MB |
| E2 | `drain_frames` | each frame copied into a fresh `Vec<Vec<f32>>` | **resolved** (4445ad7): the encoder borrows the frame (`&[impl AsRef<[f32]>]`); only lookahead copies | 12.03 → 9.02 allocs/frame |
| E3 | WAV demux | whole file read, then `raw.to_vec()` of the data chunk (2× the file alive) | **resolved** (eb7f6a0): 64 KiB prefix read, data read straight into its own `Vec`; odd layouts fall back to an in-place truncate+drain | open phase 1.473× (72 MB s16) / 1.542× (145 MB f32), z=+3.50 / +4.00 |
| D1 | `conform_sample_format` | plane → `Vec<f32>` → `flat_map(to_le_bytes).collect()` | **resolved** (dc9f1c3): bytes→bytes in one sized pass; `f32_frame` likewise | whole-file f32→s16 1.38× / s16→f32 2.20× (12/12); MP3 9 KB frames: flat |
| D2 | decode adapter `receive_frame` | per-sample `extend_from_slice` (capacity check + length spill each sample) | **resolved** (dc9f1c3): exact-length plane, sized loop | 11 → 5 instructions/sample (.s); clock inadmissible |
| D3 | WAV mux trailer | whole output copied into `body` before one `write_all` | **resolved** (dc9f1c3): head built, samples written from the buffer | mux phase 1.457× / 1.501×, 15/16, z=+3.50 |
| D4 | `decode_frame_entropy` | per-frame `Vec<GranuleWork>`, every 4.6 KB spectrum moved in and out | **resolved** (bb68f0f): granules handed to the transform by reference; only the pipelined decoder collects | 6.02 → 1.02 decode allocs/frame (with D5, D6) |
| D5 | `Reservoir::assemble` | new `Vec` per frame + `rev().take(512).rev().collect()` | **resolved** (bb68f0f): one reused buffer, in-place slide | (counter above) |
| D6 | `parse_frames` | side info + main data `to_vec`'d per frame | **resolved** (bb68f0f): slices lent | (counter above) |
| E5 | `analyze_frame` | `freqs: Vec<[f32;576]>` per granule, then copied again into `analyzed` | **resolved** (f70c629): MDCT writes into the `analyzed` slot, M/S in place | 9.02 → 4.02 allocs/frame, 42.1 → 22.9 MB |
| E6 | `analyze_frame` | `attacks` / block-type `Vec`s | **resolved** (f70c629): fixed arrays | (counter above) |
| E7 | `decide_stereo` | `env::var("MP3_STEREO")` every frame | **resolved** (f70c629): read once | found by `ALLOCAUDIT_TRACE=22` |
| E8 | `Mp3Encoder::finish` | reservoir stream cut back into packets with `to_vec` apiece | **resolved** (f70c629) for the default causal path: `assemble_frames` | (counter above) |

End to end, base → final, rff CLI totals: MP3→WAV **1.016× min / 1.074× median,
15/16, z=+3.50**; WAV→MP3 **1.040× / 1.057×, 9/10, z=+2.53** (conservative: the
final binary's total also includes the `open` phase the base's total omits).

## Kept, with the reason

| site | why it stays |
|---|---|
| MP3 demux `frame.to_vec()` per packet | `Packet` owns its bytes; 9.9 MB total |
| `Mp3Decoder::push` → `buf.extend_from_slice` | frames may be split across packets; compressed bytes only |
| decoder output `Vec<f32>` per frame | owned output, moved into the frame, not copied |
| `granule_to_pcm` channel interleave | the output is interleaved; the copy *is* the format |
| WAV mux buffering the whole output | the RIFF sizes precede the data and the sink may be a pipe (no seek) |
| lookahead reservoir (`MP3_RESV_LOOKAHEAD=1`) frame copies + packet cut | opt-in, off by default; it must own every frame until flush |
| side-info `Vec` (32 B/frame) copied into the packet | public `serialize_side_info` returns a `Vec`; 32 B |
| `analyzed` `Vec` per frame | `FrameAnalysis` is stored by the lookahead path; a fixed array would be a 9.6 KB move instead |

## Found on the way, not a copy

- **`f32_to_s16`'s `round()` is a libm call per sample** at the SSE2 baseline — it,
  not the intermediate buffer, is the conform's cost on small frames. A compute
  item for `rusty-fast-transcendentals`, not this skill.

## Instruments added

- `RFF_PHASES=1` — per-phase wall time of the transcode loop (`open`, `demux`,
  `decode`, `conform`, `encode`, `mux`).
- `pinab.py --` (any command line) and `PINAB_DUR` (A/B one phase; anchor it —
  `mux +` also matches `demux`).
- `allocaudit` — peak live bytes, realloc-carried bytes, a whole-input push (the
  CLI/WAV shape), `ALLOCAUDIT_SIZES=1` size histogram, `ALLOCAUDIT_TRACE=<bytes>`.
- Gates: `mp3_cli_identity.sh` (69 CLI runs), `mp3_decode_identity.sh` (1466
  decode hashes, serial + pipelined), plus the existing `mp3_encode_identity.sh`.
  Each poison-checked.
