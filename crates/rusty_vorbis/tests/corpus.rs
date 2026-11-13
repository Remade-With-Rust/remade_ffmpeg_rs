//! Correctness gate: the in-house decoder vs lewton 0.10.2 (dev-dependency
//! oracle) and vs FFmpeg's native Vorbis decoder.
//!
//! - Committed tiny fixtures (`tests/data/*.ogg`) are always checked against
//!   lewton.
//! - `RUSTY_VORBIS_CORPUS=<dir>` additionally checks every `*.ogg` in `<dir>`
//!   against lewton AND `ffmpeg` (on PATH), printing a gate table
//!   (`cargo test --release --test corpus -- --nocapture`).
//!
//! Gate: identical frame counts, max |diff| <= 2e-6 and SNR >= 100 dB against
//! both references (float op order differs, so not bit-exact; lewton vs FFmpeg
//! themselves differ by ~5e-8).

#[path = "common/harness.rs"]
mod harness;
#[path = "common/ogg.rs"]
mod ogg;

use harness::{
    compare, decode_ffmpeg, decode_lewton, decode_ours, single_audio_page, FFMPEG_FROM_VORBIS,
};
use std::path::Path;

const MAX_ABS: f64 = 2e-6;
const MIN_SNR: f64 = 100.0;

fn identity(n: usize) -> Vec<usize> {
    (0..n).collect()
}

/// Gate one file; returns the table row.
fn gate(path: &Path, with_ffmpeg: bool) -> String {
    let file = std::fs::read(path).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let ours = decode_ours(&file).unwrap_or_else(|e| panic!("{name}: ours failed: {e}"));
    let lew = decode_lewton(&file).unwrap_or_else(|e| panic!("{name}: lewton failed: {e}"));
    assert_eq!(ours.frames(), lew.frames(), "{name}: frame count vs lewton");
    let dl = compare(&ours, &lew, &identity(ours.channels));
    assert!(
        dl.max_abs <= MAX_ABS && dl.snr_db >= MIN_SNR,
        "{name}: vs lewton max|d|={:.3e} snr={:.1}",
        dl.max_abs,
        dl.snr_db
    );
    let mut row = format!(
        "| {name} | {}ch {} Hz | {} | {} |",
        ours.channels,
        ours.rate,
        ours.frames(),
        lew.frames()
    );
    if with_ffmpeg {
        let ff = decode_ffmpeg(path, ours.channels, ours.rate).expect("ffmpeg decode");
        let map: Vec<usize> = if ours.channels <= 8 {
            FFMPEG_FROM_VORBIS[ours.channels - 1].to_vec()
        } else {
            identity(ours.channels)
        };
        // FFmpeg's Ogg demuxer mis-trims streams whose audio sits on one page
        // (both its native and libvorbis decoders report the same count); there
        // ours == lewton == the granule position, and the common prefix is
        // compared.
        let quirk = ours.frames() != ff.frames() && single_audio_page(&file);
        if !quirk {
            assert_eq!(ours.frames(), ff.frames(), "{name}: frame count vs ffmpeg");
        }
        let df = compare(&ours, &ff, &map);
        let dlf = compare(&lew, &ff, &map);
        assert!(
            df.max_abs <= MAX_ABS && df.snr_db >= MIN_SNR,
            "{name}: vs ffmpeg max|d|={:.3e} snr={:.1}",
            df.max_abs,
            df.snr_db
        );
        row += &format!(
            " {}{} | {:.2e} | {:.1} | {:.2e} | {:.2e} |",
            ff.frames(),
            if quirk { " (demux quirk)" } else { "" },
            df.max_abs,
            df.snr_db,
            dl.max_abs,
            dlf.max_abs
        );
    } else {
        row += &format!(" - | - | - | {:.2e} | - |", dl.max_abs);
    }
    row
}

fn header() {
    println!("| file | layout | frames ours | frames lewton | frames ffmpeg | max\\|d\\| vs ffmpeg | SNR vs ffmpeg (dB) | max\\|d\\| vs lewton | lewton vs ffmpeg |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|");
}

#[test]
fn fixtures_match_lewton() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut n = 0;
    header();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "ogg") {
            println!("{}", gate(&p, false));
            n += 1;
        }
    }
    assert!(n > 0, "no fixtures in {}", dir.display());
}

#[test]
fn corpus_matches_lewton_and_ffmpeg() {
    let Ok(dir) = std::env::var("RUSTY_VORBIS_CORPUS") else {
        eprintln!("RUSTY_VORBIS_CORPUS not set; skipping the external corpus gate");
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "ogg"))
        .collect();
    files.sort();
    header();
    for p in files {
        println!("{}", gate(&p, true));
    }
}

/// The planar API (`decode` + `channel`) yields exactly the interleaved API's
/// samples, packet by packet, and `packet_frames` predicts every count.
#[test]
fn planar_api_matches_interleaved() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "ogg") {
            continue;
        }
        let pk = ogg::read_packets(&std::fs::read(&p).unwrap());
        let (h0, h1, h2) = (&pk[0].data, &pk[1].data, &pk[2].data);
        let mut a = rusty_vorbis::decode::Decoder::new(h0, h1, h2).unwrap();
        let mut b = rusty_vorbis::decode::Decoder::new(h0, h1, h2).unwrap();
        let ch = a.channels();
        let mut buf = vec![0.0f32; a.max_frames_per_packet() * ch];
        for (i, q) in pk[3..].iter().enumerate() {
            let predicted = if i == 0 {
                0
            } else {
                a.packet_frames(&q.data).unwrap()
            };
            let n = a.decode_interleaved(&q.data, &mut buf).unwrap();
            let m = b.decode(&q.data).unwrap();
            assert_eq!(
                (n, m),
                (predicted, predicted),
                "{}: packet {i}",
                p.display()
            );
            for c in 0..ch {
                let plane = b.channel(c);
                assert_eq!(plane.len(), n);
                for f in 0..n {
                    assert_eq!(plane[f].to_bits(), buf[f * ch + c].to_bits());
                }
            }
        }
    }
}
