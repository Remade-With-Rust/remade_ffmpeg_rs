# rusty_jbig2

Pure-Rust JBIG2 (ITU-T T.88 | ISO/IEC 14492) decoder, for PDF `JBIG2Decode`
streams and standalone `.jbig2` files. No dependencies. Safe API; the one
`unsafe` block is in `src/kernels.rs`, next to the safe code it is tested
against, and denied elsewhere.

```rust
// A PDF JBIG2Decode stream and its JBIG2Globals stream, if any.
let bitmap = rusty_jbig2::decode_jbig2(page, globals)?;
// Packed rows, MSB first, 1 = black. PDF wants 0 = black:
let samples = bitmap.into_pdf_samples();
```

`Jbig2File` indexes a standalone file and decodes its pages one at a time.

## What it decodes

| | |
|---|---|
| Generic regions | arithmetic templates 0-3, adaptive pixels, the 12-AT-pixel extended template, typical prediction; MMR; unknown length |
| Refinement regions | both templates, adaptive pixels, typical prediction; of intermediate regions and of the page |
| Symbol dictionaries | arithmetic and Huffman, direct, refinement and aggregate coding, retained contexts, export flags |
| Text regions | arithmetic and Huffman, every standard table, custom tables, all reference corners, transposed, refined instances |
| Halftone | pattern dictionaries and regions, arithmetic and MMR, skipping, rotated grids, patterns up to 255 pixels wide |
| Pages | OR, AND, XOR, XNOR, REPLACE; default pixel; stripes; unknown height |
| Organisations | embedded with globals, sequential, random access |

Not implemented: the colour extensions, which are `Error::Unsupported`.

## Strict and lenient

The default is strict: a truncated or damaged stream is an error.
`DecodeOptions::with_lenient(true)` leaves out segments that cannot be decoded
and returns the page as far as it goes.

Two things are accepted in strict mode although the specification asks for
more: a text region whose last strip lacks its terminating OOB (no pixel
depends on it), and line ends after an embedded stream.

## Untrusted input

Every size is checked against `Limits` before it is allocated, and every loop
whose length the data controls draws on one work budget (`max_work`), so a few
bytes cannot declare their way to gigabytes or minutes. The tests cut, flip and
fabricate streams; `fuzz/` has cargo-fuzz targets.

## Correctness

| Reference | Result |
|---|---|
| SerenityOS JBIG2 suite (111 files, reference image and T.88 Annex H) | 111/111 |
| hayro-jbig2's suite, its valid streams (158, reference images) | 158/158 |
| hayro-jbig2's lenient streams, lenient mode (20) | 19/20 |
| jbig2enc output against jbig2dec 0.20 (PDF pages with globals, generic, symbol) | 9/9 |
| T.88 Annex H.2 arithmetic coder sequence, B.2 and 7.2.8 examples | exact |
| Random images through an encoder written from the specification (generic and refinement, all templates, random AT pixels) | 2400/2400 |

The lenient stream that differs is cut inside a segment: this decoder draws
what is there of it, hayro leaves it out.

## Licence

Apache-2.0. Implemented from ITU-T T.88; the T.4 run-code tables are
`rusty_tiff`'s. Test streams are third-party and not part of the package
(`tests/data/README.md`).
