//! Raw (length-prefixed) H.264 / HEVC samples written back through the mp4,
//! mov and matroska muxers keep every byte; Opus reports its pre-skip as the
//! codec delay; a MOV `colr` keeps its range through MOV → MOV. (FilmCraft's
//! report on #114/#158.)

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_format::avc::split_annexb;
use rff_format::nal::{record_nal_length_size, record_with_4byte_lengths};

use super::*;
use rff_core::{ColorInfo, ColorRange};

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

fn registry() -> FormatRegistry {
    let mut reg = FormatRegistry::new();
    crate::register(&mut reg);
    rff_format_mkv::register(&mut reg);
    reg
}

/// Every muxer under test: format, and whether through its seekable muxer.
const OUTPUTS: [(&str, bool); 6] = [
    ("mp4", false),
    ("mp4", true),
    ("mov", false),
    ("mov", true),
    ("matroska", false),
    ("matroska", true),
];

fn mux(
    reg: &FormatRegistry,
    (format, seekable): (&str, bool),
    streams: &[Stream],
    packets: &[Packet],
) -> Result<Vec<u8>> {
    let sink = Shared::default();
    let mut m = if seekable {
        reg.open_muxer_seekable(format, Box::new(sink.clone()))?
    } else {
        reg.open_muxer(format, Box::new(sink.clone()))?
    };
    m.write_header(streams)?;
    for p in packets {
        m.write_packet(p)?;
    }
    m.write_trailer()?;
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    Ok(bytes)
}

fn demux(reg: &FormatRegistry, format: &str, file: &[u8], raw: bool) -> (Vec<Stream>, Vec<Packet>) {
    let name = if format == "matroska" {
        "matroska"
    } else {
        "mp4"
    };
    let mut d = reg
        .open_demuxer_seekable(name, Box::new(Cursor::new(file.to_vec())))
        .unwrap();
    let streams = d.read_header().unwrap();
    if raw {
        assert!(d.set_raw_samples(true));
    }
    let mut packets = Vec::new();
    loop {
        match d.read_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => return (streams, packets),
            Err(e) => panic!("{format}: {e}"),
        }
    }
}

/// A NAL: `header`, then filler up to `len` bytes that holds an
/// emulation-prevented `00 00 03 01` but no start code.
fn nal(header: &[u8], len: usize) -> Vec<u8> {
    let mut v = header.to_vec();
    let mut i = 0u32;
    while v.len() < len {
        if v.len() + 5 <= len && i % 97 == 7 {
            v.extend_from_slice(&[0, 0, 3, 1]);
        } else {
            v.push((i % 250) as u8 + 3);
        }
        i += 1;
    }
    v
}

/// A length-prefixed sample with `n`-byte lengths.
fn prefixed(nals: &[Vec<u8>], n: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for nal in nals {
        v.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - n..]);
        v.extend_from_slice(nal);
    }
    v
}

const HIGH_SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1F, 0xAE];
const AVC_PPS: &[u8] = &[0x68, 0xCE, 0xBB];
/// The real VPS/SPS/PPS of JCT-VC `IPRED_A_docomo_2` (832x480 Main).
const VPS: &[u8] = &[
    0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00,
    0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0xf0, 0x24,
];
const HEVC_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00,
    0x03, 0x00, 0x99, 0xa0, 0x06, 0x82, 0x01, 0xe1, 0xfe, 0x5f, 0x92, 0x46, 0xd9, 0x6c, 0x80,
];
const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xc1, 0x90, 0x95, 0x81, 0x12];

/// An access unit: its NALs and whether it is a keyframe.
type Unit = (Vec<Vec<u8>>, bool);

/// Each access unit's NALs and whether it is a keyframe. The 4-byte lengths
/// cover the cases that read as start codes: `00 00 01 2C` (300), `00 00 01
/// 90` (400), `00 00 00 01` (a 1-byte NAL), `00 00 01 00` (256), plus
/// lengths with no start code at all (1000, 70000) and in-band parameter sets.
fn h264_units(n: usize) -> Vec<Unit> {
    let mut units = vec![
        (vec![nal(&[0x65], 300)], true),
        (vec![nal(&[0x41], 400)], false),
        (vec![nal(&[0x06, 0x05], 20), nal(&[0x41], 1000)], false),
        (vec![vec![0x0A], nal(&[0x41], 260)], false),
        (
            vec![HIGH_SPS.to_vec(), AVC_PPS.to_vec(), nal(&[0x65], 511)],
            true,
        ),
        (vec![nal(&[0x01], 256)], false),
    ];
    if n == 4 {
        units.push((vec![nal(&[0x41], 70_000)], false));
    }
    units
}

fn hevc_units(n: usize) -> Vec<Unit> {
    let mut units = vec![
        (vec![nal(&[0x26, 0x01], 300)], true),
        (vec![nal(&[0x02, 0x01], 400)], false),
        (
            vec![nal(&[0x4E, 0x01, 0x05], 30), nal(&[0x02, 0x01], 1000)],
            false,
        ),
        (
            vec![
                VPS.to_vec(),
                HEVC_SPS.to_vec(),
                HEVC_PPS.to_vec(),
                nal(&[0x26, 0x01], 511),
            ],
            true,
        ),
        (vec![nal(&[0x02, 0x01], 257)], false),
    ];
    if n == 4 {
        units.push((vec![nal(&[0x02, 0x01], 70_000)], false));
    }
    units
}

/// The stream and its packets as raw samples (`n`-byte lengths, the record
/// declaring `n`), as a raw-mode demuxer or an editor hands them over.
fn raw_source(codec: CodecId, n: usize) -> (Stream, Vec<Unit>, Vec<Packet>) {
    let hevc = codec == CodecId::Hevc;
    let mut record = if hevc {
        rff_format::hvc::build_hvcc_record(VPS, HEVC_SPS, HEVC_PPS).unwrap()
    } else {
        rff_format::avc::build_avcc_record(HIGH_SPS, AVC_PPS)
    };
    let at = if hevc { 21 } else { 4 };
    record[at] = (record[at] & !3) | (n as u8 - 1);
    assert_eq!(record_nal_length_size(&record, hevc), Some(n));
    let mut s = Stream::new(0, codec);
    (s.width, s.height) = if hevc { (832, 480) } else { (64, 48) };
    s.time_base = Rational::new(1, 1000);
    s.extradata = record;
    let units = if hevc { hevc_units(n) } else { h264_units(n) };
    let packets = units
        .iter()
        .enumerate()
        .map(|(i, (nals, key))| {
            let mut p = Packet::from_data(0, prefixed(nals, n));
            p.pts = Some(i as i64 * 40);
            p.dts = p.pts;
            p.duration = 40;
            p.flags.keyframe = *key;
            p
        })
        .collect();
    (s, units, packets)
}

#[test]
fn the_length_prefixes_read_as_start_codes() {
    let (_, _, p) = raw_source(CodecId::H264, 4);
    assert_eq!(&p[0].data[..4], &[0, 0, 1, 0x2C]);
    assert_eq!(&p[1].data[..4], &[0, 0, 1, 0x90]);
    assert_eq!(&p[3].data[..4], &[0, 0, 0, 1]);
    assert_eq!(&p[5].data[..4], &[0, 0, 1, 0]);
    let (_, _, p) = raw_source(CodecId::Hevc, 4);
    assert_eq!(&p[0].data[..4], &[0, 0, 1, 0x2C]);
    assert_eq!(&p[4].data[..4], &[0, 0, 1, 1]);
}

/// Raw samples through every muxer, read back raw: byte-identical with
/// 4-byte lengths (the record too), the same NALs with 4-byte lengths when
/// the source had 2-byte ones; and read back normally, the same NALs as
/// Annex-B.
fn raw_samples_survive(codec: CodecId) {
    let reg = registry();
    let hevc = codec == CodecId::Hevc;
    for n in [4, 2] {
        let (s, units, packets) = raw_source(codec, n);
        let want_record = record_with_4byte_lengths(&s.extradata, hevc).unwrap();
        let total: usize = packets.iter().map(|p| p.data.len()).sum();
        for out in OUTPUTS {
            let what = format!("{codec:?} n={n} {out:?}");
            let file = mux(&reg, out, std::slice::from_ref(&s), &packets)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            let (got_s, got) = demux(&reg, out.0, &file, true);
            assert_eq!(got_s[0].extradata, want_record, "{what}: record");
            assert_eq!(got.len(), packets.len(), "{what}: packet count");
            let mut stored = 0;
            for (i, (g, (nals, key))) in got.iter().zip(&units).enumerate() {
                assert_eq!(g.data, prefixed(nals, 4), "{what}: sample {i}");
                if n == 4 {
                    assert_eq!(g.data, packets[i].data, "{what}: sample {i} bytes");
                }
                assert_eq!(g.flags.keyframe, *key, "{what}: sample {i} keyframe");
                stored += g.data.len();
            }
            if n == 4 {
                assert_eq!(stored, total, "{what}: track bytes");
            }
            // Normal reads: the same NALs, as Annex-B.
            let (_, normal) = demux(&reg, out.0, &file, false);
            for (i, (g, (nals, _))) in normal.iter().zip(&units).enumerate() {
                let split = split_annexb(&g.data);
                let want: Vec<&[u8]> = nals.iter().map(|v| v.as_slice()).collect();
                assert!(split.ends_with(&want), "{what}: Annex-B sample {i}");
            }
        }
    }
}

#[test]
fn raw_h264_samples_survive_every_muxer() {
    raw_samples_survive(CodecId::H264);
}

#[test]
fn raw_hevc_samples_survive_every_muxer() {
    raw_samples_survive(CodecId::Hevc);
}

/// FilmCraft's repro: read a file with raw samples, write the demuxer's
/// streams and packets back, read the result raw: every sample identical.
/// From each container to each container.
#[test]
fn raw_remux_from_a_file_keeps_every_byte() {
    let reg = registry();
    for codec in [CodecId::H264, CodecId::Hevc] {
        let (s, _, packets) = raw_source(codec, 4);
        for src in [("mp4", true), ("mov", false), ("matroska", true)] {
            let file = mux(&reg, src, std::slice::from_ref(&s), &packets).unwrap();
            let (s1, raw1) = demux(&reg, src.0, &file, true);
            for out in OUTPUTS {
                let what = format!("{codec:?} {src:?} -> {out:?}");
                let again = mux(&reg, out, &s1, &raw1).unwrap_or_else(|e| panic!("{what}: {e}"));
                let (s2, raw2) = demux(&reg, out.0, &again, true);
                assert_eq!(s2[0].extradata, s1[0].extradata, "{what}: record");
                assert_eq!(raw2.len(), raw1.len(), "{what}");
                for (i, (a, b)) in raw1.iter().zip(&raw2).enumerate() {
                    assert_eq!(a.data, b.data, "{what}: sample {i}");
                }
            }
        }
    }
}

/// Annex-B input (an encoder, TS, a normal read) is converted as before:
/// a normal MP4 → MP4/MKV remux stores the same samples and record.
#[test]
fn annexb_input_still_converts() {
    let reg = registry();
    for codec in [CodecId::H264, CodecId::Hevc] {
        let (s, _, packets) = raw_source(codec, 4);
        let src = mux(&reg, ("mp4", true), std::slice::from_ref(&s), &packets).unwrap();
        let (s1, annexb) = demux(&reg, "mp4", &src, false);
        assert!(annexb.iter().all(|p| p.data.starts_with(&[0, 0, 0, 1])));
        let (_, raw1) = demux(&reg, "mp4", &src, true);
        for out in OUTPUTS {
            let what = format!("{codec:?} Annex-B -> {out:?}");
            let again = mux(&reg, out, &s1, &annexb).unwrap_or_else(|e| panic!("{what}: {e}"));
            let (s2, raw2) = demux(&reg, out.0, &again, true);
            assert_eq!(s2[0].extradata[..], s1[0].extradata[..], "{what}: record");
            // The in-band parameter sets are lifted out again, so only the
            // samples without them compare byte for byte.
            let in_band = if codec == CodecId::Hevc { 3 } else { 4 };
            assert_eq!(raw2.len(), raw1.len(), "{what}");
            for (i, (a, b)) in raw1.iter().zip(&raw2).enumerate() {
                if i != in_band {
                    assert_eq!(a.data, b.data, "{what}: sample {i}");
                }
            }
        }
    }
}

/// A packet that fits neither form, or not the track's form, is an error,
/// never a shortened sample.
#[test]
fn inconsistent_samples_are_refused() {
    let reg = registry();
    for codec in [CodecId::H264, CodecId::Hevc] {
        let (s, _, packets) = raw_source(codec, 4);
        for out in OUTPUTS {
            let what = format!("{codec:?} {out:?}");
            // A length running past the end of a later packet.
            let mut bad = packets.clone();
            let len = bad[2].data.len();
            bad[2].data.truncate(len - 1);
            assert!(
                mux(&reg, out, std::slice::from_ref(&s), &bad).is_err(),
                "{what}: overrun"
            );
            // Bytes left over after the last NAL.
            let mut bad = packets.clone();
            bad[1].data.extend_from_slice(&[0, 0]);
            assert!(
                mux(&reg, out, std::slice::from_ref(&s), &bad).is_err(),
                "{what}: tail"
            );
            // An Annex-B packet in a length-prefixed track.
            let mut bad = packets.clone();
            bad[2].data = [&[0, 0, 0, 1][..], &nal(&[0x41, 0x01], 50)].concat();
            assert!(
                mux(&reg, out, std::slice::from_ref(&s), &bad).is_err(),
                "{what}: mixed"
            );
            // Raw samples without a record to declare their length size.
            let mut no_record = s.clone();
            no_record.extradata = Vec::new();
            assert!(
                mux(&reg, out, &[no_record], &packets[1..]).is_err(),
                "{what}: no record"
            );
            // The first packet is neither form.
            let mut bad = packets.clone();
            bad[0].data[0] = 0x12;
            assert!(
                mux(&reg, out, std::slice::from_ref(&s), &bad).is_err(),
                "{what}: neither"
            );
        }
    }
}

/// The pre-skip of an MP4 Opus track is its codec delay, as Matroska's
/// `CodecDelay` gives it for the same stream (312 samples = 6.5 ms).
#[test]
fn mp4_opus_reports_its_pre_skip_as_the_codec_delay() {
    let reg = registry();
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    let mut s = Stream::new(0, CodecId::Opus);
    (s.channels, s.sample_rate) = (2, 48_000);
    s.time_base = Rational::new(1, 48_000);
    s.extradata = head;
    let packets: Vec<Packet> = (0..5)
        .map(|i| {
            let mut p = Packet::from_data(0, vec![0xFC, i as u8]);
            p.pts = Some(i * 960);
            p.dts = p.pts;
            p.duration = 960;
            p.flags.keyframe = true;
            p
        })
        .collect();
    for out in OUTPUTS {
        let file = mux(&reg, out, std::slice::from_ref(&s), &packets).unwrap();
        let (got, _) = demux(&reg, out.0, &file, false);
        assert_eq!(got[0].codec_delay_ns, 6_500_000, "{out:?}");
    }
    // MP4 → MKV → MP4 keeps it.
    let mp4 = mux(&reg, ("mp4", true), std::slice::from_ref(&s), &packets).unwrap();
    let (s1, p1) = demux(&reg, "mp4", &mp4, false);
    let mkv = mux(&reg, ("matroska", true), &s1, &p1).unwrap();
    let (s2, p2) = demux(&reg, "matroska", &mkv, false);
    assert_eq!(s2[0].codec_delay_ns, 6_500_000);
    let back = mux(&reg, ("mp4", true), &s2, &p2).unwrap();
    assert_eq!(
        demux(&reg, "mp4", &back, false).0[0].codec_delay_ns,
        6_500_000
    );
}

/// `colr` `nclx` with a known range survives MOV → MOV (and MP4 → MP4):
/// QuickTime's `nclc` has no range flag, so a known range is written as
/// `nclx`.
#[test]
fn colr_range_survives_mov_to_mov() {
    let reg = registry();
    for range in [ColorRange::Limited, ColorRange::Full] {
        let (mut s, _, packets) = raw_source(CodecId::H264, 4);
        s.color = ColorInfo {
            primaries: 1,
            transfer: 1,
            matrix: 1,
        };
        s.color_range = range;
        for flavor in ["mov", "mp4"] {
            for seekable in [false, true] {
                let file =
                    mux(&reg, (flavor, seekable), std::slice::from_ref(&s), &packets).unwrap();
                let colr = file.windows(4).position(|w| w == b"colr").unwrap() + 4;
                assert_eq!(&file[colr..colr + 4], b"nclx", "{flavor} {range:?}");
                let (s1, p1) = demux(&reg, flavor, &file, true);
                assert_eq!((s1[0].color, s1[0].color_range), (s.color, range));
                // And through a second remux.
                let again = mux(&reg, (flavor, seekable), &s1, &p1).unwrap();
                let (s2, _) = demux(&reg, flavor, &again, false);
                assert_eq!(
                    (s2[0].color, s2[0].color_range),
                    (s.color, range),
                    "{flavor} {range:?} remux"
                );
            }
        }
    }
    // An unknown range still gets QuickTime's `nclc` in MOV.
    let (mut s, _, packets) = raw_source(CodecId::H264, 4);
    s.color = ColorInfo {
        primaries: 1,
        transfer: 1,
        matrix: 1,
    };
    let file = mux(&reg, ("mov", true), std::slice::from_ref(&s), &packets).unwrap();
    let colr = file.windows(4).position(|w| w == b"colr").unwrap() + 4;
    assert_eq!(&file[colr..colr + 4], b"nclc");
}
