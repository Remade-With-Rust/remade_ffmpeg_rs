//! HLS both directions: segment an H.264 encode into `.m3u8` + `.ts` pieces
//! (registry path-muxer, `-hls_time`), then read the playlist back as an
//! input (HLS input → chained TS stream → MPEG-TS demux) and decode it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rff::core::{CodecId, Dictionary};
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rff_hlsrt_{}_{name}", std::process::id()));
    let _ = fs::create_dir_all(&d);
    d
}

fn write_y4m(path: &Path, frames: usize) {
    let mut f = fs::File::create(path).unwrap();
    write!(f, "YUV4MPEG2 W64 H64 F25:1 Ip A1:1 C420mpeg2\n").unwrap();
    for i in 0..frames {
        write!(f, "FRAME\n").unwrap();
        f.write_all(&vec![(16 + i * 4) as u8; 64 * 64]).unwrap();
        f.write_all(&[128u8; 32 * 32]).unwrap();
        f.write_all(&[128u8; 32 * 32]).unwrap();
    }
}

#[test]
fn hls_segments_then_reads_back() {
    let engine = Engine::new();
    let dir = tmpdir("io");
    let input = dir.join("in.y4m");
    let playlist = dir.join("out.m3u8");
    write_y4m(&input, 30); // 1.2 s @25 fps

    let mut fmt_opts = Dictionary::new();
    fmt_opts.set("hls_time", "0.5");
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: input.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: playlist.clone(),
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
    rff::transcode::run(&engine, &spec).expect("hls encode");

    // Playlist + at least one segment exist, and the playlist is VOD-complete.
    let m3u8 = fs::read_to_string(&playlist).unwrap();
    assert!(m3u8.starts_with("#EXTM3U"));
    assert!(m3u8.contains("#EXT-X-ENDLIST"));
    let segments = m3u8
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .count();
    assert!(segments >= 1, "no segments listed:\n{m3u8}");
    assert!(dir.join("out0.ts").exists());

    // Read the playlist back through the HLS input path: it must resolve to
    // an MPEG-TS stream carrying our H.264.
    let info = rff::probe::probe(&engine, &playlist).expect("hls probe");
    assert_eq!(info.format_name, "mpegts");
    assert_eq!(info.streams[0].codec_id, CodecId::H264);

    // And it decodes end to end: m3u8 → rawvideo y4m with all 30 frames.
    let back = dir.join("back.y4m");
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: playlist.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: back.clone(),
            overwrite: true,
            ..Default::default()
        }],
    };
    rff::transcode::run(&engine, &spec).expect("hls decode");
    let y4m = fs::read(&back).unwrap();
    let frames = y4m.windows(5).filter(|w| w == b"FRAME").count();
    assert_eq!(frames, 30, "frame count through HLS round trip");

    let _ = fs::remove_dir_all(&dir);
}
