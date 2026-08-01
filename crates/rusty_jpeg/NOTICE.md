# NOTICE — attribution for `rusty_jpeg`

This crate is a **vendored fork**, not original work. It merges two upstream
pure-Rust projects into a single codec crate and carries them forward.

## `src/decode/` — from `jpeg-decoder` 0.3.2

- Upstream: <https://github.com/image-rs/jpeg-decoder>
- Authors: The image-rs Developers
- Licence: **MIT OR Apache-2.0** (see `LICENSE-MIT`, `LICENSE-APACHE`)

## `src/encode/` — from `jpeg-encoder` 0.7.0

- Upstream: <https://github.com/vstroebel/jpeg-encoder>
- Author: Volker Ströbel
- Licence: **(MIT OR Apache-2.0) AND IJG**

The `IJG` half of that licence is not incidental — it attaches to specific
files. `src/encode/fdct.rs` and `src/encode/avx2/fdct.rs` derive from the
Independent JPEG Group's software (the AAN forward-DCT and its x86 SIMD
extension). Those files retain their original IJG headers verbatim and must
keep them. The IJG terms require, in summary:

1. If any part of the source is distributed, this notice must remain intact.
2. Documentation of a product using this code must acknowledge that it
   "is based in part on the work of the Independent JPEG Group".
3. The authors provide no warranty, and accept no liability.
4. The software may be referred to only as "the Independent JPEG Group's
   software"; no IJG author's or company name may be used to promote products
   derived from it.

Because of (2), any product shipping `rusty_jpeg` with the encoder enabled
must carry that acknowledgement in its documentation. The workspace README
does so.

> If a future rewrite replaces `fdct.rs` with an independently derived forward
> DCT, the IJG clause can be dropped and this crate relicensed to plain
> `MIT OR Apache-2.0`. Until then it stays.

## Local changes

Changes made after vendoring are tracked in `CHANGES.md`.
