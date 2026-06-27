//! `rff-format-hls` — HLS output: an MPEG-TS segmenter plus an `.m3u8` playlist.
//!
//! HTTP Live Streaming is a `.m3u8` playlist referencing a series of short
//! MPEG-TS segments. Unlike a normal muxer (one container → one byte sink), HLS
//! writes *many* files, so [`HlsSegmenter`] is constructed with the playlist
//! **path** (not just a writer) and creates `name0.ts`, `name1.ts`, … itself,
//! one [`TsMuxer`] per segment, then writes the playlist on finalize.
//!
//! It still implements [`Muxer`], so the transcode loop drives it exactly like
//! any other muxer — it just fans packets out across segment files. Segments
//! roll over once a segment has covered `target_duration` seconds, preferring a
//! video keyframe boundary so each segment starts independently decodable.
//!
//! This is copy-friendly VOD output (`#EXT-X-ENDLIST`); live/event playlists,
//! fMP4 segments, and DASH are the natural follow-ups.

use std::fs::File;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rff_core::{Error, MediaType, Packet, Rational, Result};
use rff_format::{Muxer, Output, Stream};
use rff_format_ts::TsMuxer;

/// Segments an output into MPEG-TS pieces and writes an `.m3u8` playlist.
pub struct HlsSegmenter {
    /// Directory the playlist + segments live in.
    dir: PathBuf,
    /// Playlist file name stem (`out` for `out.m3u8`), used to name segments.
    stem: String,
    /// Where the `.m3u8` is written on finalize.
    playlist_path: PathBuf,
    /// Target seconds per segment before rolling over on a keyframe.
    target_duration: f64,

    streams: Vec<Stream>,
    /// Index of the stream whose keyframes/timing drive segment boundaries.
    ref_stream: usize,
    /// Time base of the reference stream (ticks → seconds).
    ref_tb: Rational,

    /// The segment currently being written, if any.
    current: Option<TsMuxer>,
    /// Names of completed + in-progress segments, in order.
    seg_names: Vec<String>,
    /// Durations (seconds) of completed segments.
    seg_durations: Vec<f64>,
    /// Reference-stream PTS at the start of the current segment.
    seg_start_pts: Option<i64>,
    /// Latest reference-stream PTS seen (to measure the final segment).
    last_ref_pts: Option<i64>,
}

impl HlsSegmenter {
    /// Create a segmenter that writes `playlist_path` (e.g. `dir/out.m3u8`) and
    /// `dir/out0.ts`, `dir/out1.ts`, … `target_duration` is the nominal seconds
    /// per segment.
    pub fn new(playlist_path: &Path, target_duration: f64) -> Result<HlsSegmenter> {
        let dir = playlist_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = playlist_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::Option("hls: output needs a .m3u8 file name".into()))?
            .to_string();
        Ok(HlsSegmenter {
            dir,
            stem,
            playlist_path: playlist_path.to_path_buf(),
            target_duration: target_duration.max(0.5),
            streams: Vec::new(),
            ref_stream: 0,
            ref_tb: Rational::new(1, 90_000),
            current: None,
            seg_names: Vec::new(),
            seg_durations: Vec::new(),
            seg_start_pts: None,
            last_ref_pts: None,
        })
    }

    fn tb_seconds(&self, ticks: i64) -> f64 {
        ticks as f64 * self.ref_tb.num as f64 / self.ref_tb.den.max(1) as f64
    }

    /// Open a fresh `.ts` segment and write its PAT/PMT header.
    fn open_segment(&mut self) -> Result<()> {
        let name = format!("{}{}.ts", self.stem, self.seg_names.len());
        let path = self.dir.join(&name);
        let out: Output = Box::new(File::create(&path)?);
        let mut mux = TsMuxer::new(out);
        mux.write_header(&self.streams)?;
        self.current = Some(mux);
        self.seg_names.push(name);
        Ok(())
    }

    /// Finalize the current segment and record its measured duration.
    fn close_segment(&mut self, end_pts: Option<i64>) -> Result<()> {
        if let Some(mut mux) = self.current.take() {
            mux.write_trailer()?;
            let dur = match (self.seg_start_pts, end_pts.or(self.last_ref_pts)) {
                (Some(start), Some(end)) if end > start => self.tb_seconds(end - start),
                _ => self.target_duration,
            };
            self.seg_durations.push(dur);
        }
        self.seg_start_pts = None;
        Ok(())
    }

    fn write_playlist(&self) -> Result<()> {
        let target = self
            .seg_durations
            .iter()
            .cloned()
            .fold(self.target_duration, f64::max)
            .ceil()
            .max(1.0) as u64;
        let mut m3u8 = String::new();
        let _ = writeln!(m3u8, "#EXTM3U");
        let _ = writeln!(m3u8, "#EXT-X-VERSION:3");
        let _ = writeln!(m3u8, "#EXT-X-TARGETDURATION:{target}");
        let _ = writeln!(m3u8, "#EXT-X-MEDIA-SEQUENCE:0");
        let _ = writeln!(m3u8, "#EXT-X-PLAYLIST-TYPE:VOD");
        for (dur, name) in self.seg_durations.iter().zip(&self.seg_names) {
            let _ = writeln!(m3u8, "#EXTINF:{dur:.3},");
            let _ = writeln!(m3u8, "{name}");
        }
        let _ = writeln!(m3u8, "#EXT-X-ENDLIST");
        std::fs::write(&self.playlist_path, m3u8)?;
        Ok(())
    }
}

impl Muxer for HlsSegmenter {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        self.streams = streams.to_vec();
        // Roll segments on the video stream's keyframes when there is one,
        // otherwise on the first stream by elapsed time.
        self.ref_stream = streams
            .iter()
            .position(|s| s.media_type == MediaType::Video)
            .unwrap_or(0);
        if let Some(s) = streams.get(self.ref_stream) {
            self.ref_tb = s.time_base;
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let is_ref = packet.stream_index == self.ref_stream;
        let has_video = self
            .streams
            .get(self.ref_stream)
            .map(|s| s.media_type == MediaType::Video)
            .unwrap_or(false);

        if is_ref {
            if let Some(pts) = packet.pts {
                let due = match self.seg_start_pts {
                    Some(start) => self.tb_seconds(pts - start) >= self.target_duration,
                    None => false,
                };
                // Roll over at a segment boundary, on a keyframe if this stream
                // is video (so the new segment is independently decodable).
                let at_boundary = due && (packet.flags.keyframe || !has_video);
                if at_boundary || self.current.is_none() {
                    self.close_segment(Some(pts))?;
                    self.open_segment()?;
                    self.seg_start_pts = Some(pts);
                }
                self.last_ref_pts = Some(pts);
            }
        }
        if self.current.is_none() {
            self.open_segment()?;
        }
        self.current
            .as_mut()
            .expect("segment open")
            .write_packet(packet)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.close_segment(None)?;
        self.write_playlist()?;
        Ok(())
    }
}
