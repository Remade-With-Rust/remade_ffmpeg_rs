//! Shared measurement plumbing for the `video-tests` analyzers.
//!
//! One copy of the corpus reader, the manifest, the ffmpeg arm and the quality
//! metric, so VP9 and AV1 are measured the *same* way and their reports can be
//! read against each other. Only the codec arms differ per analyzer.

pub mod ffmpeg;
pub mod quality;
pub mod y4m;

use std::path::{Path, PathBuf};

/// One row of `manifest.tsv` whose pixels are present on disk.
pub struct ClipEntry {
    pub name: String,
    pub class: String,
    pub path: PathBuf,
}

/// `video-tests/`, found by walking up from the analyzer's own directory.
pub fn root() -> PathBuf {
    let mut p = std::env::current_dir().expect("cwd");
    // Analyzers run from `video-tests/<analyzer-dir>/`.
    if p.file_name()
        .map(|n| n.to_string_lossy().starts_with("analyzer"))
        .unwrap_or(false)
    {
        p.pop();
    }
    p
}

/// Where the pixels live.
///
/// Defaults to the `rs_h264` checkout's corpus rather than a private copy, so
/// every codec in the family is measured on byte-identical input and the numbers
/// are comparable across repositories. `CLIPS_DIR` overrides; a local
/// `video-tests/clips` wins if present.
pub fn clips_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLIPS_DIR") {
        return PathBuf::from(d);
    }
    let local = root().join("clips");
    if local.is_dir() {
        return local;
    }
    root()
        .join("..")
        .join("..")
        .join("rs_h264")
        .join("video-tests")
        .join("clips")
}

/// Parse `manifest.tsv`, honouring the `CLIPS=a,b` filter and skipping clips
/// whose pixels are absent.
pub fn manifest() -> Vec<ClipEntry> {
    let base = root();
    let txt = std::fs::read_to_string(base.join("manifest.tsv"))
        .unwrap_or_else(|e| panic!("read manifest.tsv: {e}"));
    let filter: Vec<String> = std::env::var("CLIPS")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let dir = clips_dir();
    txt.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 6 {
                return None;
            }
            let name = f[0].to_string();
            if !filter.is_empty() && !filter.contains(&name) {
                return None;
            }
            let path = dir.join(format!("{name}.y4m"));
            if !path.exists() {
                eprintln!("  ! missing clip {name} — skipped");
                return None;
            }
            Some(ClipEntry {
                name,
                class: f[5].to_string(),
                path,
            })
        })
        .collect()
}

/// Frames to measure per clip. `FRAMES=0` means the whole clip.
///
/// The default caps a full-corpus run at minutes rather than hours while still
/// crossing every resolution rung.
pub fn frames_per_clip(clip_len: usize, default: usize) -> usize {
    let want = std::env::var("FRAMES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default);
    if want == 0 {
        clip_len
    } else {
        want.min(clip_len)
    }
}

/// `video-tests/results/<codec>/`, created on demand. Per-codec so a VP9 run and
/// an AV1 run never overwrite each other's TSVs.
pub fn results_dir(codec: &str) -> PathBuf {
    let d = root().join("results").join(codec);
    std::fs::create_dir_all(&d).expect("create results dir");
    d
}

pub fn scratch(codec: &str) -> PathBuf {
    let d = results_dir(codec).join("_tmp");
    std::fs::create_dir_all(&d).expect("create scratch dir");
    d
}

/// Wrap raw codec packets in an IVF container so external tools can be pointed
/// at exactly the stream we produced. `fourcc` is `VP90` or `AV01`.
pub fn to_ivf(
    fourcc: &[u8; 4],
    width: usize,
    height: usize,
    fps: (u32, u32),
    packets: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(packets.iter().map(|p| p.len() + 12).sum::<usize>() + 32);
    out.extend_from_slice(b"DKIF");
    out.extend_from_slice(&0u16.to_le_bytes()); // version
    out.extend_from_slice(&32u16.to_le_bytes()); // header length
    out.extend_from_slice(fourcc);
    out.extend_from_slice(&(width as u16).to_le_bytes());
    out.extend_from_slice(&(height as u16).to_le_bytes());
    out.extend_from_slice(&fps.0.to_le_bytes()); // time base denominator
    out.extend_from_slice(&fps.1.to_le_bytes()); // time base numerator
    out.extend_from_slice(&(packets.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (i, p) in packets.iter().enumerate() {
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(&(i as u64).to_le_bytes());
        out.extend_from_slice(p);
    }
    out
}

/// Split an IVF back into its frame payloads.
pub fn read_ivf(path: &Path) -> Vec<Vec<u8>> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    if data.len() < 32 || &data[0..4] != b"DKIF" {
        return Vec::new();
    }
    let hdr = u16::from_le_bytes([data[6], data[7]]) as usize;
    let mut out = Vec::new();
    let mut pos = hdr;
    while pos + 12 <= data.len() {
        let sz =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 12;
        if pos + sz > data.len() {
            break;
        }
        out.push(data[pos..pos + sz].to_vec());
        pos += sz;
    }
    out
}

// ---------------------------------------------------------------------------
// The two row shapes every analyzer emits.
// ---------------------------------------------------------------------------

/// One measured (clip x codec x kind) throughput result.
#[derive(Clone)]
pub struct Row {
    pub clip: String,
    pub class: String,
    pub width: usize,
    pub height: usize,
    pub frames: usize,
    pub codec: String,
    /// `encode` | `decode`
    pub kind: String,
    /// Which bitstream a decode row consumed (`-` for encode rows).
    pub source: String,
    pub wall_ms: f64,
    pub fps: f64,
    pub mpx_s: f64,
    pub bytes: usize,
    pub kbps: f64,
    pub psnr: f64,
    pub ssim: f64,
}

/// One stage/function bucket from a profiler.
#[derive(Clone)]
pub struct StageRow {
    pub clip: String,
    pub codec: String,
    pub kind: String,
    pub stage: String,
    /// `self` (exclusive — these sum to 100%), `incl` (contains children), or
    /// `info` (a diagnostic deliberately outside the partition).
    pub scope: String,
    pub ms: f64,
    pub calls: u64,
    pub pct: f64,
}

impl Row {
    pub fn new(c: &ClipEntry, clip: &y4m::Clip, frames: usize, codec: &str, kind: &str) -> Row {
        Row {
            clip: c.name.clone(),
            class: c.class.clone(),
            width: clip.width,
            height: clip.height,
            frames,
            codec: codec.into(),
            kind: kind.into(),
            source: "-".into(),
            wall_ms: 0.0,
            fps: 0.0,
            mpx_s: 0.0,
            bytes: 0,
            kbps: 0.0,
            psnr: f64::NAN,
            ssim: f64::NAN,
        }
    }

    pub fn fill(
        &mut self,
        clip: &y4m::Clip,
        frames: usize,
        d: std::time::Duration,
        bytes: usize,
    ) {
        let secs = d.as_secs_f64().max(1e-9);
        self.wall_ms = secs * 1e3;
        self.fps = frames as f64 / secs;
        self.mpx_s = (clip.width * clip.height * frames) as f64 / secs / 1e6;
        self.bytes = bytes;
        let dur = frames as f64 / clip.fps_f64().max(1e-9);
        self.kbps = if bytes > 0 && dur > 0.0 {
            bytes as f64 * 8.0 / dur / 1e3
        } else {
            0.0
        };
    }
}

/// Write the throughput TSV. Called after EVERY clip: a full-corpus run is tens
/// of minutes, and writing only at the end means one transient failure throws
/// away every clip that already succeeded.
pub fn write_rows(path: &Path, rows: &[Row]) {
    let mut s = String::from(
        "clip\tclass\twidth\theight\tframes\tcodec\tkind\tsource\twall_ms\tfps\tmpx_s\tbytes\tkbps\tpsnr\tssim\n",
    );
    for r in rows {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.2}\t{:.2}\t{:.3}\t{}\t{:.1}\t{:.3}\t{:.5}\n",
            r.clip, r.class, r.width, r.height, r.frames, r.codec, r.kind, r.source,
            r.wall_ms, r.fps, r.mpx_s, r.bytes, r.kbps, r.psnr, r.ssim
        ));
    }
    std::fs::write(path, s).expect("write rows");
}

pub fn write_stages(path: &Path, rows: &[StageRow]) {
    let mut s = String::from("clip\tcodec\tkind\tstage\tscope\tms\tcalls\tns_per_call\tpct\n");
    for r in rows {
        let nspc = if r.calls > 0 {
            r.ms * 1e6 / r.calls as f64
        } else {
            0.0
        };
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{:.4}\t{}\t{:.1}\t{:.2}\n",
            r.clip, r.codec, r.kind, r.stage, r.scope, r.ms, r.calls, nspc, r.pct
        ));
    }
    std::fs::write(path, s).expect("write stages");
}

/// Median-of-N over a profiler snapshot.
///
/// Median rather than best-of-N: a stage table is a vector of correlated
/// numbers, and taking the minimum of each independently would produce a table
/// summing to less than any single pass actually took.
pub fn profile_median<T: Clone>(
    mut run: impl FnMut(),
    passes: usize,
    reset: impl Fn(),
    snapshot: impl Fn() -> Vec<(T, f64, u64)>,
) -> Vec<(T, f64, u64)> {
    let passes = passes.max(1);
    let mut per: Vec<Vec<(T, f64, u64)>> = Vec::with_capacity(passes);
    for _ in 0..passes {
        reset();
        run();
        per.push(snapshot());
    }
    let mid = passes / 2;
    let n = per.iter().map(|p| p.len()).min().unwrap_or(0);
    (0..n)
        .map(|i| {
            let mut ms: Vec<f64> = per.iter().map(|s| s[i].1).collect();
            ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (per[mid][i].0.clone(), ms[mid], per[mid][i].2)
        })
        .collect()
}
