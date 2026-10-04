# rusty_mp3 — threat model

**Scope**: the `rusty_mp3` library crate (decoder + encoder), as published to
crates.io and as embedded by `rff-codec-mp3`. **Reviewed**: 2026-10-04, against
`main` + branch `mp3-memory-copies` (0.9.0 → next). **Re-review**: after any new
entry point, any new `unsafe`, any new dependency, or by 2027-01-04, whichever is
first (H-02).

## What the crate is

A pure-Rust MPEG-1/2/2.5 Layer III decoder and encoder. **No runtime
dependencies, no I/O, no networking, no threads except the opt-in
`decode_pipelined`, no global state except relaxed atomic profiling counters, no
secrets.** It turns bytes into PCM and PCM into bytes, in memory, for whoever
calls it.

## Assets

| Asset | Why it matters |
|---|---|
| Memory safety of the host process | The crate runs in-process; a memory-safety bug is the host's bug. |
| Availability of the host | A panic, hang, or unbounded allocation on hostile input takes the caller down. |
| Integrity of the output | Wrong PCM / a non-conformant bitstream is silent corruption downstream. |

There is no confidentiality asset: the crate holds no keys, credentials or
personal data, and emits nothing but the caller's own media.

## Adversaries

1. **A malicious media supplier** — controls every byte the decoder sees (an
   uploaded file, a network stream). The primary adversary.
2. **A malicious PCM supplier** — controls the encoder's sample values, channel
   count and sample rate (any f32: NaN, ±Inf, denormals, 1e30).
3. **A compromised dependency** — only dev-dependencies exist (our own
   `rusty_alloc-api`, for examples/benches); nothing ships in the library's graph.

Out of scope: an attacker who already runs code in the host process; side
channels (nothing secret is processed).

## Entry points (the untrusted surface)

| Entry point | Input | Notes |
|---|---|---|
| `Mp3Decoder::push` / `next_frame` / `flush` | arbitrary byte chunks | the stream-level decoder; frame sync over hostile bytes |
| `decode_pipelined(&[u8])` | a whole byte slice | same parse, entropy and transform on two threads |
| `decode::Mp3Decode::decode_frame` / `decode_frame_entropy` | header + side info + main data slices | the frame-level API (public) |
| `header::FrameHeader::parse([u8; 4])` | 4 bytes | rejects reserved fields and free format |
| `Mp3Encoder::push_pcm_{f32,s16,f32le,s16le}` | samples, channel count, rate | sanitized at the boundary (below) |
| `encode::Mp3Encode::encode_frame*` | per-channel frame slices | the frame-level API; trusts its caller for slice length |

## STRIDE pass

| Threat | Applies? | Analysis / mitigation |
|---|---|---|
| **S**poofing | No | No identities or authentication. |
| **T**ampering | Output integrity | Tampered input yields wrong-but-valid PCM by design (MP3 has no integrity check beyond the optional CRC, which is parsed). Decoder conformance is gated bit-exact (ISO 16/16, FFmpeg 678/678). |
| **R**epudiation | No | No actions to attribute. |
| **I**nformation disclosure | Memory reads | The only path to disclosure is an out-of-bounds read. Safe Rust everywhere except the SIMD kernels, whose bounds are fixed-size arrays established in safe code at the dispatch site (see `UNSAFE.md`). |
| **D**enial of service | **Yes — the live threat** | (a) *Panics*: a panic on hostile input is a bug. Decoder: ~77,000 mutated/random streams with overflow checks + debug assertions, 0 panics. Encoder: hostile PCM used to panic in the reservoir assembly (fixed 8383e4b — NaN→0, clamp to 8× full scale, graceful layout). (b) *Hangs*: frame sync advances at least one byte per iteration; all loops are bounded by frame geometry. (c) *Memory*: the sync buffer holds at most the caller's pushed bytes plus one partial frame (≤ 1,441 B; free format is rejected); decoded frames queue until the caller pulls them — bounded by what the caller pushes; `decode_pipelined`'s channel is bounded (32 frames). |
| **E**levation of privilege | Memory corruption | Same surface as disclosure: the SIMD kernels' writes go to fixed-size arrays. No FFI, no `transmute`, no raw allocation. |

## Highest-value attack path

A crafted stream that drives a SIMD kernel out of bounds (memory corruption) —
mitigated by fixed-size array operands and dispatch-site invariants, and checked by
the scalar-oracle tests. Next: a crafted stream that panics the decoder (DoS) —
mitigated by bounds-checked safe Rust and the mutation sweep; coverage-guided
fuzzing (H-26/H-27) is the outstanding control.

## Residual risks

Tracked with owners and review dates in the plan file's register:
[`docs/plans/use-protection-please.md`](plans/use-protection-please.md).
