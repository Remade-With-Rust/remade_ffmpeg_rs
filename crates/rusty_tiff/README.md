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
  the `fax` decoders. G3 2-D and modified Huffman (compression 2) have no
  independent oracle on this machine: they are covered by our own encoder
  round trip and by sharing the G4 2-D and the G3 1-D code paths.
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
