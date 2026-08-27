# rff-codec-av2

AV2 decoding for [`remade_ffmpeg_rs`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs),
backed by the pure-Rust [`rusty_av2d`](https://crates.io/crates/rusty_av2d)
engine — no C, no FFI.

This crate is a thin adapter. `register()` wires the decoder into a
`CodecRegistry`; a private wrapper translates between the rff `Decoder` trait
and `rusty_av2d`'s push/pull API. All codec logic — the decoder that is
byte-identical to AOM's `avmdec` across a 45-clip conformance corpus — lives in
`rusty_av2d`.

> ### ⚠️ Decode robustness: sandbox untrusted AV2 in debug builds
>
> `rusty_av2d 0.2.8` performs an **unchecked subtraction** (`av2_gdf.rs:549`)
> that trips Rust's arithmetic-overflow check, and aborts
> (`STATUS_STACK_BUFFER_OVERRUN`) on malformed OBU input. An `abort()` is not a
> panic, so `catch_unwind` cannot contain it — a hostile file takes the process
> down.
>
> This affects **debug builds only**. Verified 2026-08-27 against a release
> build: valid AV2 decodes **byte-identically to the reference**, and a fuzz
> sweep over malformed input passes with AV2 included. A shipped release binary
> therefore neither crashes on hostile AV2 nor mis-decodes valid AV2.
>
> Treat the unchecked arithmetic as a defect to fix upstream, not a property to
> rely on: a wrap that is correct today is correct by accident, not by contract.
> If you decode untrusted input in a debug or CI build, sandbox this path.

```rust
let mut registry = rff_codec::CodecRegistry::new();
rff_codec_av2::register(&mut registry);
let mut decoder = registry.find_decoder(rff_core::CodecId::Av2)?;
```

From the CLI, AV2 in an IVF container (fourcc `AV02`) is detected automatically:

```sh
rff -i input.ivf -c:v rawvideo out.y4m
```

## Scope

**Decode only** — there is no AV2 encoder, and the registry does not advertise
one.

**Research preview.** AV2 is not a finalized standard: there are no official
conformance vectors, correctness is defined against the AVM reference as of the
pinned `rusty_av2d` release, and the bitstream may still change. Performance is
unoptimized. See `rusty_av2d`'s `STATUS.md` for the full scope statement.

Monochrome (4:0:0) output is refused rather than approximated — no corpus clip
exercises it.

## Linking alongside AV1

`rusty_av2d` and `rusty_av1d` share the dav1d symbol lineage, so their C ABI and
assembly table exports collide at link time. This crate depends on `rusty_av2d`
with `default-features = false`, which drops both and leaves the safe Rust API —
that is how this workspace links AV1 and AV2 into one binary.

## License

Apache-2.0 (this adapter). The underlying `rusty_av2d` engine is BSD-2-Clause.
