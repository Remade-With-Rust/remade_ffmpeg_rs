//! Header and IFD parsing over a bounds-checked reader.
//!
//! Every read is checked against the stream length BEFORE anything is
//! allocated, so a lying count or offset costs an error, not memory.

use std::collections::BTreeMap;
use std::io::{ErrorKind, IoSliceMut, Read, Seek, SeekFrom};

use crate::error::{TiffError, TiffResult};
use crate::tags::{Tag, Type};
use crate::value::Value;

/// Byte order of a TIFF file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    /// `II`: little-endian.
    LittleEndian,
    /// `MM`: big-endian.
    BigEndian,
}

/// A reader that knows its own length and refuses out-of-range reads.
pub(crate) struct Stream<R> {
    pub(crate) inner: R,
    pub(crate) len: u64,
    pub(crate) big_endian: bool,
}

impl<R: Read + Seek> Stream<R> {
    pub(crate) fn new(mut inner: R) -> TiffResult<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self {
            inner,
            len,
            big_endian: false,
        })
    }

    /// Read exactly `n` bytes at `offset`; errors if the range leaves the file.
    pub(crate) fn read_at(&mut self, offset: u64, n: u64) -> TiffResult<Vec<u8>> {
        let end = offset
            .checked_add(n)
            .ok_or_else(|| TiffError::format("offset overflow"))?;
        if end > self.len {
            return Err(TiffError::format(format!(
                "range {offset}..{end} past end of file ({} bytes)",
                self.len
            )));
        }
        let n = usize::try_from(n).map_err(|_| TiffError::limits("read too large"))?;
        let mut buf = Vec::new();
        self.read_at_into(offset, n, &mut buf)?;
        Ok(buf)
    }

    /// Append exactly `n` bytes at `offset` to `out`, reading straight into
    /// its spare capacity: no zero-fill first (the old `zeroed` + `read_exact`
    /// wrote every byte twice). The caller has range-checked `offset + n`.
    pub(crate) fn read_at_into(
        &mut self,
        offset: u64,
        n: usize,
        out: &mut Vec<u8>,
    ) -> TiffResult<()> {
        out.try_reserve_exact(n)
            .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
        self.inner.seek(SeekFrom::Start(offset))?;
        let before = out.len();
        (&mut self.inner).take(n as u64).read_to_end(out)?;
        if out.len() - before != n {
            return Err(TiffError::format(format!(
                "range at {offset} ended after {} of {n} bytes",
                out.len() - before
            )));
        }
        Ok(())
    }

    /// Read the bytes at `offset` straight into `bufs`, in order, filling
    /// every buffer: one vectored read where the reader supports it (a
    /// `Cursor` copies into all of them in one call; a Unix `File` issues
    /// one `readv`; a `BufReader` passes a read this large straight to its
    /// inner reader). The caller has range-checked the bytes.
    ///
    /// A reader that fills only one buffer per call (the default
    /// `read_vectored`, which a Windows `File` uses) would cost a call per
    /// buffer, so as soon as a call stops at the end of its first buffer the
    /// rest is read in one go into `scratch` and copied out.
    pub(crate) fn read_at_scatter(
        &mut self,
        offset: u64,
        mut bufs: &mut [IoSliceMut<'_>],
        scratch: &mut Vec<u8>,
    ) -> TiffResult<()> {
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        let short = |done: usize| {
            TiffError::format(format!(
                "range at {offset} ended after {done} of {total} bytes"
            ))
        };
        self.inner.seek(SeekFrom::Start(offset))?;
        let mut done = 0;
        IoSliceMut::advance_slices(&mut bufs, 0);
        while let Some(first) = bufs.first().map(|b| b.len()) {
            let k = match self.inner.read_vectored(bufs) {
                Ok(0) => return Err(short(done)),
                Ok(k) => k,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            done += k;
            if done == total {
                return Ok(());
            }
            IoSliceMut::advance_slices(&mut bufs, k);
            if k <= first && !bufs.is_empty() {
                let rest = total - done;
                scratch.clear();
                self.read_at_into(offset + done as u64, rest, scratch)
                    .map_err(|_| short(done))?;
                let mut src = &scratch[..];
                for b in bufs.iter_mut() {
                    let (head, tail) = src.split_at(b.len());
                    b.copy_from_slice(head);
                    src = tail;
                }
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) fn u16_from(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.big_endian {
            u16::from_be_bytes(a)
        } else {
            u16::from_le_bytes(a)
        }
    }
    pub(crate) fn u32_from(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.big_endian {
            u32::from_be_bytes(a)
        } else {
            u32::from_le_bytes(a)
        }
    }
    pub(crate) fn u64_from(&self, b: &[u8]) -> u64 {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[..8]);
        if self.big_endian {
            u64::from_be_bytes(a)
        } else {
            u64::from_le_bytes(a)
        }
    }
}

/// One raw IFD entry: type, count and the inline value/offset field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub(crate) ty: u16,
    pub(crate) count: u64,
    /// The 4-byte (classic) or 8-byte (BigTIFF) value-or-offset field.
    pub(crate) field: [u8; 8],
}

impl Entry {
    /// The field type, or `None` for a type code TIFF does not define.
    #[must_use]
    pub fn field_type(&self) -> Option<Type> {
        Type::from_u16(self.ty)
    }

    /// The raw field-type code.
    #[must_use]
    pub fn field_type_code(&self) -> u16 {
        self.ty
    }

    /// The element count.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// An image file directory: its entries (values are read on demand through
/// the [`crate::Decoder`]) and the offset of the next IFD in its chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ifd {
    pub(crate) offset: u64,
    pub(crate) next: u64,
    pub(crate) entries: BTreeMap<u16, Entry>,
}

impl Ifd {
    /// File offset of this IFD.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Offset of the next IFD in the chain (0 = none).
    #[must_use]
    pub fn next_offset(&self) -> u64 {
        self.next
    }

    /// The tags present, in ascending numeric order.
    pub fn tags(&self) -> impl Iterator<Item = Tag> + '_ {
        self.entries.keys().map(|&k| Tag::from_u16(k))
    }

    /// The raw entry for a tag.
    #[must_use]
    pub fn entry(&self, tag: Tag) -> Option<&Entry> {
        self.entries.get(&tag.to_u16())
    }

    /// Whether the tag is present.
    #[must_use]
    pub fn contains(&self, tag: Tag) -> bool {
        self.entries.contains_key(&tag.to_u16())
    }
}

/// Parse the 8-byte (classic) or 16-byte (BigTIFF) header. Returns
/// `(bigtiff, first_ifd_offset)` and sets the stream byte order.
pub(crate) fn read_header<R: Read + Seek>(s: &mut Stream<R>) -> TiffResult<(bool, u64)> {
    if s.len < 8 {
        return Err(TiffError::format("file too short for a TIFF header"));
    }
    let h = s.read_at(0, 8)?;
    s.big_endian = match &h[0..2] {
        b"II" => false,
        b"MM" => true,
        _ => return Err(TiffError::format("not a TIFF (bad byte-order mark)")),
    };
    match s.u16_from(&h[2..4]) {
        42 => Ok((false, u64::from(s.u32_from(&h[4..8])))),
        43 => {
            let bytesize = s.u16_from(&h[4..6]);
            let zero = s.u16_from(&h[6..8]);
            if bytesize != 8 || zero != 0 {
                return Err(TiffError::format("bad BigTIFF header"));
            }
            let h2 = s.read_at(8, 8)?;
            Ok((true, s.u64_from(&h2)))
        }
        v => Err(TiffError::format(format!("bad TIFF version {v}"))),
    }
}

/// Read the IFD at `offset`.
pub(crate) fn read_ifd<R: Read + Seek>(
    s: &mut Stream<R>,
    bigtiff: bool,
    offset: u64,
) -> TiffResult<Ifd> {
    if offset == 0 {
        return Err(TiffError::format("IFD offset 0"));
    }
    let (count_size, entry_size, next_size) = if bigtiff { (8, 20, 8) } else { (2, 12, 4) };
    let cb = s.read_at(offset, count_size)?;
    let n = if bigtiff {
        s.u64_from(&cb)
    } else {
        u64::from(s.u16_from(&cb))
    };
    // A lying BigTIFF count is caught here: the table must fit in the file.
    let table_len = n
        .checked_mul(entry_size)
        .ok_or_else(|| TiffError::format("IFD entry count overflow"))?;
    if n > 65_535 * 4 {
        return Err(TiffError::format(format!("IFD with {n} entries")));
    }
    let table = s.read_at(offset + count_size, table_len)?;
    let next_off = offset + count_size + table_len;
    // A missing next pointer at EOF is tolerated as "no next IFD".
    let next = match s.read_at(next_off, next_size) {
        Ok(b) => {
            if bigtiff {
                s.u64_from(&b)
            } else {
                u64::from(s.u32_from(&b))
            }
        }
        Err(_) => 0,
    };
    let mut entries = BTreeMap::new();
    for e in table.chunks_exact(entry_size as usize) {
        let tag = s.u16_from(&e[0..2]);
        let ty = s.u16_from(&e[2..4]);
        let mut field = [0u8; 8];
        let count = if bigtiff {
            field.copy_from_slice(&e[12..20]);
            s.u64_from(&e[4..12])
        } else {
            field[..4].copy_from_slice(&e[8..12]);
            u64::from(s.u32_from(&e[4..8]))
        };
        // The last occurrence wins on a duplicate tag, as in the `tiff`
        // crate: a bit flip that turns one tag number into another's
        // duplicates the second tag ahead of its real entry (ArtCraft's
        // case: BitsPerSample 258 -> 259 put Compression 8 before the real 1).
        entries.insert(tag, Entry { ty, count, field });
    }
    Ok(Ifd {
        offset,
        next,
        entries,
    })
}

/// Read and decode an entry's value, bounded by `max_bytes`.
pub(crate) fn read_value<R: Read + Seek>(
    s: &mut Stream<R>,
    bigtiff: bool,
    e: &Entry,
    max_bytes: usize,
) -> TiffResult<Value> {
    let ty = Type::from_u16(e.ty)
        .ok_or_else(|| TiffError::format(format!("unknown field type {}", e.ty)))?;
    let total = e
        .count
        .checked_mul(ty.size() as u64)
        .ok_or_else(|| TiffError::format("tag value size overflow"))?;
    if total > max_bytes as u64 {
        return Err(TiffError::limits(format!(
            "tag value of {total} bytes exceeds ifd_value_size {max_bytes}"
        )));
    }
    let inline = if bigtiff { 8 } else { 4 };
    let bytes = if total <= inline {
        e.field[..total as usize].to_vec()
    } else {
        let off = if bigtiff {
            s.u64_from(&e.field)
        } else {
            u64::from(s.u32_from(&e.field))
        };
        s.read_at(off, total)?
    };
    Ok(Value::decode(ty, &bytes, s.big_endian))
}

/// The raw bytes of an entry's value, in file byte order.
pub(crate) fn read_raw<R: Read + Seek>(
    s: &mut Stream<R>,
    bigtiff: bool,
    e: &Entry,
    max_bytes: usize,
) -> TiffResult<Vec<u8>> {
    let ty = Type::from_u16(e.ty)
        .ok_or_else(|| TiffError::format(format!("unknown field type {}", e.ty)))?;
    let total = e
        .count
        .checked_mul(ty.size() as u64)
        .ok_or_else(|| TiffError::format("tag value size overflow"))?;
    if total > max_bytes as u64 {
        return Err(TiffError::limits("tag value exceeds ifd_value_size"));
    }
    let inline = if bigtiff { 8 } else { 4 };
    if total <= inline {
        Ok(e.field[..total as usize].to_vec())
    } else {
        let off = if bigtiff {
            s.u64_from(&e.field)
        } else {
            u64::from(s.u32_from(&e.field))
        };
        s.read_at(off, total)
    }
}
