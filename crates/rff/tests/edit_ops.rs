//! The "editing" surface added for CLI parity: the -ss/-t trim window, -r CFR
//! conversion, -ac channel mixing, -af audio filters, and the WebM muxer end
//! to end (encode → mux → demux back).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, MediaType, SampleFormat};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_edit_{}_{name}.{ext}", std::process::id()))
}

// ---- helpers ---------------------------------------------------------------

fn write_wav(engine: &Engine, path: &Path, pcm: &[u8], sr: u32, ch: u16) {
    let af = AudioFrame {
        sample_rate: sr,
        channels: ch,
        format: SampleFormat::S16,
        planes: vec![pcm.to_vec()],
        samples: pcm.len() / (2 * ch as usize),
        pts: Some(0),
    };
    let mut enc = engine.codecs.find_encoder(CodecId::Pcm).unwrap();
    enc.send_frame(&Frame::Audio(af)).unwrap();
    enc.flush();
    let packet = enc.receive_packet().unwrap();
    let mut mux = engine
        .formats
        .open_muxer("wav", Box::new(fs::File::create(path).unwrap()))
        .unwrap();
    let mut s = Stream::new(0, CodecId::Pcm);
    s.sample_rate = sr;
    s.channels = ch;
    s.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

fn read_wav(engine: &Engine, path: &Path) -> (Stream, Vec<u8>) {
    let mut dem = engine
        .formats
        .open_demuxer("wav", Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    let streams = dem.read_header().unwrap();
    let mut data = Vec::new();
    while let Ok(p) = dem.read_packet() {
        data.extend_from_slice(&p.data);
    }
    (streams[0].clone(), data)
}

/// Write a tiny y4m: `frames` gray frames, 16x16, 25 fps, each frame's luma
/// filled with its index (so duplicates are detectable).
fn write_y4m(path: &Path, frames: usize) {
    let mut f = fs::File::create(path).unwrap();
    write!(f, "YUV4MPEG2 W16 H16 F25:1 Ip A1:1 C420mpeg2\n").unwrap();
    for i in 0..frames {
        write!(f, "FRAME\n").unwrap();
        f.write_all(&vec![(16 + i) as u8; 16 * 16]).unwrap(); // Y
        f.write_all(&[128u8; 8 * 8]).unwrap(); // U
        f.write_all(&[128u8; 8 * 8]).unwrap(); // V
    }
}

fn audio_out(codec: CodecId) -> Option<StreamCodec> {
    Some(StreamCodec {
        codec,
        options: Dictionary::new(),
        sample_format: None,
    })
}

fn s16_ramp(n: usize) -> Vec<u8> {
    // Monotone ramp: sample k has value k/8 (fits i16 up to n=262k).
    (0..n)
        .flat_map(|k| ((k / 8) as i16).to_le_bytes())
        .collect()
}

// ---- trim ------------------------------------------------------------------

#[test]
fn ss_t_trims_audio_sample_accurately() {
    let engine = Engine::new();
    let in_path = tmp("trim_in", "wav");
    let out_path = tmp("trim_out", "wav");
    // 2 s of 8 kHz mono.
    write_wav(&engine, &in_path, &s16_ramp(16_000), 8_000, 1);

    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            audio_codec: audio_out(CodecId::Pcm),
            overwrite: true,
            trim_start: Some(0.5),
            trim_end: Some(1.5),
            ..Default::default()
        }],
    };
    rff::transcode::run(&engine, &spec).expect("trim transcode");

    let (s, data) = read_wav(&engine, &out_path);
    assert_eq!(s.sample_rate, 8_000);
    // [0.5 s, 1.5 s) of 8 kHz = exactly 8000 samples.
    assert_eq!(data.len() / 2, 8_000);
    // And they are the RIGHT samples: the first kept sample is #4000.
    let first = i16::from_le_bytes([data[0], data[1]]);
    assert_eq!(first, (4000 / 8) as i16);

    for p in [in_path, out_path] {
        let _ = fs::remove_file(p);
    }
}

// ---- -ac / -af -------------------------------------------------------------

#[test]
fn ac_downmixes_stereo_to_mono() {
    let engine = Engine::new();
    let in_path = tmp("ac_in", "wav");
    let out_path = tmp("ac_out", "wav");
    // Stereo: L = 1000, R = 3000 → mono average 2000.
    let pcm: Vec<u8> = (0..1000)
        .flat_map(|_| {
            let mut b = 1000i16.to_le_bytes().to_vec();
            b.extend_from_slice(&3000i16.to_le_bytes());
            b
        })
        .collect();
    write_wav(&engine, &in_path, &pcm, 8_000, 2);

    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            audio_codec: Some(StreamCodec {
                codec: CodecId::Pcm,
                options: Dictionary::new(),
                sample_format: Some(SampleFormat::S16),
            }),
            overwrite: true,
            audio_channels: Some(1),
            ..Default::default()
        }],
    };
    rff::transcode::run(&engine, &spec).expect("-ac transcode");

    let (s, data) = read_wav(&engine, &out_path);
    assert_eq!(s.channels, 1);
    assert_eq!(data.len() / 2, 1000);
    let first = i16::from_le_bytes([data[0], data[1]]);
    assert_eq!(first, 2000);

    for p in [in_path, out_path] {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn af_volume_scales_samples() {
    let engine = Engine::new();
    let in_path = tmp("vol_in", "wav");
    let out_path = tmp("vol_out", "wav");
    let pcm: Vec<u8> = (0..1000).flat_map(|_| 1000i16.to_le_bytes()).collect();
    write_wav(&engine, &in_path, &pcm, 8_000, 1);

    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            audio_codec: Some(StreamCodec {
                codec: CodecId::Pcm,
                options: Dictionary::new(),
                sample_format: Some(SampleFormat::S16),
            }),
            overwrite: true,
            audio_filters: Some("volume=2.0".into()),
            ..Default::default()
        }],
    };
    rff::transcode::run(&engine, &spec).expect("-af transcode");

    let (_, data) = read_wav(&engine, &out_path);
    let first = i16::from_le_bytes([data[0], data[1]]);
    assert_eq!(first, 2000);

    for p in [in_path, out_path] {
        let _ = fs::remove_file(p);
    }
}

// ---- -r (CFR) --------------------------------------------------------------

#[test]
fn r_upsamples_frame_rate_by_duplication() {
    let engine = Engine::new();
    let in_path = tmp("fps_in", "y4m");
    let out_path = tmp("fps_out", "y4m");
    write_y4m(&in_path, 10); // 10 frames @25 fps = 0.4 s

    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            overwrite: true,
            frame_rate: Some((50, 1)),
            ..Default::default()
        }],
    };
    rff::transcode::run(&engine, &spec).expect("-r transcode");

    // The output header must declare 50 fps and carry ~2x the frames.
    let head = fs::read(&out_path).unwrap();
    let header = String::from_utf8_lossy(&head[..64.min(head.len())]).to_string();
    assert!(header.contains("F50:1"), "header: {header}");
    let frames = head.windows(5).filter(|w| w == b"FRAME").count();
    assert!(
        (19..=21).contains(&frames),
        "expected ~20 output frames, got {frames}"
    );

    for p in [in_path, out_path] {
        let _ = fs::remove_file(p);
    }
}

// ---- WebM end to end -------------------------------------------------------

#[test]
fn vp9_encodes_into_webm_and_demuxes_back() {
    let engine = Engine::new();
    let in_path = tmp("webm_in", "y4m");
    let out_path = tmp("webm_out", "webm");
    write_y4m(&in_path, 5);

    let mut opts = Dictionary::new();
    opts.set("speed", "4"); // fastest — this is a plumbing test, not RD
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            video_codec: Some(StreamCodec {
                codec: CodecId::Vp9,
                options: opts,
                sample_format: None,
            }),
            overwrite: true,
            ..Default::default()
        }],
    };
    let report = rff::transcode::run(&engine, &spec).expect("webm encode");
    assert!(report.packets_written >= 5);

    // Demux the produced WebM with our own Matroska demuxer.
    let mut dem = engine
        .formats
        .open_demuxer("matroska", Box::new(fs::File::open(&out_path).unwrap()))
        .unwrap();
    let streams = dem.read_header().unwrap();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].codec_id, CodecId::Vp9);
    assert_eq!(streams[0].media_type, MediaType::Video);
    assert_eq!((streams[0].width, streams[0].height), (16, 16));

    // Frames carry real, advancing timestamps (25 fps = 40 ms apart) — this is
    // the encoder-pts stamping path; without it every block sits at t=0.
    let p0 = dem.read_packet().unwrap();
    let p1 = dem.read_packet().unwrap();
    assert!(p0.flags.keyframe);
    assert_eq!(p0.pts, Some(0));
    assert_eq!(p1.pts, Some(40));

    // And the file starts with the EBML magic + webm doctype.
    let bytes = fs::read(&out_path).unwrap();
    assert_eq!(&bytes[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
    let head = &bytes[..64.min(bytes.len())];
    assert!(
        head.windows(4).any(|w| w == b"webm"),
        "missing webm doctype"
    );

    for p in [in_path, out_path] {
        let _ = fs::remove_file(p);
    }
}
