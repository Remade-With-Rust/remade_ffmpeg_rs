# rff-codec-av2

AV2 decoding for [`remade_ffmpeg_rs`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs),
backed by the pure-Rust [`rusty_av2d`](https://crates.io/crates/rusty_av2d)
engine — no C, no FFI.

This crate is a thin adapter. `register()` wires the decoder into a
`CodecRegistry`; a private wrapper translates between the rff `Decoder` trait
and `rusty_av2d`'s push/pull API. All codec logic — the decoder that is
byte-identical to AOM's `avmdec` across a 45-clip conformance corpus — lives in
`rusty_av2d`.

> ### Decode robustness in debug builds
>
> Two separate `rusty_av2d` (≤ 0.2.11) behaviours used to bite **debug builds
> only**; release builds were never affected. Both are handled in this crate:
>
> * **Empty packet → process abort.** `rusty_av2d`'s input validation calls
>   `abort()` under `debug_assertions` when handed zero bytes
>   (`STATUS_STACK_BUFFER_OVERRUN` on Windows). An abort is not a panic, so
>   `catch_unwind` cannot contain it. This adapter now rejects empty or
>   oversized packets with `Error::InvalidData` **before** they reach the
>   engine, in every build — the same guard the AVIF adapter has for rav1d.
> * **`av2_gdf.rs:549` overflow → ordinary panic.** The GDF stripe base is
>   computed as `(i_min - 1)` in `usize`, which underflows on the first stripe
>   of every GDF frame (valid input included) and panics under overflow checks.
>   Release wraps to exactly the intended address — `av2f_still` decodes
>   byte-identically to the reference there. This workspace disables overflow
>   checks for `rusty_av2d` in its dev profile until the upstream
>   add-before-subtract fix is published. A downstream **debug** build that
>   decodes GDF content needs the same `[profile.dev.package.rusty_av2d]
>   overflow-checks = false` override, or a release build.

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
