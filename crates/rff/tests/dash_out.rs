//! DASH VOD output: y4m → H.264 → out.mpd + init/chunk fMP4 segments, with an
//! exact SegmentTimeline in a static manifest.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use rff::core::{CodecId, Dictionary};
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("rff_dash_{}", std::process::id()));
    let _ = fs::create_dir_all(&d);
    d
}

#[test]
fn dash_writes_manifest_and_fmp4_segments() {
    let engine = Engine::new();
    let dir = tmpdir();
    let input = dir.join("in.y4m");
    let mpd = dir.join("out.mpd");

    let mut f = fs::File::create(&input).unwrap();
    write!(f, "YUV4MPEG2 W64 H64 F25:1 Ip A1:1 C420mpeg2\n").unwrap();
    for i in 0..30 {
        write!(f, "FRAME\n").unwrap();
        f.write_all(&vec![(16 + i * 4) as u8; 64 * 64]).unwrap();
        f.write_all(&[128u8; 32 * 32]).unwrap();
        f.write_all(&[128u8; 32 * 32]).unwrap();
    }
    drop(f);

    let mut fmt_opts = Dictionary::new();
    fmt_opts.set("seg_duration", "0.5");
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: input,
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: mpd.clone(),
            video_codec: Some(StreamCodec {
                codec: CodecId::H264,
                options: Dictionary::new(),
                sample_format: None,
            }),
            overwrite: true,
            format_options: fmt_opts,
            ..Default::default()
        }],
    };
    rff::transcode::run(&engine, &spec).expect("dash encode");

    // Manifest is a static MPD with a timeline covering ~1.2 s.
    let xml = fs::read_to_string(&mpd).unwrap();
    assert!(xml.contains("type=\"static\""), "{xml}");
    assert!(xml.contains("<SegmentTimeline>"));
    assert!(xml.contains("init-stream0.m4s"));
    assert!(xml.contains("codecs=\"avc1."), "codec string: {xml}");
    assert!(xml.contains("mediaPresentationDuration=\"PT1.2"), "{xml}");

    // Init segment: ftyp + moov with mvex (fragmented profile).
    let init = fs::read(dir.join("init-stream0.m4s")).unwrap();
    assert_eq!(&init[4..8], b"ftyp");
    assert!(init.windows(4).any(|w| w == b"mvex"), "no mvex in init");
    assert!(init.windows(4).any(|w| w == b"avcC"), "no avcC in init");

    // First media segment: moof + mdat.
    let seg = fs::read(dir.join("chunk-stream0-00001.m4s")).unwrap();
    assert_eq!(&seg[4..8], b"moof");
    assert!(seg.windows(4).any(|w| w == b"mdat"));

    let _ = fs::remove_dir_all(&dir);
}
