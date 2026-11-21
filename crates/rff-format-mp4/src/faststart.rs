//! Faststart (`moov` before `mdat`) without holding the media in memory
//! (#114).
//!
//! * [`Spool`] is where the seekable muxer puts samples under
//!   `movflags=+faststart`: a temporary file, so memory stays at the sample
//!   tables, or memory where there is no file system (wasm32-unknown).
//! * [`faststart_in_place`] rewrites an MP4/MOV whose `moov` is at the end
//!   into the faststart layout in place, through any `Read + Write + Seek`
//!   (what FFmpeg's `qt-faststart` does): memory is `moov` plus a 1 MiB copy
//!   buffer, whatever the media size.

use std::io::{self, Read, Seek, SeekFrom, Write};

use rff_core::{Error, Result};

use super::{be32, be64, boxes_at, bx};

/// The copy buffer for moving media.
const COPY_CHUNK: usize = 1 << 20;

/// Spooled sample bytes: a temporary file removed on drop, or memory.
pub(crate) struct Spool {
    inner: Inner,
    len: u64,
}

enum Inner {
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    File(std::fs::File, std::path::PathBuf),
    Memory(Vec<u8>),
}

impl Spool {
    /// A temporary file in [`std::env::temp_dir`], or memory when one cannot
    /// be created.
    pub(crate) fn new() -> Spool {
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        if let Some(spool) = Spool::temp_file() {
            return spool;
        }
        Spool::memory()
    }

    pub(crate) fn memory() -> Spool {
        Spool {
            inner: Inner::Memory(Vec::new()),
            len: 0,
        }
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    fn temp_file() -> Option<Spool> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        for _ in 0..8 {
            let name = format!(
                "rff-faststart-{}-{}-{nanos}.mdat",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            if let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                return Some(Spool {
                    inner: Inner::File(file, path),
                    len: 0,
                });
            }
        }
        None
    }

    /// Whether the bytes go to a temporary file (not memory).
    #[cfg(test)]
    pub(crate) fn on_disk(&self) -> bool {
        !matches!(self.inner, Inner::Memory(_))
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match &mut self.inner {
            #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
            Inner::File(f, _) => f.write_all(bytes)?,
            Inner::Memory(v) => v.extend_from_slice(bytes),
        }
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Copy everything spooled to `out`, a chunk at a time.
    pub(crate) fn copy_to(&mut self, out: &mut dyn Write) -> io::Result<()> {
        match &mut self.inner {
            #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
            Inner::File(f, _) => {
                f.flush()?;
                f.seek(SeekFrom::Start(0))?;
                let mut buf = vec![0u8; COPY_CHUNK];
                let mut left = self.len;
                while left > 0 {
                    let n = (left.min(COPY_CHUNK as u64)) as usize;
                    f.read_exact(&mut buf[..n])?;
                    out.write_all(&buf[..n])?;
                    left -= n as u64;
                }
                Ok(())
            }
            Inner::Memory(v) => out.write_all(v),
        }
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        if let Inner::File(_, path) = &self.inner {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One top-level box: type, start offset, total size.
#[derive(Clone, Copy, Debug)]
struct Top {
    typ: [u8; 4],
    start: u64,
    size: u64,
}

fn top_boxes<F: Read + Seek + ?Sized>(f: &mut F) -> Result<Vec<Top>> {
    let len = f.seek(SeekFrom::End(0))?;
    let mut out = Vec::new();
    let mut pos = 0u64;
    while pos.saturating_add(8) <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let typ = [h[4], h[5], h[6], h[7]];
        let size = match be32(&h, 0) {
            1 => {
                f.read_exact(&mut h[8..16])?;
                be64(&h, 8)
            }
            0 => len - pos,
            n => u64::from(n),
        };
        if size < 8 || pos.checked_add(size).is_none_or(|end| end > len) {
            return Err(Error::invalid("mp4 faststart: malformed top-level box"));
        }
        out.push(Top {
            typ,
            start: pos,
            size,
        });
        pos += size;
    }
    Ok(out)
}

/// Rebuild a container box's children, shifting every chunk offset in
/// `stco`/`co64` by `shift(offset)`, and turning an `stco` into a `co64`
/// when a shifted offset no longer fits 32 bits.
fn patch_children(body: &[u8], shift: &dyn Fn(u64) -> u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 64);
    for (typ, at, payload) in boxes_at(body) {
        match &typ {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
                out.extend_from_slice(&bx(&typ, &patch_children(payload, shift)));
            }
            b"stco" | b"co64" if payload.len() >= 8 => {
                let wide_in = &typ == b"co64";
                let stride = if wide_in { 8 } else { 4 };
                let n = (be32(payload, 4) as usize).min((payload.len() - 8) / stride);
                let offsets: Vec<u64> = (0..n)
                    .map(|k| {
                        let o = 8 + k * stride;
                        shift(if wide_in {
                            be64(payload, o)
                        } else {
                            u64::from(be32(payload, o))
                        })
                    })
                    .collect();
                let wide = wide_in || offsets.iter().any(|&o| o > u64::from(u32::MAX));
                let mut b = payload[..4].to_vec(); // version + flags
                b.extend_from_slice(&(n as u32).to_be_bytes());
                for o in offsets {
                    if wide {
                        b.extend_from_slice(&o.to_be_bytes());
                    } else {
                        b.extend_from_slice(&(o as u32).to_be_bytes());
                    }
                }
                out.extend_from_slice(&bx(if wide { b"co64" } else { b"stco" }, &b));
            }
            // Anything else is copied byte for byte (its header included).
            _ => {
                let end = (payload.as_ptr() as usize - body.as_ptr() as usize) + payload.len();
                out.extend_from_slice(&body[at..end]);
            }
        }
    }
    out
}

/// Move `len` bytes at `from` forward to `to` (`to >= from`), copying from
/// the end so the regions may overlap.
fn move_forward<F: Read + Write + Seek + ?Sized>(
    f: &mut F,
    from: u64,
    to: u64,
    len: u64,
) -> io::Result<()> {
    if to == from || len == 0 {
        return Ok(());
    }
    let mut buf = vec![0u8; usize::try_from(len).map_or(COPY_CHUNK, |l| l.min(COPY_CHUNK))];
    let mut left = len;
    while left > 0 {
        let n = left.min(buf.len() as u64);
        left -= n;
        f.seek(SeekFrom::Start(from + left))?;
        f.read_exact(&mut buf[..n as usize])?;
        f.seek(SeekFrom::Start(to + left))?;
        f.write_all(&buf[..n as usize])?;
    }
    Ok(())
}

/// Rewrite an MP4/MOV with `moov` after its `mdat` so that `moov` comes
/// first, in place: `moov` is read (it is the index, a few bytes per
/// sample), the media from the first `mdat` on is moved forward by its size
/// a chunk at a time, every `stco`/`co64` offset is shifted (an `stco`
/// becomes a `co64` when an offset passes 4 GiB), and `moov` is written
/// where `mdat` began. Memory: `moov` plus a 1 MiB buffer.
///
/// Returns `Ok(false)` when the file already has `moov` before its media.
/// Fragmented files (`moof`) are refused. If an I/O error interrupts the
/// move, the file is left damaged: write to a copy when that matters.
pub fn faststart_in_place<F: Read + Write + Seek + ?Sized>(f: &mut F) -> Result<bool> {
    let tops = top_boxes(f)?;
    if tops.iter().any(|t| &t.typ == b"moof") {
        return Err(Error::unsupported(
            "mp4 faststart: a fragmented file has no single moov to move",
        ));
    }
    let moov_at = tops
        .iter()
        .position(|t| &t.typ == b"moov")
        .ok_or_else(|| Error::invalid("mp4 faststart: no `moov`"))?;
    let Some(mdat_at) = tops.iter().position(|t| &t.typ == b"mdat") else {
        return Ok(false);
    };
    if moov_at < mdat_at {
        return Ok(false);
    }
    let moov = tops[moov_at];
    let b_start = tops[mdat_at].start;
    let moov_end = moov.start + moov.size;
    let file_end = tops.last().map_or(0, |t| t.start + t.size);
    if moov.size > 1 << 30 {
        return Err(Error::unsupported("mp4 faststart: `moov` over 1 GiB"));
    }
    let header = if be32_at(f, moov.start)? == 1 { 16 } else { 8 };
    f.seek(SeekFrom::Start(moov.start + header))?;
    let mut body = vec![0u8; (moov.size - header) as usize];
    f.read_exact(&mut body)?;

    // New layout: [before mdat][moov'][mdat .. moov)[after moov]. Offsets in
    // the moved run shift by |moov'|, those past the old moov by the growth.
    let mut new_len = moov.size;
    let mut patched = Vec::new();
    for _ in 0..4 {
        let grow = new_len - moov.size.min(new_len);
        let shift = |o: u64| {
            if o >= b_start && o < moov.start {
                o + new_len
            } else if o >= moov_end {
                o + grow
            } else {
                o
            }
        };
        patched = bx(b"moov", &patch_children(&body, &shift));
        if patched.len() as u64 == new_len {
            break;
        }
        new_len = patched.len() as u64;
    }
    if patched.len() as u64 != new_len || new_len < moov.size {
        return Err(Error::invalid("mp4 faststart: moov layout did not settle"));
    }
    let grow = new_len - moov.size;
    move_forward(f, moov_end, moov_end + grow, file_end - moov_end)?;
    move_forward(f, b_start, b_start + new_len, moov.start - b_start)?;
    f.seek(SeekFrom::Start(b_start))?;
    f.write_all(&patched)?;
    f.flush()?;
    Ok(true)
}

fn be32_at<F: Read + Seek + ?Sized>(f: &mut F, at: u64) -> io::Result<u32> {
    let mut b = [0u8; 4];
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// [`patch_children`] with every offset shifted by `shift`, for tests.
#[cfg(test)]
pub(crate) fn tests_patch(body: &[u8], shift: u64) -> Vec<u8> {
    patch_children(body, &|o| o + shift)
}
