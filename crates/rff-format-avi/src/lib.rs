//! AVI (Audio Video Interleaved) container.
//!
//! AVI is a RIFF file: a tree of `LIST`/chunk records. Layout we read:
//!
//! ```text
//!   RIFF "AVI "
//!     LIST "hdrl"          stream headers
//!       "avih"             main header (frame size, stream count)
//!       LIST "strl"        one per stream
//!         "strh"           stream header (type, codec fourcc, rate)
//!         "strf"           stream format (BITMAPINFOHEADER / WAVEFORMATEX)
//!     LIST "movi"          interleaved data chunks ("##dc", "##wb", ...)
//!     "idx1"               (optional) legacy index — not needed to demux
//! ```
//!
//! Status: **demuxer implemented** (reads headers and yields packets); the
//! muxer is still scaffolded. Note: AVI carries codecs we don't decode yet
//! (e.g. H.264), so a full transcode out of an AVI waits on those codec bodies
//! — but probing and stream-copy remuxing work today.

use std::collections::VecDeque;
use std::io::Read;
use std::ops::Range;

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the AVI format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "avi",
        long_name: "AVI (Audio Video Interleaved)",
        extensions: &["avi"],
        demuxer: Some(|input| Box::new(AviDemuxer::new(input))),
        muxer: Some(|output| Box::new(AviMuxer::new(output))),
        probe: Some(probe_avi),
    });
}

/// Sniff AVI: a RIFF file whose form type is `AVI `.
fn probe_avi(data: &[u8]) -> i32 {
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"AVI " {
        100
    } else {
        0
    }
}

// ===========================================================================
// RIFF parsing helpers
// ===========================================================================

fn rd_u16(buf: &[u8], at: usize) -> u16 {
    buf.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0)
}

fn rd_u32(buf: &[u8], at: usize) -> u32 {
    buf.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}

fn fourcc(buf: &[u8], at: usize) -> [u8; 4] {
    let mut f = [0u8; 4];
    if let Some(s) = buf.get(at..at + 4) {
        f.copy_from_slice(s);
    }
    f
}

/// Parse sibling RIFF chunks in `buf[start..end]` into `(id, data_range)` pairs.
/// Chunk = `[4cc id][u32 LE size][size bytes]`, padded to an even boundary.
fn riff_chunks(buf: &[u8], mut p: usize, end: usize) -> Vec<([u8; 4], Range<usize>)> {
    let mut out = Vec::new();
    while p + 8 <= end {
        let id = fourcc(buf, p);
        let size = rd_u32(buf, p + 4) as usize;
        let data_start = p + 8;
        let data_end = data_start.saturating_add(size);
        if data_end > end {
            break; // truncated chunk
        }
        out.push((id, data_start..data_end));
        // Chunks are word-aligned: an odd size carries one pad byte.
        p = data_end + (size & 1);
    }
    out
}

/// Find a `LIST` chunk of the given list type, returning its data range
/// (including the 4-byte list type at the front).
fn find_list(chunks: &[([u8; 4], Range<usize>)], buf: &[u8], list_type: &[u8; 4]) -> Option<Range<usize>> {
    chunks
        .iter()
        .find(|(id, r)| id == b"LIST" && r.len() >= 4 && &buf[r.start..r.start + 4] == list_type)
        .map(|(_, r)| r.clone())
}

/// Map a video codec fourcc (from `strf`/`strh`) to a [`CodecId`].
fn map_video_fourcc(mut f: [u8; 4]) -> CodecId {
    f.make_ascii_uppercase();
    match &f {
        b"H264" | b"X264" | b"AVC1" | b"DAVC" => CodecId::H264,
        b"AV01" => CodecId::Avif,
        _ => CodecId::None,
    }
}

/// Map a WAVE `(format_tag, bits)` to a PCM [`SampleFormat`] (1=int, 3=float).
fn pcm_format(tag: u16, bits: u16) -> Option<SampleFormat> {
    match (tag, bits) {
        (1, 16) => Some(SampleFormat::S16),
        (3, 32) => Some(SampleFormat::F32),
        _ => None,
    }
}

// ===========================================================================
// Demuxer
// ===========================================================================

struct AviDemuxer {
    input: Option<Input>,
    /// Packets pre-extracted from `movi` during `read_header`.
    packets: VecDeque<Packet>,
}

impl AviDemuxer {
    fn new(input: Input) -> AviDemuxer {
        AviDemuxer {
            input: Some(input),
            packets: VecDeque::new(),
        }
    }
}

impl Demuxer for AviDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("avi demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;

        if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"AVI " {
            return Err(Error::invalid("avi demux: not a RIFF/AVI file"));
        }

        let top = riff_chunks(&buf, 12, buf.len());
        let hdrl = find_list(&top, &buf, b"hdrl")
            .ok_or_else(|| Error::invalid("avi demux: no `hdrl` list"))?;
        let hdrl_children = riff_chunks(&buf, hdrl.start + 4, hdrl.end);

        // Main header gives the frame size shared by the video stream(s).
        let (frame_w, frame_h) = match find(&hdrl_children, b"avih") {
            Some(r) => (rd_u32(&buf, r.start + 32), rd_u32(&buf, r.start + 36)),
            None => (0, 0),
        };

        // One stream per `strl` list.
        let mut streams = Vec::new();
        for (id, r) in &hdrl_children {
            if id != b"LIST" || r.len() < 4 || &buf[r.start..r.start + 4] != b"strl" {
                continue;
            }
            if let Some(stream) =
                parse_strl(&buf, r.start + 4, r.end, streams.len(), frame_w, frame_h)
            {
                streams.push(stream);
            }
        }

        // Pre-extract every data chunk in `movi` into a packet.
        if let Some(movi) = find_list(&top, &buf, b"movi") {
            collect_movi_packets(&buf, movi, &mut self.packets);
        }

        Ok(streams)
    }

    fn read_packet(&mut self) -> Result<Packet> {
        self.packets.pop_front().ok_or(Error::Eof)
    }
}

fn find<'a>(chunks: &[([u8; 4], Range<usize>)], id: &[u8; 4]) -> Option<Range<usize>> {
    chunks.iter().find(|(i, _)| i == id).map(|(_, r)| r.clone())
}

/// Build a [`Stream`] from one `strl` list (its `strh` + `strf`).
fn parse_strl(
    buf: &[u8],
    start: usize,
    end: usize,
    index: usize,
    frame_w: u32,
    frame_h: u32,
) -> Option<Stream> {
    let children = riff_chunks(buf, start, end);
    let strh = find(&children, b"strh")?;
    let strf = find(&children, b"strf");

    let fcc_type = fourcc(buf, strh.start); // 'vids' / 'auds'
    let fcc_handler = fourcc(buf, strh.start + 4);
    let scale = rd_u32(buf, strh.start + 20);
    let rate = rd_u32(buf, strh.start + 24);
    let time_base = if rate != 0 {
        Rational::new(scale.max(1) as i32, rate as i32)
    } else {
        Rational::new(1, 1000)
    };

    let mut stream = match &fcc_type {
        b"vids" => {
            // Codec fourcc: prefer biCompression (strf+16), else the handler.
            let codec_fourcc = strf
                .as_ref()
                .map(|r| fourcc(buf, r.start + 16))
                .filter(|f| f != &[0; 4])
                .unwrap_or(fcc_handler);
            let mut s = Stream::new(index, map_video_fourcc(codec_fourcc));
            s.media_type = MediaType::Video;
            s.width = frame_w;
            s.height = frame_h;
            s
        }
        b"auds" => {
            let mut s = Stream::new(index, CodecId::None);
            s.media_type = MediaType::Audio;
            if let Some(r) = &strf {
                s.channels = rd_u16(buf, r.start + 2);
                s.sample_rate = rd_u32(buf, r.start + 4);
                // WAVEFORMATEX: wFormatTag@0, wBitsPerSample@14 → recognize PCM.
                if let Some(fmt) = pcm_format(rd_u16(buf, r.start), rd_u16(buf, r.start + 14)) {
                    s.codec_id = CodecId::Pcm;
                    s.sample_format = Some(fmt);
                }
            }
            s
        }
        _ => return None, // subtitle/data streams: skip for now
    };
    stream.time_base = time_base;
    Some(stream)
}

/// Walk a `movi` list (data range includes the leading `movi` 4cc) and turn
/// every data chunk into a packet, descending into `rec ` interleave groups.
fn collect_movi_packets(buf: &[u8], movi: Range<usize>, out: &mut VecDeque<Packet>) {
    for (id, r) in riff_chunks(buf, movi.start + 4, movi.end) {
        if id == *b"LIST" && r.len() >= 4 && &buf[r.start..r.start + 4] == b"rec " {
            // Interleave group: its children are the real data chunks.
            for (cid, cr) in riff_chunks(buf, r.start + 4, r.end) {
                push_data_chunk(buf, cid, cr, out);
            }
        } else {
            push_data_chunk(buf, id, r, out);
        }
    }
}

/// Append a `##xx` data chunk as a packet; the first two ASCII digits of the
/// chunk id are the stream index. Non-data chunks (e.g. `ix##`, `JUNK`) skip.
fn push_data_chunk(buf: &[u8], id: [u8; 4], data: Range<usize>, out: &mut VecDeque<Packet>) {
    if !id[0].is_ascii_digit() || !id[1].is_ascii_digit() {
        return;
    }
    let stream_index = ((id[0] - b'0') * 10 + (id[1] - b'0')) as usize;
    out.push_back(Packet::from_data(stream_index, buf[data].to_vec()));
}

// ===========================================================================
// RIFF writing helpers
// ===========================================================================

fn put_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Append a chunk `[4cc id][u32 LE size][data]`, padded to an even boundary.
fn put_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

/// Append a `LIST` chunk wrapping `body` under `list_type`.
fn put_list(out: &mut Vec<u8>, list_type: &[u8; 4], body: &[u8]) {
    let total = 4 + body.len();
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(list_type);
    out.extend_from_slice(body);
    if total % 2 == 1 {
        out.push(0);
    }
}

/// Codec id → AVI fourcc for the `strf`/`strh` codec field (inverse of
/// [`map_video_fourcc`]). Unknown codecs get a zero fourcc.
fn codec_to_fourcc(id: CodecId) -> [u8; 4] {
    match id {
        CodecId::H264 => *b"H264",
        CodecId::Avif => *b"AV01",
        _ => [0; 4],
    }
}

/// `##xx` chunk id: two ASCII digits of the stream index plus a 2-byte type.
fn chunk_id(stream_index: usize, suffix: &[u8; 2]) -> [u8; 4] {
    [
        b'0' + (stream_index / 10) as u8,
        b'0' + (stream_index % 10) as u8,
        suffix[0],
        suffix[1],
    ]
}

// ===========================================================================
// Muxer
// ===========================================================================

/// Writes streams + packets as an AVI file. The sink only needs to be `Write`:
/// everything is buffered and assembled in [`write_trailer`], because the AVI
/// headers carry totals (frame counts, the index) only known once all packets
/// are in.
struct AviMuxer {
    out: Output,
    streams: Vec<Stream>,
    packets: Vec<Packet>,
}

impl AviMuxer {
    fn new(output: Output) -> AviMuxer {
        AviMuxer {
            out: output,
            streams: Vec::new(),
            packets: Vec::new(),
        }
    }

    /// The `##xx` type suffix for a stream's data chunks.
    fn suffix_for(&self, stream_index: usize) -> [u8; 2] {
        match self.streams.iter().find(|s| s.index == stream_index) {
            Some(s) if s.media_type == MediaType::Audio => *b"wb",
            _ => *b"dc", // video (compressed) / default
        }
    }

    fn packet_count(&self, stream_index: usize) -> u32 {
        self.packets
            .iter()
            .filter(|p| p.stream_index == stream_index)
            .count() as u32
    }
}

impl Muxer for AviMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("avi mux: no streams"));
        }
        self.streams = streams.to_vec();
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.packets.push(packet.clone());
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let primary_video = self.streams.iter().find(|s| s.media_type == MediaType::Video);
        let (vw, vh) = primary_video.map_or((0, 0), |s| (s.width, s.height));
        let micros_per_frame = primary_video.map_or(0, |s| {
            let tb = s.time_base;
            if tb.den != 0 && tb.num > 0 {
                (tb.num as i64 * 1_000_000 / tb.den as i64) as u32
            } else {
                0
            }
        });
        let total_frames = primary_video.map_or(0, |s| self.packet_count(s.index));

        // --- hdrl: avih + one strl (strh + strf) per stream ---
        let mut hdrl = Vec::new();
        put_chunk(&mut hdrl, b"avih", &self.avih(micros_per_frame, total_frames, vw, vh));
        for s in &self.streams {
            let length = self.packet_count(s.index);
            let mut strl = Vec::new();
            put_chunk(&mut strl, b"strh", &strh(s, length));
            put_chunk(&mut strl, b"strf", &strf(s));
            put_list(&mut hdrl, b"strl", &strl);
        }

        // --- movi: data chunks, recording index entries as we go ---
        let mut movi = Vec::new();
        let mut idx1 = Vec::new();
        for p in &self.packets {
            let id = chunk_id(p.stream_index, &self.suffix_for(p.stream_index));
            // idx1 offsets are relative to the `movi` 4cc; the first chunk
            // sits 4 bytes past it.
            let offset = 4 + movi.len();
            put_chunk(&mut movi, &id, &p.data);

            let flags: u32 = if p.is_keyframe() { 0x10 } else { 0 }; // AVIIF_KEYFRAME
            idx1.extend_from_slice(&id);
            idx1.extend_from_slice(&flags.to_le_bytes());
            idx1.extend_from_slice(&(offset as u32).to_le_bytes());
            idx1.extend_from_slice(&(p.data.len() as u32).to_le_bytes());
        }

        // --- assemble RIFF "AVI " ---
        let mut body = Vec::new();
        body.extend_from_slice(b"AVI ");
        put_list(&mut body, b"hdrl", &hdrl);
        put_list(&mut body, b"movi", &movi);
        put_chunk(&mut body, b"idx1", &idx1);

        let mut file = Vec::with_capacity(body.len() + 8);
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);

        self.out.write_all(&file)?;
        self.out.flush()?;
        Ok(())
    }
}

impl AviMuxer {
    /// MainAVIHeader (56 bytes).
    fn avih(&self, micros_per_frame: u32, total_frames: u32, width: u32, height: u32) -> Vec<u8> {
        let mut a = vec![0u8; 56];
        put_u32(&mut a, 0, micros_per_frame);
        put_u32(&mut a, 12, 0x10); // dwFlags = AVIF_HASINDEX
        put_u32(&mut a, 16, total_frames);
        put_u32(&mut a, 24, self.streams.len() as u32);
        put_u32(&mut a, 32, width);
        put_u32(&mut a, 36, height);
        a
    }
}

/// AVIStreamHeader (56 bytes) for one stream.
fn strh(s: &Stream, length: u32) -> Vec<u8> {
    let mut h = vec![0u8; 56];
    match s.media_type {
        MediaType::Audio => {
            h[0..4].copy_from_slice(b"auds");
            put_u32(&mut h, 20, 1); // dwScale
            put_u32(&mut h, 24, s.sample_rate.max(1)); // dwRate
        }
        _ => {
            h[0..4].copy_from_slice(b"vids");
            h[4..8].copy_from_slice(&codec_to_fourcc(s.codec_id)); // fccHandler
            put_u32(&mut h, 20, s.time_base.num.max(1) as u32); // dwScale
            put_u32(&mut h, 24, s.time_base.den.max(1) as u32); // dwRate
            put_u16(&mut h, 52, s.width as u16); // rcFrame.right
            put_u16(&mut h, 54, s.height as u16); // rcFrame.bottom
        }
    }
    put_u32(&mut h, 32, length); // dwLength (frames/samples)
    h
}

/// Stream format: BITMAPINFOHEADER (video) or WAVEFORMATEX (audio).
fn strf(s: &Stream) -> Vec<u8> {
    match s.media_type {
        MediaType::Audio => {
            let (tag, bits): (u16, u16) = match s.sample_format {
                Some(SampleFormat::F32) => (3, 32),
                _ => (1, 16), // default to s16 PCM
            };
            let block_align = s.channels * (bits / 8);
            let mut w = vec![0u8; 18]; // WAVEFORMATEX
            put_u16(&mut w, 0, tag); // wFormatTag (1=PCM, 3=float)
            put_u16(&mut w, 2, s.channels); // nChannels
            put_u32(&mut w, 4, s.sample_rate); // nSamplesPerSec
            put_u32(&mut w, 8, s.sample_rate * block_align as u32); // nAvgBytesPerSec
            put_u16(&mut w, 12, block_align); // nBlockAlign
            put_u16(&mut w, 14, bits); // wBitsPerSample
            w
        }
        _ => {
            let mut b = vec![0u8; 40]; // BITMAPINFOHEADER
            put_u32(&mut b, 0, 40); // biSize
            put_u32(&mut b, 4, s.width); // biWidth
            put_u32(&mut b, 8, s.height); // biHeight
            put_u16(&mut b, 12, 1); // biPlanes
            put_u16(&mut b, 14, 24); // biBitCount
            b[16..20].copy_from_slice(&codec_to_fourcc(s.codec_id)); // biCompression
            put_u32(&mut b, 20, s.width * s.height * 3); // biSizeImage
            b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    /// A `Write` sink whose bytes can be recovered after the muxer drops it.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut o = Vec::new();
        o.extend_from_slice(id);
        o.extend_from_slice(&(data.len() as u32).to_le_bytes());
        o.extend_from_slice(data);
        if data.len() % 2 == 1 {
            o.push(0); // pad to even
        }
        o
    }

    fn list(list_type: &[u8; 4], children: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(list_type);
        body.extend_from_slice(children);
        chunk(b"LIST", &body)
    }

    fn riff_avi(children: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"AVI ");
        body.extend_from_slice(children);
        let mut o = Vec::new();
        o.extend_from_slice(b"RIFF");
        o.extend_from_slice(&(body.len() as u32).to_le_bytes());
        o.extend_from_slice(&body);
        o
    }

    #[test]
    fn demuxes_minimal_avi() {
        let (w, h) = (320u32, 240u32);

        let mut avih = vec![0u8; 56];
        avih[24..28].copy_from_slice(&1u32.to_le_bytes()); // dwStreams
        avih[32..36].copy_from_slice(&w.to_le_bytes()); // dwWidth
        avih[36..40].copy_from_slice(&h.to_le_bytes()); // dwHeight

        let mut strh = vec![0u8; 56];
        strh[0..4].copy_from_slice(b"vids"); // fccType
        strh[4..8].copy_from_slice(b"H264"); // fccHandler
        strh[20..24].copy_from_slice(&1u32.to_le_bytes()); // dwScale
        strh[24..28].copy_from_slice(&30u32.to_le_bytes()); // dwRate

        let mut strf = vec![0u8; 40]; // BITMAPINFOHEADER
        strf[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
        strf[4..8].copy_from_slice(&w.to_le_bytes()); // biWidth
        strf[8..12].copy_from_slice(&h.to_le_bytes()); // biHeight
        strf[16..20].copy_from_slice(b"H264"); // biCompression

        let mut strl_children = Vec::new();
        strl_children.extend_from_slice(&chunk(b"strh", &strh));
        strl_children.extend_from_slice(&chunk(b"strf", &strf));
        let strl = list(b"strl", &strl_children);

        let mut hdrl_children = Vec::new();
        hdrl_children.extend_from_slice(&chunk(b"avih", &avih));
        hdrl_children.extend_from_slice(&strl);
        let hdrl = list(b"hdrl", &hdrl_children);

        let payload = b"H264-frame-bytes";
        let movi = list(b"movi", &chunk(b"00dc", payload));

        let mut top = Vec::new();
        top.extend_from_slice(&hdrl);
        top.extend_from_slice(&movi);
        let file = riff_avi(&top);

        let mut dem = AviDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].media_type, MediaType::Video);
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert_eq!((streams[0].width, streams[0].height), (w, h));

        let packet = dem.read_packet().unwrap();
        assert_eq!(packet.stream_index, 0);
        assert_eq!(packet.data, payload);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn mux_then_demux_roundtrips() {
        let (w, h) = (320u32, 240u32);
        let mut vstream = Stream::new(0, CodecId::H264);
        vstream.media_type = MediaType::Video;
        vstream.width = w;
        vstream.height = h;
        vstream.time_base = Rational::new(1, 30);

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = AviMuxer::new(Box::new(sink.clone()));
            mux.write_header(&[vstream]).unwrap();
            mux.write_packet(&Packet::from_data(0, b"frame-one".to_vec())).unwrap();
            let mut p2 = Packet::from_data(0, b"frame-two!".to_vec());
            p2.flags.keyframe = true;
            mux.write_packet(&p2).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();

        // Looks like a RIFF/AVI file...
        assert_eq!(&file[0..4], b"RIFF");
        assert_eq!(&file[8..12], b"AVI ");

        // ...and our own demuxer reads the streams and packets back.
        let mut dem = AviDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].media_type, MediaType::Video);
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert_eq!((streams[0].width, streams[0].height), (w, h));

        assert_eq!(dem.read_packet().unwrap().data, b"frame-one");
        assert_eq!(dem.read_packet().unwrap().data, b"frame-two!");
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn sniffs_avi_by_magic() {
        assert_eq!(probe_avi(&riff_avi(&[])), 100);
        assert_eq!(probe_avi(b"RIFF\0\0\0\0WAVE"), 0); // RIFF, but not AVI
        assert_eq!(probe_avi(b"not a riff file"), 0);
        assert_eq!(probe_avi(b"RIFF"), 0); // too short
    }
}
