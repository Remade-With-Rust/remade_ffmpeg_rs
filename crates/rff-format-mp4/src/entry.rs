//! Sample-entry detail beyond the codec id: what `stsd` says about colour,
//! aspect, HDR, PCM layout and timecode, read on demux and written on mux.
//!
//! The box layouts follow ISO/IEC 14496-12 (visual / audio sample entries,
//! `colr`, `pasp`, `clap`), ISO/IEC 23001-8 and 14496-12 Amd (`mdcv`, `clli`),
//! the QuickTime File Format (sound description v0/v1/v2, `enda`, `nclc`,
//! `tmcd`), ISO/IEC 23003-5 (`ipcm`/`fpcm` + `pcmC`), the VP codec ISO media
//! binding (`vpcC`), ETSI TS 102 366 Annex F (`dac3`, `dec3`) and the FLAC /
//! ALAC / APV ISO media mappings.

use rff_core::{
    CleanAperture, CodecId, ColorInfo, ColorRange, ContentLightLevel, MasteringDisplay,
    SampleFormat, Timecode,
};
use rff_format::Stream;

use super::{be16, be32, be64, bx, child_boxes, fbx, pu16, pu32};

// ---------------------------------------------------------------------------
// Codec mapping
// ---------------------------------------------------------------------------

/// The ProRes sample-entry fourccs, Proxy to 4444 XQ.
pub(crate) const PRORES_TAGS: [[u8; 4]; 6] =
    [*b"apco", *b"apcs", *b"apcn", *b"apch", *b"ap4h", *b"ap4x"];
/// DNxHD (`AVdn`) and DNxHR (`AVdh`).
pub(crate) const DNX_TAGS: [[u8; 4]; 2] = [*b"AVdn", *b"AVdh"];
/// QuickTime / ISO uncompressed audio entries.
pub(crate) const PCM_TAGS: [[u8; 4]; 11] = [
    *b"sowt", *b"twos", *b"lpcm", *b"in24", *b"in32", *b"fl32", *b"fl64", *b"raw ", *b"NONE",
    *b"ipcm", *b"fpcm",
];

/// A sample-entry fourcc's codec, for every entry the demuxer understands.
pub(crate) fn map_codec(fourcc: &[u8; 4]) -> CodecId {
    match fourcc {
        b"avc1" | b"avc3" => CodecId::H264,
        b"hvc1" | b"hev1" => CodecId::Hevc,
        b"av01" => CodecId::Avif, // AV1 video, decoded by our rav1d-backed codec
        b"vp09" => CodecId::Vp9,
        b"vp08" => CodecId::Vp8,
        b"apv1" => CodecId::Apv,
        b"jpeg" | b"mjpa" | b"mjpb" => CodecId::Jpeg,
        b"Opus" => CodecId::Opus,
        b"mp4a" => CodecId::Aac,
        b".mp3" => CodecId::Mp3,
        b"fLaC" => CodecId::Flac,
        b"alac" => CodecId::Alac,
        b"ac-3" => CodecId::Ac3,
        b"ec-3" => CodecId::Eac3,
        f if PRORES_TAGS.contains(f) => CodecId::ProRes,
        f if DNX_TAGS.contains(f) => CodecId::DnxHd,
        f if PCM_TAGS.contains(f) => CodecId::Pcm,
        _ => CodecId::None,
    }
}

// ---------------------------------------------------------------------------
// Visual sample entries
// ---------------------------------------------------------------------------

/// Offset of a VisualSampleEntry's child boxes within the entry payload.
pub(crate) const VISUAL_CHILDREN: usize = 78;

/// Read the descriptive boxes a visual sample entry may carry into `stream`:
/// `colr`, `pasp`, `clap`, `mdcv`, `clli`, plus the colour that `vpcC` and
/// `apvC` repeat. A `colr` wins over a `vpcC`'s colour fields whichever
/// comes first, as in FFmpeg. An `apvC` record becomes the stream's extradata.
pub(crate) fn read_visual_children(stream: &mut Stream, children: &[u8]) {
    let mut colr_seen = false;
    for (typ, p) in child_boxes(children) {
        match &typ {
            b"colr" if p.len() >= 10 => match &p[..4] {
                b"nclx" | b"nclc" => {
                    colr_seen = true;
                    stream.color = ColorInfo {
                        primaries: be16(p, 4) as u16,
                        transfer: be16(p, 6) as u16,
                        matrix: be16(p, 8) as u16,
                    };
                    if &p[..4] == b"nclx" {
                        if let Some(&flags) = p.get(10) {
                            stream.color_range = if flags & 0x80 != 0 {
                                ColorRange::Full
                            } else {
                                ColorRange::Limited
                            };
                        }
                    }
                }
                _ => {} // ICC profiles (`prof`/`rICC`): no slot for them
            },
            b"pasp" if p.len() >= 8 => {
                let (h, v) = (be32(p, 0), be32(p, 4));
                if h != 0 && v != 0 {
                    stream.pixel_aspect = Some((h, v));
                }
            }
            b"clap" if p.len() >= 32 => {
                stream.clean_aperture = Some(CleanAperture {
                    width: (be32(p, 0), be32(p, 4)),
                    height: (be32(p, 8), be32(p, 12)),
                    horiz_offset: (be32(p, 16) as i32, be32(p, 20)),
                    vert_offset: (be32(p, 24) as i32, be32(p, 28)),
                });
            }
            b"mdcv" if p.len() >= 24 => {
                // ST 2086 order: green, blue, red, then the white point.
                let xy = |o: usize| (be16(p, o) as u16, be16(p, o + 2) as u16);
                stream.mastering_display = Some(MasteringDisplay {
                    green: xy(0),
                    blue: xy(4),
                    red: xy(8),
                    white_point: xy(12),
                    max_luminance: be32(p, 16),
                    min_luminance: be32(p, 20),
                });
            }
            b"clli" if p.len() >= 4 => {
                stream.content_light = Some(ContentLightLevel {
                    max_cll: be16(p, 0) as u16,
                    max_fall: be16(p, 2) as u16,
                });
            }
            // vpcC v1: profile, level, depth(4) chroma(3) range(1), primaries,
            // transfer, matrix, init size(16) + init data.
            // The av1C record is the stream's extradata, as FFmpeg and
            // Matroska (`V_AV1` CodecPrivate) give it.
            b"av1C" if p.len() >= 4 && p[0] == 0x81 => stream.extradata = p.to_vec(),
            // The vpcC record (FullBox header included) becomes extradata,
            // so a remux writes the same profile, level and chroma siting.
            b"vpcC" if p.len() >= 12 && p[0] == 1 => {
                stream.extradata = p.to_vec();
                if colr_seen {
                    continue;
                }
                stream.color = ColorInfo {
                    primaries: u16::from(p[7]),
                    transfer: u16::from(p[8]),
                    matrix: u16::from(p[9]),
                };
                stream.color_range = if p[6] & 1 != 0 {
                    ColorRange::Full
                } else {
                    ColorRange::Limited
                };
            }
            // fiel (QuickTime; ISO readers take it too): field count, then
            // the detail code Matroska's FieldOrder shares.
            b"fiel" if p.len() >= 2 => {
                stream.field_order = Some(match (p[0], p[1]) {
                    (1, _) => 0,
                    (_, 0) => 2,
                    (_, detail) => detail,
                });
            }
            // apvC: FullBox, then APVDecoderConfigurationRecord.
            b"apvC" if p.len() > 4 => stream.extradata = p[4..].to_vec(),
            _ => {}
        }
    }
}

/// The descriptive boxes for a visual sample entry, in FilmCraft's (and
/// QuickTime's) order: `colr`, `mdcv`, `clli`, `pasp`, `clap`. ISO gets
/// `nclx`; QuickTime gets `nclc`, as FFmpeg's movenc writes, except when the
/// range is known (full or limited): `nclc` has no range flag and would lose
/// it, so those get `nclx`, which QuickTime writers (iPhone, FilmCraft) use too.
pub(crate) fn visual_children(stream: &Stream, qt: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let range_known = stream.color_range != ColorRange::Unspecified;
    let full = stream.color_range == ColorRange::Full;
    if !stream.color.is_unspecified() || range_known {
        let nclc = qt && !range_known;
        let mut b = Vec::new();
        b.extend_from_slice(if nclc { b"nclc" } else { b"nclx" });
        pu16(&mut b, stream.color.primaries);
        pu16(&mut b, stream.color.transfer);
        pu16(&mut b, stream.color.matrix);
        if !nclc {
            b.push(if full { 0x80 } else { 0 });
        }
        out.extend_from_slice(&bx(b"colr", &b));
    }
    if let Some(md) = stream.mastering_display {
        let mut b = Vec::new();
        for (x, y) in [md.green, md.blue, md.red, md.white_point] {
            pu16(&mut b, x);
            pu16(&mut b, y);
        }
        pu32(&mut b, md.max_luminance);
        pu32(&mut b, md.min_luminance);
        out.extend_from_slice(&bx(b"mdcv", &b));
    }
    if let Some(cl) = stream.content_light {
        let mut b = Vec::new();
        pu16(&mut b, cl.max_cll);
        pu16(&mut b, cl.max_fall);
        out.extend_from_slice(&bx(b"clli", &b));
    }
    if let Some((h, v)) = stream.pixel_aspect {
        let mut b = Vec::new();
        pu32(&mut b, h);
        pu32(&mut b, v);
        out.extend_from_slice(&bx(b"pasp", &b));
    }
    if let Some(order) = stream.field_order {
        let (fields, detail) = match order {
            0 => (1, 0),
            2 => (2, 0),
            d => (2, d),
        };
        out.extend_from_slice(&bx(b"fiel", &[fields, detail]));
    }
    if let Some(c) = stream.clean_aperture {
        let mut b = Vec::new();
        for v in [
            c.width.0,
            c.width.1,
            c.height.0,
            c.height.1,
            c.horiz_offset.0 as u32,
            c.horiz_offset.1,
            c.vert_offset.0 as u32,
            c.vert_offset.1,
        ] {
            pu32(&mut b, v);
        }
        out.extend_from_slice(&bx(b"clap", &b));
    }
    out
}

/// A `tkhd` / `mvhd` matrix at `at`, or `None` when it is the identity
/// (or the box is too short to hold one).
pub(crate) fn read_matrix(b: &[u8], at: usize) -> Option<[i32; 9]> {
    let raw = b.get(at..at + 36)?;
    let mut m = [0i32; 9];
    for (k, v) in m.iter_mut().enumerate() {
        *v = be32(raw, 4 * k) as i32;
    }
    (m != [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000]).then_some(m)
}

/// An `mdhd` language code → an ISO 639-2 code. `None` for `und`, for a
/// Macintosh language code (below 0x400) and for nonsense.
pub(crate) fn mdhd_language(code: u32) -> Option<String> {
    if !(0x400..=0x7FFF).contains(&code) {
        return None;
    }
    let letters: Vec<u8> = (0..3)
        .rev()
        .map(|k| ((code >> (5 * k)) & 0x1F) as u8 + 0x60)
        .collect();
    if !letters.iter().all(u8::is_ascii_lowercase) || letters == b"und" {
        return None;
    }
    String::from_utf8(letters).ok()
}

/// The packed `mdhd` code for a language: an ISO 639-2 three-letter code
/// packs as itself, anything else (none, a BCP 47 tag) as `und`.
pub(crate) fn mdhd_language_code(lang: Option<&str>) -> u16 {
    match lang.map(str::as_bytes) {
        Some(&[a, b, c]) if [a, b, c].iter().all(u8::is_ascii_lowercase) => [a, b, c]
            .iter()
            .fold(0u16, |v, &l| v << 5 | u16::from(l - 0x60)),
        _ => 0x55C4, // 'und'
    }
}

/// ProRes profile from the frame header when the stream carries no tag:
/// 4:4:4 chroma (`chroma_format` 3) is 4444, anything else standard 422.
pub(crate) fn prores_tag(stream: &Stream, first_frame: Option<&[u8]>) -> [u8; 4] {
    if let Some(tag) = stream.codec_tag.filter(|t| PRORES_TAGS.contains(t)) {
        return tag;
    }
    // frame: size(4) 'icpf'(4) hdr_size(2) version(2) creator(4) w(2) h(2) flags(1)
    match first_frame {
        Some(f) if f.len() > 20 && &f[4..8] == b"icpf" && f[20] >> 6 == 3 => *b"ap4h",
        _ => *b"apcn",
    }
}

/// The DNx sample entry: the stream's own tag, else `AVdn`.
pub(crate) fn dnx_tag(stream: &Stream) -> [u8; 4] {
    stream
        .codec_tag
        .filter(|t| DNX_TAGS.contains(t))
        .unwrap_or(*b"AVdn")
}

/// A `vpcC` box from what a VP9 stream's extradata already says: a `vpcC`
/// record (an MP4 source; written back verbatim), or Matroska's `V_VP9`
/// CodecPrivate feature list (profile, level, bit depth, chroma
/// subsampling: IDs 1-4), completed with the stream's colour. `None` when
/// the extradata is neither, and the muxer reads the first keyframe instead.
pub(crate) fn vpcc_from_extradata(stream: &Stream) -> Option<Vec<u8>> {
    let e = &stream.extradata;
    if e.len() >= 12 && e[0] == 1 && e[1..4] == [0, 0, 0] {
        return Some(bx(b"vpcC", e));
    }
    let (mut profile, mut level, mut depth, mut chroma) = (None, None, None, None);
    let mut i = 0;
    while i + 2 <= e.len() {
        let (id, len) = (e[i], usize::from(e[i + 1]));
        let v = *e.get(i + 2)?;
        if len != 1 {
            return None;
        }
        match id {
            1 => profile = Some(v),
            2 => level = Some(v),
            3 => depth = Some(v),
            4 => chroma = Some(v),
            _ => return None,
        }
        i += 3;
    }
    let profile = profile?;
    let full = stream.color_range == ColorRange::Full;
    let narrow = |v: u16| u8::try_from(v).unwrap_or(2);
    let mut b = vec![
        profile,
        level.unwrap_or_else(|| vp9_level(stream.width, stream.height)),
        (depth.unwrap_or(8) << 4) | (chroma.unwrap_or(1).min(3) << 1) | u8::from(full),
        narrow(stream.color.primaries),
        narrow(stream.color.transfer),
        narrow(stream.color.matrix),
    ];
    pu16(&mut b, 0);
    Some(fbx(b"vpcC", 1, 0, &b))
}

/// `vpcC` (version 1) for a VP9 stream, from its first keyframe's
/// uncompressed header (profile, bit depth, subsampling, range) and the
/// stream's colour. `None` if the frame is not a parsable VP9 keyframe.
pub(crate) fn build_vpcc(stream: &Stream, frame: &[u8]) -> Option<Vec<u8>> {
    let hdr = vp9_keyframe_header(frame)?;
    let level = vp9_level(stream.width, stream.height);
    let full = match stream.color_range {
        ColorRange::Full => true,
        ColorRange::Limited => false,
        ColorRange::Unspecified => hdr.full_range,
    };
    let narrow = |v: u16| u8::try_from(v).unwrap_or(2);
    let mut b = vec![
        hdr.profile,
        level,
        (hdr.bit_depth << 4) | (hdr.chroma << 1) | u8::from(full),
        narrow(stream.color.primaries),
        narrow(stream.color.transfer),
        narrow(stream.color.matrix),
    ];
    pu16(&mut b, 0); // codecIntializationDataSize: always 0 for VP9
    Some(fbx(b"vpcC", 1, 0, &b))
}

struct Vp9Header {
    profile: u8,
    bit_depth: u8,
    /// vpcC chroma_subsampling: 0 = 4:2:0 vertical, 1 = 4:2:0 colocated,
    /// 2 = 4:2:2, 3 = 4:4:4.
    chroma: u8,
    full_range: bool,
}

/// The fields `vpcC` needs from a VP9 keyframe's uncompressed header.
fn vp9_keyframe_header(frame: &[u8]) -> Option<Vp9Header> {
    let mut r = BitReader {
        data: frame,
        pos: 0,
    };
    if r.bits(2)? != 2 {
        return None; // frame_marker
    }
    let lo = r.bits(1)?;
    let hi = r.bits(1)?;
    let profile = (hi << 1 | lo) as u8;
    if profile == 3 {
        r.bits(1)?; // reserved_zero
    }
    if r.bits(1)? == 1 {
        return None; // show_existing_frame
    }
    if r.bits(1)? != 0 {
        return None; // frame_type: not a keyframe
    }
    r.bits(2)?; // show_frame, error_resilient_mode
    if r.bits(24)? != 0x49_83_42 {
        return None; // sync code
    }
    let bit_depth = if profile >= 2 {
        if r.bits(1)? == 1 {
            12
        } else {
            10
        }
    } else {
        8
    };
    let color_space = r.bits(3)?;
    let (full_range, chroma) = if color_space != 7 {
        let full = r.bits(1)? == 1;
        let chroma = if profile == 1 || profile == 3 {
            let (sx, sy) = (r.bits(1)?, r.bits(1)?);
            match (sx, sy) {
                (1, 0) => 2,
                (0, 0) => 3,
                _ => 1,
            }
        } else {
            1
        };
        (full, chroma)
    } else {
        (true, 3) // sRGB: 4:4:4, full range
    };
    Some(Vp9Header {
        profile,
        bit_depth,
        chroma,
        full_range,
    })
}

/// The lowest VP9 level whose maximum picture size holds `w`×`h` (the rate
/// limits need the bitrate, which a muxer does not know).
fn vp9_level(w: u32, h: u32) -> u8 {
    let samples = u64::from(w) * u64::from(h);
    const LEVELS: [(u64, u8); 12] = [
        (36_864, 10),
        (73_728, 11),
        (122_880, 20),
        (245_760, 21),
        (552_960, 30),
        (983_040, 31),
        (2_228_224, 40),
        (2_228_224, 41),
        (8_912_896, 50),
        (8_912_896, 51),
        (8_912_896, 52),
        (35_651_584, 60),
    ];
    LEVELS
        .iter()
        .find(|&&(max, _)| samples <= max)
        .map_or(62, |&(_, l)| l)
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = v << 1 | u32::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Some(v)
    }
}

// ---------------------------------------------------------------------------
// Audio sample entries
// ---------------------------------------------------------------------------

/// The fields of an AudioSampleEntry / QuickTime sound description that the
/// demuxer uses, and where its child boxes start.
pub(crate) struct AudioEntry {
    pub channels: u32,
    pub sample_size: u32,
    pub sample_rate: u32,
    /// QuickTime v1: bytes per frame (all channels), 0 if absent.
    pub bytes_per_frame: u32,
    /// QuickTime v2 LPCM: bits per channel and the format flags.
    pub v2_bits: u32,
    pub v2_flags: u32,
    pub v2: bool,
    /// Child boxes, with any QuickTime `wave` box flattened in.
    pub children: Vec<([u8; 4], Vec<u8>)>,
}

impl AudioEntry {
    pub fn child(&self, typ: &[u8; 4]) -> Option<&[u8]> {
        self.children
            .iter()
            .find(|(t, _)| t == typ)
            .map(|(_, p)| p.as_slice())
    }
}

/// Parse an audio sample entry's payload. `qt_layout` says whether a
/// non-zero version means the QuickTime v1/v2 layout (FFmpeg's rule: a
/// QuickTime file, or an ISO file whose `stsd` is version 0); ISO
/// AudioSampleEntryV1 has no extra fields.
pub(crate) fn parse_audio_entry(entry: &[u8], qt_layout: bool) -> AudioEntry {
    let version = if qt_layout { be16(entry, 8) } else { 0 };
    let mut a = AudioEntry {
        channels: be16(entry, 16),
        sample_size: be16(entry, 18),
        sample_rate: be32(entry, 24) >> 16,
        bytes_per_frame: 0,
        v2_bits: 0,
        v2_flags: 0,
        v2: false,
        children: Vec::new(),
    };
    let children_at = match version {
        1 => {
            a.bytes_per_frame = be32(entry, 36);
            44
        }
        2 => {
            a.v2 = true;
            let rate = f64::from_bits(be64(entry, 32));
            if rate.is_finite() && rate > 0.0 && rate < f64::from(u32::MAX) {
                a.sample_rate = rate.round() as u32;
            }
            a.channels = be32(entry, 40);
            a.v2_bits = be32(entry, 48);
            a.v2_flags = be32(entry, 52);
            a.bytes_per_frame = be32(entry, 56);
            64
        }
        _ => 28,
    };
    if let Some(rest) = entry.get(children_at..) {
        for (typ, p) in child_boxes(rest) {
            if &typ == b"wave" {
                for (t, q) in child_boxes(p) {
                    a.children.push((t, q.to_vec()));
                }
            } else {
                a.children.push((typ, p.to_vec()));
            }
        }
    }
    a
}

/// The rate an audio track really runs at, recovered whatever the writer
/// put in the 16.16 `samplerate` field: a QuickTime v2 description's float,
/// an ISO `srat` box, else the codec configuration (AudioSpecificConfig,
/// FLAC STREAMINFO, ALAC cookie) when the field cannot hold it — which also
/// repairs files whose 96 kHz was written wrapped as 30464 Hz — or is 0.
pub(crate) fn true_sample_rate(a: &AudioEntry, codec: CodecId, extradata: &[u8]) -> u32 {
    if a.v2 {
        return a.sample_rate;
    }
    if let Some(rate) = a.child(b"srat").and_then(|p| p.get(4..8)) {
        let rate = be32(rate, 0);
        if rate > 0 {
            return rate;
        }
    }
    let config = match codec {
        CodecId::Aac => rff_format::aac::asc_sample_rate(extradata),
        CodecId::Flac => flac_streaminfo(extradata)
            .map(|si| u32::from(si[10]) << 12 | u32::from(si[11]) << 4 | u32::from(si[12]) >> 4),
        CodecId::Alac => extradata.get(20..24).map(|r| be32(r, 0)),
        _ => None,
    };
    match config {
        Some(rate) if rate > 0 && (rate > 0xFFFF || a.sample_rate == 0) => rate,
        _ => a.sample_rate,
    }
}

/// The fields of a QuickTime version 2 sound description after the data
/// reference index (QuickTime File Format, "Sound Sample Description
/// (Version 2)"), for a compressed codec: the rate as a float64, so any
/// rate is exact. `frames_per_packet` 0 means variable.
pub(crate) fn sound_v2_fields(rate: u32, channels: u16, frames_per_packet: u32) -> Vec<u8> {
    let mut e = Vec::new();
    pu16(&mut e, 2); // version
    pu16(&mut e, 0); // revision
    pu32(&mut e, 0); // vendor
    pu16(&mut e, 3); // always 3
    pu16(&mut e, 16); // always 16
    pu16(&mut e, 0xFFFE); // always -2
    pu16(&mut e, 0); // always 0
    pu32(&mut e, 65536); // always 65536
    pu32(&mut e, 72); // sizeOfStructOnly
    e.extend_from_slice(&f64::from(rate).to_bits().to_be_bytes());
    pu32(&mut e, u32::from(channels));
    pu32(&mut e, 0x7F00_0000);
    pu32(&mut e, 0); // constBitsPerChannel: compressed
    pu32(&mut e, 0); // formatSpecificFlags
    pu32(&mut e, 0); // constBytesPerAudioPacket: variable
    pu32(&mut e, frames_per_packet); // constLPCMFramesPerAudioPacket
    e
}

/// `dOps` (OpusSpecificBox, "Encapsulation of Opus in ISO Base Media File
/// Format" 4.3.2) for a stream: from its `OpusHead` extradata when it has
/// one (pre-skip, input rate, gain and the channel mapping table kept), else
/// from its channels and rate, with the codec delay as the pre-skip.
pub(crate) fn build_dops(stream: &Stream) -> Vec<u8> {
    let h = &stream.extradata;
    let mut b = Vec::new();
    if h.len() >= 19 && h.starts_with(b"OpusHead") {
        b.push(0); // Version
        b.push(h[9]); // OutputChannelCount
        b.extend_from_slice(&[h[11], h[10]]); // PreSkip (LE -> BE)
        b.extend_from_slice(&[h[15], h[14], h[13], h[12]]); // InputSampleRate
        b.extend_from_slice(&[h[17], h[16]]); // OutputGain
        b.push(h[18]); // ChannelMappingFamily
        if h[18] != 0 {
            // StreamCount, CoupledCount, ChannelMapping[OutputChannelCount]
            let end = (21 + usize::from(h[9])).min(h.len());
            b.extend_from_slice(h.get(19..end).unwrap_or(&[]));
        }
    } else {
        let pre_skip = (u128::from(stream.codec_delay_ns) * 48_000 / 1_000_000_000)
            .min(u128::from(u16::MAX)) as u16;
        b.push(0);
        b.push(stream.channels.clamp(1, 255) as u8);
        pu16(&mut b, pre_skip);
        pu32(&mut b, stream.sample_rate);
        pu16(&mut b, 0); // OutputGain
        b.push(0); // ChannelMappingFamily 0: mono / stereo
    }
    bx(b"dOps", &b)
}

/// `dOps` payload → `OpusHead` (RFC 7845 5.1), the extradata rff (and
/// Matroska, and FFmpeg) carry for Opus.
pub(crate) fn dops_to_opus_head(d: &[u8]) -> Option<Vec<u8>> {
    if d.len() < 11 || d[0] != 0 {
        return None;
    }
    let mut h = b"OpusHead".to_vec();
    h.push(1); // version
    h.push(d[1]);
    h.extend_from_slice(&[d[3], d[2]]);
    h.extend_from_slice(&[d[7], d[6], d[5], d[4]]);
    h.extend_from_slice(&[d[9], d[8]]);
    h.push(d[10]);
    if d[10] != 0 {
        let end = (13 + usize::from(d[1])).min(d.len());
        h.extend_from_slice(d.get(11..end)?);
    }
    Some(h)
}

/// How a PCM track's bytes sit on disk relative to rff's little-endian
/// [`SampleFormat`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PcmLayout {
    pub format: SampleFormat,
    /// Big-endian on disk: reverse each `bytes_per_sample` group.
    pub big_endian: bool,
    /// Signed 8-bit on disk (`twos`/`sowt`/`ipcm` at 8 bits): flip the sign
    /// bit to and from rff's unsigned `U8`.
    pub signed8: bool,
    /// Bytes per frame (all channels).
    pub frame_bytes: usize,
}

impl PcmLayout {
    /// Convert between on-disk bytes and rff's layout (the transform is its
    /// own inverse).
    pub fn convert(&self, data: &mut [u8]) {
        if self.signed8 {
            data.iter_mut().for_each(|b| *b ^= 0x80);
        }
        let n = self.format.bytes_per_sample();
        if self.big_endian && n > 1 {
            data.chunks_exact_mut(n).for_each(<[u8]>::reverse);
        }
    }
}

/// The PCM layout of an uncompressed audio entry, or `None` when the bits
/// per sample are not one rff has a format for.
pub(crate) fn pcm_layout(fourcc: &[u8; 4], a: &AudioEntry) -> Option<PcmLayout> {
    let little = a
        .child(b"enda")
        .map(|p| be16(p, 0) != 0 || p.first().copied().unwrap_or(0) != 0);
    let size_or = |d: u32| if a.sample_size == 0 { d } else { a.sample_size };
    let (bits, float, big_endian, signed) = match fourcc {
        b"sowt" => (size_or(16), false, false, true),
        b"twos" => (size_or(16), false, true, true),
        b"raw " | b"NONE" => {
            let bits = size_or(8);
            (bits, false, true, bits != 8)
        }
        b"in24" => (24, false, !little.unwrap_or(false), true),
        b"in32" => (32, false, !little.unwrap_or(false), true),
        b"fl32" => (32, true, !little.unwrap_or(false), true),
        b"fl64" => (64, true, !little.unwrap_or(false), true),
        b"lpcm" => {
            let bits = if a.v2_bits != 0 {
                a.v2_bits
            } else {
                a.sample_size
            };
            let float = a.v2_flags & 1 != 0;
            (
                bits,
                float,
                a.v2_flags & 2 != 0,
                float || a.v2_flags & 4 != 0,
            )
        }
        b"ipcm" | b"fpcm" => {
            // pcmC: FullBox, format_flags (bit 0 = little endian), sample size.
            let (le, bits) = match a.child(b"pcmC") {
                Some(p) if p.len() >= 6 => (p[4] & 1 != 0, u32::from(p[5])),
                _ => (false, a.sample_size),
            };
            (bits, fourcc == b"fpcm", !le, true)
        }
        _ => return None,
    };
    let format = match (bits, float) {
        (8, false) => SampleFormat::U8,
        (16, false) => SampleFormat::S16,
        (24, false) => SampleFormat::S24,
        (32, false) => SampleFormat::S32,
        (32, true) => SampleFormat::F32,
        (64, true) => SampleFormat::F64,
        _ => return None,
    };
    // Unsigned PCM wider than 8 bits has no rff format.
    if !signed && bits != 8 {
        return None;
    }
    Some(PcmLayout {
        format,
        big_endian,
        signed8: bits == 8 && signed,
        frame_bytes: format.bytes_per_sample() * a.channels.max(1) as usize,
    })
}

/// The on-disk layout and sample entry the muxer writes for a PCM stream.
pub(crate) struct PcmOut {
    pub fourcc: [u8; 4],
    pub layout: PcmLayout,
    /// Sound description version (0, 1 or 2; ISO entries always 0).
    pub version: u16,
}

/// Choose how to store a PCM stream. QuickTime keeps the stream's own tag
/// when it can hold the format (a camera's `in24` stays `in24`), else
/// FilmCraft's choice: `sowt` for 16-bit, `raw ` for 8-bit, `lpcm` v2 for
/// the rest (and for any rate over 65535 Hz, which only v2 can state).
///
/// Only the `mov` muxer writes PCM. ISO `ipcm`/`fpcm` are read, but `mp4`
/// does not offer PCM: most MP4 players cannot play it, and an engine that
/// stream-copies whatever the container accepts would turn `in.wav ->
/// out.mp4` into such a file instead of asking for `-c:a aac`.
pub(crate) fn pcm_out(stream: &Stream) -> Option<PcmOut> {
    let format = stream.sample_format?;
    let frame_bytes = format.bytes_per_sample() * usize::from(stream.channels.max(1));
    let layout = |big_endian: bool, signed8: bool| PcmLayout {
        format,
        big_endian,
        signed8,
        frame_bytes,
    };
    use SampleFormat::*;
    if !matches!(format, U8 | S16 | S24 | S32 | F32 | F64) {
        return None;
    }
    let wide_rate = stream.sample_rate > 0xFFFF;
    let tagged = stream.codec_tag.filter(|_| !wide_rate).and_then(|tag| {
        let (big, signed8, version) = match (&tag, format) {
            (b"sowt", S16) => (false, false, 0),
            (b"twos", S16) => (true, false, 0),
            (b"sowt", U8) => (false, true, 0),
            (b"twos", U8) => (true, true, 0),
            (b"raw ", U8) => (false, false, 0),
            (b"in24", S24) | (b"in32", S32) | (b"fl32", F32) | (b"fl64", F64) => (true, false, 1),
            (b"lpcm", _) => return None, // falls through to the default lpcm
            _ => return None,
        };
        Some(PcmOut {
            fourcc: tag,
            layout: layout(big, signed8),
            version,
        })
    });
    if let Some(out) = tagged {
        return Some(out);
    }
    Some(match format {
        S16 if !wide_rate => PcmOut {
            fourcc: *b"sowt",
            layout: layout(false, false),
            version: 0,
        },
        U8 if !wide_rate => PcmOut {
            fourcc: *b"raw ",
            layout: layout(false, false),
            version: 0,
        },
        _ => PcmOut {
            fourcc: *b"lpcm",
            layout: layout(false, false),
            version: 2,
        },
    })
}

/// The whole payload of a PCM audio sample entry (reserved bytes and data
/// reference index included; the caller adds the box header).
pub(crate) fn pcm_entry(stream: &Stream, out: &PcmOut) -> Vec<u8> {
    let format = out.layout.format;
    let bits = (format.bytes_per_sample() * 8) as u16;
    let channels = stream.channels.max(1);
    let mut e = Vec::new();
    e.extend_from_slice(&[0u8; 6]);
    pu16(&mut e, 1); // data_reference_index
    if out.version == 2 {
        pu16(&mut e, 2); // version
        pu16(&mut e, 0); // revision
        pu32(&mut e, 0); // vendor
        pu16(&mut e, 3); // always 3
        pu16(&mut e, 16); // always 16
        pu16(&mut e, 0xFFFE); // always -2
        pu16(&mut e, 0); // always 0
        pu32(&mut e, 65536); // always 65536
        pu32(&mut e, 72); // sizeOfStructOnly
        e.extend_from_slice(&f64::from(stream.sample_rate).to_bits().to_be_bytes());
        pu32(&mut e, u32::from(channels));
        pu32(&mut e, 0x7F00_0000);
        pu32(&mut e, u32::from(bits));
        let float = matches!(format, SampleFormat::F32 | SampleFormat::F64);
        let signed = !float && format != SampleFormat::U8;
        let flags =
            u32::from(float) | u32::from(out.layout.big_endian) << 1 | u32::from(signed) << 2 | 8; // kAudioFormatFlagIsPacked
        pu32(&mut e, flags);
        pu32(&mut e, out.layout.frame_bytes as u32); // constBytesPerAudioPacket
        pu32(&mut e, 1); // constLPCMFramesPerAudioPacket
    } else {
        pu16(&mut e, out.version);
        pu16(&mut e, 0); // revision
        pu32(&mut e, 0); // vendor
        pu16(&mut e, channels);
        pu16(&mut e, bits);
        pu16(&mut e, 0); // compression id
        pu16(&mut e, 0); // packet size
                         // Only rates up to 65535 Hz get a v0/v1 entry (`pcm_out`).
        pu32(&mut e, stream.sample_rate.min(0xFFFF) << 16);
        if out.version == 1 {
            let bps = format.bytes_per_sample() as u32;
            pu32(&mut e, 1); // samples per packet
            pu32(&mut e, bps); // bytes per packet (per channel)
            pu32(&mut e, out.layout.frame_bytes as u32); // bytes per frame
            pu32(&mut e, bps); // bytes per sample
        }
    }
    e
}

/// FLAC in MP4: rff carries the 34-byte STREAMINFO as extradata (with or
/// without the `fLaC` marker and block header); `dfLa` stores metadata blocks
/// with their headers. Returns the `dfLa` box.
pub(crate) fn build_dfla(extradata: &[u8]) -> Option<Vec<u8>> {
    let si = flac_streaminfo(extradata)?;
    let mut b = vec![0x80, 0, 0, 34]; // last block, STREAMINFO, length 34
    b.extend_from_slice(si);
    Some(fbx(b"dfLa", 0, 0, &b))
}

/// [`build_dfla`] with the STREAMINFO rewritten from the frames written
/// (total samples, block and frame size bounds; FFmpeg's movenc does the
/// same at the trailer).
pub(crate) fn build_dfla_with(
    extradata: &[u8],
    stats: &rff_format::flac::FlacStats,
) -> Option<Vec<u8>> {
    let si: [u8; 34] = flac_streaminfo(extradata)?.try_into().ok()?;
    let mut b = vec![0x80, 0, 0, 34];
    b.extend_from_slice(&stats.rewrite(&si));
    Some(fbx(b"dfLa", 0, 0, &b))
}

/// The STREAMINFO body inside a FLAC extradata blob, whichever of the usual
/// forms it takes (bare 34 bytes, a block with its header, or `fLaC` + blocks).
pub(crate) fn flac_streaminfo(extradata: &[u8]) -> Option<&[u8]> {
    let body = extradata.strip_prefix(b"fLaC").unwrap_or(extradata);
    match body.len() {
        34 => Some(body),
        n if n >= 38 && body[0] & 0x7F == 0 => body.get(4..38),
        _ => None,
    }
}

/// `dfLa` payload (after its FullBox header) → rff's FLAC extradata (the
/// STREAMINFO body).
pub(crate) fn dfla_extradata(dfla: &[u8]) -> Option<Vec<u8>> {
    let blocks = dfla.get(4..)?;
    (blocks.len() >= 38 && blocks[0] & 0x7F == 0).then(|| blocks[4..38].to_vec())
}

/// `dac3` from an AC-3 frame header (ETSI TS 102 366 F.4): fscod, bsid,
/// bsmod, acmod, lfeon, bit_rate_code.
pub(crate) fn dac3_from_frame(frame: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 8 || frame[0] != 0x0B || frame[1] != 0x77 {
        return None;
    }
    let mut r = BitReader {
        data: &frame[4..],
        pos: 0,
    };
    let fscod = r.bits(2)?;
    let frmsizecod = r.bits(6)?;
    let bsid = r.bits(5)?;
    let bsmod = r.bits(3)?;
    let acmod = r.bits(3)?;
    if bsid > 10 {
        return None; // E-AC-3
    }
    if acmod & 1 != 0 && acmod != 1 {
        r.bits(2)?; // cmixlev
    }
    if acmod & 4 != 0 {
        r.bits(2)?; // surmixlev
    }
    if acmod == 2 {
        r.bits(2)?; // dsurmod
    }
    let lfeon = r.bits(1)?;
    let v =
        fscod << 22 | bsid << 17 | bsmod << 14 | acmod << 11 | lfeon << 10 | (frmsizecod >> 1) << 5;
    Some(bx(b"dac3", &v.to_be_bytes()[1..]))
}

/// `dec3` for an E-AC-3 stream whose first frame is a single independent
/// substream (the common case): data_rate, then one substream's fscod, bsid,
/// bsmod, acmod, lfeon (ETSI TS 102 366 F.6).
pub(crate) fn dec3_from_frame(frame: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 6 || frame[0] != 0x0B || frame[1] != 0x77 {
        return None;
    }
    let mut r = BitReader {
        data: &frame[2..],
        pos: 0,
    };
    let _strmtyp = r.bits(2)?;
    let _substreamid = r.bits(3)?;
    let frmsiz = r.bits(11)?;
    let fscod = r.bits(2)?;
    let (fscod2, numblkscod) = if fscod == 3 {
        (r.bits(2)?, 3)
    } else {
        (0, r.bits(2)?)
    };
    let acmod = r.bits(3)?;
    let lfeon = r.bits(1)?;
    let bsid = r.bits(5)?;
    if bsid <= 10 {
        return None; // plain AC-3
    }
    let rate = [48_000u32, 44_100, 32_000][if fscod == 3 { 2 } else { fscod as usize }.min(2)];
    let rate = if fscod == 3 {
        [24_000, 22_050, 16_000][fscod2.min(2) as usize]
    } else {
        rate
    };
    let blocks = [1u32, 2, 3, 6][numblkscod as usize];
    // bytes per frame * 8 / seconds per frame, in kbit/s.
    let kbps = ((frmsiz + 1) * 2 * 8 * rate / (blocks * 256) / 1000) as u16;
    let mut b = Vec::new();
    pu16(&mut b, kbps << 3); // data_rate(13) num_ind_sub(3)=0 → one substream
    let fs = if fscod == 3 { 3 } else { fscod };
    // fscod(2) bsid(5) reserved(1) asvc(1) bsmod(3) acmod(3) lfeon(1) reserved(3) num_dep_sub(4) reserved(1)
    let v: u32 = fs << 22 | bsid << 17 | acmod << 9 | lfeon << 8;
    b.extend_from_slice(&v.to_be_bytes()[1..]);
    Some(bx(b"dec3", &b))
}

// ---------------------------------------------------------------------------
// Timecode
// ---------------------------------------------------------------------------

/// A `tmcd` sample entry's payload → [`Timecode`] (start frame 0 until the
/// demuxer reads the track's sample).
pub(crate) fn parse_tmcd_entry(entry: &[u8]) -> Option<Timecode> {
    if entry.len() < 26 {
        return None;
    }
    let flags = be32(entry, 12);
    let timescale = be32(entry, 16);
    let frame_duration = be32(entry, 20);
    let fps = entry[24];
    let reel_name = entry.get(26..).and_then(|rest| {
        child_boxes(rest)
            .into_iter()
            .find(|(t, _)| t == b"name")
            .and_then(|(_, p)| {
                let len = be16(p, 0) as usize;
                p.get(4..4 + len)
                    .map(|s| String::from_utf8_lossy(s).into_owned())
            })
    });
    Some(Timecode {
        start_frame: 0,
        frames_per_second: fps.max(1),
        drop_frame: flags & 1 != 0,
        timescale,
        frame_duration,
        flags: flags & !1,
        reel_name,
    })
}

/// The `tmcd` sample entry box for `tc`.
pub(crate) fn tmcd_entry(tc: &Timecode) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&[0u8; 6]);
    pu16(&mut e, 1); // data_reference_index
    pu32(&mut e, 0); // reserved
    pu32(&mut e, tc.flags & !1 | u32::from(tc.drop_frame));
    let (timescale, duration) = tmcd_rate(tc);
    pu32(&mut e, timescale);
    pu32(&mut e, duration);
    e.push(tc.frames_per_second.max(1));
    e.push(0);
    if let Some(name) = &tc.reel_name {
        let mut n = Vec::new();
        pu16(&mut n, name.len().min(0xFFFF) as u16);
        pu16(&mut n, 0); // language
        n.extend_from_slice(&name.as_bytes()[..name.len().min(0xFFFF)]);
        e.extend_from_slice(&bx(b"name", &n));
    }
    bx(b"tmcd", &e)
}

/// `(timescale, frame_duration)` for a timecode, defaulting a missing rate
/// to the nominal integer rate (29.97 for 30 fps drop-frame).
pub(crate) fn tmcd_rate(tc: &Timecode) -> (u32, u32) {
    if tc.timescale != 0 && tc.frame_duration != 0 {
        (tc.timescale, tc.frame_duration)
    } else if tc.drop_frame {
        (u32::from(tc.frames_per_second) * 1000, 1001)
    } else {
        (u32::from(tc.frames_per_second.max(1)), 1)
    }
}

/// The `gmhd` media header of a QuickTime timecode track (`gmin` + `tmcd`
/// with its `tcmi` text-style record), as QuickTime and FilmCraft write it.
pub(crate) fn tmcd_gmhd() -> Vec<u8> {
    let mut gmin = Vec::new();
    pu16(&mut gmin, 0x40); // graphics mode: dither copy
    for _ in 0..3 {
        pu16(&mut gmin, 0x8000); // opcolor
    }
    pu16(&mut gmin, 0); // balance
    pu16(&mut gmin, 0); // reserved
    let mut tcmi = Vec::new();
    pu16(&mut tcmi, 0); // text font
    pu16(&mut tcmi, 0); // text face
    pu16(&mut tcmi, 12); // text size
    pu16(&mut tcmi, 0); // reserved
    tcmi.extend_from_slice(&[0xFF; 6]); // text colour
    tcmi.extend_from_slice(&[0; 6]); // background colour
    tcmi.push(0); // font name (empty Pascal string)
    let tmcd = bx(b"tmcd", &fbx(b"tcmi", 0, 0, &tcmi));
    bx(b"gmhd", &[fbx(b"gmin", 0, 0, &gmin), tmcd].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(
        fourcc: &[u8; 4],
        channels: u32,
        bits: u32,
        children: Vec<([u8; 4], Vec<u8>)>,
    ) -> PcmLayout {
        let a = AudioEntry {
            channels,
            sample_size: bits,
            sample_rate: 48_000,
            bytes_per_frame: 0,
            v2_bits: 0,
            v2_flags: 0,
            v2: false,
            children,
        };
        pcm_layout(fourcc, &a).unwrap()
    }

    #[test]
    fn quicktime_pcm_entries_map_to_little_endian_formats() {
        let twos = audio(b"twos", 2, 16, vec![]);
        assert_eq!(
            (twos.format, twos.big_endian, twos.frame_bytes),
            (SampleFormat::S16, true, 4)
        );
        let sowt = audio(b"sowt", 2, 16, vec![]);
        assert!(!sowt.big_endian);
        let in24 = audio(b"in24", 6, 16, vec![]);
        assert_eq!(
            (in24.format, in24.big_endian, in24.frame_bytes),
            (SampleFormat::S24, true, 18)
        );
        let in24le = audio(b"in24", 2, 16, vec![(*b"enda", vec![0, 1])]);
        assert!(!in24le.big_endian);
        let fl64 = audio(b"fl64", 1, 16, vec![]);
        assert_eq!(fl64.format, SampleFormat::F64);
        let s8 = audio(b"twos", 1, 8, vec![]);
        assert_eq!((s8.format, s8.signed8), (SampleFormat::U8, true));
        let u8 = audio(b"raw ", 1, 8, vec![]);
        assert_eq!((u8.format, u8.signed8), (SampleFormat::U8, false));
    }

    #[test]
    fn pcm_conversion_is_its_own_inverse() {
        let l = PcmLayout {
            format: SampleFormat::S24,
            big_endian: true,
            signed8: false,
            frame_bytes: 3,
        };
        let mut d = vec![1, 2, 3, 4, 5, 6];
        l.convert(&mut d);
        assert_eq!(d, [3, 2, 1, 6, 5, 4]);
        l.convert(&mut d);
        assert_eq!(d, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn vp9_keyframe_header_fields() {
        // profile 0 keyframe: marker 10, profile 00, show_existing 0, type 0,
        // show 1, err 0, sync 0x498342, color_space 001 (BT.601), range 0.
        let bits = "10 0 0 0 0 1 0 010010011000001101000010 001 0";
        let bits: String = bits.split_whitespace().collect();
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (i, c) in bits.chars().enumerate() {
            if c == '1' {
                bytes[i / 8] |= 0x80 >> (i % 8);
            }
        }
        let h = vp9_keyframe_header(&bytes).unwrap();
        assert_eq!(
            (h.profile, h.bit_depth, h.chroma, h.full_range),
            (0, 8, 1, false)
        );
        assert_eq!(vp9_level(1920, 1080), 40);
        assert_eq!(vp9_level(3840, 2160), 50);
    }

    #[test]
    fn ac3_header_gives_dac3() {
        // syncword, crc, fscod=0 frmsizecod=0x1C (448k... /2 = 14), bsid=8, bsmod=0, acmod=7 (3/2), cmix, surmix, lfeon=1.
        let mut f = vec![0x0B, 0x77, 0, 0];
        // fscod(2)=00 frmsizecod(6)=011100 | bsid(5)=01000 bsmod(3)=000 | acmod(3)=111 cmix(2)=00 surmix(2)=00 lfeon(1)=1
        f.extend_from_slice(&[0b0001_1100, 0b0100_0000, 0b1110_0001, 0, 0, 0]);
        let dac3 = dac3_from_frame(&f).unwrap();
        let v = u32::from_be_bytes([0, dac3[8], dac3[9], dac3[10]]);
        assert_eq!(v >> 22, 0); // fscod
        assert_eq!(v >> 17 & 31, 8); // bsid
        assert_eq!(v >> 11 & 7, 7); // acmod
        assert_eq!(v >> 10 & 1, 1); // lfeon
        assert_eq!(v >> 5 & 31, 14); // bit_rate_code
    }
}
