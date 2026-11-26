//! HEIF / HEIC still images, decoded in pure Rust.
//!
//! An iPhone photo (`.heic`) is a HEIF file: a small database of *items* in
//! ISO base media boxes, whose primary item is usually a `grid` of HEVC
//! intra-coded tiles. This crate reads that database with the shared parser
//! in [`rff_format::heif`], decodes the HEVC items with
//! [`rusty_h265`](https://crates.io/crates/rusty_h265), and assembles the
//! picture the file describes:
//!
//! * **primary image** — an `hvc1` item, a `grid` of them (cropped to the
//!   grid's output size), or an `iden` derivation; 8- and 10-bit 4:2:0;
//! * **transforms** — `clap`, `irot`, `imir`, applied in the order the file
//!   associates them (`ipma`), rounded exactly as libheif rounds `clap`;
//! * **alpha** — an `auxl` auxiliary image with an alpha `auxC` type, and the
//!   `prem` (premultiplied) flag;
//! * **colour** — `colr` `nclx` (falling back to the HEVC VUI) and ICC;
//! * **metadata** — the Exif and XMP items, as raw bytes;
//! * **depth maps and thumbnails** — decoded on request.
//!
//! Not covered: encoding, image sequences (`msf1` tracks), overlays
//! (`iovl`), and HEVC range extensions (4:0:0, 4:2:2, 4:4:4, 12-bit), which
//! `rusty_h265` refuses by name. Gain maps are reported, not applied.
//!
//! ```no_run
//! let data = std::fs::read("photo.heic").unwrap();
//! let heif = rff_format_heif::Heif::parse(&data).unwrap();
//! let picture = heif.decode_primary().unwrap();
//! println!("{}x{}", picture.image.width, picture.image.height);
//! let rgba = picture.to_rgba8();
//! let exif = heif.exif();
//! # let _ = (rgba, exif);
//! ```
//!
//! Every count and size in the file is bounded by [`Limits`]; a malformed
//! file returns an error, never a panic or an unbounded allocation.

// `unsafe` lives only in `simd`, behind safe bounds-checked wrappers.
#![deny(unsafe_code)]

mod demux;
mod hevc;
mod image;
mod rgb;
mod simd;

use std::borrow::Cow;

use rff_core::{Error, Result};
pub use rff_format::heif::{Colour, HeifFile, Limits, Property};

pub use crate::image::{Chroma, Image, Plane};
pub use demux::register;

/// How [`Heif`] decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeOptions {
    pub limits: Limits,
    /// Worker threads for decoding grid tiles: `0` = one per available core
    /// (the default), `1` = decode on the calling thread only.
    pub threads: usize,
    /// Apply `clap`/`irot`/`imir` (default `true`).
    pub apply_transforms: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        DecodeOptions {
            limits: Limits::default(),
            threads: 0,
            apply_transforms: true,
        }
    }
}

/// `colr` `nclx` code points (ITU-T H.273).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nclx {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

/// Where an image's colour description came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColourInfo {
    /// The `colr` `nclx` property, else the HEVC VUI's colour description.
    pub nclx: Option<Nclx>,
    /// True when [`nclx`](Self::nclx) came from the HEVC VUI, not `colr`.
    pub nclx_from_bitstream: bool,
    /// The `colr` ICC profile (`prof` or `rICC`), verbatim.
    pub icc: Option<Vec<u8>>,
}

/// An alpha plane, at the primary image's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alpha {
    pub plane: Plane,
    pub bit_depth: u8,
    /// `prem`: colour samples are premultiplied by this alpha.
    pub premultiplied: bool,
}

/// A decoded primary image with what belongs to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub image: Image,
    pub alpha: Option<Alpha>,
    pub colour: ColourInfo,
}

impl Decoded {
    /// Convert to 8-bit straight-alpha RGBA (alpha 255 when there is none),
    /// using the `nclx` matrix and range (BT.601 when unspecified, as
    /// libheif does) and nearest-neighbour chroma.
    pub fn to_rgba8(&self) -> Vec<u8> {
        rgb::to_rgba8(&self.image, self.alpha.as_ref(), self.colour.nclx)
    }
}

/// What a file says about its primary image, without decoding pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    pub item_id: u32,
    /// Size after transforms.
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub chroma: Chroma,
    pub has_alpha: bool,
    pub has_depth: bool,
    /// Item ids of the thumbnails (`thmb`).
    pub thumbnails: Vec<u32>,
    /// True when the file carries an HDR gain map (Apple
    /// `urn:com:apple:photo:2020:aux:hdrgainmap` or an ISO 21496-1 `tmap`
    /// item); it is not applied.
    pub has_gain_map: bool,
}

/// A parsed HEIF file, ready to decode.
#[derive(Debug, Clone)]
pub struct Heif<'a> {
    file: HeifFile<'a>,
    opts: DecodeOptions,
}

const APPLE_GAIN_MAP: &str = "urn:com:apple:photo:2020:aux:hdrgainmap";

impl<'a> Heif<'a> {
    /// Parse with default options.
    pub fn parse(data: &'a [u8]) -> Result<Heif<'a>> {
        Heif::parse_with(data, DecodeOptions::default())
    }

    /// Parse with `opts`.
    pub fn parse_with(data: &'a [u8], opts: DecodeOptions) -> Result<Heif<'a>> {
        let file = HeifFile::parse(data, opts.limits)?;
        if &file.handler != b"pict" {
            return Err(Error::invalid(
                "heif: no still image (`meta` handler is not `pict`)",
            ));
        }
        Ok(Heif { file, opts })
    }

    /// The parsed item model.
    pub fn file(&self) -> &HeifFile<'a> {
        &self.file
    }

    /// The primary item's id.
    pub fn primary_id(&self) -> Result<u32> {
        self.file
            .primary_item()
            .map(|it| it.id)
            .ok_or_else(|| Error::invalid("heif: no primary item"))
    }

    /// Describe the primary image without decoding it.
    pub fn primary_info(&self) -> Result<ImageInfo> {
        let id = self.primary_id()?;
        let (w, h, chroma, bit_depth) = self.geometry(id, 0)?;
        let (w, h) = if self.opts.apply_transforms {
            self.transformed_size(id, w, h)
        } else {
            (w, h)
        };
        Ok(ImageInfo {
            item_id: id,
            width: w,
            height: h,
            bit_depth,
            chroma,
            has_alpha: self.alpha_id(id).is_some(),
            has_depth: self.depth_id(id).is_some(),
            thumbnails: self.thumbnail_ids(id),
            has_gain_map: self.has_gain_map(),
        })
    }

    /// Decode the primary image, its alpha plane and colour description.
    pub fn decode_primary(&self) -> Result<Decoded> {
        let id = self.primary_id()?;
        self.decode_with_alpha(id)
    }

    /// Decode item `id` (any image item: a thumbnail, a depth map, a hidden
    /// tile) with its own transforms and alpha.
    pub fn decode_with_alpha(&self, id: u32) -> Result<Decoded> {
        let image = self.decode_item(id)?;
        let alpha = match self.alpha_id(id) {
            Some(a) => {
                let ai = self.decode_item(a)?;
                if (ai.width, ai.height) != (image.width, image.height) {
                    return Err(Error::invalid(format!(
                        "heif: alpha is {}x{}, image is {}x{}",
                        ai.width, ai.height, image.width, image.height
                    )));
                }
                let bit_depth = ai.bit_depth;
                let plane = ai
                    .planes
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| Plane::filled(0, 0, 0));
                Some(Alpha {
                    plane,
                    bit_depth,
                    premultiplied: self.file.refs_from(id, b"prem").contains(&a),
                })
            }
            None => None,
        };
        Ok(Decoded {
            image,
            alpha,
            colour: self.colour(id),
        })
    }

    /// Decode item `id` to an [`Image`] (transforms applied unless disabled).
    pub fn decode_item(&self, id: u32) -> Result<Image> {
        self.decode_at(id, 0)
    }

    /// The decoded depth map of the primary image, if it has one.
    pub fn decode_depth(&self) -> Option<Result<Image>> {
        let id = self.primary_id().ok()?;
        self.depth_id(id).map(|d| self.decode_item(d))
    }

    /// The primary image's thumbnails (item ids, smallest first).
    pub fn thumbnails(&self) -> Vec<u32> {
        self.primary_id()
            .map(|id| self.thumbnail_ids(id))
            .unwrap_or_default()
    }

    /// The Exif item describing the primary image (else any Exif item), raw:
    /// a 4-byte big-endian offset to the TIFF header, then the Exif block.
    pub fn exif(&self) -> Option<Cow<'a, [u8]>> {
        let id = self.metadata_item(|it| &it.item_type == b"Exif")?;
        self.file.item_data(id).ok()
    }

    /// [`exif`](Self::exif) without its offset prefix: the TIFF header
    /// (`II*\0` / `MM\0*`) onwards.
    pub fn exif_tiff(&self) -> Option<Cow<'a, [u8]>> {
        let raw = self.exif()?;
        let off = u32::from_be_bytes(raw.get(..4)?.try_into().ok()?) as usize;
        let start = 4usize.checked_add(off)?;
        if start > raw.len() {
            return None;
        }
        Some(match raw {
            Cow::Borrowed(b) => Cow::Borrowed(&b[start..]),
            Cow::Owned(v) => Cow::Owned(v[start..].to_vec()),
        })
    }

    /// The XMP packet (a `mime` item of type `application/rdf+xml`).
    pub fn xmp(&self) -> Option<Cow<'a, [u8]>> {
        let id = self.metadata_item(|it| {
            &it.item_type == b"mime" && it.content_type.as_deref() == Some("application/rdf+xml")
        })?;
        self.file.item_data(id).ok()
    }

    /// True if the file carries an HDR gain map (reported, not applied).
    pub fn has_gain_map(&self) -> bool {
        self.file
            .items
            .iter()
            .any(|it| &it.item_type == b"tmap" || self.file.aux_type(it.id) == Some(APPLE_GAIN_MAP))
    }

    // -- internals ----------------------------------------------------------

    fn metadata_item(&self, pred: impl Fn(&rff_format::heif::Item) -> bool) -> Option<u32> {
        let primary = self.file.primary;
        let described = |it: &&&rff_format::heif::Item| {
            primary.is_some_and(|p| self.file.refs_from(it.id, b"cdsc").contains(&p))
        };
        let candidates: Vec<_> = self.file.items.iter().filter(|it| pred(it)).collect();
        candidates
            .iter()
            .find(described)
            .or_else(|| candidates.first())
            .map(|it| it.id)
    }

    fn aux_of(&self, id: u32, want: impl Fn(&str) -> bool) -> Option<u32> {
        self.file
            .refs_to(id, b"auxl")
            .into_iter()
            .find(|&a| a != id && self.file.aux_type(a).is_some_and(&want))
    }

    fn alpha_id(&self, id: u32) -> Option<u32> {
        self.aux_of(id, rff_format::heif::is_alpha_urn)
    }

    fn depth_id(&self, id: u32) -> Option<u32> {
        self.aux_of(id, rff_format::heif::is_depth_urn)
    }

    fn thumbnail_ids(&self, id: u32) -> Vec<u32> {
        let mut t: Vec<(u64, u32)> = self
            .file
            .refs_to(id, b"thmb")
            .into_iter()
            .filter(|&t| t != id)
            .map(|t| {
                let (w, h) = self.file.ispe(t).unwrap_or((0, 0));
                (w as u64 * h as u64, t)
            })
            .collect();
        t.sort_unstable();
        t.into_iter().map(|(_, id)| id).collect()
    }

    fn colour(&self, id: u32) -> ColourInfo {
        let mut c = ColourInfo::default();
        // A grid usually carries `colr` itself; if not, its first tile does.
        let mut chain = vec![id];
        if let Some(&first) = self.file.refs_from(id, b"dimg").first() {
            chain.push(first);
        }
        for &item in &chain {
            for (p, _) in self.file.properties_of(item) {
                match p {
                    Property::Colr(Colour::Nclx {
                        primaries,
                        transfer,
                        matrix,
                        full_range,
                    }) if c.nclx.is_none() => {
                        c.nclx = Some(Nclx {
                            primaries: *primaries,
                            transfer: *transfer,
                            matrix: *matrix,
                            full_range: *full_range,
                        })
                    }
                    Property::Colr(Colour::Icc(icc)) if c.icc.is_none() => {
                        c.icc = Some(icc.to_vec())
                    }
                    _ => {}
                }
            }
        }
        if c.nclx.is_none() {
            if let Some(info) = self
                .coded_item(id, 0)
                .and_then(|i| self.file.hvcc(i))
                .and_then(|h| hevc::hvc_info(h).ok())
            {
                if let Some((primaries, transfer, matrix, full_range)) = info.vui {
                    c.nclx = Some(Nclx {
                        primaries,
                        transfer,
                        matrix,
                        full_range,
                    });
                    c.nclx_from_bitstream = true;
                }
            }
        }
        c
    }

    /// The first coded (`hvc1`) item reached through `dimg`.
    fn coded_item(&self, id: u32, depth: u32) -> Option<u32> {
        if depth > self.opts.limits.max_derivation_depth {
            return None;
        }
        match &self.file.item(id)?.item_type {
            b"hvc1" => Some(id),
            _ => {
                let first = *self.file.refs_from(id, b"dimg").first()?;
                self.coded_item(first, depth + 1)
            }
        }
    }

    /// Size before transforms, chroma and bit depth, from the file alone.
    fn geometry(&self, id: u32, depth: u32) -> Result<(u32, u32, Chroma, u8)> {
        self.check_depth(depth)?;
        let item = self.item(id)?;
        match &item.item_type {
            b"hvc1" => {
                let info = hevc::hvc_info(self.file.hvcc(id).ok_or_else(|| {
                    Error::invalid(format!("heif: hvc1 item {id} has no `hvcC`"))
                })?)?;
                let (w, h) = self.file.ispe(id).unwrap_or((info.width, info.height));
                Ok((w, h, info.chroma, info.bit_depth))
            }
            b"grid" => {
                let g = self.file.grid(id)?;
                let first = *self
                    .file
                    .refs_from(id, b"dimg")
                    .first()
                    .ok_or_else(|| Error::invalid("heif: grid has no tiles"))?;
                let (_, _, c, b) = self.geometry(first, depth + 1)?;
                Ok((g.output_width, g.output_height, c, b))
            }
            b"iden" => {
                let src = self.single_input(id)?;
                let (w, h, c, b) = self.geometry(src, depth + 1)?;
                let (w, h) = self.transformed_size(src, w, h);
                Ok((w, h, c, b))
            }
            t => Err(unsupported_item(id, t)),
        }
    }

    fn transformed_size(&self, id: u32, mut w: u32, mut h: u32) -> (u32, u32) {
        for (p, _) in self.file.properties_of(id) {
            match p {
                Property::Irot(r) if r & 1 == 1 => std::mem::swap(&mut w, &mut h),
                Property::Clap(c) => {
                    if let Some((_, _, cw, ch)) = c.rect(w, h) {
                        (w, h) = (cw, ch);
                    }
                }
                _ => {}
            }
        }
        (w, h)
    }

    /// Worker threads to use: [`DecodeOptions::threads`], 0 meaning all cores.
    fn threads(&self) -> usize {
        match self.opts.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        }
    }

    fn item(&self, id: u32) -> Result<&rff_format::heif::Item> {
        self.file
            .item(id)
            .ok_or_else(|| Error::invalid(format!("heif: no item {id}")))
    }

    fn check_depth(&self, depth: u32) -> Result<()> {
        if depth > self.opts.limits.max_derivation_depth {
            return Err(Error::invalid(
                "heif: derived-image chain too deep (or a cycle)",
            ));
        }
        Ok(())
    }

    fn single_input(&self, id: u32) -> Result<u32> {
        match self.file.refs_from(id, b"dimg").as_slice() {
            [one] if *one != id => Ok(*one),
            _ => Err(Error::invalid(format!(
                "heif: derived item {id} needs exactly one input"
            ))),
        }
    }

    /// Refuse an item with an essential property this crate does not apply,
    /// and more than one of any transform (as libheif does).
    fn check_properties(&self, id: u32) -> Result<()> {
        let (mut rot, mut mir, mut clap) = (0, 0, 0);
        for (p, essential) in self.file.properties_of(id) {
            match p {
                Property::Irot(_) => rot += 1,
                Property::Imir(_) => mir += 1,
                Property::Clap(_) => clap += 1,
                Property::Other(t) if essential => {
                    return Err(Error::unsupported(format!(
                        "heif: item {id} has essential property `{}`",
                        String::from_utf8_lossy(t)
                    )))
                }
                _ => {}
            }
        }
        if rot > 1 || mir > 1 || clap > 1 {
            return Err(Error::invalid(format!(
                "heif: item {id} has more than one irot, imir or clap"
            )));
        }
        Ok(())
    }

    fn decode_at(&self, id: u32, depth: u32) -> Result<Image> {
        self.check_depth(depth)?;
        self.check_properties(id)?;
        let item = self.item(id)?;
        let mut img = match &item.item_type {
            b"hvc1" => hevc::frame_to_image(&hevc::decode_frame(&self.file, id)?)?,
            b"grid" => self.decode_grid(id, depth)?,
            b"iden" => self.decode_at(self.single_input(id)?, depth + 1)?,
            t => return Err(unsupported_item(id, t)),
        };
        if self.opts.apply_transforms {
            for (p, _) in self.file.properties_of(id) {
                match p {
                    // `irot` 0 (which Apple writes on every image) is the
                    // identity: rotating would only copy the whole image.
                    Property::Irot(0) => {}
                    Property::Irot(r) => img = img.rotate_ccw_threads(*r, self.threads()),
                    Property::Imir(a) => img.mirror(*a),
                    Property::Clap(c) => {
                        let (x, y, w, h) = c
                            .rect(img.width, img.height)
                            .ok_or_else(|| Error::invalid("heif: invalid clean aperture"))?;
                        img = img.crop(x, y, w, h)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(img)
    }

    fn decode_grid(&self, id: u32, depth: u32) -> Result<Image> {
        match self.compose_grid(id, depth, true)? {
            Some(img) => Ok(img),
            // A tile that does not exactly fill its cell (a malformed file):
            // compose again on the neutral canvas, which is what shows
            // through wherever no tile writes, serially.
            None => self
                .compose_grid(id, depth, false)?
                .ok_or_else(|| Error::invalid("heif: grid composition failed")),
        }
    }

    /// Decode grid `id` and place its tiles. With `zeroed`, the canvas is
    /// left unfilled and the composition gives up (`Ok(None)`) at the first
    /// tile whose decoded size is not exactly its cell's: only when every
    /// tile fills its cell exactly do the cells partition the canvas (luma
    /// and chroma -- the cell size is a multiple of the subsampling), so that
    /// every sample is written once and the fill would never be seen. Without
    /// `zeroed` the canvas is the neutral [`Image::blank`], the tiles are
    /// decoded and placed in order on the calling thread (so overlapping
    /// tiles give one answer), and it never gives up.
    fn compose_grid(&self, id: u32, depth: u32, zeroed: bool) -> Result<Option<Image>> {
        let g = self.file.grid(id)?;
        let tiles = self.file.refs_from(id, b"dimg");
        if tiles.len() as u64 != g.rows as u64 * g.columns as u64 {
            return Err(Error::invalid(format!(
                "heif: grid {}x{} has {} tiles",
                g.columns,
                g.rows,
                tiles.len()
            )));
        }
        if tiles.contains(&id) {
            return Err(Error::invalid("heif: grid refers to itself"));
        }
        // Every tile has the first tile's size (ISO/IEC 23008-12 §6.6.2.3.1).
        let (tw, th, chroma, bit_depth) = self.geometry(tiles[0], depth + 1)?;
        let (tw, th) = if self.opts.apply_transforms {
            self.transformed_size(tiles[0], tw, th)
        } else {
            (tw, th)
        };
        if (tw as u64) * (g.columns as u64) < g.output_width as u64
            || (th as u64) * (g.rows as u64) < g.output_height as u64
        {
            return Err(Error::invalid("heif: grid tiles do not cover the output"));
        }
        let (sx, sy) = chroma.shift();
        if (tw & ((1 << sx) - 1)) != 0 || (th & ((1 << sy) - 1)) != 0 {
            return Err(Error::unsupported(
                "heif: grid tiles not aligned to the chroma subsampling",
            ));
        }
        let mut out = if zeroed {
            Image::zeroed(g.output_width, g.output_height, chroma, bit_depth)
        } else {
            Image::blank(g.output_width, g.output_height, chroma, bit_depth)
        };
        let misfit = |tile: &Tile| zeroed && tile.size() != (tw, th);
        let place =
            |i: usize| -> (u32, u32) { ((i as u32 % g.columns) * tw, (i as u32 / g.columns) * th) };

        // A tile that is a plain `hvc1` item (no transforms) is copied straight
        // from the decoder's picture into the output; anything else goes
        // through a full item decode.
        let plain = |t: u32| -> bool {
            self.file
                .item(t)
                .map(|it| &it.item_type == b"hvc1")
                .unwrap_or(false)
                && !self.file.properties_of(t).any(|(p, _)| {
                    matches!(p, Property::Irot(_) | Property::Imir(_) | Property::Clap(_))
                })
        };
        let decode_tile = |dec: &mut hevc::ItemDecoder, t: u32| -> Result<Tile> {
            if plain(t) {
                self.check_properties(t)?;
                Ok(Tile::Frame(dec.decode(&self.file, t)?))
            } else {
                Ok(Tile::Image(self.decode_at(t, depth + 1)?))
            }
        };
        // The fallback places its tiles on this thread, in order: tiles that
        // miss their cells may overlap, and overlapping tiles placed in the
        // order the workers finish gave a different picture run to run.
        let threads = if zeroed {
            self.threads().min(tiles.len())
        } else {
            1
        };

        if threads <= 1 {
            let mut dec = hevc::ItemDecoder::new();
            for (i, &t) in tiles.iter().enumerate() {
                let (x, y) = place(i);
                let tile = decode_tile(&mut dec, t)?;
                if misfit(&tile) {
                    return Ok(None);
                }
                tile.blit(&mut out, x, y)?;
            }
            return Ok(Some(out));
        }
        // Workers decode, this thread places: tiles are independent pictures,
        // and the copy into the output is cheap next to the decode.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut gave_up = false;
        let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, Result<Tile>)>(threads * 2);
        std::thread::scope(|s| -> Result<()> {
            for _ in 0..threads {
                let tx = tx.clone();
                let next = &next;
                let tiles = &tiles;
                let decode_tile = &decode_tile;
                s.spawn(move || {
                    let mut dec = hevc::ItemDecoder::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= tiles.len() {
                            break;
                        }
                        let r = decode_tile(&mut dec, tiles[i]);
                        let failed = r.is_err();
                        if tx.send((i, r)).is_err() || failed {
                            // Stop the other workers early on the first error.
                            next.store(tiles.len(), std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut first_err = None;
            for (i, r) in rx {
                match r {
                    Ok(tile) if first_err.is_none() && !gave_up => {
                        if misfit(&tile) {
                            gave_up = true;
                            next.store(tiles.len(), std::sync::atomic::Ordering::Relaxed);
                            continue;
                        }
                        let (x, y) = place(i);
                        if let Err(e) = tile.blit(&mut out, x, y) {
                            first_err = Some(e);
                            next.store(tiles.len(), std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
            first_err.map_or(Ok(()), Err)
        })?;
        Ok((!gave_up).then_some(out))
    }
}

/// A decoded grid tile: a decoder picture (copied straight into the output)
/// or a fully derived image.
enum Tile {
    Frame(rusty_h265::Frame),
    Image(Image),
}

impl Tile {
    /// The decoded size.
    fn size(&self) -> (u32, u32) {
        match self {
            Tile::Frame(f) => hevc::frame_size(f),
            Tile::Image(img) => (img.width, img.height),
        }
    }

    fn blit(self, out: &mut Image, x: u32, y: u32) -> Result<()> {
        match self {
            Tile::Frame(f) => hevc::blit_frame(&f, out, x, y),
            Tile::Image(img) => {
                if img.chroma != out.chroma || img.bit_depth != out.bit_depth {
                    return Err(Error::invalid(
                        "heif: tiles disagree on chroma format or bit depth",
                    ));
                }
                out.blit(&img, x, y);
                Ok(())
            }
        }
    }
}

fn unsupported_item(id: u32, t: &[u8; 4]) -> Error {
    let name = String::from_utf8_lossy(t);
    match t {
        b"av01" => Error::unsupported(format!("heif: item {id} is AV1 (open it as AVIF)")),
        b"iovl" => Error::unsupported(format!("heif: item {id} is an overlay (`iovl`)")),
        _ => Error::unsupported(format!("heif: item {id} has unsupported type `{name}`")),
    }
}
