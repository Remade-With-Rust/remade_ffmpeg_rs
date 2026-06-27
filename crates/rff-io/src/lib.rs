//! `rff-io` — byte sources for demuxers: local files and HTTP streaming input.
//!
//! FFmpeg reads inputs through `libavformat`'s URL protocols (`file:`, `http:`,
//! ...). This crate is the equivalent seam: [`open`] turns a path-or-URL into a
//! boxed `Read`, picking the local file or an HTTP stream.
//!
//! The HTTP client is a minimal, dependency-free HTTP/1.1 `GET` over
//! [`std::net::TcpStream`] — no async runtime, no TLS crate. That keeps the
//! "100% Rust, permissively licensed" promise intact (a TLS stack would pull in
//! `ring`/OpenSSL-licensed code). The trade-off: **`https://` is not supported
//! yet** — it needs a vetted permissive TLS backend, a deliberate follow-up.
//! Plain `http://` (incl. redirects and chunked transfer-encoding) works today.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use rff_core::{Error, Result};

/// Maximum number of HTTP redirects to follow before giving up.
const MAX_REDIRECTS: usize = 5;

/// Is `path` a URL we should fetch over the network rather than open on disk?
pub fn is_url(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

/// Open a path-or-URL as a streaming byte source.
///
/// * `http://…` → an HTTP/1.1 `GET` stream (see the module docs).
/// * `https://…` → [`Error::Unsupported`] (no TLS backend yet).
/// * anything else → a local file.
pub fn open(path: &str) -> Result<Box<dyn Read + Send>> {
    if path.starts_with("https://") {
        return Err(Error::unsupported(
            "https:// input needs a TLS backend (not built in yet); use http:// or a local file",
        ));
    }
    if let Some(rest) = path.strip_prefix("http://") {
        return http_get(rest, MAX_REDIRECTS);
    }
    Ok(Box::new(std::fs::File::open(path)?))
}

/// A parsed `http://` authority + path.
struct Url<'a> {
    host: &'a str,
    port: u16,
    path: String,
}

fn parse_http(rest: &str) -> Result<Url<'_>> {
    // `rest` is everything after `http://`: `host[:port][/path][?query]`.
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse()
                .map_err(|_| Error::invalid(format!("bad port in URL: {p}")))?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(Error::invalid("URL has no host"));
    }
    Ok(Url { host, port, path })
}

/// Perform an HTTP `GET`, following up to `redirects` further `http://` hops,
/// and return a reader positioned at the start of the response body.
fn http_get(rest: &str, redirects: usize) -> Result<Box<dyn Read + Send>> {
    let url = parse_http(rest)?;
    let stream = TcpStream::connect((url.host, url.port))?;
    let mut reader = BufReader::new(stream);

    // --- request ---
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rff-io/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );
    reader.get_mut().write_all(req.as_bytes())?;

    // --- status line ---
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| Error::invalid(format!("malformed HTTP status line: {line:?}")))?;

    // --- headers ---
    let mut location = None;
    let mut content_length = None;
    let mut chunked = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
            match name.as_str() {
                "location" => location = Some(value.to_string()),
                "content-length" => content_length = value.parse::<u64>().ok(),
                "transfer-encoding" if value.eq_ignore_ascii_case("chunked") => chunked = true,
                _ => {}
            }
        }
    }

    // --- redirects ---
    if (300..400).contains(&status) {
        let loc = location
            .ok_or_else(|| Error::invalid(format!("HTTP {status} redirect without Location")))?;
        if redirects == 0 {
            return Err(Error::invalid("too many HTTP redirects"));
        }
        if let Some(next) = loc.strip_prefix("http://") {
            return http_get(next, redirects - 1);
        }
        if loc.starts_with("https://") {
            return Err(Error::unsupported(format!(
                "redirect to {loc}: https:// needs a TLS backend (not built in yet)"
            )));
        }
        // Relative redirect (`/other`): keep the same host/port.
        let next = format!("{}:{}{}", url.host, url.port, loc);
        return http_get(&next, redirects - 1);
    }
    if !(200..300).contains(&status) {
        return Err(Error::invalid(format!("HTTP request failed: status {status}")));
    }

    // --- body ---
    if chunked {
        Ok(Box::new(ChunkedReader::new(reader)))
    } else if let Some(len) = content_length {
        Ok(Box::new(reader.take(len)))
    } else {
        // No length and not chunked: the body runs until the server closes
        // (we asked for `Connection: close`), so read to EOF.
        Ok(Box::new(reader))
    }
}

/// Decodes HTTP/1.1 chunked transfer-encoding on the fly. Each chunk is a hex
/// length line, the bytes, then CRLF; a zero-length chunk ends the body.
struct ChunkedReader<R: BufRead> {
    inner: R,
    /// Bytes left in the current chunk before the next size line.
    remaining: u64,
    done: bool,
}

impl<R: BufRead> ChunkedReader<R> {
    fn new(inner: R) -> ChunkedReader<R> {
        ChunkedReader { inner, remaining: 0, done: false }
    }

    /// Read the next `len\r\n` size line, setting `remaining`.
    fn next_size(&mut self) -> std::io::Result<()> {
        let mut line = String::new();
        self.inner.read_line(&mut line)?;
        // Ignore any chunk extensions after `;`, parse the hex length.
        let hex = line.trim().split(';').next().unwrap_or("").trim();
        self.remaining = u64::from_str_radix(hex, 16)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))?;
        if self.remaining == 0 {
            self.done = true;
        }
        Ok(())
    }
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.next_size()?;
            if self.done {
                return Ok(0);
            }
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n as u64;
        if self.remaining == 0 {
            // Consume the trailing CRLF that follows the chunk data.
            let mut crlf = [0u8; 2];
            let _ = self.inner.read_exact(&mut crlf);
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn is_url_distinguishes_schemes() {
        assert!(is_url("http://example.com/a.mp4"));
        assert!(is_url("https://example.com/a.mp4"));
        assert!(!is_url("C:/videos/a.mp4"));
        assert!(!is_url("./a.mp4"));
    }

    #[test]
    fn https_is_rejected_with_a_clear_message() {
        match open("https://example.com/a.mp4") {
            Err(e) => assert!(e.to_string().contains("TLS"), "got: {e}"),
            Ok(_) => panic!("https:// should be rejected without a TLS backend"),
        }
    }

    #[test]
    fn parse_http_splits_host_port_path() {
        let u = parse_http("host.example:8080/dir/file.ts?x=1").unwrap();
        assert_eq!((u.host, u.port), ("host.example", 8080));
        assert_eq!(u.path, "/dir/file.ts?x=1");
        // Default port and root path.
        let u = parse_http("host.example").unwrap();
        assert_eq!((u.host, u.port, u.path.as_str()), ("host.example", 80, "/"));
    }

    #[test]
    fn chunked_reader_reassembles_the_body() {
        // "Wikipedia" sent as 4 + 5 byte chunks, then a 0 terminator.
        let body = "4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut out = String::new();
        ChunkedReader::new(Cursor::new(body.as_bytes()))
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "Wikipedia");
    }
}
