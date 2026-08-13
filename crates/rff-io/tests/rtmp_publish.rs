//! RTMP publish against an in-process mock server: plain handshake, AMF0
//! connect/createStream/publish command exchange, then FLV tags forwarded as
//! media messages. The mock speaks just enough server-side RTMP (chunk
//! parsing at the negotiated size, canned `_result` replies) to prove the
//! client's wire format.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use rff_io::RtmpPublisher;

const HANDSHAKE_LEN: usize = 1536;

/// What the mock observed, sent back to the test thread.
struct Observed {
    connect_seen: bool,
    publish_seen: bool,
    media: Vec<(u8, Vec<u8>)>, // (type_id, payload)
}

/// A one-connection RTMP server: handshake, reply to connect/createStream,
/// then collect messages until the peer closes.
fn mock_server(listener: TcpListener, tx: mpsc::Sender<Observed>) {
    let (mut sock, _) = listener.accept().unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // --- handshake: read C0+C1, send S0+S1+S2, read C2 ---
    let mut c0c1 = vec![0u8; 1 + HANDSHAKE_LEN];
    sock.read_exact(&mut c0c1).unwrap();
    assert_eq!(c0c1[0], 3, "client RTMP version");
    let mut reply = vec![3u8];
    reply.extend(std::iter::repeat(0xAB).take(HANDSHAKE_LEN)); // S1
    reply.extend_from_slice(&c0c1[1..]); // S2 = echo C1
    sock.write_all(&reply).unwrap();
    let mut c2 = vec![0u8; HANDSHAKE_LEN];
    sock.read_exact(&mut c2).unwrap();
    assert_eq!(&c2[..], &reply[1..1 + HANDSHAKE_LEN], "C2 must echo S1");

    // --- message loop ---
    let mut obs = Observed {
        connect_seen: false,
        publish_seen: false,
        media: Vec::new(),
    };
    let mut chunk_size = 128usize;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    'outer: loop {
        let n = match sock.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&tmp[..n]);
        // Parse chunks (fmt0 headers + fmt3 continuations; csid 2..63).
        loop {
            match parse_message(&mut buf, &mut chunk_size) {
                Some((type_id, payload)) => match type_id {
                    1 => {} // handled inside parse_message
                    20 => {
                        let name = amf_first_string(&payload);
                        match name.as_deref() {
                            Some("connect") => {
                                obs.connect_seen = true;
                                send_result(&mut sock, 1.0, None);
                            }
                            Some("createStream") => {
                                send_result(&mut sock, 2.0, Some(7.0)); // stream id 7
                            }
                            Some("publish") => obs.publish_seen = true,
                            _ => {}
                        }
                    }
                    8 | 9 | 18 => obs.media.push((type_id, payload)),
                    _ => {}
                },
                None => continue 'outer,
            }
        }
    }
    let _ = tx.send(obs);
}

/// Server-side chunk assembly. Handles the shapes our client emits: one fmt0
/// header per message + fmt3 continuations, single outstanding message.
fn parse_message(buf: &mut Vec<u8>, chunk_size: &mut usize) -> Option<(u8, Vec<u8>)> {
    if buf.is_empty() {
        return None;
    }
    let fmt = buf[0] >> 6;
    assert_eq!(fmt, 0, "client sends fmt0 message heads");
    if buf.len() < 12 {
        return None;
    }
    let length = u32::from_be_bytes([0, buf[4], buf[5], buf[6]]) as usize;
    let type_id = buf[7];
    // Collect `length` payload bytes across fmt3 continuation headers.
    let mut needed = length;
    let mut i = 12;
    let mut payload = Vec::with_capacity(length);
    while needed > 0 {
        let take = needed.min(*chunk_size);
        if buf.len() < i + take {
            return None;
        }
        payload.extend_from_slice(&buf[i..i + take]);
        i += take;
        needed -= take;
        if needed > 0 {
            if buf.len() <= i {
                return None;
            }
            assert_eq!(buf[i] >> 6, 3, "continuation must be fmt3");
            i += 1;
        }
    }
    buf.drain(..i);
    if type_id == 1 && payload.len() >= 4 {
        *chunk_size = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
        return Some((1, payload));
    }
    Some((type_id, payload))
}

/// First AMF0 string in a command payload (the command name).
fn amf_first_string(p: &[u8]) -> Option<String> {
    if p.first() != Some(&0x02) || p.len() < 3 {
        return None;
    }
    let len = u16::from_be_bytes([p[1], p[2]]) as usize;
    p.get(3..3 + len)
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

/// Send `_result(txn, null, [stream_id])` as one fmt0 chunk on csid 3.
fn send_result(sock: &mut TcpStream, txn: f64, stream_id: Option<f64>) {
    let mut payload = Vec::new();
    payload.push(0x02);
    payload.extend_from_slice(&7u16.to_be_bytes());
    payload.extend_from_slice(b"_result");
    payload.push(0x00);
    payload.extend_from_slice(&txn.to_be_bytes());
    payload.push(0x05); // null
    if let Some(id) = stream_id {
        payload.push(0x00);
        payload.extend_from_slice(&id.to_be_bytes());
    }
    assert!(payload.len() <= 128, "mock keeps replies in one chunk");
    let mut wire = vec![0x03u8, 0, 0, 0];
    wire.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    wire.push(20);
    wire.extend_from_slice(&0u32.to_le_bytes());
    wire.extend_from_slice(&payload);
    sock.write_all(&wire).unwrap();
}

/// A tiny FLV stream: header + onMetaData-ish script tag + a video tag.
fn flv_bytes() -> Vec<u8> {
    let mut v = b"FLV\x01\x05\x00\x00\x00\x09".to_vec();
    v.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
    for (tag_type, ts, data) in [
        (18u8, 0u32, vec![0x02, 0, 3, b'm' as u8, b'd' as u8, b'x' as u8]),
        (9, 40, vec![0x17, 0x00, 1, 2, 3, 4, 5]),
        (8, 60, vec![0xAF, 0x01, 9, 9]),
    ] {
        v.push(tag_type);
        v.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
        v.extend_from_slice(&ts.to_be_bytes()[1..]);
        v.push(0); // timestamp extended
        v.extend_from_slice(&[0, 0, 0]); // stream id
        v.extend_from_slice(&data);
        v.extend_from_slice(&((11 + data.len()) as u32).to_be_bytes());
    }
    v
}

#[test]
fn publishes_flv_tags_as_rtmp_messages() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || mock_server(listener, tx));

    {
        let mut publisher =
            RtmpPublisher::connect(&format!("rtmp://127.0.0.1:{port}/live/streamkey"))
                .expect("rtmp connect/publish");
        publisher.write_all(&flv_bytes()).unwrap();
        publisher.flush().unwrap();
    } // drop closes the socket; the mock's read loop ends

    let obs = rx.recv_timeout(Duration::from_secs(10)).expect("mock died");
    server.join().unwrap();

    assert!(obs.connect_seen, "no connect command");
    assert!(obs.publish_seen, "no publish command");
    let types: Vec<u8> = obs.media.iter().map(|(t, _)| *t).collect();
    assert_eq!(types, vec![18, 9, 8], "script+video+audio in order");
    assert_eq!(obs.media[1].1, vec![0x17, 0x00, 1, 2, 3, 4, 5]);
}
