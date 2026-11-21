//! AAC configuration helpers shared by containers (MP4 `esds`, Matroska
//! CodecPrivate, ADTS): the AudioSpecificConfig for AAC-LC.

/// The sample rates a 4-bit samplingFrequencyIndex names (ISO 14496-3).
const RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// AAC sample-rate → 4-bit samplingFrequencyIndex (ISO 14496-3); 44.1 kHz default.
pub fn sampling_frequency_index(rate: u32) -> u32 {
    RATES.iter().position(|&r| r == rate).unwrap_or(4) as u32
}

/// AudioSpecificConfig for AAC-LC: objectType=2 (5b) + samplingFrequencyIndex (4b)
/// + channelConfiguration (4b) + GASpecificConfig (3b, all zero) = 16 bits.
///
/// A rate the index table does not name (192 kHz, say) takes the escape
/// index 15 and is written out in 24 bits, so it is never mislabelled.
pub fn audio_specific_config(sample_rate: u32, channels: u16) -> Vec<u8> {
    let ch = channels.clamp(1, 7) as u32;
    match RATES.iter().position(|&r| r == sample_rate) {
        Some(index) => {
            let bits = (2u32 << 11) | ((index as u32) << 7) | (ch << 3);
            vec![(bits >> 8) as u8, (bits & 0xff) as u8]
        }
        None => {
            // 5 + 4 + 24 + 4 + 3 = 40 bits.
            let rate = u64::from(sample_rate.min((1 << 24) - 1));
            let bits: u64 = (2u64 << 35) | (15u64 << 31) | (rate << 7) | (u64::from(ch) << 3);
            bits.to_be_bytes()[3..].to_vec()
        }
    }
}

/// The sampling rate an AudioSpecificConfig declares (the core rate: for
/// HE-AAC with implicit SBR, half the output rate). `None` if it is too short.
pub fn asc_sample_rate(asc: &[u8]) -> Option<u32> {
    let bit = |i: usize| -> Option<u32> { Some(u32::from(asc.get(i / 8)? >> (7 - i % 8) & 1)) };
    let read = |from: usize, n: usize| -> Option<u32> {
        (from..from + n).try_fold(0u32, |v, i| Some(v << 1 | bit(i)?))
    };
    let mut pos = 5;
    if read(0, 5)? == 31 {
        pos += 6; // audioObjectTypeExt
    }
    match read(pos, 4)? {
        15 => read(pos + 4, 24),
        i => RATES.get(i as usize).copied(),
    }
}

/// The sampling frequency index for `rate`, when the table names it.
pub fn exact_frequency_index(rate: u32) -> Option<u32> {
    RATES.iter().position(|&r| r == rate).map(|i| i as u32)
}

/// The SBR output rate an AudioSpecificConfig signals (HE-AAC): explicit
/// (audio object type 5 or 29, the extension rate right after the core
/// one) or backward compatible (the `0x2B7` sync extension after an AAC-LC
/// GASpecificConfig, as FFmpeg's Matroska demuxer synthesises it). `None`
/// for plain AAC.
pub fn asc_sbr_rate(asc: &[u8]) -> Option<u32> {
    let bit = |i: usize| -> Option<u32> { Some(u32::from(asc.get(i / 8)? >> (7 - i % 8) & 1)) };
    let read = |from: usize, n: usize| -> Option<u32> {
        (from..from + n).try_fold(0u32, |v, i| Some(v << 1 | bit(i)?))
    };
    let rate_at = |pos: usize| -> Option<(u32, usize)> {
        match read(pos, 4)? {
            15 => Some((read(pos + 4, 24)?, pos + 28)),
            i => Some((*RATES.get(i as usize)?, pos + 4)),
        }
    };
    let aot = read(0, 5)?;
    let (_, mut pos) = rate_at(5)?;
    if aot == 5 || aot == 29 {
        pos += 4; // channel configuration
        return rate_at(pos).map(|(r, _)| r);
    }
    if aot != 2 {
        return None;
    }
    pos += 4 + 3; // channel configuration, GASpecificConfig (frameLength, dependsOnCore, extension)
    if read(pos, 11)? == 0x2B7 && read(pos + 11, 5)? == 5 && read(pos + 16, 1)? == 1 {
        return rate_at(pos + 17).map(|(r, _)| r);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rate_survives_the_asc() {
        for rate in [7350, 44_100, 48_000, 88_200, 96_000, 192_000, 176_400] {
            assert_eq!(
                asc_sample_rate(&audio_specific_config(rate, 2)),
                Some(rate),
                "{rate}"
            );
        }
        assert_eq!(audio_specific_config(48_000, 2), [0x11, 0x90]);
    }

    #[test]
    fn sbr_rates_are_found_either_way() {
        // LC 22.05 kHz stereo + backward-compatible SBR to 44.1 kHz.
        assert_eq!(
            asc_sbr_rate(&[0x13, 0x90, 0x56, 0xE5, 0x80 | (4 << 3)]),
            Some(44_100)
        );
        // Explicit: AOT 5, core 24 kHz (index 6), stereo, ext 48 kHz (3), AOT 2.
        let bits: u32 = 5 << 27 | 6 << 23 | 2 << 19 | 3 << 15 | 2 << 10;
        assert_eq!(asc_sbr_rate(&bits.to_be_bytes()), Some(48_000));
        assert_eq!(asc_sbr_rate(&[0x11, 0x90]), None);
    }
}
