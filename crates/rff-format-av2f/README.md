# rff-format-av2f

The **AV2F** still-image container for [remade_ffmpeg_rs] — one AV2 picture in an
ISOBMFF/HEIF file, in the shape AVIF uses for AV1. The container work lives in
[`rusty_av2f`]; this crate is the demuxer/muxer that registers it with the engine.

```rust
let engine = rff::Engine::new();
let mut demuxer = engine.formats.open_demuxer("av2f", input)?;
let streams = demuxer.read_header()?;      // one stream, CodecId::Av2
let packet  = demuxer.read_packet()?;      // the still picture
```

Decoding goes through the existing `CodecId::Av2` decoder (`rff-codec-av2`, backed
by [`rusty_av2d`]) — the payload inside an AV2F file is a plain AV2 bitstream, so
the container adds no codec surface at all.

## Experimental — not an AOM standard

AVIF's brand, item type and configuration record are fixed by a published AOM
specification. **No equivalent document exists for AV2**, so AV2F's four-character
codes (`av2f` / `av02` / `av2C`) are *chosen*, not *specified*. They are isolated
in `rusty_av2f::fourcc` so a real specification can be adopted by editing one file.

Files written here are readable here and nowhere else. Useful for pipeline work
and for measuring AV2 against AVIF on stills; **not for interchange**, and with no
compatibility promise if a specification later says something different. See the
[`rusty_av2f` README][`rusty_av2f`] for the full statement.

## Header forms

Both AV2 still-picture header forms mux and demux: the full form and the compact
`single_picture_header_flag` form (the natural choice for an image format — it
is what AVIF does for AV1). The historical full-only restriction was lifted with
`rusty_av2f` 0.2.0, once `rusty_av2d` 0.2.5 decoded the compact form
byte-identically to AOM's reference decoder.

## Behaviour worth knowing

- **A still image is one picture.** A second `write_packet` is an error, not a
  silently dropped frame.
- **Probing is by content**, not filename: `probe` requires the `ftyp` brand, so
  an AVIF file scores zero here.
- **The demuxer reads the whole file** before returning its header. Still images
  are small and `iloc` offsets are absolute, so streaming buys nothing.

## Tests

`crates/rff/tests/av2f_still.rs` carries the end-to-end gate: a committed `.av2f`
fixture is sniffed, demuxed, decoded, and compared **byte-for-byte against the
pixels AOM's `avmdec` produced** for the same picture. It also muxes the picture
back out and asserts the engine writes the exact bytes it read — so a drift in the
container layout fails the build instead of quietly minting a new dialect.

[remade_ffmpeg_rs]: https://github.com/Remade-With-Rust/remade_ffmpeg_rs
[`rusty_av2f`]: https://crates.io/crates/rusty_av2f
[`rusty_av2d`]: https://crates.io/crates/rusty_av2d
