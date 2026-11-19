//! Fixtures in `testdata/` were encoded by an independent ALAC encoder (a
//! Rust port of Apple's reference encoder) from the `.wav` beside each `.caf`.
//! ALAC is lossless, so decoding must give back those samples exactly.

use super::*;

/// The cookie and packets of a fixture CAF (its `kuki`, `pakt` and `data`).
fn caf_packets(caf: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut at = 8;
    let (mut cookie, mut sizes, mut data) = (Vec::new(), Vec::new(), Vec::new());
    while at + 12 <= caf.len() {
        let id = &caf[at..at + 4];
        let size = i64::from_be_bytes(caf[at + 4..at + 12].try_into().unwrap()) as usize;
        let body = &caf[at + 12..at + 12 + size];
        match id {
            b"kuki" => cookie = body.to_vec(),
            b"pakt" => {
                let count = i64::from_be_bytes(body[0..8].try_into().unwrap()) as usize;
                let mut p = 24;
                for _ in 0..count {
                    let mut v = 0usize;
                    loop {
                        let b = body[p];
                        p += 1;
                        v = (v << 7) | (b & 0x7F) as usize;
                        if b & 0x80 == 0 {
                            break;
                        }
                    }
                    sizes.push(v);
                }
            }
            b"data" => data = body[4..].to_vec(),
            _ => {}
        }
        at += 12 + size;
    }
    let mut packets = Vec::new();
    let mut p = 0;
    for s in sizes {
        packets.push(data[p..p + s].to_vec());
        p += s;
    }
    (cookie, packets)
}

/// The `data` chunk of a fixture WAV and its bits per sample.
fn wav_samples(wav: &[u8]) -> (Vec<u8>, u16) {
    let mut at = 12;
    let (mut data, mut bits) = (Vec::new(), 0);
    while at + 8 <= wav.len() {
        let size = u32::from_le_bytes(wav[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &wav[at + 8..at + 8 + size];
        match &wav[at..at + 4] {
            b"fmt " => bits = u16::from_le_bytes([body[14], body[15]]),
            b"data" => data = body.to_vec(),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    (data, bits)
}

fn decode_all(cookie: &[u8], packets: &[Vec<u8>]) -> (SampleFormat, u16, Vec<u8>) {
    let mut dec = AlacDecoder::default();
    dec.configure(&CodecParams {
        codec_id: CodecId::Alac,
        extradata: cookie.to_vec(),
        ..Default::default()
    })
    .unwrap();
    let mut out = Vec::new();
    let (mut format, mut channels) = (SampleFormat::S16, 0);
    for p in packets {
        dec.send_packet(&Packet::from_data(0, p.clone())).unwrap();
        let Frame::Audio(af) = dec.receive_frame().unwrap() else {
            unreachable!()
        };
        format = af.format;
        channels = af.channels;
        out.extend_from_slice(&af.planes[0]);
    }
    (format, channels, out)
}

fn check_lossless(caf: &[u8], wav: &[u8], want_channels: u16) {
    let (cookie, packets) = caf_packets(caf);
    let (pcm, bits) = wav_samples(wav);
    let (format, channels, out) = decode_all(&cookie, &packets);
    assert_eq!(channels, want_channels);
    match bits {
        16 => {
            assert_eq!(format, SampleFormat::S16);
            assert_eq!(out, pcm);
        }
        24 => {
            assert_eq!(format, SampleFormat::F32);
            let want: Vec<i32> = pcm
                .chunks_exact(3)
                .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8)
                .collect();
            let got: Vec<i32> = out
                .chunks_exact(4)
                .map(|c| (f32::from_le_bytes([c[0], c[1], c[2], c[3]]) * 8_388_608.0) as i32)
                .collect();
            assert_eq!(got, want);
        }
        _ => unreachable!(),
    }
}

#[test]
fn mono_16_bit_is_lossless() {
    check_lossless(
        include_bytes!("../testdata/m16.caf"),
        include_bytes!("../testdata/m16.wav"),
        1,
    );
}

#[test]
fn stereo_24_bit_with_shifted_bytes_is_lossless() {
    check_lossless(
        include_bytes!("../testdata/s24.caf"),
        include_bytes!("../testdata/s24.wav"),
        2,
    );
}

#[test]
fn six_channels_come_back_in_wav_order() {
    // ALAC stores 5.1 as C, L R, Ls Rs, LFE; output must be L R C LFE Ls Rs,
    // which is the order of the source WAV (each channel a distinct tone).
    check_lossless(
        include_bytes!("../testdata/c6.caf"),
        include_bytes!("../testdata/c6.wav"),
        6,
    );
}

#[test]
fn cookie_wrappings_all_parse() {
    let (cookie, _) = caf_packets(include_bytes!("../testdata/s24.caf"));
    let bare = AlacConfig::from_cookie(&cookie).unwrap();
    assert_eq!(
        (bare.bit_depth, bare.channels, bare.frame_length),
        (24, 2, 1024)
    );
    // FFmpeg-style 36-byte atom.
    let mut atom = 36u32.to_be_bytes().to_vec();
    atom.extend_from_slice(b"alac");
    atom.extend_from_slice(&[0; 4]);
    atom.extend_from_slice(&cookie[..24]);
    assert_eq!(AlacConfig::from_cookie(&atom), Some(bare));
    // CAF cookie with a leading `frma` atom.
    let mut frma = 12u32.to_be_bytes().to_vec();
    frma.extend_from_slice(b"frmaalac");
    frma.extend_from_slice(&atom);
    assert_eq!(AlacConfig::from_cookie(&frma), Some(bare));
    // Garbage and an impossible depth are refused.
    assert_eq!(AlacConfig::from_cookie(&[0; 10]), None);
    let mut bad = cookie[..24].to_vec();
    bad[5] = 17;
    assert_eq!(AlacConfig::from_cookie(&bad), None);
}

#[test]
fn uncompressed_escape_frame() {
    // A hand-built 16-bit stereo CPE frame with the escape (verbatim) flag:
    // tag 1, instance 0, 12 unused bits, has_size 1, shift 0, escape 1, then
    // 32-bit size 3, then L/R pairs, then END.
    let samples: [(i16, i16); 3] = [(1, -1), (32767, -32768), (-1234, 5678)];
    let mut bits: Vec<bool> = Vec::new();
    let mut put = |v: u32, n: u32| {
        for i in (0..n).rev() {
            bits.push(v >> i & 1 == 1);
        }
    };
    put(1, 3);
    put(0, 4);
    put(0, 12);
    put(1, 1);
    put(0, 2);
    put(1, 1);
    put(3, 32);
    for (l, r) in samples {
        put(l as u16 as u32, 16);
        put(r as u16 as u32, 16);
    }
    put(7, 3);
    let bytes: Vec<u8> = bits
        .chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |a, (i, &b)| a | (b as u8) << (7 - i))
        })
        .collect();
    let mut cookie = 4096u32.to_be_bytes().to_vec();
    cookie.extend_from_slice(&[0, 16, 40, 10, 14, 2, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0]);
    cookie.extend_from_slice(&44_100u32.to_be_bytes());
    let (_, _, out) = decode_all(&cookie, &[bytes]);
    let want: Vec<u8> = samples
        .iter()
        .flat_map(|&(l, r)| [l.to_le_bytes(), r.to_le_bytes()].concat())
        .collect();
    assert_eq!(out, want);
}

#[test]
fn damaged_packets_error_without_panicking() {
    let (cookie, packets) = caf_packets(include_bytes!("../testdata/c6.caf"));
    let mut dec = AlacDecoder::default();
    dec.configure(&CodecParams {
        extradata: cookie,
        ..Default::default()
    })
    .unwrap();
    let good = &packets[0];
    for cut in 0..good.len() {
        let _ = dec.send_packet(&Packet::from_data(0, good[..cut].to_vec()));
        let _ = dec.receive_frame();
    }
    let mut x = 0x1357_9BDFu32;
    for i in 0..2000 {
        let mut p = good.clone();
        for _ in 0..1 + i % 8 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let at = x as usize % p.len();
            p[at] ^= 1 << (x >> 29);
        }
        let _ = dec.send_packet(&Packet::from_data(0, p));
        let _ = dec.receive_frame();
    }
    // After all that, a clean packet still decodes.
    dec.send_packet(&Packet::from_data(0, good.clone()))
        .unwrap();
    assert!(dec.receive_frame().is_ok());
}
