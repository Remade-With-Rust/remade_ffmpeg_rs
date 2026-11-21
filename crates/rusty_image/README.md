# rusty_image

Pure-Rust decode and encode for the small image formats, and header-only
probing. No C, no `unsafe` (`#![forbid(unsafe_code)]`), one dependency
([`rusty_png`](https://crates.io/crates/rusty_png), for PNG icon entries).

| format | decode | encode |
|---|---|---|
| **BMP**: 1/2/4/8/16/24/32-bit, RLE4/RLE8, `BI_BITFIELDS`, core and V2-V5 headers, top-down | ✓, plus headerless DIBs: `CF_DIB` / `CF_DIBV5` clipboard payloads, ICO entries | ✓ `L8`/`Rgb8`/`Rgba8`, plus `encode_dib` for the clipboard |
| **TGA**: true-colour, grey, colour-mapped, RLE, all four origins | ✓ | ✓ RLE |
| **Radiance HDR**: RGBE, new and old RLE, all eight orientations | ✓ `Rgb32F` | ✓ |
| **ICO / CUR**: BMP and PNG entries, AND masks, hotspots | ✓ (`image`'s entry choice, or every entry) | ✓ multi-size, cursors |
| **PNM**: P1-P6 and PAM (P7), ASCII and binary, 8 and 16-bit | ✓ | ✓ PGM / PPM / PAM |
| **QOI** | ✓ | ✓ |

`probe` returns the format, width and height from the header alone for all
of the above plus PNG, JPEG, GIF, WebP, TIFF and AVIF.

## Verified against `image` 0.25 and `imagesize`

Decoded pixels match the [`image`](https://crates.io/crates/image) crate
0.25.10 byte for byte, including its choice of pixel layout for each file,
so one can replace the other. The gate (`tests/oracle.rs`) generates random,
structurally valid files per format, decodes each with both crates, and
requires identical results: both refuse it, or both produce the same layout
and the same bytes (HDR floats bit for bit).

| corpus | identical | refused by both | mismatched |
|---|---:|---:|---:|
| BMP files (all header versions, bit depths, compressions, masks, top-down, masks-in-header-only) | 4,195 | 47 | 0 |
| the same DIBs headerless (`CF_DIB` / `CF_DIBV5` path) | 3,856 | 144 | 0 |
| TGA (types 1/2/3/9/10/11, 8-32 bit, colour maps, origins, RLE) | 3,199 | 801 | 0 |
| Radiance HDR (new RLE, old RLE, flat) | 600 | 0 | 0 |
| ICO (BMP 8/24/32-bit with and without AND masks, PNG entries) | 800 | 0 | 0 |
| PNM (P1-P7, maxvals 1 to 65535, comments) | 3,000 | 0 | 0 |
| QOI | 500 | 0 | 0 |
| **real files** (183 Windows cursors, icons and bitmaps under Program Files, files in the local cargo registry) | 365 | 5 | 0 |

- **Encoders:** 4,000 images went through our encoders and decoded identically
  in both crates. `image`'s own QOI/TGA/PNM output decodes identically here.
  `encode_dib(CF_DIBV5)` keeps alpha through `image`'s headerless reader.
- **`probe`:** agrees with `imagesize` 0.14 on 660/660 generated files across
  all 12 formats, and on all 6,236 real files that `imagesize` recognises. The
  only difference is that `probe` also reads CUR files (214 here), which
  `imagesize` does not recognise.
- **Damaged input:** more than 17,000 truncated and bit-flipped copies decoded
  without a panic. A header claiming more than 1 GiB of pixels, or a BMP side
  past 65,535, is refused before anything is allocated. Each decoder also
  refuses, before allocating, an image whose declared size the bytes present
  could not cover
  ([PR #164](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/164)).
  On 26,323 damaged BMP/DIB/CUR/ICO/TGA/QOI/HDR/PNM files the largest
  allocation in one decode fell from 805 MB to 6.3 MB, and files allocating
  over 16 MiB from 15 to 0, with no verdict or pixel changes
  ([independent check](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/171#issuecomment-7059366157)).

## Speed (after 0.1.1)

A 22-commit instruction-count pass
([PR #171](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/171)).
Output is byte-identical to 0.1.1: all 17 timed workloads hash the same, the
3,975-file intact set is identical, and 44,495 damaged files get the same
verdicts and pixels. Ratios are the other decoder's time divided by
rusty_image's; above 1 means rusty_image is faster.

| decode unless noted, 1024x1024 | 0.1.1 | `image` 0.25.10 | craft-image-codecs |
|---|---:|---:|---:|
| BMP 1 / 2 / 4-bit palette | 1.96 / 1.73 / 1.66 | 1.63 / 1.49 / 1.48 | 1.28 / 1.27 / 1.46 |
| BMP 8-bit grey / RLE8 | 1.29 / 1.17 | 1.62 / 2.07 | 1.57 / 2.09 |
| BMP 24 / 32-bit | level / 1.07 | 2.05 / 1.96 | 2.04 / 1.95 |
| TGA 32-bit RLE / 24-bit raw | level | 1.62 / 1.67 | 1.65 / 1.65 |
| HDR RLE / flat | 1.11 / 1.15 | 1.08 / 1.13 | 1.42 / 1.83 |
| QOI RGBA | 1.09 | 1.23 | 1.23 |
| **QOI RGB** | level | **0.98** | **0.97 (level)** |
| PPM 8 / 16-bit | level | level | 6.22 / 2.66 |
| ICO encode: one PNG entry / BMP + PNG entries | 1.16 / 2.50 | 1.03 / 5.86 | n/a |

"Level" against 0.1.1 means 0.97-1.03. craft-image-codecs uses `image`
underneath for BMP, TGA, QOI and HDR and its own PNM reader, so its HDR
column includes its wrapper's cost. The pass also fixed footer-less
true-colour TGA files, which `probe` and `decode` used to miss (27 of 27 in
the independent set now decode). The commits' own pinned walls were larger
on the palette and 32-bit BMP rows; the table uses the independent figures.

Method: an independent run by a downstream team
([PR #171 comment](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/171#issuecomment-7059366157)),
PR head `47c8594b`, one process per arm per round pinned to one core at High
priority, 20 alternating rounds, minimum in-memory loop time, null arms
0.97-1.02 (one flat-HDR null at 0.91, so read that row loosely). The pass's
own instruction counts ran under
[rusty_alloc](https://github.com/Remade-With-Rust/rusty_alloc), the house
allocator.

## Against the C references (stb_image, qoi.h, ImageMagick 7, gdk-pixbuf)

`bench-c/` decodes and encodes the same files with each engine, in one process (setup and method in `bench-c/README.md`). The speed tables in this section were measured before the pass above and have not been re-run.

**Corpus:** 115 files.
- 102 are small variants written by ImageMagick: every BMP depth (with and without RLE), core/V3/V5 headers, TGA raw/RLE/grey/colour-mapped, PNM ASCII and binary at 8 and 16-bit, PAM, HDR, QOI and ICO.
- 13 are 8.3 Mpx versions of a real photograph.

**Result:** no file that this crate refuses or decodes wrongly. The disagreements come from the other libraries:
- **stb_image:** reads no RLE BMP and no PAM. It does not rescale low-maxval PNM. It decodes HDR as `(c + 0.5) * 2^(e-136)`; this crate, `image` and ImageMagick use `c * 2^(e-136)`.
- **gdk-pixbuf:** returns 16-bit PNM as 8-bit.
- **ImageMagick:** picks a different entry from multi-size icons. Its HDR values differ by at most 1.5e-5 (it stores pixels at 16 bits).

The corpus did find one bug here, now fixed: a V5 bitfield BMP with its masks only inside the header. The `image` oracle now generates that layout too.

**Decode speed** (Mpx/s, 8.3 Mpx photo; higher is faster; "-" means the engine does not read the format):

| file | rusty_image | stb_image | qoi.h | ImageMagick 7 | gdk-pixbuf | image (Rust) |
|---|---:|---:|---:|---:|---:|---:|
| BMP 24-bit | **1756** | 539 | | 369 | 149 | 639 |
| BMP 32-bit bitfields + alpha | **997** | 400 | | 147 | 107 | 558 |
| BMP RLE8 | 461 | - | | 115 | 244 | **543** |
| TGA 24-bit raw | 1576 | **1626** | | 88 | 158 | 1001 |
| TGA RLE 24-bit | 273 | **288** | | 101 | 167 | 252 |
| TGA RLE 32-bit | **990** | 265 | | 73 | 120 | 700 |
| PPM 8-bit | 4691 | **7632** | | 330 | 197 | 4305 |
| PPM 16-bit | 1122 | 297 | | 331 | 64 | **1168** |
| PGM 8-bit | **16105** | 15304 | | 283 | 253 | 12554 |
| PAM RGBA | 3019 | - | | 243 | - | **3099** |
| HDR (RLE) | 161 | 133 | | 46 | | **210** |
| QOI RGB | 212 | | 274 | 114 | | **279** |
| QOI RGBA | 262 | | 271 | 52 | | **357** |

**Encode speed** (Mpx/s):

| output | rusty_image | stb_image_write | qoi.h | ImageMagick 7 |
|---|---:|---:|---:|---:|
| BMP 24-bit | **1325** | 1014 | | 138 |
| BMP 32-bit | **1231** | 335 | | 110 |
| TGA RLE 24-bit | **252** | 174 | | 61 |
| TGA RLE 32-bit | **608** | 233 | | 49 |
| PPM | **7988** | | | 160 |
| HDR | 58 | **73** | | 20 |
| QOI RGB | 191 | | **226** | 106 |
| QOI RGBA | 199 | | **224** | 38 |

**Summary:**
- **Faster than every C engine** at BMP decode, RLE32 TGA decode, 16-bit PNM decode, and BMP, TGA and PPM encode.
- **Faster than ImageMagick and gdk-pixbuf everywhere.**
- **At parity with stb_image** on raw TGA, TGA RLE24 and PGM.
- **Behind:**
  - stb_image on 8-bit PPM (0.61x);
  - qoi.h on QOI (0.77-0.97x decode, 0.85-0.89x encode);
  - stb_image_write on HDR encode (0.80x).
- **Against the `image` crate:** behind on QOI, HDR and RLE8 decode.

## API

```rust
use rusty_image::{decode, encode, probe, Format, Image, PixelFormat};

let bytes = std::fs::read("icon.ico")?;
if let Some((format, w, h)) = probe(&bytes) {
    println!("{format:?} {w}x{h}");
}
let img: Image = decode(&bytes)?;          // width, height, format, pixels
let qoi = encode(&img, Format::Qoi)?;

// The Windows clipboard (arboard's CF_DIB / CF_DIBV5 payloads):
let from_clipboard = rusty_image::bmp::decode_dib(&dib_bytes)?;
let to_clipboard = rusty_image::bmp::encode_dib(&img, rusty_image::bmp::DibKind::DibV5)?;

// Every icon size, with cursor hotspots:
for entry in rusty_image::ico::decode_entries(&bytes)? {
    println!("{}x{} hotspot {:?}", entry.image.width, entry.image.height, entry.hotspot);
}
```

`Image::pixels` holds rows top to bottom with no padding. 16-bit and float
samples are in native byte order, as `image`'s `as_bytes()` gives them.
`PixelFormat` is one of `L8`, `La8`, `Rgb8`, `Rgba8`, `L16`, `La16`, `Rgb16`,
`Rgba16`, `Rgb32F` and `Rgba32F`.

## Where it differs from `image`

These are all files `image` refuses and this crate reads:
- HDR in the seven orientations other than `-Y H +X W`;
- the `#?RGBE` HDR signature;
- ICO PNG entries that are not 8-bit RGBA, through `decode_entries`.

`decode` still makes the same entry choice as `image`.

## Not covered yet

- BMP with embedded JPEG/PNG (compression 4/5) and CMYK BMP. `image` refuses
  these too.
- HDR `XYZE` data.
- 1-bit PBM encoding. P4 decodes; encoding writes PGM.
