//! **SBR reconstruction** (ISO/IEC 14496-3 §4.6.18): QMF analysis of the core
//! output, the frequency-band tables, envelope/noise decoding, HF generation by
//! patching with inverse filtering, HF adjustment (envelope gains, limiter,
//! smoothing, noise floor, sinusoids) and the 64-band synthesis — plus the hook
//! for Parametric Stereo.
//!
//! The QMF banks are evaluated directly from the standard's complex-exponential
//! definitions (in f64). The buffer timing (`X_low` with its 8-slot history, the
//! two-frame `Y` toggle, the envelope-adjustment offset of 2) is the standard's,
//! so frame boundaries line up with other conforming decoders.

use super::qmf::{qmf_analysis, qmf_synthesis, QmfSynthState};
use super::tables::{
    HUFF_ENV_1_5DB_F, HUFF_ENV_1_5DB_T, HUFF_ENV_3_0DB_F, HUFF_ENV_3_0DB_T, HUFF_ENV_BAL_1_5DB_F,
    HUFF_ENV_BAL_1_5DB_T, HUFF_ENV_BAL_3_0DB_F, HUFF_ENV_BAL_3_0DB_T, HUFF_NOISE_3_0DB_T,
    HUFF_NOISE_BAL_3_0DB_T, NOISE_TABLE, SBR_OFFSET,
};
use crate::bits::BitReader;
use crate::decode::layout::{TYPE_CCE, TYPE_CPE, TYPE_SCE};
use crate::decode::{Decoder, Element};
use crate::Result;
use std::sync::OnceLock;

/// t_HFAdj: the envelope adjuster runs two slots behind the HF generator.
const ENV_ADJ: usize = 2;
/// t_HFGen: slots of the previous frame kept in X_low.
const HF_GEN: usize = 8;
const NOISE_FLOOR_OFFSET: i32 = 6;

const FIXFIX: u32 = 0;
const VARFIX: u32 = 2;

type Cpx = [f32; 2];

// ---------------------------------------------------------------------------
// Huffman decoding of the SBR codebooks.
// ---------------------------------------------------------------------------

pub(super) struct SbrBook {
    /// (code, len, value), in canonical order.
    entries: Vec<(u32, u8, i32)>,
    /// Index into `entries` + 1 (0 = longer than the table), by the next bits.
    lut: Vec<u16>,
}

const SBR_LUT_BITS: u32 = 9;

impl SbrBook {
    pub(super) fn new(tab: &(&[(u8, u8)], i32)) -> SbrBook {
        let (syms, off) = *tab;
        let mut acc: u64 = 0;
        let mut entries = Vec::with_capacity(syms.len());
        for &(s, l) in syms {
            let code = (acc >> (32 - l as u32)) as u32;
            entries.push((code, l, s as i32 + off));
            acc += 1u64 << (32 - l as u32);
        }
        let mut lut = vec![0u16; 1 << SBR_LUT_BITS];
        for (i, &(c, l, _)) in entries.iter().enumerate() {
            if l as u32 <= SBR_LUT_BITS {
                let span = 1usize << (SBR_LUT_BITS - l as u32);
                let base = (c as usize) << (SBR_LUT_BITS - l as u32);
                lut[base..base + span]
                    .iter_mut()
                    .for_each(|e| *e = i as u16 + 1);
            }
        }
        SbrBook { entries, lut }
    }

    pub(super) fn decode(&self, r: &mut BitReader) -> Result<i32> {
        let i = self.lut[r.peek_bits(SBR_LUT_BITS) as usize];
        if i != 0 {
            let (_, l, v) = self.entries[i as usize - 1];
            r.skip(l as usize)?;
            return Ok(v);
        }
        let peek = r.peek_bits(24);
        for &(c, l, v) in &self.entries {
            if l as u32 > SBR_LUT_BITS && (peek >> (24 - l as u32)) == c {
                r.skip(l as usize)?;
                return Ok(v);
            }
        }
        Err(crate::Error::invalid("sbr: invalid Huffman codeword"))
    }
}

struct Books {
    env_1_5_t: SbrBook,
    env_1_5_f: SbrBook,
    bal_1_5_t: SbrBook,
    bal_1_5_f: SbrBook,
    env_3_0_t: SbrBook,
    env_3_0_f: SbrBook,
    bal_3_0_t: SbrBook,
    bal_3_0_f: SbrBook,
    noise_t: SbrBook,
    noise_bal_t: SbrBook,
}

fn books() -> &'static Books {
    static B: OnceLock<Books> = OnceLock::new();
    B.get_or_init(|| Books {
        env_1_5_t: SbrBook::new(&HUFF_ENV_1_5DB_T),
        env_1_5_f: SbrBook::new(&HUFF_ENV_1_5DB_F),
        bal_1_5_t: SbrBook::new(&HUFF_ENV_BAL_1_5DB_T),
        bal_1_5_f: SbrBook::new(&HUFF_ENV_BAL_1_5DB_F),
        env_3_0_t: SbrBook::new(&HUFF_ENV_3_0DB_T),
        env_3_0_f: SbrBook::new(&HUFF_ENV_3_0DB_F),
        bal_3_0_t: SbrBook::new(&HUFF_ENV_BAL_3_0DB_T),
        bal_3_0_f: SbrBook::new(&HUFF_ENV_BAL_3_0DB_F),
        noise_t: SbrBook::new(&HUFF_NOISE_3_0DB_T),
        noise_bal_t: SbrBook::new(&HUFF_NOISE_BAL_3_0DB_T),
    })
}

// ---------------------------------------------------------------------------
// State.
// ---------------------------------------------------------------------------

/// Per-channel SBR data and filterbank state.
#[derive(Clone)]
pub(crate) struct SbrChannel {
    bs_num_env: usize,
    bs_freq_res: [u8; 7],
    t_env: [usize; 7],
    t_env_num_env_old: usize,
    t_q: [usize; 3],
    bs_num_noise: usize,
    bs_amp_res: bool,
    e_a: [i32; 2],
    bs_df_env: [bool; 5],
    bs_df_noise: [bool; 2],
    bs_invf_mode: [[u8; 5]; 2],
    env_facs_q: [[i32; 48]; 6],
    env_facs: [[f32; 48]; 6],
    noise_facs_q: [[i32; 5]; 3],
    noise_facs: [[f32; 5]; 3],
    bs_add_harmonic_flag: bool,
    bs_add_harmonic: [bool; 48],
    s_indexmapped: [[bool; 48]; 8],
    bw_array: [f32; 5],
    ana_hist: Vec<f32>,
    w: Vec<[[Cpx; 32]; 32]>,
    ypos: usize,
    y: Vec<Vec<[Cpx; 64]>>,
    syn_v: QmfSynthState,
    g_temp: Vec<[f32; 48]>,
    q_temp: Vec<[f32; 48]>,
    f_indexnoise: usize,
    f_indexsine: usize,
}

impl SbrChannel {
    fn new() -> SbrChannel {
        SbrChannel {
            bs_num_env: 0,
            bs_freq_res: [0; 7],
            t_env: [0; 7],
            t_env_num_env_old: 0,
            t_q: [0; 3],
            bs_num_noise: 0,
            bs_amp_res: false,
            e_a: [-1, -1],
            bs_df_env: [false; 5],
            bs_df_noise: [false; 2],
            bs_invf_mode: [[0; 5]; 2],
            env_facs_q: [[0; 48]; 6],
            env_facs: [[0.0; 48]; 6],
            noise_facs_q: [[0; 5]; 3],
            noise_facs: [[0.0; 5]; 3],
            bs_add_harmonic_flag: false,
            bs_add_harmonic: [false; 48],
            s_indexmapped: [[false; 48]; 8],
            bw_array: [0.0; 5],
            ana_hist: vec![0.0; 288],
            w: vec![[[[0.0; 2]; 32]; 32]; 2],
            ypos: 0,
            y: vec![vec![[[0.0; 2]; 64]; 38]; 2],
            syn_v: QmfSynthState::new(),
            g_temp: vec![[0.0; 48]; 42],
            q_temp: vec![[0.0; 48]; 42],
            f_indexnoise: 0,
            f_indexsine: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Spectrum {
    start_freq: i32,
    stop_freq: i32,
    xover_band: i32,
    freq_scale: i32,
    alter_scale: i32,
    noise_bands: i32,
}

const SPECTRUM_UNSET: Spectrum = Spectrum {
    start_freq: -1,
    stop_freq: -1,
    xover_band: -1,
    freq_scale: -1,
    alter_scale: -1,
    noise_bands: -1,
};

/// Element-level SBR state.
#[derive(Clone)]
pub(crate) struct SbrChannelState {
    /// The SBR rate (twice the core rate); 0 until the first SBR payload.
    sample_rate: u32,
    start: bool,
    reset: bool,
    ready_for_dequant: bool,
    id_aac: u8,
    spectrum: Spectrum,
    bs_amp_res_header: bool,
    bs_limiter_bands: i32,
    bs_limiter_gains: usize,
    bs_interpol_freq: bool,
    bs_smoothing_mode: bool,
    bs_coupling: bool,
    k: [usize; 3],
    kx: [usize; 2],
    m: [usize; 2],
    kx_and_m_pushed: bool,
    n_master: usize,
    n: [usize; 2],
    n_q: usize,
    n_lim: usize,
    f_master: [i32; 49],
    f_tablelow: [usize; 25],
    f_tablehigh: [usize; 49],
    f_tablenoise: [usize; 6],
    f_tablelim: [usize; 30],
    num_patches: usize,
    patch_num_subbands: [usize; 6],
    patch_start_subband: [usize; 6],
    data: [SbrChannel; 2],
    ps: Option<Box<super::ps::PsState>>,
}

impl SbrChannelState {
    pub(crate) fn new(id_aac: u8) -> SbrChannelState {
        let mut s = SbrChannelState {
            sample_rate: 0,
            start: false,
            reset: false,
            ready_for_dequant: false,
            id_aac,
            spectrum: SPECTRUM_UNSET,
            bs_amp_res_header: false,
            bs_limiter_bands: 0,
            bs_limiter_gains: 0,
            bs_interpol_freq: false,
            bs_smoothing_mode: false,
            bs_coupling: false,
            k: [0; 3],
            kx: [0; 2],
            m: [0; 2],
            kx_and_m_pushed: false,
            n_master: 0,
            n: [0; 2],
            n_q: 0,
            n_lim: 0,
            f_master: [0; 49],
            f_tablelow: [0; 25],
            f_tablehigh: [0; 49],
            f_tablenoise: [0; 6],
            f_tablelim: [0; 30],
            num_patches: 0,
            patch_num_subbands: [0; 6],
            patch_start_subband: [0; 6],
            data: [SbrChannel::new(), SbrChannel::new()],
            ps: None,
        };
        s.turnoff();
        s
    }

    /// Pure-upsampling mode until the next valid header.
    fn turnoff(&mut self) {
        self.start = false;
        self.ready_for_dequant = false;
        self.kx[1] = 32;
        self.m[1] = 0;
        self.data[0].e_a[1] = -1;
        self.data[1].e_a[1] = -1;
        self.spectrum = SPECTRUM_UNSET;
    }
}

// ---------------------------------------------------------------------------
// Frequency band tables (§4.6.18.3).
// ---------------------------------------------------------------------------

fn lrintf(x: f32) -> i32 {
    x.round_ties_even() as i32
}

fn make_bands(start: i32, stop: i32, num_bands: usize) -> Vec<i32> {
    let mut bands = vec![0i32; num_bands];
    let base = (stop as f32 / start as f32).powf(1.0 / num_bands as f32);
    let mut prod = start as f32;
    let mut previous = start;
    for b in bands.iter_mut().take(num_bands - 1) {
        prod *= base;
        let present = lrintf(prod);
        *b = present - previous;
        previous = present;
    }
    bands[num_bands - 1] = stop - previous;
    bands
}

impl SbrChannelState {
    fn make_f_master(&mut self) -> bool {
        let sp = self.spectrum;
        let sr = self.sample_rate;
        let row = match sr {
            16000 => 0,
            22050 => 1,
            24000 => 2,
            32000 => 3,
            44100 | 48000 | 64000 => 4,
            88200 | 96000 | 128000 | 176400 | 192000 => 5,
            _ => return false,
        };
        let temp: u32 = if sr < 32000 {
            3000
        } else if sr < 64000 {
            4000
        } else {
            5000
        };
        let start_min = (((temp << 7) + (sr >> 1)) / sr) as i32;
        let stop_min = (((temp << 8) + (sr >> 1)) / sr) as i32;
        let k0 = start_min + SBR_OFFSET[row][sp.start_freq as usize] as i32;
        let k2 = match sp.stop_freq {
            0..=13 => {
                let mut dk = make_bands(stop_min, 64, 13);
                dk.sort_unstable();
                stop_min + dk[..sp.stop_freq as usize].iter().sum::<i32>()
            }
            14 => 2 * k0,
            15 => 3 * k0,
            _ => return false,
        }
        .min(64);
        let max_qmf: i32 = if sr <= 32000 {
            48
        } else if sr == 44100 {
            35
        } else {
            32
        };
        if k2 - k0 > max_qmf || k0 <= 0 {
            return false;
        }
        self.k[0] = k0 as usize;
        self.k[2] = k2 as usize;

        if sp.freq_scale == 0 {
            let dk = sp.alter_scale + 1;
            let n_master = ((k2 - k0 + (dk & 2)) >> dk) << 1;
            if n_master <= 0 || sp.xover_band >= n_master || n_master > 48 {
                return false;
            }
            let n_master = n_master as usize;
            for k in 1..=n_master {
                self.f_master[k] = dk;
            }
            let k2diff = k2 - k0 - n_master as i32 * dk;
            if k2diff < 0 {
                self.f_master[1] -= 1;
                self.f_master[2] -= (k2diff < -1) as i32;
            } else if k2diff != 0 {
                self.f_master[n_master] += 1;
            }
            self.f_master[0] = k0;
            for k in 1..=n_master {
                self.f_master[k] += self.f_master[k - 1];
            }
            self.n_master = n_master;
        } else {
            let half_bands = (7 - sp.freq_scale) as f32;
            let two_regions = 49 * k2 > 110 * k0;
            let k1 = if two_regions { 2 * k0 } else { k2 };
            self.k[1] = k1 as usize;
            let num_bands_0 = lrintf(half_bands * (k1 as f32 / k0 as f32).log2()) * 2;
            if num_bands_0 <= 0 || num_bands_0 > 48 {
                return false;
            }
            let nb0 = num_bands_0 as usize;
            let mut vk0 = make_bands(k0, k1, nb0);
            vk0.sort_unstable();
            let vdk0_max = vk0[nb0 - 1];
            let mut vk0c = vec![k0; nb0 + 1];
            for k in 1..=nb0 {
                if vk0[k - 1] <= 0 {
                    return false;
                }
                vk0c[k] = vk0c[k - 1] + vk0[k - 1];
            }
            if two_regions {
                let invwarp = if sp.alter_scale != 0 {
                    0.769_230_77f32
                } else {
                    1.0
                };
                let num_bands_1 = lrintf(half_bands * invwarp * (k2 as f32 / k1 as f32).log2()) * 2;
                if num_bands_1 <= 0 {
                    return false;
                }
                let nb1 = num_bands_1 as usize;
                let mut vk1 = make_bands(k1, k2, nb1);
                let vdk1_min = *vk1.iter().min().unwrap();
                if vdk1_min < vdk0_max {
                    vk1.sort_unstable();
                    let change = (vdk0_max - vk1[0]).min((vk1[nb1 - 1] - vk1[0]) >> 1);
                    vk1[0] += change;
                    vk1[nb1 - 1] -= change;
                }
                vk1.sort_unstable();
                let mut vk1c = vec![k1; nb1 + 1];
                for k in 1..=nb1 {
                    if vk1[k - 1] <= 0 {
                        return false;
                    }
                    vk1c[k] = vk1c[k - 1] + vk1[k - 1];
                }
                let n_master = nb0 + nb1;
                if n_master > 48 || sp.xover_band as usize >= n_master {
                    return false;
                }
                self.f_master[..=nb0].copy_from_slice(&vk0c);
                self.f_master[nb0 + 1..=n_master].copy_from_slice(&vk1c[1..]);
                self.n_master = n_master;
            } else {
                if sp.xover_band as usize >= nb0 {
                    return false;
                }
                self.f_master[..=nb0].copy_from_slice(&vk0c);
                self.n_master = nb0;
            }
        }
        true
    }

    fn calc_patches(&mut self) -> bool {
        let (mut last_k, mut last_msb) = (-1i64, -1i64);
        let mut sb: i32;
        let k0 = self.k[0] as i32;
        let mut msb = k0;
        let mut usb = self.kx[1] as i32;
        let goal_sb = (((1000u32 << 11) + (self.sample_rate >> 1)) / self.sample_rate) as i32;
        let top = (self.kx[1] + self.m[1]) as i32;
        self.num_patches = 0;
        let mut k = if goal_sb < top {
            let mut k = 0;
            while self.f_master[k] < goal_sb {
                k += 1;
            }
            k
        } else {
            self.n_master
        };
        loop {
            let mut odd;
            if k as i64 == last_k && msb as i64 == last_msb {
                return false;
            }
            last_k = k as i64;
            last_msb = msb as i64;
            let mut i = k as i64;
            loop {
                sb = self.f_master[i as usize];
                odd = (sb + k0) & 1;
                i -= 1;
                if !(sb > k0 - 1 + msb - odd) || i < 0 {
                    break;
                }
            }
            if self.num_patches > 5 {
                return false;
            }
            let nsub = (sb - usb).max(0);
            self.patch_num_subbands[self.num_patches] = nsub as usize;
            self.patch_start_subband[self.num_patches] = (k0 - odd - nsub).max(0) as usize;
            if nsub > 0 {
                usb = sb;
                msb = sb;
                self.num_patches += 1;
            } else {
                msb = self.kx[1] as i32;
            }
            if self.f_master[k] - sb < 3 {
                k = self.n_master;
            }
            if sb == top {
                break;
            }
        }
        if self.num_patches > 1 && self.patch_num_subbands[self.num_patches - 1] < 3 {
            self.num_patches -= 1;
        }
        true
    }

    fn make_f_tablelim(&mut self) {
        if self.bs_limiter_bands > 0 {
            let warped =
                [1.327_151_8f32, 1.185_092_8, 1.119_871_6][self.bs_limiter_bands as usize - 1];
            let np = self.num_patches;
            let mut borders = [0usize; 7];
            borders[0] = self.kx[1];
            for k in 1..=np {
                borders[k] = borders[k - 1] + self.patch_num_subbands[k - 1];
            }
            let mut lim: Vec<usize> = self.f_tablelow[..=self.n[0]].to_vec();
            if np > 1 {
                lim.extend_from_slice(&borders[1..np]);
            }
            lim.sort_unstable();
            let is_border = |v: usize| borders[..=np].contains(&v);
            // Merge bands narrower than the limiter resolution, keeping patch
            // borders where possible.
            let mut n_lim = (self.n[0] + np).saturating_sub(1);
            let (mut out, mut inp) = (0usize, 1usize);
            while out < n_lim {
                if lim[inp] as f32 >= lim[out] as f32 * warped {
                    out += 1;
                    lim[out] = lim[inp];
                    inp += 1;
                } else if lim[inp] == lim[out] || !is_border(lim[inp]) {
                    inp += 1;
                    n_lim -= 1;
                } else if !is_border(lim[out]) {
                    lim[out] = lim[inp];
                    inp += 1;
                    n_lim -= 1;
                } else {
                    out += 1;
                    lim[out] = lim[inp];
                    inp += 1;
                }
            }
            self.f_tablelim[..=n_lim].copy_from_slice(&lim[..=n_lim]);
            self.n_lim = n_lim;
        } else {
            self.f_tablelim[0] = self.f_tablelow[0];
            self.f_tablelim[1] = self.f_tablelow[self.n[0]];
            self.n_lim = 1;
        }
    }

    fn make_f_derived(&mut self) -> bool {
        let xover = self.spectrum.xover_band as usize;
        self.n[1] = self.n_master - xover;
        self.n[0] = (self.n[1] + 1) >> 1;
        for k in 0..=self.n[1] {
            self.f_tablehigh[k] = self.f_master[xover + k] as usize;
        }
        self.m[1] = self.f_tablehigh[self.n[1]] - self.f_tablehigh[0];
        self.kx[1] = self.f_tablehigh[0];
        if self.kx[1] + self.m[1] > 64 || self.kx[1] > 32 || self.kx[1] == 0 {
            return false;
        }
        self.f_tablelow[0] = self.f_tablehigh[0];
        let odd = self.n[1] & 1;
        for k in 1..=self.n[0] {
            self.f_tablelow[k] = self.f_tablehigh[2 * k - odd];
        }
        let nq = lrintf(
            self.spectrum.noise_bands as f32 * (self.k[2] as f32 / self.kx[1] as f32).log2(),
        )
        .max(1);
        if nq > 5 {
            self.n_q = 1;
            return false;
        }
        self.n_q = nq as usize;
        self.f_tablenoise[0] = self.f_tablelow[0];
        let mut temp = 0usize;
        for k in 1..=self.n_q {
            temp += (self.n[0] - temp) / (self.n_q + 1 - k);
            self.f_tablenoise[k] = self.f_tablelow[temp];
        }
        if !self.calc_patches() {
            return false;
        }
        self.make_f_tablelim();
        self.data[0].f_indexnoise = 0;
        self.data[1].f_indexnoise = 0;
        true
    }

    fn sbr_reset(&mut self) {
        if !(self.make_f_master() && self.make_f_derived()) {
            self.turnoff();
        }
    }
}

// ---------------------------------------------------------------------------
// Bitstream (§4.4.2.8).
// ---------------------------------------------------------------------------

impl SbrChannelState {
    fn read_header(&mut self, r: &mut BitReader) -> Result<()> {
        self.start = true;
        self.ready_for_dequant = false;
        let old = self.spectrum;
        let old_lim = self.bs_limiter_bands;
        self.bs_amp_res_header = r.read_bool()?;
        let mut sp = Spectrum {
            start_freq: r.read_bits(4)? as i32,
            stop_freq: r.read_bits(4)? as i32,
            xover_band: r.read_bits(3)? as i32,
            ..old
        };
        r.skip(2)?;
        let extra1 = r.read_bool()?;
        let extra2 = r.read_bool()?;
        if extra1 {
            sp.freq_scale = r.read_bits(2)? as i32;
            sp.alter_scale = r.read_bits(1)? as i32;
            sp.noise_bands = r.read_bits(2)? as i32;
        } else {
            sp.freq_scale = 2;
            sp.alter_scale = 1;
            sp.noise_bands = 2;
        }
        self.spectrum = sp;
        if sp != old {
            self.reset = true;
        }
        if extra2 {
            self.bs_limiter_bands = r.read_bits(2)? as i32;
            self.bs_limiter_gains = r.read_bits(2)? as usize;
            self.bs_interpol_freq = r.read_bool()?;
            self.bs_smoothing_mode = r.read_bool()?;
        } else {
            self.bs_limiter_bands = 2;
            self.bs_limiter_gains = 2;
            self.bs_interpol_freq = true;
            self.bs_smoothing_mode = true;
        }
        if self.bs_limiter_bands != old_lim && !self.reset {
            self.make_f_tablelim();
        }
        Ok(())
    }

    fn read_grid(&mut self, r: &mut BitReader, ch: usize, num_time_slots: usize) -> Result<bool> {
        const CEIL_LOG2: [u32; 6] = [0, 1, 2, 2, 3, 3];
        let d = &mut self.data[ch];
        let mut bs_pointer = 0usize;
        let mut abs_bord_trail = num_time_slots;
        let bs_num_env_old = d.bs_num_env;
        d.bs_freq_res[0] = d.bs_freq_res[d.bs_num_env];
        d.bs_amp_res = self.bs_amp_res_header;
        d.t_env_num_env_old = d.t_env[bs_num_env_old];
        let class = r.read_bits(2)?;
        // Relative borders count back from the trailing border; an underflow
        // is caught by the monotonicity check below.
        let mut t_env = d.t_env.map(|v| v as i64);
        let n = match class {
            FIXFIX => {
                let n = 1usize << r.read_bits(2)?;
                if n > 5 {
                    return Ok(false);
                }
                if n == 1 {
                    d.bs_amp_res = false;
                }
                t_env[0] = 0;
                t_env[n] = abs_bord_trail as i64;
                let step = ((abs_bord_trail + (n >> 1)) / n) as i64;
                for i in 0..n - 1 {
                    t_env[i + 1] = t_env[i] + step;
                }
                d.bs_freq_res[1] = r.read_bits(1)? as u8;
                for i in 1..n {
                    d.bs_freq_res[i + 1] = d.bs_freq_res[1];
                }
                n
            }
            1 => {
                abs_bord_trail += r.read_bits(2)? as usize;
                let num_rel_trail = r.read_bits(2)? as usize;
                let n = num_rel_trail + 1;
                t_env[0] = 0;
                t_env[n] = abs_bord_trail as i64;
                for i in 0..num_rel_trail {
                    t_env[n - 1 - i] = t_env[n - i] - 2 * r.read_bits(2)? as i64 - 2;
                }
                bs_pointer = r.read_bits(CEIL_LOG2[n])? as usize;
                for i in 0..n {
                    d.bs_freq_res[n - i] = r.read_bits(1)? as u8;
                }
                n
            }
            VARFIX => {
                t_env[0] = r.read_bits(2)? as i64;
                let num_rel_lead = r.read_bits(2)? as usize;
                let n = num_rel_lead + 1;
                t_env[n] = abs_bord_trail as i64;
                for i in 0..num_rel_lead {
                    t_env[i + 1] = t_env[i] + 2 * r.read_bits(2)? as i64 + 2;
                }
                bs_pointer = r.read_bits(CEIL_LOG2[n])? as usize;
                for i in 0..n {
                    d.bs_freq_res[i + 1] = r.read_bits(1)? as u8;
                }
                n
            }
            _ => {
                t_env[0] = r.read_bits(2)? as i64;
                abs_bord_trail += r.read_bits(2)? as usize;
                let num_rel_lead = r.read_bits(2)? as usize;
                let num_rel_trail = r.read_bits(2)? as usize;
                let n = num_rel_lead + num_rel_trail + 1;
                if n > 5 {
                    return Ok(false);
                }
                t_env[n] = abs_bord_trail as i64;
                for i in 0..num_rel_lead {
                    t_env[i + 1] = t_env[i] + 2 * r.read_bits(2)? as i64 + 2;
                }
                for i in 0..num_rel_trail {
                    t_env[n - 1 - i] = t_env[n - i] - 2 * r.read_bits(2)? as i64 - 2;
                }
                bs_pointer = r.read_bits(CEIL_LOG2[n])? as usize;
                for i in 0..n {
                    d.bs_freq_res[i + 1] = r.read_bits(1)? as u8;
                }
                n
            }
        };
        if bs_pointer > n + 1 {
            return Ok(false);
        }
        for i in 1..=n {
            if t_env[i - 1] >= t_env[i] || t_env[i - 1] < 0 {
                return Ok(false);
            }
        }
        for (dst, &src) in d.t_env.iter_mut().zip(&t_env) {
            *dst = src.max(0) as usize;
        }
        d.bs_num_env = n;
        d.bs_num_noise = (n > 1) as usize + 1;
        d.t_q[0] = d.t_env[0];
        d.t_q[d.bs_num_noise] = d.t_env[n];
        if d.bs_num_noise > 1 {
            let idx = if class == FIXFIX {
                n >> 1
            } else if class & 1 == 1 {
                n - bs_pointer.saturating_sub(1).max(1)
            } else if bs_pointer == 0 {
                1
            } else if bs_pointer == 1 {
                n - 1
            } else {
                bs_pointer - 1
            };
            d.t_q[1] = d.t_env[idx];
        }
        d.e_a[0] = -((d.e_a[1] != bs_num_env_old as i32) as i32);
        d.e_a[1] = -1;
        if class & 1 == 1 && bs_pointer != 0 {
            d.e_a[1] = (n + 1 - bs_pointer) as i32;
        } else if class == VARFIX && bs_pointer > 1 {
            d.e_a[1] = bs_pointer as i32 - 1;
        }
        Ok(true)
    }

    fn copy_grid(&mut self) {
        let [src, dst] = &mut self.data;
        dst.bs_freq_res[0] = dst.bs_freq_res[dst.bs_num_env];
        dst.t_env_num_env_old = dst.t_env[dst.bs_num_env];
        dst.e_a[0] = -((dst.e_a[1] != dst.bs_num_env as i32) as i32);
        dst.bs_freq_res[1..].copy_from_slice(&src.bs_freq_res[1..]);
        dst.t_env = src.t_env;
        dst.t_q = src.t_q;
        dst.bs_num_env = src.bs_num_env;
        dst.bs_amp_res = src.bs_amp_res;
        dst.bs_num_noise = src.bs_num_noise;
        dst.e_a[1] = src.e_a[1];
    }

    fn read_dtdf(&mut self, r: &mut BitReader, ch: usize) -> Result<()> {
        let d = &mut self.data[ch];
        for i in 0..d.bs_num_env {
            d.bs_df_env[i] = r.read_bool()?;
        }
        for i in 0..d.bs_num_noise {
            d.bs_df_noise[i] = r.read_bool()?;
        }
        Ok(())
    }

    fn read_invf(&mut self, r: &mut BitReader, ch: usize) -> Result<()> {
        let n_q = self.n_q;
        let d = &mut self.data[ch];
        d.bs_invf_mode[1] = d.bs_invf_mode[0];
        for i in 0..n_q {
            d.bs_invf_mode[0][i] = r.read_bits(2)? as u8;
        }
        Ok(())
    }

    fn read_envelope(&mut self, r: &mut BitReader, ch: usize) -> Result<bool> {
        let b = books();
        let coupled = self.bs_coupling && ch == 1;
        let delta = if coupled { 2 } else { 1 };
        let odd = self.n[1] & 1;
        let n = self.n;
        let d = &mut self.data[ch];
        let (bits, t_huff, f_huff) = match (coupled, d.bs_amp_res) {
            (true, true) => (5, &b.bal_3_0_t, &b.bal_3_0_f),
            (true, false) => (6, &b.bal_1_5_t, &b.bal_1_5_f),
            (false, true) => (6, &b.env_3_0_t, &b.env_3_0_f),
            (false, false) => (7, &b.env_1_5_t, &b.env_1_5_f),
        };
        for i in 0..d.bs_num_env {
            let res = d.bs_freq_res[i + 1] as usize;
            if d.bs_df_env[i] {
                for j in 0..n[res] {
                    // Map to the previous envelope's resolution.
                    let k = if d.bs_freq_res[i + 1] == d.bs_freq_res[i] {
                        j
                    } else if res == 1 {
                        (j + odd) >> 1
                    } else if j == 0 {
                        0
                    } else {
                        2 * j - odd
                    };
                    let v = d.env_facs_q[i][k] + delta * t_huff.decode(r)?;
                    if !(0..=127).contains(&v) {
                        return Ok(false);
                    }
                    d.env_facs_q[i + 1][j] = v;
                }
            } else {
                d.env_facs_q[i + 1][0] = delta * r.read_bits(bits)? as i32;
                for j in 1..n[res] {
                    let v = d.env_facs_q[i + 1][j - 1] + delta * f_huff.decode(r)?;
                    if !(0..=127).contains(&v) {
                        return Ok(false);
                    }
                    d.env_facs_q[i + 1][j] = v;
                }
            }
        }
        d.env_facs_q[0] = d.env_facs_q[d.bs_num_env];
        Ok(true)
    }

    fn read_noise(&mut self, r: &mut BitReader, ch: usize) -> Result<bool> {
        let b = books();
        let coupled = self.bs_coupling && ch == 1;
        let delta = if coupled { 2 } else { 1 };
        let n_q = self.n_q;
        let d = &mut self.data[ch];
        let (t_huff, f_huff) = if coupled {
            (&b.noise_bal_t, &b.bal_3_0_f)
        } else {
            (&b.noise_t, &b.env_3_0_f)
        };
        for i in 0..d.bs_num_noise {
            if d.bs_df_noise[i] {
                for j in 0..n_q {
                    let v = d.noise_facs_q[i][j] + delta * t_huff.decode(r)?;
                    if !(0..=30).contains(&v) {
                        return Ok(false);
                    }
                    d.noise_facs_q[i + 1][j] = v;
                }
            } else {
                d.noise_facs_q[i + 1][0] = delta * r.read_bits(5)? as i32;
                for j in 1..n_q {
                    let v = d.noise_facs_q[i + 1][j - 1] + delta * f_huff.decode(r)?;
                    if !(0..=30).contains(&v) {
                        return Ok(false);
                    }
                    d.noise_facs_q[i + 1][j] = v;
                }
            }
        }
        d.noise_facs_q[0] = d.noise_facs_q[d.bs_num_noise];
        Ok(true)
    }

    fn read_harmonics(&mut self, r: &mut BitReader, ch: usize) -> Result<()> {
        let n1 = self.n[1];
        let d = &mut self.data[ch];
        d.bs_add_harmonic_flag = r.read_bool()?;
        if d.bs_add_harmonic_flag {
            for i in 0..n1 {
                d.bs_add_harmonic[i] = r.read_bool()?;
            }
        }
        Ok(())
    }

    /// `sbr_data()`; Ok(false) = invalid data (SBR turns off).
    fn read_data(
        &mut self,
        r: &mut BitReader,
        id_aac: u8,
        nts: usize,
        ps_allowed: bool,
    ) -> Result<bool> {
        self.id_aac = id_aac;
        self.ready_for_dequant = true;
        if id_aac == TYPE_SCE || id_aac == TYPE_CCE {
            if r.read_bool()? {
                r.skip(4)?; // bs_reserved
            }
            if !self.read_grid(r, 0, nts)? {
                return Ok(false);
            }
            self.read_dtdf(r, 0)?;
            self.read_invf(r, 0)?;
            if !self.read_envelope(r, 0)? || !self.read_noise(r, 0)? {
                return Ok(false);
            }
            self.read_harmonics(r, 0)?;
        } else if id_aac == TYPE_CPE {
            if r.read_bool()? {
                r.skip(8)?; // bs_reserved
            }
            self.bs_coupling = r.read_bool()?;
            if self.bs_coupling {
                if !self.read_grid(r, 0, nts)? {
                    return Ok(false);
                }
                self.copy_grid();
                self.read_dtdf(r, 0)?;
                self.read_dtdf(r, 1)?;
                self.read_invf(r, 0)?;
                self.data[1].bs_invf_mode[1] = self.data[1].bs_invf_mode[0];
                self.data[1].bs_invf_mode[0] = self.data[0].bs_invf_mode[0];
                for ch in 0..2 {
                    if !self.read_envelope(r, ch)? || !self.read_noise(r, ch)? {
                        return Ok(false);
                    }
                }
            } else {
                if !self.read_grid(r, 0, nts)? || !self.read_grid(r, 1, nts)? {
                    return Ok(false);
                }
                self.read_dtdf(r, 0)?;
                self.read_dtdf(r, 1)?;
                self.read_invf(r, 0)?;
                self.read_invf(r, 1)?;
                if !self.read_envelope(r, 0)? || !self.read_envelope(r, 1)? {
                    return Ok(false);
                }
                if !self.read_noise(r, 0)? || !self.read_noise(r, 1)? {
                    return Ok(false);
                }
            }
            self.read_harmonics(r, 0)?;
            self.read_harmonics(r, 1)?;
        } else {
            return Ok(false);
        }
        if r.read_bool()? {
            // bs_extended_data: sbr_extension() elements (PS is id 2).
            let mut cnt = r.read_bits(4)? as i64;
            if cnt == 15 {
                cnt += r.read_bits(8)? as i64;
            }
            let mut bits_left = cnt * 8;
            while bits_left > 7 {
                bits_left -= 2;
                let id = r.read_bits(2)?;
                if id == 2 && ps_allowed {
                    let ps = self
                        .ps
                        .get_or_insert_with(|| Box::new(super::ps::PsState::new()));
                    bits_left -= ps.read_data(r, bits_left as usize, 2 * nts)? as i64;
                } else {
                    r.skip(bits_left as usize)?;
                    bits_left = 0;
                }
            }
            if bits_left > 0 {
                r.skip(bits_left as usize)?;
            }
        }
        Ok(true)
    }

    /// `sbr_extension_data()` from a fill element payload (after its 4-bit
    /// extension type): `bits` payload bits, `crc` = EXT_SBR_DATA_CRC.
    #[allow(clippy::too_many_arguments)]
    fn decode_extension(
        &mut self,
        payload: &[u8],
        bits: usize,
        crc: bool,
        id_aac: u8,
        nts: usize,
        ps_allowed: bool,
        rate: u32,
    ) {
        if self.sample_rate == 0 {
            self.sample_rate = rate;
        }
        let mut r = BitReader::new(payload);
        self.reset = false;
        let res = (|| -> Result<bool> {
            if crc {
                r.skip(10)?; // bs_sbr_crc_bits
            }
            self.kx[0] = self.kx[1];
            self.m[0] = self.m[1];
            self.kx_and_m_pushed = true;
            if r.read_bool()? {
                self.read_header(&mut r)?;
            }
            if self.reset {
                self.sbr_reset();
            }
            if self.start {
                return self.read_data(&mut r, id_aac, nts, ps_allowed);
            }
            Ok(true)
        })();
        match res {
            Ok(true) if r.position() <= bits => {}
            _ => self.turnoff(),
        }
    }
}

// ---------------------------------------------------------------------------
// Processing (§4.6.18.5-4.6.18.7).
// ---------------------------------------------------------------------------

#[inline]
fn exp2i(x: i32) -> f32 {
    2f32.powi(x)
}

/// 2^(q/2) for the 1.5 dB resolution: the odd half-step is √2, applied in double.
#[inline]
fn exp2_half(q: i32, base: i32) -> f32 {
    let odd = if q & 1 == 1 {
        std::f64::consts::SQRT_2
    } else {
        1.0
    };
    (exp2i((q >> 1) + base) as f64 * odd) as f32
}

impl SbrChannelState {
    fn dequant(&mut self, id_aac: u8) {
        let n = self.n;
        let n_q = self.n_q;
        if id_aac == TYPE_CPE && self.bs_coupling {
            let amp = self.data[0].bs_amp_res;
            let pan_offset = if amp { 12 } else { 24 };
            let [d0, d1] = &mut self.data;
            for e in 1..=d0.bs_num_env {
                for k in 0..n[d0.bs_freq_res[e] as usize] {
                    let (q0, q1) = (d0.env_facs_q[e][k], d1.env_facs_q[e][k]);
                    let (mut t1, t2) = if amp {
                        (exp2i(q0 + 7), exp2i(pan_offset - q1))
                    } else {
                        (exp2_half(q0, 7), exp2_half(pan_offset - q1, 0))
                    };
                    if t1 > 1e20 {
                        t1 = 1.0;
                    }
                    let fac = t1 / (1.0 + t2);
                    d0.env_facs[e][k] = fac;
                    d1.env_facs[e][k] = fac * t2;
                }
            }
            for e in 1..=d0.bs_num_noise {
                for k in 0..n_q {
                    let t1 = exp2i(NOISE_FLOOR_OFFSET - d0.noise_facs_q[e][k] + 1);
                    let t2 = exp2i(12 - d1.noise_facs_q[e][k]);
                    let fac = t1 / (1.0 + t2);
                    d0.noise_facs[e][k] = fac;
                    d1.noise_facs[e][k] = fac * t2;
                }
            }
        } else {
            let nch = (id_aac == TYPE_CPE) as usize + 1;
            for d in self.data.iter_mut().take(nch) {
                for e in 1..=d.bs_num_env {
                    for k in 0..n[d.bs_freq_res[e] as usize] {
                        let q = d.env_facs_q[e][k];
                        let v = if d.bs_amp_res {
                            exp2i(q + 6)
                        } else {
                            exp2_half(q, 6)
                        };
                        d.env_facs[e][k] = if v > 1e20 { 1.0 } else { v };
                    }
                }
                for e in 1..=d.bs_num_noise {
                    for k in 0..n_q {
                        d.noise_facs[e][k] = exp2i(NOISE_FLOOR_OFFSET - d.noise_facs_q[e][k]);
                    }
                }
            }
        }
    }
}

/// Scratch shared by one element's processing.
struct Work {
    x_low: Vec<[Cpx; 40]>,
    x_high: Vec<[Cpx; 40]>,
    alpha0: [Cpx; 64],
    alpha1: [Cpx; 64],
    e_origmapped: [[f32; 48]; 7],
    q_mapped: [[f32; 48]; 7],
    s_mapped: [[bool; 48]; 7],
    e_curr: [[f32; 48]; 7],
    q_m: [[f32; 48]; 7],
    s_m: [[f32; 48]; 7],
    gain: [[f32; 48]; 7],
}

impl Work {
    fn new() -> Work {
        Work {
            x_low: vec![[[0.0; 2]; 40]; 32],
            x_high: vec![[[0.0; 2]; 40]; 64],
            alpha0: [[0.0; 2]; 64],
            alpha1: [[0.0; 2]; 64],
            e_origmapped: [[0.0; 48]; 7],
            q_mapped: [[0.0; 48]; 7],
            s_mapped: [[false; 48]; 7],
            e_curr: [[0.0; 48]; 7],
            q_m: [[0.0; 48]; 7],
            s_m: [[0.0; 48]; 7],
            gain: [[0.0; 48]; 7],
        }
    }
}

/// Covariance-method LPC of order 2 per low band (§4.6.18.6.2).
fn hf_inverse_filter(
    x_low: &[[Cpx; 40]],
    k0: usize,
    alpha0: &mut [Cpx; 64],
    alpha1: &mut [Cpx; 64],
) {
    for k in 0..k0 {
        let x = &x_low[k];
        let (mut r0, mut r1, mut i1, mut r2, mut i2) = (0f32, 0f32, 0f32, 0f32, 0f32);
        for i in 1..38 {
            r0 += x[i][0] * x[i][0] + x[i][1] * x[i][1];
            r1 += x[i][0] * x[i + 1][0] + x[i][1] * x[i + 1][1];
            i1 += x[i][0] * x[i + 1][1] - x[i][1] * x[i + 1][0];
            r2 += x[i][0] * x[i + 2][0] + x[i][1] * x[i + 2][1];
            i2 += x[i][0] * x[i + 2][1] - x[i][1] * x[i + 2][0];
        }
        // phi(i, j) over the window, in the standard's notation.
        let phi01 = [
            r2 + x[0][0] * x[2][0] + x[0][1] * x[2][1],
            i2 + x[0][0] * x[2][1] - x[0][1] * x[2][0],
        ];
        let phi22 = r0 + x[0][0] * x[0][0] + x[0][1] * x[0][1];
        let phi11 = r0 + x[38][0] * x[38][0] + x[38][1] * x[38][1];
        let phi12 = [
            r1 + x[0][0] * x[1][0] + x[0][1] * x[1][1],
            i1 + x[0][0] * x[1][1] - x[0][1] * x[1][0],
        ];
        let phi00 = [
            r1 + x[38][0] * x[39][0] + x[38][1] * x[39][1],
            i1 + x[38][0] * x[39][1] - x[38][1] * x[39][0],
        ];

        let dk = phi22 * phi11 - (phi12[0] * phi12[0] + phi12[1] * phi12[1]) / 1.000_001;
        let a1 = if dk == 0.0 {
            [0.0, 0.0]
        } else {
            let tr = phi00[0] * phi12[0] - phi00[1] * phi12[1] - phi01[0] * phi11;
            let ti = phi00[0] * phi12[1] + phi00[1] * phi12[0] - phi01[1] * phi11;
            [tr / dk, ti / dk]
        };
        let a0 = if phi11 == 0.0 {
            [0.0, 0.0]
        } else {
            let tr = phi00[0] + a1[0] * phi12[0] + a1[1] * phi12[1];
            let ti = phi00[1] + a1[1] * phi12[0] - a1[0] * phi12[1];
            [-tr / phi11, -ti / phi11]
        };
        if a1[0] * a1[0] + a1[1] * a1[1] >= 16.0 || a0[0] * a0[0] + a0[1] * a0[1] >= 16.0 {
            alpha0[k] = [0.0, 0.0];
            alpha1[k] = [0.0, 0.0];
        } else {
            alpha0[k] = a0;
            alpha1[k] = a1;
        }
    }
}

#[inline]
fn sum_square(row: &[Cpx; 40], lo: usize, hi: usize) -> f32 {
    let (mut s0, mut s1) = (0f32, 0f32);
    let mut i = lo;
    while i + 1 < hi {
        s0 += row[i][0] * row[i][0];
        s1 += row[i][1] * row[i][1];
        s0 += row[i + 1][0] * row[i + 1][0];
        s1 += row[i + 1][1] * row[i + 1][1];
        i += 2;
    }
    if i < hi {
        s0 += row[i][0] * row[i][0];
        s1 += row[i][1] * row[i][1];
    }
    s0 + s1
}

impl SbrChannelState {
    fn chirp(&mut self, ch: usize) {
        const BW_TAB: [f32; 4] = [0.0, 0.75, 0.9, 0.98];
        let n_q = self.n_q;
        let d = &mut self.data[ch];
        for i in 0..n_q {
            let mut nb = if d.bs_invf_mode[0][i] + d.bs_invf_mode[1][i] == 1 {
                0.6
            } else {
                BW_TAB[d.bs_invf_mode[0][i] as usize]
            };
            if nb < d.bw_array[i] {
                nb = 0.75 * nb + 0.25 * d.bw_array[i];
            } else {
                nb = 0.90625 * nb + 0.09375 * d.bw_array[i];
            }
            d.bw_array[i] = if nb < 0.015625 { 0.0 } else { nb };
        }
    }

    /// X_low: this frame's slots for bands < kx', plus 8 slots of the previous
    /// frame for bands < kx (the previous frame's crossover).
    fn lf_gen(&self, wk: &mut Work, ch: usize, buf_idx: usize, nts: usize) {
        let d = &self.data[ch];
        let i_f = 2 * nts;
        for row in wk.x_low.iter_mut() {
            *row = [[0.0; 2]; 40];
        }
        for k in 0..self.kx[1] {
            for i in HF_GEN..i_f + HF_GEN {
                wk.x_low[k][i] = d.w[buf_idx][i - HF_GEN][k];
            }
        }
        for k in 0..self.kx[0] {
            for i in 0..HF_GEN {
                wk.x_low[k][i] = d.w[1 - buf_idx][i + i_f - HF_GEN][k];
            }
        }
    }

    fn hf_gen(&self, wk: &mut Work, ch: usize) -> bool {
        let d = &self.data[ch];
        let mut g = 0usize;
        let mut k = self.kx[1];
        let (start, end) = (2 * d.t_env[0], 2 * d.t_env[d.bs_num_env]);
        for j in 0..self.num_patches {
            for x in 0..self.patch_num_subbands[j] {
                let p = self.patch_start_subband[j] + x;
                while g <= self.n_q && k >= self.f_tablenoise[g] {
                    g += 1;
                }
                if g == 0 {
                    return false;
                }
                g -= 1;
                let bw = d.bw_array[g];
                let (a0, a1) = (wk.alpha0[p], wk.alpha1[p]);
                let al = [a1[0] * bw * bw, a1[1] * bw * bw, a0[0] * bw, a0[1] * bw];
                let xl = wk.x_low[p];
                let xh = &mut wk.x_high[k];
                for i in start + ENV_ADJ..end + ENV_ADJ {
                    xh[i] = [
                        xl[i - 2][0] * al[0] - xl[i - 2][1] * al[1] + xl[i - 1][0] * al[2]
                            - xl[i - 1][1] * al[3]
                            + xl[i][0],
                        xl[i - 2][1] * al[0]
                            + xl[i - 2][0] * al[1]
                            + xl[i - 1][1] * al[2]
                            + xl[i - 1][0] * al[3]
                            + xl[i][1],
                    ];
                }
                k += 1;
            }
        }
        while k < self.m[1] + self.kx[1] {
            wk.x_high[k] = [[0.0; 2]; 40];
            k += 1;
        }
        true
    }

    fn mapping(&mut self, wk: &mut Work, ch: usize) -> bool {
        let kx = self.kx[1];
        let d = &mut self.data[ch];
        let e_a = d.e_a;
        for e in 1..8 {
            d.s_indexmapped[e] = [false; 48];
        }
        for e in 0..d.bs_num_env {
            let res = d.bs_freq_res[e + 1] as usize;
            let ilim = self.n[res];
            let table: &[usize] = if res == 1 {
                &self.f_tablehigh
            } else {
                &self.f_tablelow
            };
            if kx != table[0] {
                return false;
            }
            for i in 0..ilim {
                for m in table[i]..table[i + 1] {
                    wk.e_origmapped[e][m - kx] = d.env_facs[e + 1][i];
                }
            }
            let k = (d.bs_num_noise > 1 && d.t_env[e] >= d.t_q[1]) as usize;
            for i in 0..self.n_q {
                for m in self.f_tablenoise[i]..self.f_tablenoise[i + 1] {
                    wk.q_mapped[e][m - kx] = d.noise_facs[k + 1][i];
                }
            }
            if d.bs_add_harmonic_flag {
                for i in 0..self.n[1] {
                    let mid = (self.f_tablehigh[i] + self.f_tablehigh[i + 1]) >> 1;
                    d.s_indexmapped[e + 1][mid - kx] = d.bs_add_harmonic[i]
                        && (e as i32 >= e_a[1] || d.s_indexmapped[0][mid - kx]);
                }
            }
            for i in 0..ilim {
                let present = (table[i]..table[i + 1]).any(|m| d.s_indexmapped[e + 1][m - kx]);
                for m in table[i]..table[i + 1] {
                    wk.s_mapped[e][m - kx] = present;
                }
            }
        }
        d.s_indexmapped[0] = d.s_indexmapped[d.bs_num_env];
        true
    }

    fn env_estimate(&self, wk: &mut Work, ch: usize) {
        let d = &self.data[ch];
        let kx = self.kx[1];
        for e in 0..d.bs_num_env {
            let ilb = d.t_env[e] * 2 + ENV_ADJ;
            let iub = (d.t_env[e + 1] * 2 + ENV_ADJ).min(40);
            if self.bs_interpol_freq {
                let recip = 0.5f32 / (d.t_env[e + 1] - d.t_env[e]) as f32;
                for m in 0..self.m[1] {
                    wk.e_curr[e][m] = sum_square(&wk.x_high[m + kx], ilb, iub) * recip;
                }
            } else {
                let env_size = 2 * (d.t_env[e + 1] - d.t_env[e]);
                let res = d.bs_freq_res[e + 1] as usize;
                let table: &[usize] = if res == 1 {
                    &self.f_tablehigh
                } else {
                    &self.f_tablelow
                };
                for p in 0..self.n[res] {
                    let den = (env_size * (table[p + 1] - table[p])) as f32;
                    let mut sum = 0f32;
                    for k in table[p]..table[p + 1] {
                        sum += sum_square(&wk.x_high[k], ilb, iub);
                    }
                    sum /= den;
                    for k in table[p]..table[p + 1] {
                        wk.e_curr[e][k - kx] = sum;
                    }
                }
            }
        }
    }

    fn gain_calc(&self, wk: &mut Work, ch: usize) {
        // Limiter: -3 dB, 0 dB, +3 dB, off.
        const LIMGAIN: [f32; 4] = [0.70795, 1.0, 1.41254, 10_000_000_000.0];
        let d = &self.data[ch];
        let kx = self.kx[1];
        for e in 0..d.bs_num_env {
            let delta = !(e as i32 == d.e_a[1] || e as i32 == d.e_a[0]);
            let deltaf = delta as u8 as f32;
            for k in 0..self.n_lim {
                let (lo, hi) = (self.f_tablelim[k] - kx, self.f_tablelim[k + 1] - kx);
                for m in lo..hi {
                    let (eo, qm, ec) = (wk.e_origmapped[e][m], wk.q_mapped[e][m], wk.e_curr[e][m]);
                    let temp = eo / (1.0 + qm);
                    wk.q_m[e][m] = (temp * qm).sqrt();
                    wk.s_m[e][m] = (temp * d.s_indexmapped[e + 1][m] as u8 as f32).sqrt();
                    let g = if !wk.s_mapped[e][m] {
                        (eo / ((1.0 + ec) * (1.0 + qm * deltaf))).sqrt()
                    } else {
                        (eo * qm / ((1.0 + ec) * (1.0 + qm))).sqrt()
                    };
                    wk.gain[e][m] = g + f32::MIN_POSITIVE;
                }
                let (mut s0, mut s1) = (0f32, 0f32);
                for m in lo..hi {
                    s0 += wk.e_origmapped[e][m];
                    s1 += wk.e_curr[e][m];
                }
                let gain_max = (LIMGAIN[self.bs_limiter_gains]
                    * ((f32::EPSILON + s0) / (f32::EPSILON + s1)).sqrt())
                .min(100_000.0);
                for m in lo..hi {
                    let q_m_max = wk.q_m[e][m] * gain_max / wk.gain[e][m];
                    wk.q_m[e][m] = wk.q_m[e][m].min(q_m_max);
                    wk.gain[e][m] = wk.gain[e][m].min(gain_max);
                }
                let (mut s0, mut s1) = (0f32, 0f32);
                for m in lo..hi {
                    s0 += wk.e_origmapped[e][m];
                    let noise = if delta && wk.s_m[e][m] == 0.0 {
                        wk.q_m[e][m] * wk.q_m[e][m]
                    } else {
                        0.0
                    };
                    s1 += wk.e_curr[e][m] * wk.gain[e][m] * wk.gain[e][m]
                        + wk.s_m[e][m] * wk.s_m[e][m]
                        + noise;
                }
                let boost = ((f32::EPSILON + s0) / (f32::EPSILON + s1))
                    .sqrt()
                    .min(1.584_893_2);
                for m in lo..hi {
                    wk.gain[e][m] *= boost;
                    wk.q_m[e][m] *= boost;
                    wk.s_m[e][m] *= boost;
                }
            }
        }
    }

    fn hf_assemble(&mut self, wk: &Work, ch: usize) {
        const H_SMOOTH: [f32; 5] = [
            0.333_333_33,
            0.301_502_83,
            0.218_169_5,
            0.115_163_83,
            0.031_830_5,
        ];
        let h_sl = if self.bs_smoothing_mode { 0 } else { 4 };
        let kx = self.kx[1];
        let m_max = self.m[1];
        let reset = self.reset;
        let d = &mut self.data[ch];
        let e_a = d.e_a;
        let mut indexnoise = d.f_indexnoise;
        let mut indexsine = d.f_indexsine;
        let t0 = 2 * d.t_env[0];
        if reset {
            for i in 0..h_sl {
                d.g_temp[i + t0][..m_max].copy_from_slice(&wk.gain[0][..m_max]);
                d.q_temp[i + t0][..m_max].copy_from_slice(&wk.q_m[0][..m_max]);
            }
        } else if h_sl > 0 {
            let old = 2 * d.t_env_num_env_old;
            for i in 0..4 {
                d.g_temp[i + t0] = d.g_temp[i + old];
                d.q_temp[i + t0] = d.q_temp[i + old];
            }
        }
        for e in 0..d.bs_num_env {
            for i in 2 * d.t_env[e]..2 * d.t_env[e + 1] {
                d.g_temp[h_sl + i][..m_max].copy_from_slice(&wk.gain[e][..m_max]);
                d.q_temp[h_sl + i][..m_max].copy_from_slice(&wk.q_m[e][..m_max]);
            }
        }
        let ypos = d.ypos;
        for e in 0..d.bs_num_env {
            let transient = e as i32 == e_a[0] || e as i32 == e_a[1];
            let s_m = &wk.s_m[e];
            for i in 2 * d.t_env[e]..2 * d.t_env[e + 1] {
                let mut g_filt = [0f32; 48];
                let mut q_filt = [0f32; 48];
                if h_sl > 0 && !transient {
                    for m in 0..m_max {
                        for (j, h) in H_SMOOTH.iter().enumerate() {
                            g_filt[m] += d.g_temp[i + h_sl - j][m] * h;
                            q_filt[m] += d.q_temp[i + h_sl - j][m] * h;
                        }
                    }
                } else {
                    g_filt[..m_max].copy_from_slice(&d.g_temp[i + h_sl][..m_max]);
                    q_filt[..m_max].copy_from_slice(&d.q_temp[i][..m_max]);
                }
                let y = &mut d.y[ypos][i];
                for m in 0..m_max {
                    let xh = wk.x_high[kx + m][i + ENV_ADJ];
                    y[kx + m] = [xh[0] * g_filt[m], xh[1] * g_filt[m]];
                }
                if !transient {
                    // Sinusoid phase φ_sin[indexsine]; the imaginary part
                    // alternates sign per band, starting from (-1)^kx.
                    let alt = 1.0 - 2.0 * (kx & 1) as f32;
                    let (p0, mut p1): (f32, f32) = match indexsine {
                        0 => (1.0, 0.0),
                        1 => (0.0, alt),
                        2 => (-1.0, 0.0),
                        _ => (0.0, -alt),
                    };
                    let mut noise = indexnoise;
                    for m in 0..m_max {
                        noise = (noise + 1) & 0x1ff;
                        let yy = &mut y[kx + m];
                        if s_m[m] != 0.0 {
                            yy[0] += s_m[m] * p0;
                            yy[1] += s_m[m] * p1;
                        } else {
                            yy[0] += q_filt[m] * NOISE_TABLE[2 * noise];
                            yy[1] += q_filt[m] * NOISE_TABLE[2 * noise + 1];
                        }
                        p1 = -p1;
                    }
                } else {
                    // Transient envelope: sinusoids only, on one component.
                    let idx = indexsine & 1;
                    let a: f32 = 1.0 - ((indexsine + (kx & 1)) & 2) as f32;
                    let b = if idx == 1 { -a } else { a };
                    let mut m = 0;
                    while m + 1 < m_max {
                        y[kx + m][idx] += s_m[m] * a;
                        y[kx + m + 1][idx] += s_m[m + 1] * b;
                        m += 2;
                    }
                    if m_max & 1 == 1 {
                        y[kx + m][idx] += s_m[m] * a;
                    }
                }
                indexnoise = (indexnoise + m_max) & 0x1ff;
                indexsine = (indexsine + 1) & 3;
            }
        }
        d.f_indexnoise = indexnoise;
        d.f_indexsine = indexsine;
    }

    /// The synthesis input X[38][64]: low band from X_low, high band from the
    /// adjusted Y of this frame and (for the first slots) the previous one.
    fn x_gen(&self, wk: &Work, ch: usize, nts: usize) -> Vec<[Cpx; 64]> {
        let d = &self.data[ch];
        let i_f = 2 * nts;
        let i_temp = (2 * d.t_env_num_env_old).saturating_sub(i_f);
        let mut x = vec![[[0f32; 2]; 64]; 38];
        let (y0, y1) = (&d.y[1 - d.ypos], &d.y[d.ypos]);
        for k in 0..self.kx[0] {
            for i in 0..i_temp {
                x[i][k] = wk.x_low[k][i + ENV_ADJ];
            }
        }
        for k in self.kx[0]..(self.kx[0] + self.m[0]).min(64) {
            for i in 0..i_temp {
                x[i][k] = y0[i + i_f][k];
            }
        }
        for k in 0..self.kx[1] {
            for i in i_temp..38 {
                x[i][k] = wk.x_low[k][i + ENV_ADJ];
            }
        }
        for k in self.kx[1]..(self.kx[1] + self.m[1]).min(64) {
            for i in i_temp..i_f {
                x[i][k] = y1[i][k];
            }
        }
        x
    }
}

/// Apply SBR (and PS) to an element after the core synthesis. `n` is the
/// core frame length; the element's outputs are replaced with the SBR rate
/// output (2n samples, or n when downsampled).
pub(crate) fn apply(dec: &mut Decoder, el: &mut Element, ty: u8, n: usize) {
    let nts = n / 64;
    let core_rate = dec.stream_config().sample_rate;
    let ext_rate = dec.sbr_ext_rate();
    let ps_on = dec.ps_on();
    let sbr = el
        .sbr
        .get_or_insert_with(|| Box::new(SbrChannelState::new(ty)));
    if let Some((payload, bits, crc)) = el.sbr_payload.take() {
        sbr.decode_extension(&payload, bits, crc, ty, nts, ps_on, 2 * core_rate);
    }
    let downsampled = ext_rate < sbr.sample_rate;
    let nch = if ty == TYPE_CPE { 2 } else { 1 };
    if ty != sbr.id_aac {
        sbr.turnoff();
    }
    if sbr.start && !sbr.ready_for_dequant {
        sbr.turnoff();
    }
    if !sbr.kx_and_m_pushed {
        sbr.kx[0] = sbr.kx[1];
        sbr.m[0] = sbr.m[1];
    } else {
        sbr.kx_and_m_pushed = false;
    }
    if sbr.start {
        sbr.dequant(ty);
        sbr.ready_for_dequant = false;
    }
    let mut wk = Work::new();
    let mut xs: Vec<Vec<[Cpx; 64]>> = Vec::with_capacity(2);
    for ch in 0..nch {
        let yp = sbr.data[ch].ypos;
        {
            let d = &mut sbr.data[ch];
            let _prof = crate::prof::scope(crate::prof::Stage::DecSbrAnalysis);
            qmf_analysis(&el.ch[ch].output[..n], &mut d.ana_hist, &mut d.w[yp], nts);
        }
        let prof_hf = crate::prof::scope(crate::prof::Stage::DecSbrHf);
        sbr.lf_gen(&mut wk, ch, yp, nts);
        sbr.data[ch].ypos ^= 1;
        if sbr.start {
            hf_inverse_filter(&wk.x_low, sbr.k[0], &mut wk.alpha0, &mut wk.alpha1);
            sbr.chirp(ch);
            if sbr.data[ch].bs_num_env > 0 && sbr.hf_gen(&mut wk, ch) {
                if sbr.mapping(&mut wk, ch) {
                    sbr.env_estimate(&mut wk, ch);
                    sbr.gain_calc(&mut wk, ch);
                    sbr.hf_assemble(&wk, ch);
                } else {
                    sbr.turnoff();
                }
            }
        }
        xs.push(sbr.x_gen(&wk, ch, nts));
        drop(prof_hf);
    }
    if ps_on {
        let top = sbr.kx[1] + sbr.m[1];
        let mut x1 = xs[0].clone();
        if let Some(ps) = sbr.ps.as_mut().filter(|p| p.started()) {
            let _prof = crate::prof::scope(crate::prof::Stage::DecPs);
            ps.apply(&mut xs[0], &mut x1, top);
        }
        xs.truncate(1);
        xs.push(x1);
    }
    let outn = if downsampled { n } else { 2 * n };
    for (ch, x) in xs.iter().enumerate() {
        let _prof = crate::prof::scope(crate::prof::Stage::DecSbrSynthesis);
        qmf_synthesis(
            &mut el.ch[ch].output[..outn],
            x,
            &mut sbr.data[ch].syn_v,
            nts,
            downsampled,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbr_books_are_prefix_complete() {
        let b = books();
        for book in [
            &b.env_1_5_t,
            &b.env_1_5_f,
            &b.bal_1_5_t,
            &b.bal_1_5_f,
            &b.env_3_0_t,
            &b.env_3_0_f,
            &b.bal_3_0_t,
            &b.bal_3_0_f,
            &b.noise_t,
            &b.noise_bal_t,
        ] {
            let kraft: f64 = book.entries.iter().map(|e| 0.5f64.powi(e.1 as i32)).sum();
            assert!((kraft - 1.0).abs() < 1e-9, "kraft {kraft}");
        }
    }
}
