//! Reading EXR files: structure, chunks, and image assembly.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use crate::compression::{self, count_sampled, sampled, BlockGeom, ChanGeom};
use crate::error::{Error, Limits, Result};
use crate::header::{
    parse_header, Box2i, ChannelDesc, Compression, Cursor, Header, LevelMode, RoundingMode,
    SampleType, TileDesc,
};
use crate::image::{Channel, Image, Samples};

const MAGIC: u32 = 20_000_630;
const FLAG_TILED: u32 = 0x200;
const FLAG_LONG_NAMES: u32 = 0x400;
const FLAG_NON_IMAGE: u32 = 0x800;
const FLAG_MULTIPART: u32 = 0x1000;

/// Thread use for whole-image decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Threads {
    /// One worker per available core (the default).
    #[default]
    Auto,
    /// Decode on the calling thread only.
    Single,
    /// Exactly this many workers.
    Count(usize),
}

impl Threads {
    pub(crate) fn resolve(self) -> usize {
        match self {
            Self::Auto => {
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
            }
            Self::Single => 1,
            Self::Count(n) => n.max(1),
        }
    }
}

/// What kind of data a part stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Scanline,
    Tiled,
    DeepScanline,
    DeepTiled,
}

/// Level geometry of a tiled part.
#[derive(Debug, Clone)]
pub(crate) struct Levels {
    pub desc: TileDesc,
    /// Level widths (x levels) and heights (y levels).
    pub widths: Vec<usize>,
    pub heights: Vec<usize>,
    /// First chunk index of each level (ripmap: `ly * nx + lx`).
    pub first_chunk: Vec<usize>,
}

impl Levels {
    pub fn tiles_x(&self, lx: usize) -> usize {
        self.widths[lx].div_ceil(self.desc.x_size as usize)
    }
    pub fn tiles_y(&self, ly: usize) -> usize {
        self.heights[ly].div_ceil(self.desc.y_size as usize)
    }
    fn level_index(&self, lx: usize, ly: usize) -> Option<usize> {
        match self.desc.level_mode {
            LevelMode::One => (lx == 0 && ly == 0).then_some(0),
            LevelMode::Mipmap => (lx == ly && lx < self.widths.len()).then_some(lx),
            LevelMode::Ripmap => (lx < self.widths.len() && ly < self.heights.len())
                .then_some(ly * self.widths.len() + lx),
        }
    }
}

fn level_count(size: usize, r: RoundingMode) -> usize {
    // floor/ceil(log2(size)) + 1
    let mut n = 0usize;
    let mut s = size;
    while s > 1 {
        s >>= 1;
        n += 1;
    }
    if r == RoundingMode::Up && size > (1usize << n) {
        n += 1;
    }
    n + 1
}

fn level_size(size: usize, l: usize, r: RoundingMode) -> usize {
    let b = 1usize << l;
    let mut s = size / b;
    if r == RoundingMode::Up && s * b < size {
        s += 1;
    }
    s.max(1)
}

/// Validated essentials of one part.
#[derive(Debug, Clone)]
pub(crate) struct PartInfo {
    pub kind: PartKind,
    pub data_window: Box2i,
    pub width: usize,
    pub height: usize,
    pub channels: Vec<ChannelDesc>,
    pub compression: Compression,
    pub levels: Option<Levels>,
    pub chunk_count: usize,
}

impl PartInfo {
    pub fn from_header(h: &Header, kind: PartKind, limits: &Limits) -> Result<Self> {
        let dw = h
            .data_window()
            .ok_or_else(|| Error::invalid("missing dataWindow"))?;
        if h.display_window().is_none() {
            return Err(Error::invalid("missing displayWindow"));
        }
        let channels = h.channels().to_vec();
        if h.get("channels").is_none() {
            return Err(Error::invalid("missing channels"));
        }
        if h.get("compression").is_none() {
            return Err(Error::invalid("missing compression"));
        }
        if channels.len() > limits.max_channels {
            return Err(Error::limit(format!("{} channels", channels.len())));
        }
        let (w, hh) = (dw.width(), dw.height());
        if w <= 0 || hh <= 0 || w > i64::from(i32::MAX) || hh > i64::from(i32::MAX) {
            return Err(Error::invalid(format!("data window {dw:?}")));
        }
        let pixels = (w as u64) * (hh as u64);
        if pixels > limits.max_pixels {
            return Err(Error::limit(format!("{w}x{hh} data window")));
        }
        for pair in channels.windows(2) {
            if pair[0].name.as_bytes() >= pair[1].name.as_bytes() {
                return Err(Error::invalid(format!(
                    "channel list not sorted or duplicated at {}",
                    pair[1].name
                )));
            }
        }
        for c in &channels {
            if c.name.is_empty() {
                return Err(Error::invalid("empty channel name"));
            }
            let (xs, ys) = (i64::from(c.x_sampling), i64::from(c.y_sampling));
            if i64::from(dw.x_min) % xs != 0
                || i64::from(dw.y_min) % ys != 0
                || w % xs != 0
                || hh % ys != 0
            {
                return Err(Error::invalid(format!(
                    "channel {} sampling {xs}x{ys} does not divide the data window",
                    c.name
                )));
            }
        }
        let compression = h.compression();
        let (w, hh) = (w as usize, hh as usize);
        let mut levels = None;
        let chunk_count;
        match kind {
            PartKind::Scanline | PartKind::DeepScanline => {
                chunk_count = hh.div_ceil(compression.lines_per_block() as usize);
            }
            PartKind::Tiled | PartKind::DeepTiled => {
                let desc = h
                    .tiles()
                    .ok_or_else(|| Error::invalid("tiled part without tiles"))?;
                if desc.x_size == 0
                    || desc.y_size == 0
                    || desc.x_size > 1 << 24
                    || desc.y_size > 1 << 24
                {
                    return Err(Error::invalid(format!(
                        "tile size {}x{}",
                        desc.x_size, desc.y_size
                    )));
                }
                if u64::from(desc.x_size) * u64::from(desc.y_size) > limits.max_pixels {
                    return Err(Error::limit("tile size"));
                }
                if channels
                    .iter()
                    .any(|c| c.x_sampling != 1 || c.y_sampling != 1)
                {
                    return Err(Error::invalid("tiled part with subsampled channels"));
                }
                let (nx, ny) = match desc.level_mode {
                    LevelMode::One => (1, 1),
                    LevelMode::Mipmap => {
                        let n = level_count(w.max(hh), desc.rounding);
                        (n, n)
                    }
                    LevelMode::Ripmap => (
                        level_count(w, desc.rounding),
                        level_count(hh, desc.rounding),
                    ),
                };
                let widths: Vec<usize> = (0..nx).map(|l| level_size(w, l, desc.rounding)).collect();
                let heights: Vec<usize> =
                    (0..ny).map(|l| level_size(hh, l, desc.rounding)).collect();
                let mut lv = Levels {
                    desc,
                    widths,
                    heights,
                    first_chunk: Vec::new(),
                };
                let mut total = 0usize;
                let mut add = |lv: &Levels, lx: usize, ly: usize| -> usize {
                    let start = total;
                    total = total.saturating_add(lv.tiles_x(lx).saturating_mul(lv.tiles_y(ly)));
                    start
                };
                let mut firsts = Vec::new();
                match desc.level_mode {
                    LevelMode::One => firsts.push(add(&lv, 0, 0)),
                    LevelMode::Mipmap => {
                        for l in 0..nx {
                            firsts.push(add(&lv, l, l));
                        }
                    }
                    LevelMode::Ripmap => {
                        for ly in 0..ny {
                            for lx in 0..nx {
                                firsts.push(add(&lv, lx, ly));
                            }
                        }
                    }
                }
                lv.first_chunk = firsts;
                chunk_count = total;
                levels = Some(lv);
            }
        }
        if chunk_count as u64 > limits.max_chunks {
            return Err(Error::limit(format!("{chunk_count} chunks")));
        }
        if let Some(crate::header::AttributeValue::Int(n)) = h.get("chunkCount") {
            if *n < 0 || *n as usize != chunk_count {
                return Err(Error::invalid(format!(
                    "chunkCount {n} disagrees with the geometry ({chunk_count})"
                )));
            }
        }
        Ok(Self {
            kind,
            data_window: dw,
            width: w,
            height: hh,
            channels,
            compression,
            levels,
            chunk_count,
        })
    }
}

/// Where a chunk sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkPos {
    /// A scanline block starting at absolute line `y`.
    Scanline { y: i32 },
    /// A tile (`tile_x`, `tile_y`) of level (`level_x`, `level_y`).
    Tile {
        tile_x: usize,
        tile_y: usize,
        level_x: usize,
        level_y: usize,
    },
}

/// One decoded chunk in the raw layout, with its geometry.
#[derive(Debug, Clone)]
pub struct Block {
    pub part: usize,
    pub pos: ChunkPos,
    /// Pixel rectangle of the chunk (absolute coordinates; for levels > 0,
    /// relative to the data window origin scaled to that level).
    pub x0: i32,
    pub y0: i32,
    pub width: usize,
    pub height: usize,
    /// Per channel (channel-list order): samples per row and sample rows
    /// inside this chunk.
    pub channel_sizes: Vec<(usize, usize)>,
    pub(crate) sample_types: Vec<SampleType>,
    pub(crate) y_sampling: Vec<i32>,
    /// Raw decompressed bytes: for each line top to bottom, for each
    /// channel sampled on that line, its row of little-endian samples.
    pub data: Vec<u8>,
}

impl Block {
    /// Iterate `(channel index, absolute y, little-endian row bytes)` in the
    /// raw layout's order.
    pub fn rows(&self) -> impl Iterator<Item = (usize, i32, &[u8])> + '_ {
        let mut off = 0usize;
        (0..self.height)
            .flat_map(move |line| {
                let y = self.y0 + line as i32;
                (0..self.channel_sizes.len()).filter_map(move |c| {
                    if !sampled(y, self.y_sampling[c]) {
                        return None;
                    }
                    Some((c, y))
                })
            })
            .map(move |(c, y)| {
                let n = self.channel_sizes[c].0 * self.sample_types[c].bytes();
                let s = &self.data[off..off + n];
                off += n;
                (c, y, s)
            })
    }

    /// The samples of channel `c` on absolute line `y` (None if that line
    /// carries no samples for the channel or is outside the chunk).
    #[must_use]
    pub fn row(&self, c: usize, y: i32) -> Option<&[u8]> {
        self.rows()
            .find(|(ci, yy, _)| *ci == c && *yy == y)
            .map(|(_, _, s)| s)
    }

    /// Sample type of channel `c`.
    #[must_use]
    pub fn sample_type(&self, c: usize) -> SampleType {
        self.sample_types[c]
    }
}

struct Part {
    header: Header,
    info: PartInfo,
    offsets: Vec<u64>,
}

/// An EXR file opened for reading. Parsing reads the headers and offset
/// tables only; pixel data is decoded on request.
pub struct Decoder<'a> {
    data: &'a [u8],
    parts: Vec<Part>,
    multipart: bool,
    limits: Limits,
    threads: Threads,
}

impl<'a> Decoder<'a> {
    /// Open with default [`Limits`].
    pub fn new(data: &'a [u8]) -> Result<Self> {
        Self::with_limits(data, Limits::default())
    }

    /// Open with explicit limits.
    pub fn with_limits(data: &'a [u8], limits: Limits) -> Result<Self> {
        let mut c = Cursor::new(data);
        let magic = c
            .u32()
            .map_err(|_| Error::invalid("not an EXR file (too short)"))?;
        if magic != MAGIC {
            return Err(Error::invalid("not an EXR file (bad magic number)"));
        }
        let version = c.u32()?;
        if version & 0xff != 2 {
            return Err(Error::unsupported(format!(
                "file format version {}",
                version & 0xff
            )));
        }
        let flags = version & !0xff;
        if flags & !(FLAG_TILED | FLAG_LONG_NAMES | FLAG_NON_IMAGE | FLAG_MULTIPART) != 0 {
            return Err(Error::unsupported(format!("version flags {flags:#x}")));
        }
        let multipart = flags & FLAG_MULTIPART != 0;
        let mut headers = Vec::new();
        if multipart {
            loop {
                if c.remaining() > 0 && c.data[c.pos] == 0 {
                    c.pos += 1;
                    break;
                }
                if headers.len() >= limits.max_parts {
                    return Err(Error::limit("too many parts"));
                }
                headers.push(parse_header(&mut c, &limits)?);
            }
            if headers.is_empty() {
                return Err(Error::invalid("multi-part file with no parts"));
            }
        } else {
            headers.push(parse_header(&mut c, &limits)?);
        }
        let mut parts = Vec::with_capacity(headers.len());
        for h in headers {
            let kind = if multipart || flags & FLAG_NON_IMAGE != 0 {
                match h.part_type() {
                    Some("scanlineimage") => PartKind::Scanline,
                    Some("tiledimage") => PartKind::Tiled,
                    Some("deepscanline") => PartKind::DeepScanline,
                    Some("deeptile") => PartKind::DeepTiled,
                    Some(t) => return Err(Error::unsupported(format!("part type {t}"))),
                    None if !multipart && flags & FLAG_TILED != 0 => PartKind::DeepTiled,
                    None if !multipart => PartKind::DeepScanline,
                    None => return Err(Error::invalid("multi-part part without type")),
                }
            } else if flags & FLAG_TILED != 0 {
                PartKind::Tiled
            } else {
                PartKind::Scanline
            };
            if multipart && h.get("chunkCount").is_none() {
                return Err(Error::invalid("multi-part part without chunkCount"));
            }
            let info = PartInfo::from_header(&h, kind, &limits)?;
            parts.push(Part {
                header: h,
                info,
                offsets: Vec::new(),
            });
        }
        for p in &mut parts {
            let n = p.info.chunk_count;
            if n as u64 * 8 > c.remaining() as u64 {
                return Err(Error::invalid("offset table truncated"));
            }
            p.offsets = (0..n).map(|_| c.u64()).collect::<Result<_>>()?;
        }
        Ok(Self {
            data,
            parts,
            multipart,
            limits,
            threads: Threads::Auto,
        })
    }

    /// Set thread use for [`Self::decode_part`] and friends.
    #[must_use]
    pub fn threads(mut self, t: Threads) -> Self {
        self.threads = t;
        self
    }

    /// Number of parts.
    #[must_use]
    pub fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// Header of part `i`.
    #[must_use]
    pub fn header(&self, i: usize) -> Option<&Header> {
        self.parts.get(i).map(|p| &p.header)
    }

    /// Kind of part `i`.
    #[must_use]
    pub fn part_kind(&self, i: usize) -> Option<PartKind> {
        self.parts.get(i).map(|p| p.info.kind)
    }

    /// True for multi-part files.
    #[must_use]
    pub fn is_multipart(&self) -> bool {
        self.multipart
    }

    /// Resolution levels of part `i`: `(x levels, y levels)` (1x1 for
    /// scanline and single-level tiled parts).
    #[must_use]
    pub fn level_count(&self, i: usize) -> Option<(usize, usize)> {
        let p = self.parts.get(i)?;
        Some(
            match (
                &p.info.levels,
                p.info.levels.as_ref().map(|l| l.desc.level_mode),
            ) {
                (Some(l), Some(LevelMode::Ripmap)) => (l.widths.len(), l.heights.len()),
                (Some(l), Some(LevelMode::Mipmap)) => (l.widths.len(), l.widths.len()),
                _ => (1, 1),
            },
        )
    }

    /// Number of chunks in part `i`.
    #[must_use]
    pub fn chunk_count(&self, i: usize) -> Option<usize> {
        self.parts.get(i).map(|p| p.info.chunk_count)
    }

    fn part(&self, i: usize) -> Result<&Part> {
        let p = self
            .parts
            .get(i)
            .ok_or_else(|| Error::usage(format!("part {i} of {}", self.parts.len())))?;
        if matches!(p.info.kind, PartKind::DeepScanline | PartKind::DeepTiled) {
            return Err(Error::unsupported("deep data"));
        }
        Ok(p)
    }

    /// Position of chunk `index` of part `part`.
    fn chunk_pos(info: &PartInfo, index: usize) -> Result<ChunkPos> {
        match &info.levels {
            None => {
                let lpb = info.compression.lines_per_block() as usize;
                Ok(ChunkPos::Scanline {
                    y: info.data_window.y_min + (index * lpb) as i32,
                })
            }
            Some(lv) => {
                let (nx, ny) = (lv.widths.len(), lv.heights.len());
                for (li, &first) in lv.first_chunk.iter().enumerate().rev() {
                    if index >= first {
                        let (lx, ly) = match lv.desc.level_mode {
                            LevelMode::Ripmap => (li % nx, li / nx),
                            _ => (li, li),
                        };
                        let _ = ny;
                        let t = index - first;
                        let tx = lv.tiles_x(lx);
                        let (tile_x, tile_y) = (t % tx, t / tx);
                        if tile_y >= lv.tiles_y(ly) {
                            break;
                        }
                        return Ok(ChunkPos::Tile {
                            tile_x,
                            tile_y,
                            level_x: lx,
                            level_y: ly,
                        });
                    }
                }
                Err(Error::usage(format!("chunk {index}")))
            }
        }
    }

    fn chunk_rect(info: &PartInfo, pos: ChunkPos) -> (i32, i32, usize, usize) {
        match pos {
            ChunkPos::Scanline { y } => {
                let lpb = info.compression.lines_per_block() as i64;
                let lines =
                    (i64::from(info.data_window.y_max) - i64::from(y) + 1).min(lpb) as usize;
                (info.data_window.x_min, y, info.width, lines)
            }
            ChunkPos::Tile {
                tile_x,
                tile_y,
                level_x,
                level_y,
            } => {
                let lv = info.levels.as_ref().expect("tiled");
                let (tw, th) = (lv.desc.x_size as usize, lv.desc.y_size as usize);
                let (lw, lh) = (lv.widths[level_x], lv.heights[level_y]);
                let x = tile_x * tw;
                let y = tile_y * th;
                (
                    info.data_window.x_min + x as i32,
                    info.data_window.y_min + y as i32,
                    tw.min(lw - x),
                    th.min(lh - y),
                )
            }
        }
    }

    fn geom(info: &PartInfo, x0: i32, y0: i32, w: usize, lines: usize) -> Result<BlockGeom<'_>> {
        let mut chans = Vec::with_capacity(info.channels.len());
        let mut raw = 0u64;
        for c in &info.channels {
            let cw = count_sampled(x0, w, c.x_sampling);
            let ch = count_sampled(y0, lines, c.y_sampling);
            raw += (cw * ch * c.sample_type.bytes()) as u64;
            chans.push(ChanGeom {
                name: &c.name,
                ty: c.sample_type,
                p_linear: c.p_linear,
                width: cw,
                height: ch,
                x_sampling: c.x_sampling,
                y_sampling: c.y_sampling,
            });
        }
        Ok(BlockGeom {
            y0,
            lines,
            width: w,
            chans,
            raw_size: usize::try_from(raw).map_err(|_| Error::limit("chunk size"))?,
        })
    }

    /// Read and decompress chunk `index` of part `part`.
    pub fn decode_chunk(&self, part: usize, index: usize) -> Result<Block> {
        let p = self.part(part)?;
        if index >= p.info.chunk_count {
            return Err(Error::usage(format!(
                "chunk {index} of {}",
                p.info.chunk_count
            )));
        }
        let pos = Self::chunk_pos(&p.info, index)?;
        let (x0, y0, w, lines) = Self::chunk_rect(&p.info, pos);
        let geom = Self::geom(&p.info, x0, y0, w, lines)?;
        let off = p.offsets[index];
        if off >= self.data.len() as u64 || off < 8 {
            return Err(Error::invalid(format!(
                "chunk {index} offset {off} outside the file"
            )));
        }
        let mut c = Cursor::new(self.data);
        c.pos = off as usize;
        if self.multipart {
            let pn = c.i32()?;
            if pn < 0 || pn as usize != part {
                return Err(Error::invalid(format!(
                    "chunk {index} belongs to part {pn}"
                )));
            }
        }
        match pos {
            ChunkPos::Scanline { y } => {
                let fy = c.i32()?;
                if fy != y {
                    return Err(Error::invalid(format!(
                        "chunk {index} has y {fy}, expected {y}"
                    )));
                }
            }
            ChunkPos::Tile {
                tile_x,
                tile_y,
                level_x,
                level_y,
            } => {
                let v = [c.i32()?, c.i32()?, c.i32()?, c.i32()?];
                if v != [tile_x as i32, tile_y as i32, level_x as i32, level_y as i32] {
                    return Err(Error::invalid(format!("chunk {index} has tile {v:?}")));
                }
            }
        }
        let size = c.i32()?;
        if size < 0 || size as usize > c.remaining() {
            return Err(Error::invalid(format!("chunk {index} data size {size}")));
        }
        let packed = c.bytes(size as usize)?;
        let data = compression::decompress(
            p.info.compression,
            &geom,
            packed,
            self.limits.max_chunk_bytes,
        )?;
        Ok(Block {
            part,
            pos,
            x0,
            y0,
            width: w,
            height: lines,
            channel_sizes: geom.chans.iter().map(|g| (g.width, g.height)).collect(),
            sample_types: geom.chans.iter().map(|g| g.ty).collect(),
            y_sampling: geom.chans.iter().map(|g| g.y_sampling).collect(),
            data,
        })
    }

    /// Decode part `i` (level 0 for multi-level tiled parts).
    pub fn decode_part(&self, i: usize) -> Result<Image> {
        self.decode_level(i, 0, 0)
    }

    /// Decode one resolution level of part `i`.
    pub fn decode_level(&self, i: usize, lx: usize, ly: usize) -> Result<Image> {
        let p = self.part(i)?;
        let info = &p.info;
        // Chunk range and level size.
        let (chunks, lw, lh): (Vec<usize>, usize, usize) = match &info.levels {
            None => {
                if lx != 0 || ly != 0 {
                    return Err(Error::usage("scanline parts have one level"));
                }
                ((0..info.chunk_count).collect(), info.width, info.height)
            }
            Some(lv) => {
                let li = lv
                    .level_index(lx, ly)
                    .ok_or_else(|| Error::usage(format!("level ({lx}, {ly})")))?;
                let n = lv.tiles_x(lx) * lv.tiles_y(ly);
                let first = lv.first_chunk[li];
                ((first..first + n).collect(), lv.widths[lx], lv.heights[ly])
            }
        };
        let dw = info.data_window;
        let level_window = Box2i {
            x_min: dw.x_min,
            y_min: dw.y_min,
            x_max: dw.x_min + lw as i32 - 1,
            y_max: dw.y_min + lh as i32 - 1,
        };
        let mut total = 0u64;
        let mut planes = Vec::with_capacity(info.channels.len());
        for c in &info.channels {
            let cw = count_sampled(dw.x_min, lw, c.x_sampling);
            let ch = count_sampled(dw.y_min, lh, c.y_sampling);
            let n = cw * ch;
            total += (n * c.sample_type.bytes()) as u64;
            if total > self.limits.max_image_bytes {
                return Err(Error::limit(format!("image of more than {total} bytes")));
            }
            planes.push((cw, ch, n));
        }
        // Every chunk must hold at least its header, so a file cannot claim
        // more image than it could possibly carry blocks for.
        let mut channels: Vec<Channel> = info
            .channels
            .iter()
            .zip(&planes)
            .map(|(d, &(w, h, n))| Channel {
                desc: d.clone(),
                width: w,
                height: h,
                samples: match d.sample_type {
                    SampleType::F16 => Samples::F16(vec![0; n]),
                    SampleType::F32 => Samples::F32(vec![0.0; n]),
                    SampleType::U32 => Samples::U32(vec![0; n]),
                },
            })
            .collect();

        let nthreads = self.threads.resolve().min(chunks.len()).max(1);
        if nthreads == 1 {
            for &ci in &chunks {
                let b = self.decode_chunk(i, ci)?;
                place(&mut channels, dw, &b);
            }
        } else {
            let next = AtomicUsize::new(0);
            let (tx, rx) = mpsc::sync_channel::<(usize, Result<Block>)>(nthreads * 2);
            let mut first_err: Option<(usize, Error)> = None;
            std::thread::scope(|s| {
                for _ in 0..nthreads {
                    let tx = tx.clone();
                    let next = &next;
                    let chunks = &chunks;
                    s.spawn(move || loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        if k >= chunks.len() {
                            break;
                        }
                        let r = self.decode_chunk(i, chunks[k]);
                        if tx.send((k, r)).is_err() {
                            break;
                        }
                    });
                }
                drop(tx);
                for (k, r) in rx {
                    match r {
                        Ok(b) => {
                            if first_err.is_none() {
                                place(&mut channels, dw, &b);
                            }
                        }
                        Err(e) => {
                            if first_err.as_ref().is_none_or(|(fk, _)| k < *fk) {
                                first_err = Some((k, e));
                            }
                        }
                    }
                }
            });
            if let Some((_, e)) = first_err {
                return Err(e);
            }
        }
        Ok(Image {
            header: p.header.clone(),
            data_window: level_window,
            level: (lx, ly),
            channels,
        })
    }
}

/// Copy a decoded chunk into the channel planes.
fn place(channels: &mut [Channel], dw: Box2i, b: &Block) {
    let mut off = 0usize;
    for line in 0..b.height {
        let y = b.y0 + line as i32;
        for (ci, ch) in channels.iter_mut().enumerate() {
            let ys = ch.desc.y_sampling;
            if !sampled(y, ys) {
                continue;
            }
            let (n, _) = b.channel_sizes[ci];
            let bytes = n * ch.desc.sample_type.bytes();
            let src = &b.data[off..off + bytes];
            off += bytes;
            let row = ((i64::from(y) - i64::from(dw.y_min)) / i64::from(ys)) as usize;
            let xs = ch.desc.x_sampling;
            let col = count_sampled(
                dw.x_min,
                (i64::from(b.x0) - i64::from(dw.x_min)) as usize,
                xs,
            );
            let start = row * ch.width + col;
            match &mut ch.samples {
                Samples::F16(v) => {
                    for (d, s) in v[start..start + n].iter_mut().zip(src.chunks_exact(2)) {
                        *d = u16::from_le_bytes([s[0], s[1]]);
                    }
                }
                Samples::F32(v) => {
                    for (d, s) in v[start..start + n].iter_mut().zip(src.chunks_exact(4)) {
                        *d = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                    }
                }
                Samples::U32(v) => {
                    for (d, s) in v[start..start + n].iter_mut().zip(src.chunks_exact(4)) {
                        *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                    }
                }
            }
        }
    }
}
