//! Fragmented MP4 (fMP4/CMAF) building blocks: the init segment
//! (`ftyp`+`moov` with `mvex`) and media segments (`moof`+`mdat`), reusing the
//! parent module's box builders and sample-entry construction. DASH output is
//! layered on these; HLS-fMP4 could be too.

use rff_core::MediaType;

use super::{bx, fbx, pu32, TrackOut};

/// One coded frame inside a fragment.
pub struct FragSample {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Decode duration in the track's timescale.
    pub duration: u32,
}

/// Build the init segment for one track: `ftyp` (iso6/dash) + `moov` whose
/// sample tables are empty and whose `mvex/trex` declares fragment defaults.
///
/// `start` is where presentation starts on the track's decode timeline
/// ([`TimingPlan::start`](super::TimingPlan)): a B-frame stream's reorder
/// delay, an audio track's priming. When it is not 0 the `trak` carries an
/// edit list with that `media_time` and a segment duration of 0 -- the rest
/// of the media, whose length the init segment doesn't know. That is what
/// FFmpeg's DASH muxer writes (movenc with `delay_moov`), rather than
/// negative composition offsets.
pub(crate) fn init_segment(track: &TrackOut, track_id: u32, timescale: u32, start: i64) -> Vec<u8> {
    let ftyp = bx(b"ftyp", b"iso6\x00\x00\x02\x00iso6dashcmfc");

    // trak with empty stbl tables (fragments carry all samples).
    let empty = super::TimingPlan {
        timescale,
        durations: Vec::new(),
        offsets: Vec::new(),
        start,
        presentation: 0,
    };
    let trak = super::build_trak(track, track_id, &empty, 0, 0);

    let mut moov_body = super::build_mvhd(1000, 0, track_id + 1);
    moov_body.extend_from_slice(&trak);

    // mvex/trex: per-fragment defaults (everything explicit in each trun).
    let mut trex = Vec::new();
    pu32(&mut trex, track_id);
    pu32(&mut trex, 1); // default_sample_description_index
    pu32(&mut trex, 0); // default_sample_duration
    pu32(&mut trex, 0); // default_sample_size
    pu32(&mut trex, 0); // default_sample_flags
    let mvex = bx(b"mvex", &fbx(b"trex", 0, 0, &trex));
    moov_body.extend_from_slice(&mvex);

    let mut out = ftyp;
    out.extend_from_slice(&bx(b"moov", &moov_body));
    out
}

/// Sample flags for a trun entry: sync samples get "depends on nothing",
/// everything else "depends + non-sync" (ISO 14496-12 §8.8.3).
fn sample_flags(keyframe: bool) -> u32 {
    if keyframe {
        0x0200_0000
    } else {
        0x0101_0000
    }
}

/// `trun` flags: data offset, then each sample's duration, size and flags.
const TRUN_DATA_OFFSET_DURATION_SIZE_FLAGS: u32 = 0x00_0701;
/// `trun` flag: each sample's composition time offset.
const TRUN_SAMPLE_CTS: u32 = 0x00_0800;

/// Build one media segment: `moof` (mfhd + traf{tfhd,tfdt,trun}) + `mdat`.
/// `base_time` is the segment's first sample decode time in the track
/// timescale.
pub fn media_segment(
    sequence: u32,
    track_id: u32,
    base_time: u64,
    samples: &[FragSample],
) -> Vec<u8> {
    media_segment_with_offsets(sequence, track_id, base_time, samples, None)
}

/// [`media_segment`] for a track whose frames are reordered (some pts
/// differing from its dts): `composition_offsets` holds each sample's
/// composition offset (pts - dts, in the track's timescale; a missing entry
/// is 0). With `Some` every `trun` entry carries its sample's offset -- in
/// each fragment of the track, as FFmpeg does -- in a version 1 `trun`
/// (signed offsets) when one is negative, else version 0. With `None` it is
/// [`media_segment`].
pub fn media_segment_with_offsets(
    sequence: u32,
    track_id: u32,
    base_time: u64,
    samples: &[FragSample],
    composition_offsets: Option<&[i64]>,
) -> Vec<u8> {
    let offset = |i: usize| {
        composition_offsets
            .and_then(|o| o.get(i))
            .copied()
            .unwrap_or(0)
    };

    let mut mfhd = Vec::new();
    pu32(&mut mfhd, sequence);
    let mfhd = fbx(b"mfhd", 0, 0, &mfhd);

    // tfhd: default-base-is-moof (0x020000) — offsets count from moof start.
    let mut tfhd = Vec::new();
    pu32(&mut tfhd, track_id);
    let tfhd = fbx(b"tfhd", 0, 0x02_0000, &tfhd);

    // tfdt (v1): 64-bit baseMediaDecodeTime.
    let mut tfdt = Vec::new();
    tfdt.extend_from_slice(&base_time.to_be_bytes());
    let tfdt = fbx(b"tfdt", 1, 0, &tfdt);

    // trun: data-offset + per-sample duration/size/flags[/composition offset].
    let mut trun = Vec::new();
    pu32(&mut trun, samples.len() as u32);
    pu32(&mut trun, 0); // data_offset placeholder (patched below)
    for (i, s) in samples.iter().enumerate() {
        pu32(&mut trun, s.duration);
        pu32(&mut trun, s.data.len() as u32);
        pu32(&mut trun, sample_flags(s.keyframe));
        if composition_offsets.is_some() {
            // The same 32 bits read unsigned (version 0) or signed (1).
            let cts = offset(i).clamp(i64::from(i32::MIN), i64::from(u32::MAX));
            pu32(&mut trun, cts as u32);
        }
    }
    let (version, flags) = if composition_offsets.is_some() {
        let signed = (0..samples.len()).any(|i| offset(i) < 0);
        (
            u8::from(signed),
            TRUN_DATA_OFFSET_DURATION_SIZE_FLAGS | TRUN_SAMPLE_CTS,
        )
    } else {
        (0, TRUN_DATA_OFFSET_DURATION_SIZE_FLAGS)
    };
    let trun = fbx(b"trun", version, flags, &trun);

    let traf = bx(b"traf", &[tfhd, tfdt, trun].concat());
    let mut moof = bx(b"moof", &[mfhd, traf].concat());

    // Patch trun's data_offset: first mdat payload byte, from moof start.
    let data_offset = (moof.len() + 8) as u32;
    // Locate the placeholder: it sits 4 bytes into the trun body. Compute its
    // absolute position: moof header(8) + mfhd + traf header(8) + tfhd + tfdt
    // + trun header(8) + fullbox(4) + sample_count(4).
    let mfhd_len = {
        let mut b = Vec::new();
        pu32(&mut b, sequence);
        fbx(b"mfhd", 0, 0, &b).len()
    };
    let tfhd_len = {
        let mut b = Vec::new();
        pu32(&mut b, track_id);
        fbx(b"tfhd", 0, 0x02_0000, &b).len()
    };
    let tfdt_len = {
        let b = base_time.to_be_bytes();
        fbx(b"tfdt", 1, 0, &b).len()
    };
    let off = 8 + mfhd_len + 8 + tfhd_len + tfdt_len + 8 + 4 + 4;
    moof[off..off + 4].copy_from_slice(&data_offset.to_be_bytes());

    let mut mdat_body = Vec::new();
    for s in samples {
        mdat_body.extend_from_slice(&s.data);
    }
    moof.extend_from_slice(&bx(b"mdat", &mdat_body));
    moof
}

/// The RFC 6381 `codecs=` string for a track, used by the DASH manifest.
pub(crate) fn codec_string(track: &TrackOut) -> String {
    match &track.fourcc {
        b"avc1" => {
            // avc1.PPCCLL from the avcC record (profile, compat, level).
            let (p, c, l) = track
                .config
                .as_deref()
                // config = avcC box: [size(4)][avcC(4)][ver][profile][compat][level]
                .and_then(|b| Some((*b.get(9)?, *b.get(10)?, *b.get(11)?)))
                .unwrap_or((66, 0, 30));
            format!("avc1.{p:02X}{c:02X}{l:02X}")
        }
        b"av01" => "av01.0.04M.08".to_string(), // best-effort level; players re-read the seq header
        b"mp4a" => "mp4a.40.2".to_string(),     // AAC-LC
        b"Opus" => "opus".to_string(),
        other => String::from_utf8_lossy(&other[..]).into_owned(),
    }
}

/// Media type → DASH AdaptationSet mimeType.
pub(crate) fn mime_type(media: MediaType) -> &'static str {
    match media {
        MediaType::Video => "video/mp4",
        _ => "audio/mp4",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_segment_data_offset_points_at_mdat_payload() {
        let samples = [
            FragSample {
                data: vec![0xAA; 10],
                keyframe: true,
                duration: 100,
            },
            FragSample {
                data: vec![0xBB; 4],
                keyframe: false,
                duration: 100,
            },
        ];
        let seg = media_segment(1, 1, 0, &samples);
        // moof is the first box; mdat follows it.
        let moof_size = u32::from_be_bytes(seg[0..4].try_into().unwrap()) as usize;
        assert_eq!(&seg[4..8], b"moof");
        assert_eq!(&seg[moof_size + 4..moof_size + 8], b"mdat");
        // First mdat payload byte is our first sample byte.
        assert_eq!(seg[moof_size + 8], 0xAA);
        // The trun data_offset must equal moof_size + 8. Find trun and check.
        let pos = seg
            .windows(4)
            .position(|w| w == b"trun")
            .expect("trun present");
        // fullbox(4) + sample_count(4) then data_offset.
        let off_pos = pos + 4 + 4 + 4;
        let got = u32::from_be_bytes(seg[off_pos..off_pos + 4].try_into().unwrap());
        assert_eq!(got as usize, moof_size + 8);
    }

    fn sample() -> FragSample {
        FragSample {
            data: vec![0xCC; 3],
            keyframe: false,
            duration: 40,
        }
    }

    fn with_offsets(offsets: &[i64]) -> Vec<u8> {
        let samples: Vec<_> = offsets.iter().map(|_| sample()).collect();
        media_segment_with_offsets(1, 1, 0, &samples, Some(offsets))
    }

    /// A segment's `trun`: (version, flags, the words after the flags).
    fn trun(seg: &[u8]) -> (u8, u32, Vec<u32>) {
        let pos = seg.windows(4).position(|w| w == b"trun").unwrap();
        let size = u32::from_be_bytes(seg[pos - 4..pos].try_into().unwrap()) as usize;
        let body = &seg[pos + 4..pos - 4 + size];
        let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
        let words = body[4..]
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
            .collect();
        (body[0], flags, words)
    }

    /// #88: composition offsets go in each `trun` entry, signed (version 1)
    /// only when one is negative; without them the layout is unchanged.
    #[test]
    fn composition_offsets_go_in_the_trun() {
        let (version, flags, w) = trun(&with_offsets(&[80, 0]));
        assert_eq!((version, flags), (0, 0xF01));
        // sample_count, data_offset, then duration/size/flags/offset each.
        assert_eq!(&w[2..], [40, 3, 0x0101_0000, 80, 40, 3, 0x0101_0000, 0]);
        let (version, flags, w) = trun(&with_offsets(&[40, -40]));
        assert_eq!((version, flags), (1, 0xF01));
        assert_eq!(w[9] as i32, -40);
        let (version, flags, w) = trun(&media_segment(1, 1, 0, &[sample()]));
        assert_eq!((version, flags, w.len()), (0, 0x701, 2 + 3));
    }

    /// #88: the init segment's edit list starts presentation `start` into
    /// the media, for the rest of it (segment duration 0), as FFmpeg's DASH
    /// muxer writes it; no edit list when presentation starts at 0.
    #[test]
    fn init_segment_edit_list_skips_the_presentation_delay() {
        let mut s = rff_format::Stream::new(0, rff_core::CodecId::H264);
        s.media_type = MediaType::Video;
        let track = TrackOut::new(&s, *b"avc1");
        let init = init_segment(&track, 1, 12_800, 1024);
        let pos = init.windows(4).position(|w| w == b"elst").unwrap();
        let word = |k: usize| {
            u32::from_be_bytes(init[pos + 4 + 4 * k..pos + 8 + 4 * k].try_into().unwrap())
        };
        // version/flags, entry_count, segment_duration, media_time, rate.
        assert_eq!(
            [word(0), word(1), word(2), word(3), word(4)],
            [0, 1, 0, 1024, 0x0001_0000]
        );
        let init = init_segment(&track, 1, 12_800, 0);
        assert!(!init.windows(4).any(|w| w == b"edts"));
    }
}
