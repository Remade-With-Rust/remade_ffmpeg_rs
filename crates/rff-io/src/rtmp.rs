//! RTMP publishing (`rtmp://host[:port]/app/stream_key`) — the "stream to
//! Twitch / YouTube / nginx-rtmp" path.
//!
//! [`RtmpPublisher`] is a byte sink fed by the **FLV muxer**: RTMP's media
//! messages are exactly FLV tag payloads (audio=8, video=9, script=18), so the
//! publisher parses the incoming FLV stream (header + tags) and forwards each
//! tag as one RTMP message. `rff -i in.mp4 -c copy -f flv rtmp://...` therefore
//! reuses the whole FLV path unchanged.
//!
//! Protocol subset: plain (non-crypto) handshake, chunking with a 4096-byte
//! chunk size, AMF0 `connect` → `createStream` → `publish`, media on message
//! stream from the server's `_result` (falling back to 1). Server-to-client
//! traffic (acks, onStatus) is drained and discarded on a cloned socket so the
//! TCP window never stalls. Publish (output) only — playback would need the
//! full message-dispatch layer.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use rff_core::{Error, Result};

const RTMP_VERSION: u8 = 3;
const HANDSHAKE_LEN: usize = 1536;
const OUT_CHUNK_SIZE: usize = 4096;
/// Server messages are read with this timeout during the command exchange.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Parsed `rtmp://` target.
struct RtmpUrl {
    host: String,
    port: u16,
    app: String,
    stream: String,
    tc_url: String,
}

fn parse_rtmp_url(url: &str) -> Result<RtmpUrl> {
    let rest = url
        .strip_prefix("rtmp://")
        .ok_or_else(|| Error::invalid(format!("not an rtmp:// URL: {url}")))?;
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| Error::invalid("rtmp:// needs /app/stream_key"))?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| Error::invalid(format!("bad rtmp port: {p}")))?,
        ),
        None => (authority.to_string(), 1935),
    };
    let (app, stream) = path
        .split_once('/')
        .ok_or_else(|| Error::invalid("rtmp:// needs /app/stream_key"))?;
    if app.is_empty() || stream.is_empty() {
        return Err(Error::invalid("rtmp:// needs /app/stream_key"));
    }
    Ok(RtmpUrl {
        tc_url: format!("rtmp://{host}:{port}/{app}"),
        host,
        port,
        app: app.to_string(),
        stream: stream.to_string(),
    })
}

// ---------------------------------------------------------------------------
// AMF0 encoding (the handful of shapes the publish handshake needs)
// ---------------------------------------------------------------------------

fn amf_string(out: &mut Vec<u8>, s: &str) {
    out.push(0x02);
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn amf_number(out: &mut Vec<u8>, v: f64) {
    out.push(0x00);
    out.extend_from_slice(&v.to_be_bytes());
}

fn amf_null(out: &mut Vec<u8>) {
    out.push(0x05);
}

/// `{ key: "value", ... }` with string values only (all `connect` needs).
fn amf_object(out: &mut Vec<u8>, props: &[(&str, &str)]) {
    out.push(0x03);
    for (k, v) in props {
        out.extend_from_slice(&(k.len() as u16).to_be_bytes());
        out.extend_from_slice(k.as_bytes());
        amf_string(out, v);
    }
    out.extend_from_slice(&[0x00, 0x00, 0x09]); // object end
}

/// Scan an AMF0 command payload for the first number after the command name —
/// for `_result` of `createStream` that is the transaction id, the SECOND
/// number is the stream id. Best-effort; malformed data yields `None`.
fn amf_numbers(payload: &[u8]) -> Vec<f64> {
    let mut nums = Vec::new();
    let mut i = 0;
    while i < payload.len() {
        match payload[i] {
            0x00 if i + 9 <= payload.len() => {
                nums.push(f64::from_be_bytes(
                    payload[i + 1..i + 9].try_into().unwrap(),
                ));
                i += 9;
            }
            0x02 if i + 3 <= payload.len() => {
                let len = u16::from_be_bytes([payload[i + 1], payload[i + 2]]) as usize;
                i += 3 + len;
            }
            0x05 => i += 1,
            0x01 => i += 2,
            0x03 => {
                // Object: skip properties conservatively until end marker.
                i += 1;
                while i + 3 <= payload.len() {
                    if payload[i..i + 3] == [0x00, 0x00, 0x09] {
                        i += 3;
                        break;
                    }
                    i += 1;
                }
            }
            _ => break, // unknown marker — stop scanning
        }
    }
    nums
}

// ---------------------------------------------------------------------------
// The publisher
// ---------------------------------------------------------------------------

pub struct RtmpPublisher {
    socket: TcpStream,
    stream_id: u32,
    /// Buffered incoming FLV bytes not yet parsed into a complete tag.
    pending: Vec<u8>,
    /// FLV header consumed?
    saw_flv_header: bool,
}

impl RtmpPublisher {
    /// Connect, handshake, and run `connect`/`createStream`/`publish`.
    pub fn connect(url: &str) -> Result<RtmpPublisher> {
        let u = parse_rtmp_url(url)?;
        let mut socket = TcpStream::connect((u.host.as_str(), u.port))
            .map_err(|e| Error::invalid(format!("rtmp connect {}:{}: {e}", u.host, u.port)))?;
        socket.set_nodelay(true).ok();
        socket.set_read_timeout(Some(RESPONSE_TIMEOUT))?;

        handshake(&mut socket)?;

        let mut pub_ = RtmpPublisher {
            socket,
            stream_id: 1,
            pending: Vec::new(),
            saw_flv_header: false,
        };

        // Set Chunk Size (type 1) so media messages fit few chunks.
        pub_.send_message(2, 1, 0, 0, &(OUT_CHUNK_SIZE as u32).to_be_bytes())?;

        // connect("app") — transaction 1.
        let mut cmd = Vec::new();
        amf_string(&mut cmd, "connect");
        amf_number(&mut cmd, 1.0);
        amf_object(
            &mut cmd,
            &[
                ("app", u.app.as_str()),
                ("type", "nonprivate"),
                ("flashVer", "FMLE/3.0 (compatible; rff)"),
                ("tcUrl", u.tc_url.as_str()),
            ],
        );
        pub_.send_message(3, 20, 0, 0, &cmd)?;
        pub_.await_result("connect")?;

        // createStream() — transaction 2; the reply names the message stream.
        let mut cmd = Vec::new();
        amf_string(&mut cmd, "createStream");
        amf_number(&mut cmd, 2.0);
        amf_null(&mut cmd);
        pub_.send_message(3, 20, 0, 0, &cmd)?;
        if let Some(id) = pub_.await_result("createStream")? {
            if id >= 1.0 && id < u32::MAX as f64 {
                pub_.stream_id = id as u32;
            }
        }

        // publish(stream_key, "live") on the new stream.
        let mut cmd = Vec::new();
        amf_string(&mut cmd, "publish");
        amf_number(&mut cmd, 3.0);
        amf_null(&mut cmd);
        amf_string(&mut cmd, &u.stream);
        amf_string(&mut cmd, "live");
        let sid = pub_.stream_id;
        pub_.send_message(3, 20, 0, sid, &cmd)?;

        // From here the socket only needs draining; short timeout suffices.
        pub_.socket
            .set_read_timeout(Some(Duration::from_millis(1)))
            .ok();
        Ok(pub_)
    }

    /// Send one RTMP message, split into `OUT_CHUNK_SIZE` chunks (fmt0 header
    /// first, fmt3 continuations).
    fn send_message(
        &mut self,
        csid: u8,
        type_id: u8,
        timestamp: u32,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<()> {
        let mut out = Vec::with_capacity(payload.len() + 18);
        let ts = timestamp.min(0x00FF_FFFF); // extended timestamps: clamp (fine for VOD pushes)
        for (i, chunk) in payload.chunks(OUT_CHUNK_SIZE).enumerate() {
            if i == 0 {
                out.push(csid & 0x3F); // fmt0
                out.extend_from_slice(&ts.to_be_bytes()[1..]);
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
                out.push(type_id);
                out.extend_from_slice(&stream_id.to_le_bytes());
            } else {
                out.push(0xC0 | (csid & 0x3F)); // fmt3 continuation
            }
            out.extend_from_slice(chunk);
        }
        self.socket.write_all(&out)?;
        Ok(())
    }

    /// Read chunks until an AMF0 `_result`/`_error` command arrives; returns
    /// the last number in the reply (the created stream id, when present).
    fn await_result(&mut self, what: &str) -> Result<Option<f64>> {
        let mut reader = ChunkReader::new();
        let deadline = std::time::Instant::now() + RESPONSE_TIMEOUT;
        let mut buf = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            let n = match self.socket.read(&mut buf) {
                Ok(0) => {
                    return Err(Error::invalid(format!(
                        "rtmp: server closed during {what}"
                    )))
                }
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(e.into()),
            };
            for msg in reader.push(&buf[..n]) {
                if msg.type_id == 20 {
                    if msg.payload.starts_with(&[0x02, 0x00, 0x07]) // "_result"
                        && msg.payload[3..].starts_with(b"_result")
                    {
                        let nums = amf_numbers(&msg.payload);
                        return Ok(nums.last().copied());
                    }
                    if msg.payload[3..].starts_with(b"_error") {
                        return Err(Error::invalid(format!("rtmp: {what} rejected")));
                    }
                }
            }
        }
        Err(Error::invalid(format!("rtmp: no reply to {what}")))
    }

    /// Drain any pending server chatter (acks/onStatus) without blocking.
    fn drain_server(&mut self) {
        let mut buf = [0u8; 4096];
        loop {
            match self.socket.read(&mut buf) {
                Ok(n) if n > 0 => continue,
                _ => break,
            }
        }
    }

    /// Consume complete FLV tags from `pending`, forwarding each as a message.
    fn pump(&mut self) -> std::io::Result<()> {
        // FLV header: "FLV" v1 flags(1) offset(4) + first PreviousTagSize(4).
        if !self.saw_flv_header {
            if self.pending.len() < 13 {
                return Ok(());
            }
            if &self.pending[..3] != b"FLV" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "rtmp sink expects an FLV stream (use -f flv)",
                ));
            }
            self.pending.drain(..13);
            self.saw_flv_header = true;
        }
        loop {
            // Tag: type(1) size(3) ts(3)+ext(1) streamid(3) data + prev(4).
            if self.pending.len() < 11 {
                return Ok(());
            }
            let size = u32::from_be_bytes([
                0,
                self.pending[1],
                self.pending[2],
                self.pending[3],
            ]) as usize;
            let total = 11 + size + 4;
            if self.pending.len() < total {
                return Ok(());
            }
            let tag_type = self.pending[0] & 0x1F;
            let timestamp = u32::from_be_bytes([
                self.pending[7], // extended byte is the high byte
                self.pending[4],
                self.pending[5],
                self.pending[6],
            ]);
            let payload: Vec<u8> = self.pending[11..11 + size].to_vec();
            self.pending.drain(..total);
            let csid = match tag_type {
                8 => 4,  // audio
                9 => 6,  // video
                _ => 5,  // script data (onMetaData)
            };
            let sid = self.stream_id;
            self.send_message(csid, tag_type, timestamp, sid, &payload)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            self.drain_server();
        }
    }
}

impl Write for RtmpPublisher {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        self.pump()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.pump()?;
        self.socket.flush()
    }
}

/// Plain RTMP handshake: C0+C1, read S0+S1+S2, send C2 (= S1 echo).
fn handshake(socket: &mut TcpStream) -> Result<()> {
    let mut c0c1 = vec![RTMP_VERSION];
    c0c1.extend_from_slice(&[0u8; 8]); // time + zero
    // "Random" fill — content is irrelevant for the plain handshake.
    c0c1.extend((0..HANDSHAKE_LEN - 8).map(|i| (i * 7 + 11) as u8));
    socket.write_all(&c0c1)?;

    let mut s0s1s2 = vec![0u8; 1 + HANDSHAKE_LEN * 2];
    socket.read_exact(&mut s0s1s2)?;
    if s0s1s2[0] != RTMP_VERSION {
        return Err(Error::invalid(format!(
            "rtmp: server speaks version {}, not 3",
            s0s1s2[0]
        )));
    }
    // C2 = echo of S1.
    socket.write_all(&s0s1s2[1..1 + HANDSHAKE_LEN])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal chunk-stream reader (for command replies)
// ---------------------------------------------------------------------------

pub(crate) struct RtmpMessage {
    pub type_id: u8,
    pub payload: Vec<u8>,
}

/// Per-chunk-stream assembly state.
#[derive(Default, Clone)]
struct CsState {
    length: usize,
    type_id: u8,
    collected: Vec<u8>,
}

pub(crate) struct ChunkReader {
    buf: Vec<u8>,
    chunk_size: usize,
    streams: std::collections::HashMap<u8, CsState>,
}

impl ChunkReader {
    pub(crate) fn new() -> ChunkReader {
        ChunkReader {
            buf: Vec::new(),
            chunk_size: 128,
            streams: std::collections::HashMap::new(),
        }
    }

    /// Feed bytes; return any complete messages.
    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<RtmpMessage> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let Some((consumed, msg)) = self.try_parse_chunk() else {
                break;
            };
            self.buf.drain(..consumed);
            if let Some(m) = msg {
                if m.type_id == 1 && m.payload.len() >= 4 {
                    self.chunk_size = u32::from_be_bytes(m.payload[..4].try_into().unwrap())
                        .clamp(1, 1 << 24) as usize;
                }
                out.push(m);
            }
        }
        out
    }

    fn try_parse_chunk(&mut self) -> Option<(usize, Option<RtmpMessage>)> {
        let b = &self.buf;
        if b.is_empty() {
            return None;
        }
        let fmt = b[0] >> 6;
        let csid = b[0] & 0x3F;
        if csid == 0 || csid == 1 {
            return None; // 2/3-byte csids: not produced by the servers we target
        }
        let mut i = 1;
        let mut st = self.streams.get(&csid).cloned().unwrap_or_default();
        match fmt {
            0 => {
                if b.len() < i + 11 {
                    return None;
                }
                st.length =
                    u32::from_be_bytes([0, b[i + 3], b[i + 4], b[i + 5]]) as usize;
                st.type_id = b[i + 6];
                i += 11;
            }
            1 => {
                if b.len() < i + 7 {
                    return None;
                }
                st.length = u32::from_be_bytes([0, b[i + 3], b[i + 4], b[i + 5]]) as usize;
                st.type_id = b[i + 6];
                i += 7;
            }
            2 => {
                if b.len() < i + 3 {
                    return None;
                }
                i += 3;
            }
            _ => {} // fmt3: headerless continuation
        }
        let remaining = st.length.saturating_sub(st.collected.len());
        let take = remaining.min(self.chunk_size);
        if b.len() < i + take {
            return None;
        }
        st.collected.extend_from_slice(&b[i..i + take]);
        i += take;
        let msg = if st.collected.len() >= st.length && st.length > 0 {
            let payload = std::mem::take(&mut st.collected);
            Some(RtmpMessage {
                type_id: st.type_id,
                payload,
            })
        } else {
            None
        };
        self.streams.insert(csid, st);
        Some((i, msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing_splits_app_and_key() {
        let u = parse_rtmp_url("rtmp://live.example:1936/app/stream/key").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("live.example", 1936));
        assert_eq!(u.app, "app");
        assert_eq!(u.stream, "stream/key");
        assert_eq!(u.tc_url, "rtmp://live.example:1936/app");
        assert!(parse_rtmp_url("rtmp://host/apponly").is_err());
    }

    #[test]
    fn amf_number_scan_finds_stream_id() {
        let mut payload = Vec::new();
        amf_string(&mut payload, "_result");
        amf_number(&mut payload, 2.0);
        amf_null(&mut payload);
        amf_number(&mut payload, 5.0);
        assert_eq!(amf_numbers(&payload), vec![2.0, 5.0]);
    }

    #[test]
    fn chunk_reader_reassembles_across_chunks() {
        // A 200-byte type-20 message at the default 128-byte chunk size.
        let payload: Vec<u8> = (0..200u8).collect();
        let mut wire = Vec::new();
        wire.push(0x03); // fmt0, csid 3
        wire.extend_from_slice(&[0, 0, 0]); // timestamp
        wire.extend_from_slice(&(200u32).to_be_bytes()[1..]);
        wire.push(20);
        wire.extend_from_slice(&1u32.to_le_bytes());
        wire.extend_from_slice(&payload[..128]);
        wire.push(0xC3); // fmt3 continuation
        wire.extend_from_slice(&payload[128..]);

        let mut r = ChunkReader::new();
        let msgs = r.push(&wire);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].type_id, 20);
        assert_eq!(msgs[0].payload, payload);
    }
}
