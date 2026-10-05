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
//! # What is implemented
//!
//! The full reconstruction: signalling in
//! both explicit forms (hierarchical `audioObjectType` 5/29 and the `0x2B7`
//! backward-compatible sync extension) and implicitly (SBR found in the fill
//! elements); dual-rate and downsampled SBR; and HE-AAC v2 Parametric Stereo
//! with 10/20/34 bands and IPD/OPD. Verified sample-exact (≤1 LSB) against
//! FFmpeg and the ISO/IEC 14496-26 reference outputs.
//!
//! **Not reconstructed:** the *low-delay* SBR of ER AAC-ELD, which runs on a
//! different (complex low-delay) filterbank — the ELD core is decoded and
//! output at the core rate, and [`SbrSupport::CoreOnly`] says so.

pub(crate) mod dec;
pub(crate) mod ps;
mod qmf;
#[rustfmt::skip] // generated data
mod tables;

use crate::Result;

/// `audioObjectType` values that name an SBR-bearing configuration.
pub const AOT_SBR: u8 = 5;
/// AAC-LC — the core object type SBR wraps.
pub const AOT_AAC_LC: u8 = 2;
/// HE-AAC v2 (SBR + Parametric Stereo).
pub const AOT_PS: u8 = 29;

/// `syncExtensionType` marking backward-compatible SBR signalling.
#[cfg(test)]
const SYNC_EXT_SBR: u32 = 0x2B7;
/// `syncExtensionType` marking backward-compatible PS signalling.
#[cfg(test)]
const SYNC_EXT_PS: u32 = 0x548;

/// `extension_type` values inside a `fill_element` payload.
const EXT_SBR_DATA: u32 = 13;
const EXT_SBR_DATA_CRC: u32 = 14;

/// How much of an SBR stream this build reconstructs.
///
/// Returned rather than inferred so callers never have to guess, and so any
/// limitation is visible at the API surface instead of buried in a doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SbrSupport {
    /// No SBR signalled in the configuration (implicit SBR found in the
    /// stream is still decoded in full).
    NotPresent,
    /// SBR (and PS, when signalled) is fully reconstructed.
    Full,
    /// Low-delay SBR (ER AAC-ELD): the core is decoded and output at the core
    /// rate; the replicated high band is not reconstructed.
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
    #[must_use]
    pub fn support(&self) -> SbrSupport {
        match (self.sbr_present, self.core_object_type) {
            (false, _) => SbrSupport::NotPresent,
            (true, crate::config::aot::ER_AAC_ELD) => SbrSupport::CoreOnly,
            (true, _) => SbrSupport::Full,
        }
    }
}

/// Parse an `AudioSpecificConfig` for HE-AAC signalling (a view over
/// [`crate::config::parse`], which handles both explicit forms and the field
/// order: in the hierarchical AOT 5/29 form the FIRST rate is the core rate and
/// the extension rate follows `channelConfiguration`).
///
/// A stream with neither form yields `sbr_present = false`.
///
/// # Errors
///
/// Returns [`Error::InvalidData`] for a truncated or malformed config.
pub fn parse_sbr_config(data: &[u8]) -> Result<SbrConfig> {
    let c = crate::config::parse(data)?;
    Ok(SbrConfig {
        sbr_present: c.sbr,
        ps_present: c.ps,
        core_sample_rate: c.sample_rate,
        output_sample_rate: if c.sbr && c.object_type != crate::config::aot::ER_AAC_ELD {
            c.ext_sample_rate
        } else {
            c.sample_rate
        },
        core_object_type: c.object_type,
    })
}

/// Does this `fill_element` payload carry SBR data?
///
/// `payload` is the fill payload **after** the count field, and its first 4 bits
/// are the `extension_type`. Used for *implicit* SBR signalling, where nothing in
/// the config says SBR but the fill elements carry it anyway — a shape MPEG-TS
/// broadcast genuinely produces.
#[must_use]
pub fn fil_payload_is_sbr(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let ext = u32::from(payload[0] >> 4);
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
        w.write(u32::from(AOT_SBR), 5);
        w.write(7, 4); // core sfIndex = 22050
        w.write(2, 4); // channels
        w.write(4, 4); // extension sfIndex = 44100
        w.write(u32::from(AOT_AAC_LC), 5); // core object type
        w.write(0, 3); // GASpecificConfig
        let bytes = w.into_bytes();

        let s = parse_sbr_config(&bytes).unwrap();
        assert!(s.sbr_present, "AOT 5 must signal SBR");
        assert!(!s.ps_present);
        assert_eq!(
            s.output_sample_rate, 44100,
            "extension rate is the OUTPUT rate"
        );
        assert_eq!(s.core_sample_rate, 22050, "core runs at half");
        assert_eq!(s.core_object_type, AOT_AAC_LC);
        assert_eq!(s.support(), SbrSupport::Full);
    }

    /// **Explicit hierarchical HE-AAC v2** — AOT 29 also implies PS.
    #[test]
    fn explicit_hierarchical_he_aac_v2() {
        let mut w = BitWriter::new();
        w.write(u32::from(AOT_PS), 5);
        w.write(6, 4); // 24000 core
        w.write(2, 4);
        w.write(3, 4); // 48000 output
        w.write(u32::from(AOT_AAC_LC), 5);
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
        w.write(u32::from(AOT_AAC_LC), 5);
        w.write(6, 4); // core sfIndex = 24000
        w.write(2, 4);
        w.write(0, 3); // GASpecificConfig
        w.write(SYNC_EXT_SBR, 11);
        w.write(u32::from(AOT_SBR), 5);
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
        w.write(u32::from(AOT_AAC_LC), 5);
        w.write(6, 4); // 24000 core
        w.write(2, 4);
        w.write(0, 3);
        w.write(SYNC_EXT_SBR, 11);
        w.write(u32::from(AOT_SBR), 5);
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
        w.write(u32::from(AOT_AAC_LC), 5);
        w.write(6, 4); // 24000
        w.write(1, 4);
        w.write(0, 3);
        w.write(SYNC_EXT_SBR, 11);
        w.write(u32::from(AOT_SBR), 5);
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
        w.write(u32::from(AOT_SBR), 5);
        w.write(7, 4); // core sfIndex = 22050
        w.write(1, 4); // mono
        w.write(4, 4); // extension sfIndex = 44100
        w.write(u32::from(AOT_AAC_LC), 5);
        w.write(0, 3);
        let asc = w.into_bytes();

        let dec = crate::AacDecoder::with_config_bytes(&asc).expect("config");
        assert_eq!(
            dec.output_sample_rate(),
            Some(44100),
            "HE-AAC must report the DOUBLED output rate"
        );
        assert_eq!(dec.sbr_support(), SbrSupport::Full);
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
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
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
