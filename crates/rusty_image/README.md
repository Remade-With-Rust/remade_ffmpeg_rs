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
| BMP files (all header versions, bit depths, compressions, masks, top-down) | 3,953 | 47 | 0 |
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
  past 65,535, is refused before anything is allocated.

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
