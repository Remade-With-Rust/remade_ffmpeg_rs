//! Reading EXR files: structure, chunks, and image assembly.

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
    /// One worker per available core (the default), but no more than one
    /// per 512 KiB of decoded samples, so a small image does not spawn
    /// threads it cannot keep busy.
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

    /// Workers for `tasks` independent tasks producing `bytes` of samples.
    /// `Auto` also caps the count at one per [`AUTO_BYTES_PER_WORKER`]:
    /// spawning a thread per core for a small image cost more than the work
    /// it shared out (the bands are independent, so the output is the same
    /// for any worker count).
    pub(crate) fn workers(self, tasks: usize, bytes: u64) -> usize {
        let n = match self {
            Self::Auto => {
                let by_work = bytes.div_ceil(AUTO_BYTES_PER_WORKER).max(1);
                self.resolve()
                    .min(usize::try_from(by_work).unwrap_or(usize::MAX))
            }
            _ => self.resolve(),
        };
        n.min(tasks).max(1)
    }
}

/// Decoded bytes per worker below which [`Threads::Auto`] adds no worker.
const AUTO_BYTES_PER_WORKER: u64 = 512 * 1024;

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

    fn rect_of(info: &PartInfo, pos: ChunkPos) -> (i32, i32, usize, usize) {
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

    /// Locate chunk `index` of part `part`, validate its header, and return
    /// its position, geometry and packed bytes.
    #[allow(clippy::type_complexity)]
    fn chunk_data<'s>(
        &'s self,
        p: &'s Part,
        part: usize,
        index: usize,
    ) -> Result<(ChunkPos, (i32, i32, usize, usize), &'s [u8])> {
        if index >= p.info.chunk_count {
            return Err(Error::usage(format!(
                "chunk {index} of {}",
                p.info.chunk_count
            )));
        }
        let pos = Self::chunk_pos(&p.info, index)?;
        let rect = Self::rect_of(&p.info, pos);
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
        Ok((pos, rect, c.bytes(size as usize)?))
    }

    /// Read and decompress chunk `index` of part `part`.
    pub fn decode_chunk(&self, part: usize, index: usize) -> Result<Block> {
        let p = self.part(part)?;
        let (pos, (x0, y0, w, lines), packed) = self.chunk_data(p, part, index)?;
        let geom = Self::geom(&p.info, x0, y0, w, lines)?;
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
        let lv = self.level_layout(p, lx, ly)?;
        let mut channels: Vec<Channel> = p
            .info
            .channels
            .iter()
            .zip(&lv.sizes)
            .map(|(d, &(w, h))| Channel {
                desc: d.clone(),
                width: w,
                height: h,
                samples: match d.sample_type {
                    SampleType::F16 => Samples::F16(vec![0; w * h]),
                    SampleType::F32 => Samples::F32(vec![0.0; w * h]),
                    SampleType::U32 => Samples::U32(vec![0; w * h]),
                },
            })
            .collect();
        let strides: Vec<usize> = lv.sizes.iter().map(|s| s.0).collect();
        let window = lv.window;
        let planes = channels
            .iter_mut()
            .map(|c| Some(PlaneMut::from(&mut c.samples)))
            .collect();
        self.run_bands(p, i, lv, planes, &strides)?;
        Ok(Image {
            header: p.header.clone(),
            data_window: window,
            level: (lx, ly),
            channels,
        })
    }

    /// Decode part `i` (level 0 for multi-level tiled parts) straight into
    /// the caller's planes, one [`ChannelSink`] per channel in channel-list
    /// (sorted) order. Each chunk is decompressed and written to its rows of
    /// the planes in one pass, converting the sample type on the way where
    /// the sink asks for it; nothing is allocated per plane.
    ///
    /// Plane `c` is [`Self::plane_sizes`]`[c]` samples wide and high (the
    /// data window divided by the channel's sampling). Uses the decoder's
    /// [`Threads`] setting; the planes come out the same for any worker
    /// count, and the same as [`Self::decode_part`]'s (converted, where a
    /// sink converts, exactly as [`Samples::to_f32`] or [`crate::f32_to_f16`]
    /// would). On error the planes may hold part of the image.
    ///
    /// ```no_run
    /// use rusty_exr::{ChannelSink, Decoder};
    /// let bytes = std::fs::read("in.exr").unwrap();
    /// let dec = Decoder::new(&bytes).unwrap();
    /// let (w, h) = dec.plane_sizes(0, 0, 0).unwrap()[0];
    /// let [mut r, mut g, mut b, mut a] = [(); 4].map(|()| vec![0f32; w * h]);
    /// // Channels come in name order; skip any you do not want.
    /// let mut sinks: Vec<ChannelSink> = Vec::new();
    /// let (mut ra, mut rb, mut rg, mut rr) = (Some(&mut a), Some(&mut b), Some(&mut g), Some(&mut r));
    /// for c in dec.header(0).unwrap().channels() {
    ///     let plane = match c.name.as_str() {
    ///         "A" => ra.take(),
    ///         "B" => rb.take(),
    ///         "G" => rg.take(),
    ///         "R" => rr.take(),
    ///         _ => None,
    ///     };
    ///     // HALF, FLOAT and UINT channels all convert into an f32 plane.
    ///     sinks.push(plane.map_or_else(ChannelSink::skip, |p| ChannelSink::f32(p)));
    /// }
    /// dec.decode_part_into(0, &mut sinks).unwrap();
    /// ```
    pub fn decode_part_into(&self, i: usize, sinks: &mut [ChannelSink<'_>]) -> Result<()> {
        self.decode_level_into(i, 0, 0, sinks)
    }

    /// [`Self::decode_part_into`] for one resolution level of part `i`.
    pub fn decode_level_into(
        &self,
        i: usize,
        lx: usize,
        ly: usize,
        sinks: &mut [ChannelSink<'_>],
    ) -> Result<()> {
        let p = self.part(i)?;
        let lv = self.level_layout(p, lx, ly)?;
        let strides = check_sinks(&p.info.channels, &lv.sizes, sinks)?;
        let planes = sinks.iter_mut().map(ChannelSink::reborrow).collect();
        self.run_bands(p, i, lv, planes, &strides)
    }

    /// [`Self::decode_level_into`] as independent jobs, for callers that run
    /// them on their own thread pool: one [`DecodeBand`] per row band (a
    /// scanline block, or a row of tiles), each holding its own disjoint
    /// rows of the sinks' planes. Run every band once, in any order and on
    /// any thread, with a [`DecodeScratch`] per thread; the planes then hold
    /// what [`Self::decode_level_into`] writes. The decoder's [`Threads`]
    /// setting is not used.
    ///
    /// A persistent pool (rayon's, say) skips the per-call thread start-up
    /// that [`Threads::Auto`] pays, which is most of the cost of decoding a
    /// cheap codec (None, RLE) on many cores.
    ///
    /// ```no_run
    /// use rusty_exr::{ChannelSink, DecodeScratch, Decoder};
    /// let bytes = std::fs::read("in.exr").unwrap();
    /// let dec = Decoder::new(&bytes).unwrap();
    /// let sizes = dec.plane_sizes(0, 0, 0).unwrap();
    /// let mut planes: Vec<Vec<f32>> = sizes.iter().map(|&(w, h)| vec![0.0; w * h]).collect();
    /// let mut sinks: Vec<ChannelSink> = planes.iter_mut().map(|p| ChannelSink::f32(p)).collect();
    /// let bands = dec.decode_bands(0, 0, 0, &mut sinks).unwrap();
    /// // With rayon: bands.into_par_iter().try_for_each_init(DecodeScratch::default, |s, b| b.decode(s))
    /// let queue = std::sync::Mutex::new(bands.into_iter());
    /// std::thread::scope(|s| {
    ///     for _ in 0..4 {
    ///         s.spawn(|| {
    ///             let mut scratch = DecodeScratch::default();
    ///             while let Some(band) = { let next = queue.lock().unwrap().next(); next } {
    ///                 band.decode(&mut scratch).unwrap();
    ///             }
    ///         });
    ///     }
    /// });
    /// ```
    pub fn decode_bands<'s>(
        &'s self,
        i: usize,
        lx: usize,
        ly: usize,
        sinks: &'s mut [ChannelSink<'_>],
    ) -> Result<Vec<DecodeBand<'s, 'a>>> {
        let p = self.part(i)?;
        let lv = self.level_layout(p, lx, ly)?;
        let strides = check_sinks(&p.info.channels, &lv.sizes, sinks)?;
        let planes = sinks.iter_mut().map(ChannelSink::reborrow).collect();
        Ok(Self::band_tasks(p, lv.bands, planes, &strides)
            .into_iter()
            .map(|task| DecodeBand {
                dec: self,
                part: i,
                task,
            })
            .collect())
    }

    /// Per channel (channel-list order): the `(width, height)` in samples of
    /// its plane at resolution level (`lx`, `ly`) of part `i`, the sizes
    /// [`Self::decode_level_into`] expects.
    pub fn plane_sizes(&self, i: usize, lx: usize, ly: usize) -> Result<Vec<(usize, usize)>> {
        let p = self.part(i)?;
        Ok(self.level_layout(p, lx, ly)?.sizes)
    }

    /// The pixel rectangle chunk `index` of part `part` covers, without
    /// reading it.
    pub fn chunk_rect(&self, part: usize, index: usize) -> Result<ChunkRect> {
        let p = self.part(part)?;
        if index >= p.info.chunk_count {
            return Err(Error::usage(format!(
                "chunk {index} of {}",
                p.info.chunk_count
            )));
        }
        let pos = Self::chunk_pos(&p.info, index)?;
        let (x0, y0, width, height) = Self::rect_of(&p.info, pos);
        Ok(ChunkRect {
            pos,
            x0,
            y0,
            width,
            height,
        })
    }

    /// Read and decompress chunk `index` of part `part` straight into the
    /// caller's buffers: one [`ChannelSink`] per channel (channel-list
    /// order) holding that channel's samples *of this chunk only*,
    /// [`ChunkRect::channel_size`] wide and high, row by row (`with_stride`
    /// lets a sink be a window into a larger plane). `scratch` keeps its
    /// buffers and inflater from call to call; give each thread its own.
    ///
    /// The samples are the ones [`Self::decode_chunk`] returns (converted
    /// where a sink converts), without the intermediate [`Block`].
    ///
    /// ```no_run
    /// use rusty_exr::{ChannelSink, DecodeScratch, Decoder};
    /// let bytes = std::fs::read("in.exr").unwrap();
    /// let dec = Decoder::new(&bytes).unwrap();
    /// let channels = dec.header(0).unwrap().channels().to_vec();
    /// let (w, h) = dec.plane_sizes(0, 0, 0).unwrap()[0];
    /// let mut planes: Vec<Vec<f32>> = channels.iter().map(|_| vec![0.0; w * h]).collect();
    /// let mut scratch = DecodeScratch::default();
    /// for k in 0..dec.chunk_count(0).unwrap() {
    ///     let r = dec.chunk_rect(0, k).unwrap();
    ///     let row0 = (r.y0 - dec.header(0).unwrap().data_window().unwrap().y_min) as usize;
    ///     // A scanline chunk spans whole rows: hand over exactly those rows.
    ///     let mut sinks: Vec<ChannelSink> = planes
    ///         .iter_mut()
    ///         .map(|p| ChannelSink::f32(&mut p[row0 * w..(row0 + r.height) * w]))
    ///         .collect();
    ///     dec.decode_chunk_into(0, k, &mut sinks, &mut scratch).unwrap();
    /// }
    /// ```
    pub fn decode_chunk_into(
        &self,
        part: usize,
        index: usize,
        sinks: &mut [ChannelSink<'_>],
        scratch: &mut DecodeScratch,
    ) -> Result<ChunkRect> {
        let r = self.chunk_rect(part, index)?;
        let p = self.part(part)?;
        let sizes: Vec<(usize, usize)> =
            p.info.channels.iter().map(|c| r.channel_size(c)).collect();
        let strides = check_sinks(&p.info.channels, &sizes, sinks)?;
        let mut task = Task {
            y0: r.y0,
            x0: r.x0,
            chunks: Vec::new(),
            planes: sinks.iter_mut().map(ChannelSink::reborrow).collect(),
            strides,
        };
        self.decode_into(
            p,
            part,
            index,
            &mut task,
            &mut scratch.inner,
            &mut scratch.rows,
        )?;
        Ok(r)
    }

    /// Row bands, window, plane sizes and decoded size of one level.
    fn level_layout(&self, p: &Part, lx: usize, ly: usize) -> Result<LevelLayout> {
        let info = &p.info;
        let dw = info.data_window;
        // Row bands: disjoint row ranges of the output, each with the chunks
        // that fill it. A scanline block is one band; a row of tiles is one.
        let (bands, lw, lh): (Vec<BandRows>, usize, usize) = match &info.levels {
            None => {
                if lx != 0 || ly != 0 {
                    return Err(Error::usage("scanline parts have one level"));
                }
                let lpb = info.compression.lines_per_block() as usize;
                let bands = (0..info.chunk_count)
                    .map(|k| {
                        let y0 = dw.y_min + (k * lpb) as i32;
                        (y0, lpb.min(info.height - k * lpb), vec![k])
                    })
                    .collect();
                (bands, info.width, info.height)
            }
            Some(lv) => {
                let li = lv
                    .level_index(lx, ly)
                    .ok_or_else(|| Error::usage(format!("level ({lx}, {ly})")))?;
                let (tx, ty) = (lv.tiles_x(lx), lv.tiles_y(ly));
                let th = lv.desc.y_size as usize;
                let first = lv.first_chunk[li];
                let lh = lv.heights[ly];
                let bands = (0..ty)
                    .map(|r| {
                        let y0 = dw.y_min + (r * th) as i32;
                        (
                            y0,
                            th.min(lh - r * th),
                            (0..tx).map(|c| first + r * tx + c).collect(),
                        )
                    })
                    .collect();
                (bands, lv.widths[lx], lh)
            }
        };
        let window = Box2i {
            x_min: dw.x_min,
            y_min: dw.y_min,
            x_max: dw.x_min + lw as i32 - 1,
            y_max: dw.y_min + lh as i32 - 1,
        };
        let mut total = 0u64;
        let mut sizes = Vec::with_capacity(info.channels.len());
        for c in &info.channels {
            let cw = count_sampled(dw.x_min, lw, c.x_sampling);
            let ch = count_sampled(dw.y_min, lh, c.y_sampling);
            total += (cw * ch * c.sample_type.bytes()) as u64;
            if total > self.limits.max_image_bytes {
                return Err(Error::limit(format!("image of more than {total} bytes")));
            }
            sizes.push((cw, ch));
        }
        Ok(LevelLayout {
            bands,
            window,
            sizes,
            total,
        })
    }

    /// Split `planes` (per channel, `None` to skip it; `strides[c]`
    /// samples per row) into the level's row bands: one task per band.
    fn band_tasks<'p>(
        p: &Part,
        bands: Vec<BandRows>,
        planes: Vec<Option<PlaneMut<'p>>>,
        strides: &[usize],
    ) -> Vec<Task<'p>> {
        let dw = p.info.data_window;
        // Split every channel plane into the bands' row ranges.
        let mut per_band: Vec<Vec<Option<PlaneMut<'p>>>> = (0..bands.len())
            .map(|_| Vec::with_capacity(planes.len()))
            .collect();
        for ((plane, desc), &stride) in planes.into_iter().zip(&p.info.channels).zip(strides) {
            let ys = desc.y_sampling;
            let mut rest = plane;
            for (b, (y0, lines, _)) in bands.iter().enumerate() {
                let Some(r) = rest.take() else {
                    per_band[b].push(None);
                    continue;
                };
                // The last row of a strided plane may stop at its width.
                let n = (count_sampled(*y0, *lines, ys) * stride).min(r.len());
                let (head, tail) = r.split_at(n);
                per_band[b].push(Some(head));
                rest = Some(tail);
            }
        }
        bands
            .into_iter()
            .zip(per_band)
            .map(|((y0, _, chunks), planes)| Task {
                y0,
                x0: dw.x_min,
                chunks,
                planes,
                strides: strides.to_vec(),
            })
            .collect()
    }

    /// Decode every chunk of a level into `planes` (see [`Self::band_tasks`]),
    /// sharing the bands out over the decoder's workers.
    fn run_bands(
        &self,
        p: &Part,
        i: usize,
        lv: LevelLayout,
        planes: Vec<Option<PlaneMut<'_>>>,
        strides: &[usize],
    ) -> Result<()> {
        let LevelLayout { bands, total, .. } = lv;
        let tasks = Self::band_tasks(p, bands, planes, strides);
        let nthreads = self.threads.workers(tasks.len(), total);
        let errors: std::sync::Mutex<Vec<(usize, Error)>> = std::sync::Mutex::new(Vec::new());
        // Bands not yet taken: helpers are spawned only while some remain.
        let left = std::sync::atomic::AtomicUsize::new(tasks.len());
        let queue = std::sync::Mutex::new(tasks.into_iter());
        let work = || {
            let mut scratch = compression::Scratch::default();
            let mut rows = RowBuf::default();
            loop {
                let task = {
                    let mut q = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    q.next()
                };
                let Some(mut task) = task else { break };
                left.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                let chunks = std::mem::take(&mut task.chunks);
                for &ci in &chunks {
                    if let Err(e) = self.decode_into(p, i, ci, &mut task, &mut scratch, &mut rows) {
                        errors
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((ci, e));
                        break;
                    }
                }
            }
        };
        if nthreads == 1 {
            work();
        } else {
            // The calling thread is one of the workers. Spawning a thread
            // costs tens of microseconds (about 1.8 ms for 24 on Windows),
            // as much as a whole uncompressed band, so helpers are added
            // one at a time and only while bands are left to hand out: a
            // cheap decode stops spawning once the queue has drained.
            std::thread::scope(|s| {
                for _ in 1..nthreads {
                    if left.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                        break;
                    }
                    s.spawn(work);
                }
                work();
            });
        }
        let mut errors = errors
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !errors.is_empty() {
            errors.sort_by_key(|(k, _)| *k);
            return Err(errors.swap_remove(0).1);
        }
        Ok(())
    }

    /// Decode chunk `ci` straight into a task's planes.
    fn decode_into(
        &self,
        p: &Part,
        part: usize,
        ci: usize,
        task: &mut Task<'_>,
        scratch: &mut compression::Scratch,
        rows: &mut RowBuf,
    ) -> Result<()> {
        let (_, (x0, y0, w, lines), packed) = self.chunk_data(p, part, ci)?;
        let geom = Self::geom(&p.info, x0, y0, w, lines)?;
        // DWA can decode HALF and FLOAT lossy channels straight into the
        // output rows when the chunk spans whole plane rows (scanline parts)
        // and the plane holds the channel's own sample type.
        let mut direct: Vec<Option<compression::Direct<'_>>> = Vec::new();
        if matches!(p.info.compression, Compression::Dwaa | Compression::Dwab)
            && geom.chans.iter().any(|g| g.ty != SampleType::U32)
        {
            let cols = chunk_cols(&geom, x0, task);
            direct = task
                .planes
                .iter_mut()
                .zip(&cols)
                .zip(&geom.chans)
                .map(|((plane, &(pw, col, row0)), g)| match plane {
                    Some(PlaneMut::F16(v))
                        if g.ty == SampleType::F16 && col == 0 && g.width == pw =>
                    {
                        v.get_mut(row0 * pw..(row0 + g.height) * pw)
                            .map(compression::Direct::F16)
                    }
                    Some(PlaneMut::F32(v))
                        if g.ty == SampleType::F32 && col == 0 && g.width == pw =>
                    {
                        v.get_mut(row0 * pw..(row0 + g.height) * pw)
                            .map(compression::Direct::F32)
                    }
                    _ => None,
                })
                .collect();
        }
        let layout = compression::decompress_layout(
            p.info.compression,
            &geom,
            packed,
            self.limits.max_chunk_bytes,
            scratch,
            &mut direct,
        )?;
        drop(direct);
        place(&geom, x0, task, &layout, rows);
        Ok(())
    }
}

/// Row bands, window, plane sizes and decoded size of one level.
struct LevelLayout {
    bands: Vec<BandRows>,
    window: Box2i,
    sizes: Vec<(usize, usize)>,
    total: u64,
}

/// A row band: first line, line count, chunk indices.
type BandRows = (i32, usize, Vec<usize>);

/// The pixel rectangle of one chunk (see [`Decoder::chunk_rect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRect {
    pub pos: ChunkPos,
    /// Top-left pixel (absolute coordinates; for levels > 0, relative to
    /// the data window origin scaled to that level).
    pub x0: i32,
    pub y0: i32,
    /// Size in pixels.
    pub width: usize,
    pub height: usize,
}

impl ChunkRect {
    /// `(samples per row, sample rows)` of a channel inside this chunk.
    #[must_use]
    pub fn channel_size(&self, c: &ChannelDesc) -> (usize, usize) {
        (
            count_sampled(self.x0, self.width, c.x_sampling),
            count_sampled(self.y0, self.height, c.y_sampling),
        )
    }
}

/// One row band of a level and its rows of the caller's planes, from
/// [`Decoder::decode_bands`].
pub struct DecodeBand<'a, 'd> {
    dec: &'a Decoder<'d>,
    part: usize,
    task: Task<'a>,
}

impl DecodeBand<'_, '_> {
    /// Decode the band's chunks into its rows. Errors are the ones
    /// [`Decoder::decode_level`] reports for those chunks.
    pub fn decode(mut self, scratch: &mut DecodeScratch) -> Result<()> {
        let p = &self.dec.parts[self.part];
        let chunks = std::mem::take(&mut self.task.chunks);
        for &ci in &chunks {
            self.dec.decode_into(
                p,
                self.part,
                ci,
                &mut self.task,
                &mut scratch.inner,
                &mut scratch.rows,
            )?;
        }
        Ok(())
    }

    /// First line (absolute y) of the band.
    #[must_use]
    pub fn y0(&self) -> i32 {
        self.task.y0
    }

    /// Chunk indices the band decodes.
    #[must_use]
    pub fn chunks(&self) -> &[usize] {
        &self.task.chunks
    }
}

impl std::fmt::Debug for DecodeBand<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodeBand")
            .field("part", &self.part)
            .field("y0", &self.task.y0)
            .field("chunks", &self.task.chunks)
            .finish_non_exhaustive()
    }
}

/// Where one channel goes in [`Decoder::decode_part_into`],
/// [`Decoder::decode_level_into`] and [`Decoder::decode_chunk_into`]: a
/// caller-owned plane of `f16` bits, `f32` or `u32` samples, or nowhere.
///
/// The plane is row-major, one row every `stride` samples (by default the
/// channel's width, so rows are packed). Its type may differ from the
/// channel's: any channel converts into an `f32` plane (HALF exactly, UINT
/// numerically, as [`Samples::to_f32`]), and a FLOAT channel into an `f16`
/// plane (rounded to nearest, as [`crate::f32_to_f16`]). Other pairs are
/// refused with [`Error::Usage`].
pub struct ChannelSink<'a> {
    plane: Option<PlaneMut<'a>>,
    stride: Option<usize>,
}

impl<'a> ChannelSink<'a> {
    /// A plane of half-float bit patterns.
    #[must_use]
    pub fn f16(plane: &'a mut [u16]) -> Self {
        Self {
            plane: Some(PlaneMut::F16(plane)),
            stride: None,
        }
    }

    /// A plane of `f32`.
    #[must_use]
    pub fn f32(plane: &'a mut [f32]) -> Self {
        Self {
            plane: Some(PlaneMut::F32(plane)),
            stride: None,
        }
    }

    /// A plane of `u32`.
    #[must_use]
    pub fn u32(plane: &'a mut [u32]) -> Self {
        Self {
            plane: Some(PlaneMut::U32(plane)),
            stride: None,
        }
    }

    /// Decode the channel's data but store it nowhere.
    #[must_use]
    pub fn skip() -> Self {
        Self {
            plane: None,
            stride: None,
        }
    }

    /// Rows start every `stride` samples (at least the channel's width), so
    /// the plane can be a window into a wider buffer. The last row needs
    /// only the channel's width.
    #[must_use]
    pub fn with_stride(mut self, stride: usize) -> Self {
        self.stride = Some(stride);
        self
    }

    /// The plane's sample type (`None` for [`Self::skip`]).
    #[must_use]
    pub fn sample_type(&self) -> Option<SampleType> {
        self.plane.as_ref().map(PlaneMut::sample_type)
    }

    fn reborrow(&mut self) -> Option<PlaneMut<'_>> {
        self.plane.as_mut().map(|p| match p {
            PlaneMut::F16(v) => PlaneMut::F16(v),
            PlaneMut::F32(v) => PlaneMut::F32(v),
            PlaneMut::U32(v) => PlaneMut::U32(v),
        })
    }
}

impl std::fmt::Debug for ChannelSink<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelSink")
            .field("sample_type", &self.sample_type())
            .field("len", &self.plane.as_ref().map(PlaneMut::len))
            .field("stride", &self.stride)
            .finish()
    }
}

/// Reusable buffers (and the zlib inflater) for
/// [`Decoder::decode_chunk_into`]. Keep one per thread.
#[derive(Default)]
pub struct DecodeScratch {
    inner: compression::Scratch,
    rows: RowBuf,
}

impl std::fmt::Debug for DecodeScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodeScratch").finish_non_exhaustive()
    }
}

/// Per-worker rows for sample-type conversion (kept, never re-zeroed: each
/// use writes every element it reads).
#[derive(Default)]
struct RowBuf {
    h: Vec<u16>,
    w: Vec<u32>,
    b: Vec<u8>,
}

/// Whether a `src` channel can be written to a `dst` plane.
fn converts(src: SampleType, dst: SampleType) -> bool {
    src == dst || dst == SampleType::F32 || (src, dst) == (SampleType::F32, SampleType::F16)
}

/// Check one sink per channel (types, strides, lengths for planes of
/// `sizes`) and return each sink's stride.
fn check_sinks(
    chans: &[ChannelDesc],
    sizes: &[(usize, usize)],
    sinks: &[ChannelSink<'_>],
) -> Result<Vec<usize>> {
    if sinks.len() != chans.len() {
        return Err(Error::usage(format!(
            "{} sinks for {} channels",
            sinks.len(),
            chans.len()
        )));
    }
    let mut strides = Vec::with_capacity(sinks.len());
    for ((c, &(w, h)), s) in chans.iter().zip(sizes).zip(sinks) {
        let stride = s.stride.unwrap_or(w);
        if let Some(plane) = &s.plane {
            let dst = plane.sample_type();
            if !converts(c.sample_type, dst) {
                return Err(Error::usage(format!(
                    "channel {} holds {:?} samples, which a {dst:?} sink cannot take",
                    c.name, c.sample_type
                )));
            }
            if stride < w {
                return Err(Error::usage(format!(
                    "channel {}: stride {stride} below its width {w}",
                    c.name
                )));
            }
            let need = if w == 0 || h == 0 {
                Some(0)
            } else {
                (h - 1).checked_mul(stride).and_then(|n| n.checked_add(w))
            };
            match need {
                Some(n) if plane.len() >= n => {}
                _ => {
                    return Err(Error::usage(format!(
                    "channel {}: a {w}x{h} plane with stride {stride} does not fit in {} samples",
                    c.name,
                    plane.len()
                )))
                }
            }
        }
        strides.push(stride);
    }
    Ok(strides)
}

/// A mutable view of one channel plane (or a row range of it).
enum PlaneMut<'a> {
    F16(&'a mut [u16]),
    F32(&'a mut [f32]),
    U32(&'a mut [u32]),
}

impl<'a> PlaneMut<'a> {
    fn from(s: &'a mut Samples) -> Self {
        match s {
            Samples::F16(v) => Self::F16(v),
            Samples::F32(v) => Self::F32(v),
            Samples::U32(v) => Self::U32(v),
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::F16(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::U32(v) => v.len(),
        }
    }
    fn sample_type(&self) -> SampleType {
        match self {
            Self::F16(_) => SampleType::F16,
            Self::F32(_) => SampleType::F32,
            Self::U32(_) => SampleType::U32,
        }
    }
    fn split_at(self, n: usize) -> (Self, Self) {
        match self {
            Self::F16(v) => {
                let (a, b) = v.split_at_mut(n);
                (Self::F16(a), Self::F16(b))
            }
            Self::F32(v) => {
                let (a, b) = v.split_at_mut(n);
                (Self::F32(a), Self::F32(b))
            }
            Self::U32(v) => {
                let (a, b) = v.split_at_mut(n);
                (Self::U32(a), Self::U32(b))
            }
        }
    }
}

/// Output rows and the chunks that fill them: a row band of a level, or
/// one chunk's own rectangle (the chunk API).
struct Task<'a> {
    /// First line (absolute y) of the planes' row 0.
    y0: i32,
    /// First pixel column (absolute x) of the planes' column 0.
    x0: i32,
    chunks: Vec<usize>,
    /// Per channel: the rows of that channel's plane (`None`: skipped).
    planes: Vec<Option<PlaneMut<'a>>>,
    /// Per channel: samples from one plane row to the next.
    strides: Vec<usize>,
}

/// Little-endian 16-bit samples from bytes.
#[inline]
fn put16(dst: &mut [u16], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(2)) {
        *d = u16::from_le_bytes([s[0], s[1]]);
    }
}

/// Little-endian 32-bit samples from bytes.
#[inline]
fn put32(dst: &mut [u32], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
        *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    }
}

#[inline]
fn put32f(dst: &mut [f32], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
        *d = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    }
}

/// 16-bit samples whose low bytes are in `a` and high bytes in `b`.
#[inline]
fn put16_halves(dst: &mut [u16], a: &[u8], b: &[u8]) {
    for ((d, &lo), &hi) in dst.iter_mut().zip(a).zip(b) {
        *d = u16::from(lo) | (u16::from(hi) << 8);
    }
}

/// 32-bit samples from (low, high) 16-bit word pairs.
#[inline]
fn put32_words(dst: &mut [u32], src: &[u16]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(2)) {
        *d = u32::from(s[0]) | (u32::from(s[1]) << 16);
    }
}

/// Source of one row segment.
#[derive(Clone, Copy)]
enum Seg<'s> {
    Bytes(&'s [u8]),
    Halves(&'s [u8], &'s [u8]),
    Words(&'s [u16]),
}

/// A row of 16-bit samples into `dst` (all of it).
#[inline]
fn seg16(dst: &mut [u16], seg: Seg<'_>) {
    match seg {
        Seg::Bytes(s) => put16(dst, s),
        Seg::Halves(a, b) => put16_halves(dst, a, b),
        Seg::Words(w) => dst.copy_from_slice(&w[..dst.len()]),
    }
}

/// A row of 32-bit samples into `dst` (all of it).
#[inline]
fn seg32(dst: &mut [u32], seg: Seg<'_>) {
    match seg {
        Seg::Bytes(s) => put32(dst, s),
        Seg::Halves(..) => unreachable!("32-bit halves are interleaved first"),
        Seg::Words(w) => put32_words(dst, w),
    }
}

/// `out[i] = f16_to_f32(h[i])`. The SIMD twin converts whole NaN-free groups
/// of 8 (identical results there: both conversions are exact); the scalar
/// code, its oracle, takes a declined group and the tail, handing back to
/// the twin at the next group boundary.
fn halves_to_f32(isa: crate::simd::Isa, h: &[u16], out: &mut [f32]) {
    let n = h.len();
    let mut i = crate::simd::dwa_f32(isa, h, out);
    while i < n {
        out[i] = crate::half::f16_to_f32(h[i]);
        i += 1;
        if i % 8 == 0 && i + 8 <= n {
            i += crate::simd::dwa_f32(isa, &h[i..], &mut out[i..]);
        }
    }
}

/// One row of `n` samples of a `src` channel into a plane at `start`.
fn put_row(
    dst: &mut PlaneMut<'_>,
    start: usize,
    n: usize,
    src: SampleType,
    seg: Seg<'_>,
    rows: &mut RowBuf,
) {
    let RowBuf { h, w, b } = rows;
    // A 32-bit row in the split-bytes layout (ZIP, RLE) is interleaved
    // back into an L1-sized byte row first: that loop vectorizes, while
    // gathering each sample's four bytes from the two halves did not.
    let seg = match seg {
        Seg::Halves(lo, hi) if src != SampleType::F16 => {
            let row = compression::grow(b, 2 * lo.len());
            for ((d, &x), &y) in row.chunks_exact_mut(2).zip(lo).zip(hi) {
                d[0] = x;
                d[1] = y;
            }
            Seg::Bytes(row)
        }
        seg => seg,
    };
    match (dst, src) {
        (PlaneMut::F16(v), SampleType::F16) => seg16(&mut v[start..start + n], seg),
        (PlaneMut::U32(v), SampleType::U32) => seg32(&mut v[start..start + n], seg),
        (PlaneMut::F32(v), SampleType::F32) => match seg {
            Seg::Bytes(s) => put32f(&mut v[start..start + n], s),
            Seg::Halves(..) => unreachable!("32-bit halves are interleaved first"),
            Seg::Words(w) => {
                for (d, s) in v[start..start + n].iter_mut().zip(w.chunks_exact(2)) {
                    *d = f32::from_bits(u32::from(s[0]) | (u32::from(s[1]) << 16));
                }
            }
        },
        // Conversions: the row is gathered into an L1-sized buffer, then
        // converted into the plane.
        (PlaneMut::F32(v), SampleType::F16) => {
            let h = compression::grow(h, n);
            seg16(h, seg);
            halves_to_f32(crate::simd::isa(), h, &mut v[start..start + n]);
        }
        (PlaneMut::F32(v), SampleType::U32) => {
            let u = compression::grow(w, n);
            seg32(u, seg);
            for (d, &s) in v[start..start + n].iter_mut().zip(&*u) {
                *d = s as f32;
            }
        }
        (PlaneMut::F16(v), SampleType::F32) => {
            let u = compression::grow(w, n);
            seg32(u, seg);
            for (d, &s) in v[start..start + n].iter_mut().zip(&*u) {
                *d = crate::half::f32_to_f16(f32::from_bits(s));
            }
        }
        _ => unreachable!("sink types are checked before decoding"),
    }
}

/// Per channel of a chunk: plane stride, the chunk's first column, and the
/// task row index of the chunk's first sampled line.
fn chunk_cols(geom: &BlockGeom<'_>, x0: i32, task: &Task<'_>) -> Vec<(usize, usize, usize)> {
    let mut cols = Vec::with_capacity(geom.chans.len());
    for (c, g) in geom.chans.iter().enumerate() {
        let col = count_sampled(
            task.x0,
            (i64::from(x0) - i64::from(task.x0)) as usize,
            g.x_sampling,
        );
        let row0 = count_sampled(
            task.y0,
            (i64::from(geom.y0) - i64::from(task.y0)) as usize,
            g.y_sampling,
        );
        cols.push((task.strides[c], col, row0));
    }
    cols
}

/// Copy a decompressed chunk into a task's planes.
fn place(
    geom: &BlockGeom<'_>,
    x0: i32,
    task: &mut Task<'_>,
    layout: &compression::Layout<'_>,
    rows_buf: &mut RowBuf,
) {
    let cols = chunk_cols(geom, x0, task);
    match *layout {
        compression::Layout::Raw(_) | compression::Layout::Halves(_) => {
            let (raw, half) = match *layout {
                compression::Layout::Raw(r) => (r, 0),
                compression::Layout::Halves(h) => (h, h.len() / 2),
                _ => unreachable!(),
            };
            let mut off = 0usize;
            let mut rows = vec![0usize; geom.chans.len()];
            for line in 0..geom.lines {
                let y = geom.y0 + line as i32;
                for (c, g) in geom.chans.iter().enumerate() {
                    if !sampled(y, g.y_sampling) {
                        continue;
                    }
                    let n = g.width;
                    let nb = n * g.ty.bytes();
                    let (pw, col, row0) = cols[c];
                    let start = (row0 + rows[c]) * pw + col;
                    rows[c] += 1;
                    if let Some(plane) = &mut task.planes[c] {
                        let seg = if half == 0 {
                            Seg::Bytes(&raw[off..off + nb])
                        } else {
                            let h = off / 2;
                            Seg::Halves(&raw[h..h + nb / 2], &raw[half + h..half + h + nb / 2])
                        };
                        put_row(plane, start, n, g.ty, seg, rows_buf);
                    }
                    off += nb;
                }
            }
        }
        compression::Layout::Planar(bytes) => {
            let mut off = 0usize;
            for (c, g) in geom.chans.iter().enumerate() {
                let (pw, col, row0) = cols[c];
                let rb = g.row_bytes();
                let Some(plane) = &mut task.planes[c] else {
                    off += rb * g.height;
                    continue;
                };
                for r in 0..g.height {
                    let start = (row0 + r) * pw + col;
                    put_row(
                        plane,
                        start,
                        g.width,
                        g.ty,
                        Seg::Bytes(&bytes[off..off + rb]),
                        rows_buf,
                    );
                    off += rb;
                }
            }
        }
        compression::Layout::PlanarU16(words, placed) => {
            let mut off = 0usize;
            for (c, g) in geom.chans.iter().enumerate() {
                let (pw, col, row0) = cols[c];
                let rw = g.width * g.ty.bytes() / 2;
                let plane = match &mut task.planes[c] {
                    // Already decoded into the output rows, or not wanted.
                    _ if placed.get(c) == Some(&true) => None,
                    p => p.as_mut(),
                };
                let Some(plane) = plane else {
                    off += rw * g.height;
                    continue;
                };
                for r in 0..g.height {
                    let start = (row0 + r) * pw + col;
                    put_row(
                        plane,
                        start,
                        g.width,
                        g.ty,
                        Seg::Words(&words[off..off + rw]),
                        rows_buf,
                    );
                    off += rw;
                }
            }
        }
    }
}
