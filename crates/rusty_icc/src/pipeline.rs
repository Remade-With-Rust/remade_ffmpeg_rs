//! Building and evaluating profile chains: device-to-PCS and PCS-to-device
//! sides, the PCS connection (absolute-colorimetric scaling and black-point
//! compensation) and the black-point detection they need. The structure
//! follows `LittleCMS` 2 (`cmsio1.c`, `cmscnvrt.c`, `cmssamp.c`) so results
//! are comparable with it.

use crate::color::{
    lab_to_xyz, mat_inv, mat_mul, mat_vec, xyz_to_lab, Lab, Mat3, Xyz, D50, IDENTITY,
};
use crate::curve::{clamp01, Curve, InvCurve};
use crate::error::{Error, Result};
use crate::lut::{make_trilinear, Lut, Stage};
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

/// One pipeline operation over up to 16 channels.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    /// A LUT stage over normalised values.
    Lut(Stage),
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
    /// XYZ affine map.
    Affine(Mat3, [f64; 3]),
    Clamp01,
    InvCurves(Vec<Curve>),
    /// [`Op::InvCurves`] when every curve is a two-segment parametric one
    /// (sRGB-shaped): the same values, without a `pow` per value for the
    /// curve's break point.
    InvBreakCurves(Vec<InvCurve>),
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

fn run(ops: &[Op], v: &mut [f64; 16], mut n: usize) -> usize {
    for op in ops {
        match op {
            Op::Lut(s) => n = s.apply(v, n),
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
    ops.extend(stages.into_iter().map(Op::Lut));
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
        ops.extend(lut.stages().into_iter().map(Op::Lut));
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
        ops.extend(stages.into_iter().map(Op::Lut));
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

impl ShaperChain<'_> {
    /// One colour: exactly what [`Chain::eval`] gives for it.
    #[inline]
    pub(crate) fn eval(&self, x: [f64; 3]) -> [f64; 3] {
        let [c0, c1, c2] = self.curves;
        let mut v = [c0.eval(x[0]), c1.eval(x[1]), c2.eval(x[2])];
        for (m, off) in &self.affines[..self.maps] {
            let r = mat_vec(m, v);
            v = [r[0] + off[0], r[1] + off[1], r[2] + off[2]];
        }
        let v = v.map(clamp01);
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
                    }
                }
            }
        }
        // Chains through a LUT or to Lab are not fused.
        let lab = Profile::new_lab_identity();
        let chain = build_chain(&[&srgb, &lab], &[rel, rel], &[false, false]).unwrap();
        assert!(chain.as_shaper().is_none());
    }
}
