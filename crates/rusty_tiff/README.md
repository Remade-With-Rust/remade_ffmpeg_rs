# rusty_tiff

Pure-Rust TIFF and BigTIFF decoder and encoder, including the TIFF layer DNG
is built on. `#![forbid(unsafe_code)]`, no C, no `*-sys`; Deflate goes through
flate2's pure-Rust zlib-rs backend and JPEG-in-TIFF through the house
[`rusty_jpeg`](https://crates.io/crates/rusty_jpeg).

## What is verified

Every number below comes from `cargo test -p rusty_tiff --release -- --nocapture`
(Windows x86-64, Rust 1.95) unless the section says otherwise.

**Against image-tiff 0.11.3** (the `tiff` crate this replaces), as a dev-only
oracle; "identical" means byte-identical decoded sample buffers, floats
compared bit for bit:

| Corpus | Files | Identical to image-tiff | image-tiff cannot decode (checked against source) |
|---|---:|---:|---:|
| Written by rusty_tiff: 31 colour types x none/LZW/Deflate/PackBits x predictor 1/2/3 x strips/tiles x planar 1/2 x min-is-white x classic/BigTIFF x 3 sizes, plus big-endian twins | 14,796 (3,372 big-endian, 5,712 BigTIFF) | 10,482 | 4,314 |
| Written by image-tiff's encoder: 16 colour types x 4 compressions x 3 strip sizes x predictor 1/2 | 348 | 348 | 0 |
| Multipage (mixed types, strips + tiles, predictors 2 and 3) | 1 (3 pages) | 1 | 0 |
| CCITT G4 written by the `fax` crate, as TIFFs | 84 | 84 | 0 |

All 14,796 + 348 files also decode to exactly the source pixels. The 4,314
image-tiff could not decode break down as: 1,840 where **image-tiff panics**
(an assertion in its planar-tile path), 98 sub-byte LZW tiles it over-reads
("no lzw end code found"; the same streams decode fully with weezl, the LZW
codec inside image-tiff), 432 palette images (unsupported there) and 1,944
min-is-white layouts it does not invert (grey+alpha, signed, half float).

- **LZW** agrees with weezl in both directions on 605 inputs (0-600 bytes,
  table-full edges, 64 KiB, 1 MB).
- **CCITT**: 84 G4 images from the independent `fax` encoder decode
  identically; 84 G4 and 84 G3 1-D images from our encoder decode correctly in
  the `fax` decoders. G3 2-D, fill-bit and modified Huffman (compression 2)
  files are verified against libtiff (next section).
- **JPEG-in-TIFF**: `read_image` equals `rusty_jpeg` decoding the original
  complete JPEGs byte for byte (strips and tiles, YCbCr and grey);
  `read_image_raw` (YCbCr unconverted, as image-tiff returns it) is within max
  9 / mean 0.17 of image-tiff's zune-jpeg (different IDCT, so not bit-exact).
- **Tags**: 47 tag values across IFD0, both SubIFDs and the EXIF IFD of a
  DNG-shaped file read identically in image-tiff.

**Malformed input** returns `Err`, never panics, never loops, never allocates
past `Limits`:

- IFD self-loop and A->B->A cycles error; `seek_to_image` restarts cleanly.
- 2^31 x 2^31 images fail with `Limits` before any allocation, and even with
  `Limits::unlimited()` the 2^62-byte buffer is a recoverable error
  (`try_reserve`).
- Offsets past EOF, lying BigTIFF entry counts, byte counts past EOF
  (clamped, as libtiff does) or too small (error), LZW garbage.
- 2,604 truncations of 7 seed files covering every codec (slowest decode
  0.13 ms) and a deterministic 20,000-iteration mutation fuzz (byte flips,
  random bytes, truncation; 5,031 still decoded; slowest 5.9 ms, total
  0.38 s, 16 MiB decode limit).

## Against libtiff

Measured 2026-11-19 against libtiff 4.7.0 (Ubuntu 26.04 in WSL2, the
system `libtiff.so.6`, linked with libdeflate, libjpeg-turbo, zlib), called
through FFI from one Rust binary. The harness is
[`bench-libtiff/`](bench-libtiff) (not part of the published crate).

**Corpus**: 494 files, 499 pages. 405 were written by ImageMagick 7.1.2
(which writes through libtiff) across compression x type x depth x tiles x
planar x byte order x predictor x alpha flavour x fill order, plus 89 written
directly by libtiff (CCITT MH / G3 1-D / G3 2-D / with fill bits / G4, both
byte orders, LSB fill order, multipage fax; JPEG YCbCr / RGB / grey;
predictors 2 and 3). Each page is decoded by both, rusty_tiff's
`read_image_raw` against libtiff's `TIFFReadEncodedStrip/Tile` assembled into
the same layout (JPEG YCbCr: `read_image` against libtiff with
`JPEGCOLORMODE_RGB`), byte for byte, floats bit-exact:

| Compression | Pages | Identical | JPEG within tolerance | Mismatch | rusty_tiff error |
|---|---:|---:|---:|---:|---:|
| none | 101 | 101 | | 0 | 0 |
| LZW | 128 | 128 | | 0 | 0 |
| Deflate | 120 | 120 | | 0 | 0 |
| PackBits | 79 | 79 | | 0 | 0 |
| CCITT modified Huffman | 5 | 5 | | 0 | 0 |
| CCITT G3 (1-D and 2-D) | 23 | 23 | | 0 | 0 |
| CCITT G4 | 11 | 11 | | 0 | 0 |
| JPEG | 32 | 0 | 28 (max diff 1; 3 for YCbCr) | 0 | 4 |
| **Total** | **499** | **467** | **28** | **0** | **4** |

By sample type the identical pages cover 1/2/4-bit, 8-bit, 16-bit uint and
16/32/64-bit float; by photometric, min-is-white (41/41), min-is-black,
RGB, palette (20/20), CMYK and YCbCr. The 4 errors are 2-component JPEG
(grey + alpha), which `rusty_jpeg` does not decode. JPEG is compared with a
tolerance because libjpeg-turbo and `rusty_jpeg` use different IDCTs. The
corpus found one real bug, fixed before these numbers: libtiff applies
`FillOrder=2` bit reversal for every codec except JPEG, not only CCITT.

**Speed** on a 5760x4320 (24.9 Mpx) mosaic of twelve real video frames
(blue_sky, crowd_run, ducks_take_off, park_joy 1080p). RGB 16-bit is that
image blurred (sigma 0.6) at 16-bit depth, so the low byte is smooth, not
replicated. Decode files were written by ImageMagick/libtiff; MB/s is
uncompressed bytes per second at the minimum time:

| Operation | rusty_tiff | libtiff | rusty_tiff time / libtiff time | Pairs rusty_tiff faster |
|---|---:|---:|---:|---:|
| decode RGB8 none | 20.2 ms, 3704 MB/s | 11.2 ms, 6653 MB/s | 1.80 | 0/20 |
| decode RGB8 LZW | 424 ms, 176 MB/s | 261 ms, 286 MB/s | 1.62 | 0/20 |
| decode RGB8 Deflate | 290 ms, 257 MB/s | 158 ms, 472 MB/s | 1.84 | 0/20 |
| decode RGB8 PackBits | 22.0 ms, 3398 MB/s | 13.2 ms, 5644 MB/s | 1.66 | 0/20 |
| decode RGB16 none | 35.8 ms, 4167 MB/s | 20.7 ms, 7206 MB/s | 1.73 | 0/20 |
| decode RGB16 LZW | 923 ms, 162 MB/s | 517 ms, 289 MB/s | 1.79 | 0/20 |
| decode RGB16 Deflate | 529 ms, 282 MB/s | 374 ms, 399 MB/s | 1.41 | 0/20 |
| decode RGB16 PackBits | 43.9 ms, 3404 MB/s | 26.1 ms, 5719 MB/s | 1.68 | 0/20 |
| decode bilevel CCITT G4 | 27.7 ms, 899 Mpx/s | 22.1 ms, 1128 Mpx/s | 1.25 | 0/20 |
| encode RGB8 LZW | 600 ms, 124 MB/s | 754 ms, 99 MB/s | **0.80** | **20/20** |
| encode RGB8 Deflate 6 | 1307 ms, 57 MB/s | 965 ms, 77 MB/s | 1.35 | 0/20 |
| encode RGB16 LZW | 960 ms, 156 MB/s | 1504 ms, 99 MB/s | **0.64** | **20/20** |
| encode RGB16 Deflate 6 | 2896 ms, 52 MB/s | 1709 ms, 87 MB/s | 1.69 | 0/20 |

libtiff is faster at every decode (1.25-1.84x) and at Deflate encode
(1.35-1.69x; libtiff uses libdeflate, rusty_tiff zlib-rs, and libtiff's
output is 2.4% / 0.1% smaller). rusty_tiff is faster at LZW encode
(1.26x / 1.57x) with byte-identical output sizes. No speed work has been
done on this crate yet; these are first numbers, not a campaign result.

Method: both arms in one process (`tiffvs dec|enc`), pinned with
`taskset -c 6 nice -n -5`, leading arm alternated every pair (ABBA), 20
pairs per row after two warm-up runs of each arm, `Instant`
(CLOCK_MONOTONIC) around the work only, minimum of N reported with the
medians and the paired win count (every row is 20/20 or 0/20, z = +-4.47).
Work parity is checked before timing: the full decoded buffers are
compared byte for byte (FNV printed); encoded outputs from both arms must
decode to the source. Both decode arms parse the file and allocate a zeroed
output per run; libtiff reads through a memory-mapped client (its normal
file path). The Rust arm runs under rusty_alloc 2.5.0. Null arms (Rust vs
the same Rust): 0.991x (10/20), 0.979x (11/20), 0.964x (6/20, z = -1.79),
so the noise floor is about 4%. Thread CPU time / wall per sample was
0.91-1.00 on every row except G4 (one sample at 0.34, i.e. descheduled;
the minimum is unaffected). Machine: i7-14650HX, rustc 1.95.0.

## Use

```rust
use rusty_tiff::encoder::{colortype, Compression, Predictor, TiffEncoder};
use rusty_tiff::{Decoder, DecodingResult, Tag};

// Encode: two pages, LZW + predictor, an ICC profile and an arbitrary tag.
let mut file = std::io::Cursor::new(Vec::new());
let mut enc = TiffEncoder::new(&mut file)?.with_compression(Compression::Lzw);
let rgb16 = vec![0x1234u16; 64 * 48 * 3];
let mut page = enc.new_image::<colortype::RGB16>(64, 48)?.with_predictor(Predictor::Horizontal);
page.icc_profile(std::fs::read("profile.icc")?);
page.add_tag(Tag::Software, rusty_tiff::Value::Ascii("me".into()));
page.write_data(&rgb16)?;
enc.write_image::<colortype::Gray8>(16, 16, &[0u8; 256])?;

// Decode every page.
file.set_position(0);
let mut dec = Decoder::new(file)?;
loop {
    let (w, h) = dec.dimensions()?;
    match dec.read_image()? {
        DecodingResult::U16(px) => println!("{w}x{h} {:?}: {} u16", dec.colortype()?, px.len()),
        other => println!("{w}x{h} {:?}: {} samples", dec.colortype()?, other.len()),
    }
    if let Some(icc) = dec.icc_profile()? { println!("ICC {} bytes", icc.len()); }
    if !dec.more_images() { break; }
    dec.next_image()?;
}
# Ok::<_, Box<dyn std::error::Error>>(())
```

DNG structure: `dec.sub_ifds()?` lists SubIFD offsets, `dec.seek_to_ifd(off)?`
decodes one, `dec.exif_ifd()?` plus `dec.find_tag_in(&ifd, tag)?` read EXIF.
The encoder writes the same shape with `new_subimage` and `write_directory`.
The CCITT codec is usable on its own (for PDF `CCITTFaxDecode`):
`rusty_tiff::ccitt::decode(data, &Options { .. })`, whose fields map onto the
PDF filter parameters.

## Scope

| | Decode | Encode |
|---|---|---|
| Container | classic + BigTIFF, II + MM, IFD chains, SubIFDs, EXIF/GPS IFDs | classic + BigTIFF (little-endian), multipage, SubIFDs, raw IFDs |
| Layout | strips, tiles, planar 1 and 2 | strips, tiles, planar 1 and 2 |
| Samples | uint 1/2/4/8/16/32/64, int 8-64, float 16/32/64 | the same (31 colour types) |
| Compression | none, LZW (incl. old-style LSB-first), Deflate 8/32946, PackBits, CCITT MH/G3 1-D/G3 2-D/G4, JPEG 7 with JPEGTables | none, LZW, Deflate, PackBits, CCITT G3 1-D/G4 (1-bit) |
| Predictor | 2 (8-64-bit ints), 3 (16/32/64-bit floats) | 2, 3 |
| Photometric | grey min-is-black / min-is-white (inverted like image-tiff), RGB, CMYK, palette (expanded to RGB16), alpha (associated/unassociated via ExtraSamples), YCbCr 1x1, everything else raw | any (written as given) |
| Tags | every tag, typed (`Value`) or raw bytes; ICC, XMP, EXIF helpers | ICC, arbitrary tags |

API notes and deviations from image-tiff:

- `DecodingResult::F16` holds raw IEEE binary16 bit patterns as `u16` (no
  `half` dependency); `f16_to_f32` converts.
- `read_image` returns interleaved samples for planar files (image-tiff's
  `read_image` returns only the first plane); palette images come back as
  `U16` RGB and `colortype()` says `RGB(16)`. `read_image_raw` skips all
  photometric interpretation (indices, min-is-white values, CFA data, JPEG
  YCbCr unconverted).
- Two-sample grey is `ColorType::GrayA` (image-tiff reports `Multiband`).
- `Value` keeps arrays packed (`Short(Vec<u16>)`...) instead of one enum node
  per element; `get_tag`/`find_tag` read the current IFD, `find_tag_in` any IFD.
- `write_data` returns the IFD offset (for SubIFD/EXIF pointers).

Not done: old-style JPEG (compression 6), 12-bit and lossless JPEG (so DNG
raw data in lossless JPEG is not decoded; its TIFF structure and tags are),
subsampled uncompressed YCbCr, bit depths other than 1/2/4/8/16/32/64, CCITT
uncompressed-mode extensions, ZSTD/WebP/LERC compression, big-endian writing.

License: Apache-2.0.
