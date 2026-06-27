//! Content-sniffing: the engine should identify a container by its bytes, not
//! its filename. We write a real AVIF to a path with a *wrong* extension (and to
//! one with *no* extension) and confirm both probe and transcode detect it.

use std::fs;
use std::path::Path;

use rff::core::{CodecId, Dictionary, Error, Frame, Packet, PixelFormat, VideoFrame};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

/// Encode a 64×64 gradient and mux it to a real `.avif` byte stream at `path`,
/// regardless of the path's extension.
fn write_avif(engine: &Engine, path: &Path, w: u32, h: u32) {
    let (wi, hi) = (w as usize, h as usize);
    let mut y = vec![0u8; wi * hi];
    for row in 0..hi {
        for col in 0..wi {
            y[row * wi + col] = (col * 255 / (wi - 1)) as u8;
        }
    }
    let chroma = vec![128u8; (wi / 2) * (hi / 2)];
    let frame = Frame::Video(VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::Yuv420p,
        planes: vec![y, chroma.clone(), chroma],
        strides: vec![wi, wi / 2, wi / 2],
        pts: Some(0),
    });

    let mut enc = engine.codecs.find_encoder(CodecId::Avif).unwrap();
    enc.send_frame(&frame).unwrap();
    enc.flush();
    let mut payload = Vec::new();
    while let Ok(p) = enc.receive_packet() {
        payload.extend_from_slice(&p.data);
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

#[test]
fn probe_detects_avif_despite_wrong_extension() {
    let engine = Engine::new();
    let (w, h) = (64u32, 64u32);
    // A `.bin` extension that maps to no format — only content can identify it.
    let path = std::env::temp_dir().join(format!("rff_sniff_{}.bin", std::process::id()));
    write_avif(&engine, &path, w, h);

    let info = rff::probe::probe(&engine, &path).expect("probe by content");
    assert_eq!(info.format_name, "avif");
    assert_eq!(info.streams.len(), 1);
    assert_eq!(info.streams[0].codec_id, CodecId::Avif);
    assert_eq!((info.streams[0].width, info.streams[0].height), (w, h));

    let _ = fs::remove_file(&path);
}

#[test]
fn transcode_sniffs_extensionless_input() {
    let engine = Engine::new();
    let (w, h) = (64u32, 64u32);
    // No extension at all on the input.
    let in_path = std::env::temp_dir().join(format!("rff_sniff_in_{}", std::process::id()));
    let out_path = std::env::temp_dir().join(format!("rff_sniff_out_{}.avif", std::process::id()));
    write_avif(&engine, &in_path, w, h);

    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None, // force nothing — detection must do the work
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
            filter_complex: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };

    let report = rff::transcode::run(&engine, &spec).expect("transcode a sniffed input");
    assert!(report.frames_decoded >= 1);
    assert!(matches!(rff::probe::probe(&engine, &out_path), Ok(_)));

    let _ = fs::remove_file(&in_path);
    let _ = fs::remove_file(&out_path);
}

/// Sniffing should not invent a format for genuinely unknown data.
#[test]
fn probe_rejects_unknown_content() {
    let engine = Engine::new();
    let path = std::env::temp_dir().join(format!("rff_sniff_junk_{}.avi", std::process::id()));
    fs::write(&path, b"this is not a real media file at all").unwrap();

    // Extension says avi, but content isn't — and our AVI demuxer rejects it.
    match rff::probe::probe(&engine, &path) {
        Err(Error::InvalidData(_)) | Err(Error::DemuxerNotFound(_)) => {}
        other => panic!("expected a clean failure, got {other:?}"),
    }
    let _ = fs::remove_file(&path);
}
