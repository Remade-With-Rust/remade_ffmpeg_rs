//! The encoder on hostile PCM and configuration: any f32 bit pattern (NaN, Inf,
//! subnormals, huge), any channel count, standard and non-standard rates, every
//! encoder switch, interleaved or planar, in fuzzer-chosen chunks. Must never
//! panic; every packet must be drainable after `finish` and fit the decoder
//! input buffer.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_aac::{AacEncoder, AacEncoderConfig, WindowShape};

const RATES: [u32; 14] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 1,
];

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let (cfg, pcm) = data.split_at(4);
    let rate = RATES[cfg[0] as usize % RATES.len()];
    let channels = u16::from(cfg[1] % 9);
    let flags = cfg[2];
    let config = AacEncoderConfig {
        bitrate_bps: [0u32, 8_000, 32_000, 64_000, 128_000, 256_000, 512_000, u32::MAX][cfg[3] as usize % 8],
        window_shape: [WindowShape::Sine, WindowShape::Kbd, WindowShape::Auto][flags as usize % 3],
        short_block_psy: flags & 0x04 != 0,
        window_grouping: flags & 0x08 != 0,
        tonality_smr: flags & 0x10 != 0,
        tns: flags & 0x20 != 0,
        pns: flags & 0x40 != 0,
        intensity: flags & 0x80 != 0,
        relative_transients: cfg[3] & 0x10 != 0,
        stereo_bit_split: cfg[3] & 0x20 != 0,
        ..AacEncoderConfig::default()
    };
    let samples: Vec<f32> = pcm
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let mut enc = AacEncoder::new(config);
    let chunk = 1 + (cfg[1] as usize >> 4) * 97;
    for c in samples.chunks(chunk) {
        let _ = if cfg[3] & 0x40 != 0 && channels > 0 {
            let ch = usize::from(channels);
            let per = c.len() / ch;
            let planes: Vec<&[f32]> = (0..ch).map(|i| &c[i * per..(i + 1) * per]).collect();
            enc.push_pcm_planar(&planes, rate)
        } else {
            enc.push_pcm(c, channels, rate)
        };
    }
    enc.finish();
    // Every block fits the decoder input buffer: 6144 bits per channel.
    let limit = 768 * usize::from(channels.clamp(1, 6));
    while let Ok(p) = enc.next_packet() {
        assert!(p.data.len() <= limit, "{} B block for {channels} ch", p.data.len());
    }
});
