//! Spectral tools applied between dequantisation and synthesis: M/S and
//! intensity stereo, TNS (synthesis filter, and the analysis form LTP needs),
//! AAC-Main backward-adaptive prediction, and channel coupling.

use super::channel::{ChannelData, Ics, Tns};
use crate::codebook::{INTENSITY_HCB, INTENSITY_HCB2, NOISE_HCB};
use crate::ics::WindowSequence;
use crate::tables_ext::AAC_PRED_SFB_MAX;

/// M/S (§4.6.8.1). Bands where either channel is noise or intensity are left
/// alone: M/S on a PNS band signals noise correlation, not a rotation.
pub fn apply_ms(ics: &Ics, ms_mask: &[bool], c0: &mut ChannelData, c1: &mut ChannelData) {
    let _prof = crate::prof::scope(crate::prof::Stage::DecTools);
    let info = &ics.info;
    let max_sfb = info.max_sfb as usize;
    let mut wbase = 0usize;
    for g in 0..info.num_window_groups {
        for sfb in 0..max_sfb {
            let idx = g * max_sfb + sfb;
            if ms_mask[idx] && c0.band_type[idx] < NOISE_HCB && c1.band_type[idx] < NOISE_HCB {
                let (s, e) = (ics.swb[sfb] as usize, ics.swb[sfb + 1] as usize);
                for w in 0..info.window_group_length[g] as usize {
                    let base = (wbase + w) * 128;
                    for i in base + s..base + e {
                        let (m, sd) = (c0.coeffs[i], c1.coeffs[i]);
                        c0.coeffs[i] = m + sd;
                        c1.coeffs[i] = m - sd;
                    }
                }
            }
        }
        wbase += info.window_group_length[g] as usize;
    }
}

/// Intensity stereo (§4.6.8.2.3): `R = ±2^(-pos/4)·L` on the right channel's
/// intensity bands; the sign flips for INTENSITY_HCB2 and again under M/S.
pub fn apply_is(
    ics1: &Ics,
    ms_present: bool,
    ms_mask: &[bool],
    c0: &ChannelData,
    c1: &mut ChannelData,
) {
    let _prof = crate::prof::scope(crate::prof::Stage::DecTools);
    let info = &ics1.info;
    let max_sfb = info.max_sfb as usize;
    let mut wbase = 0usize;
    for g in 0..info.num_window_groups {
        for sfb in 0..max_sfb {
            let idx = g * max_sfb + sfb;
            let bt = c1.band_type[idx];
            if bt == INTENSITY_HCB || bt == INTENSITY_HCB2 {
                let mut c: f32 = if bt == INTENSITY_HCB { 1.0 } else { -1.0 };
                if ms_present && ms_mask.get(idx).copied().unwrap_or(false) {
                    c = -c;
                }
                let scale = c * 2f32.powf(-0.25 * c1.sfo[idx] as f32);
                let (s, e) = (ics1.swb[sfb] as usize, ics1.swb[sfb + 1] as usize);
                for w in 0..info.window_group_length[g] as usize {
                    let base = (wbase + w) * 128;
                    for i in base + s..base + e {
                        c1.coeffs[i] = scale * c0.coeffs[i];
                    }
                }
            }
        }
        wbase += info.window_group_length[g] as usize;
    }
}

/// TNS (§4.6.9.3). `decode = true` runs the all-pole synthesis filter; `false`
/// the moving-average analysis filter LTP applies to its prediction.
pub fn apply_tns(coef: &mut [f32], tns: &Tns, ics: &Ics, decode: bool) {
    let _prof = crate::prof::scope(crate::prof::Stage::DecTools);
    let info = &ics.info;
    let mmm = (ics.tns_max_bands as usize).min(info.max_sfb as usize);
    if mmm == 0 {
        return;
    }
    for (w, filters) in tns.windows.iter().enumerate() {
        let mut bottom = info.num_swb;
        for f in filters {
            let top = bottom;
            bottom = top.saturating_sub(f.length);
            let order = f.order;
            if order == 0 {
                continue;
            }
            let start = ics.swb[bottom.min(mmm)] as usize;
            let end = ics.swb[top.min(mmm)] as usize;
            if end <= start {
                continue;
            }
            let size = end - start;
            let (mut p, inc): (isize, isize) = if f.direction {
                ((end - 1) as isize, -1)
            } else {
                (start as isize, 1)
            };
            p += (w * 128) as isize;
            if decode {
                for m in 0..size {
                    let mut y = coef[p as usize];
                    for i in 1..=m.min(order) {
                        y -= coef[(p - i as isize * inc) as usize] * f.lpc[i];
                    }
                    coef[p as usize] = y;
                    p += inc;
                }
            } else {
                let mut tmp = [0f32; 21];
                for m in 0..size {
                    tmp[0] = coef[p as usize];
                    let mut y = coef[p as usize];
                    for i in 1..=m.min(order) {
                        y += tmp[i] * f.lpc[i];
                    }
                    coef[p as usize] = y;
                    for i in (1..=order).rev() {
                        tmp[i] = tmp[i - 1];
                    }
                    p += inc;
                }
            }
        }
    }
}

/// One backward-adaptive lattice predictor (§4.6.7), with the reference's
/// 16-bit-mantissa float rounding so prediction is bit-reproducible.
#[derive(Clone, Copy)]
pub struct PredState {
    r0: f32,
    r1: f32,
    cor0: f32,
    cor1: f32,
    var0: f32,
    var1: f32,
}

impl Default for PredState {
    fn default() -> PredState {
        PredState {
            r0: 0.0,
            r1: 0.0,
            cor0: 0.0,
            cor1: 0.0,
            var0: 1.0,
            var1: 1.0,
        }
    }
}

#[inline]
fn flt16_round(x: f32) -> f32 {
    f32::from_bits((x.to_bits().wrapping_add(0x0000_8000)) & 0xFFFF_0000)
}
#[inline]
fn flt16_even(x: f32) -> f32 {
    let i = x.to_bits();
    f32::from_bits(i.wrapping_add(0x0000_7FFF).wrapping_add(i & 1) & 0xFFFF_0000)
}
#[inline]
fn flt16_trunc(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0xFFFF_0000)
}

#[inline]
fn predict(ps: &mut PredState, coef: &mut f32, output_enable: bool) {
    let a = 0.953125f32;
    let alpha = 0.90625f32;
    let (r0, r1, cor0, cor1, var0, var1) = (ps.r0, ps.r1, ps.cor0, ps.cor1, ps.var0, ps.var1);
    let k1 = if var0 > 1.0 {
        cor0 * flt16_even(a / var0)
    } else {
        0.0
    };
    let k2 = if var1 > 1.0 {
        cor1 * flt16_even(a / var1)
    } else {
        0.0
    };
    let pv = flt16_round(k1 * r0 + k2 * r1);
    if output_enable {
        *coef += pv;
    }
    let e0 = *coef;
    let e1 = e0 - k1 * r0;
    ps.cor1 = flt16_trunc(alpha * cor1 + r1 * e1);
    ps.var1 = flt16_trunc(alpha * var1 + 0.5 * (r1 * r1 + e1 * e1));
    ps.cor0 = flt16_trunc(alpha * cor0 + r0 * e0);
    ps.var0 = flt16_trunc(alpha * var0 + 0.5 * (r0 * r0 + e0 * e0));
    ps.r1 = flt16_trunc(a * (r0 - k1 * e0));
    ps.r0 = flt16_trunc(a * e0);
}

/// AAC-Main frequency-domain prediction for one channel.
pub fn apply_prediction(
    ics: &Ics,
    sf_index: u8,
    coeffs: &mut [f32],
    ps: &mut [PredState],
    initialized: &mut bool,
) {
    let _prof = crate::prof::scope(crate::prof::Stage::DecTools);
    if !*initialized {
        ps.iter_mut().for_each(|p| *p = PredState::default());
        *initialized = true;
    }
    if ics.info.window_sequence != WindowSequence::EightShort {
        let n = AAC_PRED_SFB_MAX[sf_index as usize] as usize;
        for sfb in 0..n.min(ics.swb.len() - 1) {
            let used = ics.predictor_present && ics.prediction_used[sfb];
            for k in ics.swb[sfb] as usize..ics.swb[sfb + 1] as usize {
                if k < ps.len() {
                    predict(&mut ps[k], &mut coeffs[k], used);
                }
            }
        }
        if ics.predictor_reset_group > 0 {
            let mut i = ics.predictor_reset_group as usize - 1;
            while i < ps.len() {
                ps[i] = PredState::default();
                i += 30;
            }
        }
    } else {
        ps.iter_mut().for_each(|p| *p = PredState::default());
    }
}

/// `coupling_channel_element` side info (§4.4.2.5).
#[derive(Clone, Default)]
pub struct Coupling {
    /// [`BEFORE_TNS`], [`BETWEEN_TNS_AND_IMDCT`] or [`AFTER_IMDCT`] (independent).
    pub point: u8,
    /// (element type, element tag, ch_select), one per coupled element.
    pub targets: Vec<(u8, u8, u8)>,
    /// gains[index][band] (band 0 only for independent coupling).
    pub gains: Vec<Vec<f32>>,
}

pub const BEFORE_TNS: u8 = 0;
pub const BETWEEN_TNS_AND_IMDCT: u8 = 1;
pub const AFTER_IMDCT: u8 = 3;

/// Dependent coupling: add the CCE's scaled spectrum into a target channel.
pub fn apply_dependent_coupling(
    cce_ics: &Ics,
    cce: &ChannelData,
    gains: &[f32],
    target: &mut [f32],
) {
    let info = &cce_ics.info;
    let max_sfb = info.max_sfb as usize;
    let mut wbase = 0usize;
    for g in 0..info.num_window_groups {
        for sfb in 0..max_sfb {
            let idx = g * max_sfb + sfb;
            if cce.band_type[idx] != 0 {
                let gain = gains[idx];
                let (s, e) = (cce_ics.swb[sfb] as usize, cce_ics.swb[sfb + 1] as usize);
                for w in 0..info.window_group_length[g] as usize {
                    let base = (wbase + w) * 128;
                    for k in base + s..base + e {
                        target[k] += gain * cce.coeffs[k];
                    }
                }
            }
        }
        wbase += info.window_group_length[g] as usize;
    }
}
