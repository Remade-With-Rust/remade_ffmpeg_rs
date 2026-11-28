//! Codestream syntax: marker segments and tile-parts (ISO/IEC 15444-1 Annex A).

use std::ops::Range;
use std::rc::Rc;

use crate::error::{Fields, JpxError, JpxResult};
use crate::t1::STYLE_HT;
use crate::Limits;

const SOC: u16 = 0xFF4F;
const CAP: u16 = 0xFF50;
const SIZ: u16 = 0xFF51;
const COD: u16 = 0xFF52;
const COC: u16 = 0xFF53;
const QCD: u16 = 0xFF5C;
const QCC: u16 = 0xFF5D;
const RGN: u16 = 0xFF5E;
const POC: u16 = 0xFF5F;
const PPM: u16 = 0xFF60;
const PPT: u16 = 0xFF61;
const SOT: u16 = 0xFF90;
const SOD: u16 = 0xFF93;
const EOC: u16 = 0xFFD9;

/// Most progression order changes accepted for one tile (a real stream has a
/// handful).
const MAX_POC: usize = 4096;

/// Image and component geometry (A.5.1).
pub(crate) struct Siz {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub tw: u32,
    pub th: u32,
    pub tx0: u32,
    pub ty0: u32,
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub comps: Vec<SizComp>,
}

#[derive(Clone, Copy)]
pub(crate) struct SizComp {
    pub bits: u8,
    pub signed: bool,
    pub dx: u32,
    pub dy: u32,
}

impl Siz {
    /// Tile `t`'s area on the reference grid: x0, y0, x1, y1 (B-7 to B-10).
    pub fn tile(&self, t: u32) -> [u32; 4] {
        let (p, q) = (u64::from(t % self.tiles_x), u64::from(t / self.tiles_x));
        let edge = |origin: u32, size: u32, i: u64, lo: u32, hi: u32| {
            (u64::from(origin) + i * u64::from(size)).clamp(u64::from(lo), u64::from(hi)) as u32
        };
        [
            edge(self.tx0, self.tw, p, self.x0, self.x1),
            edge(self.ty0, self.th, q, self.y0, self.y1),
            edge(self.tx0, self.tw, p + 1, self.x0, self.x1),
            edge(self.ty0, self.th, q + 1, self.y0, self.y1),
        ]
    }

    /// Whether no component has samples in tile `t`.
    pub fn tile_is_empty(&self, t: u32) -> bool {
        let r = self.tile(t);
        self.comps.iter().all(|k| {
            r[0].div_ceil(k.dx) >= r[2].div_ceil(k.dx) || r[1].div_ceil(k.dy) >= r[3].div_ceil(k.dy)
        })
    }

    /// Component `c`'s sample area at `reduce` discarded resolutions.
    pub fn comp_rect(&self, c: usize, reduce: u32) -> [u32; 4] {
        let k = &self.comps[c];
        [
            ceil_shift(self.x0.div_ceil(k.dx), reduce),
            ceil_shift(self.y0.div_ceil(k.dy), reduce),
            ceil_shift(self.x1.div_ceil(k.dx), reduce),
            ceil_shift(self.y1.div_ceil(k.dy), reduce),
        ]
    }
}

/// `ceil(v / 2^s)`.
pub(crate) fn ceil_shift(v: u32, s: u32) -> u32 {
    if s >= 32 {
        return u32::from(v != 0);
    }
    ((u64::from(v) + (1 << s) - 1) >> s) as u32
}

/// Coding style of a tile-component (Tables A.15, A.20).
#[derive(Clone, Copy)]
pub(crate) struct Coding {
    pub levels: u8,
    /// Code-block size exponents.
    pub xcb: u8,
    pub ycb: u8,
    pub style: u8,
    pub reversible: bool,
    /// Precinct size exponents per resolution, `PPy << 4 | PPx`.
    pub precincts: [u8; 33],
}

/// Quantization of a tile-component (A.6.4).
pub(crate) struct Quant {
    pub style: u8,
    pub guard: u8,
    pub steps: Vec<u16>,
}

impl Quant {
    /// Exponent and mantissa of a subband: `band` 0 is LL, then HL, LH, HH
    /// from the lowest resolution up.
    pub fn step(&self, band: usize) -> JpxResult<(u32, u32)> {
        let missing = JpxError::Format("too few quantization steps");
        if self.style == 1 {
            let v = u32::from(*self.steps.first().ok_or(missing)?);
            // E-5: one exponent less per resolution above the first.
            let drop = if band == 0 { 0 } else { (band as u32 - 1) / 3 };
            return Ok(((v >> 11).saturating_sub(drop), v & 0x7FF));
        }
        let v = u32::from(*self.steps.get(band).ok_or(missing)?);
        Ok(if self.style == 0 {
            (v >> 3, 0)
        } else {
            (v >> 11, v & 0x7FF)
        })
    }
}

/// One progression volume.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Progression {
    pub order: u8,
    pub res: Range2,
    pub comp: Range2,
    /// Layers before this one (exclusive end).
    pub layer_end: u32,
}

/// A copyable half-open range.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Range2 {
    pub start: u32,
    pub end: u32,
}

/// Parameters in force for a tile: the main header's, then overridden by the
/// tile's own marker segments.
#[derive(Clone)]
pub(crate) struct Params {
    pub order: u8,
    pub layers: u32,
    pub mct: bool,
    cod_rank: u8,
    coding: Vec<Option<Coding>>,
    coding_rank: Vec<u8>,
    quant: Vec<Option<Rc<Quant>>>,
    quant_rank: Vec<u8>,
    /// ROI shift per component.
    pub roi: Vec<u8>,
    pub poc: Vec<Progression>,
    poc_in_tile: bool,
}

impl Params {
    fn new(comps: usize) -> Self {
        Params {
            order: 0,
            layers: 0,
            mct: false,
            cod_rank: 0,
            coding: vec![None; comps],
            coding_rank: vec![0; comps],
            quant: vec![None; comps],
            quant_rank: vec![0; comps],
            roi: vec![0; comps],
            poc: Vec::new(),
            poc_in_tile: false,
        }
    }

    pub fn coding(&self, c: usize) -> JpxResult<&Coding> {
        self.coding[c]
            .as_ref()
            .ok_or(JpxError::Format("no COD marker segment"))
    }

    pub fn quant(&self, c: usize) -> JpxResult<&Quant> {
        self.quant[c]
            .as_deref()
            .ok_or(JpxError::Format("no QCD marker segment"))
    }

    /// Apply a COD, COC, QCD, QCC, RGN or POC segment. Rank, low to high:
    /// main default, main component, tile default, tile component; a segment
    /// never replaces one of higher rank.
    pub fn apply(&mut self, marker: u16, body: &[u8], in_tile: bool) -> JpxResult<()> {
        let n = self.coding.len();
        let mut f = Fields(body);
        let base = if in_tile { 3 } else { 1 };
        match marker {
            COD => {
                let scod = f.u8()?;
                let order = f.u8()?;
                let layers = u32::from(f.u16()?);
                let mct = f.u8()?;
                let coding = parse_coding(&mut f, scod)?;
                if layers == 0 {
                    return Err(JpxError::Format("no quality layers"));
                }
                if base >= self.cod_rank {
                    self.cod_rank = base;
                    self.order = order;
                    self.layers = layers;
                    self.mct = mct & 1 != 0;
                }
                for c in 0..n {
                    if base >= self.coding_rank[c] {
                        self.coding_rank[c] = base;
                        self.coding[c] = Some(coding);
                    }
                }
            }
            COC => {
                let c = f.component(n)?;
                let scoc = f.u8()?;
                let coding = parse_coding(&mut f, scoc)?;
                let slot = self.coding.get_mut(c).ok_or(BAD_COMPONENT)?;
                if base + 1 >= self.coding_rank[c] {
                    self.coding_rank[c] = base + 1;
                    *slot = Some(coding);
                }
            }
            QCD => {
                let q = Rc::new(parse_quant(&mut f)?);
                for c in 0..n {
                    if base >= self.quant_rank[c] {
                        self.quant_rank[c] = base;
                        self.quant[c] = Some(Rc::clone(&q));
                    }
                }
            }
            QCC => {
                let c = f.component(n)?;
                let q = Rc::new(parse_quant(&mut f)?);
                let slot = self.quant.get_mut(c).ok_or(BAD_COMPONENT)?;
                if base + 1 >= self.quant_rank[c] {
                    self.quant_rank[c] = base + 1;
                    *slot = Some(q);
                }
            }
            RGN => {
                let c = f.component(n)?;
                if f.u8()? != 0 {
                    return Err(JpxError::Unsupported("region of interest style"));
                }
                *self.roi.get_mut(c).ok_or(BAD_COMPONENT)? = f.u8()?;
            }
            POC => {
                if in_tile && !self.poc_in_tile {
                    self.poc_in_tile = true;
                    self.poc.clear();
                }
                while !f.0.is_empty() {
                    let r0 = f.u8()?;
                    let c0 = f.component(n)? as u32;
                    let layer_end = u32::from(f.u16()?);
                    let r1 = f.u8()?;
                    // 0 in the component end field stands for "all" (A.6.6).
                    let c1 = match f.component(n)? as u32 {
                        0 => 16384,
                        c => c,
                    };
                    let order = f.u8()?;
                    if self.poc.len() >= MAX_POC {
                        return Err(JpxError::Limit("progression order changes"));
                    }
                    self.poc.push(Progression {
                        order,
                        res: Range2 {
                            start: r0.into(),
                            end: r1.into(),
                        },
                        comp: Range2 { start: c0, end: c1 },
                        layer_end,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The progression volumes of the tile, in order.
    pub fn progressions(&self) -> Vec<Progression> {
        if !self.poc.is_empty() {
            return self.poc.clone();
        }
        vec![Progression {
            order: self.order,
            res: Range2 { start: 0, end: 33 },
            comp: Range2 {
                start: 0,
                end: 16384,
            },
            layer_end: self.layers,
        }]
    }
}

const BAD_COMPONENT: JpxError = JpxError::Format("component index out of range");

fn parse_coding(f: &mut Fields, scod: u8) -> JpxResult<Coding> {
    let levels = f.u8()?;
    let xcb = f.u8()?;
    let ycb = f.u8()?;
    let style = f.u8()?;
    let transform = f.u8()?;
    if levels > 32 {
        return Err(JpxError::Format("more than 32 decomposition levels"));
    }
    if xcb > 8 || ycb > 8 || xcb + ycb > 8 {
        return Err(JpxError::Format("code-block size"));
    }
    if style & STYLE_HT != 0 {
        return Err(JpxError::Unsupported("HTJ2K (Part 15) code-blocks"));
    }
    if transform > 1 {
        return Err(JpxError::Unsupported("wavelet transform"));
    }
    let mut precincts = [0xFF; 33];
    if scod & 1 != 0 {
        for (r, p) in precincts.iter_mut().enumerate().take(levels as usize + 1) {
            *p = f.u8()?;
            if r > 0 && (*p & 15 == 0 || *p >> 4 == 0) {
                return Err(JpxError::Format("precinct size"));
            }
        }
    }
    Ok(Coding {
        levels,
        xcb: xcb + 2,
        ycb: ycb + 2,
        style,
        reversible: transform == 1,
        precincts,
    })
}

fn parse_quant(f: &mut Fields) -> JpxResult<Quant> {
    let s = f.u8()?;
    let style = s & 31;
    let mut steps = Vec::new();
    match style {
        0 => {
            while steps.len() < 97 && !f.0.is_empty() {
                steps.push(u16::from(f.u8()?));
            }
        }
        1 | 2 => {
            while steps.len() < 97 && f.0.len() >= 2 {
                steps.push(f.u16()?);
            }
        }
        _ => return Err(JpxError::Format("quantization style")),
    }
    Ok(Quant {
        style,
        guard: s >> 5,
        steps,
    })
}

/// What a tile's tile-parts contain.
#[derive(Default)]
pub(crate) struct TileParts {
    /// Parameter marker segments, in order.
    pub markers: Vec<(u16, Range<usize>)>,
    /// PPT bodies: (tile-part, Zppt, data).
    ppt: Vec<(usize, u8, Range<usize>)>,
    /// Packed packet headers taken from PPM, one range list per tile-part.
    ppm: Vec<Vec<Range<usize>>>,
    /// Bodies of the tile-parts.
    pub bodies: Vec<Range<usize>>,
    /// The file ended inside one of the tile-parts.
    pub truncated: bool,
}

impl TileParts {
    /// The packed packet headers of this tile, when it has any.
    pub fn packed_headers(&self, cs: &[u8]) -> Option<Vec<u8>> {
        if self.ppt.is_empty() && self.ppm.is_empty() {
            return None;
        }
        let mut ppt: Vec<_> = self.ppt.iter().collect();
        ppt.sort_by_key(|p| (p.0, p.1));
        let mut out = Vec::new();
        for r in self.ppm.iter().flatten().chain(ppt.iter().map(|p| &p.2)) {
            out.extend_from_slice(&cs[r.clone()]);
        }
        Some(out)
    }
}

/// A parsed codestream: everything but the packets.
pub(crate) struct Codestream {
    pub siz: Siz,
    pub params: Params,
    /// Indexed by tile; tiles without a tile-part are `None`.
    pub tiles: Vec<Option<TileParts>>,
    /// There is an EOC marker where the last tile-part ends.
    pub ended: bool,
    /// Tile-parts a lenient parse skipped: (tile when known, why).
    pub skipped: Vec<(Option<u32>, &'static str)>,
}

/// Where a lenient parse picks the tile-parts up again after a damaged one:
/// the next SOT marker segment (`FF 90 00 0A`), which packet data cannot
/// contain (a byte after `FF` there is below `0x90`).
fn resync(cs: &[u8], from: usize) -> Option<usize> {
    let start = from.min(cs.len());
    cs[start..]
        .windows(4)
        .position(|w| w == [0xFF, 0x90, 0x00, 0x0A])
        .map(|i| start + i)
}

/// Parse the main header only.
pub(crate) fn parse_main(cs: &[u8], limits: &Limits) -> JpxResult<(Siz, Params)> {
    let (siz, params, _, _) = main_header(cs, limits)?;
    Ok((siz, params))
}

type PpmParts = Vec<(u8, Range<usize>)>;

fn main_header(cs: &[u8], limits: &Limits) -> JpxResult<(Siz, Params, PpmParts, usize)> {
    if cs.len() < 4 || be16(cs, 0) != SOC {
        return Err(JpxError::Format("no SOC marker"));
    }
    if be16(cs, 2) != SIZ {
        return Err(JpxError::Format("SIZ is not the first marker segment"));
    }
    let mut pos = 2;
    let mut siz = None;
    let mut params = None;
    let mut ppm = Vec::new();
    loop {
        let marker = next_marker(cs, pos)?;
        if marker == SOT {
            break;
        }
        if marker == EOC || marker == SOD {
            return Err(JpxError::Format("no tile-part before the end"));
        }
        if is_bare(marker) {
            pos += 2;
            continue;
        }
        let body = segment(cs, pos)?;
        let data = &cs[body.clone()];
        match (marker, &mut params) {
            (SIZ, None) => {
                let s = parse_siz(data, limits)?;
                params = Some(Params::new(s.comps.len()));
                siz = Some(s);
            }
            (SIZ, Some(_)) => return Err(JpxError::Format("second SIZ marker segment")),
            (CAP, _) => {
                // Pcap bit 15 from the top: Part 15 code-blocks are present.
                let mut f = Fields(data);
                if f.u32()? & (1 << 17) != 0 {
                    return Err(JpxError::Unsupported("HTJ2K (Part 15) codestream"));
                }
            }
            (PPM, _) => {
                let z = Fields(data).u8()?;
                ppm.push((z, body.start + 1..body.end));
            }
            (PPT, _) => return Err(JpxError::Format("PPT in the main header")),
            (_, Some(p)) => p.apply(marker, data, false)?,
            (_, None) => return Err(JpxError::Format("marker segment before SIZ")),
        }
        pos = body.end;
    }
    match (siz, params) {
        (Some(s), Some(p)) => Ok((s, p, ppm, pos)),
        _ => Err(JpxError::Format("no SIZ marker segment")),
    }
}

/// Tile index and length of a tile-part (A.4.2).
fn read_sot(body: &[u8]) -> JpxResult<(usize, usize)> {
    let mut f = Fields(body);
    let t = f.u16()? as usize;
    let psot = f.u32()? as usize;
    let _part_index = f.u8()?;
    let _part_count = f.u8()?;
    Ok((t, psot))
}

/// Parse the whole codestream's headers. `lenient` skips a damaged tile-part
/// (noted in [`Codestream::skipped`]) and goes on at the next SOT marker
/// instead of failing.
pub(crate) fn parse(cs: &[u8], limits: &Limits, lenient: bool) -> JpxResult<Codestream> {
    let (siz, params, ppm, mut pos) = main_header(cs, limits)?;
    let n_tiles = (siz.tiles_x * siz.tiles_y) as usize;
    let mut tiles: Vec<Option<TileParts>> = Vec::new();
    tiles.resize_with(n_tiles, || None);
    let mut ppm = PpmReader::new(cs, ppm);
    let mut skipped = Vec::new();
    // A damaged tile-part: an error, or (lenient) skip to the next SOT.
    macro_rules! damaged {
        ($tile:expr, $err:expr, $sot:expr) => {{
            if !lenient {
                return Err($err);
            }
            skipped.push(($tile, err_text(&$err)));
            match resync(cs, $sot + 2) {
                Some(p) => {
                    pos = p;
                    continue;
                }
                None => break,
            }
        }};
    }
    // Whatever ends the run of tile-parts (EOC, the end of the data, garbage),
    // those read so far stand; missing packets show up when they are needed.
    // (Not even a lenient parse looks for tile-parts past that point: what
    // strict mode decodes, lenient mode must decode the same.)
    while let Ok(SOT) = next_marker(cs, pos) {
        let sot = pos;
        let Ok(body) = segment(cs, pos) else {
            break;
        };
        let (t, psot) = match read_sot(&cs[body.clone()]) {
            Ok(v) => v,
            Err(e) => damaged!(None, e, sot),
        };
        if psot != 0 && psot < 14 {
            damaged!(
                u32::try_from(t).ok(),
                JpxError::Format("tile-part length"),
                sot
            );
        }
        let Some(slot) = tiles.get_mut(t) else {
            damaged!(None, JpxError::Format("tile index out of range"), sot);
        };
        let tile = slot.get_or_insert_with(TileParts::default);
        // What this tile-part adds, taken back if it turns out damaged.
        let (marks, ppts) = (tile.markers.len(), tile.ppt.len());
        let mut end = if psot == 0 {
            // Up to EOC, for the last tile-part.
            match cs.last_chunk::<2>() {
                Some(&[0xFF, 0xD9]) => cs.len() - 2,
                _ => cs.len(),
            }
        } else {
            pos.saturating_add(psot)
        };
        if end > cs.len() {
            end = cs.len();
            tile.truncated = true;
        }
        let index = tile.bodies.len();
        pos = body.end;
        let mut complete = false;
        let mut failure = None;
        while let Ok(marker) = next_marker(&cs[..end], pos) {
            if marker == SOD {
                pos += 2;
                complete = true;
                break;
            }
            if is_bare(marker) {
                pos += 2;
                continue;
            }
            let Ok(body) = segment(&cs[..end], pos) else {
                break;
            };
            match marker {
                SOT | EOC | SIZ => {
                    failure = Some(JpxError::Format("marker in a tile-part header"));
                    break;
                }
                PPT => match Fields(&cs[body.clone()]).u8() {
                    Ok(z) => tile.ppt.push((index, z, body.start + 1..body.end)),
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                },
                COD | COC | QCD | QCC | RGN | POC => tile.markers.push((marker, body.clone())),
                _ => {}
            }
            pos = body.end;
        }
        if failure.is_none() && !complete {
            if tile.truncated {
                break;
            }
            failure = Some(JpxError::Format("tile-part header"));
        }
        if let Some(e) = failure {
            tile.markers.truncate(marks);
            tile.ppt.truncate(ppts);
            damaged!(u32::try_from(t).ok(), e, sot);
        }
        match ppm.next_part() {
            Ok(Some(headers)) => tile.ppm.push(headers),
            Ok(None) => {}
            Err(e) => {
                tile.markers.truncate(marks);
                tile.ppt.truncate(ppts);
                damaged!(u32::try_from(t).ok(), e, sot)
            }
        }
        tile.bodies.push(pos..end.max(pos));
        pos = end.max(pos);
    }
    Ok(Codestream {
        siz,
        params,
        tiles,
        ended: next_marker(cs, pos) == Ok(EOC),
        skipped,
    })
}

fn err_text(e: &JpxError) -> &'static str {
    match e {
        JpxError::Format(m)
        | JpxError::Unsupported(m)
        | JpxError::Limit(m)
        | JpxError::Usage(m) => m,
        JpxError::Truncated => "tile-part header cut short",
    }
}

/// Splits the concatenated PPM data into one range list per tile-part (A.7.4).
struct PpmReader<'a> {
    cs: &'a [u8],
    parts: Vec<Range<usize>>,
    index: usize,
}

impl<'a> PpmReader<'a> {
    fn new(cs: &'a [u8], mut ppm: PpmParts) -> Self {
        ppm.sort_by_key(|p| p.0);
        PpmReader {
            cs,
            parts: ppm.into_iter().map(|p| p.1).collect(),
            index: 0,
        }
    }

    /// Next `n` bytes, as ranges of the codestream.
    fn take(&mut self, mut n: usize) -> JpxResult<Vec<Range<usize>>> {
        let mut out = Vec::new();
        while n > 0 {
            let part = self
                .parts
                .get_mut(self.index)
                .ok_or(JpxError::Format("PPM data ends early"))?;
            let k = n.min(part.len());
            if k > 0 {
                out.push(part.start..part.start + k);
                part.start += k;
                n -= k;
            }
            if part.start >= part.end {
                self.index += 1;
            }
        }
        Ok(out)
    }

    fn next_part(&mut self) -> JpxResult<Option<Vec<Range<usize>>>> {
        if self.parts.is_empty() {
            return Ok(None);
        }
        let n = self.take(4)?.into_iter().flatten();
        let n = n.fold(0, |a, i| (a << 8) | usize::from(self.cs[i]));
        self.take(n).map(Some)
    }
}

fn parse_siz(data: &[u8], limits: &Limits) -> JpxResult<Siz> {
    let mut f = Fields(data);
    let _capabilities = f.u16()?;
    let x1 = f.u32()?;
    let y1 = f.u32()?;
    let x0 = f.u32()?;
    let y0 = f.u32()?;
    let tw = f.u32()?;
    let th = f.u32()?;
    let tx0 = f.u32()?;
    let ty0 = f.u32()?;
    let n = usize::from(f.u16()?);
    if x0 >= x1 || y0 >= y1 || tw == 0 || th == 0 {
        return Err(JpxError::Format("empty image or tile"));
    }
    let reaches = |o: u32, s: u32, lo: u32| o <= lo && u64::from(o) + u64::from(s) > u64::from(lo);
    if !reaches(tx0, tw, x0) || !reaches(ty0, th, y0) {
        return Err(JpxError::Format("first tile does not cover the image"));
    }
    if n == 0 || n > 16384 {
        return Err(JpxError::Format("component count"));
    }
    if n > limits.max_components as usize {
        return Err(JpxError::Limit("component count"));
    }
    let tiles_x = (x1 - tx0).div_ceil(tw);
    let tiles_y = (y1 - ty0).div_ceil(th);
    if u64::from(tiles_x) * u64::from(tiles_y) > 65535 {
        return Err(JpxError::Format("more than 65535 tiles"));
    }
    let mut comps = Vec::with_capacity(n);
    for _ in 0..n {
        let s = f.u8()?;
        let dx = u32::from(f.u8()?);
        let dy = u32::from(f.u8()?);
        let bits = (s & 0x7F) + 1;
        if bits > 38 || dx == 0 || dy == 0 {
            return Err(JpxError::Format("component depth or subsampling"));
        }
        comps.push(SizComp {
            bits,
            signed: s & 0x80 != 0,
            dx,
            dy,
        });
    }
    Ok(Siz {
        x0,
        y0,
        x1,
        y1,
        tw,
        th,
        tx0,
        ty0,
        tiles_x,
        tiles_y,
        comps,
    })
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn next_marker(cs: &[u8], pos: usize) -> JpxResult<u16> {
    if pos + 2 > cs.len() {
        return Err(JpxError::Truncated);
    }
    let m = be16(cs, pos);
    if m < 0xFF00 {
        return Err(JpxError::Format("marker expected"));
    }
    Ok(m)
}

/// Markers without a length field.
fn is_bare(m: u16) -> bool {
    (0xFF30..=0xFF3F).contains(&m)
}

/// The body of the marker segment at `pos` (after its length field).
fn segment(cs: &[u8], pos: usize) -> JpxResult<Range<usize>> {
    if pos + 4 > cs.len() {
        return Err(JpxError::Truncated);
    }
    let len = usize::from(be16(cs, pos + 2));
    if len < 2 {
        return Err(JpxError::Format("marker segment length"));
    }
    if pos + 2 + len > cs.len() {
        return Err(JpxError::Truncated);
    }
    Ok(pos + 4..pos + 2 + len)
}
