//! Writing EXR files.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::compression::{self, count_sampled, sampled, BlockGeom, ChanGeom};
use crate::decode::Threads;
use crate::error::{Error, Result};
use crate::header::{
    write_attribute, AttributeValue, Box2i, Compression, LevelMode, LineOrder, RoundingMode,
    TileDesc,
};
use crate::image::{ChannelRef, Image, ImageRef, SamplesRef};

/// Options for [`encode`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeOptions {
    pub compression: Compression,
    /// Chunk order: `Increasing` (the default) or `Decreasing`; tiled files
    /// also accept `Random` (the `exr` crate's `LineOrder::Unspecified`),
    /// whose chunks this writer stores in increasing order.
    pub line_order: LineOrder,
    pub threads: Threads,
    /// Deflate implementation for ZIP, ZIPS and PXR24.
    pub deflate: Deflate,
    /// DWAA/DWAB compression level. `None` uses the default (45) and
    /// writes no attribute; `Some(level)` also stores a
    /// `dwaCompressionLevel` attribute (as the `exr` crate does).
    pub dwa_level: Option<f32>,
    /// `Some((width, height))` writes a tiled file (one resolution level)
    /// with tiles of this size; `None` (the default) writes scanlines.
    pub tiles: Option<(u32, u32)>,
}

/// Which deflate implementation the ZIP, ZIPS and PXR24 writers use (both
/// at level 4; any EXR reader decodes either).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Deflate {
    /// miniz_oxide, as the `exr` crate uses: files are byte-identical to
    /// the ones `exr` writes for the same image.
    #[default]
    ExrCompatible,
    /// zlib-rs: about twice as fast as miniz_oxide on image data and
    /// slightly smaller output; the bytes differ from `exr`'s.
    Fast,
}

impl Default for EncodeOptions {
    /// ZIP (16 lines per block), increasing line order, all cores.
    fn default() -> Self {
        Self {
            compression: Compression::Zip,
            line_order: LineOrder::Increasing,
            threads: Threads::Auto,
            deflate: Deflate::ExrCompatible,
            dwa_level: None,
            tiles: None,
        }
    }
}

impl EncodeOptions {
    /// Options with this compression and the other defaults.
    #[must_use]
    pub fn new(compression: Compression) -> Self {
        Self {
            compression,
            ..Self::default()
        }
    }
}

/// Attributes this writer derives itself; any copies in the image header
/// are ignored.
const MANAGED: &[&str] = &[
    "channels",
    "compression",
    "dataWindow",
    "displayWindow",
    "lineOrder",
    "pixelAspectRatio",
    "screenWindowCenter",
    "screenWindowWidth",
    "chunkCount",
    "type",
    "tiles",
    "version",
    "maxSamplesPerPixel",
    "dwaCompressionLevel",
];

/// Optional standard attributes in the order the `exr` crate writes them
/// (with the type each must have to be treated as standard).
const KNOWN: &[(&str, &str)] = &[
    ("name", "string"),
    ("whiteLuminance", "float"),
    ("adoptedNeutral", "v2f"),
    ("renderingTransform", "string"),
    ("lookModTransform", "string"),
    ("xDensity", "float"),
    ("owner", "string"),
    ("comments", "string"),
    ("capDate", "string"),
    ("utcOffset", "float"),
    ("longitude", "float"),
    ("latitude", "float"),
    ("altitude", "float"),
    ("focus", "float"),
    ("expTime", "float"),
    ("aperture", "float"),
    ("isoSpeed", "float"),
    ("envmap", "envmap"),
    ("keyCode", "keycode"),
    ("timeCode", "timecode"),
    ("wrapmodes", "string"),
    ("framesPerSecond", "rational"),
    ("multiView", "stringvector"),
    ("worldToCamera", "m44f"),
    ("worldToNDC", "m44f"),
    ("deepImageState", "rational"),
    ("originalDataWindow", "box2i"),
    ("chromaticities", "chromaticities"),
    ("preview", "preview"),
    ("view", "string"),
    ("near", "float"),
    ("far", "float"),
    ("fieldOfViewHorizontal", "float"),
    ("fieldOfViewVertical", "float"),
    ("software", "string"),
];

/// Encode an image as a single-part EXR file: scanlines, or one-level tiles
/// when [`EncodeOptions::tiles`] is set. Every compression can be written.
///
/// Channels are written in name order (the file format requires it),
/// whatever their order in `image.channels`. Header attributes other than
/// the structural ones are copied from `image.header`.
pub fn encode(image: &Image, opts: &EncodeOptions) -> Result<Vec<u8>> {
    let channels: Vec<ChannelRef<'_>> = image.channels.iter().map(ChannelRef::from).collect();
    encode_channels(&image.header, image.data_window, &channels, opts)
}

/// [`encode`] from borrowed planes: the same file, byte for byte, without
/// first copying the samples into an [`Image`].
pub fn encode_ref(image: &ImageRef<'_>, opts: &EncodeOptions) -> Result<Vec<u8>> {
    encode_channels(&image.header, image.data_window, &image.channels, opts)
}

fn encode_channels(
    header: &crate::header::Header,
    dw: Box2i,
    image_channels: &[ChannelRef<'_>],
    opts: &EncodeOptions,
) -> Result<Vec<u8>> {
    let comp = opts.compression;
    if opts.tiles.is_none() && opts.line_order == LineOrder::Random {
        return Err(Error::usage(
            "scanline files need increasing or decreasing line order",
        ));
    }
    let dwa_level = opts.dwa_level.unwrap_or(45.0);
    if !dwa_level.is_finite() || dwa_level < 0.0 {
        return Err(Error::usage(format!("DWA level {dwa_level}")));
    }
    if let Some((tw, th)) = opts.tiles {
        if tw == 0 || th == 0 || tw > i32::MAX as u32 || th > i32::MAX as u32 {
            return Err(Error::usage(format!("tile size {tw}x{th}")));
        }
    }
    let (w, h) = (dw.width(), dw.height());
    if w <= 0 || h <= 0 || w > i64::from(i32::MAX) || h > i64::from(i32::MAX) {
        return Err(Error::usage(format!("data window {dw:?}")));
    }
    let (w, h) = (w as usize, h as usize);
    if image_channels.is_empty() {
        return Err(Error::usage("an image needs at least one channel"));
    }
    let mut order: Vec<usize> = (0..image_channels.len()).collect();
    order.sort_by(|&a, &b| {
        image_channels[a]
            .desc
            .name
            .as_bytes()
            .cmp(image_channels[b].desc.name.as_bytes())
    });
    for k in 0..order.len() {
        let c = &image_channels[order[k]];
        let d = &c.desc;
        if d.name.is_empty() || d.name.len() > 255 || d.name.as_bytes().contains(&0) {
            return Err(Error::usage(format!("channel name {:?}", d.name)));
        }
        if k > 0 && image_channels[order[k - 1]].desc.name == d.name {
            return Err(Error::usage(format!("duplicate channel {}", d.name)));
        }
        if d.sample_type != c.samples.sample_type() {
            return Err(Error::usage(format!("channel {} type mismatch", d.name)));
        }
        let (xs, ys) = (d.x_sampling, d.y_sampling);
        if opts.tiles.is_some() && (xs != 1 || ys != 1) {
            return Err(Error::usage(format!(
                "channel {}: tiled files cannot hold subsampled channels",
                d.name
            )));
        }
        if xs < 1
            || ys < 1
            || dw.x_min.rem_euclid(xs) != 0
            || dw.y_min.rem_euclid(ys) != 0
            || w % xs as usize != 0
            || h % ys as usize != 0
        {
            return Err(Error::usage(format!(
                "channel {} sampling {xs}x{ys}",
                d.name
            )));
        }
        let (cw, ch) = (
            count_sampled(dw.x_min, w, xs),
            count_sampled(dw.y_min, h, ys),
        );
        if c.width != cw || c.height != ch || c.samples.len() != cw * ch {
            return Err(Error::usage(format!(
                "channel {} holds {}x{} ({} samples), expected {cw}x{ch}",
                d.name,
                c.width,
                c.height,
                c.samples.len()
            )));
        }
    }
    let channels: Vec<&ChannelRef<'_>> = order.iter().map(|&i| &image_channels[i]).collect();
    // Chunk grid: a scanline block is a tile as wide as the image.
    let (tw, th) = match opts.tiles {
        Some((x, y)) => (x as usize, y as usize),
        None => (w, comp.lines_per_block() as usize),
    };
    let ntx = w.div_ceil(tw);
    let nchunks = ntx * h.div_ceil(th);
    if nchunks > i32::MAX as usize {
        return Err(Error::usage("too many chunks"));
    }

    // Header.
    let display = match header.get("displayWindow") {
        Some(AttributeValue::Box2i(b)) => *b,
        _ => dw,
    };
    let hget = |n: &str| header.get(n);
    let pixel_aspect = match hget("pixelAspectRatio") {
        Some(AttributeValue::Float(f)) => *f,
        _ => 1.0,
    };
    let swc = match hget("screenWindowCenter") {
        Some(AttributeValue::V2f(v)) => *v,
        _ => [0.0, 0.0],
    };
    let sww = match hget("screenWindowWidth") {
        Some(AttributeValue::Float(f)) => *f,
        _ => 1.0,
    };
    let mut attrs: Vec<(String, AttributeValue)> = vec![
        ("chunkCount".into(), AttributeValue::Int(nchunks as i32)),
        (
            "channels".into(),
            AttributeValue::Channels(channels.iter().map(|c| c.desc.clone()).collect()),
        ),
        ("compression".into(), AttributeValue::Compression(comp)),
        (
            "lineOrder".into(),
            AttributeValue::LineOrder(opts.line_order),
        ),
        ("displayWindow".into(), AttributeValue::Box2i(display)),
        (
            "pixelAspectRatio".into(),
            AttributeValue::Float(pixel_aspect),
        ),
        ("screenWindowCenter".into(), AttributeValue::V2f(swc)),
        ("screenWindowWidth".into(), AttributeValue::Float(sww)),
        ("dataWindow".into(), AttributeValue::Box2i(dw)),
        (
            "type".into(),
            AttributeValue::Text(if opts.tiles.is_some() {
                b"tiledimage".to_vec()
            } else {
                b"scanlineimage".to_vec()
            }),
        ),
    ];
    if let Some((x, y)) = opts.tiles {
        attrs.push((
            "tiles".into(),
            AttributeValue::TileDesc(TileDesc {
                x_size: x,
                y_size: y,
                level_mode: LevelMode::One,
                rounding: RoundingMode::Down,
            }),
        ));
    }
    if let (Some(level), Compression::Dwaa | Compression::Dwab) = (opts.dwa_level, comp) {
        attrs.push(("dwaCompressionLevel".into(), AttributeValue::Float(level)));
    }
    let is_known = |a: &crate::header::Attribute| {
        KNOWN
            .iter()
            .any(|(n, t)| *n == a.name && a.value.type_name() == *t)
    };
    for (n, _) in KNOWN {
        if let Some(a) = header.attributes.iter().find(|a| a.name == *n) {
            if is_known(a) {
                attrs.push((a.name.clone(), a.value.clone()));
            }
        }
    }
    for a in &header.attributes {
        if MANAGED.contains(&a.name.as_str()) || is_known(a) {
            continue;
        }
        attrs.push((a.name.clone(), a.value.clone()));
    }
    let mut long = false;
    for (n, v) in &attrs {
        if n.is_empty() || n.len() > 255 || v.type_name().len() > 255 || n.as_bytes().contains(&0) {
            return Err(Error::usage(format!("attribute name {n:?}")));
        }
        if n.len() > 31 || v.type_name().len() > 31 {
            long = true;
        }
    }
    if channels.iter().any(|c| c.desc.name.len() > 31) {
        long = true;
    }

    let mut out = Vec::new();
    out.extend_from_slice(&20_000_630u32.to_le_bytes());
    let tiled_flag = if opts.tiles.is_some() { 0x200 } else { 0 };
    let long_flag = if long { 0x400 } else { 0 };
    out.extend_from_slice(&(2u32 | tiled_flag | long_flag).to_le_bytes());
    for (n, v) in &attrs {
        write_attribute(&mut out, n, v);
    }
    out.push(0);
    let table_pos = out.len();
    out.resize(table_pos + nchunks * 8, 0);

    // Chunks.
    let tiled = opts.tiles.is_some();
    let compress_one = |i: usize, work: &mut compression::Work| -> Result<Vec<u8>> {
        let (tx, ty) = (i % ntx, i / ntx);
        let x_off = tx * tw;
        let width = tw.min(w - x_off);
        let y0 = dw.y_min + (ty * th) as i32;
        let lines = th.min(h - ty * th);
        let mut chans = Vec::with_capacity(channels.len());
        let mut raw_size = 0usize;
        for c in &channels {
            // Tiled channels are never subsampled, so the tile width is the
            // sample count; a scanline block spans the whole row.
            let cw = if tiled { width } else { c.width };
            let ch = count_sampled(y0, lines, c.desc.y_sampling);
            raw_size += cw * ch * c.desc.sample_type.bytes();
            chans.push(ChanGeom {
                name: &c.desc.name,
                ty: c.desc.sample_type,
                p_linear: c.desc.p_linear,
                width: cw,
                height: ch,
                x_sampling: c.desc.x_sampling,
                y_sampling: c.desc.y_sampling,
            });
        }
        let geom = BlockGeom {
            y0,
            lines,
            width,
            chans,
            raw_size,
        };
        // The worker's raw buffer, reused: every byte below is written. An
        // uncompressed chunk is built in a fresh buffer that is stored as is
        // (`vec!` takes the allocator's zeroed path, cheaper than `resize`).
        let mut buf = if comp == Compression::None {
            vec![0u8; raw_size]
        } else {
            std::mem::take(&mut work.raw)
        };
        let raw = compression::grow(&mut buf, raw_size);
        let mut o = 0usize;
        for line in 0..lines {
            let y = y0 + line as i32;
            for c in &channels {
                let ys = c.desc.y_sampling;
                if !sampled(y, ys) {
                    continue;
                }
                let row = ((i64::from(y) - i64::from(dw.y_min)) / i64::from(ys)) as usize;
                let (cx, cw) = if tiled { (x_off, width) } else { (0, c.width) };
                let (a, b) = (row * c.width + cx, row * c.width + cx + cw);
                let n = cw * c.desc.sample_type.bytes();
                let dst = &mut raw[o..o + n];
                o += n;
                match c.samples {
                    SamplesRef::F16(v) => {
                        for (d, s) in dst.chunks_exact_mut(2).zip(&v[a..b]) {
                            d.copy_from_slice(&s.to_le_bytes());
                        }
                    }
                    SamplesRef::F32(v) => {
                        for (d, s) in dst.chunks_exact_mut(4).zip(&v[a..b]) {
                            d.copy_from_slice(&s.to_le_bytes());
                        }
                    }
                    SamplesRef::U32(v) => {
                        for (d, s) in dst.chunks_exact_mut(4).zip(&v[a..b]) {
                            d.copy_from_slice(&s.to_le_bytes());
                        }
                    }
                }
            }
        }
        if comp == Compression::None {
            return Ok(buf);
        }
        let packed = compression::compress(comp, &geom, raw, opts.deflate, dwa_level, work);
        work.raw = buf;
        packed
    };

    let nthreads = opts.threads.resolve().min(nchunks).max(1);
    let mut packed: Vec<Option<Vec<u8>>> = vec![None; nchunks];
    if nthreads == 1 {
        let mut work = compression::Work::default();
        for (i, slot) in packed.iter_mut().enumerate() {
            *slot = Some(compress_one(i, &mut work)?);
        }
    } else {
        let next = AtomicUsize::new(0);
        let results: Vec<Vec<(usize, Result<Vec<u8>>)>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..nthreads)
                .map(|_| {
                    s.spawn(|| {
                        let mut v = Vec::new();
                        let mut work = compression::Work::default();
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            if k >= nchunks {
                                break;
                            }
                            v.push((k, compress_one(k, &mut work)));
                        }
                        v
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("encoder worker panicked"))
                .collect()
        });
        let mut all: Vec<(usize, Result<Vec<u8>>)> = results.into_iter().flatten().collect();
        all.sort_by_key(|(k, _)| *k);
        for (k, r) in all {
            packed[k] = Some(r?);
        }
    }

    let order: Vec<usize> = match opts.line_order {
        LineOrder::Decreasing => (0..nchunks).rev().collect(),
        _ => (0..nchunks).collect(),
    };
    let mut offsets = vec![0u64; nchunks];
    for i in order {
        let data = packed[i].take().expect("every chunk compressed");
        offsets[i] = out.len() as u64;
        if tiled {
            for v in [(i % ntx) as i32, (i / ntx) as i32, 0, 0] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        } else {
            let y0 = dw.y_min + ((i / ntx) * th) as i32;
            out.extend_from_slice(&y0.to_le_bytes());
        }
        out.extend_from_slice(&(data.len() as i32).to_le_bytes());
        out.extend_from_slice(&data);
    }
    for (i, o) in offsets.iter().enumerate() {
        out[table_pos + i * 8..table_pos + i * 8 + 8].copy_from_slice(&o.to_le_bytes());
    }
    Ok(out)
}

/// A data window helper for callers building images at an offset.
#[must_use]
pub fn window(x_min: i32, y_min: i32, width: u32, height: u32) -> Box2i {
    Box2i {
        x_min,
        y_min,
        x_max: x_min + width as i32 - 1,
        y_max: y_min + height as i32 - 1,
    }
}
