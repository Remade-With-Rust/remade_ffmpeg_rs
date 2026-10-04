//! The full **`AudioSpecificConfig`** (ISO/IEC 14496-3 §1.6.2.1) and everything it
//! can carry: `GASpecificConfig` (§4.4.1), the `program_config_element` (§4.4.1.1),
//! `ELDSpecificConfig` (§4.4.1.2) with its low-delay SBR headers, the error
//! protection selector, and both SBR/PS signalling forms.
//!
//! This is the single place a decoder learns *what it is decoding*: the core object
//! type, the core and output sample rates, the frame length (1024/960 for GA,
//! 512/480 for LD/ELD), the channel layout (a `channelConfiguration` or a PCE), the
//! error-resilience flags, and whether SBR/PS ride on top.
//!
//! The field order in the hierarchical (AOT 5/29) form is the classic trap: the
//! FIRST `samplingFrequencyIndex` is the **core** rate and the
//! `extensionSamplingFrequencyIndex` follows `channelConfiguration`. Reading it the
//! other way round decodes the core at the wrong rate and parses the
//! `GASpecificConfig` four bits early.

use crate::bits::BitReader;
use crate::{Error, Result};

/// Audio object types this crate distinguishes (ISO 14496-3 Table 1.17).
pub mod aot {
    pub const AAC_MAIN: u8 = 1;
    pub const AAC_LC: u8 = 2;
    pub const AAC_SSR: u8 = 3;
    pub const AAC_LTP: u8 = 4;
    pub const SBR: u8 = 5;
    pub const AAC_SCALABLE: u8 = 6;
    pub const ER_AAC_LC: u8 = 17;
    pub const ER_AAC_LTP: u8 = 19;
    pub const ER_AAC_SCALABLE: u8 = 20;
    pub const ER_BSAC: u8 = 22;
    pub const ER_AAC_LD: u8 = 23;
    pub const PS: u8 = 29;
    pub const ER_AAC_ELD: u8 = 39;
    pub const USAC: u8 = 42;
}

/// The standard sampling-frequency table (`samplingFrequencyIndex` 0..12).
pub const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// The table index a decoder uses for a rate (ISO 14496-3 Table 4.82): exact rates
/// map to themselves, an escaped non-standard rate to the nearest table set.
pub fn sf_index_for_any_rate(rate: u32) -> u8 {
    if let Some(i) = SAMPLE_RATES.iter().position(|&r| r == rate) {
        return i as u8;
    }
    const EDGES: [u32; 11] = [
        92017, 75132, 55426, 46009, 37566, 27713, 23004, 18783, 13856, 11502, 9391,
    ];
    EDGES.iter().position(|&e| rate >= e).unwrap_or(11) as u8
}

/// One element slot of a program configuration: an SCE/CPE/LFE/CCE/DSE tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PceElement {
    /// `true` for a CPE (two channels) in the front/side/back lists.
    pub is_cpe: bool,
    pub tag: u8,
}

/// Where a PCE element sits in the speaker layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Front,
    Side,
    Back,
    Lfe,
}

/// `program_config_element` (§4.4.1.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pce {
    pub element_instance_tag: u8,
    pub object_type: u8,
    pub sf_index: u8,
    pub front: Vec<PceElement>,
    pub side: Vec<PceElement>,
    pub back: Vec<PceElement>,
    pub lfe: Vec<u8>,
    pub assoc_data: Vec<u8>,
    /// Coupling channel elements: (`is_ind_sw`, tag).
    pub cc: Vec<(bool, u8)>,
    pub mono_mixdown: Option<u8>,
    pub stereo_mixdown: Option<u8>,
    /// (`matrix_mixdown_idx`, `pseudo_surround_enable`).
    pub matrix_mixdown: Option<(u8, bool)>,
    pub comment: Vec<u8>,
}

impl Pce {
    /// Total output channels the program describes.
    pub fn channels(&self) -> usize {
        let pairs = |v: &[PceElement]| v.iter().map(|e| 1 + e.is_cpe as usize).sum::<usize>();
        pairs(&self.front) + pairs(&self.side) + pairs(&self.back) + self.lfe.len()
    }

    /// Parse a PCE. `align_base` is the bit position `byte_alignment()` is
    /// relative to (the start of the enclosing config or access unit).
    pub fn parse(r: &mut BitReader, align_base: usize) -> Result<Pce> {
        let tag = r.read_bits(4)? as u8;
        Pce::parse_after_tag(r, tag, align_base)
    }

    /// Parse a PCE whose `element_instance_tag` was already read (the in-band
    /// form, where the tag rides in the `raw_data_block` element header).
    pub fn parse_after_tag(r: &mut BitReader, tag: u8, align_base: usize) -> Result<Pce> {
        let mut p = Pce {
            element_instance_tag: tag,
            object_type: r.read_bits(2)? as u8,
            sf_index: r.read_bits(4)? as u8,
            ..Default::default()
        };
        let nf = r.read_bits(4)? as usize;
        let ns = r.read_bits(4)? as usize;
        let nb = r.read_bits(4)? as usize;
        let nl = r.read_bits(2)? as usize;
        let na = r.read_bits(3)? as usize;
        let nc = r.read_bits(4)? as usize;
        if r.read_bool()? {
            p.mono_mixdown = Some(r.read_bits(4)? as u8);
        }
        if r.read_bool()? {
            p.stereo_mixdown = Some(r.read_bits(4)? as u8);
        }
        if r.read_bool()? {
            let idx = r.read_bits(2)? as u8;
            p.matrix_mixdown = Some((idx, r.read_bool()?));
        }
        let list = |r: &mut BitReader, n: usize| -> Result<Vec<PceElement>> {
            (0..n)
                .map(|_| {
                    Ok(PceElement {
                        is_cpe: r.read_bool()?,
                        tag: r.read_bits(4)? as u8,
                    })
                })
                .collect()
        };
        p.front = list(r, nf)?;
        p.side = list(r, ns)?;
        p.back = list(r, nb)?;
        for _ in 0..nl {
            p.lfe.push(r.read_bits(4)? as u8);
        }
        for _ in 0..na {
            p.assoc_data.push(r.read_bits(4)? as u8);
        }
        for _ in 0..nc {
            let ind = r.read_bool()?;
            p.cc.push((ind, r.read_bits(4)? as u8));
        }
        // byte_alignment() relative to the enclosing structure's start.
        let used = r.position() - align_base;
        if used % 8 != 0 {
            r.skip(8 - used % 8)?;
        }
        let n = r.read_bits(8)? as usize;
        for _ in 0..n {
            p.comment.push(r.read_bits(8)? as u8);
        }
        Ok(p)
    }
}

/// An SBR header as carried in `ELDSpecificConfig` (`ld_sbr_header`) or in-band
/// (§4.4.2.8 `sbr_header`). Fields keep their bitstream names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SbrHeader {
    pub bs_amp_res: u8,
    pub bs_start_freq: u8,
    pub bs_stop_freq: u8,
    pub bs_xover_band: u8,
    pub bs_freq_scale: u8,
    pub bs_alter_scale: u8,
    pub bs_noise_bands: u8,
    pub bs_limiter_bands: u8,
    pub bs_limiter_gains: u8,
    pub bs_interpol_freq: u8,
    pub bs_smoothing_mode: u8,
}

impl SbrHeader {
    /// Parse `sbr_header()`; absent optional groups take their spec defaults.
    pub fn parse(r: &mut BitReader) -> Result<SbrHeader> {
        let mut h = SbrHeader {
            bs_amp_res: r.read_bits(1)? as u8,
            bs_start_freq: r.read_bits(4)? as u8,
            bs_stop_freq: r.read_bits(4)? as u8,
            bs_xover_band: r.read_bits(3)? as u8,
            ..SbrHeader::defaults()
        };
        let _reserved = r.read_bits(2)?;
        let extra1 = r.read_bool()?;
        let extra2 = r.read_bool()?;
        if extra1 {
            h.bs_freq_scale = r.read_bits(2)? as u8;
            h.bs_alter_scale = r.read_bits(1)? as u8;
            h.bs_noise_bands = r.read_bits(2)? as u8;
        }
        if extra2 {
            h.bs_limiter_bands = r.read_bits(2)? as u8;
            h.bs_limiter_gains = r.read_bits(2)? as u8;
            h.bs_interpol_freq = r.read_bits(1)? as u8;
            h.bs_smoothing_mode = r.read_bits(1)? as u8;
        }
        Ok(h)
    }

    /// Defaults the spec assigns when `bs_header_extra_1/2` are 0.
    pub fn defaults() -> SbrHeader {
        SbrHeader {
            bs_amp_res: 1,
            bs_freq_scale: 2,
            bs_alter_scale: 1,
            bs_noise_bands: 2,
            bs_limiter_bands: 2,
            bs_limiter_gains: 2,
            bs_interpol_freq: 1,
            bs_smoothing_mode: 1,
            ..Default::default()
        }
    }
}

/// `ELDSpecificConfig` (§4.4.1.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EldConfig {
    /// Low-delay SBR rides on the ELD core.
    pub ld_sbr: bool,
    /// `ldSbrSamplingRate`: 1 = dual-rate SBR (output 2x core), 0 = single-rate.
    pub ld_sbr_dual_rate: bool,
    pub ld_sbr_crc: bool,
    /// One `ld_sbr_header` per SBR element (count from the channel configuration).
    pub sbr_headers: Vec<SbrHeader>,
}

/// Everything an `AudioSpecificConfig` says about a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    /// The CORE audio object type (5/29 unwrapped to what they extend).
    pub object_type: u8,
    /// Core `samplingFrequencyIndex` as used for table lookups (escaped rates are
    /// mapped to the nearest table, ISO Table 4.82).
    pub sf_index: u8,
    /// Core sample rate.
    pub sample_rate: u32,
    pub channel_config: u8,
    /// Present when `channel_config == 0` (and possibly also in-band).
    pub pce: Option<Pce>,
    /// Samples per channel per frame of the core: 1024/960 (GA), 512/480 (LD/ELD).
    pub frame_length: usize,
    pub depends_on_core_coder: bool,
    pub core_coder_delay: u16,
    pub section_data_resilience: bool,
    pub scalefactor_data_resilience: bool,
    pub spectral_data_resilience: bool,
    /// `epConfig` (ER object types only; 0 = no error protection).
    pub ep_config: u8,
    /// SBR explicitly signalled present (hierarchical or backward-compatible).
    pub sbr: bool,
    /// SBR explicitly signalled ABSENT (`sbrPresentFlag = 0`): implicit SBR in the
    /// fill elements must then be ignored.
    pub sbr_explicitly_absent: bool,
    pub ps: bool,
    /// Output rate after SBR (equals `sample_rate` when there is no SBR).
    pub ext_sample_rate: u32,
    /// An extension rate was signalled even though SBR presence stays
    /// implicit (a sync extension naming the core rate). If SBR then appears
    /// it runs downsampled, at this rate.
    pub ext_rate_signalled: bool,
    pub eld: Option<EldConfig>,
}

impl StreamConfig {
    /// A plain AAC-LC configuration (what an ADTS header or the simple
    /// `AudioSpecificConfig` describes).
    pub fn lc(object_type: u8, sample_rate: u32, channel_config: u8) -> StreamConfig {
        StreamConfig {
            object_type,
            sf_index: sf_index_for_any_rate(sample_rate),
            sample_rate,
            channel_config,
            pce: None,
            frame_length: 1024,
            depends_on_core_coder: false,
            core_coder_delay: 0,
            section_data_resilience: false,
            scalefactor_data_resilience: false,
            spectral_data_resilience: false,
            ep_config: 0,
            sbr: false,
            sbr_explicitly_absent: false,
            ps: false,
            ext_sample_rate: sample_rate,
            ext_rate_signalled: false,
            eld: None,
        }
    }

    /// Is this an error-resilient (ER) object type, i.e. `er_raw_data_block`?
    pub fn is_er(&self) -> bool {
        matches!(self.object_type, 17 | 19 | 20 | 21 | 22 | 23 | 24 | 25 | 26 | 27 | 39)
    }

    /// Channels described by the configuration (0 if unknown).
    pub fn channels(&self) -> usize {
        if let Some(p) = &self.pce {
            return p.channels();
        }
        channels_for_config(self.channel_config)
    }
}

/// Output channel count of a `channelConfiguration` (ISO Table 1.19, plus the
/// 23003-3 additions 11-14).
pub fn channels_for_config(cc: u8) -> usize {
    match cc {
        1..=6 => cc as usize,
        7 => 8,
        11 => 7,
        12 | 14 => 8,
        13 => 24,
        _ => 0,
    }
}

fn read_aot(r: &mut BitReader) -> Result<u8> {
    let ot = r.read_bits(5)? as u8;
    Ok(if ot == 31 { 32 + r.read_bits(6)? as u8 } else { ot })
}

fn read_rate(r: &mut BitReader) -> Result<(u8, u32)> {
    let idx = r.read_bits(4)? as u8;
    if idx == 0x0F {
        let rate = r.read_bits(24)?;
        if rate == 0 {
            return Err(Error::invalid("aac config: zero explicit sampling rate"));
        }
        Ok((sf_index_for_any_rate(rate), rate))
    } else if (idx as usize) < SAMPLE_RATES.len() {
        Ok((idx, SAMPLE_RATES[idx as usize]))
    } else {
        Err(Error::invalid("aac config: reserved sampling frequency index"))
    }
}

/// Number of SBR elements (hence `ld_sbr_header`s) a channel configuration implies.
fn sbr_elements_for_config(cc: u8) -> usize {
    match cc {
        1 | 2 => 1,
        3 => 2,
        4 => 3,
        5 | 6 => 3,
        7 => 4,
        _ => 0,
    }
}

/// Parse a complete `AudioSpecificConfig` from its raw bytes.
pub fn parse(data: &[u8]) -> Result<StreamConfig> {
    let mut r = BitReader::new(data);
    parse_from(&mut r, 0)
}

/// Parse from a reader positioned at an `AudioSpecificConfig` (e.g. inside a LATM
/// `StreamMuxConfig`). `align_base` is the bit position the config starts at.
pub fn parse_from(r: &mut BitReader, align_base: usize) -> Result<StreamConfig> {
    parse_from_opts(r, align_base, true)
}

/// As [`parse_from`], choosing whether to look for the backward-compatible
/// SBR/PS sync extension after the core config. A config embedded in a LATM
/// `StreamMuxConfig` (audioMuxVersion 0) is followed directly by mux fields, so
/// probing there would read them as a sync word; the reference does not probe.
pub fn parse_from_opts(
    r: &mut BitReader,
    align_base: usize,
    sync_extension: bool,
) -> Result<StreamConfig> {
    let mut object_type = read_aot(r)?;
    let (sf_index, sample_rate) = read_rate(r)?;
    let channel_config = r.read_bits(4)? as u8;
    let mut c = StreamConfig::lc(object_type, sample_rate, channel_config);
    c.sf_index = sf_index;

    let mut ext_aot = 0u8;
    if object_type == aot::SBR || object_type == aot::PS {
        ext_aot = aot::SBR;
        c.sbr = true;
        c.ps = object_type == aot::PS;
        let (_, ext_rate) = read_rate(r)?;
        c.ext_sample_rate = ext_rate;
        object_type = read_aot(r)?;
        if object_type == aot::ER_BSAC {
            let _ext_channel_config = r.read_bits(4)?;
        }
    }
    c.object_type = object_type;

    match object_type {
        1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23 => {
            ga_specific_config(r, &mut c, align_base)?;
        }
        aot::ER_AAC_ELD => eld_specific_config(r, &mut c)?,
        aot::USAC => {
            return Err(Error::unsupported(
                "aac config: USAC (xHE-AAC) is a separate codec (ISO 23003-3)",
            ))
        }
        other => {
            return Err(Error::unsupported(format!(
                "aac config: audio object type {other} is not an AAC-family type"
            )))
        }
    }

    if matches!(object_type, 17 | 19 | 20 | 21 | 22 | 23 | 24 | 25 | 26 | 27 | 39) {
        c.ep_config = r.read_bits(2)? as u8;
        if c.ep_config >= 2 {
            return Err(Error::unsupported(
                "aac config: ErrorProtectionSpecificConfig (epConfig 2/3) not supported",
            ));
        }
    }

    // Backward-compatible SBR/PS signalling after the core config.
    // The sync word may follow padding, so it is searched bit by bit.
    // (Hierarchical signalling with an extension rate equal to the core rate
    // is downsampled SBR: the output stays at the core rate.)
    while sync_extension && ext_aot != aot::SBR && r.bits_left() > 15 {
        if r.peek_bits(11) != 0x2B7 {
            r.skip(1)?;
            continue;
        }
        r.skip(11)?;
        let e = read_aot(r)?;
        if e == aot::SBR {
            if r.read_bool()? {
                let (_, ext_rate) = read_rate(r)?;
                if ext_rate == c.sample_rate {
                    // No distinct rate: SBR presence stays implicit.
                    c.ext_sample_rate = c.sample_rate;
                    c.ext_rate_signalled = true;
                } else {
                    c.sbr = true;
                    c.ext_sample_rate = ext_rate;
                }
            } else {
                c.sbr_explicitly_absent = true;
            }
        }
        if r.bits_left() > 11 && r.read_bits(11)? == 0x548 {
            c.ps = r.read_bool()?;
        }
        break;
    }
    if c.sbr_explicitly_absent {
        c.ps = false;
    }
    if let Some(e) = &c.eld {
        if e.ld_sbr {
            c.sbr = true;
            c.ext_sample_rate = if e.ld_sbr_dual_rate { c.sample_rate * 2 } else { c.sample_rate };
        }
    }
    Ok(c)
}

fn ga_specific_config(r: &mut BitReader, c: &mut StreamConfig, align_base: usize) -> Result<()> {
    let frame_length_flag = r.read_bool()?;
    c.frame_length = match (c.object_type, frame_length_flag) {
        (aot::ER_AAC_LD, false) => 512,
        (aot::ER_AAC_LD, true) => 480,
        (_, false) => 1024,
        (_, true) => 960,
    };
    c.depends_on_core_coder = r.read_bool()?;
    if c.depends_on_core_coder {
        c.core_coder_delay = r.read_bits(14)? as u16;
    }
    let extension_flag = r.read_bool()?;
    if c.channel_config == 0 {
        c.pce = Some(Pce::parse(r, align_base)?);
    }
    if c.object_type == aot::AAC_SCALABLE || c.object_type == aot::ER_AAC_SCALABLE {
        let _layer_nr = r.read_bits(3)?;
    }
    if extension_flag {
        if c.object_type == aot::ER_BSAC {
            let _num_of_sub_frame = r.read_bits(5)?;
            let _layer_length = r.read_bits(11)?;
        }
        if matches!(c.object_type, 17 | 19 | 20 | 23) {
            c.section_data_resilience = r.read_bool()?;
            c.scalefactor_data_resilience = r.read_bool()?;
            c.spectral_data_resilience = r.read_bool()?;
        }
        let _extension_flag3 = r.read_bool()?;
    }
    Ok(())
}

fn eld_specific_config(r: &mut BitReader, c: &mut StreamConfig) -> Result<()> {
    c.frame_length = if r.read_bool()? { 480 } else { 512 };
    c.section_data_resilience = r.read_bool()?;
    c.scalefactor_data_resilience = r.read_bool()?;
    c.spectral_data_resilience = r.read_bool()?;
    let mut e = EldConfig {
        ld_sbr: r.read_bool()?,
        ..Default::default()
    };
    if e.ld_sbr {
        e.ld_sbr_dual_rate = r.read_bool()?;
        e.ld_sbr_crc = r.read_bool()?;
        for _ in 0..sbr_elements_for_config(c.channel_config) {
            e.sbr_headers.push(SbrHeader::parse(r)?);
        }
    }
    loop {
        let ext_type = r.read_bits(4)?;
        if ext_type == 0 {
            break; // ELDEXT_TERM
        }
        let mut len = r.read_bits(4)? as usize;
        if len == 15 {
            len += r.read_bits(8)? as usize;
            if len == 15 + 255 {
                len += r.read_bits(16)? as usize;
            }
        }
        r.skip(8 * len)?;
    }
    c.eld = Some(e);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lc_stereo_44100() {
        let c = parse(&[0x12, 0x10]).unwrap();
        assert_eq!((c.object_type, c.sample_rate, c.channel_config), (2, 44100, 2));
        assert_eq!(c.frame_length, 1024);
        assert!(!c.sbr && !c.ps);
    }

    /// The real HE-AAC v2 config from the FATE CT_DecoderCheck corpus
    /// (`eb 8a 08 00`): AOT 29, CORE 22050 Hz, mono, extension 44100 Hz, core
    /// AOT 2. The previous parser read 44100 as the output and halved it to
    /// 11025 for the core — and then parsed the GASpecificConfig 4 bits early.
    #[test]
    fn hierarchical_ps_core_rate_comes_first() {
        let c = parse(&[0xEB, 0x8A, 0x08, 0x00]).unwrap();
        assert_eq!(c.object_type, aot::AAC_LC);
        assert_eq!(c.sample_rate, 22050);
        assert_eq!(c.ext_sample_rate, 44100);
        assert_eq!(c.channel_config, 1);
        assert!(c.sbr && c.ps);
        assert_eq!(c.frame_length, 1024);
    }

    /// The 960-sample-frame LC config from FATE `al04sf_48`.
    #[test]
    fn frame_length_flag_selects_960() {
        // AOT 2, sf 3 (48000), cc 1, frameLengthFlag 1, 0, 0 → 00010 0011 0001 100
        let c = parse(&[0b0001_0001, 0b1000_1100]).unwrap();
        assert_eq!(c.sample_rate, 48000);
        assert_eq!(c.frame_length, 960);
    }

    /// A PCE inside the config (channelConfiguration 0) — FATE `al07_96`.
    #[test]
    fn pce_in_config_al07() {
        let c = parse(&[0x10, 0x00, 0x04, 0x08, 0x05, 0x02, 0x01, 0x08, 0x80, 0x00, 0x00]).unwrap();
        assert_eq!(c.channel_config, 0);
        let p = c.pce.as_ref().unwrap();
        assert_eq!(c.channels(), 6, "{p:?}");
        assert_eq!(c.sample_rate, 96000);
    }

    #[test]
    fn non_standard_rate_maps_to_nearest_table() {
        assert_eq!(sf_index_for_any_rate(44100), 4);
        assert_eq!(sf_index_for_any_rate(50000), 3);
        assert_eq!(sf_index_for_any_rate(9000), 11);
    }
}
