//! Colour-science primitives: CIE XYZ, L*a*b*, chromaticities, the D50
//! profile connection space white, Bradford adaptation and CIEDE2000.

/// CIE XYZ tristimulus values, `Y = 1.0` at the reference white.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Xyz {
    /// X.
    pub x: f64,
    /// Y (luminance).
    pub y: f64,
    /// Z.
    pub z: f64,
}

impl Xyz {
    /// Builds a value from its three components.
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    pub(crate) const fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }

    pub(crate) const fn from_array(a: [f64; 3]) -> Self {
        Self::new(a[0], a[1], a[2])
    }

    /// The chromaticity of this colour (`x = X / (X + Y + Z)`, ...).
    #[must_use]
    pub fn to_xy(self) -> Xy {
        let s = self.x + self.y + self.z;
        if s == 0.0 {
            return Xy::new(0.0, 0.0);
        }
        Xy::new(self.x / s, self.y / s)
    }
}

/// CIE 1976 L*a*b* (D50 white unless stated otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Lab {
    /// Lightness, `0..=100`.
    pub l: f64,
    /// Green-red axis.
    pub a: f64,
    /// Blue-yellow axis.
    pub b: f64,
}

impl Lab {
    /// Builds a value from its three components.
    #[must_use]
    pub const fn new(l: f64, a: f64, b: f64) -> Self {
        Self { l, a, b }
    }
}

/// A CIE 1931 xy chromaticity.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Xy {
    /// x.
    pub x: f64,
    /// y.
    pub y: f64,
}

impl Xy {
    /// Builds a chromaticity.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// The tristimulus value with this chromaticity and `Y = 1`.
    #[must_use]
    pub fn to_xyz(self) -> Xyz {
        if self.y == 0.0 {
            return Xyz::default();
        }
        Xyz::new(self.x / self.y, 1.0, (1.0 - self.x - self.y) / self.y)
    }
}

/// The D50 white of the ICC profile connection space, as `LittleCMS` defines
/// it (`cmsD50_XYZ`).
pub const D50: Xyz = Xyz::new(0.9642, 1.0, 0.8249);

/// The CIE D65 white point chromaticity used by sRGB, Display P3 and Rec. 2020.
pub const D65_XY: Xy = Xy::new(0.3127, 0.3290);

fn lab_f(t: f64) -> f64 {
    const LIMIT: f64 = (24.0 / 116.0) * (24.0 / 116.0) * (24.0 / 116.0);
    if t <= LIMIT {
        (841.0 / 108.0) * t + 16.0 / 116.0
    } else {
        t.cbrt()
    }
}

fn lab_f_inv(t: f64) -> f64 {
    const LIMIT: f64 = 24.0 / 116.0;
    if t <= LIMIT {
        (108.0 / 841.0) * (t - 16.0 / 116.0)
    } else {
        t * t * t
    }
}

/// XYZ to L*a*b* relative to `white` (the `LittleCMS` formulation).
#[must_use]
pub fn xyz_to_lab(xyz: Xyz, white: Xyz) -> Lab {
    xyz_to_lab_in_line(xyz, white)
}

/// The body of [`xyz_to_lab`], in line wherever it is used: a straight-line
/// float chain takes its own copy (no call, the result in registers), every
/// other caller keeps calling the out-of-line [`xyz_to_lab`].
#[inline(always)]
pub(crate) fn xyz_to_lab_in_line(xyz: Xyz, white: Xyz) -> Lab {
    let fx = lab_f(xyz.x / white.x);
    let fy = lab_f(xyz.y / white.y);
    let fz = lab_f(xyz.z / white.z);
    Lab::new(116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// L*a*b* relative to `white` back to XYZ.
#[must_use]
pub fn lab_to_xyz(lab: Lab, white: Xyz) -> Xyz {
    let fy = (lab.l + 16.0) / 116.0;
    let fx = fy + 0.002 * lab.a;
    let fz = fy - 0.005 * lab.b;
    Xyz::new(
        lab_f_inv(fx) * white.x,
        lab_f_inv(fy) * white.y,
        lab_f_inv(fz) * white.z,
    )
}

/// CIEDE2000 colour difference (Sharma, Wu and Dalal 2005), `kL = kC = kH = 1`.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn delta_e_2000(lab1: Lab, lab2: Lab) -> f64 {
    use core::f64::consts::PI;
    let (l1, a1, b1) = (lab1.l, lab1.a, lab1.b);
    let (l2, a2, b2) = (lab2.l, lab2.a, lab2.b);
    let c1 = a1.hypot(b1);
    let c2 = a2.hypot(b2);
    let cm = 0.5 * (c1 + c2);
    let cm7 = cm.powi(7);
    let g = 0.5 * (1.0 - (cm7 / (cm7 + 25f64.powi(7))).sqrt());
    let a1p = (1.0 + g) * a1;
    let a2p = (1.0 + g) * a2;
    let c1p = a1p.hypot(b1);
    let c2p = a2p.hypot(b2);
    let hue = |b: f64, a: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            let h = b.atan2(a);
            if h < 0.0 {
                h + 2.0 * PI
            } else {
                h
            }
        }
    };
    let h1p = hue(b1, a1p);
    let h2p = hue(b2, a2p);
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let dhp = if c1p * c2p == 0.0 {
        0.0
    } else {
        let d = h2p - h1p;
        if d > PI {
            d - 2.0 * PI
        } else if d < -PI {
            d + 2.0 * PI
        } else {
            d
        }
    };
    let dh = 2.0 * (c1p * c2p).sqrt() * (dhp / 2.0).sin();
    let lm = 0.5 * (l1 + l2);
    let cmp = 0.5 * (c1p + c2p);
    let hm = if c1p * c2p == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= PI {
        0.5 * (h1p + h2p)
    } else if h1p + h2p < 2.0 * PI {
        0.5 * (h1p + h2p + 2.0 * PI)
    } else {
        0.5 * (h1p + h2p - 2.0 * PI)
    };
    let t = 1.0 - 0.17 * (hm - PI / 6.0).cos()
        + 0.24 * (2.0 * hm).cos()
        + 0.32 * (3.0 * hm + PI / 30.0).cos()
        - 0.20 * (4.0 * hm - 63.0 * PI / 180.0).cos();
    let dtheta = (30.0 * PI / 180.0) * (-((hm * 180.0 / PI - 275.0) / 25.0).powi(2)).exp();
    let cmp7 = cmp.powi(7);
    let rc = 2.0 * (cmp7 / (cmp7 + 25f64.powi(7))).sqrt();
    let lm50 = (lm - 50.0) * (lm - 50.0);
    let sl = 1.0 + 0.015 * lm50 / (20.0 + lm50).sqrt();
    let sc = 1.0 + 0.045 * cmp;
    let sh = 1.0 + 0.015 * cmp * t;
    let rt = -(2.0 * dtheta).sin() * rc;
    let (tl, tc, th) = (dl / sl, dc / sc, dh / sh);
    (tl * tl + tc * tc + th * th + rt * tc * th).max(0.0).sqrt()
}

/// A row-major 3x3 matrix.
pub(crate) type Mat3 = [[f64; 3]; 3];

pub(crate) const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub(crate) fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    r
}

pub(crate) fn mat_vec(m: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Inverse of a 3x3 matrix, `None` when (numerically) singular.
pub(crate) fn mat_inv(m: &Mat3) -> Option<Mat3> {
    let a = m;
    let c00 = a[1][1] * a[2][2] - a[1][2] * a[2][1];
    let c01 = a[1][2] * a[2][0] - a[1][0] * a[2][2];
    let c02 = a[1][0] * a[2][1] - a[1][1] * a[2][0];
    let det = a[0][0] * c00 + a[0][1] * c01 + a[0][2] * c02;
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    Some([
        [
            c00 * inv,
            (a[0][2] * a[2][1] - a[0][1] * a[2][2]) * inv,
            (a[0][1] * a[1][2] - a[0][2] * a[1][1]) * inv,
        ],
        [
            c01 * inv,
            (a[0][0] * a[2][2] - a[0][2] * a[2][0]) * inv,
            (a[0][2] * a[1][0] - a[0][0] * a[1][2]) * inv,
        ],
        [
            c02 * inv,
            (a[0][1] * a[2][0] - a[0][0] * a[2][1]) * inv,
            (a[0][0] * a[1][1] - a[0][1] * a[1][0]) * inv,
        ],
    ])
}

const BRADFORD: Mat3 = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

/// The Bradford chromatic adaptation matrix taking colours seen under
/// `src_white` to their corresponding colours under `dst_white`.
pub(crate) fn bradford(src_white: Xyz, dst_white: Xyz) -> Mat3 {
    let s = mat_vec(&BRADFORD, src_white.to_array());
    let d = mat_vec(&BRADFORD, dst_white.to_array());
    let scale = [
        [d[0] / s[0], 0.0, 0.0],
        [0.0, d[1] / s[1], 0.0],
        [0.0, 0.0, d[2] / s[2]],
    ];
    // BRADFORD is a constant with a non-zero determinant.
    let inv = mat_inv(&BRADFORD).unwrap_or(IDENTITY);
    mat_mul(&inv, &mat_mul(&scale, &BRADFORD))
}

/// RGB-to-XYZ matrix (columns are the primaries' XYZ) for the given
/// primaries and white point, white mapping to `Y = 1`.
pub(crate) fn rgb_to_xyz_matrix(white: Xy, primaries: [Xy; 3]) -> Option<Mat3> {
    let p = primaries.map(Xy::to_xyz);
    let m = [
        [p[0].x, p[1].x, p[2].x],
        [p[0].y, p[1].y, p[2].y],
        [p[0].z, p[1].z, p[2].z],
    ];
    let s = mat_vec(&mat_inv(&m)?, white.to_xyz().to_array());
    let mut r = m;
    for row in &mut r {
        for (c, v) in row.iter_mut().enumerate() {
            *v *= s[c];
        }
    }
    Some(r)
}

/// Rounds to the nearest `s15Fixed16Number` and back, saturating.
pub(crate) fn q15f16(v: f64) -> f64 {
    f64::from(to_s15f16(v)) / 65536.0
}

/// Encodes an `s15Fixed16Number`, saturating out-of-range and NaN to 0.
pub(crate) fn to_s15f16(v: f64) -> i32 {
    let r = (v * 65536.0).round();
    if r.is_nan() {
        0
    } else {
        // Saturating float-to-int cast is the defined Rust semantics.
        r.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn de2000_sharma_reference_pairs() {
        // Rows 1, 7, 13 and 25 of Sharma's published test data.
        let rows = [
            ((50.0, 2.6772, -79.7751), (50.0, 0.0, -82.7485), 2.0425),
            ((50.0, 0.0, 0.0), (50.0, -1.0, 2.0), 2.3669),
            ((50.0, 2.49, -0.001), (50.0, -2.49, 0.0011), 7.2195),
            (
                (60.2574, -34.0099, 36.2677),
                (60.4626, -34.1751, 39.4387),
                1.2644,
            ),
        ];
        for (a, b, want) in rows {
            let d = delta_e_2000(Lab::new(a.0, a.1, a.2), Lab::new(b.0, b.1, b.2));
            assert!((d - want).abs() < 1e-4, "{d} vs {want}");
        }
    }

    #[test]
    fn lab_round_trip() {
        for &(x, y, z) in &[(0.2, 0.3, 0.1), (0.0, 0.0, 0.0), (0.9642, 1.0, 0.8249)] {
            let v = Xyz::new(x, y, z);
            let back = lab_to_xyz(xyz_to_lab(v, D50), D50);
            assert!((back.x - x).abs() < 1e-12 && (back.y - y).abs() < 1e-12);
        }
    }
}
