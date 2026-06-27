//! Drives `transcode::run` end to end: build a real input `.avif`, run the
//! demux→decode→encode→mux loop to produce an output `.avif`, then decode the
//! output and confirm the picture survived. Also checks the stream-copy path.

use std::fs;
use std::path::PathBuf;

use rff::core::{CodecId, Dictionary, Error, Frame, Packet, PixelFormat, VideoFrame};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_drive_{}_{}.avif", std::process::id(), name))
}

/// A 64×64 YUV420p horizontal luma gradient over flat chroma.
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

/// Encode `frame` and mux it to a real `.avif` at `path` via the engine APIs.
fn write_avif(engine: &Engine, path: &PathBuf, frame: &Frame, w: u32, h: u32) {
    let mut enc = engine.codecs.find_encoder(CodecId::Avif).unwrap();
    enc.send_frame(frame).unwrap();
    enc.flush();
    let mut payload = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => payload.extend_from_slice(&p.data),
            Err(Error::Eof) | Err(Error::Again) => break,
            Err(e) => panic!("encode: {e}"),
        }
    }
    let out = Box::new(fs::File::create(path).unwrap());
    let mut mux = engine.formats.open_muxer("avif", out).unwrap();
    let mut s = Stream::new(0, CodecId::Avif);
    s.width = w;
    s.height = h;
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&Packet::from_data(0, payload)).unwrap();
    mux.write_trailer().unwrap();
}

/// Demux+decode an `.avif` at `path` back to a single video frame.
fn read_avif(engine: &Engine, path: &PathBuf) -> VideoFrame {
    let input = Box::new(fs::File::open(path).unwrap());
    let mut dem = engine.formats.open_demuxer("avif", input).unwrap();
    let _streams = dem.read_header().unwrap();
    let packet = dem.read_packet().unwrap();
    let mut dec = engine.codecs.find_decoder(CodecId::Avif).unwrap();
    dec.send_packet(&packet).unwrap();
    dec.flush();
    match dec.receive_frame().unwrap() {
        Frame::Video(v) => v,
        Frame::Audio(_) => panic!("audio from a video codec"),
    }
}

fn mean_luma_diff(a: &VideoFrame, b: &VideoFrame) -> f64 {
    let (w, h) = (a.width as usize, a.height as usize);
    let mut total = 0u64;
    for row in 0..h {
        let ra = &a.planes[0][row * a.strides[0]..][..w];
        let rb = &b.planes[0][row * b.strides[0]..][..w];
        for (x, y) in ra.iter().zip(rb) {
            total += (*x as i16 - *y as i16).unsigned_abs() as u64;
        }
    }
    total as f64 / (w * h) as f64
}

#[test]
fn drive_loop_transcodes_avif_to_avif() {
    let engine = Engine::new();
    let (w, h) = (64u32, 64u32);
    let original = gradient_frame(w as usize, h as usize);
    let Frame::Video(src) = &original else { unreachable!() };

    let in_path = tmp("xcode_in");
    let out_path = tmp("xcode_out");
    write_avif(&engine, &in_path, &original, w, h);

    // `ffmpeg -i in.avif -c:v avif -y out.avif`: full decode → re-encode.
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            format: None,
            video_codec: Some(StreamCodec {
                codec: CodecId::Avif,
                options: Dictionary::new(),
            }),
            audio_codec: None,
            video_filters: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };

    let report = rff::transcode::run(&engine, &spec).expect("transcode run");
    assert!(report.frames_decoded >= 1, "no frames decoded");
    assert!(report.packets_written >= 1, "no packets written");

    let decoded = read_avif(&engine, &out_path);
    assert_eq!((decoded.width, decoded.height), (w, h));
    assert_eq!(decoded.format, PixelFormat::Yuv420p);
    // Two lossy AV1 passes now; allow more drift than a single round-trip.
    let diff = mean_luma_diff(src, &decoded);
    assert!(diff < 40.0, "luma drifted too far after transcode: {diff:.2}");

    let _ = fs::remove_file(&in_path);
    let _ = fs::remove_file(&out_path);
}

#[test]
fn drive_loop_copies_stream_without_reencode() {
    let engine = Engine::new();
    let (w, h) = (64u32, 64u32);
    let original = gradient_frame(w as usize, h as usize);

    let in_path = tmp("copy_in");
    let out_path = tmp("copy_out");
    write_avif(&engine, &in_path, &original, w, h);

    // No `-c:v` → stream copy (remux the AV1 packet unchanged).
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            format: None,
            video_codec: None,
            audio_codec: None,
            video_filters: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };

    let report = rff::transcode::run(&engine, &spec).expect("copy run");
    assert_eq!(report.frames_decoded, 0, "copy path should not decode");
    assert!(report.packets_written >= 1);

    // A copied stream is bit-for-bit the same picture, so it decodes cleanly.
    let in_frame = read_avif(&engine, &in_path);
    let out_frame = read_avif(&engine, &out_path);
    assert_eq!((out_frame.width, out_frame.height), (w, h));
    assert!(mean_luma_diff(&in_frame, &out_frame) < 0.01, "copy altered pixels");

    let _ = fs::remove_file(&in_path);
    let _ = fs::remove_file(&out_path);
}

#[test]
fn drive_loop_applies_scale_filter() {
    let engine = Engine::new();
    let (w, h) = (64u32, 64u32);
    let original = gradient_frame(w as usize, h as usize);

    let in_path = tmp("scale_in");
    let out_path = tmp("scale_out");
    write_avif(&engine, &in_path, &original, w, h);

    // `ffmpeg -i in.avif -vf scale=32:24 -c:v avif out.avif`.
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            format: None,
            video_codec: Some(StreamCodec {
                codec: CodecId::Avif,
                options: Dictionary::new(),
            }),
            audio_codec: None,
            video_filters: Some("scale=32:24".into()),
            maps: Vec::new(),
            overwrite: true,
        }],
    };

    let report = rff::transcode::run(&engine, &spec).expect("scale transcode");
    assert!(report.frames_decoded >= 1);

    // The output picture (and its avif `ispe`) must be the scaled size.
    let decoded = read_avif(&engine, &out_path);
    assert_eq!((decoded.width, decoded.height), (32, 24));

    let _ = fs::remove_file(&in_path);
    let _ = fs::remove_file(&out_path);
}
