//! Random access over a Matroska file ([`Demuxer::sample_index`], #114).
//!
//! The index is a per-stream frame table in file (decode) order, one entry
//! per frame: a laced block contributes one entry per lace, with the lace's
//! own offset and size and the timing `read_packet` gives that frame. It is
//! built on the first `sample_index` call by running the same Cluster walk as
//! `read_packet` (known- and unknown-size Clusters, resync on corrupt
//! framing, a cut-off final block dropped) over a second, small-buffered
//! reader that *seeks* past frame payloads: only block headers, the lace
//! sizes of laced blocks (which are small: audio) and the small children of
//! BlockGroups (BlockDuration, DiscardPadding, ReferenceBlock) are read.
//! BlockAdditions are recorded by position and read with the frame.
//!
//! What the entries hold, against what `read_packet` returns for the frame:
//!
//! * `offset`/`size` locate the frame bytes as stored: after the lace
//!   header, before any ContentEncoding is undone (header-stripped bytes are
//!   not there) and, for ProRes, without the 8-byte frame header.
//! * `pts` and `duration` are the packet's. Matroska records presentation
//!   time only and `read_packet` leaves `dts` unset, so the entry's `dts` is
//!   its `pts`. A laced frame `read_packet` gives no timestamp (lacing
//!   without any duration to share out) reports the last timestamp of its
//!   block, and is never a [`sync_at_or_before`] answer.
//! * `sync` is the packet's keyframe flag: the SimpleBlock flag bit, or for
//!   a BlockGroup the absence of a ReferenceBlock.
//!
//! [`read_sample`] reads the stored bytes by offset and shapes them exactly as
//! `read_packet` does (ContentEncodings, ProRes header, AVCC → Annex-B, ASS
//! reduction, raw mode), with the block's additions and DiscardPadding. A
//! frame whose ContentEncoding does not decode, which `read_packet` silently
//! drops, is indexed (the scan does not read payloads) and `read_sample`
//! returns an error for it.
//!
//! [`sync_at_or_before`] answers from a per-stream table of the sync frames
//! sorted by pts (a binary search). Cues are not consulted: by the time the
//! index exists the scan has met every keyframe, which Cues only sample
//! (rff's muxer writes one CuePoint per Cluster, for one track) and which a
//! stale or partial Cues element could contradict.
//!
//! [`Demuxer::sample_index`]: rff_format::Demuxer::sample_index
//! [`read_sample`]: SampleIndex::read_sample
//! [`sync_at_or_before`]: SampleIndex::sync_at_or_before

use std::io::{self, BufReader, Read, Seek, SeekFrom};

use rff_core::{Error, Packet, Result};
use rff_format::{SampleIndex, SampleInfo};

use crate::{
    discard_samples, parse_block_additions, read_byte, split_laces, Ebml, Inner, MkvDemuxer, Src,
    Tracks, Walk, ID_BLOCK, ID_BLOCK_ADDITIONS, ID_BLOCK_DURATION, ID_DISCARD_PADDING,
    ID_REFERENCE_BLOCK, ID_SIMPLE_BLOCK, MAX_BLOCK,
};

/// The scan's read-ahead: small, since after each block header the reader
/// seeks past the frame and the rest of the buffer is usually wasted.
const SCAN_BUF: usize = 4 * 1024;

/// [`Entry::additions`] for a frame without BlockAdditions.
const NO_ADDITIONS: u32 = u32::MAX;

/// One frame of the index.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// File offset of the stored frame.
    offset: u64,
    size: u32,
    /// The packet's pts, or the block's last known one (see `pts_known`).
    pts: i64,
    /// `read_packet` gives this frame a pts.
    pts_known: bool,
    duration: i64,
    sync: bool,
    /// Samples of DiscardPadding (the last frame of a BlockGroup's block).
    discard_padding: u32,
    /// Index into [`Index::additions`], or [`NO_ADDITIONS`].
    additions: u32,
}

/// The per-stream frame tables.
#[derive(Debug, Default)]
pub(crate) struct Index {
    entries: Vec<Vec<Entry>>,
    /// Per stream, `(pts, sample)` for each sync frame with a known pts,
    /// sorted by pts; `sample` is the latest (in decode order) sync frame at
    /// or before that pts.
    sync: Vec<Vec<(i64, u64)>>,
    /// Per BlockGroup with additions: its BlockAdditions bodies
    /// (file offset, length).
    additions: Vec<Vec<(u64, u64)>>,
}

impl Index {
    fn finish(&mut self) {
        self.sync = self
            .entries
            .iter()
            .map(|entries| {
                let mut sync: Vec<(i64, u64)> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.sync && e.pts_known)
                    .map(|(i, e)| (e.pts, i as u64))
                    .collect();
                sync.sort_unstable();
                let mut latest = 0;
                for s in &mut sync {
                    latest = latest.max(s.1);
                    s.1 = latest;
                }
                sync
            })
            .collect();
    }
}

/// The scan's reader: the demuxer's own input (its buffered reader set
/// aside), positioned independently and seeking past frames. `len` is the
/// input's length, so a skip clamps at end of input as a streaming one would.
struct ScanSrc<'a> {
    r: BufReader<&'a mut Inner>,
    pos: u64,
    base: u64,
    len: u64,
}

impl Src for ScanSrc<'_> {
    fn pos(&self) -> u64 {
        self.pos
    }

    fn byte(&mut self) -> io::Result<Option<u8>> {
        let b = read_byte(&mut self.r)?;
        if b.is_some() {
            self.pos += 1;
        }
        Ok(b)
    }

    fn skip(&mut self, n: u64) -> io::Result<()> {
        let to = self.pos.saturating_add(n).min(self.len).max(self.pos);
        self.seek_to(to)
    }

    fn body(&mut self, n: u64) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        (&mut self.r).take(n).read_to_end(&mut v)?;
        self.pos += v.len() as u64;
        Ok(v)
    }
}

impl ScanSrc<'_> {
    /// Move to `pos` (inside the read-ahead when it can).
    fn seek_to(&mut self, pos: u64) -> io::Result<()> {
        if pos != self.pos {
            match i64::try_from(pos as i128 - self.pos as i128) {
                Ok(delta) => self.r.seek_relative(delta)?,
                Err(_) => {
                    self.r
                        .seek(SeekFrom::Start(self.base.saturating_add(pos)))?;
                }
            }
            self.pos = pos;
        }
        Ok(())
    }
}

/// Where one (Simple)Block sits and what its container said about it.
struct BlockAt {
    at: u64,
    len: u64,
    cluster_ts: i64,
    /// BlockDuration (0: none).
    duration: i64,
    /// Decided by the BlockGroup (`None`: the SimpleBlock flag bit).
    keyframe: Option<bool>,
    discard_ns: i64,
    additions: u32,
}

impl MkvDemuxer {
    /// Scan the Clusters into an [`Index`]. The streaming walk's position is
    /// restored afterwards, whatever happens.
    pub(crate) fn build_index(&mut self) -> io::Result<Index> {
        let streams = self.streams.len();
        let Some(start) = self.clusters_at else {
            let mut idx = Index {
                entries: vec![Vec::new(); streams],
                ..Index::default()
            };
            idx.finish();
            return Ok(idx);
        };
        let walk = Walk {
            seg_end: self.walk.seg_end,
            ..Walk::default()
        };
        let base = self.src.base;
        let result = scan_input(self.src.r.get_mut(), &self.t, base, start, walk, streams);
        self.src.restore()?;
        result
    }
}

fn scan_input(
    inner: &mut Inner,
    t: &Tracks,
    base: u64,
    start: u64,
    walk: Walk,
    streams: usize,
) -> io::Result<Index> {
    let len = inner.seek(SeekFrom::End(0))?.saturating_sub(base);
    inner.seek(SeekFrom::Start(base + start))?;
    let mut src = ScanSrc {
        r: BufReader::with_capacity(SCAN_BUF, inner),
        pos: start,
        base,
        len,
    };
    scan(t, &mut src, walk, streams)
}

/// The walk `read_packet` makes, indexing blocks instead of reading them.
fn scan(t: &Tracks, src: &mut ScanSrc, mut walk: Walk, streams: usize) -> io::Result<Index> {
    let mut idx = Index {
        entries: vec![Vec::new(); streams],
        ..Index::default()
    };
    while let Some((h, ts)) = walk.next_block(src)? {
        if h.size > MAX_BLOCK {
            let len = walk.bounded(&h);
            src.skip(len)?;
            continue;
        }
        let end = h.start.saturating_add(h.size);
        if end > src.len {
            // As `read_packet`: a block cut off by end of input is dropped,
            // and the input is exhausted.
            walk.done = true;
            continue;
        }
        if h.id == ID_SIMPLE_BLOCK {
            let block = BlockAt {
                at: h.start,
                len: h.size,
                cluster_ts: ts,
                duration: 0,
                keyframe: None,
                discard_ns: 0,
                additions: NO_ADDITIONS,
            };
            index_block(t, src, &mut idx, block)?;
        } else {
            scan_group(t, src, &mut idx, h.start, end, ts)?;
        }
        src.seek_to(end)?;
    }
    idx.finish();
    Ok(idx)
}

/// A BlockGroup body `[start, end)`, walked the way
/// `Tracks::parse_block_group` walks it in memory (child headers clipped at
/// the group's end, the last Block winning), but reading only the small
/// children.
fn scan_group(
    t: &Tracks,
    src: &mut ScanSrc,
    idx: &mut Index,
    start: u64,
    end: u64,
    cluster_ts: i64,
) -> io::Result<()> {
    let mut pos = start;
    let mut block: Option<(u64, u64)> = None;
    let mut duration = 0i64;
    let mut discard_ns = 0i64;
    let mut referenced = false;
    let mut additions = Vec::new();
    while pos < end {
        src.seek_to(pos)?;
        // An ID and a size take at most 12 bytes.
        let window = src.body((end - pos).min(12))?;
        let mut e = Ebml::new(&window);
        let Some(id) = e.read_id() else { break };
        let Some((size, _)) = e.read_size() else {
            break;
        };
        let body = pos + e.pos as u64;
        match id {
            ID_BLOCK => block = Some((body, size.min(end - body))),
            ID_BLOCK_DURATION => duration = int_at(src, body, size, end, false)?,
            ID_DISCARD_PADDING => discard_ns = int_at(src, body, size, end, true)?,
            ID_REFERENCE_BLOCK => referenced = true,
            ID_BLOCK_ADDITIONS => additions.push((src.base + body, size.min(end - body))),
            _ => {}
        }
        pos = body.saturating_add(size);
    }
    let Some((at, len)) = block else {
        return Ok(());
    };
    let additions = if additions.is_empty() {
        NO_ADDITIONS
    } else {
        idx.additions.push(additions);
        u32::try_from(idx.additions.len() - 1).unwrap_or(NO_ADDITIONS)
    };
    let block = BlockAt {
        at,
        len,
        cluster_ts,
        duration,
        keyframe: Some(!referenced),
        discard_ns,
        additions,
    };
    index_block(t, src, idx, block)
}

/// The integer element of `size` bytes whose body starts at `body`, read as
/// [`Ebml::read_uint`] / [`Ebml::read_int`] read it from a group body ending
/// at `end`: only the last 8 bytes count, and bytes past `end` read as zero.
fn int_at(src: &mut ScanSrc, body: u64, size: u64, end: u64, signed: bool) -> io::Result<i64> {
    let n = size.min(8);
    let first = body.saturating_add(size - n);
    let stop = body.saturating_add(size).min(end);
    let bytes = if first < stop {
        src.seek_to(first)?;
        src.body(stop - first)?
    } else {
        Vec::new()
    };
    let mut v = 0u64;
    for i in 0..n as usize {
        v = (v << 8) | u64::from(bytes.get(i).copied().unwrap_or(0));
    }
    Ok(match n {
        0 => 0,
        n if signed => {
            let shift = 64 - 8 * n as u32;
            ((v << shift) as i64) >> shift
        }
        _ => v as i64,
    })
}

/// Index one (Simple)Block: its header, and for a laced block its lace sizes
/// (the whole block is read, as `read_packet` would), then one entry per
/// frame. A block `read_packet` would drop (too short, unknown track, lace
/// sizes that don't fit) adds nothing.
fn index_block(t: &Tracks, src: &mut ScanSrc, idx: &mut Index, b: BlockAt) -> io::Result<()> {
    let Ok(len) = usize::try_from(b.len) else {
        return Ok(());
    };
    src.seek_to(b.at)?;
    let mut data = src.body(b.len.min(16))?;
    let Some(head) = t.block_head(&data, len) else {
        return Ok(());
    };
    let keyframe = b.keyframe.unwrap_or(head.flags & 0x80 != 0);
    let payload_at = head.payload_at as u64;
    let laces: Vec<(u64, u64)> = if head.lacing() == 0 {
        vec![(b.at + payload_at, b.len - payload_at)]
    } else {
        let rest = src.body(b.len - data.len() as u64)?;
        data.extend_from_slice(&rest);
        if data.len() != len {
            return Ok(());
        }
        let payload = &data[head.payload_at..];
        let Some(frames) = split_laces(payload, head.lacing()) else {
            return Ok(());
        };
        // The frames are back to back at the end of the payload.
        let total: usize = frames.iter().map(|f| f.len()).sum();
        let mut at = b.at + payload_at + (payload.len() - total) as u64;
        frames
            .iter()
            .map(|f| {
                let lace = (at, f.len() as u64);
                at += f.len() as u64;
                lace
            })
            .collect()
    };
    let times = t.lace_times(head.index, b.cluster_ts, head.rel, b.duration, laces.len());
    let rate = t.sample_rate.get(head.index).copied().unwrap_or(0);
    let Some(entries) = idx.entries.get_mut(head.index) else {
        return Ok(());
    };
    let count = laces.len();
    let mut last_pts = 0;
    for (n, ((offset, size), (pts, duration))) in laces.into_iter().zip(times).enumerate() {
        // Only a frame after the first can lack a pts; it reports the last.
        let resolved = pts.unwrap_or(last_pts);
        last_pts = resolved;
        entries.push(Entry {
            offset: src.base + offset,
            size: u32::try_from(size).unwrap_or(u32::MAX),
            pts: resolved,
            pts_known: pts.is_some(),
            duration,
            sync: keyframe,
            discard_padding: if n + 1 == count {
                discard_samples(b.discard_ns, rate)
            } else {
                0
            },
            additions: b.additions,
        });
    }
    Ok(())
}

impl MkvDemuxer {
    fn entry(&self, stream: usize, index: u64) -> Option<&Entry> {
        let entries = self.index.as_ref()?.entries.get(stream)?;
        entries.get(usize::try_from(index).ok()?)
    }
}

impl SampleIndex for MkvDemuxer {
    fn sample_count(&self, stream: usize) -> u64 {
        self.index
            .as_ref()
            .and_then(|i| i.entries.get(stream))
            .map_or(0, |e| e.len() as u64)
    }

    fn sample_info(&self, stream: usize, index: u64) -> Option<SampleInfo> {
        let e = self.entry(stream, index)?;
        Some(SampleInfo {
            offset: e.offset,
            size: e.size,
            dts: e.pts,
            pts: e.pts,
            duration: e.duration,
            sync: e.sync,
        })
    }

    fn read_sample(&mut self, stream: usize, index: u64) -> Result<Packet> {
        let e = self
            .entry(stream, index)
            .cloned()
            .ok_or_else(|| Error::invalid(format!("mkv: stream {stream} has no sample {index}")))?;
        let stored = self.src.read_at(e.offset, u64::from(e.size))?;
        if stored.len() < e.size as usize {
            return Err(Error::invalid(format!(
                "mkv: sample {index} of stream {stream} is cut short by end of input"
            )));
        }
        let mut additions = Vec::new();
        let places = self
            .index
            .as_ref()
            .and_then(|i| i.additions.get(e.additions as usize))
            .cloned()
            .unwrap_or_default();
        for (at, len) in places {
            additions.extend(parse_block_additions(&self.src.read_at(at, len)?));
        }
        let data = self.t.shape(stream, &stored, e.sync).ok_or_else(|| {
            Error::invalid(format!(
                "mkv: sample {index} of stream {stream} does not decode (corrupt \
                 ContentEncoding)"
            ))
        })?;
        let mut p = Packet::from_data(stream, data);
        p.pts = e.pts_known.then_some(e.pts);
        p.duration = e.duration;
        p.flags.keyframe = e.sync;
        p.discard_padding = e.discard_padding;
        p.additions = additions;
        Ok(p)
    }

    fn sync_at_or_before(&self, stream: usize, pts: i64) -> Option<u64> {
        let sync = self.index.as_ref()?.sync.get(stream)?;
        let n = sync.partition_point(|&(p, _)| p <= pts);
        n.checked_sub(1).map(|i| sync[i].1)
    }
}
