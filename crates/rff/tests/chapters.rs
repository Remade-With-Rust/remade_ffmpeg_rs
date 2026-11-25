//! #114 phase 2 through the pipeline: chapters follow a copy between
//! containers (and a trim, and `map_chapters`), timecode tracks follow when
//! data streams are mapped.

use std::path::{Path, PathBuf};

use rff::core::{CodecId, MediaType, Packet, Rational, Timecode};
use rff::format::{Chapter, Muxer, Stream};
use rff::probe::probe_with_chapters;
use rff::transcode::{run, InputSpec, MapSelector, MapSpec, OutputSpec, TranscodeSpec};
use rff::Engine;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rff-chapters-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 10 s MJPEG file with three chapters and a timecode.
fn source(engine: &Engine, path: &Path) {
    let format = path.extension().unwrap().to_str().unwrap();
    let name = if format == "mkv" { "matroska" } else { format };
    let file = std::fs::File::create(path).unwrap();
    let mut m: Box<dyn Muxer> = engine
        .formats
        .open_muxer_seekable(name, Box::new(file))
        .unwrap();
    let mut s = Stream::new(0, CodecId::Jpeg);
    (s.width, s.height) = (16, 16);
    s.time_base = Rational::new(1, 1000);
    s.timecode = Timecode::parse("01:00:00;02", 30);
    let tb = Rational::new(1, 1000);
    assert!(m.set_chapters(&[
        Chapter::new(1, tb, 0, 4000).with_title("Anfang ä"),
        Chapter::new(2, tb, 4000, 6000).with_title("\"two\""),
        Chapter::new(3, tb, 6000, 10_000).with_title("三"),
    ]));
    m.write_header(&[s]).unwrap();
    for i in 0..100i64 {
        let mut p = Packet::from_data(0, vec![0xFF, 0xD8, i as u8, 0xFF, 0xD9]);
        (p.pts, p.dts, p.duration) = (Some(i * 100), Some(i * 100), 100);
        p.flags.keyframe = true;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
}

fn copy(engine: &Engine, from: &Path, to: &Path, set: impl Fn(&mut OutputSpec)) {
    let mut output = OutputSpec {
        path: to.to_path_buf(),
        overwrite: true,
        ..Default::default()
    };
    set(&mut output);
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: from.to_path_buf(),
            format: None,
        }],
        outputs: vec![output],
    };
    run(engine, &spec).unwrap();
}

fn chapters(engine: &Engine, path: &Path) -> Vec<(i64, i64, String)> {
    let (_, _, list) = probe_with_chapters(engine, path).unwrap();
    list.iter()
        .map(|c| {
            let title = c.title().unwrap_or("").to_string();
            (c.start_in(1000), c.end_in(1000), title)
        })
        .collect()
}

#[test]
fn a_copy_keeps_the_chapters() {
    let engine = Engine::new();
    let d = dir("copy");
    let want = [
        (0, 4000, "Anfang ä".to_string()),
        (4000, 6000, "\"two\"".into()),
        (6000, 10_000, "三".into()),
    ];
    for from in ["mp4", "mov", "mkv"] {
        let src = d.join(format!("in.{from}"));
        source(&engine, &src);
        assert_eq!(chapters(&engine, &src), want);
        for to in ["mp4", "mov", "mkv"] {
            let out = d.join(format!("out.{to}"));
            copy(&engine, &src, &out, |_| {});
            assert_eq!(chapters(&engine, &out), want, "{from} to {to}");
            // The media alone was selected: one stream (and the chapter
            // track MP4 needs, which is a data stream when read back).
            let (info, _, _) = probe_with_chapters(&engine, &out).unwrap();
            let media = info
                .streams
                .iter()
                .filter(|s| s.media_type == MediaType::Video);
            assert_eq!(media.count(), 1);
        }
    }
    let src = d.join("in.mkv");
    let out = d.join("cut.mkv");
    copy(&engine, &src, &out, |o| {
        o.trim_start = Some(3.0);
        o.trim_end = Some(5.0);
    });
    let cut = [
        (0, 1000, "Anfang ä".to_string()),
        (1000, 2000, "\"two\"".into()),
    ];
    assert_eq!(chapters(&engine, &out), cut);
    copy(&engine, &src, &out, |o| {
        o.format_options.set("map_chapters", "-1");
    });
    assert!(chapters(&engine, &out).is_empty());
    copy(&engine, &src, &out, |o| {
        o.format_options.set("map_chapters", "0");
    });
    assert_eq!(chapters(&engine, &out), want);
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn mapping_data_streams_copies_the_timecode_track() {
    let engine = Engine::new();
    let d = dir("tmcd");
    let src = d.join("in.mov");
    source(&engine, &src);
    let out = d.join("out.mov");
    let map = |selector| MapSpec { input: 0, selector };
    // The chapter track is a data stream too, carried as chapters instead.
    copy(&engine, &src, &out, |o| {
        o.maps = vec![
            map(MapSelector::Kind(MediaType::Video)),
            map(MapSelector::Kind(MediaType::Data)),
        ];
    });
    assert_eq!(chapters(&engine, &out).len(), 3);
    let file = std::fs::File::open(&out).unwrap();
    let mut demuxer = engine
        .formats
        .open_demuxer_seekable("mov", Box::new(file))
        .unwrap();
    assert!(demuxer.set_timecode_streams(true));
    let streams = demuxer.read_header().unwrap();
    // Video, the new chapter track, the timecode track.
    assert_eq!(streams.len(), 3);
    assert_eq!(streams[2].codec_tag, Some(*b"tmcd"));
    for s in [&streams[0], &streams[2]] {
        assert_eq!(s.timecode.as_ref().unwrap().to_string(), "01:00:00;02");
    }
    let refs = demuxer.track_references(0);
    assert_eq!(
        refs.iter().find(|r| &r.kind == b"tmcd").unwrap().streams,
        [2]
    );
    // `-map 0` takes the media and passes over the chapter track.
    copy(&engine, &src, &out, |o| {
        o.maps = vec![map(MapSelector::All)]
    });
    assert_eq!(chapters(&engine, &out).len(), 3);
    std::fs::remove_dir_all(d).unwrap();
}
