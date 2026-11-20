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

impl ClutEval {
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
        // `roundsd`), the cast is one `cvttsd2si`.
        let i0 = (p as usize).min(g - 1);
        (i0, p - i0 as f64)
    }

    /// Grid points along input `i`.
    pub(crate) fn points(&self, i: usize) -> usize {
        self.grid[i]
    }

    pub(crate) fn eval(&self, x: &[f64], out: &mut [f64; 16]) {
        // Kept as its own body, not routed through `eval_axes`: building the
        // 16-entry axis array first cost the 4-input grid path +11.8% Ir
        // (fog16) and the grid builds +1.5%.
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
            step[i] = if i0 + 1 < self.grid[i] { self.strides[i] } else { 0 };
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
