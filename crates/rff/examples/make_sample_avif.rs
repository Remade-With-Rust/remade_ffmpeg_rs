//! Generate a sample `.avif` for trying the CLI: `cargo run -p rff --example
//! make_sample_avif -- out.avif`. Encodes a 96×64 color-ish gradient through
//! the engine's avif encoder + muxer — the same public API the CLI uses.

use std::fs::File;

use rff::core::{CodecId, Frame, Packet, PixelFormat, VideoFrame};
use rff::format::Stream;
use rff::Engine;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sample.avif".into());
    let (w, h) = (96usize, 64usize);

    // Luma gradient left→right; chroma sweeps so it isn't flat gray.
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = (col * 255 / (w - 1)) as u8;
        }
    }
    let mut u = vec![0u8; (w / 2) * (h / 2)];
    let mut v = vec![0u8; (w / 2) * (h / 2)];
    for row in 0..h / 2 {
        for col in 0..w / 2 {
            u[row * (w / 2) + col] = (col * 255 / (w / 2 - 1)) as u8;
            v[row * (w / 2) + col] = (row * 255 / (h / 2 - 1)) as u8;
        }
    }
    let frame = Frame::Video(VideoFrame {
        width: w as u32,
        height: h as u32,
        format: PixelFormat::Yuv420p,
        planes: vec![y, u, v],
        strides: vec![w, w / 2, w / 2],
        pts: Some(0),
    });

    let engine = Engine::new();
    let mut enc = engine
        .codecs
        .find_encoder(CodecId::Avif)
        .expect("avif encoder");
    enc.send_frame(&frame).expect("send_frame");
    enc.flush();
    let mut payload = Vec::new();
    while let Ok(p) = enc.receive_packet() {
        payload.extend_from_slice(&p.data);
    }

    let mut mux = engine
        .formats
        .open_muxer("avif", Box::new(File::create(&path).expect("create")))
        .expect("avif muxer");
    let mut stream = Stream::new(0, CodecId::Avif);
    stream.width = w as u32;
    stream.height = h as u32;
    mux.write_header(&[stream]).expect("write_header");
    mux.write_packet(&Packet::from_data(0, payload))
        .expect("write_packet");
    mux.write_trailer().expect("write_trailer");

    println!("wrote {path}");
}
