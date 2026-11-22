//! Building and evaluating profile chains: device-to-PCS and PCS-to-device
//! sides, the PCS connection (absolute-colorimetric scaling and black-point
//! compensation) and the black-point detection they need. The structure
//! follows `LittleCMS` 2 (`cmsio1.c`, `cmscnvrt.c`, `cmssamp.c`) so results
//! are comparable with it.

use crate::color::{
    lab_to_xyz, mat_inv, mat_mul, mat_vec, xyz_to_lab, xyz_to_lab_in_line, Lab, Mat3, Xyz, D50,
    IDENTITY,
};
use crate::curve::{clamp01, Curve, FwdSeg, InvCurve, InvSeg, Sampled};
use crate::error::{Error, Result};
use crate::lut::{make_trilinear, Clut3Ref, ClutEval, FixedClut, Lut, Stage};
use crate::profile::{ColorSpace, Profile, ProfileClass};
use crate::transform::Intent;

/// `LittleCMS`'s fixed v4 perceptual black (`cmsPERCEPTUAL_BLACK_*`).
const PERCEPTUAL_BLACK: Xyz = Xyz::new(0.00336, 0.003_473_1, 0.00287);

/// 16-bit legacy (v2) Lab encoding to v4: `65535 / 65280`.
const LAB_V2_TO_V4: f64 = 65535.0 / 65280.0;

/// Which PCS a value is in. PCS values are carried in real units: XYZ with
/// `Y = 1` at the D50 white, L*a*b* with `L` in `0..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pcs {
    Xyz,
    Lab,
}

/// The `f32` value of every 8-bit sample, `f32::from(k) / 255.0` (how float
/// pixels decoded from 8-bit data arrive).
pub(crate) static U8_POINTS: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut k = 0;
    while k < 256 {
        t[k] = k as f32 / 255.0;
        k += 1;
    }
    t
};

/// The 8-bit sample whose [`U8_POINTS`] value `x` is, if it is one.
#[inline(always)]
pub(crate) fn u8_point(x: f32) -> Option<usize> {
    // `x * 255` lands within an ulp of `k` for the point of `k`, so the
    // rounded value is the only candidate; the bit comparison settles it
    // (and rejects `-0.0`, NaN and everything between the points).
    let k = (x * 255.0 + 0.5) as i32 as usize;
    match U8_POINTS.get(k) {
        Some(p) if p.to_bits() == x.to_bits() => Some(k),
        _ => None,
    }
}

/// One pipeline operation over up to 16 channels.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    /// A LUT stage over normalised values.
    Lut(Stage),
    /// A LUT stage of per-channel curves that are all identities written as
    /// a power of 1 (how v4 profiles spell "no curve"), over this many
    /// channels: the clamp those curves amount to, without a stage call and
    /// a curve evaluation per channel.
    UnitCurves(usize),
    /// A LUT stage of per-channel curves that are all sampled tables (LUT
    /// input and output tables), evaluated in line: the values the stage
    /// gives, without a stage call and a curve call per channel.
    Sampled(Vec<Sampled>),
    /// A stage of per-channel curves that are all regular two-segment
    /// parametric ones (the sRGB curves at the device end of a chain through
    /// a LUT), evaluated in line: the values the stage gives, without a stage
    /// call, a curve call per channel and the tests on the curve kind.
    FwdSegs(Vec<FwdSeg>),
    /// Multiply every channel.
    Scale(f64),
    /// v4-normalised Lab to real Lab.
    DecodeLab,
    /// Real Lab to v4-normalised, clamped to `0..=1`.
    EncodeLab,
    /// `u1Fixed15`-normalised XYZ to real XYZ.
    DecodeXyz,
    /// Real XYZ to normalised, clamped.
    EncodeXyz,
    LabToXyz,
    XyzToLab,
    /// [`Op::XyzToLab`] then [`Op::EncodeLab`], then the [`Op::Scale`] that
    /// may follow (legacy 16-bit Lab): one op for the three.
    XyzToEncodedLab(Option<f64>),
    /// The [`Op::Scale`] that may come first, then [`Op::DecodeLab`] and
    /// [`Op::LabToXyz`]: one op for the three.
    EncodedLabToXyz(Option<f64>),
    /// XYZ affine map.
    Affine(Mat3, [f64; 3]),
    Clamp01,
    InvCurves(Vec<Curve>),
    /// [`Op::InvCurves`] when every curve is a two-segment parametric one
    /// (sRGB-shaped): the same values, without a `pow` per value for the
    /// curve's break point.
    InvBreakCurves(Vec<InvCurve>),
    /// [`Op::InvBreakCurves`] when every curve is a regular function-3 one
    /// (the sRGB curves at the device end of a chain through a LUT),
    /// evaluated in line: the same values, without a call and the tests on
    /// the curve kind per value, and with the reciprocal exponent worked out
    /// once.
    InvSegs(Vec<InvSeg>),
    /// Grey `Y` to D50 XYZ (1 to 3 channels).
    GrayToXyz,
    /// Grey value as `L*/100` to Lab (1 to 3 channels).
    GrayToLab,
    /// XYZ to `Y` (3 to 1 channels).
    XyzToGray,
    /// Lab to `L*/100` (3 to 1 channels).
    LabToGray,
}

/// The inverse-curves op for `curves`, prepared when every curve allows it.
fn inv_curves(curves: Vec<Curve>) -> Op {
    match curves.iter().map(InvCurve::new).collect::<Option<Vec<_>>>() {
        Some(prepared) => Op::InvBreakCurves(prepared),
        None => Op::InvCurves(curves),
    }
}

/// The op for one LUT stage.
fn lut_op(stage: Stage) -> Op {
    match stage {
        Stage::Curves(cs) if !cs.is_empty() && cs.iter().all(Curve::is_unit_gamma) => {
            Op::UnitCurves(cs.len())
        }
        Stage::Curves(cs) => match cs.iter().map(Sampled::new).collect::<Option<Vec<_>>>() {
            Some(prepared) if !prepared.is_empty() => Op::Sampled(prepared),
            _ => Op::Lut(Stage::Curves(cs)),
        },
        stage => Op::Lut(stage),
    }
}

fn run(ops: &[Op], v: &mut [f64; 16], mut n: usize) -> usize {
    for op in ops {
        match op {
            Op::Lut(s) => n = s.apply(v, n),
            Op::UnitCurves(k) => {
                for x in v.iter_mut().take(*k) {
                    *x = if *x > 0.0 { x.min(1.0) } else { 0.0 };
                }
            }
            Op::Sampled(cs) => run_sampled(cs, v),
            Op::FwdSegs(cs) => {
                for (x, c) in v.iter_mut().zip(cs) {
                    *x = c.eval(*x);
                }
            }
            Op::Scale(k) => {
                for x in v.iter_mut().take(n) {
                    *x *= k;
                }
            }
            Op::DecodeLab => {
                v[0] *= 100.0;
                v[1] = v[1] * 255.0 - 128.0;
                v[2] = v[2] * 255.0 - 128.0;
            }
            Op::EncodeLab => {
                v[0] = clamp01(v[0] / 100.0);
                v[1] = clamp01((v[1] + 128.0) / 255.0);
                v[2] = clamp01((v[2] + 128.0) / 255.0);
            }
            Op::DecodeXyz => {
                for x in v.iter_mut().take(3) {
                    *x *= 65535.0 / 32768.0;
                }
            }
            Op::EncodeXyz => {
                for x in v.iter_mut().take(3) {
                    *x = clamp01(*x * 32768.0 / 65535.0);
                }
            }
            Op::LabToXyz => {
                let x = lab_to_xyz(Lab::new(v[0], v[1], v[2]), D50);
                v[..3].copy_from_slice(&x.to_array());
            }
            Op::XyzToLab => {
                let l = xyz_to_lab(Xyz::new(v[0], v[1], v[2]), D50);
                v[..3].copy_from_slice(&[l.l, l.a, l.b]);
            }
            Op::XyzToEncodedLab(k) => {
                let l = xyz_to_lab(Xyz::new(v[0], v[1], v[2]), D50);
                v[0] = clamp01(l.l / 100.0);
                v[1] = clamp01((l.a + 128.0) / 255.0);
                v[2] = clamp01((l.b + 128.0) / 255.0);
                if let Some(k) = k {
                    for x in v.iter_mut().take(n) {
                        *x *= k;
                    }
                }
            }
            Op::EncodedLabToXyz(k) => {
                if let Some(k) = k {
                    for x in v.iter_mut().take(n) {
                        *x *= k;
                    }
                }
                let lab = Lab::new(v[0] * 100.0, v[1] * 255.0 - 128.0, v[2] * 255.0 - 128.0);
                let x = lab_to_xyz(lab, D50);
                v[..3].copy_from_slice(&x.to_array());
            }
            Op::Affine(m, off) => {
                let r = mat_vec(m, [v[0], v[1], v[2]]);
                for i in 0..3 {
                    v[i] = r[i] + off[i];
                }
            }
            Op::Clamp01 => {
                for x in v.iter_mut().take(n) {
                    *x = clamp01(*x);
                }
            }
            Op::InvCurves(cs) => {
                for (x, c) in v.iter_mut().zip(cs) {
                    *x = c.eval_inverse(*x);
                }
            }
            Op::InvBreakCurves(cs) => {
                for (x, c) in v.iter_mut().zip(cs) {
                    *x = c.eval(*x);
                }
            }
            Op::InvSegs(cs) => {
                for (x, c) in v.iter_mut().zip(cs) {
                    *x = c.eval(*x);
                }
            }
            Op::GrayToXyz => {
                let y = v[0];
                v[..3].copy_from_slice(&[D50.x * y, y, D50.z * y]);
                n = 3;
            }
            Op::GrayToLab => {
                let l = v[0] * 100.0;
                v[..3].copy_from_slice(&[l, 0.0, 0.0]);
                n = 3;
            }
            Op::XyzToGray => {
                v[0] = v[1];
                n = 1;
            }
            Op::LabToGray => {
                v[0] /= 100.0;
                n = 1;
            }
        }
    }
    n
}

/// [`Op::Sampled`]: the tables in line, in a frame of their own. In line in
/// [`run`], the loop kept its constants in registers across the whole op
/// loop, and every other op's calls (`cbrt`, `pow`, the stage calls) then
/// reloaded them: +16 instructions per colour on chains without a table
/// stage (labf32 +1.19%, v4f32 +1.14%).
#[inline(never)]
fn run_sampled(cs: &[Sampled], v: &mut [f64; 16]) {
    for (x, c) in v.iter_mut().zip(cs) {
        *x = c.eval(*x);
    }
}

/// A matrix-shaper side: device-linear values relate to PCS XYZ by
/// `matrix` (input: `XYZ = matrix * linear`; output: `linear = matrix * XYZ`).
#[derive(Debug, Clone)]
pub(crate) struct Shaper {
    pub(crate) curves: Vec<Curve>,
    pub(crate) matrix: Mat3,
}

/// One side of a profile: device (normalised) to PCS (real units) for an
/// input side, PCS to device for an output side.
pub(crate) struct Side {
    ops: Vec<Op>,
    pcs: Pcs,
    shaper: Option<Shaper>,
}

fn pcs_of(p: &Profile) -> Result<Pcs> {
    match p.header.pcs {
        ColorSpace::Xyz => Ok(Pcs::Xyz),
        ColorSpace::Lab => Ok(Pcs::Lab),
        _ => Err(Error::Unsupported("PCS other than XYZ or Lab")),
    }
}

fn data_pcs(cs: ColorSpace) -> Result<Pcs> {
    match cs {
        ColorSpace::Xyz => Ok(Pcs::Xyz),
        ColorSpace::Lab => Ok(Pcs::Lab),
        _ => Err(Error::Unsupported(
            "abstract profile data space must be XYZ or Lab",
        )),
    }
}

pub(crate) fn channels(cs: ColorSpace) -> Result<usize> {
    cs.channels()
        .ok_or(Error::Unsupported("colour space not transformable"))
}

fn decode_ops(ops: &mut Vec<Op>, pcs: Pcs, legacy16: bool) {
    match pcs {
        Pcs::Lab => {
            if legacy16 {
                ops.push(Op::Scale(LAB_V2_TO_V4));
            }
            ops.push(Op::DecodeLab);
        }
        Pcs::Xyz => ops.push(Op::DecodeXyz),
    }
}

fn encode_ops(ops: &mut Vec<Op>, pcs: Pcs, legacy16: bool) {
    match pcs {
        Pcs::Lab => {
            ops.push(Op::EncodeLab);
            if legacy16 {
                ops.push(Op::Scale(1.0 / LAB_V2_TO_V4));
            }
        }
        Pcs::Xyz => ops.push(Op::EncodeXyz),
    }
}

/// Data-normalised input through an abstract profile's `A2B0` to real PCS.
fn abstract_side(p: &Profile) -> Result<Side> {
    let lut = p
        .lut_tag(*b"A2B0")?
        .ok_or(Error::Unsupported("abstract profile without A2B0"))?;
    let data = data_pcs(p.header.color_space)?;
    let pcs = pcs_of(p)?;
    if lut.inputs() != 3 || lut.outputs() != 3 {
        return Err(Error::Malformed("abstract LUT channel counts"));
    }
    let legacy16 = matches!(lut, Lut::Lut16(_));
    let mut ops = Vec::new();
    if legacy16 && data == Pcs::Lab {
        ops.push(Op::Scale(1.0 / LAB_V2_TO_V4));
    }
    let mut stages = lut.stages();
    if pcs == Pcs::Lab {
        // `_cmsReadDevicelinkLUT` also interpolates Lab-PCS CLUTs trilinearly.
        make_trilinear(&mut stages);
    }
    ops.extend(stages.into_iter().map(lut_op));
    decode_ops(&mut ops, pcs, legacy16);
    Ok(Side {
        ops,
        pcs,
        shaper: None,
    })
}

/// Device to PCS for `intent` (`LittleCMS` `_cmsReadInputLUT`).
pub(crate) fn input_side(p: &Profile, intent: Intent) -> Result<Side> {
    match p.header.class {
        ProfileClass::Abstract => return abstract_side(p),
        ProfileClass::Link | ProfileClass::NamedColor => {
            return Err(Error::Unsupported("device-link and named-colour profiles"))
        }
        _ => {}
    }
    let pcs = pcs_of(p)?;
    let cs = p.header.color_space;
    let n = channels(cs)?;
    if let Some(lut) = p.a2b(intent)? {
        if lut.inputs() != n || lut.outputs() != 3 {
            return Err(Error::Malformed(
                "A2B channel counts do not match the header",
            ));
        }
        let legacy16 = matches!(lut, Lut::Lut16(_));
        let mut ops = Vec::new();
        if legacy16 && cs == ColorSpace::Lab {
            ops.push(Op::Scale(1.0 / LAB_V2_TO_V4));
        }
        ops.extend(lut.stages().into_iter().map(lut_op));
        decode_ops(&mut ops, pcs, legacy16);
        return Ok(Side {
            ops,
            pcs,
            shaper: None,
        });
    }
    match cs {
        ColorSpace::Gray => {
            let c = p
                .curve_tag(*b"kTRC")
                .ok_or(Error::Unsupported("grey profile without A2B or kTRC"))?
                .clone();
            let mut ops = vec![Op::Lut(Stage::Curves(vec![c.clone()]))];
            if pcs == Pcs::Xyz {
                ops.push(Op::GrayToXyz);
                Ok(Side {
                    ops,
                    pcs,
                    shaper: Some(Shaper {
                        curves: vec![c],
                        matrix: [[D50.x, 0.0, 0.0], [1.0, 0.0, 0.0], [D50.z, 0.0, 0.0]],
                    }),
                })
            } else {
                ops.push(Op::GrayToLab);
                Ok(Side {
                    ops,
                    pcs,
                    shaper: None,
                })
            }
        }
        ColorSpace::Rgb if p.is_matrix_shaper() => {
            let m = colorant_matrix(p)?;
            let curves: Vec<Curve> = [*b"rTRC", *b"gTRC", *b"bTRC"]
                .iter()
                .filter_map(|s| p.curve_tag(*s).cloned())
                .collect();
            let mut ops = vec![
                Op::Lut(Stage::Curves(curves.clone())),
                Op::Affine(m, [0.0; 3]),
            ];
            let shaper = if pcs == Pcs::Lab {
                ops.push(Op::XyzToLab);
                None
            } else {
                Some(Shaper { curves, matrix: m })
            };
            Ok(Side { ops, pcs, shaper })
        }
        _ => Err(Error::Unsupported("profile has no device-to-PCS transform")),
    }
}

fn colorant_matrix(p: &Profile) -> Result<Mat3> {
    let c = p
        .colorants()
        .ok_or(Error::Malformed("missing colorant tags"))?;
    Ok([
        [c[0].x, c[1].x, c[2].x],
        [c[0].y, c[1].y, c[2].y],
        [c[0].z, c[1].z, c[2].z],
    ])
}

/// PCS to device for `intent` (`LittleCMS` `_cmsReadOutputLUT`).
pub(crate) fn output_side(p: &Profile, intent: Intent) -> Result<Side> {
    match p.header.class {
        ProfileClass::Abstract | ProfileClass::Link | ProfileClass::NamedColor => {
            return Err(Error::Unsupported("profile class cannot be an output side"))
        }
        _ => {}
    }
    let pcs = pcs_of(p)?;
    let cs = p.header.color_space;
    let n = channels(cs)?;
    if let Some(lut) = p.b2a(intent)? {
        if lut.inputs() != 3 || lut.outputs() != n {
            return Err(Error::Malformed(
                "B2A channel counts do not match the header",
            ));
        }
        let legacy16 = matches!(lut, Lut::Lut16(_));
        let mut ops = Vec::new();
        encode_ops(&mut ops, pcs, legacy16);
        let mut stages = lut.stages();
        if pcs == Pcs::Lab {
            // As `LittleCMS` `_cmsReadOutputLUT`: Lab-indexed output CLUTs
            // interpolate trilinearly.
            make_trilinear(&mut stages);
        }
        ops.extend(stages.into_iter().map(lut_op));
        if legacy16 && cs == ColorSpace::Lab {
            ops.push(Op::Scale(LAB_V2_TO_V4));
        }
        ops.push(Op::Clamp01);
        return Ok(Side {
            ops,
            pcs,
            shaper: None,
        });
    }
    match cs {
        ColorSpace::Gray => {
            let c = p
                .curve_tag(*b"kTRC")
                .ok_or(Error::Unsupported("grey profile without B2A or kTRC"))?
                .clone();
            let (first, shaper) = if pcs == Pcs::Xyz {
                (
                    Op::XyzToGray,
                    Some(Shaper {
                        curves: vec![c.clone()],
                        matrix: [[0.0, 1.0, 0.0], [0.0; 3], [0.0; 3]],
                    }),
                )
            } else {
                (Op::LabToGray, None)
            };
            Ok(Side {
                ops: vec![first, Op::Clamp01, inv_curves(vec![c])],
                pcs,
                shaper,
            })
        }
        ColorSpace::Rgb if p.is_matrix_shaper() => {
            let inv = mat_inv(&colorant_matrix(p)?)
                .ok_or(Error::Malformed("singular colorant matrix"))?;
            let curves: Vec<Curve> = [*b"rTRC", *b"gTRC", *b"bTRC"]
                .iter()
                .filter_map(|s| p.curve_tag(*s).cloned())
                .collect();
            let mut ops = Vec::new();
            if pcs == Pcs::Lab {
                ops.push(Op::LabToXyz);
            }
            ops.push(Op::Affine(inv, [0.0; 3]));
            ops.push(Op::Clamp01);
            ops.push(inv_curves(curves.clone()));
            let shaper = (pcs == Pcs::Xyz).then_some(Shaper {
                curves,
                matrix: inv,
            });
            Ok(Side { ops, pcs, shaper })
        }
        _ => Err(Error::Unsupported("profile has no PCS-to-device transform")),
    }
}

/// Appends the conversion from PCS `from` to PCS `to` through an optional
/// XYZ affine map.
fn connect(ops: &mut Vec<Op>, from: Pcs, to: Pcs, conv: Option<(Mat3, [f64; 3])>) {
    if conv.is_none() && from == to {
        return;
    }
    if from == Pcs::Lab {
        ops.push(Op::LabToXyz);
    }
    if let Some((m, off)) = conv {
        ops.push(Op::Affine(m, off));
    }
    if to == Pcs::Lab {
        ops.push(Op::XyzToLab);
    }
}

fn to_lab(v: &[f64; 16], pcs: Pcs) -> Lab {
    match pcs {
        Pcs::Lab => Lab::new(v[0], v[1], v[2]),
        Pcs::Xyz => xyz_to_lab(Xyz::new(v[0], v[1], v[2]), D50),
    }
}

fn clip_black(lab: Lab) -> Xyz {
    let l = if lab.l > 50.0 { 50.0 } else { lab.l };
    lab_to_xyz(Lab::new(l, 0.0, 0.0), D50)
}

/// `BlackPointAsDarkerColorant`.
fn black_as_darker_colorant(p: &Profile, intent: Intent) -> Xyz {
    if !p.intent_supported(intent, true) {
        return Xyz::default();
    }
    let black: &[f64] = match p.header.color_space {
        ColorSpace::Gray => &[0.0],
        ColorSpace::Rgb => &[0.0, 0.0, 0.0],
        ColorSpace::Cmyk => &[1.0, 1.0, 1.0, 1.0],
        _ => return Xyz::default(),
    };
    let Ok(side) = input_side(p, intent) else {
        return Xyz::default();
    };
    let mut v = [0.0; 16];
    v[..black.len()].copy_from_slice(black);
    run(&side.ops, &mut v, black.len());
    clip_black(to_lab(&v, side.pcs))
}

/// Lab through a profile's output side for an intent, then back through
/// its relative-colorimetric input side (`CreateRoundtripXForm`). Built
/// once and evaluated per colour: building a side converts the profile's
/// whole CLUT, and the destination black-point search evaluates 256
/// colours, so building it per colour was most of a `Transform::new` with
/// black-point compensation.
struct Roundtrip {
    out: Side,
    inn: Side,
}

impl Roundtrip {
    fn new(p: &Profile, intent: Intent) -> Result<Self> {
        Ok(Self {
            out: output_side(p, intent)?,
            inn: input_side(p, Intent::RelativeColorimetric)?,
        })
    }

    fn eval(&self, lab: Lab) -> Lab {
        let mut v = [0.0; 16];
        match self.out.pcs {
            Pcs::Lab => v[..3].copy_from_slice(&[lab.l, lab.a, lab.b]),
            Pcs::Xyz => v[..3].copy_from_slice(&lab_to_xyz(lab, D50).to_array()),
        }
        let n = run(&self.out.ops, &mut v, 3);
        run(&self.inn.ops, &mut v, n);
        to_lab(&v, self.inn.pcs)
    }
}

/// `BlackPointUsingPerceptualBlack`.
fn black_using_perceptual(p: &Profile) -> Xyz {
    if !p.intent_supported(Intent::Perceptual, true) {
        return Xyz::default();
    }
    Roundtrip::new(p, Intent::Perceptual)
        .map_or(Xyz::default(), |r| clip_black(r.eval(Lab::default())))
}

/// `cmsDetectBlackPoint`.
pub(crate) fn detect_black_point(p: &Profile, intent: Intent) -> Xyz {
    if matches!(
        p.header.class,
        ProfileClass::Link | ProfileClass::Abstract | ProfileClass::NamedColor
    ) || intent == Intent::AbsoluteColorimetric
    {
        return Xyz::default();
    }
    if p.header.version >= 0x0400_0000 && matches!(intent, Intent::Perceptual | Intent::Saturation)
    {
        if p.is_matrix_shaper() {
            return black_as_darker_colorant(p, Intent::RelativeColorimetric);
        }
        return PERCEPTUAL_BLACK;
    }
    if intent == Intent::RelativeColorimetric
        && p.header.class == ProfileClass::Output
        && p.header.color_space == ColorSpace::Cmyk
    {
        return black_using_perceptual(p);
    }
    black_as_darker_colorant(p, intent)
}

/// Vertex of the least-squares quadratic through `(x, y)`
/// (`RootOfLeastSquaresFitQuadraticCurve`).
fn quadratic_root(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 4 {
        return 0.0;
    }
    let (mut sx, mut sx2, mut sx3, mut sx4, mut sy, mut syx, mut syx2) =
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for (&xn, &yn) in x.iter().zip(y) {
        sx += xn;
        sx2 += xn * xn;
        sx3 += xn * xn * xn;
        sx4 += xn * xn * xn * xn;
        sy += yn;
        syx += yn * xn;
        syx2 += yn * xn * xn;
    }
    let m = [[n as f64, sx, sx2], [sx, sx2, sx3], [sx2, sx3, sx4]];
    let Some(inv) = mat_inv(&m) else {
        return 0.0;
    };
    let res = mat_vec(&inv, [sy, syx, syx2]);
    let (a, b, c) = (res[2], res[1], res[0]);
    if a.abs() < 1.0e-10 {
        if b.abs() < 1.0e-10 {
            return 0.0;
        }
        // LittleCMS writes cmsmin(0, cmsmax(50, -c/b)), which is always 0.
        return 0.0;
    }
    let d = b * b - 4.0 * a * c;
    if d <= 0.0 {
        return 0.0;
    }
    let rt = (-b + d.sqrt()) / (2.0 * a);
    rt.clamp(0.0, 50.0)
}

/// `cmsDetectDestinationBlackPoint` (the Adobe algorithm for LUT outputs).
pub(crate) fn detect_destination_black_point(p: &Profile, intent: Intent) -> Xyz {
    if p.header.version >= 0x0400_0000 && matches!(intent, Intent::Perceptual | Intent::Saturation)
    {
        if p.is_matrix_shaper() {
            return black_as_darker_colorant(p, Intent::RelativeColorimetric);
        }
        return PERCEPTUAL_BLACK;
    }
    let cs = p.header.color_space;
    if !p.has_exact_lut(intent, false)
        || !matches!(cs, ColorSpace::Gray | ColorSpace::Rgb | ColorSpace::Cmyk)
    {
        return detect_black_point(p, intent);
    }
    let initial = if intent == Intent::RelativeColorimetric {
        xyz_to_lab(detect_black_point(p, intent), D50)
    } else {
        Lab::default()
    };
    let Ok(roundtrip) = Roundtrip::new(p, intent) else {
        return Xyz::default();
    };
    let mut in_ramp = [0.0; 256];
    let mut out_ramp = [0.0; 256];
    for l in 0..256 {
        let lab = Lab::new(
            l as f64 * 100.0 / 255.0,
            initial.a.clamp(-50.0, 50.0),
            initial.b.clamp(-50.0, 50.0),
        );
        let d = roundtrip.eval(lab);
        in_ramp[l] = lab.l;
        out_ramp[l] = d.l;
    }
    for l in (1..255).rev() {
        out_ramp[l] = out_ramp[l].min(out_ramp[l + 1]);
    }
    if out_ramp[0].partial_cmp(&out_ramp[255]) != Some(core::cmp::Ordering::Less) {
        return Xyz::default();
    }
    let (min_l, max_l) = (out_ramp[0], out_ramp[255]);
    if intent == Intent::RelativeColorimetric {
        let straight = (0..256).all(|l| {
            in_ramp[l] <= min_l + 0.2 * (max_l - min_l) || (in_ramp[l] - out_ramp[l]).abs() < 4.0
        });
        if straight {
            return lab_to_xyz(initial, D50);
        }
    }
    let (lo, hi) = if intent == Intent::RelativeColorimetric {
        (0.1, 0.5)
    } else {
        (0.03, 0.25)
    };
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for l in 0..256 {
        let ff = (out_ramp[l] - min_l) / (max_l - min_l);
        if ff >= lo && ff < hi {
            xs.push(in_ramp[l]);
            ys.push(ff);
        }
    }
    if xs.len() < 3 {
        return Xyz::default();
    }
    let l = quadratic_root(&xs, &ys).max(0.0);
    lab_to_xyz(Lab::new(l, initial.a, initial.b), D50)
}

/// The PCS-level adjustment between `prev` (as input) and `cur` (as
/// output) (`ComputeConversion`): absolute-colorimetric white scaling, or
/// black-point compensation.
fn conversion(
    prev: &Profile,
    cur: &Profile,
    intent: Intent,
    bpc: bool,
) -> Option<(Mat3, [f64; 3])> {
    if intent == Intent::AbsoluteColorimetric {
        let wi = prev.absolute_white_point();
        let wo = cur.absolute_white_point();
        let m = [
            [wi.x / wo.x, 0.0, 0.0],
            [0.0, wi.y / wo.y, 0.0],
            [0.0, 0.0, wi.z / wo.z],
        ];
        return (m != IDENTITY).then_some((m, [0.0; 3]));
    }
    if !bpc {
        return None;
    }
    let bi = detect_black_point(prev, intent);
    let bo = detect_destination_black_point(cur, intent);
    if bi == bo {
        return None;
    }
    let (ti, to) = (bi.to_array(), bo.to_array());
    let d = D50.to_array();
    let mut m = IDENTITY;
    let mut off = [0.0; 3];
    for k in 0..3 {
        let t = ti[k] - d[k];
        if t.abs() < 1e-12 {
            return None;
        }
        m[k][k] = (to[k] - d[k]) / t;
        off[k] = -d[k] * (to[k] - ti[k]) / t;
    }
    Some((m, off))
}

/// A matrix-shaper-to-matrix-shaper link: `out_linear = m * in_linear + off`.
#[derive(Debug, Clone)]
pub(crate) struct ShaperLink {
    pub(crate) in_curves: Vec<Curve>,
    pub(crate) out_curves: Vec<Curve>,
    pub(crate) m: Mat3,
    pub(crate) off: [f64; 3],
}

/// A complete device-to-device chain over normalised values.
#[derive(Debug, Clone)]
pub(crate) struct Chain {
    ops: Vec<Op>,
    pub(crate) in_space: ColorSpace,
    pub(crate) out_space: ColorSpace,
    pub(crate) in_ch: usize,
    pub(crate) out_ch: usize,
    pub(crate) shaper: Option<ShaperLink>,
}

/// A chain that is one RGB matrix/TRC profile into another, viewed for a
/// straight-line evaluation per colour: the operations [`run`] performs for
/// the ops it borrows, in the same order (so every result is the same),
/// without the per-op dispatch, the calls and the 16-channel scratch arrays.
pub(crate) struct ShaperChain<'a> {
    curves: &'a [Curve; 3],
    /// The XYZ affine maps between the curves, the first `maps` of them.
    affines: [(Mat3, [f64; 3]); 3],
    maps: usize,
    inverse: Inverse<'a>,
}

enum Inverse<'a> {
    Break(&'a [InvCurve; 3]),
    Plain(&'a [Curve; 3]),
}

/// A [`ShaperChain`] whose six curves are all regular two-segment ones (an
/// sRGB-shaped profile into another, the common display case), each as
/// constants for evaluation in line: no call and no per-curve test per
/// value.
pub(crate) struct SegChain {
    curves: [FwdSeg; 3],
    affines: [(Mat3, [f64; 3]); 3],
    maps: usize,
    inverse: [InvSeg; 3],
}

impl SegChain {
    /// One colour: exactly what [`Chain::eval`] gives for it.
    #[inline]
    pub(crate) fn eval(&self, x: [f64; 3]) -> [f64; 3] {
        let [c0, c1, c2] = &self.curves;
        let mut v = [c0.eval(x[0]), c1.eval(x[1]), c2.eval(x[2])];
        for (m, off) in &self.affines[..self.maps] {
            let r = mat_vec(m, v);
            v = [r[0] + off[0], r[1] + off[1], r[2] + off[2]];
        }
        // No clamp of `v` here, though the chain clamps before its inverse
        // curves: every inverse evaluation clamps its input first, and the
        // clamp is idempotent (NaN to 0, -0.0 kept), so a second one is the
        // first's value.
        let [i0, i1, i2] = &self.inverse;
        [i0.eval(v[0]), i1.eval(v[1]), i2.eval(v[2])]
    }

    /// The device curves of one colour: the values [`SegChain::eval`]
    /// starts from.
    #[inline(always)]
    pub(crate) fn curves(&self, x: [f64; 3]) -> [f64; 3] {
        let [c0, c1, c2] = &self.curves;
        [c0.eval(x[0]), c1.eval(x[1]), c2.eval(x[2])]
    }

    /// [`SegChain::eval`] after the device curves: `v` holds the values
    /// they give for the colour.
    #[inline(always)]
    pub(crate) fn rest(&self, mut v: [f64; 3]) -> [f64; 3] {
        for (m, off) in &self.affines[..self.maps] {
            let r = mat_vec(m, v);
            v = [r[0] + off[0], r[1] + off[1], r[2] + off[2]];
        }
        // No clamp of `v` here, though the chain clamps before its inverse
        // curves: every inverse evaluation clamps its input first, and the
        // clamp is idempotent (NaN to 0, -0.0 kept), so a second one is the
        // first's value.
        let [i0, i1, i2] = &self.inverse;
        [i0.eval(v[0]), i1.eval(v[1]), i2.eval(v[2])]
    }
}

/// A chain from an RGB matrix/TRC profile into a Lab-PCS LUT output profile
/// (sRGB to a CMYK printer: the common float CMYK row), viewed for a
/// straight-line evaluation per colour: the operations [`run`] performs for
/// the ops it borrows, in the same order (so every result is the same),
/// without the per-op dispatch, the stage calls, the 128-byte copy of the
/// colour array per CLUT lookup and the tests on the CLUT's shape. The ops
/// are: three function-3 curves, one or two XYZ affine maps, the Lab
/// encoding (with the legacy 16-bit scale, if any), the LUT's input tables,
/// its CLUT (three inputs, `N` outputs, trilinear as every Lab-indexed
/// output CLUT is), its output tables and the final clamp.
pub(crate) struct LutOutChain<'a, const N: usize> {
    curves: &'a [FwdSeg; 3],
    affines: [(Mat3, [f64; 3]); 2],
    maps: usize,
    /// Per map: `true` when it is a diagonal matrix with non-zero offsets
    /// (black-point compensation), applied as three products and sums.
    diagonal: [bool; 2],
    /// The legacy 16-bit Lab scale, or 1 without one (both in `0..=1`).
    lab_scale: Option<f64>,
    pre: &'a [Sampled; 3],
    clut: Clut3Ref<'a, N>,
    post: &'a [Sampled; N],
}

impl<const N: usize> LutOutChain<'_, N> {
    /// `true` when one of the maps runs as a diagonal (for the tests).
    #[cfg(test)]
    pub(crate) fn has_diagonal_map(&self) -> bool {
        self.diagonal[..self.maps].contains(&true)
    }

    /// The device curves of one colour (`x` normalised, as the chain takes
    /// it): the values the chain's first op gives.
    #[inline(always)]
    pub(crate) fn curves(&self, x: [f64; 3]) -> [f64; 3] {
        let [c0, c1, c2] = self.curves;
        [c0.eval(x[0]), c1.eval(x[1]), c2.eval(x[2])]
    }

    /// The rest of the chain on the values [`LutOutChain::curves`] gives,
    /// into `out[..N]`: exactly what [`Chain::eval`] gives for the colour.
    #[inline(always)]
    pub(crate) fn rest(&self, mut v: [f64; 3], out: &mut [f64; 16]) {
        for ((m, off), &diagonal) in self.affines[..self.maps].iter().zip(&self.diagonal) {
            if diagonal {
                // The full product's off-diagonal terms are `0 * v` = a zero
                // of either sign for the finite values met here (curve
                // outputs and their affine images), which leaves a non-zero
                // sum unchanged; a zero sum, of either sign, then takes the
                // non-zero offset as the result in both forms.
                v = [
                    m[0][0] * v[0] + off[0],
                    m[1][1] * v[1] + off[1],
                    m[2][2] * v[2] + off[2],
                ];
            } else {
                let r = mat_vec(m, v);
                v = [r[0] + off[0], r[1] + off[1], r[2] + off[2]];
            }
        }
        let l = xyz_to_lab_in_line(Xyz::new(v[0], v[1], v[2]), D50);
        let mut e = [
            clamp01(l.l / 100.0),
            clamp01((l.a + 128.0) / 255.0),
            clamp01((l.b + 128.0) / 255.0),
        ];
        if let Some(k) = self.lab_scale {
            for x in &mut e {
                *x *= k;
            }
        }
        // The tables' own clamp is left out: the clamped encoding times a
        // scale in `0..=1` is already in `0..=1` (or -0.0), where it returns
        // the value itself.
        let [p0, p1, p2] = self.pre;
        let e = [p0.eval_unit(e[0]), p1.eval_unit(e[1]), p2.eval_unit(e[2])];
        self.clut.lookup::<true>(e, out);
        // The chain's final clamp is left out: it never acts on what a
        // sampled table gives (between two of its samples over 65535, so in
        // `0..=1`, and never -0.0 since the samples are not), and the `N`
        // tables cover every channel it clamps.
        for (x, c) in out.iter_mut().zip(self.post) {
            *x = c.eval(*x);
        }
    }
}

impl ShaperChain<'_> {
    /// The chain as a [`SegChain`], when every curve allows it.
    pub(crate) fn segments(&self) -> Option<SegChain> {
        let Inverse::Break([i0, i1, i2]) = self.inverse else {
            return None;
        };
        let [c0, c1, c2] = self.curves;
        Some(SegChain {
            curves: [FwdSeg::new(c0)?, FwdSeg::new(c1)?, FwdSeg::new(c2)?],
            affines: self.affines,
            maps: self.maps,
            inverse: [InvSeg::new(i0)?, InvSeg::new(i1)?, InvSeg::new(i2)?],
        })
    }

    /// One colour: exactly what [`Chain::eval`] gives for it.
    #[inline]
    pub(crate) fn eval(&self, x: [f64; 3]) -> [f64; 3] {
        let [c0, c1, c2] = self.curves;
        let mut v = [c0.eval(x[0]), c1.eval(x[1]), c2.eval(x[2])];
        for (m, off) in &self.affines[..self.maps] {
            let r = mat_vec(m, v);
            v = [r[0] + off[0], r[1] + off[1], r[2] + off[2]];
        }
        // No clamp of `v` here, though the chain clamps before its inverse
        // curves: every inverse evaluation clamps its input first, and the
        // clamp is idempotent (NaN to 0, -0.0 kept), so a second one is the
        // first's value.
        match self.inverse {
            Inverse::Break([i0, i1, i2]) => [i0.eval(v[0]), i1.eval(v[1]), i2.eval(v[2])],
            Inverse::Plain([i0, i1, i2]) => [
                i0.eval_inverse(v[0]),
                i1.eval_inverse(v[1]),
                i2.eval_inverse(v[2]),
            ],
        }
    }

    /// The device curves of one colour: the values [`ShaperChain::eval`]
    /// starts from.
    #[inline(always)]
    pub(crate) fn curves(&self, x: [f64; 3]) -> [f64; 3] {
        let [c0, c1, c2] = self.curves;
        [c0.eval(x[0]), c1.eval(x[1]), c2.eval(x[2])]
    }

    /// [`ShaperChain::eval`] after the device curves: `v` holds the values
    /// they give for the colour.
    #[inline(always)]
    pub(crate) fn rest(&self, mut v: [f64; 3]) -> [f64; 3] {
        for (m, off) in &self.affines[..self.maps] {
            let r = mat_vec(m, v);
            v = [r[0] + off[0], r[1] + off[1], r[2] + off[2]];
        }
        // No clamp of `v` here, though the chain clamps before its inverse
        // curves: every inverse evaluation clamps its input first, and the
        // clamp is idempotent (NaN to 0, -0.0 kept), so a second one is the
        // first's value.
        match self.inverse {
            Inverse::Break([i0, i1, i2]) => [i0.eval(v[0]), i1.eval(v[1]), i2.eval(v[2])],
            Inverse::Plain([i0, i1, i2]) => [
                i0.eval_inverse(v[0]),
                i1.eval_inverse(v[1]),
                i2.eval_inverse(v[2]),
            ],
        }
    }
}

impl Chain {
    /// The chain as a [`ShaperChain`], when its ops are exactly: three
    /// device curves, XYZ affine maps (in, an optional PCS adjustment,
    /// out), the clamp, three inverse device curves. Worked out per call
    /// from the ops themselves (a few dozen instructions, nothing stored),
    /// so building a transform costs nothing more.
    pub(crate) fn as_shaper(&self) -> Option<ShaperChain<'_>> {
        if self.in_ch != 3 || self.out_ch != 3 {
            return None;
        }
        let [Op::Lut(Stage::Curves(curves)), mid @ .., Op::Clamp01, last] = self.ops.as_slice()
        else {
            return None;
        };
        let curves: &[Curve; 3] = curves.as_slice().try_into().ok()?;
        let mut affines = [(IDENTITY, [0.0; 3]); 3];
        if mid.len() > affines.len() {
            return None;
        }
        for (slot, op) in affines.iter_mut().zip(mid) {
            let Op::Affine(m, off) = op else {
                return None;
            };
            *slot = (*m, *off);
        }
        let inverse = match last {
            Op::InvBreakCurves(cs) => Inverse::Break(cs.as_slice().try_into().ok()?),
            Op::InvCurves(cs) => Inverse::Plain(cs.as_slice().try_into().ok()?),
            _ => return None,
        };
        Some(ShaperChain {
            curves,
            affines,
            maps: mid.len(),
            inverse,
        })
    }

    /// The chain as a [`LutOutChain`] of `N` outputs, when its ops have
    /// exactly that shape. Worked out per call from the ops themselves, as
    /// [`Chain::as_shaper`] is.
    pub(crate) fn as_lut_out<const N: usize>(&self) -> Option<LutOutChain<'_, N>> {
        if self.in_ch != 3 || self.out_ch != N {
            return None;
        }
        let [Op::FwdSegs(curves), rest @ ..] = self.ops.as_slice() else {
            return None;
        };
        let mut rest = rest;
        let mut affines = [(IDENTITY, [0.0; 3]); 2];
        let mut maps = 0;
        while let [Op::Affine(m, off), tail @ ..] = rest {
            *affines.get_mut(maps)? = (*m, *off);
            maps += 1;
            rest = tail;
        }
        let [Op::XyzToEncodedLab(lab_scale), Op::Sampled(pre), Op::Lut(Stage::Clut(clut)), Op::Sampled(post), Op::Clamp01] =
            rest
        else {
            return None;
        };
        let clut = Clut3Ref::new(clut)?;
        if maps == 0 || !clut.trilinear() || lab_scale.is_some_and(|k| !(0.0..=1.0).contains(&k)) {
            return None;
        }
        let diagonal = affines.map(|(m, off)| {
            let zero = |x: f64| x == 0.0;
            [m[0][1], m[0][2], m[1][0], m[1][2], m[2][0], m[2][1]]
                .into_iter()
                .all(zero)
                && off.iter().all(|&o| o != 0.0)
                && m.iter().flatten().all(|x| x.is_finite())
        });
        Some(LutOutChain {
            curves: curves.as_slice().try_into().ok()?,
            affines,
            maps,
            diagonal,
            lab_scale: *lab_scale,
            pre: pre.as_slice().try_into().ok()?,
            clut,
            post: post.as_slice().try_into().ok()?,
        })
    }

    /// Evaluates one colour (normalised in, normalised out). Values past
    /// `in_ch` in `input` must be zero (callers keep zero-tailed arrays);
    /// `out` past `out_ch` is unspecified. Whole-array copies have a
    /// constant length and inline, where the old runtime-length
    /// `copy_from_slice`s were two `memcpy` calls per colour.
    pub(crate) fn eval(&self, input: &[f64; 16], out: &mut [f64; 16]) {
        let mut v = *input;
        run(&self.ops, &mut v, self.in_ch);
        *out = v;
    }

    /// The chain sampled on a regular grid of `g` points per input (first
    /// input slowest, `out_ch` values per point): [`Chain::eval`] at every
    /// `(i0, i1, ..) / (g - 1)`.
    ///
    /// When the chain starts with per-channel curves (every matrix/TRC
    /// input and most LUT inputs), they are evaluated once per axis value
    /// instead of once per grid point. A grid point's coordinates are
    /// exactly those axis values, so every result is the same: three libm
    /// `pow` calls per point of a 33^3 grid become 99 calls in all.
    pub(crate) fn sample_grid(&self, g: usize) -> Vec<f32> {
        let total = (0..self.in_ch).fold(1usize, |a, _| a * g);
        let axis: Vec<f64> = (0..g).map(|i| i as f64 / (g - 1) as f64).collect();
        let (head, tail): (Vec<Vec<f64>>, &[Op]) = match self.ops.split_first() {
            Some((Op::Lut(Stage::Curves(cs)), tail)) if cs.len() == self.in_ch => (
                cs.iter()
                    .map(|c| axis.iter().map(|&x| c.eval(x)).collect())
                    .collect(),
                tail,
            ),
            Some((Op::FwdSegs(cs), tail)) if cs.len() == self.in_ch => (
                cs.iter()
                    .map(|c| axis.iter().map(|&x| c.eval(x)).collect())
                    .collect(),
                tail,
            ),
            Some((Op::Sampled(cs), tail)) if cs.len() == self.in_ch => (
                cs.iter()
                    .map(|c| axis.iter().map(|&x| c.eval(x)).collect())
                    .collect(),
                tail,
            ),
            _ => (vec![axis; self.in_ch], &self.ops),
        };
        // Written in place, point by point, the points visited in order by an
        // odometer over the axis indices (last input fastest) instead of a
        // division and a remainder per input per point.
        let mut data = vec![0.0f32; total * self.out_ch];
        // A tetrahedral CLUT right after the curves (CMYK and RGB LUT
        // inputs): its grid cell and fraction along an axis depend on that
        // axis value alone, so they are worked out once per axis value.
        if let Some((Op::Lut(Stage::Clut(c)), rest)) = tail.split_first() {
            if grid_through_clut(c, &head, rest, (self.in_ch, self.out_ch), &mut data) {
                return data;
            }
        }
        grid_ops(&head, tail, (self.in_ch, self.out_ch), &mut data);
        data
    }

    /// [`Chain::eval`] on one array: `v` holds the colour (zero past
    /// `in_ch`) and receives the result.
    pub(crate) fn eval_in_place(&self, v: &mut [f64; 16]) {
        run(&self.ops, v, self.in_ch);
    }

    /// When the chain starts with per-channel curves (the sRGB curves of a
    /// matrix/TRC source, a LUT's input tables): the values they give at
    /// the 256 points `f32::from(k) / 255.0`, one row per input channel, so
    /// that float pixels widened from 8-bit values (where every sample is
    /// such a point) look their first op up instead of evaluating it. Each
    /// entry is [`run`] of that op on the point, so a lookup gives exactly
    /// what the op gives. `None` for every other first op.
    pub(crate) fn head_on_u8_points(&self) -> Option<Box<[[f64; 256]]>> {
        let head = self.ops.first()?;
        let per_channel = match head {
            Op::FwdSegs(cs) => cs.len() == self.in_ch,
            Op::Sampled(cs) => cs.len() == self.in_ch,
            Op::Lut(Stage::Curves(cs)) => cs.len() == self.in_ch,
            _ => false,
        };
        if !per_channel {
            return None;
        }
        let mut rows = vec![[0.0f64; 256]; self.in_ch].into_boxed_slice();
        for (k, x) in U8_POINTS.iter().enumerate() {
            let mut v = [0.0f64; 16];
            v[..self.in_ch].fill(f64::from(*x));
            run(core::slice::from_ref(head), &mut v, self.in_ch);
            for (row, y) in rows.iter_mut().zip(v) {
                row[k] = y;
            }
        }
        Some(rows)
    }

    /// The chain after its first op: with the values that op gives in `v`,
    /// [`Chain::eval_in_place`] of the colour they came from.
    pub(crate) fn eval_tail_in_place(&self, v: &mut [f64; 16]) {
        run(self.ops.get(1..).unwrap_or(&[]), v, self.in_ch);
    }
}

/// The general loop of [`Chain::sample_grid`]: per point, the head values
/// and then the rest of the ops. In its own frame, so the CLUT path beside it
/// cannot reshape it.
#[inline(never)]
fn grid_ops(head: &[Vec<f64>], tail: &[Op], (in_ch, out_ch): (usize, usize), data: &mut [f32]) {
    let g = head[0].len();
    let mut at = [0usize; 16];
    for point in data.chunks_exact_mut(out_ch) {
        let mut v = [0.0f64; 16];
        for c in 0..in_ch {
            v[c] = head[c][at[c]];
        }
        run(tail, &mut v, in_ch);
        for (d, &x) in point.iter_mut().zip(&v) {
            *d = x as f32;
        }
        advance(&mut at[..in_ch], g);
    }
}

/// The grid by [`grid_clut`] when the CLUT `c` has a fixed-shape kernel
/// (`true`); `false`, with nothing done, otherwise. Out of line, so the
/// general loop of `sample_grid` keeps its code (with this dispatch in its
/// frame it cost +3 instructions per grid point: build3, buildbpc +0.19%).
#[inline(never)]
fn grid_through_clut(
    c: &ClutEval,
    h: &[Vec<f64>],
    rest: &[Op],
    (in_ch, out_ch): (usize, usize),
    data: &mut [f32],
) -> bool {
    let done = match (in_ch, c.outputs) {
        (3, 3) => c
            .fixed::<3, 3>()
            .map(|f| grid_clut(&f, h, rest, out_ch, data)),
        (3, 4) => c
            .fixed::<3, 4>()
            .map(|f| grid_clut(&f, h, rest, out_ch, data)),
        (4, 3) => c
            .fixed::<4, 3>()
            .map(|f| grid_clut(&f, h, rest, out_ch, data)),
        (4, 4) => c
            .fixed::<4, 4>()
            .map(|f| grid_clut(&f, h, rest, out_ch, data)),
        _ => None,
    };
    done.is_some()
}

/// [`Chain::sample_grid`] when the ops after the head curves start with a
/// tetrahedral CLUT of `IN` inputs and `OUT` outputs: per point, the head
/// values and their tabulated `(cell, fraction)` per axis, the lookup by
/// [`FixedClut::at`] (exactly what the stage's `ClutEval::eval` gives for
/// those values, written to the same slots) and then the rest of the ops.
/// Out of line: inlined into `sample_grid`, its four instances re-rolled the
/// general loop there (+12 instructions per grid point on every other grid
/// build: build3, buildlab, buildproof +431K each).
#[inline(never)]
fn grid_clut<const IN: usize, const OUT: usize>(
    f: &FixedClut<'_, IN, OUT>,
    head: &[Vec<f64>],
    rest: &[Op],
    out_ch: usize,
    data: &mut [f32],
) {
    let cells: [Vec<(usize, f64)>; IN] = core::array::from_fn(|c| {
        head[c]
            .iter()
            .map(|&x| ClutEval::axis(f.points(c), x))
            .collect()
    });
    let g = head[0].len();
    let mut at = [0usize; IN];
    for point in data.chunks_exact_mut(out_ch) {
        let mut v = [0.0f64; 16];
        let mut cell = [(0usize, 0.0f64); IN];
        for c in 0..IN {
            v[c] = head[c][at[c]];
            cell[c] = cells[c][at[c]];
        }
        f.at(&cell, &mut v);
        run(rest, &mut v, OUT);
        for (d, &x) in point.iter_mut().zip(&v) {
            *d = x as f32;
        }
        advance(&mut at, g);
    }
}

/// Steps the axis indices `at` (last fastest, each below `g`) to the next
/// grid point in row-major order.
#[inline(always)]
fn advance(at: &mut [usize], g: usize) {
    for i in at.iter_mut().rev() {
        *i += 1;
        if *i < g {
            return;
        }
        *i = 0;
    }
}

/// Effective BPC flags (`LittleCMS` `_cmsLinkProfiles`): never for absolute
/// colorimetric, always for perceptual and saturation into v4 profiles.
fn effective_bpc(p: &Profile, intent: Intent, bpc: bool) -> bool {
    match intent {
        Intent::AbsoluteColorimetric => false,
        Intent::Perceptual | Intent::Saturation if p.header.version >= 0x0400_0000 => true,
        _ => bpc,
    }
}

enum Cur {
    Device(ColorSpace),
    Pcs(Pcs),
}

/// Links `profiles` (with one intent and BPC flag per profile, as
/// `LittleCMS`'s `cmsCreateExtendedTransform`): a profile is used as input
/// while the current values are device values, as output while they are
/// PCS values.
pub(crate) fn build_chain(
    profiles: &[&Profile],
    intents: &[Intent],
    bpc: &[bool],
) -> Result<Chain> {
    let mut chain = link_ops(profiles, intents, bpc)?;
    // A matrix/TRC pair keeps its curve ops as they are: the fused `f32`
    // loop reads them through `Chain::as_shaper`.
    if chain.as_shaper().is_none() {
        prepare_curves(&mut chain.ops);
        chain.ops = fuse_lab(core::mem::take(&mut chain.ops));
    }
    Ok(chain)
}

/// [`build_chain`] before its curve ops are prepared.
fn link_ops(profiles: &[&Profile], intents: &[Intent], bpc: &[bool]) -> Result<Chain> {
    let first = profiles
        .first()
        .ok_or(Error::InvalidTransform("no profiles"))?;
    let in_space = first.header.color_space;
    let in_ch = channels(in_space)?;
    let mut ops = Vec::new();
    let mut cur = Cur::Device(in_space);
    let mut sides_in: Vec<Option<Shaper>> = Vec::new();
    let mut sides_out: Vec<Option<Shaper>> = Vec::new();
    let mut convs = Vec::new();
    let mut any_abstract = false;
    for (i, p) in profiles.iter().enumerate() {
        let intent = intents[i];
        let bpc_i = effective_bpc(p, intent, bpc[i]);
        let is_abstract = p.header.class == ProfileClass::Abstract;
        any_abstract |= is_abstract;
        match cur {
            Cur::Device(space) => {
                if space != p.header.color_space {
                    return Err(Error::InvalidTransform(
                        "profile colour space does not match the previous output",
                    ));
                }
                let side = input_side(p, intent)?;
                ops.extend(side.ops);
                sides_in.push(side.shaper);
                cur = Cur::Pcs(side.pcs);
            }
            Cur::Pcs(from) if is_abstract => {
                let conv = conversion(profiles[i - 1], p, intent, bpc_i);
                let data = data_pcs(p.header.color_space)?;
                connect(&mut ops, from, data, conv);
                let side = abstract_side(p)?;
                // `abstract_side` takes v4-normalised data and applies any
                // legacy-encoding scale itself.
                encode_ops(&mut ops, data, false);
                ops.extend(side.ops);
                cur = Cur::Pcs(side.pcs);
            }
            Cur::Pcs(from) => {
                let conv = conversion(profiles[i - 1], p, intent, bpc_i);
                let side = output_side(p, intent)?;
                connect(&mut ops, from, side.pcs, conv);
                ops.extend(side.ops);
                sides_out.push(side.shaper);
                convs.push(conv);
                cur = Cur::Device(p.header.color_space);
            }
        }
    }
    let (out_space, out_ch) = match cur {
        Cur::Device(s) => (s, channels(s)?),
        Cur::Pcs(pcs) => {
            encode_ops(&mut ops, pcs, false);
            (
                if pcs == Pcs::Lab {
                    ColorSpace::Lab
                } else {
                    ColorSpace::Xyz
                },
                3,
            )
        }
    };
    let shaper = if profiles.len() == 2 && !any_abstract {
        match (sides_in.first(), sides_out.first(), convs.first()) {
            (Some(Some(a)), Some(Some(b)), Some(conv)) => {
                let (c, off) = conv.unwrap_or((IDENTITY, [0.0; 3]));
                let m = mat_mul(&b.matrix, &mat_mul(&c, &a.matrix));
                Some(ShaperLink {
                    in_curves: a.curves.clone(),
                    out_curves: b.curves.clone(),
                    m,
                    off: mat_vec(&b.matrix, off),
                })
            }
            _ => None,
        }
    } else {
        None
    };
    Ok(Chain {
        ops,
        in_space,
        out_space,
        in_ch,
        out_ch,
        shaper,
    })
}

/// Joins the Lab encoding and decoding runs of `ops` into one op each (the
/// same arithmetic in the same order).
fn fuse_lab(ops: Vec<Op>) -> Vec<Op> {
    let mut out: Vec<Op> = Vec::with_capacity(ops.len());
    let mut it = ops.into_iter().peekable();
    while let Some(op) = it.next() {
        match op {
            Op::XyzToLab if matches!(it.peek(), Some(Op::EncodeLab)) => {
                it.next();
                let k = match it.peek() {
                    Some(&Op::Scale(k)) => {
                        it.next();
                        Some(k)
                    }
                    _ => None,
                };
                out.push(Op::XyzToEncodedLab(k));
            }
            Op::DecodeLab if matches!(it.peek(), Some(Op::LabToXyz)) => {
                it.next();
                match out.last() {
                    Some(&Op::Scale(k)) => {
                        out.pop();
                        out.push(Op::EncodedLabToXyz(Some(k)));
                    }
                    _ => out.push(Op::EncodedLabToXyz(None)),
                }
            }
            op => out.push(op),
        }
    }
    out
}

/// Replaces curve stages by forms evaluated in line where every curve of the
/// stage allows it (the same values: see [`FwdSeg`] and [`InvSeg`]).
fn prepare_curves(ops: &mut [Op]) {
    for op in ops {
        match op {
            Op::Lut(Stage::Curves(cs)) => {
                if let Some(segs) = cs.iter().map(FwdSeg::new).collect::<Option<Vec<_>>>() {
                    if !segs.is_empty() {
                        *op = Op::FwdSegs(segs);
                    }
                }
            }
            Op::InvBreakCurves(cs) => {
                if let Some(segs) = cs.iter().map(InvSeg::new).collect::<Option<Vec<_>>>() {
                    if !segs.is_empty() {
                        *op = Op::InvSegs(segs);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Device values of `src` to L*a*b* as they would arrive at `proof`, then
/// through `proof` and back, for gamut checking.
pub(crate) struct GamutChain {
    to_lab: Vec<Op>,
    proof_out: Vec<Op>,
    proof_in: Vec<Op>,
    pub(crate) in_ch: usize,
    pub(crate) in_space: ColorSpace,
}

impl GamutChain {
    pub(crate) fn new(src: &Profile, proof: &Profile, intent: Intent, bpc: bool) -> Result<Self> {
        let in_space = src.header.color_space;
        let in_ch = channels(in_space)?;
        let s = input_side(src, intent)?;
        let conv = conversion(src, proof, intent, effective_bpc(proof, intent, bpc));
        let mut to_lab = s.ops;
        connect(&mut to_lab, s.pcs, Pcs::Lab, conv);
        let o = output_side(proof, intent)?;
        let mut proof_out = Vec::new();
        connect(&mut proof_out, Pcs::Lab, o.pcs, None);
        proof_out.extend(o.ops);
        let back = input_side(proof, Intent::RelativeColorimetric)?;
        let mut proof_in = back.ops;
        connect(&mut proof_in, back.pcs, Pcs::Lab, None);
        Ok(Self {
            to_lab,
            proof_out,
            proof_in,
            in_ch,
            in_space,
        })
    }

    /// `(Lab before, Lab after the proof round trip)` for one colour.
    pub(crate) fn eval(&self, input: &[f64]) -> (Lab, Lab) {
        let mut v = [0.0; 16];
        v[..self.in_ch].copy_from_slice(&input[..self.in_ch]);
        run(&self.to_lab, &mut v, self.in_ch);
        let a = Lab::new(v[0], v[1], v[2]);
        let n = run(&self.proof_out, &mut v, 3);
        run(&self.proof_in, &mut v, n);
        (a, Lab::new(v[0], v[1], v[2]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fused_shaper_chain_matches_the_interpreter_bit_for_bit() {
        use crate::color::{Xy, D65_XY};
        let srgb = Profile::new_srgb();
        let p3 = Profile::new_display_p3();
        let lin = Profile::new_rgb(
            D65_XY,
            [
                Xy::new(0.708, 0.292),
                Xy::new(0.170, 0.797),
                Xy::new(0.131, 0.046),
            ],
            Curve::gamma(1.0),
        )
        .unwrap();
        let g22 = Profile::new_rgb(
            D65_XY,
            [
                Xy::new(0.64, 0.33),
                Xy::new(0.21, 0.71),
                Xy::new(0.15, 0.06),
            ],
            Curve::gamma(2.2),
        )
        .unwrap();
        let mut xs: Vec<f64> = (0..=64).map(|i| f64::from(i) / 64.0).collect();
        xs.extend([
            -0.25,
            -0.0,
            1.0e-9,
            0.04045,
            0.5000001,
            1.25,
            f64::NAN,
            f64::INFINITY,
        ]);
        let abs = Intent::AbsoluteColorimetric;
        let rel = Intent::RelativeColorimetric;
        for (a, b, intent, bpc) in [
            (&srgb, &p3, rel, false),
            (&p3, &srgb, rel, true),
            (&srgb, &lin, abs, false),
            (&lin, &g22, rel, false),
            (&g22, &srgb, Intent::Perceptual, true),
        ] {
            let chain = build_chain(&[a, b], &[intent, intent], &[bpc, bpc]).unwrap();
            let fused = chain.as_shaper().expect("matrix-shaper pair");
            // sRGB and Display P3 both ways are all two-segment curves.
            let segments = fused.segments();
            assert_eq!(
                segments.is_some(),
                std::ptr::eq(a, &srgb) && std::ptr::eq(b, &p3)
                    || std::ptr::eq(a, &p3) && std::ptr::eq(b, &srgb)
            );
            for &r in &xs {
                for &g in &xs {
                    for &bl in &[0.0, 0.3, 1.0, f64::NAN, -1.0, 2.0] {
                        let mut input = [0.0; 16];
                        input[..3].copy_from_slice(&[r, g, bl]);
                        let mut want = [0.0; 16];
                        chain.eval(&input, &mut want);
                        let got = fused.eval([r, g, bl]);
                        assert_eq!(
                            got.map(f64::to_bits),
                            [want[0], want[1], want[2]].map(f64::to_bits),
                            "{r} {g} {bl}"
                        );
                        if let Some(seg) = &segments {
                            assert_eq!(
                                seg.eval([r, g, bl]).map(f64::to_bits),
                                got.map(f64::to_bits),
                                "segments {r} {g} {bl}"
                            );
                        }
                    }
                }
            }
        }
        // Chains through a LUT or to Lab are not fused.
        let lab = Profile::new_lab_identity();
        let chain = build_chain(&[&srgb, &lab], &[rel, rel], &[false, false]).unwrap();
        assert!(chain.as_shaper().is_none());
    }

    #[test]
    fn sample_grid_matches_eval_at_every_point() {
        let srgb = Profile::new_srgb();
        let lab = Profile::new_lab_identity();
        let gray = Profile::new_gray(crate::color::D65_XY, Curve::gamma(2.2));
        let rel = Intent::RelativeColorimetric;
        let cgats =
            Profile::new_from_slice(include_bytes!("../tests/data/CGATS001Compat-v2-micro.icc"))
                .unwrap();
        for (a, b, g) in [
            (&srgb, &lab, 9usize),
            (&lab, &srgb, 7),
            (&gray, &srgb, 33),
            (&cgats, &srgb, 5),
            (&cgats, &lab, 4),
        ] {
            let chain = build_chain(&[a, b], &[rel, rel], &[false, false]).unwrap();
            if std::ptr::eq(a, &cgats) {
                // The tabulated-cell path of `sample_grid` is the one taken.
                assert!(matches!(chain.ops[0], Op::Sampled(_)));
                assert!(matches!(chain.ops[1], Op::Lut(Stage::Clut(_))));
            }
            let data = chain.sample_grid(g);
            let total = (0..chain.in_ch).fold(1usize, |n, _| n * g);
            assert_eq!(data.len(), total * chain.out_ch);
            for idx in 0..total {
                let mut input = [0.0; 16];
                let mut rem = idx;
                for c in (0..chain.in_ch).rev() {
                    input[c] = (rem % g) as f64 / (g - 1) as f64;
                    rem /= g;
                }
                let mut out = [0.0; 16];
                chain.eval(&input, &mut out);
                for o in 0..chain.out_ch {
                    let got = data[idx * chain.out_ch + o];
                    assert_eq!(got.to_bits(), (out[o] as f32).to_bits(), "point {idx}");
                }
            }
        }
    }

    /// Every LUT tag of the test profiles: its stages as prepared ops
    /// against the same stages run as plain stages, bit for bit.
    #[test]
    fn prepared_lut_ops_match_the_plain_stages_bit_for_bit() {
        let files: [&[u8]; 3] = [
            include_bytes!("../tests/data/CGATS001Compat-v2-micro.icc"),
            include_bytes!("../tests/data/sRGB_v4_ICC_preference.icc"),
            include_bytes!("../tests/data/LAB.icc"),
        ];
        let mut sampled = 0;
        for bytes in files {
            let p = Profile::new_from_slice(bytes).unwrap();
            for sig in [*b"A2B0", *b"A2B1", *b"A2B2", *b"B2A0", *b"B2A1", *b"B2A2"] {
                let Ok(Some(lut)) = p.lut_tag(sig) else {
                    continue;
                };
                let plain: Vec<Op> = lut.stages().into_iter().map(Op::Lut).collect();
                let prepared: Vec<Op> = lut.stages().into_iter().map(lut_op).collect();
                sampled += prepared
                    .iter()
                    .filter(|op| matches!(op, Op::Sampled(_)))
                    .count();
                let n = lut.inputs();
                let coords = [-0.25, 0.0, 0.1, 0.33, 0.5, 0.77, 0.999, 1.0, 1.5, f64::NAN];
                for k in 0..coords.len().pow(n.min(4) as u32) {
                    let mut a = [0.0; 16];
                    for (c, x) in a.iter_mut().enumerate().take(n) {
                        *x = coords[(k / coords.len().pow((c % 4) as u32)) % coords.len()];
                    }
                    let mut b = a;
                    let na = run(&plain, &mut a, n);
                    let nb = run(&prepared, &mut b, n);
                    assert_eq!(na, nb);
                    assert_eq!(
                        a[..na].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                        b[..nb].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                        "{sig:?} {k}"
                    );
                }
            }
        }
        // The prepared table op is reached by these profiles.
        assert!(sampled > 0);
    }

    /// Chains through LUT profiles with prepared curve ops against the same
    /// chains with every op left as built, bit for bit.
    #[test]
    fn prepared_chain_curves_match_the_plain_ops_bit_for_bit() {
        let srgb = Profile::new_srgb();
        let lab = Profile::new_lab_identity();
        let cgats =
            Profile::new_from_slice(include_bytes!("../tests/data/CGATS001Compat-v2-micro.icc"))
                .unwrap();
        let v4 =
            Profile::new_from_slice(include_bytes!("../tests/data/sRGB_v4_ICC_preference.icc"))
                .unwrap();
        let rel = Intent::RelativeColorimetric;
        let per = Intent::Perceptual;
        let (mut fwd, mut inv, mut fused, mut chains) = (0, 0, 0, 0);
        for (a, b, intent) in [
            (&srgb, &lab, rel),
            (&lab, &srgb, rel),
            (&srgb, &cgats, rel),
            (&cgats, &srgb, per),
            (&srgb, &v4, per),
            (&v4, &srgb, rel),
        ] {
            // The micro test profiles are one-directional.
            let Ok(chain) = build_chain(&[a, b], &[intent, intent], &[false, false]) else {
                continue;
            };
            chains += 1;
            assert!(chain.as_shaper().is_none());
            // The ops as linked, before `prepare_curves`.
            let plain = link_ops(&[a, b], &[intent, intent], &[false, false]).unwrap();

            for op in &chain.ops {
                fwd += usize::from(matches!(op, Op::FwdSegs(_)));
                inv += usize::from(matches!(op, Op::InvSegs(_)));
                fused += usize::from(matches!(
                    op,
                    Op::XyzToEncodedLab(_) | Op::EncodedLabToXyz(_)
                ));
            }
            let coords = [
                -0.25,
                0.0,
                0.01,
                0.04,
                0.33,
                0.5,
                0.77,
                0.999,
                1.0,
                1.5,
                f64::NAN,
            ];
            let n = chain.in_ch;
            for k in 0..coords.len().pow(n as u32) {
                let mut input = [0.0; 16];
                for (c, x) in input.iter_mut().enumerate().take(n) {
                    *x = coords[(k / coords.len().pow(c as u32)) % coords.len()];
                }
                let (mut want, mut got) = ([0.0; 16], [0.0; 16]);
                plain.eval(&input, &mut want);
                chain.eval(&input, &mut got);
                let m = chain.out_ch;
                assert_eq!(
                    want[..m].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    got[..m].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    "{k}"
                );
            }
            // The grid sampler reads the prepared head as it read the plain one.
            if n == 3 {
                assert_eq!(plain.sample_grid(9), chain.sample_grid(9));
            }
        }
        assert!(
            fwd > 0 && inv > 0 && fused > 0 && chains >= 4,
            "{fwd} {inv} {fused} {chains}"
        );
    }

    #[test]
    fn fused_lab_ops_match_the_separate_ops_bit_for_bit() {
        let k = 1.0 / LAB_V2_TO_V4;
        let runs: [Vec<Op>; 4] = [
            vec![Op::XyzToLab, Op::EncodeLab, Op::Scale(k), Op::Clamp01],
            vec![Op::XyzToLab, Op::EncodeLab, Op::Clamp01],
            vec![Op::Scale(LAB_V2_TO_V4), Op::DecodeLab, Op::LabToXyz],
            vec![Op::Clamp01, Op::DecodeLab, Op::LabToXyz, Op::XyzToLab],
        ];
        let xs = [-0.5, 0.0, 1e-4, 0.008, 0.2, 0.5, 0.9642, 1.0, 1.7, f64::NAN];
        for plain in runs {
            let fused = fuse_lab(plain.clone());
            assert!(fused.len() < plain.len());
            for (i, &a) in xs.iter().enumerate() {
                for &b in &xs {
                    for &c in &xs[i % 3..] {
                        let mut want = [0.0; 16];
                        want[..4].copy_from_slice(&[a, b, c, 0.25]);
                        let mut got = want;
                        assert_eq!(run(&plain, &mut want, 3), run(&fused, &mut got, 3));
                        assert_eq!(want.map(f64::to_bits), got.map(f64::to_bits), "{a} {b} {c}");
                    }
                }
            }
        }
    }

    #[test]
    fn unit_curve_stage_matches_curve_eval_bit_for_bit() {
        let forms = [
            Curve::gamma(1.0),
            Curve::parametric(1, &[1.0, 1.0, 0.0]).unwrap(),
            Curve::Parametric {
                function: 1,
                params: [1.0, 1.0, -0.0, 0.0, 0.0, 0.0, 0.0],
            },
            Curve::Table(vec![256]),
        ];
        let xs = [
            -1.0,
            -0.0,
            0.0,
            f64::MIN_POSITIVE,
            1.0e-300,
            0.25,
            0.999_999_999,
            1.0,
            1.0 + f64::EPSILON,
            7.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        for c in &forms {
            assert!(c.is_unit_gamma());
            let op = lut_op(Stage::Curves(vec![c.clone(); 3]));
            assert!(matches!(op, Op::UnitCurves(3)));
            for &x in &xs {
                let mut v = [0.0; 16];
                v[..4].copy_from_slice(&[x, x, x, x]);
                assert_eq!(run(std::slice::from_ref(&op), &mut v, 3), 3);
                for got in &v[..3] {
                    assert_eq!(got.to_bits(), c.eval(x).to_bits(), "{c:?} at {x}");
                }
                // Channels past the curves are left alone.
                assert_eq!(v[3].to_bits(), x.to_bits());
            }
        }
        // Not every identity: an empty `curv` keeps the sign of a zero, and
        // mixed stages stay per-curve.
        assert!(!Curve::identity().is_unit_gamma());
        assert!(!Curve::gamma(2.2).is_unit_gamma());
        let mixed = lut_op(Stage::Curves(vec![Curve::gamma(1.0), Curve::srgb()]));
        assert!(matches!(mixed, Op::Lut(Stage::Curves(_))));
    }
}
