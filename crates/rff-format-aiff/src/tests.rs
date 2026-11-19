use std::io::Cursor;

use super::*;

/// An 80-bit extended encoding of an integer rate.
fn extended(rate: u32) -> [u8; 10] {
    let mut out = [0u8; 10];
    let shift = rate.leading_zeros();
    let exp = 16383 + 31 - shift as u16;
    out[0..2].copy_from_slice(&exp.to_be_bytes());
    let mantissa = (rate as u64) << (32 + shift);
    out[2..10].copy_from_slice(&mantissa.to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

fn comm(channels: u16, bits: u16, rate: u32, compression: Option<&[u8; 4]>) -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(&channels.to_be_bytes());
    c.extend_from_slice(&0u32.to_be_bytes()); // frame count: ignored
    c.extend_from_slice(&bits.to_be_bytes());
    c.extend_from_slice(&extended(rate));
    if let Some(t) = compression {
        c.extend_from_slice(t);
        c.extend_from_slice(&[0, 0]); // empty pascal-string name, padded
    }
    c
}

fn ssnd(data: &[u8], offset: u32) -> Vec<u8> {
    let mut s = Vec::new();
    s.extend_from_slice(&offset.to_be_bytes());
    s.extend_from_slice(&0u32.to_be_bytes());
    s.extend(std::iter::repeat_n(0xEE, offset as usize));
    s.extend_from_slice(data);
    s
}

fn form(kind: &[u8; 4], chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = kind.to_vec();
    for (id, b) in chunks {
        chunk(&mut body, id, b);
    }
    let mut out = b"FORM".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn demux(file: Vec<u8>) -> Result<(Stream, Vec<Packet>)> {
    let mut d = AiffDemuxer::new(Box::new(Cursor::new(file)));
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

fn payload(packets: &[Packet]) -> Vec<u8> {
    packets.iter().flat_map(|p| p.data.clone()).collect()
}

#[test]
fn extended_rates_round_trip() {
    for rate in [8000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000, 1] {
        assert_eq!(extended_to_f64(&extended(rate)), rate as f64);
    }
    // 44100 as Apple writes it.
    let apple = [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0];
    assert_eq!(extended_to_f64(&apple), 44_100.0);
    assert_eq!(extended_to_f64(&[0; 10]), 0.0);
}

#[test]
fn big_and_little_endian_pcm_deliver_the_same_samples() {
    let samples: [i16; 6] = [0, 1, -1, 32767, -32768, 1234];
    let be: Vec<u8> = samples.iter().flat_map(|v| v.to_be_bytes()).collect();
    let le: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
    for file in [
        form(
            b"AIFF",
            &[
                (b"COMM", comm(2, 16, 44_100, None)),
                (b"SSND", ssnd(&be, 0)),
            ],
        ),
        form(
            b"AIFC",
            &[
                (b"FVER", vec![0xA2, 0x80, 0x51, 0x40]),
                (b"COMM", comm(2, 16, 44_100, Some(b"NONE"))),
                (b"SSND", ssnd(&be, 6)),
            ],
        ),
        form(
            b"AIFC",
            &[
                (b"COMM", comm(2, 16, 44_100, Some(b"sowt"))),
                (b"SSND", ssnd(&le, 0)),
            ],
        ),
    ] {
        let (s, packets) = demux(file).unwrap();
        assert_eq!(
            (s.codec_id, s.channels, s.sample_rate),
            (CodecId::Pcm, 2, 44_100)
        );
        assert_eq!(s.sample_format, Some(SampleFormat::S16));
        assert_eq!(payload(&packets), le);
        assert_eq!(packets[0].duration, 3);
    }
}

#[test]
fn wide_and_narrow_pcm_layouts() {
    // 8-bit signed -> s16.
    let (s, p) = demux(form(
        b"AIFF",
        &[
            (b"COMM", comm(1, 8, 8000, None)),
            (b"SSND", ssnd(&[0x80, 0x7F, 0x00], 0)),
        ],
    ))
    .unwrap();
    assert_eq!(s.sample_format, Some(SampleFormat::S16));
    assert_eq!(payload(&p), [0x00, 0x80, 0x00, 0x7F, 0x00, 0x00]);
    // 24-bit and 20-bit (a 3-byte container) -> s24 little-endian.
    for bits in [24, 20] {
        let (s, p) = demux(form(
            b"AIFF",
            &[
                (b"COMM", comm(1, bits, 48_000, None)),
                (b"SSND", ssnd(&[0x12, 0x34, 0x56], 0)),
            ],
        ))
        .unwrap();
        assert_eq!(s.sample_format, Some(SampleFormat::S24));
        assert_eq!(payload(&p), [0x56, 0x34, 0x12]);
    }
    // 32-bit -> s32.
    let (s, p) = demux(form(
        b"AIFF",
        &[
            (b"COMM", comm(1, 32, 48_000, None)),
            (b"SSND", ssnd(&[1, 2, 3, 4], 0)),
        ],
    ))
    .unwrap();
    assert_eq!(s.sample_format, Some(SampleFormat::S32));
    assert_eq!(payload(&p), [4, 3, 2, 1]);
    // fl32 / fl64.
    let (s, p) = demux(form(
        b"AIFC",
        &[
            (b"COMM", comm(1, 32, 48_000, Some(b"fl32"))),
            (b"SSND", ssnd(&0.5f32.to_be_bytes(), 0)),
        ],
    ))
    .unwrap();
    assert_eq!(s.sample_format, Some(SampleFormat::F32));
    assert_eq!(payload(&p), 0.5f32.to_le_bytes());
    let (s, p) = demux(form(
        b"AIFC",
        &[
            (b"COMM", comm(1, 64, 48_000, Some(b"FL64"))),
            (b"SSND", ssnd(&(-0.25f64).to_be_bytes(), 0)),
        ],
    ))
    .unwrap();
    assert_eq!(s.sample_format, Some(SampleFormat::F64));
    assert_eq!(payload(&p), (-0.25f64).to_le_bytes());
    // raw (unsigned 8-bit).
    let (_, p) = demux(form(
        b"AIFC",
        &[
            (b"COMM", comm(1, 8, 8000, Some(b"raw "))),
            (b"SSND", ssnd(&[0x80, 0xFF], 0)),
        ],
    ))
    .unwrap();
    assert_eq!(payload(&p), [0x00, 0x00, 0x00, 0x7F]);
}

#[test]
fn coded_types_pass_through_to_their_decoders() {
    let bytes: Vec<u8> = (0..=255).collect();
    for (t, codec) in [(b"ulaw", CodecId::PcmMulaw), (b"ALAW", CodecId::PcmAlaw)] {
        let (s, p) = demux(form(
            b"AIFC",
            &[
                (b"COMM", comm(2, 16, 8000, Some(t))),
                (b"SSND", ssnd(&bytes, 0)),
            ],
        ))
        .unwrap();
        assert_eq!(
            (s.codec_id, s.sample_format),
            (codec, Some(SampleFormat::S16))
        );
        assert_eq!(payload(&p), bytes);
        assert_eq!(p.iter().map(|p| p.duration).sum::<i64>(), 128);
    }
    // ima4 stereo: frames of 2 x 34 bytes, 64 samples each; a partial frame is dropped.
    let data = vec![0u8; 68 * 70 + 30];
    let (s, p) = demux(form(
        b"AIFC",
        &[
            (b"COMM", comm(2, 16, 44_100, Some(b"ima4"))),
            (b"SSND", ssnd(&data, 0)),
        ],
    ))
    .unwrap();
    assert_eq!(s.codec_id, CodecId::AdpcmImaQt);
    assert!(p.iter().all(|p| p.data.len() % 68 == 0));
    assert_eq!(p.iter().map(|p| p.duration).sum::<i64>(), 70 * 64);
    assert_eq!((p[0].pts, p[1].pts), (Some(0), Some(64 * 64)));
}

#[test]
fn ssnd_before_comm_and_unknown_chunks() {
    let be: Vec<u8> = [5i16, -5].iter().flat_map(|v| v.to_be_bytes()).collect();
    let file = form(
        b"AIFF",
        &[
            (b"NAME", b"odd".to_vec()),
            (b"SSND", ssnd(&be, 2)),
            (b"MARK", vec![0; 7]),
            (b"COMM", comm(1, 16, 22_050, None)),
        ],
    );
    let (s, p) = demux(file).unwrap();
    assert_eq!(s.sample_rate, 22_050);
    assert_eq!(
        payload(&p),
        [5i16, -5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>()
    );
}

#[test]
fn errors_and_damage() {
    assert_eq!(probe_aiff(b"RIFF\0\0\0\0WAVE"), 0);
    // No COMM, no SSND, zero channels, unknown compression: errors, not panics.
    assert!(demux(form(b"AIFF", &[(b"SSND", ssnd(&[0; 4], 0))])).is_err());
    assert!(demux(form(b"AIFF", &[(b"COMM", comm(1, 16, 8000, None))])).is_err());
    assert!(demux(form(
        b"AIFF",
        &[
            (b"COMM", comm(0, 16, 8000, None)),
            (b"SSND", ssnd(&[0; 4], 0))
        ]
    ))
    .is_err());
    assert!(demux(form(
        b"AIFC",
        &[
            (b"COMM", comm(1, 16, 8000, Some(b"MAC3"))),
            (b"SSND", ssnd(&[0; 4], 0))
        ]
    ))
    .is_err());
    // An SSND whose size runs past the file: the samples that are there.
    let mut file = form(
        b"AIFF",
        &[
            (b"COMM", comm(1, 16, 8000, None)),
            (b"SSND", ssnd(&[0, 1, 0, 2, 0, 3], 0)),
        ],
    );
    let at = file.len() - 6 - 8 - 4;
    file[at..at + 4].copy_from_slice(&0x7FFF_FFF0u32.to_be_bytes());
    let (_, p) = demux(file.clone()).unwrap();
    assert_eq!(payload(&p), [1, 0, 2, 0, 3, 0]);
    // Every truncation and random garbage behind a FORM header: no panic.
    for cut in 0..file.len() {
        let _ = demux(file[..cut].to_vec());
    }
    let mut x = 0xACE1_u32;
    for _ in 0..200 {
        let mut g = file[..12].to_vec();
        for _ in 0..200 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            g.push(x as u8);
        }
        let _ = demux(g);
    }
}

#[test]
fn registers_and_probes() {
    let mut reg = FormatRegistry::new();
    register(&mut reg);
    let file = form(
        b"AIFC",
        &[
            (b"COMM", comm(1, 16, 8000, Some(b"NONE"))),
            (b"SSND", ssnd(&[0; 2], 0)),
        ],
    );
    assert_eq!(reg.probe(&file).map(|f| f.name), Some("aiff"));
    assert_eq!(reg.by_extension("aifc").map(|f| f.name), Some("aiff"));
}

/// Case variants and QuickTime's little-endian `23ni` / `42ni` (#125 follow-up:
/// symphonia accepts all of these, and they occur in the wild).
#[test]
fn compression_types_ignore_case_and_take_the_little_endian_spellings() {
    let s16: [i16; 4] = [0, 1, -1, 1234];
    let be16: Vec<u8> = s16.iter().flat_map(|v| v.to_be_bytes()).collect();
    let le16: Vec<u8> = s16.iter().flat_map(|v| v.to_le_bytes()).collect();
    for (tag, data) in [
        (b"none", be16.as_slice()),
        (b"TWOS", be16.as_slice()),
        (b"Twos", be16.as_slice()),
        (b"SOWT", le16.as_slice()),
    ] {
        let file = form(
            b"AIFC",
            &[
                (b"COMM", comm(2, 16, 8000, Some(tag))),
                (b"SSND", ssnd(data, 0)),
            ],
        );
        let (s, p) = demux(file).unwrap();
        assert_eq!(s.sample_format, Some(SampleFormat::S16), "{tag:?}");
        assert_eq!(payload(&p), le16, "{tag:?}");
    }

    let s32: [i32; 2] = [0x0102_0304, -5];
    let le32: Vec<u8> = s32.iter().flat_map(|v| v.to_le_bytes()).collect();
    let be32: Vec<u8> = s32.iter().flat_map(|v| v.to_be_bytes()).collect();
    for (tag, data) in [(b"IN32", be32.as_slice()), (b"23ni", le32.as_slice())] {
        let file = form(
            b"AIFC",
            &[
                (b"COMM", comm(1, 32, 8000, Some(tag))),
                (b"SSND", ssnd(data, 0)),
            ],
        );
        let (s, p) = demux(file).unwrap();
        assert_eq!(s.sample_format, Some(SampleFormat::S32), "{tag:?}");
        assert_eq!(payload(&p), le32, "{tag:?}");
    }

    let le24 = [0x03u8, 0x02, 0x01, 0xFB, 0xFF, 0xFF];
    let be24 = [0x01u8, 0x02, 0x03, 0xFF, 0xFF, 0xFB];
    for (tag, data) in [(b"IN24", be24.as_slice()), (b"42ni", le24.as_slice())] {
        let file = form(
            b"AIFC",
            &[
                (b"COMM", comm(1, 24, 8000, Some(tag))),
                (b"SSND", ssnd(data, 0)),
            ],
        );
        let (s, p) = demux(file).unwrap();
        assert_eq!(s.sample_format, Some(SampleFormat::S24), "{tag:?}");
        assert_eq!(payload(&p), le24, "{tag:?}");
    }

    for (tag, codec) in [(b"ALAW", CodecId::PcmAlaw), (b"Ulaw", CodecId::PcmMulaw)] {
        let file = form(
            b"AIFC",
            &[
                (b"COMM", comm(1, 16, 8000, Some(tag))),
                (b"SSND", ssnd(&[0x55; 4], 0)),
            ],
        );
        assert_eq!(demux(file).unwrap().0.codec_id, codec, "{tag:?}");
    }
}
