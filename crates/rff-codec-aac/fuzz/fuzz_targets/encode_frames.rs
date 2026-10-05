//! The adapter's `Encoder` on hostile frames and options: any sample format,
//! channel count, rate, `samples` claim (true or not), plane bytes, and the
//! `-b:a` / `-q:a` option strings. Must never panic or over-allocate.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rff_codec::CodecRegistry;
use rff_core::{AudioFrame, CodecId, Dictionary, Frame, SampleFormat};

const RATES: [u32; 8] = [44_100, 48_000, 32_000, 22_050, 11_025, 8_000, 0, 96_000];
const OPTS: [&str; 8] = ["128k", "320000", "1e30k", "-5k", "nan", "0.5M", "", "9999999999"];

fuzz_target!(|data: &[u8]| {
    if data.len() < 6 { return; }
    let (h, rest) = data.split_at(6);
    let mut reg = CodecRegistry::new();
    rff_codec_aac::register(&mut reg);
    let Ok(mut enc) = reg.find_encoder(CodecId::Aac) else { return };
    let mut opts = Dictionary::new();
    opts.set(if h[0] & 1 == 0 { "b" } else { "q" }, OPTS[usize::from(h[0] >> 5)]);
    let _ = enc.configure(&opts);
    let format = [SampleFormat::S16, SampleFormat::F32, SampleFormat::F32Planar][usize::from(h[1] % 3)];
    let channels = u16::from(h[2] % 9);
    let rate = RATES[usize::from(h[3] % 8)];
    let claim = if h[4] & 0x80 != 0 { 1usize << (h[4] & 0x3F) } else { rest.len() / 2 };
    for part in rest.chunks(1 + usize::from(h[5]) * 64) {
        let frame = Frame::Audio(AudioFrame {
            sample_rate: rate,
            channels,
            format,
            planes: if h[5] == 0 { vec![] } else { vec![part.to_vec()] },
            samples: claim,
            pts: None,
        });
        let _ = enc.send_frame(&frame);
        while enc.receive_packet().is_ok() {}
    }
    enc.flush();
    while enc.receive_packet().is_ok() {}
});
