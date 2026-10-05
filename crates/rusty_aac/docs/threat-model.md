# rusty_aac — threat model

**Scope**: the `rusty_aac` library crate (decoder + encoder), as published to
crates.io and as embedded by `rff-codec-aac`. **Reviewed**: 2026-10-04, against the
1.0.1 release plus the 1.1.0 work (memory-copies round, hardening pass).
**Re-review**: after any new entry point, any new `unsafe`, any new dependency, or by
2027-01-04, whichever comes first (H-02).

## What the crate is

A pure-Rust MPEG-4 AAC decoder (AAC-LC, Main, LTP, HE-AAC v1/v2 with SBR and
Parametric Stereo, ER AAC-LC/LTP, AAC-LD, AAC-ELD, 960-sample frames, PCE layouts,
LATM/LOAS) and an AAC-LC encoder. **No runtime dependencies, no I/O, no networking,
no FFI, no secrets.** Global state is limited to immutable lookup tables built once
behind `OnceLock` and relaxed atomic profiling counters (feature `profile` only). The
encoder spreads frames over scoped threads; the decoder is single-threaded.

## Assets

| Asset | Why it matters |
|---|---|
| Memory safety of the host process | The crate runs in-process; a memory-safety bug is the host's bug. |
| Availability of the host | A panic, hang, or unbounded allocation on hostile input takes the caller down. |
| Integrity of the output | Wrong PCM or a non-conformant bitstream is silent corruption downstream. |

There is no confidentiality asset: the crate holds no keys, credentials or personal
data, and emits nothing but the caller's own media.

## Adversaries

1. **A malicious media supplier** — controls every byte the decoder sees: the
   AudioSpecificConfig handed over by a container, every ADTS / LATM frame, every
   SBR / PS extension payload. The primary adversary.
2. **A malicious PCM supplier** — controls the encoder's sample values (any `f32`:
   NaN, ±Inf, subnormals, 1e30), channel count and sample rate.
3. **A compromised dependency** — the library has no runtime dependencies; only
   dev-dependencies exist (our own `rusty_alloc-api`, for benches and examples).

Out of scope: an attacker who already runs code in the host process; side channels
(nothing secret is processed).

## Entry points (the untrusted surface)

| Entry point | Input | Notes |
|---|---|---|
| `AacDecoder::decode` | an ADTS frame or a raw access unit | ADTS headers re-parsed per packet; a header change reconfigures the decoder |
| `AacDecoder::with_config_bytes`, `parse_audio_specific_config`, `config::parse` | AudioSpecificConfig bytes from a container | object type, rates, PCE (up to 16 elements per class), SBR/PS signalling, ER/LD/ELD parameters |
| `parse_adts`, `is_adts` | header bytes | reserved fields rejected |
| `latm::{parse_loas_frame, LatmReader, LatmDecoder}` | LOAS / LATM frames | `StreamMuxConfig` with an embedded config; multi-program / multi-layer muxes rejected |
| `decode::Decoder::{decode, decode_block}` | raw data blocks | the element-level decoder behind `AacDecoder` |
| `sbr::parse_sbr_config` | config bytes | SBR / PS presence |
| `AacEncoder::push_pcm`, `push_pcm_planar` | samples, channel count, rate | 1–6 channels, standard rates only |

## STRIDE pass

| Threat | Applies? | Analysis / mitigation |
|---|---|---|
| **S**poofing | No | No identities or authentication. |
| **T**ampering | Output integrity | Tampered input yields wrong-but-valid PCM by design (AAC carries no integrity check beyond optional CRCs). Decoder conformance is gated against FFmpeg on the ISO/IEC 14496-26 suite: 81 streams within 1 LSB, 2 documented FFmpeg deviations. |
| **R**epudiation | No | No actions to attribute. |
| **I**nformation disclosure | Memory reads | The only path to disclosure is an out-of-bounds read. Safe, bounds-checked Rust everywhere except the SIMD kernels and the capacity fills listed in [`UNSAFE.md`](../UNSAFE.md), whose bounds are re-established in safe code at each call. |
| **D**enial of service | **Yes — the live threat** | (a) *Panics*: a panic on hostile input is a bug. Every `unwrap` / `expect` on an input path is justified as unreachable in writing (pattern rule R3, enforced in CI); coverage-guided fuzzing covers every entry point above (H-26). (b) *Hangs*: every parse loop advances the bit reader or is bounded by a syntax limit (element counts, band counts, envelope counts); reads past the end are errors, not waits. (c) *Memory*: per-frame decoder allocations are bounded by the syntax (≤ 64 channel elements, 2048 samples each, × SBR's 2); lookup plans are cached in fixed tables (no cache keyed by stream data grows). The **encoder buffers the whole stream until `finish`** — memory grows linearly with the PCM the caller pushes, by design; a caller encoding unbounded live input must chunk it into separate encoders. |
| **E**levation of privilege | Memory corruption | Same surface as disclosure: SIMD writes go to slices re-bounded at the dispatch site or to fixed-size arrays; class-2 fills write exactly the reserved capacity. No FFI, no `transmute`, no `static mut`. |

## Highest-value attack path

A crafted stream driving a SIMD kernel out of bounds (memory corruption) — mitigated
by dispatch-site bounds and fixed-size operands, checked by the scalar-oracle tests
on x86-64 and AArch64, Miri (scalar paths) and AddressSanitizer. Next: a crafted
config or extension payload that panics the decoder (DoS) — mitigated by
bounds-checked parsing, the R3 discipline and continuous fuzzing.

## Residual risks

Tracked with owners and review dates in the plan file's register:
[`docs/plans/use-protection-please.md`](plans/use-protection-please.md).
