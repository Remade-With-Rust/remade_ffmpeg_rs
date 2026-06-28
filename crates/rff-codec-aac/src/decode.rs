//! AAC-LC decode assembly: `raw_data_block` → individual channels → spectral
//! reconstruction → filterbank → PCM (ISO 14496-3 §4.4 / §4.6).
//!
//! Supports long (`ONLY_LONG`), transition (`LONG_START`/`LONG_STOP`) and short
//! (`EIGHT_SHORT`) windows with grouped scalefactors/sections, regular Huffman
//! codebooks, pulse data and M/S stereo, synthesised via the verified IMDCT +
//! windowed overlap-add (2048 for long, 8×256 for short). Output is normalised
//! to float [-1, 1] and is bit-exact against FFmpeg on real AAC-LC files. TNS is
//! parsed (for sync) but not yet applied; intensity stereo and PNS are rejected
//! with a clear error rather than mis-decoded.

use rff_core::{AudioFrame, Error, Frame, Result, SampleFormat};

use crate::bits::BitReader;
use crate::codebook::{decode_tuple, CODEBOOKS, INTENSITY_HCB2, NOISE_HCB, ZERO_HCB};
use crate::dsp;
use crate::ics::{parse_ics_info, IcsInfo, WindowSequence};
use crate::swb::swb_offsets;
use crate::tables::{spectral_book, SCALEFACTOR_BOOK};
use crate::SAMPLE_RATES;

const FRAME_LEN: usize = 1024;
const LONG_N: usize = 2048;
const SHORT_N: usize = 256;
const SHORT_HALF: usize = 128;
/// The ISO reconstruction is on a ~16-bit scale; normalise to float [-1, 1]
/// (verified bit-exact vs FFmpeg).
const OUTPUT_NORM: f32 = 1.0 / 32768.0;

// raw_data_block element identifiers (id_syn_ele).
const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_CCE: u32 = 2;
const ID_LFE: u32 = 3;
const ID_DSE: u32 = 4;
const ID_PCE: u32 = 5;
const ID_FIL: u32 = 6;
const ID_END: u32 = 7;

/// Stateful AAC-LC decoder: holds per-channel overlap memory and the windows.
pub struct Decoder {
    sample_rate: u32,
    fs_index: u8,
    /// Per-channel overlap-add memory (second half of the previous frame).
    overlap: Vec<[f32; FRAME_LEN]>,
    /// Per-channel previous window shape (false = sine, true = KBD).
    prev_kbd: Vec<bool>,
    /// Per-channel previous window sequence.
    prev_seq: Vec<WindowSequence>,
    sine: Vec<f32>,
    kbd: Vec<f32>,
    sine_s: Vec<f32>,
    kbd_s: Vec<f32>,
    /// Deterministic PRNG state for PNS noise fill.
    rng: u32,
}

impl Decoder {
    pub fn new(sample_rate: u32) -> Decoder {
        Decoder {
            sample_rate,
            fs_index: fs_index_for(sample_rate),
            overlap: Vec::new(),
            prev_kbd: Vec::new(),
            prev_seq: Vec::new(),
            sine: dsp::sine_window(LONG_N),
            kbd: dsp::kbd_window(LONG_N, 4.0),
            sine_s: dsp::sine_window(SHORT_N),
            kbd_s: dsp::kbd_window(SHORT_N, 6.0),
            rng: 0x1234_5678,
        }
    }

    /// Decode one raw access unit into an interleaved-`f32` [`Frame`].
    pub fn decode(&mut self, au: &[u8], pts: Option<i64>) -> Result<Frame> {
        let mut r = BitReader::new(au);
        let mut outputs: Vec<Vec<f32>> = Vec::new();
        let mut ch = 0usize;

        loop {
            if r.bits_left() < 3 {
                break;
            }
            match r.read_bits(3)? {
                ID_SCE | ID_LFE => {
                    let _tag = r.read_bits(4)?;
                    let mut c = decode_channel(&mut r, self.fs_index, None, &mut self.rng)?;
                    apply_tns(&mut c.spec, &c.tns, &c.info, self.fs_index);
                    outputs.push(self.synthesize(ch, &c.info, &c.spec));
                    ch += 1;
                }
                ID_CPE => {
                    let _tag = r.read_bits(4)?;
                    let common = r.read_bool()?;
                    let (info, ms_used) = if common {
                        let info = parse_ics_info(&mut r, self.fs_index)?;
                        let ms = read_ms_used(&mut r, &info)?;
                        (Some(info), ms)
                    } else {
                        (None, Vec::new())
                    };
                    let info_ref = info.as_ref();
                    let mut c0 = decode_channel(&mut r, self.fs_index, info_ref, &mut self.rng)?;
                    let mut c1 = decode_channel(&mut r, self.fs_index, info_ref, &mut self.rng)?;
                    let fs = self.fs_index;
                    // M/S and intensity stereo require a shared window/grouping.
                    if common {
                        apply_ms(&c0.info, &ms_used, fs, &c1.cb, &mut c0.spec, &mut c1.spec);
                        apply_is(
                            &c0.info,
                            &ms_used,
                            fs,
                            &c1.cb,
                            &c1.sf,
                            &c0.spec,
                            &mut c1.spec,
                        );
                    }
                    apply_tns(&mut c0.spec, &c0.tns, &c0.info, fs);
                    apply_tns(&mut c1.spec, &c1.tns, &c1.info, fs);
                    outputs.push(self.synthesize(ch, &c0.info, &c0.spec));
                    outputs.push(self.synthesize(ch + 1, &c1.info, &c1.spec));
                    ch += 2;
                }
                ID_FIL => {
                    let mut count = r.read_bits(4)? as usize;
                    if count == 15 {
                        count += r.read_bits(8)? as usize - 1;
                    }
                    r.skip(count * 8)?;
                }
                ID_DSE => {
                    let _tag = r.read_bits(4)?;
                    let align = r.read_bool()?;
                    let mut count = r.read_bits(8)? as usize;
                    if count == 255 {
                        count += r.read_bits(8)? as usize;
                    }
                    if align {
                        r.byte_align();
                    }
                    r.skip(count * 8)?;
                }
                ID_END => break,
                ID_CCE | ID_PCE => {
                    return Err(Error::unsupported(
                        "aac: coupling / program-config elements not supported",
                    ));
                }
                _ => unreachable!(),
            }
        }

        if outputs.is_empty() {
            return Err(Error::invalid("aac: no channel elements in frame"));
        }
        Ok(interleave(outputs, self.sample_rate, pts))
    }

    /// Build the 2048-sample analysis window for a long/transition block: left
    /// half follows the previous block's shape, right half the current's.
    fn long_window(&self, seq: WindowSequence, prev_kbd: bool, cur_kbd: bool) -> Vec<f32> {
        let long_prev = if prev_kbd { &self.kbd } else { &self.sine };
        let long_cur = if cur_kbd { &self.kbd } else { &self.sine };
        let short_prev = if prev_kbd { &self.kbd_s } else { &self.sine_s };
        let short_cur = if cur_kbd { &self.kbd_s } else { &self.sine_s };
        let mut w = vec![0f32; LONG_N];

        if seq == WindowSequence::LongStop {
            for n in 0..128 {
                w[448 + n] = short_prev[n];
            }
            for s in w.iter_mut().take(FRAME_LEN).skip(576) {
                *s = 1.0;
            }
        } else {
            w[..FRAME_LEN].copy_from_slice(&long_prev[..FRAME_LEN]);
        }

        if seq == WindowSequence::LongStart {
            for s in w.iter_mut().take(FRAME_LEN + 448).skip(FRAME_LEN) {
                *s = 1.0;
            }
            for n in 0..128 {
                w[FRAME_LEN + 448 + n] = short_cur[128 + n];
            }
        } else {
            w[FRAME_LEN..].copy_from_slice(&long_cur[FRAME_LEN..]);
        }
        w
    }

    /// Synthesise eight 256-IMDCT short windows into a normalised 2048 frame.
    fn short_frame(&self, spec: &[f32; FRAME_LEN], cur_kbd: bool) -> Vec<f32> {
        let sw = if cur_kbd { &self.kbd_s } else { &self.sine_s };
        let mut frame = vec![0f32; LONG_N];
        for w in 0..8 {
            let time = dsp::imdct(&spec[w * SHORT_HALF..(w + 1) * SHORT_HALF]); // 256
            let off = 448 + w * SHORT_HALF;
            for n in 0..SHORT_N {
                frame[off + n] += time[n] * sw[n] * OUTPUT_NORM;
            }
        }
        frame
    }

    /// IMDCT + window + overlap-add for one channel's spectrum → 1024 samples.
    fn synthesize(&mut self, ch: usize, info: &IcsInfo, spec: &[f32; FRAME_LEN]) -> Vec<f32> {
        while self.overlap.len() <= ch {
            self.overlap.push([0.0; FRAME_LEN]);
            self.prev_kbd.push(false);
            self.prev_seq.push(WindowSequence::OnlyLong);
        }

        let frame = if info.window_sequence == WindowSequence::EightShort {
            self.short_frame(spec, info.window_shape_kbd)
        } else {
            let time = dsp::imdct(spec); // 2048
            let win = self.long_window(
                info.window_sequence,
                self.prev_kbd[ch],
                info.window_shape_kbd,
            );
            (0..LONG_N)
                .map(|n| time[n] * win[n] * OUTPUT_NORM)
                .collect()
        };

        let mut out = vec![0.0f32; FRAME_LEN];
        for n in 0..FRAME_LEN {
            out[n] = frame[n] + self.overlap[ch][n];
        }
        for n in 0..FRAME_LEN {
            self.overlap[ch][n] = frame[FRAME_LEN + n];
        }
        self.prev_kbd[ch] = info.window_shape_kbd;
        self.prev_seq[ch] = info.window_sequence;
        out
    }
}

/// Decode one `individual_channel_stream` to a dequantized spectrum. For short
/// blocks the layout is window-major (eight 128-bin windows).
struct Channel {
    info: IcsInfo,
    spec: [f32; FRAME_LEN],
    cb: Vec<Vec<u8>>,
    sf: Vec<Vec<i32>>,
    tns: Tns,
}

fn decode_channel(
    r: &mut BitReader,
    fs_index: u8,
    common_info: Option<&IcsInfo>,
    rng: &mut u32,
) -> Result<Channel> {
    let global_gain = r.read_bits(8)? as i32;
    let info = match common_info {
        Some(i) => i.clone(),
        None => parse_ics_info(r, fs_index)?,
    };

    let is_short = info.window_sequence == WindowSequence::EightShort;
    let swb = swb_offsets(!is_short, fs_index);
    let max_sfb = info.max_sfb as usize;
    if max_sfb + 1 > swb.len() {
        return Err(Error::invalid("aac: max_sfb out of range"));
    }
    let window_len = if is_short { SHORT_HALF } else { FRAME_LEN };

    let sfb_cb = read_sections(r, info.num_window_groups, max_sfb, is_short)?;
    let sf = read_scalefactors(r, &sfb_cb, global_gain)?;

    let pulse = if r.read_bool()? {
        Some(read_pulse(r)?)
    } else {
        None
    };
    let tns = if r.read_bool()? {
        parse_tns(r, &info)?
    } else {
        Tns::default()
    };
    if r.read_bool()? {
        return Err(Error::unsupported("aac: gain control not supported"));
    }

    let mut quant = [0i32; FRAME_LEN];
    read_spectrum(r, &sfb_cb, &info, swb, window_len, &mut quant)?;
    if let (Some(p), false) = (&pulse, is_short) {
        apply_pulse(&mut quant, p, swb);
    }

    // Inverse quantization, per group/window with the group's band scalefactor.
    // PNS (NOISE_HCB) bands are filled with energy-scaled random noise instead.
    let mut spec = [0f32; FRAME_LEN];
    let mut wbase = 0usize;
    for (g, group) in sfb_cb.iter().enumerate() {
        for (sfb, &cb) in group.iter().enumerate() {
            if cb == ZERO_HCB {
                continue;
            }
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            for w in 0..info.window_group_length[g] as usize {
                let base = (wbase + w) * window_len;
                if cb == NOISE_HCB {
                    fill_noise(rng, &mut spec, base + s, base + e, sf[g][sfb]);
                } else {
                    let gain = dsp::sf_gain(sf[g][sfb]);
                    for i in s..e {
                        spec[base + i] = dsp::dequant(quant[base + i]) * gain;
                    }
                }
            }
        }
        wbase += info.window_group_length[g] as usize;
    }
    Ok(Channel {
        info,
        spec,
        cb: sfb_cb,
        sf,
        tns,
    })
}

/// section_data: a codebook number per scalefactor band, per group.
fn read_sections(
    r: &mut BitReader,
    groups: usize,
    max_sfb: usize,
    is_short: bool,
) -> Result<Vec<Vec<u8>>> {
    let bits = if is_short { 3 } else { 5 };
    let esc = (1u32 << bits) - 1;
    let mut out = Vec::with_capacity(groups);
    for _ in 0..groups {
        let mut cbs = vec![0u8; max_sfb];
        let mut k = 0usize;
        while k < max_sfb {
            let cb = r.read_bits(4)? as u8;
            let mut len = 0usize;
            loop {
                let incr = r.read_bits(bits)? as usize;
                len += incr;
                if incr as u32 != esc {
                    break;
                }
            }
            if k + len > max_sfb {
                return Err(Error::invalid("aac: section overruns max_sfb"));
            }
            for c in cbs.iter_mut().skip(k).take(len) {
                *c = cb;
            }
            k += len;
        }
        out.push(cbs);
    }
    Ok(out)
}

/// scale_factor_data: differentially-coded scalefactors (regular codebooks).
fn read_scalefactors(
    r: &mut BitReader,
    sfb_cb: &[Vec<u8>],
    global_gain: i32,
) -> Result<Vec<Vec<i32>>> {
    let mut acc = global_gain;
    let mut noise = global_gain - 90; // PNS noise-energy accumulator
    let mut noise_pcm = true;
    let mut is_pos = 0i32; // intensity-stereo position accumulator
    let mut out = Vec::with_capacity(sfb_cb.len());
    for group in sfb_cb {
        let mut sf = vec![0i32; group.len()];
        for (sfb, &cb) in group.iter().enumerate() {
            if cb == ZERO_HCB {
                continue;
            } else if cb >= INTENSITY_HCB2 {
                is_pos += SCALEFACTOR_BOOK.decode(r)? as i32 - 60;
                sf[sfb] = is_pos;
            } else if cb == NOISE_HCB {
                // PNS: first noise energy is a 9-bit PCM delta, rest are Huffman.
                if noise_pcm {
                    noise_pcm = false;
                    noise += r.read_bits(9)? as i32 - 256;
                } else {
                    noise += SCALEFACTOR_BOOK.decode(r)? as i32 - 60;
                }
                sf[sfb] = noise;
            } else {
                acc += SCALEFACTOR_BOOK.decode(r)? as i32 - 60;
                sf[sfb] = acc;
            }
        }
        out.push(sf);
    }
    Ok(out)
}

/// spectral_data: Huffman-decode quantized coefficients per group/band/window.
fn read_spectrum(
    r: &mut BitReader,
    sfb_cb: &[Vec<u8>],
    info: &IcsInfo,
    swb: &[u16],
    window_len: usize,
    quant: &mut [i32; FRAME_LEN],
) -> Result<()> {
    let mut wbase = 0usize;
    for (g, group) in sfb_cb.iter().enumerate() {
        for (sfb, &cb) in group.iter().enumerate() {
            // ZERO, NOISE (PNS) and intensity bands carry no spectral data.
            if cb == ZERO_HCB || cb == NOISE_HCB || cb >= INTENSITY_HCB2 {
                continue;
            }
            let meta = &CODEBOOKS[cb as usize];
            let book = spectral_book(cb);
            let dim = meta.dim as usize;
            let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
            for w in 0..info.window_group_length[g] as usize {
                let base = (wbase + w) * window_len;
                let mut i = s;
                let mut tuple = [0i32; 4];
                while i + dim <= e {
                    decode_tuple(meta, book, r, &mut tuple)?;
                    quant[base + i..base + i + dim].copy_from_slice(&tuple[..dim]);
                    i += dim;
                }
            }
        }
        wbase += info.window_group_length[g] as usize;
    }
    Ok(())
}

/// xorshift32 → uniform f32 in [-1, 1). Decoder-local; PNS noise is random by
/// design, so it is energy-matched (not sample-exact) across decoders.
fn next_rand(s: &mut u32) -> f32 {
    *s ^= *s << 13;
    *s ^= *s >> 17;
    *s ^= *s << 5;
    (*s as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Fill `spec[start..end]` with random noise whose spectral energy matches the
/// transmitted `noise_energy` (ISO 14496-3 §4.6.13): unit-energy random vector
/// scaled so band energy = 2^(noise_energy/2).
fn fill_noise(rng: &mut u32, spec: &mut [f32], start: usize, end: usize, noise_energy: i32) {
    let mut energy = 0f64;
    for i in start..end {
        let v = next_rand(rng);
        spec[i] = v;
        energy += (v as f64) * (v as f64);
    }
    if energy <= 0.0 {
        return;
    }
    let scale = (2f64.powf(0.25 * noise_energy as f64) / energy.sqrt()) as f32;
    for v in spec[start..end].iter_mut() {
        *v *= scale;
    }
}

struct Pulse {
    start_sfb: usize,
    offsets: Vec<u32>,
    amps: Vec<u32>,
}

fn read_pulse(r: &mut BitReader) -> Result<Pulse> {
    let n = r.read_bits(2)? as usize; // number_pulse - 1
    let start_sfb = r.read_bits(6)? as usize;
    let mut offsets = Vec::with_capacity(n + 1);
    let mut amps = Vec::with_capacity(n + 1);
    for _ in 0..=n {
        offsets.push(r.read_bits(5)?);
        amps.push(r.read_bits(4)?);
    }
    Ok(Pulse {
        start_sfb,
        offsets,
        amps,
    })
}

fn apply_pulse(quant: &mut [i32; FRAME_LEN], p: &Pulse, swb: &[u16]) {
    let mut k = *swb.get(p.start_sfb).unwrap_or(&0) as usize;
    for (off, amp) in p.offsets.iter().zip(&p.amps) {
        k += *off as usize;
        if k < FRAME_LEN {
            if quant[k] > 0 {
                quant[k] += *amp as i32;
            } else {
                quant[k] -= *amp as i32;
            }
        }
    }
}

/// One TNS noise-shaping filter (a span of bands + its LPC synthesis filter).
struct TnsFilter {
    length: usize, // span in scalefactor bands (from the top down)
    order: usize,
    direction: bool, // true = downward (high→low) filtering
    lpc: Vec<f32>,   // lpc[0]=1, lpc[1..=order]
}

/// TNS data for a channel: filters per window.
#[derive(Default)]
struct Tns {
    windows: Vec<Vec<TnsFilter>>,
}

/// Max TNS-affected band per sampling-frequency index (ISO Table 4.A.45/46).
const TNS_MAX_LONG: [u8; 13] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39, 39];
const TNS_MAX_SHORT: [u8; 13] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14];

/// Parse TNS, dequantizing PARCOR coefficients and converting them to LPC.
fn parse_tns(r: &mut BitReader, info: &IcsInfo) -> Result<Tns> {
    use std::f32::consts::PI;
    let short = info.window_sequence == WindowSequence::EightShort;
    let (nf_bits, len_bits, ord_bits) = if short { (1, 4, 3) } else { (2, 6, 5) };
    let mut windows = Vec::with_capacity(info.num_windows);
    for _ in 0..info.num_windows {
        let mut filters = Vec::new();
        let n_filt = r.read_bits(nf_bits)?;
        let coef_res = if n_filt > 0 { r.read_bits(1)? } else { 0 };
        for _ in 0..n_filt {
            let length = r.read_bits(len_bits)? as usize;
            let order = r.read_bits(ord_bits)? as usize;
            let (mut direction, mut lpc) = (false, Vec::new());
            if order > 0 {
                direction = r.read_bool()?;
                let coef_compress = r.read_bits(1)?;
                let res_bits = 3 + coef_res; // 3 or 4
                let coef_bits = res_bits - coef_compress;
                let iqfac = ((1i32 << (res_bits - 1)) as f32 - 0.5) / (PI / 2.0);
                let iqfac_m = ((1i32 << (res_bits - 1)) as f32 + 0.5) / (PI / 2.0);
                let mut parcor = vec![0f32; order];
                for p in parcor.iter_mut() {
                    let raw = r.read_bits(coef_bits)? as i32;
                    let c = if raw & (1 << (coef_bits - 1)) != 0 {
                        raw - (1 << coef_bits)
                    } else {
                        raw
                    };
                    let t = if c >= 0 {
                        c as f32 / iqfac
                    } else {
                        c as f32 / iqfac_m
                    };
                    *p = t.sin();
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
    Ok(Tns { windows })
}

/// PARCOR (reflection) → LPC coefficients via the step-up recursion.
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

/// Apply TNS: an all-pole (synthesis) filter over each filter's band span,
/// per window, in the coded direction (ISO 14496-3 §4.6.9.3).
fn apply_tns(spec: &mut [f32; FRAME_LEN], tns: &Tns, info: &IcsInfo, fs_index: u8) {
    if tns.windows.iter().all(|w| w.is_empty()) {
        return;
    }
    let short = info.window_sequence == WindowSequence::EightShort;
    let swb = swb_offsets(!short, fs_index);
    let window_len = if short { SHORT_HALF } else { FRAME_LEN };
    let tns_max = if short {
        TNS_MAX_SHORT[fs_index as usize]
    } else {
        TNS_MAX_LONG[fs_index as usize]
    } as usize;
    let mmm = tns_max.min(info.max_sfb as usize);

    for (w, filters) in tns.windows.iter().enumerate() {
        let mut bottom = info.num_swb;
        for f in filters {
            let top = bottom;
            bottom = top.saturating_sub(f.length);
            if f.order == 0 {
                continue;
            }
            let start = swb[bottom.min(mmm)] as usize;
            let end = swb[top.min(mmm)] as usize;
            if end <= start {
                continue;
            }
            let size = end - start;
            let base = w * window_len;
            let (mut idx, inc): (isize, isize) = if f.direction {
                ((base + end - 1) as isize, -1)
            } else {
                ((base + start) as isize, 1)
            };
            let mut state = vec![0f32; f.order];
            for _ in 0..size {
                let p = idx as usize;
                let mut y = spec[p];
                for j in 0..f.order {
                    y -= state[j] * f.lpc[j + 1];
                }
                for j in (1..f.order).rev() {
                    state[j] = state[j - 1];
                }
                state[0] = y;
                spec[p] = y;
                idx += inc;
            }
        }
    }
}

/// M/S stereo flags (`num_window_groups × max_sfb`) for a common-window CPE.
fn read_ms_used(r: &mut BitReader, info: &IcsInfo) -> Result<Vec<bool>> {
    let n = info.num_window_groups * info.max_sfb as usize;
    match r.read_bits(2)? {
        0 => Ok(vec![false; n]),
        2 => Ok(vec![true; n]),
        1 => {
            let mut v = vec![false; n];
            for slot in v.iter_mut() {
                *slot = r.read_bool()?;
            }
            Ok(v)
        }
        _ => Err(Error::unsupported("aac: reserved ms_mask_present")),
    }
}

/// Reconstruct L/R from Mid/Side on the flagged bands (group/window aware).
/// Intensity-coded bands of the right channel are excluded (handled by IS).
fn apply_ms(
    info: &IcsInfo,
    ms_used: &[bool],
    fs_index: u8,
    cb1: &[Vec<u8>],
    s0: &mut [f32; FRAME_LEN],
    s1: &mut [f32; FRAME_LEN],
) {
    if ms_used.is_empty() {
        return;
    }
    let is_short = info.window_sequence == WindowSequence::EightShort;
    let swb = swb_offsets(!is_short, fs_index);
    let window_len = if is_short { SHORT_HALF } else { FRAME_LEN };
    let max_sfb = info.max_sfb as usize;
    let mut wbase = 0usize;
    for g in 0..info.num_window_groups {
        for sfb in 0..max_sfb {
            if ms_used[g * max_sfb + sfb] && cb1[g][sfb] < INTENSITY_HCB2 {
                let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                for w in 0..info.window_group_length[g] as usize {
                    let base = (wbase + w) * window_len;
                    for i in s..e {
                        let (mid, side) = (s0[base + i], s1[base + i]);
                        s0[base + i] = mid + side;
                        s1[base + i] = mid - side;
                    }
                }
            }
        }
        wbase += info.window_group_length[g] as usize;
    }
}

/// Intensity stereo (ISO 14496-3 §4.6.8.2.3): the right channel's IS bands are
/// `± 2^(-is_position/4)` times the left channel. Sign is negative for
/// INTENSITY_HCB2, and flipped again when the band is M/S-flagged.
fn apply_is(
    info: &IcsInfo,
    ms_used: &[bool],
    fs_index: u8,
    cb1: &[Vec<u8>],
    sf1: &[Vec<i32>],
    s0: &[f32; FRAME_LEN],
    s1: &mut [f32; FRAME_LEN],
) {
    let is_short = info.window_sequence == WindowSequence::EightShort;
    let swb = swb_offsets(!is_short, fs_index);
    let window_len = if is_short { SHORT_HALF } else { FRAME_LEN };
    let max_sfb = info.max_sfb as usize;
    let mut wbase = 0usize;
    for g in 0..info.num_window_groups {
        for sfb in 0..max_sfb {
            let cb = cb1[g][sfb];
            if cb >= INTENSITY_HCB2 {
                let mut scale = 0.5f32.powf(0.25 * sf1[g][sfb] as f32);
                if cb == INTENSITY_HCB2 {
                    scale = -scale; // out of phase
                }
                if !ms_used.is_empty() && ms_used[g * max_sfb + sfb] {
                    scale = -scale; // M/S flips intensity sign
                }
                let (s, e) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                for w in 0..info.window_group_length[g] as usize {
                    let base = (wbase + w) * window_len;
                    for i in s..e {
                        s1[base + i] = scale * s0[base + i];
                    }
                }
            }
        }
        wbase += info.window_group_length[g] as usize;
    }
}

fn interleave(outputs: Vec<Vec<f32>>, sample_rate: u32, pts: Option<i64>) -> Frame {
    let nch = outputs.len();
    let mut bytes = Vec::with_capacity(FRAME_LEN * nch * 4);
    for n in 0..FRAME_LEN {
        for ch in &outputs {
            bytes.extend_from_slice(&ch[n].to_le_bytes());
        }
    }
    Frame::Audio(AudioFrame {
        sample_rate,
        channels: nch as u16,
        format: SampleFormat::F32,
        planes: vec![bytes],
        samples: FRAME_LEN,
        pts,
    })
}

fn fs_index_for(rate: u32) -> u8 {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
        .unwrap_or(3) // default 48 kHz layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_and_scalefactors_parse() {
        // One group, one cb=1 section of length 2 (long, 5-bit length).
        // bits: cb(0001) len_incr(00010) → 0001_0001 0... = 0x11 0x00
        let mut r = BitReader::new(&[0x11, 0x00]);
        let cb = read_sections(&mut r, 1, 2, false).unwrap();
        assert_eq!(cb, vec![vec![1, 1]]);
    }

    #[test]
    fn scalefactors_accumulate_from_global_gain() {
        // Two regular bands; SF deltas both index 60 ("0" = delta 0) → both
        // equal global_gain. Two "0" bits → 0x00.
        let sfb_cb = vec![vec![1u8, 1u8]];
        let mut r = BitReader::new(&[0x00]);
        let sf = read_scalefactors(&mut r, &sfb_cb, 100).unwrap();
        assert_eq!(sf, vec![vec![100, 100]]);
    }

    /// End-to-end: a hand-built ONLY_LONG SCE frame, one cb1 codeword giving a
    /// single non-zero coefficient. Decoded PCM must equal the IMDCT of that
    /// spectrum, windowed and normalised — using the verified DSP as oracle.
    #[test]
    fn hand_built_frame_matches_independent_imdct() {
        let au = [0x00, 0xC8, 0x00, 0x84, 0x21, 0x1E];
        let mut dec = Decoder::new(48_000);
        let frame = dec.decode(&au, Some(0)).unwrap();
        let Frame::Audio(af) = frame else {
            panic!("expected audio")
        };
        assert_eq!(af.channels, 1);
        assert_eq!(af.samples, FRAME_LEN);
        let got: Vec<f32> = af.planes[0]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();

        let mut spec = [0f32; FRAME_LEN];
        spec[0] = -1.0;
        let time = dsp::imdct(&spec);
        let win = dsp::sine_window(LONG_N);
        for n in 0..FRAME_LEN {
            let expected = time[n] * win[n] / 32768.0;
            assert!(
                (got[n] - expected).abs() < 1e-4,
                "sample {n}: got {} want {}",
                got[n],
                expected
            );
        }
    }
}
