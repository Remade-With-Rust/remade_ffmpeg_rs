//! Demux (+ remux) a real FLV. Gated on `FLV_REF`; with `FLV_OUT` set it also
//! remuxes for an external `ffprobe`-validates-it check.

use std::fs::File;

use rff_format::{Demuxer, Muxer};
use rff_format_flv::{FlvDemuxer, FlvMuxer};

#[test]
fn demux_and_remux_real_flv() {
    let Ok(path) = std::env::var("FLV_REF") else {
        return;
    };
    let mut dem = FlvDemuxer::new(Box::new(File::open(&path).unwrap()));
    let streams = dem.read_header().expect("read_header");
    eprintln!(
        "[FLV] {} streams: {:?}",
        streams.len(),
        streams
            .iter()
            .map(|s| (s.codec_id, s.media_type, s.extradata.len()))
            .collect::<Vec<_>>()
    );
    assert!(!streams.is_empty());

    let mut mux = std::env::var("FLV_OUT")
        .ok()
        .map(|p| FlvMuxer::new(Box::new(File::create(p).unwrap())));
    if let Some(m) = mux.as_mut() {
        m.write_header(&streams).unwrap();
    }

    let mut packets = 0usize;
    while let Ok(p) = dem.read_packet() {
        packets += 1;
        if let Some(m) = mux.as_mut() {
            m.write_packet(&p).unwrap();
        }
    }
    if let Some(m) = mux.as_mut() {
        m.write_trailer().unwrap();
    }
    eprintln!("[FLV] {packets} packets");
    assert!(packets > 0);
}
