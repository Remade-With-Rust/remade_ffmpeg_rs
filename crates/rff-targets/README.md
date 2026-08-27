# rff-targets

**"I have this file — what can I turn it into?"**

Given the streams of an input, `rff-targets` enumerates every container the
engine can actually write for it, and says for each one exactly what would
happen to each stream: a byte-exact stream copy, a re-encode (and whether that
re-encode is lossy), or a drop the container forces.

```text
$ rffprobe -show_targets clip.webm

Input #0, matroska, from 'clip.webm':
  Stream #0:0: video: vp9

Convert to (15 targets):

  video
    .mkv    copy      vp9 copy
               $ rff -i clip.webm -c:v copy clip.mkv
    .webm   copy      vp9 copy
               $ rff -i clip.webm -c:v copy clip.webm
    .ivf    copy      vp9 copy
               $ rff -i clip.webm -c:v copy clip.ivf
    .y4m    lossless  vp9->rawvideo
               $ rff -i clip.webm -c:v rawvideo clip.y4m
    .mp4    lossy     vp9->h264
               ! vp9 -> h264 re-encodes already-lossy data (generation loss)
               $ rff -i clip.webm -c:v h264 clip.mp4
    ...

  image
    .png    lossless  vp9->png
               ! holds one picture — only the first frame is written
               $ rff -i clip.webm -c:v png clip.png
    ...
```

## Why this is a crate and not a lookup table

The naive version of this feature is a hard-coded map of "these extensions can
become those extensions". That map is wrong the moment a codec is added, a
feature flag turns one off, or a muxer learns a new codec — and it is wrong
*silently*, offering conversions that fail.

This crate derives the answer from the engine's own registries instead:

- **`MuxCaps`** on each registered `Format` declares what that container's muxer
  accepts. It lives next to the muxer, in the same crate, and
  `crates/rff/tests/mux_caps.rs` drives every declared `(format, codec)` pair
  through the real muxer to prove the declaration is true.
- **The `CodecRegistry`** says which codecs this build can encode and decode.

So a build compiled without the VP9 codec reports fewer targets, automatically
and correctly. And `crates/rff/tests/targets_end_to_end.rs` closes the loop:
it takes a real file, asks what it can become, and *runs every answer* through
the transcoder.

## Using it

```rust
let engine = rff::Engine::new();
let plan = rff::targets::targets(&engine, "clip.mp4")?;

for t in &plan.targets {
    println!("{:<6} {:<9} {}", t.extension, t.fidelity, t.stream_summary());
    for note in &t.notes {
        println!("       ! {note}");
    }
}

// The subset that needs no re-encoding at all:
for t in plan.stream_copies() {
    println!("remux: {}", t.extension);
}
```

Each `Target` carries a ready-to-run `args` list (`["-c:v", "copy", "-c:a",
"opus"]`), so a caller does not have to trust that the engine's defaults match
what was reported — the plan pins its own codecs.

### For a web app

`Plan::to_json()` renders the whole answer as JSON with no serde dependency:

```json
{"source_format":"matroska",
 "source":[{"index":0,"type":"video","codec":"vp9"}],
 "targets":[{"format":"webm","extension":"webm","kind":"video",
             "fidelity":"copy","summary":"vp9 copy (copy)",
             "args":["-c:v","copy"],"notes":[],
             "streams":[{"input_index":0,"type":"video","from":"vp9",
                         "action":"copy"}]}]}
```

Two more helpers need no input file at all:

- `format_matrix(&engine.formats)` — the whole read/write matrix for this build,
  to populate a format picker up front.
- `readable_extensions(&engine.formats)` — the accept list for a file input.

## What the fields mean

| Field | Meaning |
|---|---|
| `fidelity: Copy` | Every kept stream is muxed through untouched. Bytes preserved. |
| `fidelity: Lossless` | Something is re-encoded, but every hop is mathematically lossless (PCM→FLAC, →PNG, →WebP/VP8L). |
| `fidelity: Lossy` | At least one stream goes through a lossy encoder. |
| `notes` | Caveats worth showing a user: a dropped stream, a still-image truncation, a second lossy generation, a multi-file output. |
| `kind` | `video` / `audio` / `image` / `subtitle` — what a UI groups the target under. |

Targets are ordered: the kind matching the input first, then copy before
lossless before lossy, then a per-kind running order (a `.flac` input leads with
`.mp3` and `.wav`, not with the `.mkv` that also happens to accept its audio).

A container where *every* stream would be dropped is not offered at all —
listing a target that produces nothing would be a lie.
