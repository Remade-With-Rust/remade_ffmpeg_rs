//! The encoder on hostile PCM and configuration: any f32 bit pattern (NaN, Inf,
//! denormals, huge), any channel count, any sample rate, CBR or VBR, pushed in
//! fuzzer-chosen chunks through every entry point. Must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_mp3::{Mp3Encoder, Mp3EncoderConfig};

const RATES: [u32; 12] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 0, 7, 96000];

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let (cfg, pcm) = data.split_at(4);
    let rate = RATES[cfg[0] as usize % RATES.len()];
    let channels = (cfg[1] % 4) as u16;
    let kbps = [0u32, 8, 32, 64, 128, 192, 256, 320, 999][cfg[2] as usize % 9];
    let vbr = (cfg[3] & 0x80 != 0).then(|| (cfg[3] & 0x0F) as f32);
    let chunk = 2 + (cfg[3] as usize & 0x70) * 9;
    let mut enc = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: kbps,
        vbr_quality: vbr,
    });
    for c in pcm.chunks(chunk) {
        let _ = match cfg[1] >> 6 {
            0 => enc.push_pcm_f32le(c, channels, rate),
            1 => enc.push_pcm_s16le(c, channels, rate),
            2 => {
                let f: Vec<f32> = c
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                enc.push_pcm_f32(&f, channels, rate)
            }
            _ => {
                let s: Vec<i16> = c
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                enc.push_pcm_s16(&s, channels, rate)
            }
        };
        while enc.next_packet().is_ok() {}
    }
    enc.finish();
    while enc.next_packet().is_ok() {}
});
