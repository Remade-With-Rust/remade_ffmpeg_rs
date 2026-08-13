//! UDP byte transport (`udp://`), the MPEG-TS broadcast workhorse.
//!
//! * [`UdpReader`] binds `udp://[@]host:port` and serves received datagram
//!   payloads as a byte stream. UDP has no end-of-stream, so an idle timeout
//!   (default 10 s, `?timeout=SECONDS` to override) turns silence into EOF —
//!   that is what lets a one-shot capture (`rff -i udp://@:1234 out.mp4`)
//!   terminate.
//! * [`UdpWriter`] sends to `udp://host:port`, packing bytes into 1316-byte
//!   datagrams (7×188, the MPEG-TS-over-UDP convention).

use std::io::{Read, Write};
use std::net::UdpSocket;
use std::time::Duration;

use rff_core::{Error, Result};

/// 7 MPEG-TS packets of 188 bytes — the conventional UDP payload size.
const TS_DATAGRAM: usize = 7 * 188;
/// Largest datagram we accept on receive.
const MAX_DATAGRAM: usize = 65_536;
/// Default idle timeout before a receive stream reports EOF.
const DEFAULT_IDLE: Duration = Duration::from_secs(10);

fn idle_timeout(path: &str) -> Duration {
    path.split_once("?timeout=")
        .and_then(|(_, v)| v.split('&').next())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(DEFAULT_IDLE)
}

/// Receives datagrams from a bound UDP socket as a byte stream.
pub struct UdpReader {
    socket: UdpSocket,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl UdpReader {
    /// Bind `udp://[@]host:port[?timeout=SECONDS]` for receiving.
    pub fn bind(path: &str) -> Result<UdpReader> {
        let addr = crate::udp_addr(path)?;
        let socket = crate::udp_bind(addr)?;
        socket.set_read_timeout(Some(idle_timeout(path)))?;
        Ok(UdpReader {
            socket,
            // Starts exhausted (pos == len) so the first read recvs a datagram.
            buf: Vec::new(),
            pos: 0,
            eof: false,
        })
    }
}

impl Read for UdpReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.eof {
            return Ok(0);
        }
        // `while`, not `if`: an empty datagram must refill again, not EOF.
        while self.pos >= self.buf.len() {
            // Refill from the next datagram; a timeout is EOF, not an error.
            self.buf.resize(MAX_DATAGRAM, 0);
            match self.socket.recv(&mut self.buf) {
                Ok(n) => {
                    self.buf.truncate(n);
                    self.pos = 0;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.eof = true;
                    return Ok(0);
                }
                Err(e) => return Err(e),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Sends a byte stream as UDP datagrams (1316-byte payloads).
pub struct UdpWriter {
    socket: UdpSocket,
    pending: Vec<u8>,
}

impl UdpWriter {
    /// Connect `udp://host:port` for sending.
    pub fn connect(path: &str) -> Result<UdpWriter> {
        let addr = crate::udp_addr(path)?;
        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| Error::invalid(format!("udp: local bind failed: {e}")))?;
        socket
            .connect(addr)
            .map_err(|e| Error::invalid(format!("udp connect {addr}: {e}")))?;
        Ok(UdpWriter {
            socket,
            pending: Vec::with_capacity(TS_DATAGRAM),
        })
    }
}

impl Write for UdpWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while self.pending.len() >= TS_DATAGRAM {
            self.socket.send(&self.pending[..TS_DATAGRAM])?;
            self.pending.drain(..TS_DATAGRAM);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            self.socket.send(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

impl Drop for UdpWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_addr_strips_scheme_listen_marker_and_query() {
        assert_eq!(crate::udp_addr("udp://@:1234").unwrap(), ":1234");
        assert_eq!(
            crate::udp_addr("udp://239.0.0.1:5000?timeout=2").unwrap(),
            "239.0.0.1:5000"
        );
        assert!(crate::udp_addr("udp://").is_err());
    }

    #[test]
    fn writer_packs_1316_byte_datagrams_and_reader_reassembles() {
        // Loopback: bind a receiver, send 3000 bytes, expect them all back.
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = recv.local_addr().unwrap().port();
        recv.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let mut w = UdpWriter::connect(&format!("udp://127.0.0.1:{port}")).unwrap();
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        w.write_all(&payload).unwrap();
        w.flush().unwrap();

        let mut got = Vec::new();
        let mut buf = [0u8; MAX_DATAGRAM];
        while got.len() < payload.len() {
            let n = recv.recv(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, payload);
    }

    #[test]
    fn reader_times_out_to_eof() {
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = recv.local_addr().unwrap().port();
        drop(recv);
        let mut r = UdpReader::bind(&format!("udp://127.0.0.1:{port}?timeout=0.2")).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(r.read(&mut buf).unwrap(), 0); // silence → EOF
    }
}
