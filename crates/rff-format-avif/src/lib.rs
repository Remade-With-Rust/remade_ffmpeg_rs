//! AVIF (AV1 Image File Format) container.
//!
//! AVIF is a HEIF/ISOBMFF file: a tree of boxes (`ftyp`, `meta`, `mdat`, ...)
//! whose primary item is a single AV1 *intra* frame. This crate is the
//! container only — it carries the AV1 bitstream the `avif` **codec** produces;
//! it performs no pixel coding itself.
//!
//! * **Mux** — wrap one AV1 still picture: write `ftyp` + `meta` (item info,
//!   location, and an `av1C` configuration property) + `mdat`.
//! * **Demux** — walk the boxes, read the image size (`ispe`) and the sample
//!   location (`iloc`), and hand the AV1 payload to the decoder as one packet.
//!
//! ## Scope (v1)
//! The whole AV1 temporal unit (sequence header + frame) is kept in `mdat`, so
//! the sample is self-contained and decodes without consulting `av1C`. Reading
//! *foreign* AVIFs that store the sequence header only in `av1C` is a later
//! addition. 8-bit only, matching the codec.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the AVIF format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "avif",
        long_name: "AVIF (AV1 Image File Format)",
        extensions: &["avif"],
        demuxer: Some(|input| Box::new(AvifDemuxer::new(input))),
        muxer: Some(|output| Box::new(AvifMuxer::new(output))),
    });
}

// ===========================================================================
// ISOBMFF box helpers (writing)
// ===========================================================================

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Wrap `body` in a box of type `typ`: `[u32 size][4cc type][body]`.
fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    push_u32(&mut out, (8 + body.len()) as u32);
    out.extend_from_slice(typ);
    out.extend_from_slice(body);
    out
}

/// Wrap `body` in a FullBox: like [`bx`] but with a leading `version`+`flags`.
/// The body therefore begins 12 bytes into the returned box (8 header + 4).
fn full_bx(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + body.len());
    b.push(version);
    b.extend_from_slice(&flags.to_be_bytes()[1..]); // low 3 bytes
    b.extend_from_slice(body);
    bx(typ, &b)
}

// ===========================================================================
// Muxer
// ===========================================================================

/// Writes a single AV1 still image as an AVIF file. The byte sink only needs to
/// be `Write` — the whole file is assembled in memory in [`write_trailer`].
struct AvifMuxer {
    out: Output,
    width: u32,
    height: u32,
    /// The AV1 bitstream, accumulated across `write_packet` calls.
    payload: Vec<u8>,
}

impl AvifMuxer {
    fn new(out: Output) -> AvifMuxer {
        AvifMuxer {
            out,
            width: 0,
            height: 0,
            payload: Vec::new(),
        }
    }
}

impl Muxer for AvifMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let stream = streams
            .iter()
            .find(|s| s.media_type == MediaType::Video)
            .ok_or_else(|| Error::invalid("avif mux: no video stream"))?;
        if stream.codec_id != CodecId::Avif {
            return Err(Error::unsupported(format!(
                "avif mux: only the `avif` codec is supported, got `{}`",
                stream.codec_id
            )));
        }
        if stream.width == 0 || stream.height == 0 {
            return Err(Error::invalid("avif mux: stream is missing image dimensions"));
        }
        self.width = stream.width;
        self.height = stream.height;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.payload.extend_from_slice(&packet.data);
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.payload.is_empty() {
            return Err(Error::invalid("avif mux: no image data was written"));
        }

        // Build the `av1C` property: fixed fields from the sequence header, plus
        // the sequence-header OBU itself as configOBUs (best effort — falls back
        // to defaults / no configOBUs if the OBU can't be located).
        let (fields, config_obus) = match find_seq_header_obu(&self.payload) {
            Some(obu) => (parse_seq_header_fields(obu.payload), obu.full.to_vec()),
            None => (Av1cFields::default(), Vec::new()),
        };
        let av1c = build_av1c(&fields, &config_obus);

        // --- iprp (item properties): ispe, av1C, pixi, then their association ---
        let mut ipco = Vec::new();
        ipco.extend_from_slice(&ispe(self.width, self.height)); // property #1
        ipco.extend_from_slice(&av1c); // property #2
        ipco.extend_from_slice(&pixi()); // property #3
        let ipco = bx(b"ipco", &ipco);

        let mut iprp = Vec::new();
        iprp.extend_from_slice(&ipco);
        iprp.extend_from_slice(&ipma());
        let iprp = bx(b"iprp", &iprp);

        // --- iloc, with a placeholder extent offset patched in below ---
        let (iloc, off_field_in_iloc) = build_iloc(self.payload.len() as u32);

        // --- meta: hdlr, pitm, iloc, iinf, iprp ---
        let mut meta_body = Vec::new();
        meta_body.extend_from_slice(&hdlr());
        meta_body.extend_from_slice(&pitm());
        let iloc_start = meta_body.len();
        meta_body.extend_from_slice(&iloc);
        meta_body.extend_from_slice(&iinf());
        meta_body.extend_from_slice(&iprp);
        let mut meta = full_bx(b"meta", 0, 0, &meta_body);

        // Patch the iloc extent offset. The meta FullBox body begins 12 bytes in
        // (8 box header + 4 version/flags); the iloc box sits at `iloc_start`
        // within that body, and `off_field_in_iloc` locates the offset field
        // within the iloc box.
        let ftyp = ftyp();
        let off_field_in_file_minus_mdat = 12 + iloc_start + off_field_in_iloc;
        let mdat_data_offset = ftyp.len() + meta.len() + 8; // +8 for mdat header
        meta[off_field_in_file_minus_mdat..off_field_in_file_minus_mdat + 4]
            .copy_from_slice(&(mdat_data_offset as u32).to_be_bytes());

        // --- assemble & write ---
        self.out.write_all(&ftyp)?;
        self.out.write_all(&meta)?;
        self.out.write_all(&bx(b"mdat", &self.payload))?;
        self.out.flush()?;
        Ok(())
    }
}

fn ftyp() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"avif"); // major_brand
    push_u32(&mut b, 0); // minor_version
    for brand in [b"avif", b"mif1", b"miaf", b"MA1B"] {
        b.extend_from_slice(brand);
    }
    bx(b"ftyp", &b)
}

fn hdlr() -> Vec<u8> {
    let mut b = Vec::new();
    push_u32(&mut b, 0); // pre_defined
    b.extend_from_slice(b"pict"); // handler_type
    push_u32(&mut b, 0);
    push_u32(&mut b, 0);
    push_u32(&mut b, 0); // reserved[3]
    b.push(0); // name = ""
    full_bx(b"hdlr", 0, 0, &b)
}

fn pitm() -> Vec<u8> {
    let mut b = Vec::new();
    push_u16(&mut b, 1); // primary item_ID
    full_bx(b"pitm", 0, 0, &b)
}

fn iinf() -> Vec<u8> {
    let mut infe = Vec::new();
    push_u16(&mut infe, 1); // item_ID
    push_u16(&mut infe, 0); // item_protection_index
    infe.extend_from_slice(b"av01"); // item_type
    infe.push(0); // item_name = ""
    let infe = full_bx(b"infe", 2, 0, &infe);

    let mut b = Vec::new();
    push_u16(&mut b, 1); // entry_count
    b.extend_from_slice(&infe);
    full_bx(b"iinf", 0, 0, &b)
}

fn ispe(width: u32, height: u32) -> Vec<u8> {
    let mut b = Vec::new();
    push_u32(&mut b, width);
    push_u32(&mut b, height);
    full_bx(b"ispe", 0, 0, &b)
}

fn pixi() -> Vec<u8> {
    let mut b = Vec::new();
    b.push(3); // num_channels
    b.push(8);
    b.push(8);
    b.push(8); // bits per channel
    full_bx(b"pixi", 0, 0, &b)
}

fn ipma() -> Vec<u8> {
    let mut b = Vec::new();
    push_u32(&mut b, 1); // entry_count
    push_u16(&mut b, 1); // item_ID (version 0 → u16)
    b.push(3); // association_count
    // essential(1) << 7 | property_index(7); property indices are 1-based into ipco.
    b.push(1); // ispe (#1), not essential
    b.push(0x80 | 2); // av1C (#2), essential
    b.push(3); // pixi (#3), not essential
    full_bx(b"ipma", 0, 0, &b)
}

/// Build an `iloc` box for a single item/extent, returning the box bytes and
/// the index (within those bytes) of the 4-byte extent-offset field to patch.
fn build_iloc(length: u32) -> (Vec<u8>, usize) {
    let mut body = Vec::new();
    body.push((4 << 4) | 4); // offset_size=4, length_size=4
    body.push(0); // base_offset_size=0, reserved=0
    push_u16(&mut body, 1); // item_count
    push_u16(&mut body, 1); // item_ID
    push_u16(&mut body, 0); // data_reference_index
    // base_offset: 0 bytes
    push_u16(&mut body, 1); // extent_count
    let off_field = body.len();
    push_u32(&mut body, 0); // extent_offset (patched later)
    push_u32(&mut body, length); // extent_length
    let box_bytes = full_bx(b"iloc", 0, 0, &body);
    // FullBox body begins 12 bytes into the box (8 header + 4 version/flags).
    (box_bytes, 12 + off_field)
}

// ===========================================================================
// av1C (AV1 Codec Configuration) + AV1 OBU parsing
// ===========================================================================

/// The fixed fields of an AV1CodecConfigurationRecord we care about.
struct Av1cFields {
    seq_profile: u8,
    seq_level_idx_0: u8,
    seq_tier_0: u8,
    high_bitdepth: u8,
    twelve_bit: u8,
    monochrome: u8,
    subsampling_x: u8,
    subsampling_y: u8,
    chroma_sample_position: u8,
}

impl Default for Av1cFields {
    fn default() -> Self {
        // Defaults describe 8-bit 4:2:0 (profile 0), the common AVIF case.
        Av1cFields {
            seq_profile: 0,
            seq_level_idx_0: 0,
            seq_tier_0: 0,
            high_bitdepth: 0,
            twelve_bit: 0,
            monochrome: 0,
            subsampling_x: 1,
            subsampling_y: 1,
            chroma_sample_position: 0,
        }
    }
}

fn build_av1c(f: &Av1cFields, config_obus: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(0x81); // marker=1, version=1
    b.push((f.seq_profile << 5) | (f.seq_level_idx_0 & 0x1f));
    b.push(
        (f.seq_tier_0 << 7)
            | (f.high_bitdepth << 6)
            | (f.twelve_bit << 5)
            | (f.monochrome << 4)
            | (f.subsampling_x << 3)
            | (f.subsampling_y << 2)
            | (f.chroma_sample_position & 0x3),
    );
    b.push(0x00); // reserved; initial_presentation_delay_present = 0
    b.extend_from_slice(config_obus);
    bx(b"av1C", &b)
}

/// A located OBU: its full bytes (header + size + payload) and just its payload.
struct Obu<'a> {
    full: &'a [u8],
    payload: &'a [u8],
}

/// Read a LEB128 value, returning it and the number of bytes consumed.
fn read_leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for i in 0..8 {
        let byte = *data.get(i)?;
        value |= ((byte & 0x7f) as u64) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Scan an AV1 temporal unit for the OBU_SEQUENCE_HEADER (type 1).
fn find_seq_header_obu(data: &[u8]) -> Option<Obu<'_>> {
    let mut i = 0;
    while i < data.len() {
        let start = i;
        let header = data[i];
        i += 1;
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = (header >> 2) & 1 == 1;
        let has_size = (header >> 1) & 1 == 1;
        if has_extension {
            if i >= data.len() {
                return None;
            }
            i += 1; // extension header byte
        }
        let payload_len = if has_size {
            let (len, used) = read_leb128(&data[i..])?;
            i += used;
            len as usize
        } else {
            data.len() - i
        };
        if i + payload_len > data.len() {
            return None;
        }
        let payload = &data[i..i + payload_len];
        i += payload_len;
        if obu_type == 1 {
            return Some(Obu {
                full: &data[start..i],
                payload,
            });
        }
    }
    None
}

/// Minimal MSB-first bit reader over an OBU payload.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, bit: 0 }
    }

    fn read(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.bit / 8;
            let shift = 7 - (self.bit % 8);
            let bit = self.data.get(byte).map_or(0, |b| (b >> shift) & 1);
            v = (v << 1) | bit as u32;
            self.bit += 1;
        }
        v
    }
}

/// Parse the leading fields of a sequence header for av1C. We read the early,
/// unambiguous fields (profile, and level when the still-picture header is in
/// reduced form) and derive chroma subsampling from the profile; bit depth is
/// fixed at 8 to match the codec. The real sequence header is also embedded in
/// configOBUs, so strict decoders can recover the exact parameters.
fn parse_seq_header_fields(payload: &[u8]) -> Av1cFields {
    let mut r = BitReader::new(payload);
    let seq_profile = r.read(3) as u8;
    let _still_picture = r.read(1);
    let reduced_still_picture_header = r.read(1);

    let mut f = Av1cFields {
        seq_profile,
        ..Av1cFields::default()
    };
    if reduced_still_picture_header == 1 {
        f.seq_level_idx_0 = r.read(5) as u8;
    }
    // Subsampling follows from the profile for our 8-bit content:
    //   0 → 4:2:0, 1 → 4:4:4, 2 → 4:2:2.
    match seq_profile {
        1 => {
            f.subsampling_x = 0;
            f.subsampling_y = 0;
        }
        2 => {
            f.subsampling_x = 1;
            f.subsampling_y = 0;
        }
        _ => {
            f.subsampling_x = 1;
            f.subsampling_y = 1;
        }
    }
    f
}

// ===========================================================================
// Demuxer
// ===========================================================================

/// Reads an AVIF file: locates the primary AV1 item and yields it as one
/// keyframe packet.
struct AvifDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl AvifDemuxer {
    fn new(input: Input) -> AvifDemuxer {
        AvifDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for AvifDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("avif demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;

        let meta = child_boxes(&buf)
            .into_iter()
            .find(|(t, _)| t == b"meta")
            .map(|(_, p)| p)
            .ok_or_else(|| Error::invalid("avif demux: no `meta` box"))?;
        // meta is a FullBox: skip its 4-byte version/flags to reach children.
        let meta_children = child_boxes(meta.get(4..).unwrap_or(&[]));

        let (width, height) = read_ispe(&meta_children)?;
        let (offset, length) = read_iloc(&meta_children)?;

        let end = offset
            .checked_add(length)
            .filter(|&e| e <= buf.len())
            .ok_or_else(|| Error::invalid("avif demux: item extent out of range"))?;
        self.sample = Some(buf[offset..end].to_vec());

        let mut stream = Stream::new(0, CodecId::Avif);
        stream.width = width;
        stream.height = height;
        stream.time_base = Rational::new(1, 1);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        match self.sample.take() {
            Some(data) => {
                let mut packet = Packet::from_data(0, data);
                packet.flags.keyframe = true;
                packet.pts = Some(0);
                Ok(packet)
            }
            None => Err(Error::Eof),
        }
    }
}

/// Parse sibling boxes at the top level of `buf` into `(type, payload)` pairs.
/// Tolerates the 64-bit `largesize` form and the `size == 0` (to-end) form.
fn child_boxes(buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= buf.len() {
        let size32 = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let typ = [buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]];
        let (payload_start, box_end) = if size32 == 1 {
            if i + 16 > buf.len() {
                break;
            }
            let large = u64::from_be_bytes(buf[i + 8..i + 16].try_into().unwrap()) as usize;
            (i + 16, i.checked_add(large).unwrap_or(buf.len()))
        } else if size32 == 0 {
            (i + 8, buf.len())
        } else {
            (i + 8, i + size32)
        };
        if box_end > buf.len() || box_end <= i {
            break;
        }
        out.push((typ, &buf[payload_start..box_end]));
        i = box_end;
    }
    out
}

/// Read image dimensions from the `ispe` property under `iprp`/`ipco`.
fn read_ispe(meta_children: &[([u8; 4], &[u8])]) -> Result<(u32, u32)> {
    let iprp = find(meta_children, b"iprp")
        .ok_or_else(|| Error::invalid("avif demux: no `iprp` box"))?;
    let ipco_children = find(&child_boxes(iprp), b"ipco")
        .map(child_boxes)
        .ok_or_else(|| Error::invalid("avif demux: no `ipco` box"))?;
    let ispe = find(&ipco_children, b"ispe")
        .ok_or_else(|| Error::invalid("avif demux: no `ispe` property"))?;
    // FullBox: 4 bytes version/flags, then width:u32, height:u32.
    if ispe.len() < 12 {
        return Err(Error::invalid("avif demux: truncated `ispe`"));
    }
    let width = u32::from_be_bytes(ispe[4..8].try_into().unwrap());
    let height = u32::from_be_bytes(ispe[8..12].try_into().unwrap());
    Ok((width, height))
}

/// Read the first item's `(offset, length)` from the `iloc` box.
fn read_iloc(meta_children: &[([u8; 4], &[u8])]) -> Result<(usize, usize)> {
    let p = find(meta_children, b"iloc")
        .ok_or_else(|| Error::invalid("avif demux: no `iloc` box"))?;
    // FullBox: byte 0 = version, 1..4 = flags.
    let version = *p.first().ok_or_else(|| Error::invalid("avif demux: empty `iloc`"))?;
    let mut i = 4;
    let read = |p: &[u8], at: usize, n: usize| -> Option<u64> {
        let mut v = 0u64;
        for k in 0..n {
            v = (v << 8) | *p.get(at + k)? as u64;
        }
        Some(v)
    };
    let sizes = *p.get(i).ok_or_else(|| Error::invalid("avif demux: short `iloc`"))?;
    i += 1;
    let offset_size = (sizes >> 4) as usize;
    let length_size = (sizes & 0x0f) as usize;
    let base_offset_size = (*p.get(i).unwrap_or(&0) >> 4) as usize;
    i += 1;
    // item_count: u16 for versions 0/1.
    let _item_count = read(p, i, if version < 2 { 2 } else { 4 });
    i += if version < 2 { 2 } else { 4 };
    // First item only.
    i += if version < 2 { 2 } else { 4 }; // item_ID
    if version == 1 || version == 2 {
        i += 2; // construction_method
    }
    i += 2; // data_reference_index
    i += base_offset_size; // base_offset
    let _extent_count = read(p, i, 2);
    i += 2;
    let offset = read(p, i, offset_size)
        .ok_or_else(|| Error::invalid("avif demux: truncated extent offset"))?;
    i += offset_size;
    let length = read(p, i, length_size)
        .ok_or_else(|| Error::invalid("avif demux: truncated extent length"))?;
    Ok((offset as usize, length as usize))
}

fn find<'a>(boxes: &[([u8; 4], &'a [u8])], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(t, _)| t == typ).map(|(_, p)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
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

    /// Mux an arbitrary payload, demux it back, and confirm the box layer
    /// preserves dimensions and the sample bytes. (No codec involved — this
    /// tests the container in isolation.)
    #[test]
    fn container_roundtrip_preserves_payload() {
        let payload = b"not-real-av1-but-bytes-survive".to_vec();

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = AvifMuxer::new(Box::new(sink.clone()));
            let mut stream = Stream::new(0, CodecId::Avif);
            stream.width = 96;
            stream.height = 64;
            mux.write_header(&[stream]).unwrap();
            mux.write_packet(&Packet::from_data(0, payload.clone())).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();

        // It should at least look like an ISOBMFF/AVIF file.
        assert_eq!(&file[4..8], b"ftyp");
        assert_eq!(&file[8..12], b"avif");

        let mut dem = AvifDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].width, 96);
        assert_eq!(streams[0].height, 64);
        assert_eq!(streams[0].codec_id, CodecId::Avif);

        let packet = dem.read_packet().unwrap();
        assert_eq!(packet.data, payload);
        assert!(packet.is_keyframe());
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
