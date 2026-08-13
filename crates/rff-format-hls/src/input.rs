//! HLS input: expand an `.m3u8` playlist into one byte stream over its
//! MPEG-TS segments.
//!
//! Works for local paths and `http(s)://` URLs (segments open through
//! [`rff_io::open`], so relative URIs inherit the playlist's transport).
//! Master playlists pick their first variant. Segments open lazily, one at a
//! time, so a long VOD doesn't buffer everything up front.

use std::io::Read;

use rff_core::{Error, Result};

/// How many playlist indirections (master → variant) to follow.
const MAX_PLAYLIST_DEPTH: usize = 3;

/// Open an HLS playlist (path or URL) as a single chained segment stream.
pub fn open_input(source: &str) -> Result<Box<dyn Read + Send>> {
    let segments = resolve_playlist(source, MAX_PLAYLIST_DEPTH)?;
    if segments.is_empty() {
        return Err(Error::invalid(format!("hls: no segments in {source}")));
    }
    Ok(Box::new(SegmentChain {
        segments,
        next: 0,
        current: None,
    }))
}

/// Fetch + parse a playlist; recurse once into a master playlist's first
/// variant. Returns the ordered list of resolved segment sources.
fn resolve_playlist(source: &str, depth: usize) -> Result<Vec<String>> {
    if depth == 0 {
        return Err(Error::invalid("hls: playlist nesting too deep"));
    }
    let mut text = String::new();
    rff_io::open(source)?
        .take(8 * 1024 * 1024) // a playlist is text; cap runaway inputs
        .read_to_string(&mut text)
        .map_err(|_| Error::invalid(format!("hls: {source} is not a UTF-8 playlist")))?;
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(Error::invalid(format!("hls: {source} is not an m3u8")));
    }
    if text.contains("#EXT-X-MAP") {
        return Err(Error::unsupported(
            "hls input: fMP4 (EXT-X-MAP) playlists — TS segments only for now",
        ));
    }

    let mut is_master = false;
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("#EXT-X-STREAM-INF") {
            is_master = true;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        entries.push(resolve_uri(source, line));
    }

    if is_master {
        // Master playlist: the URI lines are variant playlists; take the first.
        let variant = entries
            .first()
            .ok_or_else(|| Error::invalid("hls: master playlist has no variants"))?;
        return resolve_playlist(variant, depth - 1);
    }
    Ok(entries)
}

/// Resolve a segment/variant URI against the playlist it came from: absolute
/// URLs pass through, everything else is relative to the playlist's directory.
fn resolve_uri(playlist: &str, uri: &str) -> String {
    if rff_io::is_url(uri) || uri.contains(":\\") || uri.starts_with('/') {
        return uri.to_string();
    }
    match playlist.rfind(['/', '\\']) {
        Some(i) => format!("{}{}", &playlist[..=i], uri),
        None => uri.to_string(),
    }
}

/// Reads segment after segment as one continuous stream.
struct SegmentChain {
    segments: Vec<String>,
    next: usize,
    current: Option<Box<dyn Read + Send>>,
}

impl Read for SegmentChain {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some(cur) = &mut self.current {
                let n = cur.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                self.current = None; // segment exhausted, move on
            }
            if self.next >= self.segments.len() {
                return Ok(0);
            }
            let source = &self.segments[self.next];
            self.next += 1;
            self.current = Some(rff_io::open(source).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("hls segment {source}: {e}"),
                )
            })?);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn resolves_relative_and_absolute_uris() {
        assert_eq!(
            resolve_uri("http://h/x/master.m3u8", "seg0.ts"),
            "http://h/x/seg0.ts"
        );
        assert_eq!(
            resolve_uri("dir/list.m3u8", "http://cdn/seg.ts"),
            "http://cdn/seg.ts"
        );
        assert_eq!(resolve_uri("list.m3u8", "seg.ts"), "seg.ts");
    }

    #[test]
    fn chains_local_segments_in_order() {
        let dir = std::env::temp_dir().join(format!("rff_hls_in_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.ts"), b"AAAA").unwrap();
        std::fs::write(dir.join("b.ts"), b"BB").unwrap();
        let playlist = dir.join("v.m3u8");
        let mut f = std::fs::File::create(&playlist).unwrap();
        writeln!(f, "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.0,\na.ts\n#EXTINF:2.0,\nb.ts\n#EXT-X-ENDLIST").unwrap();
        drop(f);

        let mut out = Vec::new();
        open_input(playlist.to_str().unwrap())
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"AAAABB");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn master_playlists_pick_the_first_variant() {
        let dir = std::env::temp_dir().join(format!("rff_hls_master_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.ts"), b"SEG").unwrap();
        std::fs::write(
            dir.join("var.m3u8"),
            "#EXTM3U\n#EXTINF:4.0,\ns.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("master.m3u8"),
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\nvar.m3u8\n",
        )
        .unwrap();

        let mut out = Vec::new();
        open_input(dir.join("master.m3u8").to_str().unwrap())
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"SEG");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
