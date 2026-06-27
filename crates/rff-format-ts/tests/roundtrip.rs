//! End-to-end demux (+ remux) of a real MPEG-TS file. Gated on `TS_REF` (an
//! ffmpeg-produced `.ts`) so it self-skips in a plain `cargo test`. With
//! `TS_OUT` set it also remuxes the streams back out for an external
//! `ffprobe`-validates-it check.

use std::fs::File;

use rff_format::{Demuxer, Muxer};
use rff_format_ts::{TsDemuxer, TsMuxer};

#[test]
fn demux_and_remux_real_ts() {
    let Ok(path) = std::env::var("TS_REF") else {
        return;
    };
    let mut dem = TsDemuxer::new(Box::new(File::open(&path).unwrap()));
    let streams = dem.read_header().expect("read_header");
    eprintln!(
        "[TS] {} streams: {:?}",
        streams.len(),
        streams.iter().map(|s| (s.codec_id, s.media_type)).collect::<Vec<_>>()
    );
    assert!(!streams.is_empty(), "must find at least one stream");

    let out = std::env::var("TS_OUT").ok();
    let mut mux = out
        .as_ref()
        .map(|p| TsMuxer::new(Box::new(File::create(p).unwrap())));
    if let Some(m) = mux.as_mut() {
        m.write_header(&streams).unwrap();
    }

    let mut packets = 0usize;
    let mut with_pts = 0usize;
    loop {
        match dem.read_packet() {
            Ok(p) => {
                packets += 1;
                if p.pts.is_some() {
                    with_pts += 1;
                }
                if let Some(m) = mux.as_mut() {
                    m.write_packet(&p).unwrap();
                }
            }
            Err(_) => break,
        }
    }
    if let Some(m) = mux.as_mut() {
        m.write_trailer().unwrap();
    }
    eprintln!("[TS] {packets} packets ({with_pts} with PTS)");
    assert!(packets > 0, "must demux packets");
    assert!(with_pts > 0, "PES packets must carry PTS");
}
