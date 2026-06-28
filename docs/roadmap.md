# Roadmap

What's next, **prioritized next-gen first** — the same lens as the rest of the
project: pure Rust, permissive license, **prefer royalty-free**. We bet on where
media is going before filling in where it's been.

Current support lives in [compatibility.md](compatibility.md); this is the
forward plan.

## Tier 1 — Next-gen (priority)

The forward bets: modern, royalty-free, where being early matters.

- **AV2 decode** — AOMedia's next-gen video, royalty-free under the AOMedia
  Patent License (same grant as AV1). In development — see
  [compatibility.md](compatibility.md).
- **fMP4 / CMAF segments** — the modern *unified* segment format for HLS **and**
  DASH; supersedes MPEG-TS segments for new deployments and lets one set of
  segments serve both protocols.
- **Low-latency live** — SRT (Secure Reliable Transport), WebRTC, and
  Media-over-QUIC (MoQ) for sub-second delivery and ingest.
- **IAMF** (Immersive Audio Model & Formats) — AOMedia's royalty-free next-gen
  spatial audio. *(exploratory)*

## Tier 2 — Current modern (round out the standard stack)

What today's products expect; brings parity with the modern baseline.

- **DASH** (`.mpd`) output — the adaptive-streaming standard alongside HLS.
- **HLS completion** — fMP4 segments, `-hls_time` / `-hls_list_size`, and
  live/event playlists (today: VOD + MPEG-TS segments).
- **`filter_complex`** — `concat` and multi-output graphs (today: `overlay`).
- **Two-pass rate control** — actual execution (today: `-pass` is parsed but
  runs single-pass).
- **HTTPS in the default build** — once a hardened, audited pure-Rust TLS
  provider lands (today: opt-in `--features https`).
- **RTMP / FLV ingest** — only if live ingest is in scope; FLV's one remaining
  live use (OBS → Twitch/YouTube push RTMP/FLV).
- **Encoders where a permissive Rust impl exists** — e.g. FLAC / Vorbis encode
  (currently decode-only, blocked on a permissive Rust encoder).

## Tier 3 — Patent-gated (evaluate; gate or skip)

Useful or even next-gen, but **standard-essential-patent encumbered** — the
opposite of the royalty-free AV1/AV2 stack. Only via the documented
ship-and-document posture or behind a Cargo feature:

- **HEVC / H.265** decode, **VVC / H.266**, **AC-3 / E-AC-3** (Dolby).

## Out of scope (for now)

`ffplay`, `libavdevice`, `libpostproc`. See
[ffmpeg-parity.md](ffmpeg-parity.md) for the full scope rationale.
