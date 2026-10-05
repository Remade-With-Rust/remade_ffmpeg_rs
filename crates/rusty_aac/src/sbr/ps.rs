//! **Parametric Stereo** (HE-AAC v2, ISO/IEC 14496-3 §8.6.4): a mono SBR
//! signal plus per-band intensity (IID), coherence (ICC) and optionally phase
//! (IPD/OPD) parameters is turned into stereo in the QMF domain — hybrid
//! analysis of the low QMF bands, an all-pass decorrelator with transient
//! ducking, per-band 2x2 mixing interpolated across envelopes, and hybrid
//! synthesis. Full (non-baseline) PS: 10/20/34 stereo bands and IPD/OPD.

use super::dec::SbrBook;
use crate::bits::BitReader;
use crate::Result;
use std::f64::consts::{PI, SQRT_2};
use std::sync::OnceLock;

type Cpx = [f32; 2];

const MAX_ENV: usize = 5;
const MAX_PAR: usize = 34;
const MAX_SSB: usize = 91;
const AP_LINKS: usize = 3;
const MAX_DELAY: usize = 14;
const MAX_AP_DELAY: usize = 5;
/// The most QMF slots in a frame (1024-sample frames; 960 gives 30).
const QMF_SLOTS: usize = 32;

const NUM_ENV_TAB: [[usize; 4]; 2] = [[0, 1, 2, 4], [1, 2, 3, 4]];
const NR_IIDICC_PAR: [usize; 6] = [10, 20, 34, 10, 20, 34];
const NR_IPDOPD_PAR: [usize; 6] = [5, 11, 17, 5, 11, 17];

const NR_PAR_BANDS: [usize; 2] = [20, 34];
const NR_IPDOPD_BANDS: [usize; 2] = [11, 17];
const NR_BANDS: [usize; 2] = [71, 91];
const DECAY_CUTOFF: [i32; 2] = [10, 32];
const NR_ALLPASS_BANDS: [usize; 2] = [30, 50];
const SHORT_DELAY_BAND: [usize; 2] = [42, 62];

/// Hybrid sub-subband to stereo parameter band (Tables 8.48/8.49).
const K_TO_I_20: [u8; 71] = [
    1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 14, 15, 15, 15, 16, 16, 16, 16, 17, 17,
    17, 17, 17, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 19,
    19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19,
];
const K_TO_I_34: [u8; 91] = [
    0, 1, 2, 3, 4, 5, 6, 6, 7, 2, 1, 0, 10, 10, 4, 5, 6, 7, 8, 9, 10, 11, 12, 9, 14, 11, 12, 13,
    14, 15, 16, 13, 16, 17, 18, 19, 20, 21, 22, 22, 23, 23, 24, 24, 25, 25, 26, 26, 27, 27, 27, 28,
    28, 28, 29, 29, 29, 30, 30, 30, 31, 31, 31, 31, 32, 32, 32, 32, 33, 33, 33, 33, 33, 33, 33, 33,
    33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33,
];

// PS Huffman codebooks (Tables 8.B.*) as (symbol, length) in canonical order;
// value = symbol + offset.
const HUFF_IID_DF1: (&[(u8, u8)], i32) = (
    &[
        (28, 4),
        (32, 4),
        (29, 3),
        (31, 3),
        (27, 5),
        (33, 5),
        (26, 6),
        (34, 6),
        (25, 7),
        (35, 7),
        (24, 8),
        (36, 8),
        (37, 9),
        (40, 11),
        (19, 12),
        (41, 12),
        (22, 10),
        (38, 10),
        (9, 17),
        (51, 17),
        (11, 17),
        (49, 17),
        (13, 16),
        (47, 16),
        (16, 14),
        (18, 13),
        (42, 13),
        (44, 14),
        (12, 17),
        (48, 17),
        (4, 18),
        (5, 18),
        (2, 18),
        (3, 18),
        (15, 15),
        (21, 11),
        (39, 11),
        (45, 15),
        (8, 18),
        (52, 18),
        (6, 18),
        (7, 18),
        (55, 18),
        (56, 18),
        (53, 18),
        (54, 18),
        (17, 14),
        (43, 14),
        (59, 18),
        (60, 18),
        (57, 18),
        (58, 18),
        (0, 18),
        (1, 18),
        (10, 18),
        (50, 18),
        (14, 16),
        (46, 16),
        (20, 12),
        (23, 10),
        (30, 1),
    ],
    -30,
);
const HUFF_IID_DT1: (&[(u8, u8)], i32) = (
    &[
        (31, 2),
        (26, 7),
        (34, 7),
        (27, 6),
        (33, 6),
        (35, 8),
        (24, 9),
        (36, 9),
        (39, 11),
        (41, 12),
        (9, 15),
        (10, 15),
        (48, 15),
        (49, 15),
        (17, 13),
        (23, 10),
        (37, 10),
        (43, 13),
        (11, 15),
        (12, 15),
        (4, 16),
        (56, 16),
        (2, 16),
        (3, 16),
        (59, 16),
        (60, 16),
        (57, 16),
        (58, 16),
        (0, 16),
        (1, 16),
        (5, 16),
        (55, 16),
        (6, 16),
        (54, 16),
        (13, 15),
        (15, 14),
        (20, 12),
        (40, 12),
        (22, 11),
        (38, 11),
        (45, 14),
        (47, 15),
        (7, 16),
        (53, 16),
        (18, 13),
        (42, 13),
        (16, 14),
        (44, 14),
        (8, 16),
        (52, 16),
        (14, 15),
        (46, 15),
        (50, 16),
        (51, 16),
        (19, 13),
        (21, 12),
        (25, 9),
        (28, 5),
        (32, 5),
        (29, 3),
        (30, 1),
    ],
    -30,
);
const HUFF_IID_DF0: (&[(u8, u8)], i32) = (
    &[
        (14, 1),
        (15, 3),
        (13, 3),
        (16, 4),
        (12, 4),
        (17, 5),
        (11, 5),
        (10, 6),
        (18, 6),
        (19, 6),
        (9, 7),
        (20, 8),
        (8, 9),
        (7, 10),
        (21, 11),
        (22, 13),
        (6, 13),
        (23, 14),
        (24, 14),
        (5, 15),
        (25, 15),
        (4, 16),
        (3, 17),
        (0, 17),
        (1, 17),
        (2, 17),
        (26, 17),
        (27, 18),
        (28, 18),
    ],
    -14,
);
const HUFF_IID_DT0: (&[(u8, u8)], i32) = (
    &[
        (14, 1),
        (13, 2),
        (15, 3),
        (12, 4),
        (16, 5),
        (11, 6),
        (17, 7),
        (10, 8),
        (18, 9),
        (9, 10),
        (19, 11),
        (8, 12),
        (20, 13),
        (21, 14),
        (7, 15),
        (22, 17),
        (6, 17),
        (23, 19),
        (0, 19),
        (1, 19),
        (2, 19),
        (3, 20),
        (4, 20),
        (5, 20),
        (24, 20),
        (25, 20),
        (26, 20),
        (27, 20),
        (28, 20),
    ],
    -14,
);
const HUFF_ICC_DF: (&[(u8, u8)], i32) = (
    &[
        (7, 1),
        (8, 2),
        (6, 3),
        (9, 4),
        (5, 5),
        (10, 6),
        (4, 7),
        (11, 8),
        (12, 9),
        (3, 10),
        (13, 11),
        (2, 12),
        (14, 13),
        (1, 14),
        (0, 14),
    ],
    -7,
);
const HUFF_ICC_DT: (&[(u8, u8)], i32) = (
    &[
        (7, 1),
        (8, 2),
        (6, 3),
        (9, 4),
        (5, 5),
        (10, 6),
        (4, 7),
        (11, 8),
        (3, 9),
        (12, 10),
        (2, 11),
        (13, 12),
        (1, 13),
        (0, 14),
        (14, 14),
    ],
    -7,
);
const HUFF_IPD_DF: (&[(u8, u8)], i32) = (
    &[
        (1, 3),
        (4, 4),
        (5, 4),
        (3, 4),
        (6, 4),
        (2, 4),
        (7, 4),
        (0, 1),
    ],
    0,
);
const HUFF_IPD_DT: (&[(u8, u8)], i32) = (
    &[
        (5, 4),
        (4, 5),
        (3, 5),
        (2, 4),
        (6, 4),
        (1, 3),
        (7, 3),
        (0, 1),
    ],
    0,
);
const HUFF_OPD_DF: (&[(u8, u8)], i32) = (
    &[
        (7, 3),
        (1, 3),
        (3, 4),
        (6, 4),
        (2, 4),
        (5, 5),
        (4, 5),
        (0, 1),
    ],
    0,
);
const HUFF_OPD_DT: (&[(u8, u8)], i32) = (
    &[
        (5, 4),
        (2, 4),
        (6, 4),
        (4, 5),
        (3, 5),
        (1, 3),
        (7, 3),
        (0, 1),
    ],
    0,
);

struct PsBooks {
    iid_df: [SbrBook; 2], // [iid_quant]
    iid_dt: [SbrBook; 2],
    icc_df: SbrBook,
    icc_dt: SbrBook,
    ipd_df: SbrBook,
    ipd_dt: SbrBook,
    opd_df: SbrBook,
    opd_dt: SbrBook,
}

fn books() -> &'static PsBooks {
    static B: OnceLock<PsBooks> = OnceLock::new();
    B.get_or_init(|| PsBooks {
        iid_df: [SbrBook::new(&HUFF_IID_DF0), SbrBook::new(&HUFF_IID_DF1)],
        iid_dt: [SbrBook::new(&HUFF_IID_DT0), SbrBook::new(&HUFF_IID_DT1)],
        icc_df: SbrBook::new(&HUFF_ICC_DF),
        icc_dt: SbrBook::new(&HUFF_ICC_DT),
        ipd_df: SbrBook::new(&HUFF_IPD_DF),
        ipd_dt: SbrBook::new(&HUFF_IPD_DT),
        opd_df: SbrBook::new(&HUFF_OPD_DF),
        opd_dt: SbrBook::new(&HUFF_OPD_DT),
    })
}

// ---------------------------------------------------------------------------
// Derived tables.
// ---------------------------------------------------------------------------

struct PsTables {
    pd_re_smooth: Vec<f32>,
    pd_im_smooth: Vec<f32>,
    ha: Vec<[[f32; 4]; 8]>,
    hb: Vec<[[f32; 4]; 8]>,
    f20_0_8: Vec<[Cpx; 8]>,
    f34_0_12: Vec<[Cpx; 8]>,
    f34_1_8: Vec<[Cpx; 8]>,
    f34_2_4: Vec<[Cpx; 8]>,
    q_fract_allpass: [Vec<[Cpx; 3]>; 2],
    phi_fract: [Vec<Cpx>; 2],
}

fn filters_from_proto(proto: &[f32; 7], bands: usize) -> Vec<[Cpx; 8]> {
    (0..bands)
        .map(|q| {
            let mut f = [[0f32; 2]; 8];
            for (n, &p) in proto.iter().enumerate() {
                let theta = 2.0 * PI * (q as f64 + 0.5) * (n as f64 - 6.0) / bands as f64;
                f[n] = [
                    (p as f64 * theta.cos()) as f32,
                    (p as f64 * -theta.sin()) as f32,
                ];
            }
            f
        })
        .collect()
}

fn tables() -> &'static PsTables {
    static T: OnceLock<PsTables> = OnceLock::new();
    T.get_or_init(|| {
        let r = std::f32::consts::FRAC_1_SQRT_2;
        let ipdopd_sin = [0.0, r, 1.0, r, 0.0, -r, -1.0, -r];
        let ipdopd_cos = [1.0, r, 0.0, -r, -1.0, -r, 0.0, r];
        let mut pd_re_smooth = vec![0f32; 512];
        let mut pd_im_smooth = vec![0f32; 512];
        for pd0 in 0..8 {
            for pd1 in 0..8 {
                for pd2 in 0..8 {
                    let re: f32 = 0.25 * ipdopd_cos[pd0] + 0.5 * ipdopd_cos[pd1] + ipdopd_cos[pd2];
                    let im: f32 = 0.25 * ipdopd_sin[pd0] + 0.5 * ipdopd_sin[pd1] + ipdopd_sin[pd2];
                    let mag = 1.0 / (im as f64).hypot(re as f64);
                    pd_re_smooth[pd0 * 64 + pd1 * 8 + pd2] = (re as f64 * mag) as f32;
                    pd_im_smooth[pd0 * 64 + pd1 * 8 + pd2] = (im as f64 * mag) as f32;
                }
            }
        }
        let iid_par_dequant: [f32; 46] = [
            0.05623413251903,
            0.12589254117942,
            0.19952623149689,
            0.31622776601684,
            0.44668359215096,
            0.63095734448019,
            0.79432823472428,
            1.0,
            1.25892541179417,
            1.58489319246111,
            2.23872113856834,
            3.16227766016838,
            5.01187233627272,
            7.94328234724282,
            17.7827941003892,
            0.00316227766017,
            0.00562341325190,
            0.01,
            0.01778279410039,
            0.03162277660168,
            0.05623413251903,
            0.07943282347243,
            0.11220184543020,
            0.15848931924611,
            0.22387211385683,
            0.31622776601684,
            0.39810717055350,
            0.50118723362727,
            0.63095734448019,
            0.79432823472428,
            1.0,
            1.25892541179417,
            1.58489319246111,
            1.99526231496888,
            2.51188643150958,
            3.16227766016838,
            4.46683592150963,
            6.30957344480193,
            8.91250938133745,
            12.5892541179417,
            17.7827941003892,
            31.6227766016838,
            56.2341325190349,
            100.0,
            177.827941003892,
            316.227766016837,
        ];
        let icc_invq: [f32; 8] = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0];
        let acos_icc_invq: [f32; 8] = [
            0.0,
            0.35685527,
            0.57133466,
            0.92614472,
            1.1943263,
            std::f32::consts::FRAC_PI_2,
            2.2006171,
            std::f32::consts::PI,
        ];
        let mut ha = vec![[[0f32; 4]; 8]; 46];
        let mut hb = vec![[[0f32; 4]; 8]; 46];
        for iid in 0..46 {
            let c = iid_par_dequant[iid];
            let c1 = std::f32::consts::SQRT_2 / (1.0 + c * c).sqrt();
            let c2 = c * c1;
            for icc in 0..8 {
                // Mixing procedure R_A (ICC modes 0-2).
                let alpha = 0.5f32 * acos_icc_invq[icc];
                let beta = alpha * (c1 - c2) * std::f32::consts::FRAC_1_SQRT_2;
                ha[iid][icc] = [
                    c2 * (beta + alpha).cos(),
                    c1 * (beta - alpha).cos(),
                    c2 * (beta + alpha).sin(),
                    c1 * (beta - alpha).sin(),
                ];
                // Mixing procedure R_B (ICC modes 3-5).
                let rho = icc_invq[icc].max(0.05);
                let mut alpha = 0.5f32 * (2.0 * c * rho).atan2(c * c - 1.0);
                let mu = c + 1.0 / c;
                let mu = (1.0 + (4.0 * rho * rho - 4.0) / (mu * mu)).sqrt();
                let gamma = ((1.0 - mu) / (1.0 + mu)).sqrt().atan();
                if alpha < 0.0 {
                    alpha = (alpha as f64 + PI / 2.0) as f32;
                }
                let (ac, as_, gc, gs) = (
                    alpha.cos() as f64,
                    alpha.sin() as f64,
                    gamma.cos() as f64,
                    gamma.sin() as f64,
                );
                hb[iid][icc] = [
                    (SQRT_2 * ac * gc) as f32,
                    (SQRT_2 * as_ * gc) as f32,
                    (-SQRT_2 * as_ * gs) as f32,
                    (SQRT_2 * ac * gs) as f32,
                ];
            }
        }
        let f_center_20: [i8; 10] = [-3, -1, 1, 3, 5, 7, 10, 14, 18, 22];
        let f_center_34: [i8; 32] = [
            2, 6, 10, 14, 18, 22, 26, 30, 34, -10, -6, -2, 51, 57, 15, 21, 27, 33, 39, 45, 54, 66,
            78, 42, 102, 66, 78, 90, 102, 114, 126, 90,
        ];
        let links: [f32; 3] = [0.43, 0.75, 0.347];
        let gain = 0.39f32;
        let mut q_fract_allpass = [Vec::new(), Vec::new()];
        let mut phi_fract = [Vec::new(), Vec::new()];
        for (is34, (n, base)) in [(30usize, 6.5f32), (50, 26.5)].into_iter().enumerate() {
            for k in 0..n {
                let f_center = if is34 == 0 {
                    f_center_20.get(k).map(|&v| v as f64 * 0.125)
                } else {
                    f_center_34.get(k).map(|&v| v as f64 / 24.0)
                }
                .unwrap_or(k as f64 - base as f64);
                let mut q = [[0f32; 2]; 3];
                for (m, &l) in links.iter().enumerate() {
                    let theta = -PI * l as f64 * f_center;
                    q[m] = [theta.cos() as f32, theta.sin() as f32];
                }
                q_fract_allpass[is34].push(q);
                let theta = -PI * gain as f64 * f_center;
                phi_fract[is34].push([theta.cos() as f32, theta.sin() as f32]);
            }
        }
        PsTables {
            pd_re_smooth,
            pd_im_smooth,
            ha,
            hb,
            f20_0_8: filters_from_proto(
                &[
                    0.00746082949812,
                    0.02270420949825,
                    0.04546865930473,
                    0.07266113929591,
                    0.09885108575264,
                    0.11793710567217,
                    0.125,
                ],
                8,
            ),
            f34_0_12: filters_from_proto(
                &[
                    0.04081179924692,
                    0.03812810994926,
                    0.05144908135699,
                    0.06399831151592,
                    0.07428313801106,
                    0.08100347892914,
                    0.08333333333333,
                ],
                12,
            ),
            f34_1_8: filters_from_proto(
                &[
                    0.01565675600122,
                    0.03752716391991,
                    0.05417891378782,
                    0.08417044116767,
                    0.10307344158036,
                    0.12222452249753,
                    0.125,
                ],
                8,
            ),
            f34_2_4: filters_from_proto(
                &[
                    -0.05908211155639,
                    -0.04871498374946,
                    0.0,
                    0.07778723915851,
                    0.16486303567403,
                    0.23279856662996,
                    0.25,
                ],
                4,
            ),
            q_fract_allpass,
            phi_fract,
        }
    })
}

/// Real 2-band split filter (center tap 0.5, odd taps only).
const G1_Q2: [f32; 7] = [
    0.0,
    0.01899487526049,
    0.0,
    -0.07293139167538,
    0.0,
    0.30596630545168,
    0.5,
];

// ---------------------------------------------------------------------------
// State.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct PsState {
    start: bool,
    enable_iid: bool,
    iid_quant: usize,
    nr_iid_par: usize,
    nr_ipdopd_par: usize,
    enable_icc: bool,
    icc_mode: usize,
    nr_icc_par: usize,
    enable_ext: bool,
    num_env_old: usize,
    num_env: usize,
    enable_ipdopd: bool,
    border_position: [i32; MAX_ENV + 1],
    iid_par: [[i8; MAX_PAR]; MAX_ENV],
    icc_par: [[i8; MAX_PAR]; MAX_ENV],
    ipd_par: [[i8; MAX_PAR]; MAX_ENV],
    opd_par: [[i8; MAX_PAR]; MAX_ENV],
    is34bands: bool,
    is34bands_old: bool,
    /// QMF slots per frame: 32, or 30 for 960-sample frames.
    slots: usize,
    // processing state
    in_buf: Vec<[Cpx; 44]>,
    delay: Vec<[Cpx; QMF_SLOTS + MAX_DELAY]>,
    ap_delay: Vec<[[Cpx; QMF_SLOTS + MAX_AP_DELAY]; AP_LINKS]>,
    /// The hybrid-domain left/right matrices, kept across frames (see `apply`).
    lbuf: Vec<[Cpx; 32]>,
    rbuf: Vec<[Cpx; 32]>,
    /// Decorrelation's per-parameter-band transient gains (see `decorrelation`).
    gain: Vec<[f32; 32]>,
    peak_decay_nrg: [f32; 34],
    power_smooth: [f32; 34],
    peak_decay_diff_smooth: [f32; 34],
    /// H11, H12, H21, H22 as [h][re/im][env][band].
    h: [[[[f32; MAX_PAR]; MAX_ENV + 1]; 2]; 4],
    opd_hist: [i8; MAX_PAR],
    ipd_hist: [i8; MAX_PAR],
}

impl PsState {
    pub(crate) fn new() -> PsState {
        PsState {
            start: false,
            enable_iid: false,
            iid_quant: 0,
            nr_iid_par: 0,
            nr_ipdopd_par: 0,
            enable_icc: false,
            icc_mode: 0,
            nr_icc_par: 0,
            enable_ext: false,
            num_env_old: 0,
            num_env: 0,
            enable_ipdopd: false,
            border_position: [0; MAX_ENV + 1],
            iid_par: [[0; MAX_PAR]; MAX_ENV],
            icc_par: [[0; MAX_PAR]; MAX_ENV],
            ipd_par: [[0; MAX_PAR]; MAX_ENV],
            opd_par: [[0; MAX_PAR]; MAX_ENV],
            is34bands: false,
            is34bands_old: false,
            slots: QMF_SLOTS,
            in_buf: vec![[[0.0; 2]; 44]; 5],
            delay: vec![[[0.0; 2]; QMF_SLOTS + MAX_DELAY]; MAX_SSB],
            ap_delay: vec![[[[0.0; 2]; QMF_SLOTS + MAX_AP_DELAY]; AP_LINKS]; 50],
            lbuf: vec![[[0.0; 2]; 32]; MAX_SSB],
            rbuf: vec![[[0.0; 2]; 32]; MAX_SSB],
            gain: vec![[0.0; 32]; 34],
            peak_decay_nrg: [0.0; 34],
            power_smooth: [0.0; 34],
            peak_decay_diff_smooth: [0.0; 34],
            h: [[[[0.0; MAX_PAR]; MAX_ENV + 1]; 2]; 4],
            opd_hist: [0; MAX_PAR],
            ipd_hist: [0; MAX_PAR],
        }
    }

    pub(crate) fn started(&self) -> bool {
        self.start
    }
}

// ---------------------------------------------------------------------------
// Bitstream (§8.4.2 ps_data).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Par {
    Iid,
    Icc,
    Ipd,
    Opd,
}

impl PsState {
    fn par(&mut self, p: Par) -> &mut [[i8; MAX_PAR]; MAX_ENV] {
        match p {
            Par::Iid => &mut self.iid_par,
            Par::Icc => &mut self.icc_par,
            Par::Ipd => &mut self.ipd_par,
            Par::Opd => &mut self.opd_par,
        }
    }

    /// Delta-decode one envelope of a parameter; Ok(false) on an illegal value.
    fn read_par(
        &mut self,
        r: &mut BitReader,
        p: Par,
        book: &SbrBook,
        e: usize,
        dt: bool,
    ) -> Result<bool> {
        let (num, mask) = match p {
            Par::Iid => (self.nr_iid_par, 0),
            Par::Icc => (self.nr_icc_par, 0),
            Par::Ipd | Par::Opd => (self.nr_ipdopd_par, 7),
        };
        let limit = 7 + 8 * self.iid_quant as i32;
        let e_prev = if e > 0 {
            e - 1
        } else {
            self.num_env_old.saturating_sub(1)
        };
        let mut acc = 0i32;
        for b in 0..num {
            let delta = book.decode(r)?;
            let mut val = if dt {
                self.par(p)[e_prev][b] as i32 + delta
            } else {
                acc + delta
            };
            if mask != 0 {
                val &= mask;
            }
            acc = val;
            let bad = match p {
                Par::Iid => val.abs() > limit,
                Par::Icc => !(0..=7).contains(&val),
                _ => false,
            };
            // The stored value is an int8 in the reference.
            self.par(p)[e][b] = val as i8;
            if bad {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn read_extension(&mut self, r: &mut BitReader, id: u32) -> Result<usize> {
        if id != 0 {
            return Ok(0);
        }
        let start = r.position();
        let bk = books();
        self.enable_ipdopd = r.read_bool()?;
        if self.enable_ipdopd {
            for e in 0..self.num_env {
                let dt = r.read_bool()?;
                self.read_par(r, Par::Ipd, if dt { &bk.ipd_dt } else { &bk.ipd_df }, e, dt)?;
                let dt = r.read_bool()?;
                self.read_par(r, Par::Opd, if dt { &bk.opd_dt } else { &bk.opd_df }, e, dt)?;
            }
        }
        r.skip(1)?; // reserved_ps
        Ok(r.position() - start)
    }

    /// Parse `ps_data()` within `bits_left` bits; returns the bits consumed
    /// (all of `bits_left` on error, which also disables PS until the next header).
    pub(crate) fn read_data(
        &mut self,
        r: &mut BitReader,
        bits_left: usize,
        slots: usize,
    ) -> Result<usize> {
        self.slots = slots;
        let start = r.position();
        match self.read_data_inner(r) {
            Ok(true) if r.position() - start <= bits_left => Ok(r.position() - start),
            _ => {
                self.start = false;
                self.iid_par = [[0; MAX_PAR]; MAX_ENV];
                self.icc_par = [[0; MAX_PAR]; MAX_ENV];
                self.ipd_par = [[0; MAX_PAR]; MAX_ENV];
                self.opd_par = [[0; MAX_PAR]; MAX_ENV];
                r.set_position(start + bits_left);
                Ok(bits_left)
            }
        }
    }

    fn read_data_inner(&mut self, r: &mut BitReader) -> Result<bool> {
        let bk = books();
        let header = r.read_bool()?;
        if header {
            self.enable_iid = r.read_bool()?;
            if self.enable_iid {
                let mode = r.read_bits(3)? as usize;
                if mode > 5 {
                    return Ok(false);
                }
                self.nr_iid_par = NR_IIDICC_PAR[mode];
                self.iid_quant = (mode > 2) as usize;
                self.nr_ipdopd_par = NR_IPDOPD_PAR[mode];
            }
            self.enable_icc = r.read_bool()?;
            if self.enable_icc {
                self.icc_mode = r.read_bits(3)? as usize;
                if self.icc_mode > 5 {
                    return Ok(false);
                }
                self.nr_icc_par = NR_IIDICC_PAR[self.icc_mode];
            }
            self.enable_ext = r.read_bool()?;
        }
        let frame_class = r.read_bool()?;
        self.num_env_old = self.num_env;
        self.num_env = NUM_ENV_TAB[frame_class as usize][r.read_bits(2)? as usize];
        self.border_position[0] = -1;
        if frame_class {
            for e in 1..=self.num_env {
                self.border_position[e] = r.read_bits(5)? as i32;
                if self.border_position[e] < self.border_position[e - 1] {
                    return Ok(false);
                }
            }
        } else {
            let log2 = [0, 0, 1, 1, 2][self.num_env];
            for e in 1..=self.num_env {
                self.border_position[e] = ((e * self.slots) >> log2) as i32 - 1;
            }
        }
        if self.enable_iid {
            for e in 0..self.num_env {
                let dt = r.read_bool()?;
                let q = self.iid_quant;
                let book = if dt { &bk.iid_dt[q] } else { &bk.iid_df[q] };
                if !self.read_par(r, Par::Iid, book, e, dt)? {
                    return Ok(false);
                }
            }
        } else {
            self.iid_par = [[0; MAX_PAR]; MAX_ENV];
        }
        if self.enable_icc {
            for e in 0..self.num_env {
                let dt = r.read_bool()?;
                let book = if dt { &bk.icc_dt } else { &bk.icc_df };
                if !self.read_par(r, Par::Icc, book, e, dt)? {
                    return Ok(false);
                }
            }
        } else {
            self.icc_par = [[0; MAX_PAR]; MAX_ENV];
        }
        if self.enable_ext {
            let mut cnt = r.read_bits(4)? as i64;
            if cnt == 15 {
                cnt += r.read_bits(8)? as i64;
            }
            cnt *= 8;
            while cnt > 7 {
                let id = r.read_bits(2)?;
                cnt -= 2 + self.read_extension(r, id)? as i64;
            }
            if cnt < 0 {
                return Ok(false);
            }
            r.skip(cnt as usize)?;
        }

        // A last border before the frame end gets a repeated envelope.
        let n = self.num_env;
        if n == 0 || self.border_position[n] < self.slots as i32 - 1 {
            let source = if n > 0 {
                n as i64 - 1
            } else {
                self.num_env_old as i64 - 1
            };
            if source >= 0 && source as usize != n {
                let s = source as usize;
                if self.enable_iid {
                    self.iid_par[n] = self.iid_par[s];
                }
                if self.enable_icc {
                    self.icc_par[n] = self.icc_par[s];
                }
                if self.enable_ipdopd {
                    self.ipd_par[n] = self.ipd_par[s];
                    self.opd_par[n] = self.opd_par[s];
                }
            }
            let limit = 7 + 8 * self.iid_quant as i32;
            if self.enable_iid
                && self.iid_par[n][..self.nr_iid_par]
                    .iter()
                    .any(|&v| (v as i32).abs() > limit)
            {
                return Ok(false);
            }
            // (The reference bounds this check by the IID band count.)
            if self.enable_icc
                && self.icc_par[n][..self.nr_iid_par]
                    .iter()
                    .any(|&v| !(0..=7).contains(&v))
            {
                return Ok(false);
            }
            self.num_env += 1;
            self.border_position[self.num_env] = self.slots as i32 - 1;
        }

        self.is34bands_old = self.is34bands;
        if self.enable_iid || self.enable_icc {
            self.is34bands = (self.enable_iid && self.nr_iid_par == 34)
                || (self.enable_icc && self.nr_icc_par == 34);
        }
        if !self.enable_ipdopd {
            self.ipd_par = [[0; MAX_PAR]; MAX_ENV];
            self.opd_par = [[0; MAX_PAR]; MAX_ENV];
        }
        if header {
            self.start = true;
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Hybrid filterbank (§8.6.4.3).
// ---------------------------------------------------------------------------

/// One complex hybrid filter output for a 13-tap window `x` (symmetric indexing).
#[inline]
fn hybrid_filter(x: &[Cpx], filter: &[Cpx; 8]) -> Cpx {
    let mut re = filter[6][0] * x[6][0];
    let mut im = filter[6][0] * x[6][1];
    for j in 0..6 {
        let inre0 = x[j][0] + x[12 - j][0];
        let inre1 = x[j][1] - x[12 - j][1];
        let inim0 = x[j][1] + x[12 - j][1];
        let inim1 = x[j][0] - x[12 - j][0];
        re += filter[j][0] * inre0 - filter[j][1] * inre1;
        im += filter[j][0] * inim0 + filter[j][1] * inim1;
    }
    [re, im]
}

impl PsState {
    fn hybrid_analysis(&mut self, out: &mut [[Cpx; 32]], l: &[[Cpx; 64]], is34: bool) {
        let t = tables();
        let len = self.slots;
        for i in 0..5 {
            for (j, slot) in l.iter().enumerate().take(len + 6) {
                self.in_buf[i][j + 6] = slot[i];
            }
        }
        if is34 {
            for (band, (filter, base)) in [
                (&t.f34_0_12, 0usize),
                (&t.f34_1_8, 12),
                (&t.f34_2_4, 20),
                (&t.f34_2_4, 24),
                (&t.f34_2_4, 28),
            ]
            .into_iter()
            .enumerate()
            {
                for n in 0..len {
                    let x = &self.in_buf[band][n..n + 13];
                    for (q, f) in filter.iter().enumerate() {
                        out[base + q][n] = hybrid_filter(x, f);
                    }
                }
            }
            for (i, row) in out.iter_mut().enumerate().take(91).skip(32) {
                for n in 0..len {
                    row[n] = l[n][i - 27];
                }
            }
        } else {
            for n in 0..len {
                let x = &self.in_buf[0][n..n + 13];
                let mut tmp = [[0f32; 2]; 8];
                for (q, f) in t.f20_0_8.iter().enumerate() {
                    tmp[q] = hybrid_filter(x, f);
                }
                out[0][n] = tmp[6];
                out[1][n] = tmp[7];
                out[2][n] = tmp[0];
                out[3][n] = tmp[1];
                out[4][n] = [tmp[2][0] + tmp[5][0], tmp[2][1] + tmp[5][1]];
                out[5][n] = [tmp[3][0] + tmp[4][0], tmp[3][1] + tmp[4][1]];
            }
            for (band, base, reverse) in [(1usize, 6usize, true), (2, 8, false)] {
                for n in 0..len {
                    let x = &self.in_buf[band][n..n + 13];
                    let re_in = G1_Q2[6] * x[6][0];
                    let im_in = G1_Q2[6] * x[6][1];
                    let (mut re_op, mut im_op) = (0f32, 0f32);
                    for j in (0..6).step_by(2) {
                        re_op += G1_Q2[j + 1] * (x[j + 1][0] + x[12 - j - 1][0]);
                        im_op += G1_Q2[j + 1] * (x[j + 1][1] + x[12 - j - 1][1]);
                    }
                    let (a, b) = if reverse {
                        (base + 1, base)
                    } else {
                        (base, base + 1)
                    };
                    out[a][n] = [re_in + re_op, im_in + im_op];
                    out[b][n] = [re_in - re_op, im_in - im_op];
                }
            }
            for (i, row) in out.iter_mut().enumerate().take(71).skip(10) {
                for n in 0..len {
                    row[n] = l[n][i - 7];
                }
            }
        }
        for row in self.in_buf.iter_mut() {
            row.copy_within(len..len + 6, 0);
        }
    }
}

fn hybrid_synthesis(out: &mut [[Cpx; 64]], inp: &[[Cpx; 32]], is34: bool, len: usize) {
    let add = |a: Cpx, b: Cpx| [a[0] + b[0], a[1] + b[1]];
    for n in 0..len {
        let o = &mut out[n];
        if is34 {
            let sum = |range: std::ops::Range<usize>| {
                let mut s = [0f32; 2];
                for i in range {
                    s = add(s, inp[i][n]);
                }
                s
            };
            o[0] = sum(0..12);
            o[1] = sum(12..20);
            o[2] = sum(20..24);
            o[3] = sum(24..28);
            o[4] = sum(28..32);
            for i in 5..64 {
                o[i] = inp[i + 27][n];
            }
        } else {
            let mut s = inp[0][n];
            for i in 1..6 {
                s = add(s, inp[i][n]);
            }
            o[0] = s;
            o[1] = add(inp[6][n], inp[7][n]);
            o[2] = add(inp[8][n], inp[9][n]);
            for i in 3..64 {
                o[i] = inp[i + 7][n];
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Decorrelation (§8.6.4.5).
// ---------------------------------------------------------------------------

impl PsState {
    fn decorrelation(&mut self, out: &mut [[Cpx; 32]], s: &[[Cpx; 32]], is34: bool) {
        const TRANSIENT_IMPACT: f32 = 1.5;
        const A_SMOOTH: f32 = 0.25;
        const PEAK_DECAY: f32 = 0.765_928_3;
        const AP_A: [f32; 3] = [0.651_439_04, 0.564_718_1, 0.489_541_66];
        let t = tables();
        let len = self.slots;
        let i34 = is34 as usize;
        let k_to_i: &[u8] = if is34 { &K_TO_I_34 } else { &K_TO_I_20 };
        if is34 != self.is34bands_old {
            self.peak_decay_nrg = [0.0; 34];
            self.power_smooth = [0.0; 34];
            self.peak_decay_diff_smooth = [0.0; 34];
            self.delay
                .iter_mut()
                .for_each(|d| *d = [[0.0; 2]; QMF_SLOTS + MAX_DELAY]);
            self.ap_delay
                .iter_mut()
                .for_each(|d| *d = [[[0.0; 2]; QMF_SLOTS + MAX_AP_DELAY]; AP_LINKS]);
        }
        let mut power = [[0f32; 32]; 34];
        for k in 0..NR_BANDS[i34] {
            let p = &mut power[k_to_i[k] as usize];
            for n in 0..len {
                p[n] += s[k][n][0] * s[k][n][0] + s[k][n][1] * s[k][n][1];
            }
        }
        // Persistent, not a zeroed stack array per call: every cell read below
        // (rows < NR_PAR_BANDS, slots < len) is written first.
        let mut gain = std::mem::take(&mut self.gain);
        for i in 0..NR_PAR_BANDS[i34] {
            for n in 0..len {
                let decayed = PEAK_DECAY * self.peak_decay_nrg[i];
                self.peak_decay_nrg[i] = decayed.max(power[i][n]);
                self.power_smooth[i] += A_SMOOTH * (power[i][n] - self.power_smooth[i]);
                self.peak_decay_diff_smooth[i] += A_SMOOTH
                    * (self.peak_decay_nrg[i] - power[i][n] - self.peak_decay_diff_smooth[i]);
                let denom = TRANSIENT_IMPACT * self.peak_decay_diff_smooth[i];
                gain[i][n] = if denom > self.power_smooth[i] {
                    self.power_smooth[i] / denom
                } else {
                    1.0
                };
            }
        }
        for k in 0..NR_BANDS[i34] {
            let d = &mut self.delay[k];
            d.copy_within(len..len + MAX_DELAY, 0);
            d[MAX_DELAY..MAX_DELAY + len].copy_from_slice(&s[k][..len]);
        }
        for k in 0..NR_ALLPASS_BANDS[i34] {
            let g = &gain[k_to_i[k] as usize];
            let slope = (1.0f32 - 0.05 * (k as i32 - DECAY_CUTOFF[i34]) as f32).clamp(0.0, 1.0);
            let ap = &mut self.ap_delay[k];
            for link in ap.iter_mut() {
                link.copy_within(len..len + MAX_AP_DELAY, 0);
            }
            let ag = [AP_A[0] * slope, AP_A[1] * slope, AP_A[2] * slope];
            let phi = t.phi_fract[i34][k];
            let q = &t.q_fract_allpass[i34][k];
            let dl = &self.delay[k];
            for n in 0..len {
                let x = dl[MAX_DELAY - 2 + n];
                let mut in_re = x[0] * phi[0] - x[1] * phi[1];
                let mut in_im = x[0] * phi[1] + x[1] * phi[0];
                for m in 0..AP_LINKS {
                    let a_re = ag[m] * in_re;
                    let a_im = ag[m] * in_im;
                    let link = ap[m][n + 2 - m];
                    let (apd_re, apd_im) = (in_re, in_im);
                    in_re = link[0] * q[m][0] - link[1] * q[m][1];
                    in_re -= a_re;
                    in_im = link[0] * q[m][1] + link[1] * q[m][0];
                    in_im -= a_im;
                    ap[m][n + 5] = [apd_re + ag[m] * in_re, apd_im + ag[m] * in_im];
                }
                out[k][n] = [g[n] * in_re, g[n] * in_im];
            }
        }
        for k in NR_ALLPASS_BANDS[i34]..NR_BANDS[i34] {
            let g = &gain[k_to_i[k] as usize];
            // Fixed delay: 14 slots below the short-delay band, 1 above it.
            let lag = if k < SHORT_DELAY_BAND[i34] { 14 } else { 1 };
            for n in 0..len {
                let x = self.delay[k][MAX_DELAY - lag + n];
                out[k][n] = [x[0] * g[n], x[1] * g[n]];
            }
        }
        self.gain = gain;
    }
}

// ---------------------------------------------------------------------------
// Parameter mapping (Tables 8.46/8.47) and stereo processing (§8.6.4.6).
// ---------------------------------------------------------------------------

fn map_idx_10_to_20(out: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    let top = if full {
        9
    } else {
        out[10] = 0;
        4
    };
    for b in (0..=top).rev() {
        out[2 * b] = par[b];
        out[2 * b + 1] = par[b];
    }
}

fn map_idx_34_to_20(out: &mut [i8; MAX_PAR], p: &[i8; MAX_PAR], full: bool) {
    let p = p.map(|v| v as i32);
    let o = |v: i32| v as i8;
    out[0] = o((2 * p[0] + p[1]) / 3);
    out[1] = o((p[1] + 2 * p[2]) / 3);
    out[2] = o((2 * p[3] + p[4]) / 3);
    out[3] = o((p[4] + 2 * p[5]) / 3);
    out[4] = o((p[6] + p[7]) / 2);
    out[5] = o((p[8] + p[9]) / 2);
    out[6] = o(p[10]);
    out[7] = o(p[11]);
    out[8] = o((p[12] + p[13]) / 2);
    out[9] = o((p[14] + p[15]) / 2);
    out[10] = o(p[16]);
    if full {
        out[11] = o(p[17]);
        out[12] = o(p[18]);
        out[13] = o(p[19]);
        out[14] = o((p[20] + p[21]) / 2);
        out[15] = o((p[22] + p[23]) / 2);
        out[16] = o((p[24] + p[25]) / 2);
        out[17] = o((p[26] + p[27]) / 2);
        out[18] = o((p[28] + p[29] + p[30] + p[31]) / 4);
        out[19] = o((p[32] + p[33]) / 2);
    }
}

fn map_idx_10_to_34(out: &mut [i8; MAX_PAR], p: &[i8; MAX_PAR], full: bool) {
    if full {
        out[28..34].fill(p[9]);
        out[24..28].fill(p[8]);
        out[20..24].fill(p[7]);
        out[18..20].fill(p[6]);
        out[16..18].fill(p[5]);
    } else {
        out[16] = 0;
    }
    out[12..16].fill(p[4]);
    out[10..12].fill(p[3]);
    out[6..10].fill(p[2]);
    out[3..6].fill(p[1]);
    out[0..3].fill(p[0]);
}

fn map_idx_20_to_34(out: &mut [i8; MAX_PAR], p: &[i8; MAX_PAR], full: bool) {
    if full {
        out[32..34].fill(p[19]);
        out[28..32].fill(p[18]);
        out[26..28].fill(p[17]);
        out[24..26].fill(p[16]);
        out[22..24].fill(p[15]);
        out[20..22].fill(p[14]);
        out[19] = p[13];
        out[18] = p[12];
        out[17] = p[11];
    }
    out[16] = p[10];
    out[15] = p[9];
    out[14] = p[9];
    out[13] = p[8];
    out[12] = p[8];
    out[11] = p[7];
    out[10] = p[6];
    out[9] = p[5];
    out[8] = p[5];
    out[7] = p[4];
    out[6] = p[4];
    out[5] = p[3];
    out[4] = ((p[2] as i32 + p[3] as i32) / 2) as i8;
    out[3] = p[2];
    out[2] = p[1];
    out[1] = ((p[0] as i32 + p[1] as i32) / 2) as i8;
    out[0] = p[0];
}

fn map_val_34_to_20(par: &mut [f32; MAX_PAR]) {
    let h = |a: f32, b: f32| (a + b) * 0.5;
    par[0] = (2.0 * par[0] + par[1]) * 0.333_333_33;
    par[1] = (par[1] + 2.0 * par[2]) * 0.333_333_33;
    par[2] = (2.0 * par[3] + par[4]) * 0.333_333_33;
    par[3] = (par[4] + 2.0 * par[5]) * 0.333_333_33;
    par[4] = h(par[6], par[7]);
    par[5] = h(par[8], par[9]);
    par[6] = par[10];
    par[7] = par[11];
    par[8] = h(par[12], par[13]);
    par[9] = h(par[14], par[15]);
    par[10] = par[16];
    par[11] = par[17];
    par[12] = par[18];
    par[13] = par[19];
    par[14] = h(par[20], par[21]);
    par[15] = h(par[22], par[23]);
    par[16] = h(par[24], par[25]);
    par[17] = h(par[26], par[27]);
    par[18] = (par[28] + par[29] + par[30] + par[31]) * 0.25;
    par[19] = h(par[32], par[33]);
}

fn map_val_20_to_34(par: &mut [f32; MAX_PAR]) {
    let h = |a: f32, b: f32| (a + b) * 0.5;
    par[33] = par[19];
    par[32] = par[19];
    par[31] = par[18];
    par[30] = par[18];
    par[29] = par[18];
    par[28] = par[18];
    par[27] = par[17];
    par[26] = par[17];
    par[25] = par[16];
    par[24] = par[16];
    par[23] = par[15];
    par[22] = par[15];
    par[21] = par[14];
    par[20] = par[14];
    par[19] = par[13];
    par[18] = par[12];
    par[17] = par[11];
    par[16] = par[10];
    par[15] = par[9];
    par[14] = par[9];
    par[13] = par[8];
    par[12] = par[8];
    par[11] = par[7];
    par[10] = par[6];
    par[9] = par[5];
    par[8] = par[5];
    par[7] = par[4];
    par[6] = par[4];
    par[5] = par[3];
    par[4] = h(par[2], par[3]);
    par[3] = par[2];
    par[2] = par[1];
    par[1] = h(par[0], par[1]);
}

type ParRows = [[i8; MAX_PAR]; MAX_ENV];

/// Map a parameter set onto the processing band resolution; `full` maps every
/// band (IID/ICC) rather than only the IPD/OPD range.
fn remap(par: &ParRows, num_par: usize, num_env: usize, full: bool, is34: bool) -> ParRows {
    let mut out = *par;
    for e in 0..num_env {
        match (is34, num_par) {
            (true, 20 | 11) => map_idx_20_to_34(&mut out[e], &par[e], full),
            (true, 10 | 5) => map_idx_10_to_34(&mut out[e], &par[e], full),
            (false, 34 | 17) => map_idx_34_to_20(&mut out[e], &par[e], full),
            (false, 10 | 5) => map_idx_10_to_20(&mut out[e], &par[e], full),
            _ => {}
        }
    }
    out
}

impl PsState {
    fn stereo_processing(&mut self, l: &mut [[Cpx; 32]], r: &mut [[Cpx; 32]], is34: bool) {
        let t = tables();
        let i34 = is34 as usize;
        let k_to_i: &[u8] = if is34 { &K_TO_I_34 } else { &K_TO_I_20 };
        let lut = if self.icc_mode < 3 { &t.ha } else { &t.hb };
        let ne_old = self.num_env_old;
        if ne_old != 0 {
            for h in self.h.iter_mut() {
                for part in h.iter_mut() {
                    part[0] = part[ne_old];
                }
            }
        }
        let iid = remap(&self.iid_par, self.nr_iid_par, self.num_env, true, is34);
        let icc = remap(&self.icc_par, self.nr_icc_par, self.num_env, true, is34);
        let (ipd, opd) = if self.enable_ipdopd {
            (
                remap(&self.ipd_par, self.nr_ipdopd_par, self.num_env, false, is34),
                remap(&self.opd_par, self.nr_ipdopd_par, self.num_env, false, is34),
            )
        } else {
            (self.ipd_par, self.opd_par)
        };
        if is34 != self.is34bands_old {
            for h in self.h.iter_mut() {
                for part in h.iter_mut() {
                    if is34 {
                        map_val_20_to_34(&mut part[0]);
                    } else {
                        map_val_34_to_20(&mut part[0]);
                    }
                }
            }
            self.ipd_hist = [0; MAX_PAR];
            self.opd_hist = [0; MAX_PAR];
        }

        let ipdopd = self.enable_ipdopd;
        for e in 0..self.num_env {
            for b in 0..NR_PAR_BANDS[i34] {
                let row = (iid[e][b] as i32 + 7 + 23 * self.iid_quant as i32) as usize;
                let mut hh = lut[row][icc[e][b] as usize];
                if ipdopd && b < NR_IPDOPD_BANDS[i34] {
                    // Phase parameters are smoothed over the last three values.
                    let opd_idx = self.opd_hist[b] as usize * 8 + opd[e][b] as usize;
                    let ipd_idx = self.ipd_hist[b] as usize * 8 + ipd[e][b] as usize;
                    let (opd_re, opd_im) = (t.pd_re_smooth[opd_idx], t.pd_im_smooth[opd_idx]);
                    let (ipd_re, ipd_im) = (t.pd_re_smooth[ipd_idx], t.pd_im_smooth[ipd_idx]);
                    self.opd_hist[b] = (opd_idx & 0x3f) as i8;
                    self.ipd_hist[b] = (ipd_idx & 0x3f) as i8;
                    let adj_re = opd_re * ipd_re + opd_im * ipd_im;
                    let adj_im = opd_im * ipd_re - opd_re * ipd_im;
                    let im = [
                        hh[0] * opd_im,
                        hh[1] * adj_im,
                        hh[2] * opd_im,
                        hh[3] * adj_im,
                    ];
                    hh = [
                        hh[0] * opd_re,
                        hh[1] * adj_re,
                        hh[2] * opd_re,
                        hh[3] * adj_re,
                    ];
                    for (j, v) in im.into_iter().enumerate() {
                        self.h[j][1][e + 1][b] = v;
                    }
                }
                for (j, v) in hh.into_iter().enumerate() {
                    self.h[j][0][e + 1][b] = v;
                }
            }
            let start = self.border_position[e];
            let stop = self.border_position[e + 1];
            let len = (stop - start).max(0) as usize;
            let width = 1.0f32
                / if stop - start != 0 {
                    (stop - start) as f32
                } else {
                    1.0
                };
            for k in 0..NR_BANDS[i34] {
                let b = k_to_i[k] as usize;
                let mut h0 = [
                    self.h[0][0][e][b],
                    self.h[1][0][e][b],
                    self.h[2][0][e][b],
                    self.h[3][0][e][b],
                ];
                let mut h1 = [0f32; 4];
                let step0: [f32; 4] =
                    std::array::from_fn(|j| (self.h[j][0][e + 1][b] - h0[j]) * width);
                let mut step1 = [0f32; 4];
                if ipdopd {
                    let neg = (is34 && (9..=13).contains(&k)) || (!is34 && k <= 1);
                    h1 = std::array::from_fn(|j| {
                        if neg {
                            -self.h[j][1][e][b]
                        } else {
                            self.h[j][1][e][b]
                        }
                    });
                    step1 = std::array::from_fn(|j| (self.h[j][1][e + 1][b] - h1[j]) * width);
                }
                if len == 0 {
                    continue;
                }
                let s0 = (start + 1) as usize;
                for n in s0..s0 + len {
                    let (lr, li) = (l[k][n][0], l[k][n][1]);
                    let (rr, ri) = (r[k][n][0], r[k][n][1]);
                    for j in 0..4 {
                        h0[j] += step0[j];
                    }
                    if ipdopd {
                        for j in 0..4 {
                            h1[j] += step1[j];
                        }
                        l[k][n] = [
                            h0[0] * lr + h0[2] * rr - h1[0] * li - h1[2] * ri,
                            h0[0] * li + h0[2] * ri + h1[0] * lr + h1[2] * rr,
                        ];
                        r[k][n] = [
                            h0[1] * lr + h0[3] * rr - h1[1] * li - h1[3] * ri,
                            h0[1] * li + h0[3] * ri + h1[1] * lr + h1[3] * rr,
                        ];
                    } else {
                        l[k][n] = [h0[0] * lr + h0[2] * rr, h0[0] * li + h0[2] * ri];
                        r[k][n] = [h0[1] * lr + h0[3] * rr, h0[1] * li + h0[3] * ri];
                    }
                }
            }
        }
    }

    /// Turn the mono QMF matrix `l` (38 slots x 64 bands) into stereo `l`/`r`.
    /// `top` is the highest QMF band carrying signal.
    pub(crate) fn apply(&mut self, l: &mut [[Cpx; 64]], r: &mut [[Cpx; 64]], top: usize) {
        let is34 = self.is34bands;
        let i34 = is34 as usize;
        let top = (top + NR_BANDS[i34]).saturating_sub(64);
        for d in self.delay.iter_mut().take(NR_BANDS[i34]).skip(top) {
            *d = [[0.0; 2]; QMF_SLOTS + MAX_DELAY];
        }
        for d in self
            .ap_delay
            .iter_mut()
            .take(NR_ALLPASS_BANDS[i34])
            .skip(top)
        {
            *d = [[[0.0; 2]; QMF_SLOTS + MAX_AP_DELAY]; AP_LINKS];
        }
        // The matrices persist instead of two zeroed 23 KB `vec!`s per frame:
        // hybrid analysis writes every row < NR_BANDS and slot < `slots` of
        // `lbuf`, decorrelation the same of `rbuf`, and every later read is of
        // those cells. Stereo processing addresses slots by envelope border,
        // so the slots no producer writes are re-zeroed - exactly what a fresh
        // buffer held.
        let (mut lbuf, mut rbuf) = (
            std::mem::take(&mut self.lbuf),
            std::mem::take(&mut self.rbuf),
        );
        if self.slots < 32 {
            for row in lbuf.iter_mut().chain(rbuf.iter_mut()) {
                row[self.slots..].fill([0.0; 2]);
            }
        }
        self.hybrid_analysis(&mut lbuf, l, is34);
        self.decorrelation(&mut rbuf, &lbuf, is34);
        self.stereo_processing(&mut lbuf, &mut rbuf, is34);
        hybrid_synthesis(l, &lbuf, is34, self.slots);
        hybrid_synthesis(r, &rbuf, is34, self.slots);
        (self.lbuf, self.rbuf) = (lbuf, rbuf);
    }
}
