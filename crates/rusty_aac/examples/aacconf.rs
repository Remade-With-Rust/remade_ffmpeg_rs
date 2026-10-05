//! `aacconf` — the decoder CONFORMANCE census: every stream in a corpus directory,
//! decoded by rusty_aac and scored against two independent references.
//!
//! ```text
//! cargo run -p rusty_aac --release --example aacconf -- <corpus_dir> [--gen] [--filter S] [-v]
//! ```
//!
//! * Reference A — FFmpeg's own decode of the same file (`-f s16le -flags +bitexact`).
//! * Reference B — the FATE reference `<stem>.s16` beside the stream, when present
//!   (the ISO 14496-26 conformance outputs FFmpeg itself is gated on).
//!
//! Demuxing goes through FFmpeg (`ffprobe` packet sizes + `-c copy -f data`
//! payload + the stream's extradata), so the census measures the CODEC, not our
//! MP4/LATM readers. Alignment is a bounded lag search, and the lag is printed:
//! a non-zero lag must be explainable (an edit list trimming priming samples),
//! never silently absorbed.
//!
//! `--gen` first synthesises the cells the FATE corpus does not cover (every
//! sampling rate, every channel configuration, each LC tool switched on and off)
//! with FFmpeg's encoder into `<corpus_dir>/gen/`.
//!
//! Verdicts, per reference: `EXACT` max |diff| <= 1 LSB (s16), `NEAR` SNR >= 70 dB,
//! `FAIL` otherwise; `ERR <msg>` when the decoder refused the stream.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusty_aac::latm::{find_sync, LatmDecoder};
use rusty_aac::AacDecoder;

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

const MAX_LAG: i64 = 4096;

fn run(cmd: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "{cmd} exit {:?}: {}",
            out.status.code(),
            err.lines().last().unwrap_or("")
        ));
    }
    Ok(out.stdout)
}

struct Probe {
    codec: String,
    profile: String,
    rate: u32,
    channels: u32,
}

fn probe(f: &str) -> Result<Probe, String> {
    let o = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,profile,sample_rate,channels",
            "-of",
            "csv=p=0",
            f,
        ],
    )?;
    let s = String::from_utf8_lossy(&o);
    let v: Vec<&str> = s.lines().next().unwrap_or("").split(',').collect();
    if v.len() < 4 {
        return Err(format!("ffprobe: unexpected '{s}'"));
    }
    Ok(Probe {
        codec: v[0].to_string(),
        profile: v[1].to_string(),
        rate: v[2].parse().unwrap_or(0),
        channels: v[3].parse().unwrap_or(0),
    })
}

/// The stream's extradata (the AudioSpecificConfig for MP4/3GP), from
/// `ffprobe -show_data`'s hexdump.
fn extradata(f: &str) -> Result<Vec<u8>, String> {
    let o = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_streams",
            "-show_data",
            f,
        ],
    )?;
    let s = String::from_utf8_lossy(&o);
    let mut size = 0usize;
    let mut bytes = Vec::new();
    let mut inside = false;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("extradata_size=") {
            size = v.trim().parse().unwrap_or(0);
        }
        if line.starts_with("extradata=") {
            inside = true;
            continue;
        }
        if inside {
            let l = line.trim_start();
            if l.len() > 10
                && l.as_bytes()[8] == b':'
                && l[..8].bytes().all(|b| b.is_ascii_hexdigit())
            {
                for grp in l[10..].split(' ').take(8) {
                    let g = grp.trim();
                    if g.is_empty() || !g.bytes().all(|b| b.is_ascii_hexdigit()) {
                        break;
                    }
                    let mut i = 0;
                    while i + 2 <= g.len() {
                        bytes.push(u8::from_str_radix(&g[i..i + 2], 16).unwrap());
                        i += 2;
                    }
                }
            } else {
                inside = false;
            }
        }
    }
    bytes.truncate(size);
    Ok(bytes)
}

/// Compressed packets exactly as the demuxer emits them.
fn packets(f: &str) -> Result<Vec<Vec<u8>>, String> {
    let sizes = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=size",
            "-of",
            "csv=p=0",
            f,
        ],
    )?;
    let data = run(
        "ffmpeg",
        &[
            "-v", "error", "-i", f, "-map", "0:a:0", "-c", "copy", "-f", "data", "-",
        ],
    )?;
    let mut out = Vec::new();
    let mut pos = 0usize;
    for l in String::from_utf8_lossy(&sizes).lines() {
        let n: usize = match l.trim().trim_end_matches(',').parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if pos + n > data.len() {
            return Err(format!(
                "packet sizes overrun payload ({} > {})",
                pos + n,
                data.len()
            ));
        }
        out.push(data[pos..pos + n].to_vec());
        pos += n;
    }
    if pos != data.len() {
        return Err(format!("payload {} != sum of sizes {}", data.len(), pos));
    }
    Ok(out)
}

struct Decoded {
    rate: u32,
    channels: u32,
    pcm: Vec<f32>,
    errors: usize,
    first_err: Option<String>,
    /// Later configuration segments: (start frame, channels, rate, pcm).
    segments: Vec<(usize, u32, u32, Vec<f32>)>,
    frames: usize,
}

fn decode_ours(p: &Probe, asc: &[u8], pkts: &[Vec<u8>]) -> Result<Decoded, String> {
    let mut d = Decoded {
        rate: 0,
        channels: 0,
        pcm: Vec::new(),
        errors: 0,
        first_err: None,
        segments: Vec::new(),
        frames: 0,
    };
    let push = |d: &mut Decoded, r: rusty_aac::Result<rusty_aac::DecodedAudio>| match r {
        Ok(a) => {
            if d.channels == 0 {
                d.channels = u32::from(a.channels);
                d.rate = a.sample_rate;
            }
            let n = a.frames();
            if u32::from(a.channels) == d.channels && d.segments.is_empty() {
                d.pcm.extend_from_slice(&a.samples);
            } else {
                let start = d.frames;
                match d.segments.last_mut() {
                    Some(seg) if seg.1 == u32::from(a.channels) && seg.2 == a.sample_rate => {
                        seg.3.extend_from_slice(&a.samples);
                    }
                    _ => d.segments.push((
                        start,
                        u32::from(a.channels),
                        a.sample_rate,
                        a.samples.clone(),
                    )),
                }
            }
            d.frames += n;
        }
        Err(rusty_aac::Error::Again) => {}
        Err(e) => {
            d.errors += 1;
            if d.first_err.is_none() {
                d.first_err = Some(format!("{e}"));
            }
        }
    };
    if p.codec == "aac_latm" {
        let mut dec = LatmDecoder::new();
        for pk in pkts {
            let mut off = find_sync(pk).unwrap_or(0);
            while off < pk.len() {
                match dec.decode(&pk[off..], None) {
                    Ok((a, used)) => {
                        push(&mut d, Ok(a));
                        off += used.max(1);
                    }
                    Err(e) => {
                        push(&mut d, Err(e));
                        break;
                    }
                }
            }
        }
    } else {
        let mut dec = if asc.is_empty() {
            AacDecoder::new()
        } else {
            AacDecoder::with_config_bytes(asc).map_err(|e| format!("config: {e}"))?
        };
        for pk in pkts {
            let r = dec.decode(pk, None);
            push(&mut d, r);
        }
    }
    Ok(d)
}

fn to_s16(x: f32) -> i32 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i32
}

struct Score {
    lag: i64,
    max: i32,
    snr: f64,
    n: usize,
    len_ours: usize,
    len_ref: usize,
}

/// Best lag (ours shifted by `lag` frames against ref), then full-overlap metrics.
fn score(ours: &[i32], refr: &[i32], ch: usize) -> Option<Score> {
    let fo = ours.len() / ch;
    let fr = refr.len() / ch;
    if fo == 0 || fr == 0 {
        return None;
    }
    // Pick a probe window with energy in the reference.
    let win = 4096usize.min(fr);
    let mut best = (i64::MAX as f64, 0i64);
    let mut start = 0usize;
    let mut best_e = -1f64;
    let mut s = 0;
    while s + win <= fr {
        let e: f64 = (s..s + win).map(|i| f64::from(refr[i * ch]).abs()).sum();
        if e > best_e {
            best_e = e;
            start = s;
        }
        s += win;
    }
    for lag in -MAX_LAG..=MAX_LAG {
        let mut acc = 0f64;
        let mut cnt = 0usize;
        for i in (start..start + win).step_by(2) {
            let j = i as i64 + lag;
            if j < 0 || j as usize >= fo {
                continue;
            }
            acc += f64::from((ours[j as usize * ch] - refr[i * ch]).abs());
            cnt += 1;
        }
        if cnt > win / 4 {
            let m = acc / cnt as f64;
            if m < best.0 {
                best = (m, lag);
            }
        }
    }
    let lag = best.1;
    let (mut max, mut se, mut sn, mut n) = (0i32, 0f64, 0f64, 0usize);
    for i in 0..fr {
        let j = i as i64 + lag;
        if j < 0 || j as usize >= fo {
            continue;
        }
        for c in 0..ch {
            let a = ours[j as usize * ch + c];
            let b = refr[i * ch + c];
            let e = (a - b).abs();
            max = max.max(e);
            se += f64::from(e) * f64::from(e);
            sn += f64::from(b) * f64::from(b);
            n += 1;
        }
    }
    let snr = if se == 0.0 {
        999.0
    } else {
        10.0 * (sn / se).log10()
    };
    Some(Score {
        lag,
        max,
        snr,
        n,
        len_ours: fo,
        len_ref: fr,
    })
}

/// For a multichannel FAIL: which reference channel each of ours matches best
/// (SNR at the given lag) — a permutation here is a channel-order fault.
fn chan_matrix(ours: &[i32], refr: &[i32], ch: usize, lag: i64) -> String {
    let (fo, fr) = (ours.len() / ch, refr.len() / ch);
    let mut out = String::new();
    for i in 0..ch {
        let mut best = (f64::MIN, 0usize);
        for j in 0..ch {
            let (mut se, mut sn) = (0f64, 0f64);
            for t in 0..fr {
                let u = t as i64 + lag;
                if u < 0 || u as usize >= fo {
                    continue;
                }
                let a = f64::from(ours[u as usize * ch + i]);
                let b = f64::from(refr[t * ch + j]);
                se += (a - b) * (a - b);
                sn += b * b;
            }
            let snr = if se == 0.0 {
                999.0
            } else {
                10.0 * (sn / se).log10()
            };
            if snr > best.0 {
                best = (snr, j);
            }
        }
        out.push_str(&format!(" ours{i}->ref{}({:.0}dB)", best.1, best.0));
    }
    out
}

/// Streams where FFmpeg 8.1.2 itself deviates, each with the reason and the
/// mapping under which our output must still be sample-exact.
/// `perm[i]` = the FFmpeg channel that our channel `i` must equal; channels with
/// `None` are excluded (FFmpeg's content there is not decoded audio).
struct Known {
    stream: &'static str,
    perm: &'static [Option<usize>],
    reason: &'static str,
}

const KNOWN: &[Known] = &[
    Known {
        stream: "al22_chCfg0PCE_44.mp4",
        perm: &[Some(6), Some(7), Some(2), Some(3), Some(4), Some(5), Some(0), Some(1)],
        reason: "PCE height extension (CRC-valid) puts the 3rd front pair on top; FFmpeg 8.1.2 ignores heights and swaps it with the FLC/FRC pair",
    },
    Known {
        stream: "aac-sce-in-stereo.mp4",
        perm: &[Some(0), None],
        reason: "stereo config carrying one SCE: FFmpeg 8.1.2 leaves channel 2 as an uninitialised buffer (no relation to any decoded signal); we emit silence",
    },
];

fn apply_known(k: &Known, ours: &[i32], refr: &[i32], ch: usize) -> (Vec<i32>, Vec<i32>) {
    let n = (ours.len() / ch).max(refr.len() / ch);
    let mut o = vec![0i32; ours.len()];
    let mut r = vec![0i32; refr.len()];
    for t in 0..n {
        for (i, m) in k.perm.iter().enumerate() {
            if let Some(j) = m {
                if t * ch + i < ours.len() {
                    o[t * ch + i] = ours[t * ch + i];
                }
                if t * ch + j < refr.len() && t * ch + i < r.len() {
                    r[t * ch + i] = refr[t * ch + j];
                }
            }
        }
    }
    (o, r)
}

fn verdict(s: &Option<Score>) -> String {
    match s {
        None => "-".into(),
        Some(s) if s.n == 0 => "NO-OVERLAP".into(),
        Some(s) => {
            let v = if s.max <= 1 {
                "EXACT"
            } else if s.snr >= 70.0 {
                "NEAR"
            } else {
                "FAIL"
            };
            format!(
                "{v:<5} lag={:<5} max={:<5} snr={:>6.1} len {}/{}",
                s.lag, s.max, s.snr, s.len_ours, s.len_ref
            )
        }
    }
}

fn read_s16(p: &Path) -> Option<Vec<i32>> {
    let b = fs::read(p).ok()?;
    Some(
        b.chunks_exact(2)
            .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
            .collect(),
    )
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    v.sort();
    for p in v {
        if p.is_dir() {
            collect(&p, out);
        } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            if ["mp4", "m4a", "3gp", "aac", "mpg", "ts"].contains(&ext) {
                out.push(p);
            }
        }
    }
}

/// Synthesise the cells FATE does not cover, with FFmpeg's native encoder.
fn generate(dir: &Path) {
    let g = dir.join("gen");
    let _ = fs::create_dir_all(&g);
    // Tone + noise bursts (transients for block switching/TNS) + a noise bed
    // (PNS) + per-channel variation (M/S vs L/R vs IS decisions).
    let expr = |ch: usize| {
        (0..ch)
            .map(|c| {
                format!(
                    "0.25*sin(2*PI*{}*t)+0.15*sin(2*PI*{}*t)*(1+sin(2*PI*0.5*t))+0.3*random({c})*lt(mod(t+{},0.37),0.01)+0.02*random({})",
                    220 + 110 * c,
                    3000 + 700 * c,
                    c as f64 * 0.05,
                    c + 10
                )
            })
            .collect::<Vec<_>>()
            .join("|")
    };
    let mut jobs: Vec<(String, u32, usize, Vec<&str>)> = Vec::new();
    for &r in &[
        96000u32, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ] {
        for ch in [1usize, 2] {
            jobs.push((format!("lc_{r}_{ch}ch"), r, ch, vec![]));
        }
    }
    for ch in [3usize, 4, 5, 6, 8] {
        jobs.push((format!("lc_44100_{ch}ch"), 44100, ch, vec![]));
    }
    jobs.push(("lc_44100_6ch_pce".into(), 44100, 6, vec!["-aac_pce", "1"]));
    for (name, opts) in [
        ("notns", vec!["-aac_tns", "0"]),
        ("nopns", vec!["-aac_pns", "0"]),
        ("nois", vec!["-aac_is", "0"]),
        ("msforce", vec!["-aac_ms", "1"]),
        ("fastcoder", vec!["-aac_coder", "fast"]),
        ("lowrate", vec!["-b:a", "32k"]),
        ("highrate", vec!["-b:a", "320k"]),
    ] {
        jobs.push((format!("lc_44100_2ch_{name}"), 44100, 2, opts));
    }
    for (name, rate, ch, opts) in jobs {
        let out = g.join(format!("{name}.aac"));
        if out.exists() {
            continue;
        }
        let layout = match ch {
            1 => "mono",
            2 => "stereo",
            3 => "3.0",
            4 => "4.0",
            5 => "5.0",
            6 => "5.1",
            _ => "7.1",
        };
        let src = format!("aevalsrc='{}':s={rate}:c={layout}:d=4", expr(ch));
        let mut args: Vec<String> = vec![
            "-v".into(),
            "error".into(),
            "-y".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            src,
            "-c:a".into(),
            "aac".into(),
        ];
        args.extend(opts.iter().map(std::string::ToString::to_string));
        args.push(out.to_string_lossy().into_owned());
        let a: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
        if let Err(e) = run("ffmpeg", &a) {
            eprintln!("gen {name}: {e}");
        }
    }
}

/// One stream's census line(s) and its category ('E'/'N'/'F', ' ' = skipped).
fn process(f: &Path, name: &str, verbose: bool) -> (String, char) {
    let mut out = String::new();
    macro_rules! outln {
        ($($t:tt)*) => {{ out.push_str(&format!($($t)*)); out.push('\n'); }};
    }
    let fs_ = f.to_string_lossy().into_owned();
    let p = match probe(&fs_) {
        Ok(p) => p,
        Err(e) => {
            outln!("{name:<36} PROBE-ERR {e}");
            return (out, ' ');
        }
    };
    let asc = extradata(&fs_).unwrap_or_default();
    let pkts = match packets(&fs_) {
        Ok(p) => p,
        Err(e) => {
            outln!("{name:<36} {:<11} DEMUX-ERR {e}", p.profile);
            return (out, 'F');
        }
    };
    let ours = decode_ours(&p, &asc, &pkts);
    let refa = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-flags",
            "+bitexact",
            "-i",
            &fs_,
            "-map",
            "0:a:0",
            "-f",
            "s16le",
            "-",
        ],
    )
    .ok()
    .map(|b| {
        b.chunks_exact(2)
            .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
            .collect::<Vec<i32>>()
    });
    // FATE keeps the ISO-order output as `<stem>.s16` and FFmpeg's output order
    // (the one we emit) as `<stem>_reorder.s16` where they differ.
    let stem = f.with_extension("");
    let refb = read_s16(Path::new(&format!(
        "{}_reorder.s16",
        stem.to_string_lossy()
    )))
    .or_else(|| {
        // `_ep0` streams: FATE's reference is the one without the suffix.
        let s = stem.to_string_lossy();
        let t = s.trim_end_matches("_ep0");
        if t.len() != s.len() {
            read_s16(Path::new(&format!("{t}.s16")))
        } else {
            None
        }
    })
    .or_else(|| read_s16(&f.with_extension("s16")));
    let (status, va, vb) = match &ours {
        Err(e) => (format!("ERR {e}"), "-".to_string(), "-".to_string()),
        Ok(d) if d.pcm.is_empty() => (
            format!(
                "ERR {}",
                d.first_err.clone().unwrap_or_else(|| "no output".into())
            ),
            "-".into(),
            "-".into(),
        ),
        Ok(d) => {
            let o: Vec<i32> = d.pcm.iter().map(|&x| to_s16(x)).collect();
            if let Ok(dir) = std::env::var("AACCONF_DUMP") {
                let bytes: Vec<u8> = o.iter().flat_map(|&v| (v as i16).to_le_bytes()).collect();
                let _ = fs::write(
                    Path::new(&dir).join(format!("{}.s16", name.replace('/', "_"))),
                    bytes,
                );
            }
            let ch = d.channels.max(1) as usize;
            let sa = if d.channels == p.channels {
                refa.as_ref().and_then(|r| score(&o, r, ch))
            } else {
                None
            };
            let sb = if d.channels == p.channels {
                refb.as_ref().and_then(|r| score(&o, r, ch))
            } else {
                None
            };
            let mut st = format!("{}Hz/{}", d.rate, d.channels);
            if d.errors > 0 {
                st.push_str(&format!(" e{}", d.errors));
            }
            let known = KNOWN
                .iter()
                .find(|k| name.ends_with(k.stream) && k.perm.len() == ch);
            let sa = match (known, refa.as_ref()) {
                (Some(k), Some(r)) if d.channels == p.channels => {
                    let (ko, kr) = apply_known(k, &o, r, ch);
                    score(&ko, &kr, ch)
                }
                _ => sa,
            };
            if std::env::var("AACCONF_DEBUG").is_ok() {
                eprintln!(
                    "dbg {name} known={} refa={:?} sa={}",
                    known.is_some(),
                    refa.as_ref().map(std::vec::Vec::len),
                    sa.is_some()
                );
            }
            let mut va = verdict(&sa);
            // Configuration changes mid-stream: score each later segment against
            // FFmpeg forced to that segment's layout (a matching layout passes
            // through its resampler unchanged).
            for (start, sch, srate, spcm) in &d.segments {
                let ac = sch.to_string();
                let ar = srate.to_string();
                let r = run(
                    "ffmpeg",
                    &[
                        "-v",
                        "error",
                        "-flags",
                        "+bitexact",
                        "-i",
                        &fs_,
                        "-map",
                        "0:a:0",
                        "-ac",
                        &ac,
                        "-ar",
                        &ar,
                        "-f",
                        "s16le",
                        "-",
                    ],
                )
                .ok()
                .map(|b| {
                    b.chunks_exact(2)
                        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
                        .collect::<Vec<i32>>()
                });
                let so: Vec<i32> = spcm.iter().map(|&x| to_s16(x)).collect();
                let c = *sch as usize;
                let tail = r.map(|r| r.get(start * c..).map(<[i32]>::to_vec).unwrap_or_default());
                let sc = tail.as_ref().and_then(|t| score(&so, t, c));
                let v = verdict(&sc);
                va = if v.starts_with("EXACT") && va.starts_with("EXACT") {
                    format!("{va} +seg@{start} {c}ch EXACT")
                } else {
                    format!("FAIL seg@{start} {c}ch: {v} | first: {va}")
                };
            }
            if known.is_some() && va.starts_with("EXACT") {
                va = va.replacen("EXACT", "KNOWN", 1);
            }
            if verbose && va.starts_with("FAIL") && ch > 1 {
                if let (Some(r), Some(sc)) = (refa.as_ref(), sa.as_ref()) {
                    va.push_str(&chan_matrix(&o, r, ch, sc.lag));
                }
            }
            if d.channels != p.channels || d.rate != p.rate {
                va = format!("MISMATCH want {}Hz/{}", p.rate, p.channels);
            }
            (st, va, verdict(&sb))
        }
    };
    let cat = if va.starts_with("EXACT") {
        'E'
    } else if va.starts_with("KNOWN") {
        'K'
    } else if va.starts_with("NEAR") {
        'N'
    } else {
        'F'
    };
    outln!(
        "{name:<36} {:<11} {:>12} | {status:<14} | {va:<58} | {vb}",
        p.profile,
        format!("{}/{}", p.rate, p.channels)
    );
    if verbose {
        if let Ok(d) = &ours {
            if let Some(e) = &d.first_err {
                outln!(
                    "    first error: {e}  (asc {:02x?}, {} packets)",
                    asc,
                    pkts.len()
                );
            }
        }
    }
    (out, cat)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else {
        eprintln!("usage: aacconf <corpus_dir> [--gen] [--filter S] [-v]");
        std::process::exit(2);
    };
    let dir = Path::new(dir);
    let verbose = args.iter().any(|a| a == "-v");
    let filter = args
        .windows(2)
        .find(|w| w[0] == "--filter")
        .map(|w| w[1].clone());
    if args.iter().any(|a| a == "--gen") {
        generate(dir);
    }
    let mut files = Vec::new();
    collect(dir, &mut files);
    println!(
        "# aacconf — decoder census, allocator rusty_alloc; verdict EXACT=max<=1 LSB, NEAR=SNR>=70dB"
    );
    println!(
        "{:<36} {:<11} {:>12} | {:<14} | {:<58} | REF(.s16)",
        "stream", "profile", "rate/ch", "ours", "vs FFmpeg"
    );
    let names: Vec<(PathBuf, String)> = files
        .into_iter()
        .map(|f| {
            let name = f
                .strip_prefix(dir)
                .unwrap_or(&f)
                .to_string_lossy()
                .replace('\\', "/");
            (f, name)
        })
        .filter(|(_, n)| {
            filter
                .as_ref()
                .map(|fl| n.contains(fl.as_str()))
                .unwrap_or(true)
        })
        .collect();
    let results = std::sync::Mutex::new(vec![None; names.len()]);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4)
        .min(8);
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= names.len() {
                    break;
                }
                let r = process(&names[i].0, &names[i].1, verbose);
                results.lock().unwrap()[i] = Some(r);
            });
        }
    });
    let (mut exact, mut near, mut fail, mut total, mut known) = (0, 0, 0, 0, 0);
    for (text, cat) in results.into_inner().unwrap().into_iter().flatten() {
        print!("{text}");
        match cat {
            'E' => exact += 1,
            'K' => known += 1,
            'N' => near += 1,
            'F' => fail += 1,
            _ => continue,
        }
        total += 1;
    }
    println!("\n# {total} streams: {exact} EXACT, {known} KNOWN (exact under a documented FFmpeg deviation), {near} NEAR, {fail} FAIL/ERR");
    for k in KNOWN {
        println!("#   KNOWN {}: {}", k.stream, k.reason);
    }
}
