//! The RTP receiver against real senders.
//!
//! The loopback tests packetize a `rusty_jpeg` frame the way RFC 2435 says
//! (and an H.264 access unit the way RFC 6184 says), push it through a UDP
//! socket into [`RtpReader`], and check that what comes out decodes to the
//! same pixels. The ffmpeg tests are the external oracle — ffmpeg's `-f rtp`
//! muxer is the sender everyone else interoperates with — and are `#[ignore]`d
//! because they spawn a process; run them with
//! `cargo test -p rff-io --test rtp_ffmpeg -- --ignored`.

use std::io::Read;
use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::time::Duration;

use rff_io::rtp::{
    parse_frame_header, RtpReader, CODEC_H264, CODEC_JPEG, FLAG_KEYFRAME, FRAME_HEADER_LEN,
    HEADER_LEN, PT_JPEG,
};

/// Split a record stream into (codec, keyframe, timestamp, bytes).
fn records(mut stream: &[u8]) -> Vec<(u8, bool, u32, Vec<u8>)> {
    let mut out = Vec::new();
    while !stream.is_empty() {
        let head: [u8; FRAME_HEADER_LEN] = stream[..FRAME_HEADER_LEN].try_into().unwrap();
        let h = parse_frame_header(&head).unwrap();
        let data = &stream[FRAME_HEADER_LEN..FRAME_HEADER_LEN + h.len];
        out.push((
            h.codec,
            h.flags & FLAG_KEYFRAME != 0,
            h.timestamp,
            data.to_vec(),
        ));
        stream = &stream[FRAME_HEADER_LEN + h.len..];
    }
    out
}

fn rtp_packet(pt: u8, marker: bool, seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER_LEN + payload.len());
    p.push(0x80);
    p.push(pt | if marker { 0x80 } else { 0 });
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&0x1234_5678u32.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

fn test_rgb(w: u16, h: u16, seed: u32) -> Vec<u8> {
    (0..u32::from(w) * u32::from(h))
        .flat_map(|i| {
            let x = i % u32::from(w);
            let y = i / u32::from(w);
            [
                (x * 255 / u32::from(w)) as u8,
                (y * 255 / u32::from(h)) as u8,
                ((x + y + seed) * 3 % 256) as u8,
            ]
        })
        .collect()
}

/// A baseline JPEG from the house encoder, at quality 75 (4:2:0, Annex K
/// Huffman tables — the RFC 2435 precondition).
fn house_jpeg(w: u16, h: u16, seed: u32) -> Vec<u8> {
    let mut out = Vec::new();
    rusty_jpeg::encode::Encoder::new(&mut out, 75)
        .encode(
            &test_rgb(w, h, seed),
            w,
            h,
            rusty_jpeg::encode::ColorType::Rgb,
        )
        .unwrap();
    out
}

/// What RFC 2435 needs from a baseline JPEG: tables, geometry, the scan.
struct Scan {
    width: u16,
    height: u16,
    /// 0 = 4:2:2, 1 = 4:2:0.
    type_: u8,
    restart: Option<u16>,
    /// Two 64-byte tables, zigzag order (as in the DQT).
    qtables: Vec<[u8; 64]>,
    data: Vec<u8>,
}

fn parse_scan(jpeg: &[u8]) -> Scan {
    let mut s = Scan {
        width: 0,
        height: 0,
        type_: 0,
        restart: None,
        qtables: vec![[0; 64]; 2],
        data: Vec::new(),
    };
    let mut i = 2;
    loop {
        assert_eq!(jpeg[i], 0xFF);
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        let body = &jpeg[i + 4..i + 2 + len];
        match marker {
            0xDB => {
                let mut p = 0;
                while p < body.len() {
                    assert_eq!(body[p] >> 4, 0, "8-bit tables only");
                    let id = usize::from(body[p] & 0x0F);
                    s.qtables[id].copy_from_slice(&body[p + 1..p + 65]);
                    p += 65;
                }
            }
            0xC0 => {
                s.height = u16::from_be_bytes([body[1], body[2]]);
                s.width = u16::from_be_bytes([body[3], body[4]]);
                s.type_ = match body[7] {
                    0x22 => 1,
                    0x21 => 0,
                    hv => panic!("sampling {hv:#x} is not an RFC 2435 type"),
                };
            }
            0xDD => s.restart = Some(u16::from_be_bytes([body[0], body[1]])),
            0xDA => {
                let start = i + 2 + len;
                let end = jpeg.len() - 2;
                assert_eq!(&jpeg[end..], &[0xFF, 0xD9]);
                s.data = jpeg[start..end].to_vec();
                return s;
            }
            _ => {}
        }
        i += 2 + len;
    }
}

/// RFC 2435 packets for one frame: quantisation tables in-band (`Q = 255`),
/// the restart header when the JPEG has a DRI, `mtu`-byte payloads.
fn packetize_jpeg(jpeg: &[u8], seq: &mut u16, ts: u32, mtu: usize) -> Vec<Vec<u8>> {
    let s = parse_scan(jpeg);
    let mut packets = Vec::new();
    let mut off = 0usize;
    let type_ = s.type_ | if s.restart.is_some() { 64 } else { 0 };
    while off < s.data.len() {
        let mut payload = vec![
            0,
            (off >> 16) as u8,
            (off >> 8) as u8,
            off as u8,
            type_,
            255,
            (s.width / 8) as u8,
            (s.height / 8) as u8,
        ];
        if let Some(ri) = s.restart {
            payload.extend_from_slice(&ri.to_be_bytes());
            payload.extend_from_slice(&0xFFFFu16.to_be_bytes());
        }
        if off == 0 {
            payload.extend_from_slice(&[0, 0, 0, 128]);
            payload.extend_from_slice(&s.qtables[0]);
            payload.extend_from_slice(&s.qtables[1]);
        }
        let room = mtu - payload.len();
        let take = room.min(s.data.len() - off);
        payload.extend_from_slice(&s.data[off..off + take]);
        off += take;
        packets.push(rtp_packet(PT_JPEG, off == s.data.len(), *seq, ts, &payload));
        *seq = seq.wrapping_add(1);
    }
    packets
}

fn decode_rgb(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
    let mut d = rusty_jpeg::decode::Decoder::new(jpeg);
    let px = d.decode().expect("decode");
    let info = d.info().unwrap();
    (info.width, info.height, px)
}

/// Bind an ephemeral loopback port for the reader; the sender gets its address.
fn loopback() -> (UdpSocket, std::net::SocketAddr) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    (sock, addr)
}

#[test]
fn jpeg_over_rtp_loopback_rebuilds_a_decodable_frame() {
    let frames: Vec<Vec<u8>> = (0..3).map(|i| house_jpeg(64, 48, i)).collect();
    let (sock, addr) = loopback();
    let sender = {
        let frames = frames.clone();
        std::thread::spawn(move || {
            let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut seq = 65_530u16; // wraps mid-stream
            for (i, f) in frames.iter().enumerate() {
                for p in packetize_jpeg(f, &mut seq, 9000 * i as u32, 1400) {
                    tx.send_to(&p, addr).unwrap();
                }
            }
        })
    };
    let mut reader = RtpReader::with_socket(sock, None, Duration::from_millis(500)).unwrap();
    assert_eq!(reader.format_name(), "rtp");
    assert_eq!(reader.codec(), CODEC_JPEG);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    sender.join().unwrap();

    let recs = records(&out);
    assert_eq!(recs.len(), 3, "stats: {:?}", reader.stats);
    for (i, (codec, key, ts, jpeg)) in recs.iter().enumerate() {
        assert_eq!((*codec, *key, *ts), (CODEC_JPEG, true, 9000 * i as u32));
        // The rebuilt file has the RFC's regenerated headers, not the sender's,
        // so compare pixels, not bytes.
        let (w, h, px) = decode_rgb(jpeg);
        let (w2, h2, px2) = decode_rgb(&frames[i]);
        assert_eq!((w, h), (w2, h2));
        assert_eq!(px, px2, "frame {i} decodes to different pixels");
    }
    assert_eq!(reader.stats.lost, 0);
    assert_eq!(reader.stats.dropped, 0);
}

#[test]
fn jpeg_loss_drops_only_the_affected_frame() {
    let frames: Vec<Vec<u8>> = (0..3).map(|i| house_jpeg(96, 64, i + 10)).collect();
    let (sock, addr) = loopback();
    let sender = {
        let frames = frames.clone();
        std::thread::spawn(move || {
            let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut seq = 0u16;
            for (i, f) in frames.iter().enumerate() {
                // Small payloads: the smooth test picture codes to ~1 KB, and the
                // test needs at least three fragments to lose the middle one.
                let packets = packetize_jpeg(f, &mut seq, 3000 * i as u32, 256);
                assert!(packets.len() >= 3, "need fragments to lose one");
                for (j, p) in packets.iter().enumerate() {
                    if i == 1 && j == 1 {
                        continue; // the second fragment of frame 1 never arrives
                    }
                    tx.send_to(p, addr).unwrap();
                }
            }
        })
    };
    let mut reader = RtpReader::with_socket(sock, None, Duration::from_millis(500)).unwrap();
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    sender.join().unwrap();
    let recs = records(&out);
    assert_eq!(recs.len(), 2, "frames 0 and 2 survive");
    assert_eq!(recs[0].2, 0);
    assert_eq!(recs[1].2, 6000);
    assert_eq!(reader.stats.lost, 1);
    assert_eq!(reader.stats.dropped, 1);
}

#[test]
fn h264_over_rtp_loopback_reassembles_the_access_unit() {
    let sps = vec![0x67u8, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0x88, 0x0F];
    let pps = vec![0x68u8, 0xCE, 0x3C, 0x80];
    let mut idr = vec![0x65u8, 0x88, 0x84];
    idr.extend((0..5000u32).map(|i| (i * 7 % 250 + 1) as u8));
    let (sock, addr) = loopback();
    let sender = {
        let (sps, pps, idr) = (sps.clone(), pps.clone(), idr.clone());
        std::thread::spawn(move || {
            let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut seq = 100u16;
            // SPS + PPS as one STAP-A.
            let mut stap = vec![0x78u8];
            for nal in [&sps, &pps] {
                stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                stap.extend_from_slice(nal);
            }
            tx.send_to(&rtp_packet(96, false, seq, 90_000, &stap), addr)
                .unwrap();
            seq += 1;
            // The IDR as FU-A fragments of 1200 bytes.
            let body = &idr[1..];
            let pieces: Vec<&[u8]> = body.chunks(1200).collect();
            for (i, piece) in pieces.iter().enumerate() {
                let mut fu = vec![(idr[0] & 0xE0) | 28, idr[0] & 0x1F];
                if i == 0 {
                    fu[1] |= 0x80;
                }
                let last = i + 1 == pieces.len();
                if last {
                    fu[1] |= 0x40;
                }
                fu.extend_from_slice(piece);
                tx.send_to(&rtp_packet(96, last, seq, 90_000, &fu), addr)
                    .unwrap();
                seq += 1;
            }
            // A second AU: one P slice, single NAL packet.
            tx.send_to(
                &rtp_packet(96, true, seq, 93_600, &[0x41, 0x9A, 0x02]),
                addr,
            )
            .unwrap();
        })
    };
    let mut reader = RtpReader::with_socket(sock, None, Duration::from_millis(500)).unwrap();
    assert_eq!(reader.codec(), CODEC_H264);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    sender.join().unwrap();
    let recs = records(&out);
    assert_eq!(recs.len(), 2, "stats: {:?}", reader.stats);
    let mut want = Vec::new();
    for nal in [&sps, &pps, &idr] {
        want.extend_from_slice(&[0, 0, 0, 1]);
        want.extend_from_slice(nal);
    }
    assert_eq!(recs[0].3, want);
    assert!(recs[0].1, "IDR is a keyframe");
    assert_eq!(recs[0].2, 90_000);
    assert_eq!(recs[1].3, [0, 0, 0, 1, 0x41, 0x9A, 0x02]);
    assert!(!recs[1].1);
}

// ---------------------------------------------------------------------------
// ffmpeg as the sender (the oracle). `#[ignore]`: spawns a process.
// ---------------------------------------------------------------------------

fn spawn_ffmpeg_rtp(port: u16, codec_args: &[&str]) -> std::process::Child {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=10",
        "-frames:v",
        "15",
    ]);
    cmd.args(codec_args);
    cmd.args(["-f", "rtp", &format!("rtp://127.0.0.1:{port}")]);
    cmd.stdout(Stdio::null()).stderr(Stdio::inherit());
    cmd.spawn().expect("ffmpeg on PATH")
}

#[test]
#[ignore = "spawns ffmpeg"]
fn ffmpeg_jpeg_over_rtp_is_received_and_decodes() {
    let (sock, addr) = loopback();
    let mut child = spawn_ffmpeg_rtp(
        addr.port(),
        &[
            "-pix_fmt", "yuvj420p", "-c:v", "mjpeg", "-huffman", "default",
        ],
    );
    let mut reader = RtpReader::with_socket(sock, None, Duration::from_secs(3)).unwrap();
    assert_eq!(reader.codec(), CODEC_JPEG);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    let _ = child.wait();
    let recs = records(&out);
    eprintln!(
        "[rtp/jpeg] ffmpeg: {} frames, stats {:?}",
        recs.len(),
        reader.stats
    );
    assert!(recs.len() >= 12, "expected ~15 frames, got {}", recs.len());
    for (_, _, _, jpeg) in &recs {
        let (w, h, _) = decode_rgb(jpeg);
        assert_eq!((w, h), (320, 240));
    }
    // 10 fps on a 90 kHz clock: 9000 ticks apart.
    assert_eq!(recs[1].2.wrapping_sub(recs[0].2), 9000);
}

#[test]
#[ignore = "spawns ffmpeg with libx264"]
fn ffmpeg_h264_over_rtp_is_received_with_parameter_sets() {
    let (sock, addr) = loopback();
    let mut child = spawn_ffmpeg_rtp(
        addr.port(),
        &[
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-x264-params",
            "repeat-headers=1:keyint=5",
        ],
    );
    let mut reader = RtpReader::with_socket(sock, None, Duration::from_secs(3)).unwrap();
    assert_eq!(reader.codec(), CODEC_H264);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    let _ = child.wait();
    let recs = records(&out);
    eprintln!(
        "[rtp/h264] ffmpeg: {} AUs, stats {:?}",
        recs.len(),
        reader.stats
    );
    assert!(recs.len() >= 12, "expected ~15 AUs, got {}", recs.len());
    let nal_types = |au: &[u8]| -> Vec<u8> {
        let mut v = Vec::new();
        let mut i = 0;
        while i + 4 < au.len() {
            if au[i..i + 4] == [0, 0, 0, 1] {
                v.push(au[i + 4] & 0x1F);
                i += 4;
            } else {
                i += 1;
            }
        }
        v
    };
    let first = nal_types(&recs[0].3);
    assert!(
        first.contains(&7) && first.contains(&8) && first.contains(&5),
        "{first:?}"
    );
    assert!(recs[0].1, "first AU is a keyframe");
    let keyframes = recs.iter().filter(|r| r.1).count();
    assert!(keyframes >= 2, "keyint=5 over 15 frames: {keyframes}");
}
