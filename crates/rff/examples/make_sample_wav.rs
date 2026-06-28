//! Generate a sample mono 16-bit WAV: `cargo run -p rff --example make_sample_wav
//! -- out.wav`. Useful for trying the audio path through the CLI.

use std::fs::File;

use rff::core::{AudioFrame, CodecId, Frame, SampleFormat};
use rff::format::Stream;
use rff::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| "sample.wav".into());
    // Optional 2nd arg: sample rate (e.g. 44100), to exercise resampling.
    let sample_rate: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8000);
    let channels = 1u16;

    // ~0.3s of a 440 Hz tone as interleaved s16, at the chosen rate.
    let mut pcm = Vec::new();
    for i in 0..(sample_rate as usize * 3 / 10) {
        let s = ((std::f32::consts::TAU * 440.0 * i as f32 / sample_rate as f32).sin() * 12000.0)
            as i16;
        pcm.extend_from_slice(&s.to_le_bytes());
    }

    let engine = Engine::new();
    let af = AudioFrame {
        sample_rate,
        channels,
        format: SampleFormat::S16,
        planes: vec![pcm.clone()],
        samples: pcm.len() / 2,
        pts: Some(0),
    };
    let mut enc = engine.codecs.find_encoder(CodecId::Pcm).unwrap();
    enc.send_frame(&Frame::Audio(af)).unwrap();
    enc.flush();
    let packet = enc.receive_packet().unwrap();

    let mut mux = engine
        .formats
        .open_muxer("wav", Box::new(File::create(&path).unwrap()))
        .unwrap();
    let mut stream = Stream::new(0, CodecId::Pcm);
    stream.sample_rate = sample_rate;
    stream.channels = channels;
    stream.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[stream]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();

    println!("wrote {path}");
}
