//! End-to-end AVIF pipeline test, exercised entirely through the public engine
//! API: encode a frame (rav1e) → write a real `.avif` file (the avif muxer) →
//! read it back (the avif demuxer) → decode (rav1d) → confirm the pixels
//! survive. This ties the codec, the container, and the registries together.

use std::fs;

use rff::core::{CodecId, Error, Frame, Packet, PixelFormat, VideoFrame};
use rff::format::{Input, Output, Stream};
use rff::Engine;

/// A 64×64 YUV420p frame: horizontal luma gradient over flat mid-gray chroma.
fn gradient_frame(w: usize, h: usize) -> Frame {
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = (col * 255 / (w - 1)) as u8;
        }
    }
    let chroma = vec![128u8; (w / 2) * (h / 2)];
    Frame::Video(VideoFrame {
        width: w as u32,
        height: h as u32,
        format: PixelFormat::Yuv420p,
        planes: vec![y, chroma.clone(), chroma],
        strides: vec![w, w / 2, w / 2],
        pts: Some(0),
    })
}

#[test]
fn encode_mux_demux_decode_roundtrip() {
    let engine = Engine::new();
    let (w, h) = (64u32, 64u32);
    let original = gradient_frame(w as usize, h as usize);

    // --- encode the frame to an AV1 bitstream ---
    let mut enc = engine.codecs.find_encoder(CodecId::Avif).expect("avif encoder");
    enc.send_frame(&original).expect("send_frame");
    enc.flush();
    let mut payload = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => payload.extend_from_slice(&p.data),
            Err(Error::Eof) | Err(Error::Again) => break,
            Err(e) => panic!("encode: {e}"),
        }
    }
    assert!(!payload.is_empty(), "encoder produced no bitstream");

    // --- mux to a real .avif file on disk ---
    let path = std::env::temp_dir().join(format!("rff_avif_roundtrip_{}.avif", std::process::id()));
    {
        let out: Output = Box::new(fs::File::create(&path).expect("create file"));
        let mut mux = engine.formats.open_muxer("avif", out).expect("avif muxer");
        let mut stream = Stream::new(0, CodecId::Avif);
        stream.width = w;
        stream.height = h;
        mux.write_header(&[stream]).expect("write_header");
        mux.write_packet(&Packet::from_data(0, payload)).expect("write_packet");
        mux.write_trailer().expect("write_trailer");
    } // dropping the muxer closes the file

    // It should be a recognizable AVIF on disk.
    let bytes = fs::read(&path).expect("read back file");
    assert_eq!(&bytes[4..8], b"ftyp");
    assert_eq!(&bytes[8..12], b"avif");

    // --- demux the file back ---
    let input: Input = Box::new(fs::File::open(&path).expect("open file"));
    let mut dem = engine.formats.open_demuxer("avif", input).expect("avif demuxer");
    let streams = dem.read_header().expect("read_header");
    assert_eq!(streams[0].width, w);
    assert_eq!(streams[0].height, h);
    assert_eq!(streams[0].codec_id, CodecId::Avif);
    let packet = dem.read_packet().expect("read_packet");

    // --- decode back to pixels ---
    let mut dec = engine.codecs.find_decoder(CodecId::Avif).expect("avif decoder");
    dec.send_packet(&packet).expect("send_packet");
    dec.flush();
    let decoded = match dec.receive_frame().expect("receive_frame") {
        Frame::Video(v) => v,
        Frame::Audio(_) => panic!("decoded audio from a video codec"),
    };

    let _ = fs::remove_file(&path);

    // Geometry is exact; luma is close (AV1 is lossy).
    assert_eq!(decoded.width, w);
    assert_eq!(decoded.height, h);
    assert_eq!(decoded.format, PixelFormat::Yuv420p);

    let Frame::Video(src) = &original else { unreachable!() };
    let (w, h) = (w as usize, h as usize);
    let mut total_diff = 0u64;
    for row in 0..h {
        let src_row = &src.planes[0][row * src.strides[0]..][..w];
        let dec_row = &decoded.planes[0][row * decoded.strides[0]..][..w];
        for (a, b) in src_row.iter().zip(dec_row) {
            total_diff += (*a as i16 - *b as i16).unsigned_abs() as u64;
        }
    }
    let mean_abs_diff = total_diff as f64 / (w * h) as f64;
    assert!(
        mean_abs_diff < 30.0,
        "luma drifted too far through encode→avif→decode: mean abs diff {mean_abs_diff:.2}"
    );
}
