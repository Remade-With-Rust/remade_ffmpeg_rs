//! #114 round 2: Annex-B extradata and High-profile `avcC`, the FLAC
//! STREAMINFO rewrite, a late-starting stream's delay through MP4 → MP4,
//! MKV → MP4 and MP4 → MKV (#115), and `&self` sample reads from several
//! handles.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_format::flac::frame_block_size;

use super::*;

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);
impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Seek for Shared {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(pos)
    }
}
impl Shared {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

fn feed(m: &mut dyn Muxer, streams: &[Stream], packets: &[Packet]) {
    m.write_header(streams).unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
}

fn mux_both(flavor: Flavor, streams: &[Stream], packets: &[Packet]) -> [Vec<u8>; 2] {
    let a = Shared::default();
    feed(
        &mut Mp4Muxer::with_flavor(Box::new(a.clone()), flavor),
        streams,
        packets,
    );
    let b = Shared::default();
    feed(
        &mut Mp4StreamMuxer::with_flavor(Box::new(b.clone()), flavor),
        streams,
        packets,
    );
    [a.bytes(), b.bytes()]
}

fn open(file: &[u8]) -> (Mp4Demuxer, Vec<Stream>) {
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file.to_vec())));
    let s = d.read_header().unwrap();
    (d, s)
}

fn drain(d: &mut dyn Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match d.read_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat())
        .collect()
}

/// A High-profile (100) SPS: sps_id 0, 4:2:0, 8-bit luma and chroma.
const HIGH_SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1F, 0xAE];
const PPS: &[u8] = &[0x68, 0xCE, 0xBB];

fn h264_bare(frames: i64) -> Vec<Packet> {
    (0..frames)
        .map(|i| {
            let mut p =
                Packet::from_data(0, annexb(&[&[if i == 0 { 0x65 } else { 0x41 }, i as u8]]));
            p.pts = Some(i * 40);
            p.flags.keyframe = i == 0;
            p
        })
        .collect()
}

// ---- Annex-B extradata, High-profile avcC -----------------------------------

#[test]
fn annexb_extradata_becomes_an_avcc_with_the_high_profile_fields() {
    let mut s = Stream::new(0, CodecId::H264);
    (s.width, s.height) = (64, 48);
    s.time_base = Rational::new(1, 1000);
    // Parameter sets only in extradata, as start-code NALs.
    s.extradata = annexb(&[HIGH_SPS, PPS]);
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        for file in mux_both(flavor, std::slice::from_ref(&s), &h264_bare(4)) {
            let (mut d, got) = open(&file);
            let avcc = &got[0].extradata;
            assert_eq!(&avcc[..5], &[1, 0x64, 0x00, 0x1F, 0xFF]);
            // chroma_format 1, 8-bit, 8-bit, no SPS extensions.
            assert_eq!(&avcc[avcc.len() - 4..], &[0xFD, 0xF8, 0xF8, 0x00]);
            // The keyframe comes back with the parameter sets in front.
            let p = drain(&mut d);
            assert_eq!(p[0].data, annexb(&[HIGH_SPS, PPS, &[0x65, 0]]));
        }
    }
}

#[test]
fn annexb_hevc_extradata_becomes_an_hvcc() {
    let vps: &[u8] = &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0xf0, 0x24,
    ];
    let sps: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x03, 0x00, 0x99, 0xa0, 0x06, 0x82, 0x01, 0xe1, 0xfe, 0x5f, 0x92, 0x46, 0xd9, 0x6c,
        0x80,
    ];
    let pps: &[u8] = &[0x44, 0x01, 0xc1, 0x90, 0x95, 0x81, 0x12];
    let mut s = Stream::new(0, CodecId::Hevc);
    (s.width, s.height) = (832, 480);
    s.time_base = Rational::new(1, 25);
    s.extradata = annexb(&[vps, sps, pps]);
    let packets: Vec<Packet> = (0..2)
        .map(|i| {
            let mut p = Packet::from_data(
                0,
                annexb(&[if i == 0 {
                    &[0x26, 0x01, 0xAF][..]
                } else {
                    &[0x02, 0x01, 0xD0][..]
                }]),
            );
            p.pts = Some(i);
            p.flags.keyframe = i == 0;
            p
        })
        .collect();
    for file in mux_both(Flavor::Mp4, std::slice::from_ref(&s), &packets) {
        let (mut d, got) = open(&file);
        assert_eq!(got[0].extradata[0], 1, "an hvcC record");
        let p = drain(&mut d);
        assert_eq!(p[0].data, annexb(&[vps, sps, pps, &[0x26, 0x01, 0xAF]]));
    }
}

// ---- FLAC STREAMINFO rewrite --------------------------------------------------

/// FLAC frames: `blocks` samples each, sized `sizes`.
pub(crate) fn flac_frames(sizes: &[usize], last_block: u16) -> Vec<Vec<u8>> {
    let n = sizes.len();
    sizes
        .iter()
        .enumerate()
        .map(|(i, &len)| {
            let mut f = if i + 1 == n {
                let b = (last_block - 1).to_be_bytes();
                vec![0xFF, 0xF8, 0x79, 0x08, i as u8, b[0], b[1]]
            } else {
                vec![0xFF, 0xF8, 0xC9, 0x08, i as u8] // 4096 samples
            };
            f.resize(len, 0x55);
            f
        })
        .collect()
}

/// A STREAMINFO claiming nothing useful: 48 kHz stereo 16-bit, no totals,
/// and an MD5 that cannot be right for the frames written.
pub(crate) fn stale_streaminfo() -> Vec<u8> {
    let mut si = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
    si.extend_from_slice(&(48_000u64 << 44 | 1 << 41 | 15 << 36).to_be_bytes());
    si.extend_from_slice(&[0xEE; 16]);
    si
}

#[test]
fn flac_streaminfo_is_rewritten_at_the_trailer() {
    let sizes = [900, 1300, 700, 333];
    let frames = flac_frames(&sizes, 1000);
    assert_eq!(frame_block_size(&frames[3]), Some(1000));
    let mut s = Stream::new(0, CodecId::Flac);
    (s.channels, s.sample_rate) = (2, 48_000);
    s.time_base = Rational::new(1, 48_000);
    s.extradata = stale_streaminfo();
    let packets: Vec<Packet> = frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut p = Packet::from_data(0, f.clone());
            p.pts = Some(i as i64 * 4096);
            p.flags.keyframe = true;
            p
        })
        .collect();
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        for file in mux_both(flavor, std::slice::from_ref(&s), &packets) {
            let (_, got) = open(&file);
            let si = &got[0].extradata;
            assert_eq!(&si[0..4], &[0x10, 0x00, 0x10, 0x00], "blocks 4096..4096");
            assert_eq!(&si[4..7], &[0, 0x01, 0x4D], "min frame 333");
            assert_eq!(&si[7..10], &[0, 0x05, 0x14], "max frame 1300");
            let total = u64::from(si[13] & 0x0F) << 32
                | u64::from(u32::from_be_bytes([si[14], si[15], si[16], si[17]]));
            assert_eq!(total, 3 * 4096 + 1000);
            assert_eq!(&si[18..], &[0; 16], "MD5 unknown, not stale");
            assert_eq!(
                &si[10..13],
                &stale_streaminfo()[10..13],
                "rate/channels kept"
            );
        }
    }
}

// ---- #115: a late-starting stream keeps its delay ---------------------------

/// Video from 0 and AAC audio from `audio_ms`, both 1 s.
fn av_late(audio_ms: i64) -> (Vec<Stream>, Vec<Packet>) {
    let mut v = Stream::new(0, CodecId::H264);
    (v.width, v.height) = (64, 48);
    v.time_base = Rational::new(1, 1000);
    v.extradata = annexb(&[HIGH_SPS, PPS]);
    let mut a = Stream::new(1, CodecId::Aac);
    (a.channels, a.sample_rate) = (2, 48_000);
    a.time_base = Rational::new(1, 48_000);
    let mut packets = h264_bare(25);
    let start = audio_ms * 48;
    for i in 0..47 {
        let mut p = Packet::from_data(1, vec![0x21, i as u8]);
        p.pts = Some(start + i * 1024);
        p.dts = p.pts;
        p.duration = 1024;
        p.flags.keyframe = true;
        packets.push(p);
    }
    packets.sort_by_key(|p| {
        let tb = if p.stream_index == 0 { 1000 } else { 48_000 };
        p.pts.unwrap() * 48_000 / tb
    });
    (vec![v, a], packets)
}

/// The first audio pts (stream 1) in ms, and the audio edit list.
fn audio_lead(file: &[u8]) -> (i64, Vec<EditEntry>) {
    let (mut d, s) = open(file);
    let edits = d.sample_index().unwrap().edit_list(1).to_vec();
    let first = drain(&mut d)
        .into_iter()
        .find(|p| p.stream_index == 1)
        .and_then(|p| p.pts)
        .unwrap();
    (first * 1000 / i64::from(s[1].time_base.den), edits)
}

#[test]
fn a_late_stream_keeps_its_delay_mp4_to_mp4() {
    let (streams, packets) = av_late(500);
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        let [file, _] = mux_both(flavor, &streams, &packets);
        let (lead, edits) = audio_lead(&file);
        assert_eq!(lead, 500, "{flavor:?}");
        assert_eq!((edits[0].segment_duration, edits[0].media_time), (500, -1));
        // MP4 → MP4: demux every packet and mux it again.
        let (mut d, s) = open(&file);
        let again = drain(&mut d);
        for f2 in mux_both(flavor, &s, &again) {
            let (lead2, edits2) = audio_lead(&f2);
            assert_eq!(lead2, 500, "remux {flavor:?}");
            assert_eq!(
                (edits2[0].segment_duration, edits2[0].media_time),
                (500, -1)
            );
        }
    }
}

#[test]
fn a_late_stream_keeps_its_delay_between_mkv_and_mp4() {
    let mut reg = FormatRegistry::new();
    rff_format_mkv::register(&mut reg);
    let (streams, packets) = av_late(500);
    // MKV first.
    let sink = Shared::default();
    let mut m = reg
        .open_muxer_seekable("matroska", Box::new(sink.clone()))
        .unwrap();
    feed(&mut *m, &streams, &packets);
    drop(m);
    let mkv = sink.bytes();
    // MKV → MP4.
    let mut d = reg
        .open_demuxer_seekable("matroska", Box::new(Cursor::new(mkv)))
        .unwrap();
    let s = d.read_header().unwrap();
    let from_mkv = drain(&mut *d);
    for file in mux_both(Flavor::Mp4, &s, &from_mkv) {
        let (lead, edits) = audio_lead(&file);
        assert_eq!(lead, 500, "MKV → MP4");
        assert_eq!((edits[0].segment_duration, edits[0].media_time), (500, -1));
    }
    // MP4 → MKV.
    let [mp4, _] = mux_both(Flavor::Mp4, &streams, &packets);
    let (mut d, s) = open(&mp4);
    let from_mp4 = drain(&mut d);
    let sink = Shared::default();
    let mut m = reg
        .open_muxer_seekable("matroska", Box::new(sink.clone()))
        .unwrap();
    feed(&mut *m, &s, &from_mp4);
    drop(m);
    let mut d = reg
        .open_demuxer_seekable("matroska", Box::new(Cursor::new(sink.bytes())))
        .unwrap();
    let s = d.read_header().unwrap();
    let first = drain(&mut *d)
        .into_iter()
        .find(|p| p.stream_index == 1)
        .and_then(|p| p.pts)
        .unwrap();
    assert_eq!(
        first * 1000 / i64::from(s[1].time_base.den),
        500,
        "MP4 → MKV"
    );
}

// ---- &self sample reads from several handles --------------------------------

#[test]
fn sample_sources_read_interleaved_and_across_threads() {
    let (streams, packets) = av_late(0);
    let [file, _] = mux_both(Flavor::Mp4, &streams, &packets);
    for seekable in [true, false] {
        let mut d = if seekable {
            Mp4Demuxer::new_seekable(Box::new(Cursor::new(file.clone())))
        } else {
            Mp4Demuxer::new(Box::new(Cursor::new(file.clone())))
        };
        d.read_header().unwrap();
        let idx = d.sample_index().unwrap();
        let want: Vec<Vec<u8>> = (0..idx.sample_count(0))
            .map(|i| idx.read_sample(0, i).unwrap().data)
            .collect();
        let n = want.len() as u64;
        let a = idx.sample_source().unwrap();
        let b = idx.sample_source().unwrap();
        assert_eq!(a.sample_count(0), n);
        // Two handles and the demuxer's own walk, interleaved.
        let mut walked = 0;
        for i in 0..n {
            assert_eq!(a.read_sample(0, i).unwrap().data, want[i as usize]);
            assert_eq!(
                b.read_sample(0, n - 1 - i).unwrap().data,
                want[(n - 1 - i) as usize]
            );
            assert_eq!(b.read_sample(1, i).unwrap().stream_index, 1);
            if let Ok(p) = d.read_packet() {
                walked += 1;
                let _ = p;
            }
            assert_eq!(
                a.sample_info(0, i),
                d.sample_index().unwrap().sample_info(0, i)
            );
        }
        assert!(walked > 0);
        // A clone of a source on another thread, while this one reads.
        let c = Arc::clone(&a);
        let w = want.clone();
        let t = std::thread::spawn(move || {
            (0..n)
                .rev()
                .all(|i| c.read_sample(0, i).unwrap().data == w[i as usize])
        });
        for i in 0..n {
            assert_eq!(b.read_sample(0, i).unwrap().data, want[i as usize]);
        }
        assert!(t.join().unwrap(), "seekable={seekable}");
        assert_eq!(a.edit_list(1), d.sample_index().unwrap().edit_list(1));
    }
}
