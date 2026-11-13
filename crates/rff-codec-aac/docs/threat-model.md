# rff-codec-aac — threat model

**Scope**: the `rff-codec-aac` adapter — the layer that registers AAC with the
remade_ffmpeg_rs codec registry and maps rff's `Decoder` / `Encoder` traits onto
[`rusty_aac`](../../rusty_aac/docs/threat-model.md). The codec internals (bitstream
and config parsing, SBR/PS, the SIMD kernels, the psychoacoustic encoder) are
modelled there; this document covers what the adapter itself adds.
**Reviewed**: 2026-10-04. **Re-review**: on any change to the trait mapping, or by
2027-01-04.

## What the adapter does

- **Decode**: configures `rusty_aac::AacDecoder` from the container's extradata (the
  raw AudioSpecificConfig, so SBR/PS signalling survives) or from the declared rate
  and channel count, forwards packet bytes, and converts each decoded block's PCM
  into an interleaved little-endian `f32` byte plane.
- **Encode**: reads the `b` (bitrate) option, and hands a frame's byte planes to
  `rusty_aac::AacEncoder` as interleaved `s16` / `f32` or planar `f32`.

No I/O, no threads of its own, no global state, no secrets;
`#![forbid(unsafe_code)]`.

## Assets and adversaries

As for `rusty_aac` — the host's memory safety and availability, and the integrity
of the output — against a malicious media supplier (packet bytes, extradata) and a
malicious caller-side producer of frames and options (any `AudioFrame` shape, any
option value).

## Entry points

| Entry point | Untrusted input |
|---|---|
| `Decoder::configure` | `CodecParams`: extradata bytes, sample rate, channel count |
| `Decoder::send_packet` / `receive_frame` / `flush` | packet bytes, split anywhere |
| `Encoder::configure` | the `b` option (any integer) |
| `Encoder::send_frame` / `receive_packet` / `flush` | `AudioFrame`: format, channel count, sample rate, plane bytes, plane count, and the `samples` count |

## STRIDE pass

| Threat | Analysis / mitigation |
|---|---|
| Spoofing, Repudiation | Not applicable: no identities, no actions to attribute. |
| Tampering | Wrong-but-valid output on hostile input is by design; the adapter adds no state that input can corrupt. |
| Information disclosure / Elevation | No `unsafe` (forbidden at compile time); every buffer access is bounds-checked safe Rust. |
| **Denial of service** | **Fixed 2026-10-04** (the same defect class as the MP3 adapter's): `send_frame` trusted the frame's `samples` count — an empty `planes` panicked on `planes[0]`, a short plane was indexed past its end, and an inflated count sized a `Vec::with_capacity` to the claim. The planes are now the authority: only the whole sample frames every plane holds are encoded; a missing plane, or fewer planes than channels for planar input, is an error. A `b` value beyond `u32` saturates instead of truncating. The decoder queues one frame per packet sent, so its memory is bounded by the caller. Fuzzed (`fuzz/`), property-tested, and differentially tested against `rusty_aac`. |

## Residual risks

Tracked in [`docs/plans/use-protection-please.md`](plans/use-protection-please.md).
