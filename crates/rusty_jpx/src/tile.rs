//! Tile structure and tier-2: subbands, precincts, packets, progressions
//! (ISO/IEC 15444-1 Annex B).

use crate::codestream::{ceil_shift, Params, Progression, Siz};
use crate::dwt::{Lifting, Plane};
use crate::error::{JpxError, JpxResult};
use crate::kernels::dequantize;
use crate::t1::{Segment, MAX_PLANES, STYLE_BYPASS, STYLE_TERMALL, T1};
use crate::tagtree::{Bits, TagTree};

/// Image-wide budget for structures whose size the header alone decides.
pub(crate) struct Budget {
    /// Resolutions and precincts that may still be created.
    pub structures: u64,
    /// Packet slots that may still be visited.
    pub steps: u64,
}

impl Budget {
    fn spend(counter: &mut u64, n: u64, what: &'static str) -> JpxResult<()> {
        *counter = counter.checked_sub(n).ok_or(JpxError::Limit(what))?;
        Ok(())
    }
}

/// Some bytes of a code-block, from one packet, for `passes` coding passes of
/// one codeword segment.
struct Chunk {
    start: usize,
    len: usize,
    passes: u8,
}

struct Block {
    /// Position in the tile-component's coefficient plane.
    x: u32,
    y: u32,
    w: u16,
    h: u16,
    included: bool,
    zero_planes: u8,
    lblock: u8,
    passes: u16,
    chunks: Vec<Chunk>,
}

/// One precinct's part of one subband.
struct PrecinctBand {
    /// Code-block grid width.
    cw: u32,
    inclusion: TagTree,
    zero_planes: TagTree,
    blocks: Vec<Block>,
}

struct Precinct {
    next_layer: u32,
    bands: Vec<PrecinctBand>,
}

struct Band {
    rect: [u32; 4],
    /// Where the band starts in the coefficient plane.
    px: u32,
    py: u32,
    class: usize,
    /// Magnitude bit-planes (`Mb`, E-2).
    planes: u32,
    /// Quantization step.
    step: f32,
}

struct Resolution {
    rect: [u32; 4],
    ppx: u32,
    ppy: u32,
    pw: u32,
    ph: u32,
    /// Code-block size exponents in this resolution's bands.
    xcb: u32,
    ycb: u32,
    bands: Vec<Band>,
    precincts: Vec<Option<Box<Precinct>>>,
}

pub(crate) struct TileComp {
    pub reversible: bool,
    style: u8,
    roi: u8,
    /// Resolutions kept after `reduce`.
    keep: usize,
    res: Vec<Resolution>,
}

pub(crate) struct Tile {
    rect: [u32; 4],
    pub comps: Vec<TileComp>,
    sub: Vec<(u32, u32)>,
}

/// Packet bytes of a tile: bodies, and the headers when they are packed
/// separately.
pub(crate) struct PacketData<'a> {
    pub body: &'a [u8],
    pub pos: usize,
    pub packed: Option<(&'a [u8], usize)>,
    /// Keep what a short body still has instead of failing.
    pub lenient: bool,
    /// Set when the data ended at a packet boundary.
    pub clean_end: bool,
    /// Set when a codeword segment was cut short (lenient mode).
    pub cut: bool,
}

impl PacketData<'_> {
    fn exhausted(&self) -> bool {
        match self.packed {
            Some((h, pos)) => pos >= h.len(),
            None => self.pos >= self.body.len(),
        }
    }
}

/// Floor of `v / 2^s` and ceil of it, on the precinct grid.
fn grid(lo: u32, hi: u32, s: u32) -> (u32, u32) {
    (lo.checked_shr(s).unwrap_or(0), ceil_shift(hi, s))
}

fn intersect(a: [u64; 4], b: [u32; 4]) -> Option<[u32; 4]> {
    let r = [
        a[0].max(b[0].into()),
        a[1].max(b[1].into()),
        a[2].min(b[2].into()),
        a[3].min(b[3].into()),
    ];
    (r[0] < r[2] && r[1] < r[3]).then(|| r.map(|v| v as u32))
}

impl Tile {
    pub fn new(
        siz: &Siz,
        params: &Params,
        rect: [u32; 4],
        reduce: u32,
        budget: &mut Budget,
    ) -> JpxResult<Self> {
        let mut comps = Vec::with_capacity(siz.comps.len());
        for (c, sc) in siz.comps.iter().enumerate() {
            let coding = params.coding(c)?;
            let quant = params.quant(c)?;
            let nl = u32::from(coding.levels);
            if reduce > nl {
                return Err(JpxError::Usage("reduce exceeds the decomposition levels"));
            }
            Budget::spend(&mut budget.structures, nl.into(), "resolutions")?;
            let tc = [
                rect[0].div_ceil(sc.dx),
                rect[1].div_ceil(sc.dy),
                rect[2].div_ceil(sc.dx),
                rect[3].div_ceil(sc.dy),
            ];
            let mut res = Vec::with_capacity(nl as usize + 1);
            let mut lower = [0u32; 2];
            for r in 0..=nl {
                let rr = tc.map(|v| ceil_shift(v, nl - r));
                let pp = coding.precincts[r as usize];
                let (ppx, ppy) = (u32::from(pp & 15), u32::from(pp >> 4));
                let empty = rr[0] >= rr[2] || rr[1] >= rr[3];
                let (pw, ph) = if empty {
                    (0, 0)
                } else {
                    let (a, b) = grid(rr[0], rr[2], ppx);
                    let (p, q) = grid(rr[1], rr[3], ppy);
                    (b - a, q - p)
                };
                let count = u64::from(pw) * u64::from(ph);
                Budget::spend(&mut budget.structures, count, "precincts")?;
                let inner = u32::from(r > 0);
                let mut bands = Vec::with_capacity(3);
                // (horizontal high-pass, vertical high-pass)
                let kinds: &[(u32, u32)] = if r == 0 {
                    &[(0, 0)]
                } else {
                    &[(1, 0), (0, 1), (1, 1)]
                };
                for (i, &(xo, yo)) in kinds.iter().enumerate() {
                    let index = if r == 0 {
                        0
                    } else {
                        3 * (r as usize - 1) + 1 + i
                    };
                    let (exponent, mantissa) = quant.step(index)?;
                    let nb = if r == 0 { nl } else { nl - r + 1 };
                    let edge = |v: u32, o: u32| -> u32 {
                        if nb == 0 {
                            return v;
                        }
                        // B-15: ceil((v - 2^(nb-1) * o) / 2^nb).
                        let v = i64::from(v) - (i64::from(o) << (nb - 1));
                        ((v + (1i64 << nb) - 1) >> nb).max(0) as u32
                    };
                    let gain = xo + yo;
                    let range = i32::from(sc.bits) + gain as i32;
                    let step = ((range - exponent as i32) as f32).exp2()
                        * (1.0 + mantissa as f32 / 2048.0);
                    bands.push(Band {
                        rect: [
                            edge(tc[0], xo),
                            edge(tc[1], yo),
                            edge(tc[2], xo),
                            edge(tc[3], yo),
                        ],
                        px: xo * lower[0],
                        py: yo * lower[1],
                        class: match (xo, yo) {
                            (1, 0) => 1,
                            (1, 1) => 2,
                            _ => 0,
                        },
                        planes: (u32::from(quant.guard) + exponent).saturating_sub(1),
                        step,
                    });
                }
                lower = [rr[2].saturating_sub(rr[0]), rr[3].saturating_sub(rr[1])];
                let mut precincts = Vec::new();
                precincts.resize_with(count as usize, || None);
                res.push(Resolution {
                    rect: rr,
                    ppx,
                    ppy,
                    pw,
                    ph,
                    xcb: u32::from(coding.xcb).min(ppx.saturating_sub(inner)),
                    ycb: u32::from(coding.ycb).min(ppy.saturating_sub(inner)),
                    bands,
                    precincts,
                });
            }
            comps.push(TileComp {
                reversible: coding.reversible,
                style: coding.style,
                roi: params.roi[c],
                keep: (nl - reduce) as usize + 1,
                res,
            });
        }
        Ok(Tile {
            rect,
            comps,
            sub: siz.comps.iter().map(|c| (c.dx, c.dy)).collect(),
        })
    }

    /// Read every packet of the tile.
    pub fn read_packets(
        &mut self,
        params: &Params,
        data: &mut PacketData,
        budget: &mut Budget,
    ) -> JpxResult<()> {
        for p in params.progressions() {
            self.progression(&p, params.layers, data, budget)?;
        }
        Ok(())
    }

    fn progression(
        &mut self,
        p: &Progression,
        layers: u32,
        data: &mut PacketData,
        budget: &mut Budget,
    ) -> JpxResult<()> {
        let layers = p.layer_end.min(layers);
        let comps = p.comp.start as usize..(p.comp.end as usize).min(self.comps.len());
        let most = self.comps.iter().map(|c| c.res.len()).max().unwrap_or(0);
        let levels = p.res.start as usize..(p.res.end as usize).min(most);
        match p.order {
            0 => {
                for l in 0..layers {
                    for r in levels.clone() {
                        for c in comps.clone() {
                            self.precincts_of(c, r, l, data, budget)?;
                        }
                    }
                }
            }
            1 => {
                for r in levels {
                    for l in 0..layers {
                        for c in comps.clone() {
                            self.precincts_of(c, r, l, data, budget)?;
                        }
                    }
                }
            }
            2..=4 => {
                let mut list = Vec::new();
                for c in comps {
                    for r in levels.clone() {
                        self.positions(c, r, p.order, budget, &mut list)?;
                    }
                }
                list.sort_unstable();
                for (_, c, r, k) in list {
                    for l in 0..layers {
                        self.packet(c as usize, r as usize, k as usize, l, data, budget)?;
                    }
                }
            }
            _ => return Err(JpxError::Format("progression order")),
        }
        Ok(())
    }

    fn precincts_of(
        &mut self,
        c: usize,
        r: usize,
        l: u32,
        data: &mut PacketData,
        budget: &mut Budget,
    ) -> JpxResult<()> {
        Budget::spend(&mut budget.steps, 1, "packet steps")?;
        let Some(res) = self.comps[c].res.get(r) else {
            return Ok(());
        };
        for k in 0..res.precincts.len() {
            self.packet(c, r, k, l, data, budget)?;
        }
        Ok(())
    }

    /// Append the precincts of (`c`, `r`) with their place in a
    /// position-driven progression (B.12.1.3 to B.12.1.5).
    fn positions(
        &self,
        c: usize,
        r: usize,
        order: u8,
        budget: &mut Budget,
        list: &mut Vec<([u64; 4], u32, u32, u32)>,
    ) -> JpxResult<()> {
        let comp = &self.comps[c];
        let Some(res) = comp.res.get(r) else {
            return Ok(());
        };
        Budget::spend(
            &mut budget.steps,
            res.precincts.len() as u64,
            "packet steps",
        )?;
        let d = (comp.res.len() - 1 - r) as u32;
        let (dx, dy) = self.sub[c];
        // The reference grid coordinate a precinct row or column is visited
        // at: the tile edge for a first one that starts before the tile.
        let at = |i: u32, lo: u32, pp: u32, sub: u32, tile: u32| -> u64 {
            if i == 0 && u64::from(lo) & ((1 << pp) - 1) != 0 {
                return u64::from(tile);
            }
            ((u64::from(lo >> pp) + u64::from(i)) << (pp + d)).saturating_mul(sub.into())
        };
        let (cc, rr) = (c as u64, r as u64);
        for py in 0..res.ph {
            let y = at(py, res.rect[1], res.ppy, dy, self.rect[1]);
            for px in 0..res.pw {
                let x = at(px, res.rect[0], res.ppx, dx, self.rect[0]);
                let key = match order {
                    2 => [rr, y, x, cc],
                    3 => [y, x, cc, rr],
                    _ => [cc, y, x, rr],
                };
                list.push((key, c as u32, r as u32, py * res.pw + px));
            }
        }
        Ok(())
    }

    /// Read one packet, unless an earlier progression already did.
    fn packet(
        &mut self,
        c: usize,
        r: usize,
        k: usize,
        layer: u32,
        data: &mut PacketData,
        budget: &mut Budget,
    ) -> JpxResult<()> {
        Budget::spend(&mut budget.steps, 1, "packet steps")?;
        let comp = &mut self.comps[c];
        let (style, keep) = (comp.style, r < comp.keep);
        let res = &mut comp.res[r];
        if res.precincts[k]
            .as_ref()
            .is_some_and(|p| layer < p.next_layer)
        {
            return Ok(());
        }
        if data.exhausted() {
            data.clean_end = true;
            return Err(JpxError::Truncated);
        }
        if res.precincts[k].is_none() {
            let precinct = res.new_precinct(k as u32, budget)?;
            res.precincts[k] = Some(Box::new(precinct));
        }
        let Some(precinct) = res.precincts[k].as_deref_mut() else {
            return Ok(());
        };
        precinct.next_layer = layer + 1;

        // SOP (FF91): a packet header never starts with that byte pair, nor
        // does packet body data contain it.
        if data.body[data.pos..].starts_with(&[0xFF, 0x91]) {
            data.pos = (data.pos + 6).min(data.body.len());
        }
        let (header, start) = match data.packed {
            Some((h, pos)) => (h, pos),
            None => (data.body, data.pos),
        };
        let mut bits = Bits::new(header, start);
        let mut pending: Vec<(u8, u32, u32, u8)> = Vec::new();
        if bits.bit()? != 0 {
            for (bi, band) in precinct.bands.iter_mut().enumerate() {
                read_band_header(band, bi as u8, layer, style, &mut bits, &mut pending)?;
            }
        }
        let mut pos = bits.finish()?;
        if header[pos..].starts_with(&[0xFF, 0x92]) {
            pos += 2;
        }
        match &mut data.packed {
            Some((_, p)) => *p = pos,
            None => data.pos = pos,
        }
        for (bi, block, len, passes) in pending {
            let left = data.body.len() - data.pos;
            let mut len = len as usize;
            if len > left {
                if !data.lenient {
                    return Err(JpxError::Truncated);
                }
                len = left;
                data.cut = true;
            }
            let block = &mut precinct.bands[usize::from(bi)].blocks[block as usize];
            if keep {
                block.chunks.push(Chunk {
                    start: data.pos,
                    len,
                    passes,
                });
            }
            data.pos += len;
        }
        Ok(())
    }
}

/// Coding passes the codeword segment containing pass `p` still takes,
/// counting `p` itself (D.4.2, Table D.9).
fn segment_room(style: u8, p: u32) -> u32 {
    if style & STYLE_TERMALL != 0 {
        1
    } else if style & STYLE_BYPASS == 0 {
        u32::MAX
    } else if p < 10 {
        10 - p
    } else if (p - 10) % 3 == 0 {
        2
    } else {
        1
    }
}

/// Whether pass `p` starts a codeword segment.
fn starts_segment(style: u8, p: u32) -> bool {
    p == 0
        || style & STYLE_TERMALL != 0
        || (style & STYLE_BYPASS != 0 && p >= 10 && (p - 10) % 3 != 1)
}

/// One subband's part of a packet header (B.10). Appends the codeword
/// segment contributions to `pending`: (band, block, bytes, passes).
fn read_band_header(
    band: &mut PrecinctBand,
    bi: u8,
    layer: u32,
    style: u8,
    bits: &mut Bits,
    pending: &mut Vec<(u8, u32, u32, u8)>,
) -> JpxResult<()> {
    for i in 0..band.blocks.len() {
        let (cx, cy) = (i as u32 % band.cw, i as u32 / band.cw);
        let block = &mut band.blocks[i];
        if block.included {
            if bits.bit()? == 0 {
                continue;
            }
        } else {
            let t = layer as u16 + 1;
            if band.inclusion.decode(bits, cx, cy, t)?.is_none() {
                continue;
            }
            let mut t = 1;
            block.zero_planes = loop {
                if let Some(v) = band.zero_planes.decode(bits, cx, cy, t)? {
                    break v as u8;
                }
                t += 1;
                if t > 128 {
                    return Err(JpxError::Format("zero bit-plane count"));
                }
            };
            block.included = true;
            block.lblock = 3;
        }
        let mut passes = read_pass_count(bits)?;
        while bits.bit()? != 0 {
            block.lblock += 1;
            if block.lblock > 32 {
                return Err(JpxError::Format("codeword length bits"));
            }
        }
        while passes > 0 {
            let first = u32::from(block.passes);
            let n = segment_room(style, first).min(passes);
            let width = u32::from(block.lblock) + n.ilog2();
            if width > 32 {
                return Err(JpxError::Format("codeword length bits"));
            }
            let len = bits.bits(width)?;
            block.passes = block.passes.saturating_add(n as u16);
            pending.push((bi, i as u32, len, n as u8));
            passes -= n;
        }
    }
    Ok(())
}

/// Number of new coding passes (Table B.4).
fn read_pass_count(bits: &mut Bits) -> JpxResult<u32> {
    if bits.bit()? == 0 {
        return Ok(1);
    }
    if bits.bit()? == 0 {
        return Ok(2);
    }
    let n = bits.bits(2)?;
    if n < 3 {
        return Ok(3 + n);
    }
    let n = bits.bits(5)?;
    if n < 31 {
        return Ok(6 + n);
    }
    Ok(37 + bits.bits(7)?)
}

impl Resolution {
    fn new_precinct(&self, k: u32, budget: &mut Budget) -> JpxResult<Precinct> {
        let px = u64::from((self.rect[0] >> self.ppx) + k % self.pw);
        let py = u64::from((self.rect[1] >> self.ppy) + k / self.pw);
        // Bands above the lowest resolution are half the resolution's size.
        let inner = u32::from(self.bands.len() > 1);
        let (sx, sy) = (
            self.ppx - inner.min(self.ppx),
            self.ppy - inner.min(self.ppy),
        );
        let area = [px << sx, py << sy, (px + 1) << sx, (py + 1) << sy];
        let mut bands = Vec::with_capacity(self.bands.len());
        for b in &self.bands {
            let Some(r) = intersect(area, b.rect) else {
                bands.push(PrecinctBand {
                    cw: 0,
                    inclusion: TagTree::new(0, 0),
                    zero_planes: TagTree::new(0, 0),
                    blocks: Vec::new(),
                });
                continue;
            };
            let (cx0, cx1) = grid(r[0], r[2], self.xcb);
            let (cy0, cy1) = grid(r[1], r[3], self.ycb);
            let (cw, ch) = (cx1 - cx0, cy1 - cy0);
            let count = u64::from(cw) * u64::from(ch);
            Budget::spend(&mut budget.structures, count, "code-blocks")?;
            let mut blocks = Vec::with_capacity(count as usize);
            for cy in cy0..cy1 {
                for cx in cx0..cx1 {
                    let (cx, cy) = (u64::from(cx), u64::from(cy));
                    let cell = [
                        cx << self.xcb,
                        cy << self.ycb,
                        (cx + 1) << self.xcb,
                        (cy + 1) << self.ycb,
                    ];
                    let q = intersect(cell, r).unwrap_or([r[0], r[1], r[0], r[1]]);
                    blocks.push(Block {
                        x: b.px + (q[0] - b.rect[0]),
                        y: b.py + (q[1] - b.rect[1]),
                        w: (q[2] - q[0]) as u16,
                        h: (q[3] - q[1]) as u16,
                        included: false,
                        zero_planes: 0,
                        lblock: 0,
                        passes: 0,
                        chunks: Vec::new(),
                    });
                }
            }
            bands.push(PrecinctBand {
                cw,
                inclusion: TagTree::new(cw, ch),
                zero_planes: TagTree::new(cw, ch),
                blocks,
            });
        }
        Ok(Precinct {
            next_layer: 0,
            bands,
        })
    }
}

/// Reusable tier-1 buffers.
#[derive(Default)]
pub(crate) struct Scratch {
    t1: T1,
    lifting: Lifting,
    bytes: Vec<u8>,
    segments: Vec<Segment>,
}

impl TileComp {
    /// Area of the kept resolution, on the component's reduced grid.
    pub fn rect(&self) -> [u32; 4] {
        self.res[self.keep - 1].rect
    }

    /// Decode the coefficients, then run the inverse wavelet transform.
    pub fn reconstruct(&self, body: &[u8], scratch: &mut Scratch) -> JpxResult<Plane> {
        let top = self.rect();
        let w = top[2].saturating_sub(top[0]) as usize;
        let h = top[3].saturating_sub(top[1]) as usize;
        let mut plane = Plane::zeroed(w * h, !self.reversible)?;
        for res in &self.res[..self.keep] {
            for precinct in res.precincts.iter().flatten() {
                for (band, info) in precinct.bands.iter().zip(&res.bands) {
                    for block in &band.blocks {
                        if !block.chunks.is_empty() {
                            self.block(block, info, body, scratch, &mut plane, w)?;
                        }
                    }
                }
            }
        }
        for pair in self.res[..self.keep].windows(2) {
            plane.synthesize(w, pair[0].rect, pair[1].rect, &mut scratch.lifting)?;
        }
        Ok(plane)
    }

    fn block(
        &self,
        block: &Block,
        band: &Band,
        body: &[u8],
        s: &mut Scratch,
        plane: &mut Plane,
        stride: usize,
    ) -> JpxResult<()> {
        let planes =
            (band.planes + u32::from(self.roi)).saturating_sub(u32::from(block.zero_planes));
        if planes == 0 {
            return Ok(());
        }
        if planes > MAX_PLANES {
            return Err(JpxError::Unsupported("more than 30 magnitude bit-planes"));
        }
        // Segments in one piece are read where they are; a segment spread over
        // several packets has to be put together, and the others with it.
        let mut whole = true;
        let mut pass = 0;
        for chunk in &block.chunks {
            whole &= starts_segment(self.style, pass);
            pass += u32::from(chunk.passes);
        }
        s.bytes.clear();
        s.segments.clear();
        pass = 0;
        for chunk in &block.chunks {
            let mut range = chunk.start..chunk.start + chunk.len;
            if !whole {
                let start = s.bytes.len();
                s.bytes.extend_from_slice(&body[range]);
                range = start..s.bytes.len();
            }
            match s.segments.last_mut() {
                Some(last) if !starts_segment(self.style, pass) => {
                    last.passes += u32::from(chunk.passes);
                    last.end = range.end;
                }
                _ => s.segments.push(Segment {
                    passes: u32::from(chunk.passes),
                    start: range.start,
                    end: range.end,
                }),
            }
            pass += u32::from(chunk.passes);
        }
        let bytes = if whole { body } else { &s.bytes };
        let (w, h) = (usize::from(block.w), usize::from(block.h));
        s.t1.decode(w, h, planes, band.class, self.style, bytes, &s.segments);
        if (1..MAX_PLANES).contains(&u32::from(self.roi)) {
            // Maxshift (H.1): anything at or above 2^s belongs to the region.
            let shift = u32::from(self.roi);
            for v in &mut s.t1.data {
                let m = *v & 0x7FFF_FFFF;
                if m >> (shift + 1) != 0 {
                    *v = (*v & 0x8000_0000) | (m >> shift);
                }
            }
        }
        let first = block.y as usize * stride + block.x as usize;
        let step = plane.reals.then_some(band.step);
        dequantize(&s.t1.data, (w, h), &mut plane.data[first..], stride, step);
        Ok(())
    }
}
