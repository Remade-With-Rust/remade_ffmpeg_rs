# rusty_jpx

Pure-Rust JPEG 2000 (ISO/IEC 15444-1) decoder: raw codestreams and JP2/JPX
files, including PDF `JPXDecode` streams. No C, no dependencies,
single-threaded.

Part of **[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)**
by **[Mata Network](https://www.mata.network/)**.

```rust
let file = std::fs::read("image.jp2")?;
let image = rusty_jpx::decode_jpx(&file)?;
println!("{}x{} {:?}", image.width, image.height, image.color);
for c in &image.components {
    println!("{} bits, {}x{}, {:?}", c.bits, c.width, c.height, c.role);
}
// PDF: the soft mask of an image with SMaskInData 1 or 2.
let mask = image.alpha();

// A quarter-size decode that keeps what a damaged file still has.
let options = rusty_jpx::DecodeOptions::new()
    .with_reduce(2)
    .with_allow_truncated(true);
let thumb = rusty_jpx::decode_jpx_with(&file, &options)?;

// Headers only.
let info = rusty_jpx::info(&file)?;
# Ok::<_, Box<dyn std::error::Error>>(())
```

## What it decodes

| | |
|---|---|
| Samples | 1 to 31 bits, signed and unsigned, returned as `i32` (headers up to 38 bits parse); any subsampling |
| Structure | tiles, tile-parts, precincts, all five progression orders, POC, quality layers |
| Markers | COD/COC, QCD/QCC, RGN (maxshift), PPM/PPT, SOP/EPH; TLM, PLM, PLT, CRG, COM skipped |
| Transforms | reversible 5/3, irreversible 9/7, RCT, ICT |
| Code-blocks | bypass, reset, terminate-all, vertically causal, predictable termination, segmentation symbols |
| JP2/JPX boxes | `colr` (enumerated and ICC), `pclr`/`cmap` (expanded), `cdef` (roles, colour order, opacity), `jp2c` |
| Options | reduced resolution, partial results from damaged files, limits |

Samples come back in the coded colour space: sYCC, CMYK, Lab or ICC data are
reported (`Jpx::color`, `Jpx::icc`), not converted. `Jpx::upsampled` repeats
subsampled components to image size.

Not supported, and refused with an error: HTJ2K (Part 15) code-blocks, Part 2
extensions, fragmented JPX codestreams (`ftbl`), samples over 31 bits, and
code-blocks with more than 30 magnitude bit-planes.

## Errors

Malformed input returns `JpxError`; it does not panic. Allocations and work
are bounded by `Limits` (output samples, components, and the number of
resolutions, precincts and code-blocks). A truncated file is
`JpxError::Truncated` unless everything the image needs is present (a missing
EOC marker alone is accepted).

`allow_truncated` decodes damaged files the way OpenJPEG does with strict mode
off: packets and code-block data past the damage decode as zero coefficients,
a tile-part whose header is damaged is skipped (reading goes on at the next
SOT marker), and missing tiles decode as zero coefficients. Nothing is
concealed silently: every such step is listed in `Jpx::concealed` (tile and
kind, printable), and `Jpx::truncated` is set exactly when that list is not
empty. A file that decodes without the option decodes to the same image with
it, with nothing concealed.

```rust
let options = rusty_jpx::DecodeOptions::new().with_allow_truncated(true);
let image = rusty_jpx::decode_jpx_with(&file, &options)?;
for c in &image.concealed {
    eprintln!("concealed: {c}"); // e.g. "tile 3: packets missing from here on (the data ends), zero coefficients"
}
```

One bound to know about: a code-block's bit-planes may be signalled in a few
bits, so a small hostile file can ask for up to about 90 passes over every
sample it declares. Lower `Limits::max_samples` where that matters.

## Verified against OpenJPEG 2.5.4

Full component buffers compared sample by sample (Linux x86-64).

| Streams | Files | Exact | Max error 1 | Other |
|---|---:|---:|---:|---|
| Conformance suite, profile 0/1 (`p0_*`, `p1_*`) | 23 | 16 | 7 | |
| Conformance suite, other codestreams (`a1`-`g4`) | 21 | 21 | | |
| Conformance suite, JP2 (`file1`-`9`, `subsampling`, `zoo`) | 13 | 9 | 4 | |
| opj_compress matrix (random options and geometry) | 304 | 244 | 42 | 18, below |

Against the reference images of ISO/IEC 15444-4, class 1: **23 of 23 within
the peak and MSE limits** (p0_08 at its specified reduction); every reversible
stream is exact.

The 18 others: 7 are 16-bit 9/7 streams where this decoder is 2 or 3 away from
OpenJPEG and, on the six whose source can be compared, closer to it (MSE
0.07-0.10 against 0.72-1.02 on the clearest four; OpenJPEG's 9/7 uses a
rounded scale constant). 8 combine ROI with bypass, which OpenJPEG's decoder
does not undo correctly (the lossless ones come back exact here and damaged
there). 2 use `-TP` with PCRL, where opj_compress writes packets out of progression
order and OpenJPEG does not decode its own file. 1 asks for a precinct size
the standard does not allow.

On OpenJPEG's non-regression corpus (the decodable part), differences
are limited to files that break the standard; damaged files give errors
(or, with `allow_truncated`, what they still hold). A writer that signals more
coding passes for a code-block than its bit-planes allow (`kodak_2layers_lrcp`,
`issue135`) is read as OpenJPEG reads it, a length field per 109 passes, and
decodes within 1 of OpenJPEG.

## Speed against OpenJPEG 2.5.4

One thread each, in-process decode time, ratio of minima (rusty_jpx /
OpenJPEG; below 1 is faster here):

| Image | rusty_jpx | OpenJPEG | ratio | pairs won |
|---|---:|---:|---:|---:|
| 2592x1944 RGB, 9/7, 10:1 | 191 ms | 260 ms | 0.74 | 12/12 |
| same, 512x512 tiles, 20:1 | 109 ms | 178 ms | 0.61 | 12/12 |
| same, 5/3 lossless | 563 ms | 671 ms | 0.84 | 12/12 |
| same, 5/3 lossless, bypass | 535 ms | 642 ms | 0.83 | 12/12 |
| 3906x2602 YCbCr 4:2:0, 9/7 (`zoo1`) | 472 ms | 575 ms | 0.82 | 12/12 |
| 2K frame, 5/3 (`movie_00000`) | 22 ms | 37 ms | 0.59 | 12/12 |

Method: i7-14650HX (AVX2), WSL2, `taskset` to one core, 12 rounds of processes
in alternating order, best of 2 decodes per process, rusty_alloc 2.6.0, rustc
1.99, thin LTO; OpenJPEG built by CMake Release (gcc, its SSE2 paths, no AVX2).
The box was shared: null pairs (OpenJPEG against itself) read 0.91-1.36, so
treat the ratios as accurate to about 10%.

The wavelet, colour and sample kernels use AVX2 where the processor has it,
and SSE2 or NEON otherwise; every kernel gives the same result to the bit as
its scalar version. `unsafe` code is confined to that module.

## Provenance

Written from ISO/IEC 15444-1. No code is taken from OpenJPEG, which served as
the test oracle. The per-stripe-column flag word (18 significance bits, signs,
refinement and visit bits in a `u32`) is the well-known arrangement several
decoders use, OpenJPEG among them.

License: Apache-2.0.
