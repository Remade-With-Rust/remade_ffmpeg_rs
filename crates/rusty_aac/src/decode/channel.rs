//! One `individual_channel_stream` (ISO 14496-3 §4.4.2.7): ics_info (with Main
//! prediction / LTP side info and SSR gain control), section data,
//! scalefactors, pulse data, TNS, spectral data — and its dequantisation,
//! including PNS noise from the reference decoder's generator so noise bands are
//! sample-identical across decoders rather than merely energy-equal.

use crate::bits::BitReader;
use crate::codebook::{CODEBOOKS, INTENSITY_HCB, INTENSITY_HCB2, NOISE_HCB, ZERO_HCB};
use crate::config::aot;
use crate::ics::{IcsInfo, WindowSequence};
use crate::swb::swb_offsets;
use crate::tables::{spectral_book, SCALEFACTOR_BOOK};
use crate::tables_ext::{
    AAC_PRED_SFB_MAX, SWB_OFFSET_120, SWB_OFFSET_480, SWB_OFFSET_512, SWB_OFFSET_960,
    TNS_MAX_BANDS_480, TNS_MAX_BANDS_512,
};
use crate::{Error, Result};

/// Max TNS-affected band per sampling-frequency index (ISO Table 4.A.45/46).
pub(crate) const TNS_MAX_LONG: [u8; 13] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39, 39];
pub(crate) const TNS_MAX_SHORT: [u8; 13] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14];

pub const MAX_LTP_LONG_SFB: usize = 40;
pub const MAX_PREDICTORS: usize = 672;

/// What the stream is, as far as one channel's syntax is concerned.
#[derive(Clone, Copy)]
pub struct Syntax {
    pub aot: u8,
    pub sf_index: u8,
    /// 1024 / 960 / 512 / 480.
    pub frame_len: usize,
}

impl Syntax {
    pub fn eld(&self) -> bool {
        self.aot == aot::ER_AAC_ELD
    }
    pub fn er(&self) -> bool {
        matches!(
            self.aot,
            aot::ER_AAC_LC | aot::ER_AAC_LTP | aot::ER_AAC_LD | aot::ER_AAC_ELD
        )
    }
    pub fn low_delay(&self) -> bool {
        matches!(self.aot, aot::ER_AAC_LD | aot::ER_AAC_ELD)
    }
    fn long_swb(&self) -> Result<&'static [u16]> {
        let i = self.sf_index as usize;
        let t = match (self.low_delay(), self.frame_len) {
            (true, 480) => SWB_OFFSET_480[i],
            (true, _) => SWB_OFFSET_512[i],
            (false, 960) => SWB_OFFSET_960[i],
            (false, _) => Some(swb_offsets(true, self.sf_index)),
        };
        t.ok_or_else(|| {
            Error::unsupported("aac: no scalefactor-band table for this low-delay rate")
        })
    }
    fn short_swb(&self) -> &'static [u16] {
        if self.frame_len == 960 {
            SWB_OFFSET_120[self.sf_index as usize].unwrap_or(swb_offsets(false, self.sf_index))
        } else {
            swb_offsets(false, self.sf_index)
        }
    }
}

#[derive(Clone, Copy)]
pub struct Ltp {
    pub present: bool,
    pub lag: usize,
    pub coef: f32,
    pub used: [bool; MAX_LTP_LONG_SFB],
}

impl Default for Ltp {
    fn default() -> Ltp {
        Ltp {
            present: false,
            lag: 0,
            coef: 0.0,
            used: [false; MAX_LTP_LONG_SFB],
        }
    }
}

/// Persistent + per-frame ICS state (FFmpeg's `IndividualChannelStream`).
#[derive(Clone)]
pub struct Ics {
    pub info: IcsInfo,
    pub prev_seq: WindowSequence,
    pub prev_kbd: bool,
    pub swb: &'static [u16],
    pub tns_max_bands: u8,
    pub predictor_present: bool,
    pub predictor_reset_group: u8,
    pub prediction_used: [bool; 41],
    pub ltp: Ltp,
}

impl Default for Ics {
    fn default() -> Ics {
        Ics {
            info: IcsInfo {
                window_sequence: WindowSequence::OnlyLong,
                window_shape_kbd: false,
                max_sfb: 0,
                num_windows: 1,
                num_window_groups: 1,
                window_group_length: vec![1],
                num_swb: 0,
            },
            prev_seq: WindowSequence::OnlyLong,
            prev_kbd: false,
            swb: &[0],
            tns_max_bands: 0,
            predictor_present: false,
            predictor_reset_group: 0,
            prediction_used: [false; 41],
            ltp: Ltp::default(),
        }
    }
}

fn seq_from_bits(v: u32) -> WindowSequence {
    match v {
        0 => WindowSequence::OnlyLong,
        1 => WindowSequence::LongStart,
        2 => WindowSequence::EightShort,
        _ => WindowSequence::LongStop,
    }
}

pub fn decode_ltp(r: &mut BitReader, ltp: &mut Ltp, max_sfb: usize) -> Result<()> {
    ltp.lag = r.read_bits(11)? as usize;
    ltp.coef = crate::tables_ext::LTP_COEF[r.read_bits(3)? as usize];
    for sfb in 0..max_sfb.min(MAX_LTP_LONG_SFB) {
        ltp.used[sfb] = r.read_bool()?;
    }
    Ok(())
}

/// `ics_info()` — updates the window history exactly as the reference does.
pub fn parse_ics_info(r: &mut BitReader, sx: &Syntax, ics: &mut Ics) -> Result<()> {
    if !sx.eld() {
        let _reserved = r.read_bit()?;
        ics.prev_seq = ics.info.window_sequence;
        ics.info.window_sequence = seq_from_bits(r.read_bits(2)?);
        if sx.aot == aot::ER_AAC_LD && ics.info.window_sequence != WindowSequence::OnlyLong {
            ics.info.window_sequence = WindowSequence::OnlyLong;
            return Err(Error::invalid(
                "aac: AAC-LD is only defined for ONLY_LONG_SEQUENCE",
            ));
        }
        ics.prev_kbd = ics.info.window_shape_kbd;
        ics.info.window_shape_kbd = r.read_bool()?;
    }
    ics.info.num_window_groups = 1;
    ics.info.window_group_length = vec![1];
    ics.predictor_present = false;
    if ics.info.window_sequence == WindowSequence::EightShort {
        ics.info.max_sfb = r.read_bits(4)? as u8;
        let mut groups = vec![1u8];
        for _ in 0..7 {
            if r.read_bool()? {
                *groups.last_mut().unwrap() += 1;
            } else {
                groups.push(1);
            }
        }
        ics.info.num_window_groups = groups.len();
        ics.info.window_group_length = groups;
        ics.info.num_windows = 8;
        ics.swb = sx.short_swb();
        ics.tns_max_bands = TNS_MAX_SHORT[sx.sf_index as usize];
    } else {
        ics.info.max_sfb = r.read_bits(6)? as u8;
        ics.info.num_windows = 1;
        ics.swb = sx.long_swb()?;
        let i = sx.sf_index as usize;
        ics.tns_max_bands = match (sx.low_delay(), sx.frame_len) {
            (true, 480) => TNS_MAX_BANDS_480[i],
            (true, _) => TNS_MAX_BANDS_512[i],
            _ => TNS_MAX_LONG[i],
        };
        if !sx.eld() {
            ics.predictor_present = r.read_bool()?;
            ics.predictor_reset_group = 0;
        }
        if ics.predictor_present {
            if sx.aot == aot::AAC_MAIN {
                if r.read_bool()? {
                    ics.predictor_reset_group = r.read_bits(5)? as u8;
                    if ics.predictor_reset_group == 0 || ics.predictor_reset_group > 30 {
                        return Err(Error::invalid("aac: invalid predictor reset group"));
                    }
                }
                let n = (ics.info.max_sfb as usize).min(AAC_PRED_SFB_MAX[i] as usize);
                for sfb in 0..n {
                    ics.prediction_used[sfb] = r.read_bool()?;
                }
            } else if sx.aot == aot::AAC_LC || sx.aot == aot::ER_AAC_LC {
                return Err(Error::invalid("aac: prediction is not allowed in AAC-LC"));
            } else {
                if sx.aot == aot::ER_AAC_LD {
                    return Err(Error::unsupported("aac: LTP in ER AAC-LD is not supported"));
                }
                ics.ltp.present = r.read_bool()?;
                if ics.ltp.present {
                    let m = ics.info.max_sfb as usize;
                    decode_ltp(r, &mut ics.ltp, m)?;
                }
            }
        }
    }
    ics.info.num_swb = ics.swb.len() - 1;
    if ics.info.max_sfb as usize > ics.info.num_swb {
        ics.info.max_sfb = 0;
        return Err(Error::invalid(
            "aac: max_sfb exceeds the number of scalefactor bands",
        ));
    }
    Ok(())
}

/// Dequantised TNS reflection coefficients `sin(c / iqfac)` (ISO 14496-3
/// §4.6.9.3), indexed by the raw two's-complement code, for
/// [coef_compress·2 + coef_res]: 3-bit, 4-bit, compressed 3-bit, compressed
/// 4-bit. These are the 8-digit values the reference decoders tabulate (not
/// correctly-rounded sines): matching them keeps the recursive TNS filter
/// sample-exact.
const TNS_PARCOR: [&[f32]; 4] = [
    &[
        0.0,
        0.43388373,
        0.78183150,
        0.97492790,
        -0.98480773,
        -0.86602539,
        -0.64278758,
        -0.34202015,
    ],
    &[
        0.0,
        0.20791170,
        0.40673664,
        0.58778524,
        0.74314481,
        0.86602539,
        0.95105654,
        0.99452192,
        -0.99573416,
        -0.96182561,
        -0.89516330,
        -0.79801720,
        -0.67369562,
        -0.52643216,
        -0.36124167,
        -0.18374951,
    ],
    &[0.0, 0.43388373, -0.64278758, -0.34202015],
    &[
        0.0,
        0.20791170,
        0.40673664,
        0.58778524,
        -0.67369562,
        -0.52643216,
        -0.36124167,
        -0.18374951,
    ],
];

/// One TNS filter: band span, order, direction, LPC (lpc[0] = 1).
#[derive(Clone, Default)]
pub struct TnsFilter {
    pub length: usize,
    pub order: usize,
    pub direction: bool,
    pub lpc: Vec<f32>,
}

#[derive(Clone, Default)]
pub struct Tns {
    pub present: bool,
    pub windows: Vec<Vec<TnsFilter>>,
}

fn parcor_to_lpc(parcor: &[f32]) -> Vec<f32> {
    let order = parcor.len();
    let mut lpc = vec![0f32; order + 1];
    lpc[0] = 1.0;
    for m in 1..=order {
        let mut tmp = lpc.clone();
        for i in 1..m {
            tmp[i] = lpc[i] + parcor[m - 1] * lpc[m - i];
        }
        lpc[..m].copy_from_slice(&tmp[..m]);
        lpc[m] = parcor[m - 1];
    }
    lpc
}

pub fn parse_tns(r: &mut BitReader, sx: &Syntax, info: &IcsInfo) -> Result<Tns> {
    let short = info.window_sequence == WindowSequence::EightShort;
    let max_order = if short {
        7
    } else if sx.aot == aot::AAC_MAIN {
        20
    } else {
        12
    };
    let (nf_bits, len_bits, ord_bits) = if short { (1, 4, 3) } else { (2, 6, 5) };
    let mut windows = Vec::with_capacity(info.num_windows);
    for _ in 0..info.num_windows {
        let mut filters = Vec::new();
        let n_filt = r.read_bits(nf_bits)?;
        let coef_res = if n_filt > 0 { r.read_bits(1)? } else { 0 };
        for _ in 0..n_filt {
            let length = r.read_bits(len_bits)? as usize;
            let order = r.read_bits(ord_bits)? as usize;
            if order > max_order {
                return Err(Error::invalid("aac: TNS filter order too large"));
            }
            let (mut direction, mut lpc) = (false, Vec::new());
            if order > 0 {
                direction = r.read_bool()?;
                let coef_compress = r.read_bits(1)?;
                let res_bits = 3 + coef_res;
                let coef_bits = res_bits - coef_compress;
                let table = TNS_PARCOR[2 * coef_compress as usize + coef_res as usize];
                let mut parcor = vec![0f32; order];
                for p in parcor.iter_mut() {
                    *p = table[r.read_bits(coef_bits)? as usize];
                }
                lpc = parcor_to_lpc(&parcor);
            }
            filters.push(TnsFilter {
                length,
                order,
                direction,
                lpc,
            });
        }
        windows.push(filters);
    }
    Ok(Tns {
        present: true,
        windows,
    })
}

/// SSR `gain_control_data` — parsed and discarded (the reference decoders do the
/// same: the SSR filterbank is not implemented anywhere in practice).
fn skip_gain_control(r: &mut BitReader, seq: WindowSequence) -> Result<()> {
    let mode: [(usize, bool, u32); 4] = [(1, false, 5), (2, true, 2), (8, false, 2), (2, true, 5)];
    let (wd_num, wd_test, aloc) = mode[seq as usize];
    let max_band = r.read_bits(2)?;
    for _ in 0..max_band {
        for wd in 0..wd_num {
            let adjust_num = r.read_bits(3)?;
            for _ in 0..adjust_num {
                r.skip(4 + if wd == 0 && wd_test { 4 } else { aloc as usize })?;
            }
        }
    }
    Ok(())
}

/// Decoded channel payload (FFmpeg's `SingleChannelElement`, per-frame part).
#[derive(Clone)]
pub struct ChannelData {
    pub band_type: [u8; 128],
    /// Regular: scalefactor offset; intensity: clipped position; noise: clipped energy.
    pub sfo: [i32; 128],
    pub tns: Tns,
    /// Spectral coefficients, windows at a stride of 128.
    pub coeffs: Vec<f32>,
}

impl Default for ChannelData {
    fn default() -> ChannelData {
        ChannelData {
            band_type: [0; 128],
            sfo: [0; 128],
            tns: Tns::default(),
            coeffs: vec![0.0; 1024],
        }
    }
}

/// `lcg_random`, the reference decoder's PNS generator.
#[inline]
pub fn lcg_random(prev: u32) -> u32 {
    prev.wrapping_mul(1664525).wrapping_add(1013904223)
}

struct Pulse {
    pos: [usize; 4],
    amp: [u32; 4],
    n: usize,
}

/// `individual_channel_stream()` from global_gain to dequantised spectrum.
pub fn decode_ics(
    r: &mut BitReader,
    sx: &Syntax,
    ics: &mut Ics,
    cd: &mut ChannelData,
    common_window: bool,
    rng: &mut u32,
) -> Result<()> {
    let prof_ics = crate::prof::scope(crate::prof::Stage::DecIcs);
    let global_gain = r.read_bits(8)? as i32;
    if !common_window {
        parse_ics_info(r, sx, ics)?;
    }
    let info = ics.info.clone();
    let max_sfb = info.max_sfb as usize;
    let short = info.window_sequence == WindowSequence::EightShort;

    // section_data
    let bits = if short { 3 } else { 5 };
    let esc = (1u32 << bits) - 1;
    cd.band_type = [0; 128];
    for g in 0..info.num_window_groups {
        let mut k = 0usize;
        while k < max_sfb {
            let cb = r.read_bits(4)? as u8;
            if cb == 12 {
                return Err(Error::invalid("aac: invalid band type 12"));
            }
            let mut end = k;
            loop {
                let incr = r.read_bits(bits)?;
                end += incr as usize;
                if end > max_sfb {
                    return Err(Error::invalid("aac: section overruns max_sfb"));
                }
                if incr != esc {
                    break;
                }
            }
            for b in k..end {
                cd.band_type[g * max_sfb + b] = cb;
            }
            k = end;
        }
    }

    // scale_factor_data
    let (mut off0, mut off1, mut off2) = (global_gain, global_gain - 90, 0i32);
    let mut noise_flag = true;
    for g in 0..info.num_window_groups {
        for sfb in 0..max_sfb {
            let idx = g * max_sfb + sfb;
            match cd.band_type[idx] {
                ZERO_HCB => cd.sfo[idx] = 0,
                INTENSITY_HCB | INTENSITY_HCB2 => {
                    off2 += SCALEFACTOR_BOOK.decode(r)? as i32 - 60;
                    cd.sfo[idx] = off2.clamp(-155, 100);
                }
                NOISE_HCB => {
                    if noise_flag {
                        noise_flag = false;
                        off1 += r.read_bits(9)? as i32 - 256;
                    } else {
                        off1 += SCALEFACTOR_BOOK.decode(r)? as i32 - 60;
                    }
                    cd.sfo[idx] = off1.clamp(-100, 155);
                }
                _ => {
                    off0 += SCALEFACTOR_BOOK.decode(r)? as i32 - 60;
                    if !(0..=255).contains(&off0) {
                        return Err(Error::invalid("aac: scalefactor out of range"));
                    }
                    cd.sfo[idx] = off0;
                }
            }
        }
    }

    // pulse / tns / gain control
    let mut pulse = None;
    cd.tns = Tns::default();
    if !sx.eld() && r.read_bool()? {
        if short {
            return Err(Error::invalid(
                "aac: pulse tool not allowed in eight short sequence",
            ));
        }
        let n = r.read_bits(2)? as usize + 1;
        let start = r.read_bits(6)? as usize;
        if start >= info.num_swb {
            return Err(Error::invalid("aac: pulse data corrupt"));
        }
        let limit = ics.swb[info.num_swb] as usize;
        let mut p = Pulse {
            pos: [0; 4],
            amp: [0; 4],
            n,
        };
        p.pos[0] = ics.swb[start] as usize + r.read_bits(5)? as usize;
        if p.pos[0] >= limit {
            return Err(Error::invalid("aac: pulse data corrupt"));
        }
        p.amp[0] = r.read_bits(4)?;
        for i in 1..n {
            p.pos[i] = r.read_bits(5)? as usize + p.pos[i - 1];
            if p.pos[i] >= limit {
                return Err(Error::invalid("aac: pulse data corrupt"));
            }
            p.amp[i] = r.read_bits(4)?;
        }
        pulse = Some(p);
    }
    let tns_present = r.read_bool()?;
    if tns_present && !sx.er() {
        cd.tns = parse_tns(r, sx, &info)?;
    }
    if !sx.eld() && r.read_bool()? {
        skip_gain_control(r, info.window_sequence)?;
    }
    if tns_present && sx.er() {
        cd.tns = parse_tns(r, sx, &info)?;
    }

    // spectral_data (integers first, so pulses stay exact)
    let swb = ics.swb;
    let mut quant = vec![0i32; 1024];
    let mut wbase = 0usize;
    let mut tuple = [0i32; 4];
    for g in 0..info.num_window_groups {
        let glen = info.window_group_length[g] as usize;
        for sfb in 0..max_sfb {
            let cb = cd.band_type[g * max_sfb + sfb];
            if cb == ZERO_HCB || cb == NOISE_HCB || cb >= INTENSITY_HCB2 {
                continue;
            }
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            let meta = CODEBOOKS
                .get(cb as usize)
                .ok_or_else(|| Error::invalid("aac: bad spectral codebook"))?;
            let book = spectral_book(cb);
            let tab = crate::codebook::tuple_table(cb);
            let dim = meta.dim as usize;
            for w in 0..glen {
                let base = (wbase + w) * 128;
                let mut i = s;
                while i + dim <= e {
                    let t = tab[book.decode(r)? as usize];
                    for (o, &v) in tuple.iter_mut().zip(&t) {
                        *o = v as i32;
                    }
                    crate::codebook::finish_tuple(meta, r, &mut tuple)?;
                    quant[base + i..base + i + dim].copy_from_slice(&tuple[..dim]);
                    i += dim;
                }
            }
        }
        wbase += glen;
    }

    // Pulses add to single long-window coefficients of regular bands, away from 0.
    if let Some(p) = pulse {
        let mut band = 0usize;
        for i in 0..p.n {
            let k = p.pos[i];
            while swb[band + 1] as usize <= k {
                band += 1;
            }
            let bt = cd.band_type[band];
            if bt == ZERO_HCB || bt == NOISE_HCB || bt >= INTENSITY_HCB2 {
                continue;
            }
            if quant[k] > 0 {
                quant[k] += p.amp[i] as i32;
            } else {
                quant[k] -= p.amp[i] as i32;
            }
        }
    }

    // Dequantisation; PNS bands from the reference generator.
    drop(prof_ics); // the stages must not nest, or shares double-count
    let _prof = crate::prof::scope(crate::prof::Stage::DecDequant);
    let pow43 = crate::dsp::pow43_table();
    cd.coeffs.iter_mut().for_each(|c| *c = 0.0);
    let mut wbase = 0usize;
    for g in 0..info.num_window_groups {
        let glen = info.window_group_length[g] as usize;
        for sfb in 0..max_sfb {
            let idx = g * max_sfb + sfb;
            let cb = cd.band_type[idx];
            if cb == ZERO_HCB || cb >= INTENSITY_HCB2 {
                continue;
            }
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            if cb == NOISE_HCB {
                let g_amp = 2f32.powf(0.25 * cd.sfo[idx] as f32);
                for w in 0..glen {
                    let base = (wbase + w) * 128;
                    let band = &mut cd.coeffs[base + s..base + e];
                    for v in band.iter_mut() {
                        *rng = lcg_random(*rng);
                        *v = *rng as i32 as f32;
                    }
                    let energy: f32 = band.iter().map(|v| v * v).sum();
                    let scale = g_amp / energy.sqrt();
                    for v in band.iter_mut() {
                        *v *= scale;
                    }
                }
                continue;
            }
            let gain = crate::dsp::sf_gain(cd.sfo[idx]);
            for w in 0..glen {
                let base = (wbase + w) * 128;
                let (out, q) = (
                    &mut cd.coeffs[base + s..base + e],
                    &quant[base + s..base + e],
                );
                for (c, &q) in out.iter_mut().zip(q) {
                    *c = crate::dsp::dequant_with(pow43, q) * gain;
                }
            }
        }
        wbase += glen;
    }
    Ok(())
}
