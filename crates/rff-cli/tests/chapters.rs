//! `rffprobe -show_chapters` and `rff -map_chapters` (#114).

use std::path::Path;
use std::process::Command;

use rff::core::{CodecId, Packet, Rational};
use rff::format::{Chapter, Muxer, Stream};

fn source(path: &Path) {
    let engine = rff::Engine::new();
    let file = std::fs::File::create(path).unwrap();
    let mut m: Box<dyn Muxer> = engine
        .formats
        .open_muxer_seekable("mp4", Box::new(file))
        .unwrap();
    let mut s = Stream::new(0, CodecId::Jpeg);
    (s.width, s.height) = (16, 16);
    let tb = Rational::new(1, 1000);
    m.set_chapters(&[
        Chapter::new(0, tb, 0, 1500).with_title("\"é\"\n"),
        Chapter::new(1, tb, 1500, 2000),
    ]);
    m.write_header(&[s]).unwrap();
    for i in 0..20i64 {
        let mut p = Packet::from_data(0, vec![0xFF, 0xD8, 0xFF, 0xD9]);
        (p.pts, p.duration, p.flags.keyframe) = (Some(i * 100), 100, true);
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
}

fn probe(args: &[&str], path: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rffprobe"))
        .arg("-hide_banner")
        .args(args)
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().replace("\r\n", "\n")
}

#[test]
fn chapters_are_shown_on_request_and_can_be_left_out() {
    let dir = std::env::temp_dir().join(format!("rff-cli-chapters-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("in.mp4");
    source(&src);
    assert!(!probe(&["-of", "json"], &src).contains("\"chapters\""));
    let json = probe(&["-of", "json", "-show_chapters"], &src);
    let want = r#"  "chapters": [
    {
      "id": 0,
      "time_base": "1/1000",
      "start": 0,
      "start_time": "0.000000",
      "end": 1500,
      "end_time": "1.500000",
      "tags": {
        "title": "\"é\"\u000a"
      }
    },
    {
      "id": 1,
      "time_base": "1/1000",
      "start": 1500,
      "start_time": "1.500000",
      "end": 2000,
      "end_time": "2.000000",
      "tags": {
        "title": ""
      }
    }
  ]
}
"#;
    assert!(json.ends_with(want), "{json}");
    let text = probe(&["-show_chapters"], &src);
    assert!(text.contains("    Chapter #0:1: start 1.500000, end 2.000000\n"));

    for (args, kept) in [(&["-map_chapters", "-1"][..], false), (&[], true)] {
        let out = dir.join("out.mkv");
        let status = Command::new(env!("CARGO_BIN_EXE_rff"))
            .args(["-hide_banner", "-y", "-i"])
            .arg(&src)
            .args(args)
            .args(["-c", "copy"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        let text = probe(&["-show_chapters"], &out);
        assert_eq!(text.contains("Chapter #0:0"), kept);
    }
    std::fs::remove_dir_all(dir).unwrap();
}
