//! **HE-AAC (SBR / PS)** — signalling, detection, and honest capability
//! reporting.
//!
//! HE-AAC v1 wraps an AAC-LC core with **SBR** (Spectral Band Replication): the
//! core codes the lower half of the spectrum at half the output rate, and SBR
//! reconstructs the top half from it plus a small parameter stream. HE-AAC v2
//! adds **PS** (Parametric Stereo), coding a mono core plus stereo parameters.
//!
//! This is what broadcast and low-bitrate streaming actually use, so a decoder
//! that mishandles it mishandles the majority of real AAC in the wild.
//!
//! # What is implemented here, and what is not
//!
//! **Implemented, exactly:** the signalling. Both forms — *explicit hierarchical*
//! (`audioObjectType` 5 or 29) and *explicit backward-compatible* (the `0x2B7`
//! sync extension after the `GASpecificConfig`) — plus detection of SBR payload
//! in the `fill_element`, and the **output sample rate**, which for HE-AAC is
//! twice the core rate.
//!
//! **NOT implemented:** the SBR reconstruction itself. See [`SbrSupport`]. The
//! blocker is not design effort — it is that the tool is defined by large
//! normative tables (the 640-tap QMF prototype filter, ISO/IEC 14496-3
//! Table 4.A.87, and the SBR envelope/noise Huffman codebooks). Those must be
//! transcribed from the specification or a reference implementation. Writing
//! *approximations* of them would produce a decoder that is subtly wrong
//! everywhere while appearing to work, which is strictly worse than not having
//! it — the same reason `decode` refuses `gain_control_data` rather than
//! guessing at it.
//!
//! # Why the signalling alone is worth having
//!
//! Without it an HE-AAC stream is not merely "missing its high band" — it is
//! **silently played at the wrong speed**. The config announces a 24 kHz core for
//! 48 kHz output; a decoder that reports 24 kHz hands the player half-rate audio.
//! With this module the core decodes correctly, the caller learns the true output
//! rate, and [`SbrSupport`] says plainly that the high band is absent instead of
//! pretending otherwise.

use crate::bits::BitReader;
use crate::{Error, Result};

/// `audioObjectType` values that name an SBR-bearing configuration.
pub const AOT_SBR: u8 = 5;
/// AAC-LC — the core object type SBR wraps.
pub const AOT_AAC_LC: u8 = 2;
/// HE-AAC v2 (SBR + Parametric Stereo).
pub const AOT_PS: u8 = 29;
/// ER-BSAC, the one object type that carries `extensionChannelConfiguration`.
const AOT_ER_BSAC: u8 = 22;

/// `syncExtensionType` marking backward-compatible SBR signalling.
const SYNC_EXT_SBR: u32 = 0x2B7;
/// `syncExtensionType` marking backward-compatible PS signalling.
const SYNC_EXT_PS: u32 = 0x548;

/// `extension_type` values inside a `fill_element` payload.
const EXT_SBR_DATA: u32 = 13;
const EXT_SBR_DATA_CRC: u32 = 14;

/// How much of an SBR stream this build can actually reconstruct.
///
/// Returned rather than inferred so callers never have to guess, and so the
/// limitation is visible at the API surface instead of buried in a doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbrSupport {
    /// No SBR in this stream; plain AAC-LC decoding is complete and exact.
    NotPresent,
    /// SBR is present. The **core is decoded correctly** and the output sample
    /// rate is reported correctly, but the replicated high band is not
    /// reconstructed — the result is band-limited to the core's Nyquist.
    ///
    /// This is the standard "core-only" fallback, and it is audio rather than
    /// silence or an error. It is not conformant HE-AAC decoding.
    CoreOnly,
}

/// HE-AAC parameters recovered from an `AudioSpecificConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SbrConfig {
    /// SBR is signalled in the config.
    pub sbr_present: bool,
    /// Parametric Stereo is signalled (HE-AAC v2).
    pub ps_present: bool,
    /// The **core** AAC sample rate — what the `raw_data_block`s are coded at.
    pub core_sample_rate: u32,
    /// The **output** sample rate after SBR, normally `2 x core_sample_rate`.
    pub output_sample_rate: u32,
    /// The core object type beneath the extension (2 for HE-AAC over AAC-LC).
    pub core_object_type: u8,
}

impl SbrConfig {
    /// What this build can do with the stream.
    pub fn support(&self) -> SbrSupport {
        if self.sbr_present {
            SbrSupport::CoreOnly
        } else {
            SbrSupport::NotPresent
        }
    }
}

/// Read a (possibly escaped) 5-bit `audioObjectType`.
fn read_aot(r: &mut BitReader) -> Result<u8> {
    let ot = r.read_bits(5)? as u8;
    if ot == 31 {
        Ok((32 + r.read_bits(6)?) as u8)
    } else {
        Ok(ot)
    }
}

/// Read a `samplingFrequencyIndex`, honouring the 0x0F escape.
fn read_sampling_frequency(r: &mut BitReader) -> Result<u32> {
    let idx = r.read_bits(4)? as u8;
    let rate = if idx == 0x0F {
        r.read_bits(24)?
    } else {
        crate::sample_rate_for_index(idx)
    };
    if rate == 0 {
        return Err(Error::invalid("sbr: reserved sampling frequency index"));
    }
    Ok(rate)
}

/// Parse an `AudioSpecificConfig` for HE-AAC signalling.
///
/// Handles both forms the standard defines, because real streams use both:
///
/// * **Explicit hierarchical** — `audioObjectType` is 5 (SBR) or 29 (PS), and the
///   extension rate and true core object type follow immediately. A legacy AAC-LC
///   decoder cannot read these at all.
/// * **Explicit backward-compatible** — `audioObjectType` is 2 (AAC-LC), and a
///   `0x2B7` sync extension *after* the `GASpecificConfig` announces SBR. Legacy
///   decoders skip the extension and decode the core, which is precisely the
///   point.
///
/// A stream with neither yields `sbr_present = false`.
pub fn parse_sbr_config(data: &[u8]) -> Result<SbrConfig> {
    let mut r = BitReader::new(data);
    let mut aot = read_aot(&mut r)?;
    let core_rate_first = read_sampling_frequency(&mut r)?;
    let _channels = r.read_bits(4)?;

    let mut cfg = SbrConfig {
        core_sample_rate: core_rate_first,
        output_sample_rate: core_rate_first,
        core_object_type: aot,
        ..Default::default()
    };

    let hierarchical = aot == AOT_SBR || aot == AOT_PS;
    if hierarchical {
        cfg.sbr_present = true;
        cfg.ps_present = aot == AOT_PS;
        // In this form the rate read above is the EXTENSION (output) rate, and
        // the core rate follows the inner object type. Getting these the wrong
        // way round is the classic half-speed/double-speed HE-AAC bug.
        cfg.output_sample_rate = core_rate_first;
        aot = read_aot(&mut r)?;
        cfg.core_object_type = aot;
        if aot == AOT_ER_BSAC {
            let _ext_channels = r.read_bits(4)?;
        }
        // The core rate is not restated; for every real HE-AAC stream it is half
        // the extension rate.
        cfg.core_sample_rate = core_rate_first / 2;
    }

    // GASpecificConfig (AAC-LC): three flags.
    if aot == AOT_AAC_LC {
        let frame_length_flag = r.read_bool()?;
        if frame_length_flag {
            return Err(Error::unsupported("sbr: 960-sample frames not supported"));
        }
        if r.read_bool()? {
            return Err(Error::unsupported("sbr: core-coder delay not supported"));
        }
        let _extension_flag = r.read_bool()?;
    }

    // Backward-compatible signalling: only meaningful when the hierarchical form
    // was NOT used.
    if !hierarchical && r.bits_left() >= 16 {
        let sync = r.read_bits(11)?;
        if sync == SYNC_EXT_SBR {
            let ext_aot = read_aot(&mut r)?;
            if ext_aot == AOT_SBR {
                let sbr_present = r.read_bool()?;
                if sbr_present {
                    cfg.sbr_present = true;
                    cfg.output_sample_rate = read_sampling_frequency(&mut r)?;
                    if r.bits_left() >= 12 {
                        let sync2 = r.read_bits(11)?;
                        if sync2 == SYNC_EXT_PS {
                            cfg.ps_present = r.read_bool()?;
                        }
                    }
                }
            }
        }
    }

    // A stream signalling SBR but no explicit output rate implies doubling.
    if cfg.sbr_present && cfg.output_sample_rate == cfg.core_sample_rate {
        cfg.output_sample_rate = cfg.core_sample_rate * 2;
    }
    Ok(cfg)
}

/// Does this `fill_element` payload carry SBR data?
///
/// `payload` is the fill payload **after** the count field, and its first 4 bits
/// are the `extension_type`. Used for *implicit* SBR signalling, where nothing in
/// the config says SBR but the fill elements carry it anyway — a shape MPEG-TS
/// broadcast genuinely produces.
pub fn fil_payload_is_sbr(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let ext = (payload[0] >> 4) as u32;
    ext == EXT_SBR_DATA || ext == EXT_SBR_DATA_CRC
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::BitWriter;

    /// Plain AAC-LC: no SBR, rates equal.
    #[test]
    fn plain_aac_lc_has_no_sbr() {
        // AOT 2, sfIndex 4 (44100), 2 channels, GASpecificConfig zeros.
        let cfg = crate::audio_specific_config_bytes(44100, 2);
        let s = parse_sbr_config(&cfg).unwrap();
        assert!(!s.sbr_present);
        assert!(!s.ps_present);
        assert_eq!(s.core_sample_rate, 44100);
        assert_eq!(s.output_sample_rate, 44100);
        assert_eq!(s.support(), SbrSupport::NotPresent);
    }

    /// **Explicit hierarchical HE-AAC v1.** AOT 5, extension rate 44100, core
    /// AOT 2 -> core runs at 22050. This is the case that silently plays at half
    /// speed if the two rates are swapped.
    #[test]
    fn explicit_hierarchical_he_aac_v1() {
        let mut w = BitWriter::new();
        w.write(AOT_SBR as u32, 5);
        w.write(4, 4); // extension sfIndex = 44100
        w.write(2, 4); // channels
        w.write(AOT_AAC_LC as u32, 5); // core object type
        w.write(0, 3); // GASpecificConfig
        let bytes = w.into_bytes();

        let s = parse_sbr_config(&bytes).unwrap();
        assert!(s.sbr_present, "AOT 5 must signal SBR");
        assert!(!s.ps_present);
        assert_eq!(s.output_sample_rate, 44100, "extension rate is the OUTPUT rate");
        assert_eq!(s.core_sample_rate, 22050, "core runs at half");
        assert_eq!(s.core_object_type, AOT_AAC_LC);
        assert_eq!(s.support(), SbrSupport::CoreOnly);
    }

    /// **Explicit hierarchical HE-AAC v2** — AOT 29 also implies PS.
    #[test]
    fn explicit_hierarchical_he_aac_v2() {
        let mut w = BitWriter::new();
        w.write(AOT_PS as u32, 5);
        w.write(3, 4); // 48000 output
        w.write(2, 4);
        w.write(AOT_AAC_LC as u32, 5);
        w.write(0, 3);
        let s = parse_sbr_config(&w.into_bytes()).unwrap();
        assert!(s.sbr_present && s.ps_present, "AOT 29 implies SBR + PS");
        assert_eq!(s.output_sample_rate, 48000);
        assert_eq!(s.core_sample_rate, 24000);
    }

    /// **Explicit backward-compatible signalling** — the form legacy decoders can
    /// still read. AOT 2 up front, then the 0x2B7 sync extension.
    #[test]
    fn backward_compatible_signalling() {
        let mut w = BitWriter::new();
        w.write(AOT_AAC_LC as u32, 5);
        w.write(6, 4); // core sfIndex = 24000
        w.write(2, 4);
        w.write(0, 3); // GASpecificConfig
        w.write(SYNC_EXT_SBR, 11);
        w.write(AOT_SBR as u32, 5);
        w.write_bool(true); // sbrPresentFlag
        w.write(3, 4); // extension sfIndex = 48000
        let s = parse_sbr_config(&w.into_bytes()).unwrap();
        assert!(s.sbr_present);
        assert!(!s.ps_present);
        assert_eq!(s.core_sample_rate, 24000);
        assert_eq!(s.output_sample_rate, 48000);
    }

    /// Backward-compatible PS signalling stacks a second sync extension.
    #[test]
    fn backward_compatible_ps_signalling() {
        let mut w = BitWriter::new();
        w.write(AOT_AAC_LC as u32, 5);
        w.write(6, 4); // 24000 core
        w.write(2, 4);
        w.write(0, 3);
        w.write(SYNC_EXT_SBR, 11);
        w.write(AOT_SBR as u32, 5);
        w.write_bool(true);
        w.write(3, 4); // 48000
        w.write(SYNC_EXT_PS, 11);
        w.write_bool(true); // psPresentFlag
        let s = parse_sbr_config(&w.into_bytes()).unwrap();
        assert!(s.sbr_present && s.ps_present);
        assert_eq!(s.output_sample_rate, 48000);
    }

    /// SBR signalled without an explicit extension rate implies doubling.
    #[test]
    fn sbr_without_explicit_rate_doubles() {
        let mut w = BitWriter::new();
        w.write(AOT_AAC_LC as u32, 5);
        w.write(6, 4); // 24000
        w.write(1, 4);
        w.write(0, 3);
        w.write(SYNC_EXT_SBR, 11);
        w.write(AOT_SBR as u32, 5);
        w.write_bool(false); // sbrPresentFlag = 0
        let s = parse_sbr_config(&w.into_bytes()).unwrap();
        assert!(!s.sbr_present, "an explicit 0 must not turn SBR on");
        assert_eq!(s.output_sample_rate, 24000);
    }

    /// Fill-element SBR payload detection, for implicit signalling.
    #[test]
    fn detects_sbr_fill_payload() {
        assert!(fil_payload_is_sbr(&[0xD0])); // EXT_SBR_DATA = 13
        assert!(fil_payload_is_sbr(&[0xE5])); // EXT_SBR_DATA_CRC = 14
        assert!(!fil_payload_is_sbr(&[0x10])); // EXT_FILL_DATA
        assert!(!fil_payload_is_sbr(&[0x00]));
        assert!(!fil_payload_is_sbr(&[]));
    }

    /// **The integration that matters.** A decoder built from raw HE-AAC config
    /// bytes must decode the CORE correctly and report the DOUBLED output rate.
    /// Built from the plain `AudioSpecificConfig` path it reports the core rate,
    /// and a player believing that runs the audio at half speed — which is the
    /// bug this module exists to prevent.
    #[test]
    fn decoder_reports_he_aac_output_rate() {
        // Explicit hierarchical HE-AAC v1: 44100 output, 22050 core.
        let mut w = BitWriter::new();
        w.write(AOT_SBR as u32, 5);
        w.write(4, 4); // extension sfIndex = 44100
        w.write(1, 4); // mono
        w.write(AOT_AAC_LC as u32, 5);
        w.write(0, 3);
        let asc = w.into_bytes();

        let dec = crate::AacDecoder::with_config_bytes(&asc).expect("config");
        assert_eq!(
            dec.output_sample_rate(),
            Some(44100),
            "HE-AAC must report the DOUBLED output rate"
        );
        assert_eq!(dec.sbr_support(), SbrSupport::CoreOnly);
        let s = dec.sbr_config().expect("sbr config");
        assert_eq!(s.core_sample_rate, 22050, "core decodes at half rate");

        // Plain AAC-LC through the same path is unaffected.
        let plain = crate::audio_specific_config_bytes(48000, 2);
        let d2 = crate::AacDecoder::with_config_bytes(&plain).expect("config");
        assert_eq!(d2.output_sample_rate(), Some(48000));
        assert_eq!(d2.sbr_support(), SbrSupport::NotPresent);
    }

    /// A real AAC-LC stream still round-trips through the new config path — the
    /// HE-AAC work must not disturb the overwhelmingly common case.
    #[test]
    fn plain_aac_still_decodes_through_the_new_path() {
        use crate::{AacEncoder, AacEncoderConfig};
        let sr = 44100u32;
        let n = 6 * 1024;
        let pcm: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect();
        let mut enc = AacEncoder::new(AacEncoderConfig::default());
        enc.push_pcm(&pcm, 1, sr).unwrap();
        enc.finish();

        let asc = crate::audio_specific_config_bytes(sr, 1);
        let mut dec = crate::AacDecoder::with_config_bytes(&asc).expect("config");
        let mut got = 0usize;
        while let Ok(p) = enc.next_packet() {
            got += dec.decode(&p.data, None).expect("decode").frames();
        }
        assert!(got >= n, "decoded {got} of {n}");
        assert_eq!(dec.sbr_support(), SbrSupport::NotPresent);
    }
}
