//! JPEG single-image container, and JPEG Multi-Picture Format (MPF).
//!
//! A JPEG file *is* its codec bitstream, so the demuxer hands the whole file to
//! the [`mjpeg`](rff-codec-jpeg) codec as one packet and the muxer writes a
//! codec packet straight out. Dimensions for `ffprobe` come from a light scan
//! of the JPEG marker segments for the frame header (`SOFn`).
//!
//! A JPEG that is an **MPF** file (CIPA DC-007: an `APP2` `MPF\0` index
//! listing more JPEG images appended after the first one's `EOI` — Ultra HDR
//! gain-map photos, `.mpo` stereo pairs, camera previews) demuxes as one
//! stream per image, the way FFmpeg 9.1's `jpeg_mpf` demuxer splits it:
//! streams in index order, one packet each holding that image's bytes
//! exactly. Each stream's [`Demuxer::stream_metadata`] names its MP Type
//! (`mpf_type`: `primary`, `gain_map`, `disparity`, `large_thumbnail_vga`,
//! ...). Both the `jpeg` and the `jpeg_mpf` demuxers do this — `jpeg_mpf` is
//! what content sniffing picks when the index is in the first 4 KiB, `jpeg`
//! what it picks when a large Exif block pushes it further.
//!
//! The `jpeg_mpf` muxer (`-f jpeg_mpf`, or a `.mpo` name) writes N JPEG
//! streams as one MPF file; see [`mpf::write`]. The index parsing and writing
//! live in [`mpf`], which is `no_std`-friendly and independent of the rest of
//! rff.
#![forbid(unsafe_code)]

extern crate alloc;

pub mod mpf;

use std::io::{Read, Write};
use std::ops::Range;

use rff_core::{CodecId, Dictionary, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};

use mpf::{LayoutSource, MpType, TrailingPolicy};

/// Register the JPEG formats (`jpeg`, `jpeg_mpf`) into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "jpeg",
        long_name: "JPEG (JFIF) image",
        extensions: &["jpg", "jpeg"],
        demuxer: Some(|input| Box::new(JpegDemuxer::new(input))),
        muxer: Some(|output| Box::new(JpegMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_jpeg),
        mux_caps: MuxCaps::single(&[CodecId::Jpeg]).image(),
    });
    registry.register(Format {
        name: "jpeg_mpf",
        long_name: "JPEG Multi-Picture Format",
        extensions: &["mpo"],
        demuxer: Some(|input| Box::new(JpegDemuxer::new(input))),
        muxer: Some(|output| Box::new(MpfMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_mpf),
        mux_caps: MuxCaps {
            codecs: &[CodecId::Jpeg],
            max_streams: Some(mpf::MAX_IMAGES),
            still_image: false,
        },
    });
}

/// Sniff JPEG by its start-of-image marker (`FF D8 FF`).
fn probe_jpeg(data: &[u8]) -> i32 {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        100
    } else {
        0
    }
}

/// Sniff an MPF file: a JPEG whose header (within the probed bytes) has an
/// `APP2` segment opening `MPF\0`, or an ISO 21496-1 segment carrying only
/// the version fields (the base image of a gain-map file), as FFmpeg's
/// probe does. Scores above plain `jpeg`.
fn probe_mpf(data: &[u8]) -> i32 {
    if probe_jpeg(data) == 0 {
        return 0;
    }
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return 0;
        }
        let mut j = i + 1;
        while data.get(j) == Some(&0xFF) {
            j += 1;
        }
        let Some(&marker) = data.get(j) else {
            return 0;
        };
        if marker == 0xDA || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            return 0;
        }
        let Some(len) = data.get(j + 1..j + 3) else {
            return 0;
        };
        let len = u16::from_be_bytes([len[0], len[1]]) as usize;
        if len < 2 {
            return 0;
        }
        let body = data
            .get(j + 3..(j + 1 + len).min(data.len()))
            .unwrap_or(&[]);
        if marker == 0xE2
            && (body.starts_with(mpf::MPF_IDENTIFIER)
                || (body.starts_with(mpf::ISO21496_IDENTIFIER)
                    && len - 2 == mpf::ISO21496_IDENTIFIER.len() + 4))
        {
            return 101;
        }
        i = j + 1 + len;
    }
    0
}

/// Scan JPEG marker segments for a frame header (`SOF0..SOF15`, minus the
/// non-frame `C4`/`C8`/`CC`) and return `(width, height)`.
fn jpeg_dimensions(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 2 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 1 < buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = buf[i + 1];
        // Standalone markers (no length): padding, SOI/EOI, RSTn, TEM.
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        // SOFn frame header: FF, marker, len(2), precision(1), height(2), width(2).
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            if i + 9 > buf.len() {
                return None;
            }
            let h = u16::from_be_bytes([buf[i + 5], buf[i + 6]]) as u32;
            let w = u16::from_be_bytes([buf[i + 7], buf[i + 8]]) as u32;
            return Some((w, h));
        }
        // Any other marker carries a 2-byte length we skip over.
        if i + 4 > buf.len() {
            return None;
        }
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

/// The per-stream metadata keys the demuxer sets and the MPF muxer reads.
pub mod keys {
    /// MP Type, by [`MpType::name`](crate::mpf::MpType::name) (`primary`,
    /// `gain_map`, `disparity`, ...) or as a hex code (`0x020002`).
    pub const MPF_TYPE: &str = "mpf_type";
    /// The MP Type code, `0x` and six hex digits.
    pub const MPF_TYPE_CODE: &str = "mpf_type_code";
    /// The image's `MPEntry` number in the MP Index.
    pub const MPF_ENTRY: &str = "mpf_entry";
    /// Attribute flags, comma-separated: `representative`,
    /// `dependent_parent`, `dependent_child` (the muxer honours
    /// `representative`; it derives the dependency flags itself).
    pub const MPF_FLAGS: &str = "mpf_flags";
    /// `index` (read from the MP Index) or `trailing` (no usable index: split
    /// at `EOI`).
    pub const MPF_SOURCE: &str = "mpf_source";
}

struct JpegDemuxer {
    input: Option<Input>,
    /// The whole file; a single-image file's packet takes it by move.
    data: Vec<u8>,
    /// `(stream, bytes)` for each packet still to hand out, in file order.
    packets: Vec<(usize, Range<usize>)>,
    next: usize,
    metadata: Vec<Dictionary>,
    discard: Vec<bool>,
}

impl JpegDemuxer {
    fn new(input: Input) -> JpegDemuxer {
        JpegDemuxer {
            input: Some(input),
            data: Vec::new(),
            packets: Vec::new(),
            next: 0,
            metadata: Vec::new(),
            discard: Vec::new(),
        }
    }
}

fn flags_string(attribute: u32) -> String {
    let mut f = Vec::new();
    if attribute & mpf::ATTR_REPRESENTATIVE != 0 {
        f.push("representative");
    }
    if attribute & mpf::ATTR_DEPENDENT_PARENT != 0 {
        f.push("dependent_parent");
    }
    if attribute & mpf::ATTR_DEPENDENT_CHILD != 0 {
        f.push("dependent_child");
    }
    f.join(",")
}

impl Demuxer for JpegDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("jpeg demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;

        let not_jpeg = || Error::invalid("jpeg demux: not a JPEG file");
        // What `locate` tolerated (`layout.warnings`) is not fatal: a damaged
        // index degrades to fewer images, never to an unreadable file.
        let layout = mpf::locate(&buf, TrailingPolicy::Split).map_err(|_| not_jpeg())?;

        let mut streams = Vec::with_capacity(layout.images.len());
        for (index, img) in layout.images.iter().enumerate() {
            let bytes = &buf[img.range.clone()];
            let (width, height) = match jpeg_dimensions(bytes) {
                Some(d) => d,
                // The first image must be a JPEG; another one we cannot size
                // is still handed out (the decoder will judge it).
                None if index == layout.first => return Err(not_jpeg()),
                None => (0, 0),
            };
            let mut stream = Stream::new(index, CodecId::Jpeg);
            stream.width = width;
            stream.height = height;
            stream.time_base = Rational::new(1, 1);
            stream.nb_frames = Some(1);
            let mut md = Dictionary::new();
            if layout.source != LayoutSource::Single {
                // The first image is what a player shows (FFmpeg marks it
                // default); the others are alternatives or auxiliaries.
                stream.disposition.default = Some(index == layout.first);
                md.set(keys::MPF_TYPE, img.mp_type.to_string());
                md.set(keys::MPF_TYPE_CODE, format!("0x{:06x}", img.mp_type.code()));
                if let Some(e) = img.entry {
                    md.set(keys::MPF_ENTRY, e.to_string());
                }
                let flags = flags_string(img.attribute);
                if !flags.is_empty() {
                    md.set(keys::MPF_FLAGS, flags);
                }
                md.set(
                    keys::MPF_SOURCE,
                    if layout.source == LayoutSource::Index {
                        "index"
                    } else {
                        "trailing"
                    },
                );
            }
            self.metadata.push(md);
            streams.push(stream);
        }
        self.packets = layout
            .file_order()
            .into_iter()
            .map(|i| (i, layout.images[i].range.clone()))
            .collect();
        self.data = buf;
        Ok(streams)
    }

    fn read_packet(&mut self) -> Result<Packet> {
        while let Some((stream, range)) = self.packets.get(self.next).cloned() {
            self.next += 1;
            if self.discard.get(stream).copied().unwrap_or(false) {
                continue;
            }
            let data = if self.packets.len() == 1 && range == (0..self.data.len()) {
                std::mem::take(&mut self.data)
            } else {
                self.data[range].to_vec()
            };
            let mut packet = Packet::from_data(stream, data);
            packet.flags.keyframe = true;
            packet.pts = Some(0);
            return Ok(packet);
        }
        Err(Error::Eof)
    }

    fn set_discard(&mut self, discard: &[bool]) {
        self.discard = discard.to_vec();
    }

    fn stream_metadata(&self, index: usize) -> Option<&Dictionary> {
        self.metadata.get(index).filter(|d| !d.is_empty())
    }
}

struct JpegMuxer {
    out: Output,
}

impl JpegMuxer {
    fn new(out: Output) -> JpegMuxer {
        JpegMuxer { out }
    }
}

impl Muxer for JpegMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams {
            [s] if s.codec_id == CodecId::Jpeg => Ok(()),
            [_] => Err(Error::unsupported(
                "jpeg mux: only the `mjpeg` codec is supported",
            )),
            [] => Err(Error::invalid("jpeg mux: no streams")),
            _ => Err(Error::unsupported(
                "jpeg mux: a JPEG file holds one image; select one stream (-map 0:N), \
                 or write several as -f jpeg_mpf",
            )),
        }
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// The `jpeg_mpf` muxer: one JPEG per stream, written as one MPF file at the
/// trailer (the index needs every image's size).
struct MpfMuxer {
    out: Output,
    images: Vec<Option<Vec<u8>>>,
    types: Vec<Option<MpType>>,
    representative: Vec<bool>,
    /// An `mpf_type` that names no MP Type, refused at the header.
    bad_type: Option<(usize, String)>,
}

impl MpfMuxer {
    fn new(out: Output) -> MpfMuxer {
        MpfMuxer {
            out,
            images: Vec::new(),
            types: Vec::new(),
            representative: Vec::new(),
            bad_type: None,
        }
    }

    fn slot<T: Default + Clone>(v: &mut Vec<T>, index: usize) -> &mut T {
        if v.len() <= index {
            v.resize(index + 1, T::default());
        }
        &mut v[index]
    }
}

impl Muxer for MpfMuxer {
    fn set_stream_metadata(&mut self, index: usize, metadata: &Dictionary) {
        if index >= mpf::MAX_IMAGES {
            return;
        }
        if let Some(t) = metadata.get(keys::MPF_TYPE) {
            match MpType::from_name(t) {
                Some(t) => *Self::slot(&mut self.types, index) = Some(t),
                None => self.bad_type = Some((index, t.to_string())),
            }
        }
        if let Some(f) = metadata.get(keys::MPF_FLAGS) {
            *Self::slot(&mut self.representative, index) =
                f.split(',').any(|f| f.trim() == "representative");
        }
    }

    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("jpeg_mpf mux: no streams"));
        }
        if let Some((i, t)) = &self.bad_type {
            return Err(Error::invalid(format!(
                "jpeg_mpf mux: stream {i}: unknown mpf_type `{t}`                  (a name such as primary, gain_map, disparity, or a code such as 0x020002)"
            )));
        }
        if streams.len() > mpf::MAX_IMAGES {
            return Err(Error::unsupported(format!(
                "jpeg_mpf mux: at most {} images",
                mpf::MAX_IMAGES
            )));
        }
        if let Some(s) = streams.iter().find(|s| s.codec_id != CodecId::Jpeg) {
            return Err(Error::unsupported(format!(
                "jpeg_mpf mux: stream {} is `{}`; every image must be JPEG (pass -c:v mjpeg)",
                s.index,
                s.codec_id.name()
            )));
        }
        self.images = vec![None; streams.len()];
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let slot = self
            .images
            .get_mut(packet.stream_index)
            .ok_or_else(|| Error::invalid("jpeg_mpf mux: packet for an undeclared stream"))?;
        if slot.is_some() {
            return Err(Error::invalid(format!(
                "jpeg_mpf mux: stream {}: expected a single JPEG image",
                packet.stream_index
            )));
        }
        *slot = Some(packet.data.clone());
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let mut images = Vec::with_capacity(self.images.len());
        for (i, img) in self.images.iter().enumerate() {
            let data = img.as_deref().ok_or_else(|| {
                Error::invalid(format!("jpeg_mpf mux: no image was supplied on stream {i}"))
            })?;
            images.push(mpf::WriteImage {
                data,
                mp_type: self.types.get(i).copied().flatten(),
                representative: self.representative.get(i).copied().unwrap_or(false),
            });
        }
        let file = mpf::write(&images).map_err(|e| Error::invalid(format!("jpeg_mpf mux: {e}")))?;
        self.out.write_all(&file)?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// A minimal baseline-JPEG header: SOI + a SOF0 declaring 5×7.
    fn jpeg_header_5x7() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
                                      // SOF0: marker, len=17, precision=8, height=7, width=5, 3 components...
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&7u16.to_be_bytes());
        v.extend_from_slice(&5u16.to_be_bytes());
        v.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 0, 3, 0x11, 0]); // components
        v
    }

    /// A structurally complete JPEG of `w`×`h` (header only; no pixels).
    fn jpeg(w: u16, h: u16, tag: u8) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[1, 1, 0x11, 0]);
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 63, 0]);
        v.extend_from_slice(&[tag, 0xFF, 0x00, tag, 0xFF, 0xD9]);
        v
    }

    #[test]
    fn sniffs_and_reads_jpeg_dimensions() {
        let jpg = jpeg_header_5x7();
        assert_eq!(probe_jpeg(&jpg), 100);
        assert_eq!(probe_jpeg(b"\xFF\xD8\xEEnope"), 0);

        let mut dem = JpegDemuxer::new(Box::new(Cursor::new(jpg.clone())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Jpeg);
        assert_eq!((streams[0].width, streams[0].height), (5, 7));
        assert!(
            dem.stream_metadata(0).is_none(),
            "a plain JPEG has no MPF tags"
        );
        assert_eq!(dem.read_packet().unwrap().data, jpg);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn demux(file: &[u8]) -> (Vec<Stream>, Vec<Dictionary>, Vec<Packet>) {
        let mut dem = JpegDemuxer::new(Box::new(Cursor::new(file.to_vec())));
        let streams = dem.read_header().unwrap();
        let md = (0..streams.len())
            .map(|i| dem.stream_metadata(i).cloned().unwrap_or_default())
            .collect();
        let mut packets = Vec::new();
        while let Ok(p) = dem.read_packet() {
            packets.push(p);
        }
        (streams, md, packets)
    }

    #[test]
    fn mux_then_demux_round_trips_every_image() {
        let imgs = [jpeg(64, 48, 1), jpeg(32, 24, 2), jpeg(16, 12, 3)];
        let sink = Sink::default();
        let mut mux = MpfMuxer::new(Box::new(sink.clone()));
        let streams: Vec<Stream> = (0..3).map(|i| Stream::new(i, CodecId::Jpeg)).collect();
        let mut thumb = Dictionary::new();
        thumb.set(keys::MPF_TYPE, "large_thumbnail_vga");
        mux.set_stream_metadata(1, &thumb);
        mux.write_header(&streams).unwrap();
        // Out of order on purpose: the trailer lays them out by stream.
        for i in [2, 0, 1] {
            mux.write_packet(&Packet::from_data(i, imgs[i].clone()))
                .unwrap();
        }
        mux.write_trailer().unwrap();
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(probe_mpf(&file), 101);

        let (streams, md, packets) = demux(&file);
        assert_eq!(streams.len(), 3);
        assert_eq!(
            streams
                .iter()
                .map(|s| (s.width, s.height))
                .collect::<Vec<_>>(),
            [(64, 48), (32, 24), (16, 12)]
        );
        assert_eq!(streams[0].disposition.default, Some(true));
        assert_eq!(streams[1].disposition.default, Some(false));
        assert_eq!(md[0].get(keys::MPF_TYPE), Some("primary"));
        assert_eq!(md[0].get(keys::MPF_FLAGS), Some("dependent_parent"));
        assert_eq!(md[1].get(keys::MPF_TYPE), Some("large_thumbnail_vga"));
        assert_eq!(md[1].get(keys::MPF_TYPE_CODE), Some("0x010001"));
        assert_eq!(md[1].get(keys::MPF_FLAGS), Some("dependent_child"));
        assert_eq!(md[2].get(keys::MPF_TYPE), Some("undefined"));
        assert_eq!(md[2].get(keys::MPF_ENTRY), Some("2"));
        assert_eq!(md[2].get(keys::MPF_SOURCE), Some("index"));
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[1].data, imgs[1]);
        assert_eq!(packets[2].data, imgs[2]);
        assert_eq!(
            packets.iter().map(|p| p.data.len()).sum::<usize>(),
            file.len()
        );
    }

    #[test]
    fn discarded_streams_are_skipped() {
        let mut file = jpeg(8, 8, 1);
        file.extend_from_slice(&jpeg(4, 4, 2));
        let mut dem = JpegDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 2, "trailing-JPEG policy");
        dem.set_discard(&[true, false]);
        let p = dem.read_packet().unwrap();
        assert_eq!((p.stream_index, p.data), (1, jpeg(4, 4, 2)));
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn muxers_refuse_what_they_cannot_write() {
        let mut jpeg_mux = JpegMuxer::new(Box::new(Sink::default()));
        let two = [Stream::new(0, CodecId::Jpeg), Stream::new(1, CodecId::Jpeg)];
        assert!(jpeg_mux.write_header(&two).is_err());

        let mut mux = MpfMuxer::new(Box::new(Sink::default()));
        assert!(mux.write_header(&[Stream::new(0, CodecId::Png)]).is_err());
        let mut mux = MpfMuxer::new(Box::new(Sink::default()));
        mux.write_header(&two).unwrap();
        mux.write_packet(&Packet::from_data(0, jpeg(8, 8, 1)))
            .unwrap();
        assert!(mux
            .write_packet(&Packet::from_data(0, jpeg(8, 8, 1)))
            .is_err());
        assert!(mux.write_trailer().is_err(), "stream 1 never got an image");
    }

    #[test]
    fn probe_mpf_needs_the_index_segment() {
        let plain = jpeg(8, 8, 1);
        assert_eq!(probe_mpf(&plain), 0);
        assert_eq!(probe_mpf(b"\xFF\xD8\xFF\xE2\x00"), 0);
        for cut in 0..plain.len() {
            let _ = probe_mpf(&plain[..cut]);
        }
    }
}
