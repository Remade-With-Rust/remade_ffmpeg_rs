//! JPEG Multi-Picture Format through the engine: N JPEGs muxed as
//! `jpeg_mpf`, demuxed back one stream per image, `-map 0:N` extracting an
//! image byte for byte, an unmapped still-image output taking the primary
//! image only, and the MP types surviving a remux.

use std::fs;
use std::path::{Path, PathBuf};

use rff::transcode::{InputSpec, MapSelector, MapSpec, OutputSpec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_mpf_{}_{name}.{ext}", std::process::id()))
}

/// A one-frame y4m of `w`×`h`; `seed` varies the picture.
fn write_y4m(path: &Path, w: usize, h: usize, seed: usize) {
    let mut out = format!("YUV4MPEG2 W{w} H{h} F25:1 Ip A1:1 C420jpeg\nFRAME\n").into_bytes();
    out.extend((0..w * h).map(|i| ((i * 7 + seed * 31) % 251) as u8));
    out.extend(std::iter::repeat_n(128u8, w * h / 2));
    fs::write(path, out).unwrap();
}

fn run(engine: &Engine, inputs: &[&Path], out: &Path, format: Option<&str>, maps: Vec<MapSpec>) {
    let spec = TranscodeSpec {
        inputs: inputs
            .iter()
            .map(|p| InputSpec {
                path: p.to_path_buf(),
                format: None,
            })
            .collect(),
        outputs: vec![OutputSpec {
            path: out.to_path_buf(),
            format: format.map(str::to_string),
            maps,
            overwrite: true,
            ..Default::default()
        }],
    };
    rff::transcode::run(engine, &spec).expect("transcode");
}

fn map(input: usize, selector: MapSelector) -> MapSpec {
    MapSpec { input, selector }
}

#[test]
fn mux_demux_extract_and_remux() {
    let engine = Engine::new();
    let sizes = [(64, 48), (32, 16), (16, 16)];
    let mut jpegs = Vec::new();
    for (k, &(w, h)) in sizes.iter().enumerate() {
        let y4m = tmp(&format!("src{k}"), "y4m");
        let jpg = tmp(&format!("src{k}"), "jpg");
        write_y4m(&y4m, w, h, k);
        run(&engine, &[&y4m], &jpg, None, vec![]);
        jpegs.push(jpg);
    }

    // Three JPEGs -> one MPF file.
    let mpf = tmp("three", "mpo");
    let inputs: Vec<&Path> = jpegs.iter().map(PathBuf::as_path).collect();
    let maps = (0..3).map(|i| map(i, MapSelector::All)).collect();
    run(&engine, &inputs, &mpf, None, maps);

    let (info, tags) = rff::probe::probe_with_metadata(&engine, &mpf).unwrap();
    assert_eq!(info.format_name, "jpeg_mpf");
    assert_eq!(info.streams.len(), 3);
    for (s, &(w, h)) in info.streams.iter().zip(&sizes) {
        assert_eq!((s.width, s.height), (w as u32, h as u32));
    }
    assert_eq!(tags[0].get("mpf_type"), Some("primary"));
    assert_eq!(tags[1].get("mpf_type"), Some("undefined"));

    // -map 0:N extracts image N byte for byte (a stream copy).
    for (n, src) in jpegs.iter().enumerate().skip(1) {
        let out = tmp(&format!("x{n}"), "jpg");
        run(
            &engine,
            &[&mpf],
            &out,
            None,
            vec![map(0, MapSelector::Index(n))],
        );
        assert_eq!(fs::read(&out).unwrap(), fs::read(src).unwrap());
    }

    // No -map into a still image: the primary only, not three images.
    let png = tmp("primary", "png");
    run(&engine, &[&mpf], &png, None, vec![]);
    let info = rff::probe::probe(&engine, &png).unwrap();
    assert_eq!(info.streams.len(), 1);
    assert_eq!((info.streams[0].width, info.streams[0].height), (64, 48));

    // A remux keeps every image and its type; the stale index is replaced,
    // so remuxing the remux changes nothing.
    let again = tmp("again", "jpg");
    run(
        &engine,
        &[&mpf],
        &again,
        Some("jpeg_mpf"),
        vec![map(0, MapSelector::All)],
    );
    let twice = tmp("twice", "jpg");
    run(
        &engine,
        &[&again],
        &twice,
        Some("jpeg_mpf"),
        vec![map(0, MapSelector::All)],
    );
    assert_eq!(fs::read(&again).unwrap(), fs::read(&mpf).unwrap());
    assert_eq!(fs::read(&twice).unwrap(), fs::read(&mpf).unwrap());

    let extracted = [tmp("x1", "jpg"), tmp("x2", "jpg")];
    for p in jpegs
        .iter()
        .chain(&extracted)
        .chain([&mpf, &png, &again, &twice])
    {
        let _ = fs::remove_file(p);
    }
}
