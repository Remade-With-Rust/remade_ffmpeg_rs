# rff-format-heif

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

**HEIF / HEIC still images** (iPhone photos) for **remade_ffmpeg_rs**, decoded
in pure Rust: our own HEIF item parser (`rff_format::heif`) and our own HEVC
decoder, [`rusty_h265`](https://crates.io/crates/rusty_h265). No C, no FFI.

- **Primary image**: an `hvc1` item, a `grid` of tiles (decoded in parallel,
  cropped to the grid's output size), or an `iden` derivation; 8- and 10-bit
  4:2:0 or monochrome (4:0:0).
- **Transforms**: `clap`, `irot`, `imir`, in the order the file lists them.
- **Alpha**: `auxl` alpha planes (monochrome-coded, as libheif and Apple
  write them), with the premultiplied (`prem`) flag.
- **Colour**: `colr` nclx (falling back to the HEVC VUI) and ICC profiles.
- **Metadata**: Exif and XMP items, raw.
- **Depth maps and thumbnails**, decoded on request.
- **Bounded**: every count and size is checked against `Limits`; malformed
  files are errors, never panics.

Not covered: encoding, image sequences and overlays (`iovl`). HEVC items
decode in 4:0:0, 4:2:0, 4:2:2 and 4:4:4 at 8 to 12 bits (camera `.HIF` files
are 4:2:2 10-bit); `rusty_h265` refuses the few range-extension tools it does
not implement (16-bit coding) by name.
HDR gain maps are reported, not applied.

## Usage

```rust,no_run
use rff_format_heif::Heif;

let data = std::fs::read("photo.heic").unwrap();
let heif = Heif::parse(&data).unwrap();

let info = heif.primary_info().unwrap(); // size, depth, alpha: no pixels decoded
let picture = heif.decode_primary().unwrap(); // planar YCbCr (+ alpha, colour)
let rgba = picture.to_rgba8(); // 8-bit RGBA, straight alpha
let exif = heif.exif_tiff(); // TIFF-header Exif block
let depth = heif.decode_depth(); // Option<Result<Image>>
# let _ = (info, rgba, exif, depth);
```

Inside the rff engine the crate registers a demuxer (`heic`, `heif`, `hif`),
so `rff -i photo.heic photo.png` works.

## License

Apache-2.0
