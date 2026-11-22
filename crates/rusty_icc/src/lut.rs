//! Multi-dimensional LUT tags: `lut8Type` (`mft1`), `lut16Type` (`mft2`),
//! `lutAToBType` (`mAB `) and `lutBToAType` (`mBA `), their parsing,
//! encoding and evaluation (tetrahedral for 3 inputs, recursive linear over
//! tetrahedral for 4 and more).

use crate::bytes::{Reader, Writer};
use crate::color::Mat3;
use crate::curve::{clamp01, Curve};
use crate::error::{Error, Result};

/// Maximum channels an ICC LUT may declare (ICC.1:2022 caps grids at 16
/// dimensions; colour spaces stop at 15 channels).
pub(crate) const MAX_CHANNELS: usize = 15;

/// A colour lookup table: `grid[i]` points along input `i`, `outputs`
/// values per point. `data` is row-major with the FIRST input varying
/// slowest (ICC order), every value `0..=65535` for `0.0..=1.0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clut {
    /// Grid points per input channel (each at least 2).
    pub grid: Vec<u8>,
    /// Output channels per grid point.
    pub outputs: usize,
    /// Grid values, `grid.iter().product() * outputs` entries.
    pub data: Vec<u16>,
    /// Bytes per entry when encoded: 1 or 2 (8-bit entries are held here
    /// multiplied by 257).
    pub precision: u8,
}

impl Clut {
    fn entries(grid: &[u8], outputs: usize) -> Result<usize> {
        let mut n = outputs;
        for &g in grid {
            if g < 2 {
                return Err(Error::Malformed("CLUT grid needs at least 2 points"));
            }
            n = n
                .checked_mul(usize::from(g))
                .ok_or(Error::Malformed("CLUT size overflows"))?;
        }
        Ok(n)
    }

    fn parse_ab(r: &mut Reader<'_>, inputs: usize, outputs: usize) -> Result<Self> {
        let grid_bytes = r.bytes(16, "CLUT grid")?;
        let grid = grid_bytes[..inputs].to_vec();
        let precision = r.u8("CLUT precision")?;
        r.skip(3, "CLUT padding")?;
        let n = Self::entries(&grid, outputs)?;
        let data = match precision {
            1 => r.u8_as_u16_vec(n, "CLUT data")?,
            2 => r.u16_vec(n, "CLUT data")?,
            _ => return Err(Error::Malformed("CLUT precision must be 1 or 2")),
        };
        Ok(Self {
            grid,
            outputs,
            data,
            precision,
        })
    }

    fn encode_ab(&self, w: &mut Writer) {
        let mut g = [0u8; 16];
        for (d, s) in g.iter_mut().zip(&self.grid) {
            *d = *s;
        }
        w.bytes(&g);
        w.u8(self.precision);
        w.zeros(3);
        self.encode_data(w);
        w.pad4();
    }

    fn encode_data(&self, w: &mut Writer) {
        if self.precision == 1 {
            w.u8s_from_u16(&self.data);
        } else {
            w.u16s(&self.data);
        }
    }
}

/// `lut8Type` / `lut16Type` contents: an optional 3x3 matrix (applied when
/// there are 3 inputs and it is not the identity, as `LittleCMS` does), input
/// tables, a CLUT and output tables. Table entries are `0..=65535` (8-bit
/// tables are held multiplied by 257).
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyLut {
    /// Row-major 3x3 matrix, `s15Fixed16Number` values.
    pub matrix: [f64; 9],
    /// One table per input channel.
    pub input_tables: Vec<Vec<u16>>,
    /// The colour lookup table.
    pub clut: Clut,
    /// One table per output channel.
    pub output_tables: Vec<Vec<u16>>,
}

/// `lutAToBType` / `lutBToAType` contents. Any element but the B curves may
/// be absent. Processing order is A, CLUT, M, matrix, B for `mAB ` and
/// B, matrix, M, CLUT, A for `mBA `.
#[derive(Debug, Clone, PartialEq)]
pub struct AbLut {
    /// Input channels.
    pub inputs: usize,
    /// Output channels.
    pub outputs: usize,
    /// A curves.
    pub a: Option<Vec<Curve>>,
    /// The CLUT.
    pub clut: Option<Clut>,
    /// M curves.
    pub m: Option<Vec<Curve>>,
    /// 3x3 matrix then 3 offsets, `[e00..e22, e03, e13, e23]`.
    pub matrix: Option<[f64; 12]>,
    /// B curves (always present).
    pub b: Vec<Curve>,
}

/// A LUT-based transform tag (`A2Bx`, `B2Ax`, `gamt` and friends).
#[derive(Debug, Clone, PartialEq)]
pub enum Lut {
    /// `lut8Type` (`mft1`).
    Lut8(LegacyLut),
    /// `lut16Type` (`mft2`).
    Lut16(LegacyLut),
    /// `lutAToBType` (`mAB `).
    AToB(AbLut),
    /// `lutBToAType` (`mBA `).
    BToA(AbLut),
}

impl Lut {
    /// Input channel count.
    #[must_use]
    pub fn inputs(&self) -> usize {
        match self {
            Self::Lut8(l) | Self::Lut16(l) => l.input_tables.len(),
            Self::AToB(l) | Self::BToA(l) => l.inputs,
        }
    }

    /// Output channel count.
    #[must_use]
    pub fn outputs(&self) -> usize {
        match self {
            Self::Lut8(l) | Self::Lut16(l) => l.output_tables.len(),
            Self::AToB(l) | Self::BToA(l) => l.outputs,
        }
    }

    pub(crate) fn parse_legacy(r: &mut Reader<'_>, sixteen: bool) -> Result<Self> {
        r.skip(8, "lut header")?; // type signature + reserved
        let inputs = usize::from(r.u8("lut inputs")?);
        let outputs = usize::from(r.u8("lut outputs")?);
        let grid = r.u8("lut grid")?;
        r.skip(1, "lut padding")?;
        if !(1..=MAX_CHANNELS).contains(&inputs) || !(1..=MAX_CHANNELS).contains(&outputs) {
            return Err(Error::Malformed("lut channel count"));
        }
        let mut matrix = [0.0; 9];
        for m in &mut matrix {
            *m = r.s15f16("lut matrix")?;
        }
        let (in_n, out_n) = if sixteen {
            let a = usize::from(r.u16("lut16 input entries")?);
            let b = usize::from(r.u16("lut16 output entries")?);
            if !(2..=4096).contains(&a) || !(2..=4096).contains(&b) {
                return Err(Error::Malformed("lut16 table entry count"));
            }
            (a, b)
        } else {
            (256, 256)
        };
        let read_tables = |r: &mut Reader<'_>, ch: usize, n: usize| -> Result<Vec<Vec<u16>>> {
            let mut v = Vec::new();
            for _ in 0..ch {
                v.push(if sixteen {
                    r.u16_vec(n, "lut tables")?
                } else {
                    r.u8_as_u16_vec(n, "lut tables")?
                });
            }
            Ok(v)
        };
        let input_tables = read_tables(r, inputs, in_n)?;
        let grid_v = vec![grid; inputs];
        let n = Clut::entries(&grid_v, outputs)?;
        let data = if sixteen {
            r.u16_vec(n, "lut CLUT")?
        } else {
            r.u8_as_u16_vec(n, "lut CLUT")?
        };
        let clut = Clut {
            grid: grid_v,
            outputs,
            data,
            precision: if sixteen { 2 } else { 1 },
        };
        let output_tables = read_tables(r, outputs, out_n)?;
        let l = LegacyLut {
            matrix,
            input_tables,
            clut,
            output_tables,
        };
        Ok(if sixteen {
            Self::Lut16(l)
        } else {
            Self::Lut8(l)
        })
    }

    pub(crate) fn parse_ab(r: &Reader<'_>, a_to_b: bool) -> Result<Self> {
        let mut h = *r;
        h.skip(8, "mAB header")?;
        let inputs = usize::from(h.u8("mAB inputs")?);
        let outputs = usize::from(h.u8("mAB outputs")?);
        h.skip(2, "mAB padding")?;
        if !(1..=MAX_CHANNELS).contains(&inputs) || !(1..=MAX_CHANNELS).contains(&outputs) {
            return Err(Error::Malformed("mAB/mBA channel count"));
        }
        let off_b = h.u32("mAB offsets")? as usize;
        let off_mat = h.u32("mAB offsets")? as usize;
        let off_m = h.u32("mAB offsets")? as usize;
        let off_clut = h.u32("mAB offsets")? as usize;
        let off_a = h.u32("mAB offsets")? as usize;
        // Channel count at each element: mAB is in -> A -> CLUT -> out;
        // mBA is in -> B, matrix, M -> CLUT -> A -> out.
        let (a_n, bm_n) = if a_to_b {
            (inputs, outputs)
        } else {
            (outputs, inputs)
        };
        let curves = |off: usize, n: usize| -> Result<Vec<Curve>> {
            let mut c = *r;
            c.seek(off, "mAB curve offset")?;
            let mut v = Vec::new();
            for _ in 0..n {
                let start = c.pos();
                v.push(Curve::parse(&mut c)?);
                let used = c.pos() - start;
                let padded = (used + 3) & !3;
                // The last curve's padding may run past the tag end.
                let _ = c.skip(padded - used, "curve padding");
            }
            Ok(v)
        };
        if off_b == 0 {
            return Err(Error::Malformed("mAB/mBA without B curves"));
        }
        let b = curves(off_b, bm_n)?;
        let m = if off_m == 0 {
            None
        } else {
            Some(curves(off_m, bm_n)?)
        };
        let a = if off_a == 0 {
            None
        } else {
            Some(curves(off_a, a_n)?)
        };
        let matrix = if off_mat == 0 {
            None
        } else {
            if bm_n != 3 {
                return Err(Error::Malformed("mAB/mBA matrix needs 3 channels"));
            }
            let mut c = *r;
            c.seek(off_mat, "mAB matrix offset")?;
            let mut mm = [0.0; 12];
            for v in &mut mm {
                *v = c.s15f16("mAB matrix")?;
            }
            Some(mm)
        };
        let clut = if off_clut == 0 {
            if inputs != outputs {
                return Err(Error::Malformed(
                    "mAB/mBA without CLUT must keep channel count",
                ));
            }
            None
        } else {
            let mut c = *r;
            c.seek(off_clut, "mAB CLUT offset")?;
            Some(Clut::parse_ab(&mut c, inputs, outputs)?)
        };
        let l = AbLut {
            inputs,
            outputs,
            a,
            clut,
            m,
            matrix,
            b,
        };
        Ok(if a_to_b { Self::AToB(l) } else { Self::BToA(l) })
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        match self {
            Self::Lut8(l) | Self::Lut16(l) => {
                let sixteen = matches!(self, Self::Lut16(_));
                w.sig(if sixteen { *b"mft2" } else { *b"mft1" });
                w.u32(0);
                w.u8(l.input_tables.len() as u8);
                w.u8(l.output_tables.len() as u8);
                w.u8(l.clut.grid.first().copied().unwrap_or(0));
                w.u8(0);
                for &m in &l.matrix {
                    w.s15f16(m);
                }
                let put = |w: &mut Writer, t: &Vec<u16>| {
                    if sixteen {
                        w.u16s(t);
                    } else {
                        w.u8s_from_u16(t);
                    }
                };
                if sixteen {
                    w.u16(l.input_tables.first().map_or(0, Vec::len) as u16);
                    w.u16(l.output_tables.first().map_or(0, Vec::len) as u16);
                }
                for t in &l.input_tables {
                    put(w, t);
                }
                l.clut.encode_data(w);
                for t in &l.output_tables {
                    put(w, t);
                }
            }
            Self::AToB(l) | Self::BToA(l) => {
                let a_to_b = matches!(self, Self::AToB(_));
                let start = w.len();
                w.sig(if a_to_b { *b"mAB " } else { *b"mBA " });
                w.u32(0);
                w.u8(l.inputs as u8);
                w.u8(l.outputs as u8);
                w.u16(0);
                let offsets_at = w.len();
                w.zeros(20);
                let mut offs = [0u32; 5]; // B, matrix, M, CLUT, A
                let curves = |w: &mut Writer, cs: &[Curve]| {
                    for c in cs {
                        c.encode(w);
                        w.pad4();
                    }
                };
                offs[0] = (w.len() - start) as u32;
                curves(w, &l.b);
                if let Some(m) = &l.matrix {
                    offs[1] = (w.len() - start) as u32;
                    for &v in m {
                        w.s15f16(v);
                    }
                }
                if let Some(m) = &l.m {
                    offs[2] = (w.len() - start) as u32;
                    curves(w, m);
                }
                if let Some(c) = &l.clut {
                    offs[3] = (w.len() - start) as u32;
                    c.encode_ab(w);
                }
                if let Some(a) = &l.a {
                    offs[4] = (w.len() - start) as u32;
                    curves(w, a);
                }
                for (i, o) in offs.iter().enumerate() {
                    let p = offsets_at + 4 * i;
                    w.buf[p..p + 4].copy_from_slice(&o.to_be_bytes());
                }
            }
        }
    }

    /// The evaluation stages, in processing order, over values normalised
    /// to `0..=1`.
    pub(crate) fn stages(&self) -> Vec<Stage> {
        let mut s = Vec::new();
        match self {
            Self::Lut8(l) | Self::Lut16(l) => {
                let m = l.matrix;
                let is_identity = m == [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
                if l.input_tables.len() == 3 && !is_identity {
                    s.push(Stage::Matrix {
                        m: [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]],
                        off: [0.0; 3],
                    });
                }
                s.push(Stage::Curves(
                    l.input_tables.iter().cloned().map(Curve::Table).collect(),
                ));
                s.push(Stage::Clut(ClutEval::new(&l.clut)));
                s.push(Stage::Curves(
                    l.output_tables.iter().cloned().map(Curve::Table).collect(),
                ));
            }
            Self::AToB(l) => {
                if let Some(a) = &l.a {
                    s.push(Stage::Curves(a.clone()));
                }
                if let Some(c) = &l.clut {
                    s.push(Stage::Clut(ClutEval::new(c)));
                }
                if let Some(m) = &l.m {
                    s.push(Stage::Curves(m.clone()));
                }
                if let Some(m) = &l.matrix {
                    s.push(Stage::from_ab_matrix(m));
                }
                s.push(Stage::Curves(l.b.clone()));
            }
            Self::BToA(l) => {
                s.push(Stage::Curves(l.b.clone()));
                if let Some(m) = &l.matrix {
                    s.push(Stage::from_ab_matrix(m));
                }
                if let Some(m) = &l.m {
                    s.push(Stage::Curves(m.clone()));
                }
                if let Some(c) = &l.clut {
                    s.push(Stage::Clut(ClutEval::new(c)));
                }
                if let Some(a) = &l.a {
                    s.push(Stage::Curves(a.clone()));
                }
            }
        }
        s
    }
}

/// One evaluation stage of a LUT over `0..=1` values.
#[derive(Debug, Clone)]
pub(crate) enum Stage {
    Curves(Vec<Curve>),
    Matrix { m: Mat3, off: [f64; 3] },
    Clut(ClutEval),
}

/// Switches every CLUT stage in `stages` to trilinear interpolation.
pub(crate) fn make_trilinear(stages: &mut [Stage]) {
    for s in stages {
        if let Stage::Clut(c) = s {
            c.trilinear = true;
        }
    }
}

impl Stage {
    fn from_ab_matrix(m: &[f64; 12]) -> Self {
        Self::Matrix {
            m: [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]],
            off: [m[9], m[10], m[11]],
        }
    }

    /// Applies the stage to `v[..n]`, returning the new channel count.
    pub(crate) fn apply(&self, v: &mut [f64; 16], n: usize) -> usize {
        match self {
            Self::Curves(cs) => {
                for (x, c) in v.iter_mut().zip(cs) {
                    *x = c.eval(*x);
                }
                n
            }
            Self::Matrix { m, off } => {
                let x = [v[0], v[1], v[2]];
                for i in 0..3 {
                    v[i] = m[i][0] * x[0] + m[i][1] * x[1] + m[i][2] * x[2] + off[i];
                }
                n
            }
            Self::Clut(c) => {
                // A whole-array copy (constant length, inlined) instead of a
                // runtime-length `memcpy`; `eval` reads only `grid.len()`
                // (== n) inputs.
                let input = *v;
                c.eval(&input[..c.grid.len()], v);
                c.outputs
            }
        }
    }
}

/// The `N` values of the grid point at flat offset `at`, when wholly inside
/// `data`.
#[inline(always)]
fn tri_corner<const N: usize>(data: &[f32], at: usize) -> Option<&[f32; N]> {
    data.get(at..)?.first_chunk::<N>()
}

/// Trilinear interpolation along the last input, from the grid point at
/// `at` to the one `s2` further, at fraction `f2`.
#[inline(always)]
fn tri_line<const N: usize>(data: &[f32], at: usize, s2: usize, f2: f64) -> Option<[f64; N]> {
    let (pa, pb) = (tri_corner::<N>(data, at)?, tri_corner::<N>(data, at + s2)?);
    let mut r = [0.0f64; N];
    for o in 0..N {
        let (a, b) = (f64::from(pa[o]), f64::from(pb[o]));
        r[o] = a + (b - a) * f2;
    }
    Some(r)
}

/// Along the middle input between two [`tri_line`]s, the upper skipped when
/// its step or fraction is zero.
#[inline(always)]
fn tri_plane<const N: usize>(
    data: &[f32],
    at: usize,
    (s1, s2): (usize, usize),
    (f1, f2): (f64, f64),
) -> Option<[f64; N]> {
    let mut lo = tri_line::<N>(data, at, s2, f2)?;
    if s1 != 0 && f1 != 0.0 {
        let hi = tri_line::<N>(data, at + s1, s2, f2)?;
        for o in 0..N {
            lo[o] += (hi[o] - lo[o]) * f1;
        }
    }
    Some(lo)
}

/// A CLUT ready for interpolation: `f32` values over `0..=1`.
#[derive(Debug, Clone)]
pub(crate) struct ClutEval {
    grid: Vec<usize>,
    strides: Vec<usize>,
    pub(crate) outputs: usize,
    data: Vec<f32>,
    /// Trilinear instead of tetrahedral for 3 inputs. `LittleCMS` uses it
    /// for every CLUT of an output (`B2Ax`) LUT whose PCS is Lab
    /// (`ChangeInterpolationToTrilinear` in `cmsio1.c`): Lab is not a space
    /// where tetrahedral splitting is well behaved.
    pub(crate) trilinear: bool,
}

/// A tetrahedral CLUT viewed for a loop of lookups with exactly `IN` inputs
/// (3 or 4) and `OUT` outputs: the per-axis sizes and strides read once into
/// arrays and the kernel fixed, so nothing about the shape of the CLUT is
/// tested per lookup.
pub(crate) struct FixedClut<'a, const IN: usize, const OUT: usize> {
    clut: &'a ClutEval,
    grid: [usize; IN],
    strides: [usize; IN],
}

impl<const IN: usize, const OUT: usize> FixedClut<'_, IN, OUT> {
    /// Grid points along input `i`.
    pub(crate) fn points(&self, i: usize) -> usize {
        self.grid[i]
    }

    /// The lookup at the given `(cell, fraction)` per axis (from
    /// [`ClutEval::axis`]) into `out[..OUT]`: exactly what
    /// [`ClutEval::eval_axes`] gives.
    #[inline(always)]
    pub(crate) fn at(&self, cells: &[(usize, f64); IN], out: &mut [f64; 16]) {
        let mut origin = 0usize;
        let mut frac = [0.0f64; IN];
        let mut step = [0usize; IN];
        for i in 0..IN {
            let (i0, f) = cells[i];
            origin += i0 * self.strides[i];
            frac[i] = f;
            step[i] = if i0 + 1 < self.grid[i] {
                self.strides[i]
            } else {
                0
            };
        }
        let done = match (frac.as_slice(), step.as_slice()) {
            (&[f0, f1, f2], &[s0, s1, s2]) => {
                self.clut
                    .tetra_walk::<OUT>(origin, [f0, f1, f2], [s0, s1, s2], out)
            }
            (&[f0, f1, f2, f3], &[s0, s1, s2, s3]) => {
                self.clut
                    .tetra4_walk::<OUT>(origin, [f0, f1, f2, f3], [s0, s1, s2, s3], out)
            }
            _ => false,
        };
        if !done {
            self.clut.eval_cells(cells, out);
        }
    }
}

/// A CLUT of exactly three inputs and `N` outputs viewed for a loop of
/// lookups in line: the per-axis sizes and strides read once into arrays.
/// Unlike [`FixedClut`] it takes trilinear CLUTs (every Lab-indexed output
/// table) too.
pub(crate) struct Clut3Ref<'a, const N: usize> {
    clut: &'a ClutEval,
    grid: [usize; 3],
    strides: [usize; 3],
}

impl<'a, const N: usize> Clut3Ref<'a, N> {
    /// `None` unless `clut` has exactly three inputs and `N` outputs.
    pub(crate) fn new(clut: &'a ClutEval) -> Option<Self> {
        if clut.outputs != N {
            return None;
        }
        Some(Self {
            clut,
            grid: clut.grid.as_slice().try_into().ok()?,
            strides: clut.strides.as_slice().try_into().ok()?,
        })
    }

    pub(crate) fn trilinear(&self) -> bool {
        self.clut.trilinear
    }

    /// The lookup at `x` into `out[..N]`: exactly what [`ClutEval::eval`]
    /// gives (its three-input front end and kernel, in line), with the
    /// interpolation `TRILINEAR` says the CLUT uses.
    #[inline(always)]
    pub(crate) fn lookup<const TRILINEAR: bool>(&self, x: [f64; 3], out: &mut [f64; 16]) {
        let [g0, g1, g2] = self.grid;
        let [s0, s1, s2] = self.strides;
        let (i0, f0) = ClutEval::axis(g0, x[0]);
        let (i1, f1) = ClutEval::axis(g1, x[1]);
        let (i2, f2) = ClutEval::axis(g2, x[2]);
        let origin = i0 * s0 + i1 * s1 + i2 * s2;
        let f = [f0, f1, f2];
        let step = [
            if i0 + 1 < g0 { s0 } else { 0 },
            if i1 + 1 < g1 { s1 } else { 0 },
            if i2 + 1 < g2 { s2 } else { 0 },
        ];
        let done = if TRILINEAR {
            self.clut.trilinear_walk::<N>(origin, f, step, out)
        } else {
            self.clut.tetra_walk::<N>(origin, f, step, out)
        };
        if !done {
            self.clut.eval_declined(x, out);
        }
    }
}

impl ClutEval {
    /// [`Clut3Ref::lookup`] for the lookup a kernel declines (a corner
    /// outside the data: only a hand-built inconsistent [`Clut`]).
    #[cold]
    #[inline(never)]
    fn eval_declined(&self, x: [f64; 3], out: &mut [f64; 16]) {
        self.eval(&x, out);
    }

    /// This CLUT as a [`FixedClut`], when it is tetrahedral with exactly
    /// `IN` inputs and `OUT` outputs.
    pub(crate) fn fixed<const IN: usize, const OUT: usize>(
        &self,
    ) -> Option<FixedClut<'_, IN, OUT>> {
        if self.outputs != OUT || self.trilinear || OUT > 16 {
            return None;
        }
        Some(FixedClut {
            clut: self,
            grid: self.grid.as_slice().try_into().ok()?,
            strides: self.strides.as_slice().try_into().ok()?,
        })
    }

    /// [`ClutEval::eval_axes`] for the rare lookup a fixed kernel declines.
    #[cold]
    #[inline(never)]
    fn eval_cells<const IN: usize>(&self, cells: &[(usize, f64); IN], out: &mut [f64; 16]) {
        let mut axes = [(0usize, 0.0f64); 16];
        for (a, c) in axes.iter_mut().zip(cells) {
            *a = *c;
        }
        self.eval_axes(&axes, out);
    }

    pub(crate) fn new(c: &Clut) -> Self {
        Self::from_f32(
            c.grid.iter().map(|&g| usize::from(g)).collect(),
            c.outputs,
            c.data.iter().map(|&v| f32::from(v) / 65535.0).collect(),
        )
    }

    /// `grid` points per input (each >= 2), `data` row-major, first input
    /// slowest. The caller guarantees `data.len() == product * outputs`.
    pub(crate) fn from_f32(grid: Vec<usize>, outputs: usize, data: Vec<f32>) -> Self {
        let mut strides = vec![0; grid.len()];
        let mut s = outputs;
        for (i, &g) in grid.iter().enumerate().rev() {
            strides[i] = s;
            s *= g;
        }
        Self {
            grid,
            strides,
            outputs,
            data,
            trilinear: false,
        }
    }

    /// Interpolates at `x` (one value per input, clamped to `0..=1`) into
    /// `out[..outputs]`.
    /// Grid cell and fraction of coordinate `x` on an axis of `g` points.
    #[inline]
    pub(crate) fn axis(g: usize, x: f64) -> (usize, f64) {
        let p = clamp01(x) * (g - 1) as f64;
        // `p` is in `0..=g-1` (clamped above), so truncation is the floor;
        // `f64::floor` is a libm call on baseline x86-64 (no SSE4.1
        // `roundsd`). The conversion goes through `i64` (one `cvttsd2si`):
        // for a value in that range it is the same index as the saturating
        // `as usize`, which is a dozen instructions on x86-64.
        let i0 = (p as i64 as usize).min(g - 1);
        (i0, p - i0 as f64)
    }

    /// Grid points along input `i`.
    pub(crate) fn points(&self, i: usize) -> usize {
        self.grid[i]
    }

    pub(crate) fn eval(&self, x: &[f64], out: &mut [f64; 16]) {
        // Three inputs (RGB- and Lab-indexed CLUTs and the 33^3 device-link
        // grid) with 3 or 4 outputs: the same arithmetic on three named
        // values and straight into a fixed-width kernel, instead of three
        // zeroed 16-entry scratch arrays filled by a counted loop and a
        // pass through the recursion.
        if let ([g0, g1, g2], [s0, s1, s2], [x0, x1, x2]) = (&self.grid[..], &self.strides[..], x) {
            let (i0, f0) = Self::axis(*g0, *x0);
            let (i1, f1) = Self::axis(*g1, *x1);
            let (i2, f2) = Self::axis(*g2, *x2);
            let origin = i0 * s0 + i1 * s1 + i2 * s2;
            let (f, step) = (
                [f0, f1, f2],
                [
                    if i0 + 1 < *g0 { *s0 } else { 0 },
                    if i1 + 1 < *g1 { *s1 } else { 0 },
                    if i2 + 1 < *g2 { *s2 } else { 0 },
                ],
            );
            let done = match (self.outputs, self.trilinear) {
                (3, false) => self.kernel3::<3, false>(origin, f, step, out),
                (4, false) => self.kernel3::<4, false>(origin, f, step, out),
                (3, true) => self.kernel3::<3, true>(origin, f, step, out),
                (4, true) => self.kernel3::<4, true>(origin, f, step, out),
                _ => false,
            };
            if done {
                return;
            }
        }
        // Four inputs, tetrahedral (CMYK-indexed CLUTs and the 17^4
        // device-link grid), likewise.
        if let ([g0, g1, g2, g3], [s0, s1, s2, s3], [x0, x1, x2, x3], false) =
            (&self.grid[..], &self.strides[..], x, self.trilinear)
        {
            let (i0, f0) = Self::axis(*g0, *x0);
            let (i1, f1) = Self::axis(*g1, *x1);
            let (i2, f2) = Self::axis(*g2, *x2);
            let (i3, f3) = Self::axis(*g3, *x3);
            let origin = i0 * s0 + i1 * s1 + i2 * s2 + i3 * s3;
            let step = [
                if i0 + 1 < *g0 { *s0 } else { 0 },
                if i1 + 1 < *g1 { *s1 } else { 0 },
                if i2 + 1 < *g2 { *s2 } else { 0 },
                if i3 + 1 < *g3 { *s3 } else { 0 },
            ];
            let done = match self.outputs {
                3 => self.kernel4::<3>(origin, [f0, f1, f2, f3], step, out),
                4 => self.kernel4::<4>(origin, [f0, f1, f2, f3], step, out),
                _ => false,
            };
            if done {
                return;
            }
        }
        // The general path, for every other shape and for the rare lookup a
        // kernel declines. Kept as its own body, not routed through
        // `eval_axes`: building the 16-entry axis array first cost the
        // 4-input grid path +11.8% Ir (fog16) and the grid builds +1.5%.
        let n = self.grid.len();
        let mut base = [0usize; 16];
        let mut frac = [0.0f64; 16];
        let mut step = [0usize; 16];
        for i in 0..n {
            let g = self.grid[i];
            let (i0, f) = Self::axis(g, x[i]);
            base[i] = i0 * self.strides[i];
            frac[i] = f;
            step[i] = if i0 + 1 < g { self.strides[i] } else { 0 };
        }
        let origin: usize = base[..n].iter().sum();
        self.interp(n, 0, origin, &frac, &step, out);
    }

    /// `eval` with every input's `(cell, fraction)` already known (from
    /// [`ClutEval::axis`], e.g. tabulated per 8-bit input value).
    pub(crate) fn eval_axes(&self, axes: &[(usize, f64); 16], out: &mut [f64; 16]) {
        let n = self.grid.len();
        let mut frac = [0.0f64; 16];
        let mut step = [0usize; 16];
        let mut origin = 0usize;
        for i in 0..n {
            let (i0, f) = axes[i];
            origin += i0 * self.strides[i];
            frac[i] = f;
            step[i] = if i0 + 1 < self.grid[i] {
                self.strides[i]
            } else {
                0
            };
        }
        // Straight into the caller's array: the old scratch + runtime-length
        // `copy_from_slice` was a `memcpy` call per lookup.
        self.interp(n, 0, origin, &frac, &step, out);
    }

    fn at(&self, idx: usize, o: usize) -> f64 {
        f64::from(self.data.get(idx + o).copied().unwrap_or(0.0))
    }

    /// The `outputs` values of the grid point at flat offset `idx`: one
    /// bounds check per corner instead of one per value. Out of range (only
    /// reachable through a hand-built inconsistent [`Clut`]) reads zeros,
    /// as `at` does.
    fn corner(&self, idx: usize) -> &[f32] {
        const ZERO: [f32; MAX_CHANNELS] = [0.0; MAX_CHANNELS];
        let n = self.outputs.min(MAX_CHANNELS);
        self.data.get(idx..idx + n).unwrap_or(&ZERO[..n])
    }

    /// Interpolates dimensions `dim..n` starting at flat offset `origin`.
    fn interp(
        &self,
        n: usize,
        dim: usize,
        origin: usize,
        frac: &[f64; 16],
        step: &[usize; 16],
        out: &mut [f64; 16],
    ) {
        let rem = n - dim;
        match rem {
            0 => {
                for (o, v) in out.iter_mut().enumerate().take(self.outputs) {
                    *v = self.at(origin, o);
                }
            }
            1 => {
                let f = frac[dim];
                let s = step[dim];
                for (o, v) in out.iter_mut().enumerate().take(self.outputs) {
                    let a = self.at(origin, o);
                    let b = self.at(origin + s, o);
                    *v = a + (b - a) * f;
                }
            }
            3 if !self.trilinear => self.tetra(dim, origin, frac, step, out),
            _ => {
                // Linear along this dimension between two lower-dimensional
                // interpolations (LittleCMS's `Eval4Inputs` pattern).
                let mut hi = [0.0f64; 16];
                self.interp(n, dim + 1, origin, frac, step, out);
                if step[dim] != 0 && frac[dim] != 0.0 {
                    self.interp(n, dim + 1, origin + step[dim], frac, step, &mut hi);
                    let f = frac[dim];
                    for o in 0..self.outputs {
                        out[o] += (hi[o] - out[o]) * f;
                    }
                }
            }
        }
    }

    /// The kernel of a three-input front end of [`ClutEval::eval`], one
    /// symbol per output count and interpolation. Out of line, so `eval`
    /// stays small: with all six kernels inlined it was 3.3K instructions,
    /// and the three-input integer path (lab16in) ran 8-13% slower on the
    /// clock while it executed 30% fewer instructions (three probes).
    #[inline(never)]
    fn kernel3<const N: usize, const TRILINEAR: bool>(
        &self,
        origin: usize,
        f: [f64; 3],
        step: [usize; 3],
        out: &mut [f64; 16],
    ) -> bool {
        if TRILINEAR {
            self.trilinear_fixed::<N>(origin, f, step, out)
        } else {
            self.tetra_walk::<N>(origin, f, step, out)
        }
    }

    /// The kernel of the four-input front end of [`ClutEval::eval`], out of
    /// line likewise.
    #[inline(never)]
    fn kernel4<const N: usize>(
        &self,
        origin: usize,
        f: [f64; 4],
        step: [usize; 4],
        out: &mut [f64; 16],
    ) -> bool {
        self.tetra4_fixed::<N>(origin, f, step, out)
    }

    /// Trilinear interpolation over three inputs when there are exactly `N`
    /// output values per grid point, on corner arrays instead of a call of
    /// the recursion per line and per plane and a checked load per value.
    /// Per value it is exactly the recursion of `interp`: linear along the
    /// last input, then the middle one, then the first, an upper line or
    /// plane being skipped when its step or its fraction is zero (so the
    /// results are bit-identical). `false`, with `out` untouched, when a
    /// corner is not wholly inside the data (only a hand-built inconsistent
    /// [`Clut`]): the caller then takes the recursion, which reads missing
    /// values as zeros one by one.
    #[inline]
    fn trilinear_fixed<const N: usize>(
        &self,
        origin: usize,
        [f0, f1, f2]: [f64; 3],
        [s0, s1, s2]: [usize; 3],
        out: &mut [f64; 16],
    ) -> bool {
        self.trilinear_walk::<N>(origin, [f0, f1, f2], [s0, s1, s2], out)
    }

    /// The body of [`ClutEval::trilinear_fixed`], in line wherever it is
    /// used (as [`ClutEval::tetra_walk`] is): [`ClutEval::eval`] keeps its
    /// copy through that handle, and each float loop over a [`Clut3Ref`]
    /// takes its own (with two such loops the handle stopped being inlined
    /// into them: +48 instructions per pixel).
    #[inline(always)]
    fn trilinear_walk<const N: usize>(
        &self,
        origin: usize,
        [f0, f1, f2]: [f64; 3],
        [s0, s1, s2]: [usize; 3],
        out: &mut [f64; 16],
    ) -> bool {
        // The line and plane steps are functions forced in line: as
        // closures they were a call per plane (two per lookup) with the
        // corner state spilled around it. The arithmetic is unchanged.
        let data = &self.data[..];
        let at = origin;
        let Some(mut lo) = tri_plane::<N>(data, at, (s1, s2), (f1, f2)) else {
            return false;
        };
        if s0 != 0 && f0 != 0.0 {
            let Some(hi) = tri_plane::<N>(data, at + s0, (s1, s2), (f1, f2)) else {
                return false;
            };
            for o in 0..N {
                lo[o] += (hi[o] - lo[o]) * f0;
            }
        }
        out[..N].copy_from_slice(&lo);
        true
    }

    /// Four inputs and exactly `N` output values per grid point: linear
    /// along the first input between two tetrahedral lookups in the other
    /// three, the upper one skipped when its step or its fraction is zero,
    /// exactly as the recursion in `interp` does it. Both lookups have the
    /// same fractions, hence the same tetrahedron: it is chosen once, and
    /// each lookup is the four-corner walk of [`ClutEval::tetra_fixed`].
    /// `false`, with `out` untouched, when a corner is not wholly inside
    /// the data: the caller then takes the recursion.
    #[inline]
    fn tetra4_fixed<const N: usize>(
        &self,
        origin: usize,
        r: [f64; 4],
        step: [usize; 4],
        out: &mut [f64; 16],
    ) -> bool {
        self.tetra4_walk::<N>(origin, r, step, out)
    }

    /// The body of [`ClutEval::tetra4_fixed`], in line wherever it is used
    /// (as [`ClutEval::tetra_walk`] is): [`ClutEval::eval`] keeps the code
    /// it had through that handle, the integer grid loops take their own
    /// copy.
    #[inline(always)]
    fn tetra4_walk<const N: usize>(
        &self,
        origin: usize,
        [rw, rx, ry, rz]: [f64; 4],
        [w, x, y, z]: [usize; 4],
        out: &mut [f64; 16],
    ) -> bool {
        let far = x + y + z;
        let upper = w != 0 && rw != 0.0;
        macro_rules! walk {
            ($q1:expr, $q2:expr, $i:literal, $j:literal, $k:literal) => {{
                let Some(d) = self.corner_chain::<N>(origin, $q1, $q2, far) else {
                    return false;
                };
                let mut lo = [0.0f64; N];
                for o in 0..N {
                    lo[o] = d[0][o] + d[$i][o] * rx + d[$j][o] * ry + d[$k][o] * rz;
                }
                if upper {
                    let Some(d) = self.corner_chain::<N>(origin + w, $q1, $q2, far) else {
                        return false;
                    };
                    for o in 0..N {
                        let hi = d[0][o] + d[$i][o] * rx + d[$j][o] * ry + d[$k][o] * rz;
                        lo[o] += (hi - lo[o]) * rw;
                    }
                }
                out[..N].copy_from_slice(&lo);
            }};
        }
        if rx >= ry && ry >= rz {
            walk!(x, x + y, 1, 2, 3);
        } else if rx >= rz && rz >= ry {
            walk!(x, x + z, 1, 3, 2);
        } else if rz >= rx && rx >= ry {
            walk!(z, x + z, 2, 3, 1);
        } else if ry >= rx && rx >= rz {
            walk!(y, x + y, 2, 1, 3);
        } else if ry >= rz && rz >= rx {
            walk!(y, y + z, 3, 1, 2);
        } else {
            walk!(z, y + z, 3, 2, 1);
        }
        true
    }

    /// The grid point at `origin` and the three successive differences on
    /// the way through `origin + q1` and `origin + q2` to `origin + far`,
    /// `N` values each. `None` when one of the four is not wholly inside
    /// the data.
    #[inline(always)]
    fn corner_chain<const N: usize>(
        &self,
        origin: usize,
        q1: usize,
        q2: usize,
        far: usize,
    ) -> Option<[[f64; N]; 4]> {
        let c = |d: usize| -> Option<&[f32; N]> { self.data.get(origin + d..)?.first_chunk::<N>() };
        let (p3, p0, p1, p2) = (c(far)?, c(0)?, c(q1)?, c(q2)?);
        let mut d = [[0.0f64; N]; 4];
        for o in 0..N {
            let v0 = f64::from(p0[o]);
            let v1 = f64::from(p1[o]);
            let v2 = f64::from(p2[o]);
            let v3 = f64::from(p3[o]);
            d[0][o] = v0;
            d[1][o] = v1 - v0;
            d[2][o] = v2 - v1;
            d[3][o] = v3 - v2;
        }
        Some(d)
    }

    /// The tetrahedral sum when there are exactly `N` output values per
    /// grid point (3 and 4 cover RGB, Lab and CMYK).
    ///
    /// A tetrahedron is a walk over four corners, from the cell origin to
    /// the far corner, and its three edge differences `P(a) - P(b)` are the
    /// three steps of that walk: the general loop reads seven corner slices
    /// (three of them twice), this reads the four once, as fixed-size
    /// arrays, so the lanes are unrolled and vectorised and each corner
    /// costs one range test. Per value it is the same subtractions,
    /// products and sums in the same order, so results are bit-identical.
    ///
    /// `false`, with `out` untouched, when a corner is not wholly inside
    /// the data (only a hand-built inconsistent [`Clut`]): the caller then
    /// takes the general loop, which reads such a corner as zeros.
    #[inline]
    fn tetra_fixed<const N: usize>(
        &self,
        origin: usize,
        r: [f64; 3],
        step: [usize; 3],
        out: &mut [f64; 16],
    ) -> bool {
        self.tetra_walk::<N>(origin, r, step, out)
    }

    /// The body of [`ClutEval::tetra_fixed`], in line wherever it is used:
    /// the recursion reaches it through that handle and keeps the code it
    /// had, the front ends of [`ClutEval::eval`] take their own copy. (One
    /// shared copy called from both cost the lookups that still go through
    /// the recursion 11 instructions each: v4u8 +3.2%, cgatsu8 +3.8%.)
    #[inline(always)]
    fn tetra_walk<const N: usize>(
        &self,
        origin: usize,
        [rx, ry, rz]: [f64; 3],
        [x, y, z]: [usize; 3],
        out: &mut [f64; 16],
    ) -> bool {
        let far = x + y + z;
        // `walk!(q1, q2, i, j, k)`: the walk goes through `q1` then `q2`,
        // and its steps number `i`, `j`, `k` (1, 2, 3 in walk order) are the
        // differences the general loop multiplies by `rx`, `ry`, `rz`.
        macro_rules! walk {
            ($q1:expr, $q2:expr, $i:literal, $j:literal, $k:literal) => {{
                let Some(d) = self.corner_chain::<N>(origin, $q1, $q2, far) else {
                    return false;
                };
                for o in 0..N {
                    out[o] = d[0][o] + d[$i][o] * rx + d[$j][o] * ry + d[$k][o] * rz;
                }
            }};
        }
        if rx >= ry && ry >= rz {
            walk!(x, x + y, 1, 2, 3);
        } else if rx >= rz && rz >= ry {
            walk!(x, x + z, 1, 3, 2);
        } else if rz >= rx && rx >= ry {
            walk!(z, x + z, 2, 3, 1);
        } else if ry >= rx && rx >= rz {
            walk!(y, x + y, 2, 1, 3);
        } else if ry >= rz && rz >= rx {
            walk!(y, y + z, 3, 1, 2);
        } else {
            walk!(z, y + z, 3, 2, 1);
        }
        true
    }

    fn tetra(
        &self,
        dim: usize,
        origin: usize,
        frac: &[f64; 16],
        step: &[usize; 16],
        out: &mut [f64; 16],
    ) {
        let (rx, ry, rz) = (frac[dim], frac[dim + 1], frac[dim + 2]);
        let (x, y, z) = (step[dim], step[dim + 1], step[dim + 2]);
        let done = match self.outputs {
            3 => self.tetra_fixed::<3>(origin, [rx, ry, rz], [x, y, z], out),
            4 => self.tetra_fixed::<4>(origin, [rx, ry, rz], [x, y, z], out),
            _ => false,
        };
        if done {
            return;
        }
        // Each `(a, b)` pair is the corner offsets of `c = P(a) - P(b)`; the
        // per-channel arithmetic is the reference's, so results are
        // bit-identical. Corners are fetched as slices (one bounds check
        // each) and zipped, instead of a checked load per value.
        let [(a1, b1), (a2, b2), (a3, b3)] = if rx >= ry && ry >= rz {
            [(x, 0), (x + y, x), (x + y + z, x + y)]
        } else if rx >= rz && rz >= ry {
            [(x, 0), (x + y + z, x + z), (x + z, x)]
        } else if rz >= rx && rx >= ry {
            [(x + z, z), (x + y + z, x + z), (z, 0)]
        } else if ry >= rx && rx >= rz {
            [(x + y, y), (y, 0), (x + y + z, x + y)]
        } else if ry >= rz && rz >= rx {
            [(x + y + z, y + z), (y, 0), (y + z, y)]
        } else {
            [(x + y + z, y + z), (y + z, z), (z, 0)]
        };
        let c = |d: usize| self.corner(origin + d);
        let (p0, pa1, pb1, pa2, pb2, pa3, pb3) = (c(0), c(a1), c(b1), c(a2), c(b2), c(a3), c(b3));
        let lanes = out
            .iter_mut()
            .zip(p0)
            .zip(pa1.iter().zip(pb1))
            .zip(pa2.iter().zip(pb2))
            .zip(pa3.iter().zip(pb3));
        for ((((v, &q0), (&qa1, &qb1)), (&qa2, &qb2)), (&qa3, &qb3)) in lanes {
            let c0 = f64::from(q0);
            let c1 = f64::from(qa1) - f64::from(qb1);
            let c2 = f64::from(qa2) - f64::from(qb2);
            let c3 = f64::from(qa3) - f64::from(qb3);
            *v = c0 + c1 * rx + c2 * ry + c3 * rz;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tetrahedral_reproduces_linear_function() {
        // A CLUT sampling f(x,y,z) = (x, 0.5y + 0.25z, z) is reproduced exactly.
        let g = 5usize;
        let mut data = Vec::new();
        for i in 0..g {
            for j in 0..g {
                for k in 0..g {
                    let (x, y, z) = (i as f32 / 4.0, j as f32 / 4.0, k as f32 / 4.0);
                    data.extend_from_slice(&[x, 0.5 * y + 0.25 * z, z]);
                }
            }
        }
        let c = ClutEval::from_f32(vec![g, g, g], 3, data);
        let mut out = [0.0; 16];
        c.eval(&[0.3, 0.71, 0.05], &mut out);
        assert!((out[0] - 0.3).abs() < 1e-6);
        assert!((out[1] - (0.355 + 0.0125)).abs() < 1e-6);
        assert!((out[2] - 0.05).abs() < 1e-6);
    }

    #[test]
    fn eval_axes_matches_eval_bit_for_bit() {
        // The tabulated 8-bit path must give exactly what `eval` gives.
        let g = 17usize;
        let data: Vec<f32> = (0..g * g * g * 3)
            .map(|i| ((i * 7919) % 1000) as f32 / 999.0)
            .collect();
        let c = ClutEval::from_f32(vec![g; 3], 3, data);
        for &(r, gg, b) in &[(0u8, 0u8, 0u8), (255, 128, 1), (17, 200, 99), (64, 64, 255)] {
            let x = [r, gg, b].map(|v| f64::from(v) / 255.0);
            let mut a = [0.0; 16];
            c.eval(&x, &mut a);
            let mut axes = [(0usize, 0.0f64); 16];
            for i in 0..3 {
                axes[i] = ClutEval::axis(g, x[i]);
            }
            let mut b2 = [0.0; 16];
            c.eval_axes(&axes, &mut b2);
            assert_eq!(a.map(f64::to_bits), b2.map(f64::to_bits));
        }
    }

    #[test]
    fn axis_matches_the_saturating_cast_form() {
        // The form released in 0.1.1.
        let old = |g: usize, x: f64| {
            let p = clamp01(x) * (g - 1) as f64;
            let i0 = (p as usize).min(g - 1);
            (i0, p - i0 as f64)
        };
        let mut xs: Vec<f64> = (-50..=1050).map(|i| f64::from(i) / 1000.0).collect();
        xs.extend([
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.0,
            1.0 - f64::EPSILON,
            1.0 + f64::EPSILON,
            f64::MIN_POSITIVE,
            1.0e300,
        ]);
        for g in [2usize, 3, 9, 17, 33, 129, 255, 4096, 65536] {
            for i in 0..g {
                let x = i as f64 / (g - 1) as f64;
                xs.extend([
                    x,
                    f64::from_bits(x.to_bits().wrapping_sub(1)),
                    f64::from_bits(x.to_bits() + 1),
                ]);
            }
            for &x in &xs {
                let (a, b) = (ClutEval::axis(g, x), old(g, x));
                assert_eq!((a.0, a.1.to_bits()), (b.0, b.1.to_bits()), "g={g} x={x}");
            }
            xs.truncate(1109);
        }
    }

    /// `eval` by the general path: axis arrays, then the recursion.
    fn eval_general(c: &ClutEval, p: &[f64]) -> [f64; 16] {
        let mut frac = [0.0; 16];
        let mut step = [0usize; 16];
        let mut origin = 0;
        for (i, &v) in p.iter().enumerate() {
            let (i0, f) = ClutEval::axis(c.grid[i], v);
            origin += i0 * c.strides[i];
            frac[i] = f;
            step[i] = if i0 + 1 < c.grid[i] { c.strides[i] } else { 0 };
        }
        let mut out = [0.0; 16];
        c.interp(p.len(), 0, origin, &frac, &step, &mut out);
        out
    }

    fn bits(v: &[f64; 16], n: usize) -> Vec<u64> {
        v[..n].iter().map(|x| x.to_bits()).collect()
    }

    /// Interior points, exact grid points (zero fractions), both ends of
    /// an axis and out-of-range inputs.
    const COORDS: [f64; 10] = [-0.5, 0.0, 0.25, 0.3, 1.0 / 3.0, 0.5, 0.71, 0.75, 1.0, 1.5];

    fn test_clut(grid: &[usize], outputs: usize) -> ClutEval {
        let data: Vec<f32> = (0..grid.iter().product::<usize>() * outputs)
            .map(|i| ((i * 7919 + 13) % 1009) as f32 / 1008.0)
            .collect();
        ClutEval::from_f32(grid.to_vec(), outputs, data)
    }

    /// The tetrahedral sum written out per value with checked reads (the
    /// 0.1.1 kernel), for the tetrahedron `tetra` would pick.
    fn tetra_reference(c: &ClutEval, p: [f64; 3]) -> [f64; 16] {
        let mut r = [0.0; 3];
        let mut s = [0usize; 3];
        let mut origin = 0;
        for i in 0..3 {
            let (i0, f) = ClutEval::axis(c.grid[i], p[i]);
            origin += i0 * c.strides[i];
            r[i] = f;
            s[i] = if i0 + 1 < c.grid[i] { c.strides[i] } else { 0 };
        }
        let ([rx, ry, rz], [x, y, z]) = (r, s);
        let [(a1, b1), (a2, b2), (a3, b3)] = if rx >= ry && ry >= rz {
            [(x, 0), (x + y, x), (x + y + z, x + y)]
        } else if rx >= rz && rz >= ry {
            [(x, 0), (x + y + z, x + z), (x + z, x)]
        } else if rz >= rx && rx >= ry {
            [(x + z, z), (x + y + z, x + z), (z, 0)]
        } else if ry >= rx && rx >= rz {
            [(x + y, y), (y, 0), (x + y + z, x + y)]
        } else if ry >= rz && rz >= rx {
            [(x + y + z, y + z), (y, 0), (y + z, y)]
        } else {
            [(x + y + z, y + z), (y + z, z), (z, 0)]
        };
        let mut out = [0.0; 16];
        for (o, v) in out.iter_mut().enumerate().take(c.outputs) {
            // A corner that is not wholly inside the data reads as zeros.
            let at = |d: usize| {
                if origin + d + c.outputs <= c.data.len() {
                    f64::from(c.data[origin + d + o])
                } else {
                    0.0
                }
            };
            *v = at(0) + (at(a1) - at(b1)) * rx + (at(a2) - at(b2)) * ry + (at(a3) - at(b3)) * rz;
        }
        out
    }

    #[test]
    fn tetrahedral_kernel_matches_the_per_value_form_bit_for_bit() {
        for outputs in [1usize, 3, 4, 7] {
            let c = test_clut(&[5, 4, 3], outputs);
            for &x in &COORDS {
                for &y in &COORDS {
                    for &z in &COORDS {
                        let mut fast = [0.0; 16];
                        c.eval(&[x, y, z], &mut fast);
                        let slow = tetra_reference(&c, [x, y, z]);
                        assert_eq!(bits(&fast, outputs), bits(&slow, outputs), "{x} {y} {z}");
                        assert_eq!(
                            bits(&fast, outputs),
                            bits(&eval_general(&c, &[x, y, z]), outputs)
                        );
                    }
                }
            }
        }
        // Data shorter than the grid says: the fixed-width kernel declines
        // and the general loop reads the missing corners as zeros.
        for outputs in [3usize, 4] {
            let short = ClutEval::from_f32(vec![2, 2, 2], outputs, vec![0.5; 8 * outputs - 5]);
            for p in [
                [0.6, 0.7, 0.8],
                [0.9, 0.2, 0.1],
                [0.1, 0.2, 0.9],
                [0.0, 0.0, 0.0],
            ] {
                let mut out = [9.0; 16];
                short.eval(&p, &mut out);
                let want = tetra_reference(&short, p);
                assert_eq!(bits(&out, outputs), bits(&want, outputs), "{p:?}");
            }
            let mut out = [9.0; 16];
            short.eval(&[0.6, 0.7, 0.8], &mut out);
            assert!(out[0] < 0.5);
        }
    }

    #[test]
    fn three_input_front_end_matches_general_path_bit_for_bit() {
        for trilinear in [false, true] {
            for outputs in [1usize, 3, 4, 7] {
                let mut c = test_clut(&[5, 4, 3], outputs);
                c.trilinear = trilinear;
                for &x in &COORDS {
                    for &y in &COORDS {
                        for &z in &COORDS {
                            let mut fast = [0.0; 16];
                            c.eval(&[x, y, z], &mut fast);
                            let slow = eval_general(&c, &[x, y, z]);
                            assert_eq!(
                                bits(&fast, outputs),
                                bits(&slow, outputs),
                                "{trilinear} {x} {y} {z}"
                            );
                        }
                    }
                }
            }
        }
        // Negative zeros in the data survive a skipped blend, as in the
        // recursion (an unconditional blend would turn them positive).
        let mut neg = ClutEval::from_f32(vec![2, 2, 2], 3, vec![-0.0; 24]);
        neg.trilinear = true;
        for p in [
            [0.0, 0.0, 0.0],
            [0.0, 0.5, 0.0],
            [1.0, 0.0, 0.5],
            [0.5, 0.5, 0.5],
        ] {
            let mut fast = [9.0; 16];
            neg.eval(&p, &mut fast);
            assert_eq!(bits(&fast, 3), bits(&eval_general(&neg, &p), 3), "{p:?}");
        }
        // Data shorter than the grid says: the kernels decline and the
        // recursion reads the missing values as zeros.
        for trilinear in [false, true] {
            for outputs in [3usize, 4] {
                let data = vec![0.5; 8 * outputs - 5];
                let mut short = ClutEval::from_f32(vec![2, 2, 2], outputs, data);
                short.trilinear = trilinear;
                let mut out = [9.0; 16];
                let steps = [4 * outputs, 2 * outputs, outputs];
                assert!(!short.trilinear_fixed::<3>(0, [0.5; 3], steps, &mut out));
                assert!(!short.tetra_walk::<3>(0, [0.5; 3], steps, &mut out));
                assert_eq!(out[0], 9.0);
                for p in [[0.5, 0.5, 0.5], [0.9, 0.2, 0.1], [0.1, 0.2, 0.9]] {
                    short.eval(&p, &mut out);
                    assert_eq!(
                        bits(&out, outputs),
                        bits(&eval_general(&short, &p), outputs)
                    );
                }
            }
        }
    }

    #[test]
    fn four_input_kernel_matches_recursion_bit_for_bit() {
        const C4: [f64; 8] = [-0.5, 0.0, 0.2, 1.0 / 3.0, 0.5, 0.77, 1.0, 1.5];
        for outputs in [1usize, 3, 4, 6] {
            let c = test_clut(&[3, 4, 5, 3], outputs);
            for &w in &C4 {
                for &x in &C4 {
                    for &y in &C4 {
                        for &z in &C4 {
                            let p = [w, x, y, z];
                            let mut fast = [0.0; 16];
                            c.eval(&p, &mut fast);
                            let slow = eval_general(&c, &p);
                            assert_eq!(bits(&fast, outputs), bits(&slow, outputs), "{p:?}");
                        }
                    }
                }
            }
        }
        // Data shorter than the grid says: the kernel declines and the
        // recursion reads the missing corners as zeros.
        for outputs in [3usize, 4] {
            let short = ClutEval::from_f32(vec![2; 4], outputs, vec![0.5; 16 * outputs - 5]);
            for p in [
                [0.6, 0.7, 0.8, 0.9],
                [0.9, 0.2, 0.1, 0.3],
                [0.0, 0.2, 0.9, 0.5],
            ] {
                let mut out = [9.0; 16];
                short.eval(&p, &mut out);
                assert_eq!(
                    bits(&out, outputs),
                    bits(&eval_general(&short, &p), outputs),
                    "{p:?}"
                );
            }
        }
    }

    #[test]
    fn fixed_clut_matches_eval_axes_bit_for_bit() {
        const C: [f64; 7] = [-0.5, 0.0, 0.3, 1.0 / 3.0, 0.5, 1.0, 1.5];
        fn check<const IN: usize, const OUT: usize>(grid: [usize; IN], short: usize) {
            let mut c = test_clut(&grid, OUT);
            c.data.truncate(c.data.len() - short);
            let f = c.fixed::<IN, OUT>().expect("shape");
            for k in 0..C.len().pow(IN as u32) {
                let mut axes = [(0usize, 0.0f64); 16];
                let mut cells = [(0usize, 0.0f64); IN];
                for i in 0..IN {
                    cells[i] = ClutEval::axis(grid[i], C[k / C.len().pow(i as u32) % C.len()]);
                    axes[i] = cells[i];
                }
                let (mut fast, mut slow) = ([7.0; 16], [7.0; 16]);
                f.at(&cells, &mut fast);
                c.eval_axes(&axes, &mut slow);
                assert_eq!(bits(&fast, 16), bits(&slow, 16), "{grid:?} {cells:?}");
            }
        }
        for short in [0, 5] {
            check::<3, 3>([5, 4, 3], short);
            check::<3, 4>([5, 4, 3], short);
            check::<4, 3>([3, 4, 5, 3], short);
            check::<4, 4>([3, 4, 5, 3], short);
        }
        let mut c = test_clut(&[3, 3, 3], 3);
        assert!(c.fixed::<3, 4>().is_none());
        assert!(c.fixed::<4, 3>().is_none());
        c.trilinear = true;
        assert!(c.fixed::<3, 3>().is_none());
    }

    #[test]
    fn four_d_reproduces_linear_function() {
        let g = 3usize;
        let mut data = Vec::new();
        for i in 0..g {
            for j in 0..g {
                for k in 0..g {
                    for l in 0..g {
                        let s = (i + j + k + l) as f32 / 8.0;
                        data.push(s);
                    }
                }
            }
        }
        let c = ClutEval::from_f32(vec![g; 4], 1, data);
        let mut out = [0.0; 16];
        c.eval(&[0.1, 0.9, 0.4, 0.6], &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6);
    }
}
