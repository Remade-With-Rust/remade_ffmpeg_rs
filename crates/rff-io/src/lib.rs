//! `rff-io` — byte sources for demuxers: local files and HTTP(S) streaming input.
//!
//! FFmpeg reads inputs through `libavformat`'s URL protocols (`file:`, `http:`,
//! `https:`, ...). This crate is the equivalent seam: [`open`] turns a
//! path-or-URL into a boxed `Read`, picking the local file or a network stream.
//!
//! The HTTP client is a minimal HTTP/1.1 `GET` over [`std::net::TcpStream`] — no
//! async runtime. Plain `http://` (redirects + chunked transfer-encoding) is
//! always available and dependency-free.
//!
//! `https://` is gated behind the **`https`** feature. When enabled it layers a
//! pure-Rust, permissively-licensed TLS stack over the same HTTP exchange:
//! [`rustls`] with default providers disabled, a [`rustls_rustcrypto`]
//! `CryptoProvider` (the RustCrypto crates — pure Rust, still pre-1.0 and not
//! yet security-audited), and the OS trust store via [`rustls_native_certs`].
//! With the feature off, `https://` returns a clear "needs the `https` feature"
//! error and the crate pulls in no TLS code at all.

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
/// * `http://…` → an HTTP/1.1 `GET` stream.
/// * `https://…` → the same over TLS (requires the `https` feature; otherwise
///   [`Error::Unsupported`]).
/// * anything else → a local file.
pub fn open(path: &str) -> Result<Box<dyn Read + Send>> {
    if is_url(path) {
        fetch(path, MAX_REDIRECTS)
    } else {
        Ok(Box::new(std::fs::File::open(path)?))
    }
}

/// A parsed HTTP(S) URL: scheme, authority, and request path.
struct Url<'a> {
    scheme: &'a str,
    host: &'a str,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url<'_>> {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("http://") {
        ("http", r)
    } else if let Some(r) = url.strip_prefix("https://") {
        ("https", r)
    } else {
        return Err(Error::invalid(format!("unsupported URL scheme: {url}")));
    };
    // `rest` is `host[:port][/path][?query]`.
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse()
                .map_err(|_| Error::invalid(format!("bad port in URL: {p}")))?,
        ),
        None => (authority, default_port),
    };
    if host.is_empty() {
        return Err(Error::invalid("URL has no host"));
    }
    Ok(Url { scheme, host, port, path })
}

/// Connect, run the HTTP exchange, and return a reader over the response body.
/// `http` goes over a raw TCP stream; `https` wraps it in TLS (feature-gated).
fn fetch(url: &str, redirects: usize) -> Result<Box<dyn Read + Send>> {
    let u = parse_url(url)?;
    if u.scheme == "https" && !cfg!(feature = "https") {
        return Err(Error::unsupported(
            "https:// needs the `https` feature (rustls); rebuild with `--features https`, or use http://",
        ));
    }
    let tcp = TcpStream::connect((u.host, u.port))?;
    match u.scheme {
        "http" => exchange(tcp, &u, redirects),
        #[cfg(feature = "https")]
        "https" => exchange(tls::connect(tcp, u.host)?, &u, redirects),
        other => Err(Error::unsupported(format!(
            "unsupported URL scheme `{other}://`"
        ))),
    }
}

/// Resolve a redirect `Location` against the URL it came from into an absolute
/// URL (handles absolute, root-relative, and bare-relative targets).
fn resolve_redirect(base: &Url, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        loc.to_string()
    } else if let Some(rel) = loc.strip_prefix('/') {
        format!("{}://{}:{}/{}", base.scheme, base.host, base.port, rel)
    } else {
        format!("{}://{}:{}/{}", base.scheme, base.host, base.port, loc)
    }
}

/// Perform the HTTP/1.1 `GET` conversation over any read/write stream (plain TCP
/// or a TLS session) and return a reader positioned at the body. Redirects are
/// followed by re-dispatching through [`fetch`] (which may switch scheme).
fn exchange<S: Read + Write + Send + 'static>(
    stream: S,
    u: &Url,
    redirects: usize,
) -> Result<Box<dyn Read + Send>> {
    let mut reader = BufReader::new(stream);

    // --- request ---
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rff-io/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        u.path, u.host
    );
    reader.get_mut().write_all(req.as_bytes())?;
    reader.get_mut().flush()?;

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
        return fetch(&resolve_redirect(u, &loc), redirects - 1);
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

/// TLS setup for `https://`, kept entirely behind the `https` feature so the
/// default build links no TLS code.
#[cfg(feature = "https")]
mod tls {
    use std::net::TcpStream;
    use std::sync::Arc;

    use rff_core::{Error, Result};
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

    /// Open a TLS client session to `host` over an established TCP stream, using
    /// a RustCrypto crypto provider and the OS trust store for roots.
    pub fn connect(
        tcp: TcpStream,
        host: &str,
    ) -> Result<StreamOwned<ClientConnection, TcpStream>> {
        let provider = Arc::new(rustls_rustcrypto::provider());

        let mut roots = RootCertStore::empty();
        let loaded = rustls_native_certs::load_native_certs();
        for cert in loaded.certs {
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            return Err(Error::invalid(
                "no system root certificates available for TLS verification",
            ));
        }

        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::invalid(format!("rustls configuration error: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();

        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|_| Error::invalid(format!("invalid TLS server name: {host}")))?;
        let conn = ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| Error::invalid(format!("rustls handshake init failed: {e}")))?;
        Ok(StreamOwned::new(conn, tcp))
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

    #[cfg(not(feature = "https"))]
    #[test]
    fn https_is_rejected_without_the_feature() {
        match open("https://example.com/a.mp4") {
            Err(e) => assert!(e.to_string().contains("https"), "got: {e}"),
            Ok(_) => panic!("https:// should be rejected without the `https` feature"),
        }
    }

    #[test]
    fn parse_url_splits_scheme_host_port_path() {
        let u = parse_url("http://host.example:8080/dir/file.ts?x=1").unwrap();
        assert_eq!((u.scheme, u.host, u.port), ("http", "host.example", 8080));
        assert_eq!(u.path, "/dir/file.ts?x=1");
        // https default port + root path.
        let u = parse_url("https://host.example").unwrap();
        assert_eq!((u.scheme, u.host, u.port, u.path.as_str()), ("https", "host.example", 443, "/"));
    }

    #[test]
    fn redirect_resolves_absolute_and_relative() {
        let base = parse_url("https://h.example:443/a/b").unwrap();
        assert_eq!(resolve_redirect(&base, "http://other/x"), "http://other/x");
        assert_eq!(resolve_redirect(&base, "/c/d"), "https://h.example:443/c/d");
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
