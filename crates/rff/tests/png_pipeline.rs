//! The real image-conversion pipeline through public APIs: write a PNG, then
//! `png → (format=yuv420p) → avif` and back `avif → (format=rgb24) → png`,
//! confirming detection, dimensions, and approximate pixels survive.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{CodecId, Dictionary, Frame, PixelFormat, VideoFrame};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_png_{}_{name}.{ext}", std::process::id()))
}

fn rgb_gradient(w: u32, h: u32) -> Frame {
    let (wi, hi) = (w as usize, h as usize);
    let mut rgb = vec![0u8; wi * hi * 3];
    for j in 0..hi {
        for i in 0..wi {
            let o = (j * wi + i) * 3;
            rgb[o] = (i * 255 / (wi - 1)) as u8;
            rgb[o + 1] = (j * 255 / (hi - 1)) as u8;
            rgb[o + 2] = 100;
        }
    }
    Frame::Video(VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::Rgb24,
        planes: vec![rgb],
        strides: vec![wi * 3],
        pts: None,
    })
}

fn write_png(engine: &Engine, path: &Path, frame: &Frame) {
    let Frame::Video(v) = frame else { unreachable!() };
    let mut enc = engine.codecs.find_encoder(CodecId::Png).unwrap();
    enc.send_frame(frame).unwrap();
    enc.flush();
    let packet = enc.receive_packet().unwrap();

    let mut mux = engine
        .formats
        .open_muxer("png", Box::new(fs::File::create(path).unwrap()))
        .unwrap();
    let mut s = Stream::new(0, CodecId::Png);
    s.width = v.width;
    s.height = v.height;
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

fn transcode(engine: &Engine, input: &Path, output: &Path, codec: CodecId, vf: &str) {
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: input.to_path_buf(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: output.to_path_buf(),
            format: None,
            video_codec: Some(StreamCodec {
                codec,
                options: Dictionary::new(),
            }),
            audio_codec: None,
            video_filters: Some(vf.to_string()),
            maps: Vec::new(),
            overwrite: true,
        }],
    };
    rff::transcode::run(engine, &spec).expect("transcode");
}

fn decode_png(engine: &Engine, path: &Path) -> VideoFrame {
    let mut dem = engine
        .formats
        .open_demuxer("png", Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    let _ = dem.read_header().unwrap();
    let packet = dem.read_packet().unwrap();
    let mut dec = engine.codecs.find_decoder(CodecId::Png).unwrap();
    dec.send_packet(&packet).unwrap();
    dec.flush();
    match dec.receive_frame().unwrap() {
        Frame::Video(v) => v,
        Frame::Audio(_) => panic!("audio from image codec"),
    }
}

#[test]
fn png_to_avif_and_back() {
    let engine = Engine::new();
    let (w, h) = (48u32, 32u32);
    let original = rgb_gradient(w, h);
    let Frame::Video(src) = &original else { unreachable!() };

    let src_png = tmp("src", "png");
    write_png(&engine, &src_png, &original);

    // Content sniffing identifies the PNG.
    let info = rff::probe::probe(&engine, &src_png).unwrap();
    assert_eq!(info.format_name, "png");
    assert_eq!(info.streams[0].codec_id, CodecId::Png);
    assert_eq!((info.streams[0].width, info.streams[0].height), (w, h));

    // png → avif (RGB decoded, converted to YUV, AV1-encoded, HEIF-wrapped).
    let avif = tmp("mid", "avif");
    transcode(&engine, &src_png, &avif, CodecId::Avif, "format=yuv420p");

    // avif → png (AV1-decoded YUV, converted back to RGB, PNG-encoded).
    let out_png = tmp("out", "png");
    transcode(&engine, &avif, &out_png, CodecId::Png, "format=rgb24");

    let info2 = rff::probe::probe(&engine, &out_png).unwrap();
    assert_eq!(info2.format_name, "png");
    assert_eq!((info2.streams[0].width, info2.streams[0].height), (w, h));

    // Pixels survive within tolerance (AV1 lossy + RGB↔YUV rounding).
    let decoded = decode_png(&engine, &out_png);
    assert_eq!(decoded.format, PixelFormat::Rgb24);
    let total: u64 = src.planes[0]
        .iter()
        .zip(&decoded.planes[0])
        .map(|(a, b)| (*a as i16 - *b as i16).unsigned_abs() as u64)
        .sum();
    let mean = total as f64 / (w * h * 3) as f64;
    assert!(mean < 20.0, "png↔avif round-trip drifted too far: {mean:.2}");

    for p in [src_png, avif, out_png] {
        let _ = fs::remove_file(p);
    }
}
