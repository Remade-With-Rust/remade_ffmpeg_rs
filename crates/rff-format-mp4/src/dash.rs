//! DASH VOD output: fMP4 init + media segments plus a static `.mpd` manifest.
//!
//! `rff -i in.mp4 -c:v h264 -c:a aac out.mpd` writes, next to `out.mpd`:
//! `init-stream0.m4s`, `chunk-stream0-00001.m4s`, … one set per stream, and a
//! `static` manifest with one AdaptationSet per stream and an exact
//! SegmentTimeline. Fragments cut on video keyframes once `-seg_duration`
//! seconds have accumulated (audio cuts purely by time).
//!
//! Everything is buffered until the trailer (VOD; the same policy as the MP4
//! muxer) so durations and the timeline are exact, not estimated.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use rff_core::{CodecId, Error, MediaType, Packet, Result};
use rff_format::avc::{build_avcc_record, split_annexb};
use rff_format::{Muxer, Stream};

use super::fmp4::{self, FragSample};
use super::{bx, codec_fourcc, pick_timescale, TrackOut};

/// One output stream's buffered samples + fragmenting state.
struct DashTrack {
    out: TrackOut,
    timescale: u32,
    /// Buffered `(data, keyframe, pts_ticks)` — AVCC-converted for H.264.
    samples: Vec<(Vec<u8>, bool, Option<i64>)>,
}

pub struct DashMuxer {
    dir: PathBuf,
    mpd_path: PathBuf,
    seg_seconds: f64,
    tracks: Vec<DashTrack>,
}

impl DashMuxer {
    pub fn new(mpd_path: &Path, seg_seconds: f64) -> Result<DashMuxer> {
        Ok(DashMuxer {
            dir: mpd_path.parent().map(Path::to_path_buf).unwrap_or_default(),
            mpd_path: mpd_path.to_path_buf(),
            seg_seconds: seg_seconds.clamp(0.5, 60.0),
            tracks: Vec::new(),
        })
    }
}

impl Muxer for DashMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("dash mux: no streams"));
        }
        for s in streams {
            if !matches!(
                s.codec_id,
                CodecId::H264 | CodecId::Aac | CodecId::Opus | CodecId::Avif
            ) {
                return Err(Error::unsupported(format!(
                    "dash mux: codec `{}` (fMP4 carries h264/aac/opus/av1 here)",
                    s.codec_id.name()
                )));
            }
            self.tracks.push(DashTrack {
                out: TrackOut {
                    stream: s.clone(),
                    fourcc: codec_fourcc(s.codec_id),
                    config: None,
                    samples: Vec::new(),
                },
                timescale: pick_timescale(s),
                samples: Vec::new(),
            });
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let Some(track) = self.tracks.get_mut(packet.stream_index) else {
            return Err(Error::invalid(format!(
                "dash mux: packet for unknown stream {}",
                packet.stream_index
            )));
        };
        // Same normalisation as the plain MP4 muxer: H.264 Annex-B → AVCC with
        // the avcC hoisted; AV1 harvests its config from the first packet.
        let data = if track.out.stream.codec_id == CodecId::H264 {
            let mut sample = Vec::new();
            let (mut sps, mut pps) = (None, None);
            for nal in split_annexb(&packet.data) {
                match nal.first().map(|b| b & 0x1F) {
                    Some(7) => sps = Some(nal.to_vec()),
                    Some(8) => pps = Some(nal.to_vec()),
                    _ => {
                        sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        sample.extend_from_slice(nal);
                    }
                }
            }
            if track.out.config.is_none() {
                if let (Some(s), Some(p)) = (&sps, &pps) {
                    track.out.config = Some(bx(b"avcC", &build_avcc_record(s, p)));
                }
            }
            sample
        } else {
            if track.out.stream.codec_id == CodecId::Avif && track.out.config.is_none() {
                track.out.config = rff_format::av1::config_record(&packet.data)
                    .map(|r| bx(b"av1C", &r));
            }
            packet.data.clone()
        };
        track
            .samples
            .push((data, packet.flags.keyframe, packet.pts));
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let mut manifest_parts = Vec::new();
        let mut presentation_secs = 0f64;

        for (id, track) in self.tracks.iter_mut().enumerate() {
            let ts = track.timescale.max(1);
            let durations =
                super::sample_durations(&track.samples, ts, track.out.stream.media_type);
            let is_video = track.out.stream.media_type == MediaType::Video;

            // --- init segment ---
            let init_name = format!("init-stream{id}.m4s");
            let init = fmp4::init_segment(&track.out, id as u32 + 1, ts);
            fs::write(self.dir.join(&init_name), init)?;

            // --- cut fragments: video on keyframes, both by elapsed time ---
            let target_ticks = (self.seg_seconds * ts as f64) as i64;
            let mut fragments: Vec<Vec<FragSample>> = Vec::new();
            let mut current: Vec<FragSample> = Vec::new();
            let mut current_ticks = 0i64;
            for (i, (data, keyframe, _)) in track.samples.iter().enumerate() {
                let due = current_ticks >= target_ticks;
                let cut = !current.is_empty() && due && (*keyframe || !is_video);
                if cut {
                    fragments.push(std::mem::take(&mut current));
                    current_ticks = 0;
                }
                current.push(FragSample {
                    data: data.clone(),
                    keyframe: *keyframe,
                    duration: durations[i],
                });
                current_ticks += durations[i] as i64;
            }
            if !current.is_empty() {
                fragments.push(current);
            }

            // --- media segments + exact timeline ---
            let mut timeline = Vec::new(); // (start_ticks, dur_ticks)
            let mut base_time = 0u64;
            for (n, frag) in fragments.iter().enumerate() {
                let name = format!("chunk-stream{id}-{:05}.m4s", n + 1);
                let seg = fmp4::media_segment(n as u32 + 1, id as u32 + 1, base_time, frag);
                fs::write(self.dir.join(&name), seg)?;
                let dur: u64 = frag.iter().map(|s| s.duration as u64).sum();
                timeline.push((base_time, dur));
                base_time += dur;
            }
            presentation_secs = presentation_secs.max(base_time as f64 / ts as f64);

            // --- Representation XML ---
            let s = &track.out.stream;
            let mime = fmp4::mime_type(s.media_type);
            let codecs = fmp4::codec_string(&track.out);
            let mut rep = String::new();
            let _ = writeln!(rep, "    <AdaptationSet mimeType=\"{mime}\" segmentAlignment=\"true\">");
            let _ = write!(rep, "      <Representation id=\"{id}\" codecs=\"{codecs}\" bandwidth=\"200000\"");
            if is_video && s.width > 0 {
                let _ = write!(rep, " width=\"{}\" height=\"{}\"", s.width, s.height);
            }
            if s.media_type == MediaType::Audio && s.sample_rate > 0 {
                let _ = write!(rep, " audioSamplingRate=\"{}\"", s.sample_rate);
            }
            let _ = writeln!(rep, ">");
            let _ = writeln!(
                rep,
                "        <SegmentTemplate timescale=\"{ts}\" initialization=\"init-stream{id}.m4s\" media=\"chunk-stream{id}-$Number%05d$.m4s\" startNumber=\"1\">"
            );
            let _ = writeln!(rep, "          <SegmentTimeline>");
            for (start, dur) in &timeline {
                let _ = writeln!(rep, "            <S t=\"{start}\" d=\"{dur}\"/>");
            }
            let _ = writeln!(rep, "          </SegmentTimeline>");
            let _ = writeln!(rep, "        </SegmentTemplate>");
            let _ = writeln!(rep, "      </Representation>");
            let _ = writeln!(rep, "    </AdaptationSet>");
            manifest_parts.push(rep);
        }

        // --- manifest ---
        let mut mpd = String::new();
        let _ = writeln!(mpd, "<?xml version=\"1.0\" encoding=\"utf-8\"?>");
        let _ = writeln!(
            mpd,
            "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" profiles=\"urn:mpeg:dash:profile:isoff-main:2011\" type=\"static\" mediaPresentationDuration=\"PT{presentation_secs:.3}S\" minBufferTime=\"PT{:.1}S\">",
            self.seg_seconds
        );
        let _ = writeln!(mpd, "  <Period start=\"PT0S\">");
        for part in &manifest_parts {
            mpd.push_str(part);
        }
        let _ = writeln!(mpd, "  </Period>");
        let _ = writeln!(mpd, "</MPD>");
        fs::write(&self.mpd_path, mpd)?;
        Ok(())
    }
}
