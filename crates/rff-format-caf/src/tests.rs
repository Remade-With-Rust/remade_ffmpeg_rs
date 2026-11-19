use std::io::Cursor;

use super::*;

fn chunk(out: &mut Vec<u8>, id: &[u8; 4], size: i64, body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(body);
}

fn desc(
    rate: f64,
    format: &[u8; 4],
    flags: u32,
    bpp: u32,
    fpp: u32,
    ch: u32,
    bits: u32,
) -> Vec<u8> {
    let mut d = rate.to_be_bytes().to_vec();
    d.extend_from_slice(format);
    for v in [flags, bpp, fpp, ch, bits] {
        d.extend_from_slice(&v.to_be_bytes());
    }
    d
}

fn caf(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = b"caff\x00\x01\x00\x00".to_vec();
    for (id, body) in chunks {
        chunk(&mut out, id, body.len() as i64, body);
    }
    out
}

fn data(samples: &[u8]) -> Vec<u8> {
    let mut d = 0u32.to_be_bytes().to_vec();
    d.extend_from_slice(samples);
    d
}

fn varints(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    for &v in values {
        let mut groups = vec![(v & 0x7F) as u8];
        let mut v = v >> 7;
        while v > 0 {
            groups.push((v & 0x7F) as u8 | 0x80);
            v >>= 7;
        }
        groups.reverse();
        out.extend_from_slice(&groups);
    }
    out
}

fn pakt(count: i64, valid: i64, priming: i32, table: &[u8]) -> Vec<u8> {
    let mut p = count.to_be_bytes().to_vec();
    p.extend_from_slice(&valid.to_be_bytes());
    p.extend_from_slice(&priming.to_be_bytes());
    p.extend_from_slice(&0i32.to_be_bytes());
    p.extend_from_slice(table);
    p
}

fn demux(file: Vec<u8>) -> Result<(Stream, Vec<Packet>)> {
    let mut d = CafDemuxer::new(Box::new(Cursor::new(file)));
    let s = d.read_header()?.remove(0);
    let mut packets = Vec::new();
    loop {
        match d.read_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    Ok((s, packets))
}

fn payload(p: &[Packet]) -> Vec<u8> {
    p.iter().flat_map(|p| p.data.clone()).collect()
}

#[test]
fn lpcm_in_both_byte_orders_and_float() {
    let v: [i16; 4] = [1, -2, 32767, -32768];
    let be: Vec<u8> = v.iter().flat_map(|x| x.to_be_bytes()).collect();
    let le: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    for (flags, body) in [(0u32, &be), (2, &le)] {
        let (s, p) = demux(caf(&[
            (b"desc", desc(44_100.0, b"lpcm", flags, 4, 1, 2, 16)),
            (b"data", data(body)),
        ]))
        .unwrap();
        assert_eq!(
            (s.codec_id, s.channels, s.sample_rate),
            (CodecId::Pcm, 2, 44_100)
        );
        assert_eq!(s.sample_format, Some(SampleFormat::S16));
        assert_eq!(payload(&p), le);
        assert_eq!(p[0].duration, 2);
    }
    let f: Vec<u8> = 0.25f32.to_be_bytes().to_vec();
    let (s, p) = demux(caf(&[
        (b"desc", desc(48_000.0, b"lpcm", 1, 4, 1, 1, 32)),
        (b"data", data(&f)),
    ]))
    .unwrap();
    assert_eq!(s.sample_format, Some(SampleFormat::F32));
    assert_eq!(payload(&p), 0.25f32.to_le_bytes());
    let (s, p) = demux(caf(&[
        (b"desc", desc(48_000.0, b"lpcm", 0, 3, 1, 1, 24)),
        (b"data", data(&[1, 2, 3])),
    ]))
    .unwrap();
    assert_eq!(s.sample_format, Some(SampleFormat::S24));
    assert_eq!(payload(&p), [3, 2, 1]);
}

#[test]
fn constant_coded_formats() {
    let bytes: Vec<u8> = (0..200).map(|i| i as u8).collect();
    let (s, p) = demux(caf(&[
        (b"desc", desc(8000.0, b"ulaw", 0, 1, 1, 1, 8)),
        (b"data", data(&bytes)),
    ]))
    .unwrap();
    assert_eq!(
        (s.codec_id, s.sample_format),
        (CodecId::PcmMulaw, Some(SampleFormat::S16))
    );
    assert_eq!(payload(&p), bytes);
    // ima4 stereo: 68-byte frames of 64 samples; a trailing partial frame is dropped.
    let ima = vec![0u8; 68 * 100 + 10];
    let (s, p) = demux(caf(&[
        (b"desc", desc(44_100.0, b"ima4", 0, 68, 64, 2, 0)),
        (b"data", data(&ima)),
    ]))
    .unwrap();
    assert_eq!(s.codec_id, CodecId::AdpcmImaQt);
    assert_eq!(p.iter().map(|p| p.duration).sum::<i64>(), 6400);
    assert!(p.iter().all(|p| p.data.len() % 68 == 0));
}

#[test]
fn variable_packets_follow_the_packet_table() {
    let packets: [&[u8]; 3] = [&[1, 1, 1], &[2; 200], &[3]];
    let table = varints(&[3, 200, 1]);
    let body: Vec<u8> = packets.concat();
    let cookie = vec![
        0u8, 0, 16, 0, 0, 16, 40, 10, 14, 2, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xAC, 0x44,
    ];
    let d = desc(44_100.0, b"alac", 1, 0, 4096, 2, 0);
    for layout in 0..3 {
        let file = match layout {
            // Usual: pakt before data (streams).
            0 => caf(&[
                (b"desc", d.clone()),
                (b"kuki", cookie.clone()),
                (b"pakt", pakt(3, 10_000, 0, &table)),
                (b"data", data(&body)),
            ]),
            // pakt after data (buffered).
            1 => caf(&[
                (b"desc", d.clone()),
                (b"kuki", cookie.clone()),
                (b"data", data(&body)),
                (b"pakt", pakt(3, 10_000, 0, &table)),
            ]),
            // data of unknown size (-1), last.
            _ => {
                let mut f = caf(&[
                    (b"desc", d.clone()),
                    (b"kuki", cookie.clone()),
                    (b"pakt", pakt(3, 10_000, 0, &table)),
                ]);
                chunk(&mut f, b"data", -1, &data(&body));
                f
            }
        };
        let (s, p) = demux(file).unwrap();
        assert_eq!(s.codec_id, CodecId::Alac);
        assert_eq!(s.extradata, cookie);
        assert_eq!(s.duration, Some(10_000));
        let got: Vec<Vec<u8>> = p.iter().map(|p| p.data.clone()).collect();
        assert_eq!(got, packets.map(<[u8]>::to_vec), "layout {layout}");
        assert_eq!(
            p.iter().map(|p| p.pts.unwrap()).collect::<Vec<_>>(),
            [0, 4096, 8192]
        );
    }
    // A table that runs past the data stops at the data's end.
    let short = caf(&[
        (b"desc", d.clone()),
        (b"kuki", cookie.clone()),
        (b"pakt", pakt(3, 10_000, 0, &table)),
        (b"data", data(&body[..100])),
    ]);
    let (_, p) = demux(short).unwrap();
    assert_eq!(p.len(), 1);
    // Variable-size packets without any pakt are an error.
    assert!(demux(caf(&[(b"desc", d), (b"data", data(&body))])).is_err());
}

#[test]
fn aac_cookie_yields_the_audio_specific_config() {
    let asc = [0x12u8, 0x10];
    // ES_Descriptor(3) > DecoderConfigDescriptor(4) > DecoderSpecificInfo(5).
    let mut dsi = vec![0x05, 0x80, 0x80, 0x80, asc.len() as u8];
    dsi.extend_from_slice(&asc);
    let mut dcd = vec![
        0x04,
        0x80,
        0x80,
        0x80,
        (13 + dsi.len()) as u8,
        0x40,
        0x15,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    dcd.extend_from_slice(&dsi);
    let mut es = vec![0x03, 0x80, 0x80, 0x80, (3 + dcd.len()) as u8, 0, 0, 0];
    es.extend_from_slice(&dcd);
    assert_eq!(aac_config(&es), asc);
    let mut wrapped = vec![0, 0, 0, 0];
    wrapped.extend_from_slice(&es);
    assert_eq!(aac_config(&wrapped), asc);
    assert_eq!(aac_config(&asc), asc);
    let d = desc(44_100.0, b"aac ", 2, 0, 1024, 2, 0);
    let file = caf(&[
        (b"desc", d),
        (b"kuki", es),
        (b"pakt", pakt(1, 1024, 2112, &varints(&[4]))),
        (b"data", data(&[9; 4])),
    ]);
    let (s, p) = demux(file).unwrap();
    assert_eq!(
        (s.codec_id, s.extradata.as_slice()),
        (CodecId::Aac, &asc[..])
    );
    assert_eq!(s.codec_delay_ns, 2112 * 1_000_000_000 / 44_100);
    assert_eq!(p.len(), 1);
}

#[test]
fn errors_and_damage() {
    assert_eq!(probe_caf(b"RIFF\0\0\0\0"), 0);
    assert!(demux(caf(&[(b"data", data(&[0; 4]))])).is_err());
    assert!(demux(caf(&[
        (b"desc", desc(44_100.0, b"opus", 0, 0, 960, 2, 0)),
        (b"data", data(&[0; 4]))
    ]))
    .is_err());
    assert!(demux(caf(&[
        (b"desc", desc(0.0, b"lpcm", 0, 2, 1, 1, 16)),
        (b"data", data(&[0; 4]))
    ]))
    .is_err());
    assert!(demux(caf(&[
        (b"desc", desc(8000.0, b"lpcm", 0, 2, 1, 0, 16)),
        (b"data", data(&[0; 4]))
    ]))
    .is_err());
    let good = caf(&[
        (b"desc", desc(44_100.0, b"alac", 1, 0, 4096, 2, 0)),
        (b"kuki", vec![0; 24]),
        (b"pakt", pakt(4, 16_000, 0, &varints(&[10, 20, 30, 5]))),
        (b"data", data(&[7; 65])),
    ]);
    for cut in 0..good.len() {
        let _ = demux(good[..cut].to_vec());
    }
    let mut x = 0xBEEF_u32;
    for _ in 0..300 {
        let mut g = good.clone();
        for _ in 0..4 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let at = 8 + x as usize % (g.len() - 8);
            g[at] = (x >> 24) as u8;
        }
        let _ = demux(g);
    }
}

#[test]
fn registers_and_probes() {
    let mut reg = FormatRegistry::new();
    register(&mut reg);
    let file = caf(&[
        (b"desc", desc(8000.0, b"lpcm", 0, 2, 1, 1, 16)),
        (b"data", data(&[0; 2])),
    ]);
    assert_eq!(reg.probe(&file).map(|f| f.name), Some("caf"));
}
