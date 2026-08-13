//! AAC configuration helpers shared by containers (MP4 `esds`, Matroska
//! CodecPrivate, ADTS): the AudioSpecificConfig for AAC-LC.

/// AAC sample-rate → 4-bit samplingFrequencyIndex (ISO 14496-3); 44.1 kHz default.
pub fn sampling_frequency_index(rate: u32) -> u32 {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    RATES.iter().position(|&r| r == rate).unwrap_or(4) as u32
}

/// AudioSpecificConfig for AAC-LC: objectType=2 (5b) + samplingFrequencyIndex (4b)
/// + channelConfiguration (4b) + GASpecificConfig (3b, all zero) = 16 bits.
pub fn audio_specific_config(sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits = (2u32 << 11)
        | (sampling_frequency_index(sample_rate) << 7)
        | ((channels.clamp(1, 7) as u32) << 3);
    vec![(bits >> 8) as u8, (bits & 0xff) as u8]
}
