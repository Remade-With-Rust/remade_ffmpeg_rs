# rff-codec-mp3 — threat model

**Scope**: the `rff-codec-mp3` adapter — the layer that registers MP3 with the
remade_ffmpeg_rs codec registry and maps rff's `Decoder` / `Encoder` traits onto
[`rusty_mp3`](../../rusty_mp3/docs/threat-model.md). The codec internals (frame
parsing, Huffman, SIMD kernels, the psychoacoustic model) are modelled there; this
document covers what the adapter itself adds. **Reviewed**: 2026-10-04.
**Re-review**: on any change to the trait mapping, or by 2027-01-04.

## What the adapter does

- **Decode**: forwards packet bytes to `rusty_mp3::Mp3Decoder` and converts each
  decoded frame's PCM into an interleaved little-endian `f32` byte plane.
- **Encode**: parses the `-b:a` / `-q:a` option strings, and hands a frame's
  byte plane to `rusty_mp3::Mp3Encoder` as interleaved `s16` or `f32`.

No I/O, no threads, no global state, no secrets; `#![forbid(unsafe_code)]`.

## Assets and adversaries

As for `rusty_mp3` — the host's memory safety and availability, and the
integrity of the output — against a malicious media supplier (packet bytes) and a
malicious caller-side producer of frames and options (any `AudioFrame` shape, any
option string).

## Entry points

| Entry point | Untrusted input |
|---|---|
| `Decoder::send_packet` / `receive_frame` / `flush` | packet bytes, split anywhere |
| `Encoder::configure` | option strings (`b`, `q`, `qp`, `crf`, `qscale`) |
| `Encoder::send_frame` / `receive_packet` / `flush` | `AudioFrame`: format, channel count, sample rate, plane bytes, and the `samples` count |

## STRIDE pass

| Threat | Analysis / mitigation |
|---|---|
| Spoofing, Repudiation | Not applicable: no identities, no actions to attribute. |
| Tampering | Wrong-but-valid output on hostile input is by design; the adapter adds no state that input can corrupt. |
| Information disclosure / Elevation | No `unsafe` (forbidden at compile time); every buffer access is bounds-checked safe Rust. |
| **Denial of service** | **Fixed 2026-10-04**: `send_frame` trusted the frame's `samples` count, so an empty `planes` panicked and an inflated count with a short plane forced an allocation of the claimed size. The plane is now the authority (only the whole samples it holds are encoded; no plane is an error). Option strings: Rust float-to-int `as` saturates, so no string reaches undefined behaviour. Fuzzed (`fuzz/`) and property-tested. |

## Residual risks

Tracked in [`docs/plans/use-protection-please.md`](plans/use-protection-please.md).
