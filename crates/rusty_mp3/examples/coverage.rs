//! `coverage` — a decoder-side SYNTAX COVERAGE census, plus a conformance score.
//!
//! ```text
//!   cargo run -p rusty_mp3 --release --example coverage -- a.mp3 b.bit dir/ ...
//! ```
//!
//! For every stream it runs the real header, side-info, scalefactor and Huffman
//! decoders and tallies each value the Layer III syntax can carry, on three axes:
//!
//! * **format** — MPEG version, sample rate, channel mode, MS / intensity flags,
//!   bitrate, CRC, emphasis, free format;
//! * **frequency** — every Huffman table actually reached (weighted by pairs
//!   decoded under it), both count1 tables, the escape (`linbits`) magnitudes, and
//!   the highest scalefactor band that carries a non-zero line, per block type;
//! * **decision** — block types, mixed blocks, `scfsi` bands, preflag,
//!   `scalefac_scale`, subblock gain, the six MPEG-2 scalefactor schemes, and the
//!   intensity-stereo positions.
//!
//! The aggregate report lists every cell that NO input reached. A path no input
//! reaches is a path no gate has ever tested — that list is the deliverable.
//!
//! If `<stem>.pcm` (interleaved s16le, the ISO / minimp3 reference convention)
//! sits beside an input, the stream is also decoded with [`Mp3Decoder`] and scored
//! against it: max |error| in s16 LSBs and SNR. Deterministic; one run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusty_mp3::decode::{huffman, reservoir::Reservoir, scalefactors, sideinfo};
use rusty_mp3::frame::{BlockType, ChannelMode, GranuleSideInfo, SideInfo, GRANULE_LINES};
use rusty_mp3::header::{FrameHeader, MpegVersion};
use rusty_mp3::{tables, Mp3Decoder};

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

/// `linbits` per pair table (ISO 11172-3 Table B.7); 0 = no escape.
const LINBITS: [u32; 32] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    1, 2, 3, 4, 6, 8, 10, 13, 4, 5, 6, 7, 8, 9, 11, 13,
];

type Tally = BTreeMap<String, u64>;

fn bump(t: &mut Tally, k: impl Into<String>, n: u64) {
    *t.entry(k.into()).or_default() += n;
}

/// Every cell the syntax allows, so the report can name the ones never reached.
fn universe() -> Vec<String> {
    let mut u: Vec<String> = Vec::new();
    for v in ["V1", "V2", "V2.5"] {
        u.push(format!("fmt.version.{v}"));
    }
    for r in [44100, 48000, 32000, 22050, 24000, 16000, 11025, 12000, 8000] {
        u.push(format!("fmt.rate.{r}"));
    }
    for m in ["mono", "stereo", "joint", "dual"] {
        u.push(format!("fmt.mode.{m}"));
    }
    for j in ["none", "ms", "is", "ms+is"] {
        u.push(format!("fmt.joint.{j}"));
    }
    u.push("fmt.crc".into());
    u.push("fmt.free_format".into());
    for e in 1..4 {
        u.push(format!("fmt.emphasis.{e}"));
    }
    for (i, &k) in tables::BITRATE_V1_L3.iter().enumerate().take(15).skip(1) {
        let _ = i;
        u.push(format!("fmt.bitrate.V1.{k}"));
    }
    for &k in tables::BITRATE_V2_L3.iter().take(15).skip(1) {
        u.push(format!("fmt.bitrate.LSF.{k}"));
    }
    for b in ["long", "start", "stop", "short", "mixed"] {
        u.push(format!("dec.block.{b}"));
    }
    for b in 0..4 {
        u.push(format!("dec.scfsi.band{b}"));
    }
    for f in ["preflag", "scalefac_scale", "subblock_gain"] {
        u.push(format!("dec.{f}"));
    }
    for s in 0..6 {
        u.push(format!("dec.lsf_scheme.{s}"));
    }
    u.push("dec.is.pos_legal".into());
    u.push("dec.is.pos_illegal".into());
    // Tables 4 and 14 do not exist in the standard.
    for t in (1..32).filter(|t| *t != 4 && *t != 14) {
        u.push(format!("freq.table.{t:02}"));
    }
    for t in 16..32 {
        u.push(format!("freq.escape.{t:02}"));
    }
    u.push("freq.count1.A".into());
    u.push("freq.count1.B".into());
    for b in 0..22 {
        u.push(format!("freq.top_sfb.long.{b:02}"));
    }
    for b in 0..13 {
        u.push(format!("freq.top_sfb.short.{b:02}"));
    }
    u
}

/// Big-value region boundaries -- the same rule as `decode::huffman`.
fn regions(gi: &GranuleSideInfo, rate: u32, bv2: usize) -> (usize, usize) {
    let sfb_long = tables::sfb_long_offsets(rate);
    if gi.window_switching && gi.block_type != BlockType::Long {
        let r0 = if gi.block_type == BlockType::Short && !gi.mixed_block {
            3 * tables::sfb_short_offsets(rate)[3] as usize
        } else {
            sfb_long[8] as usize
        };
        (r0.min(bv2), bv2)
    } else {
        let i1 = (gi.region0_count as usize + 1).min(22);
        let i2 = (gi.region0_count as usize + gi.region1_count as usize + 2).min(22);
        let r1 = (sfb_long[i1] as usize).min(bv2);
        let r2 = (sfb_long[i2] as usize).min(bv2).max(r1);
        (r1, r2)
    }
}

/// The MPEG-2 scalefactor scheme index (ISO 13818-3 2.4.3.2): three ranges of
/// `scalefac_compress` for ordinary channels, three more for the intensity-coded
/// right channel, which halves the field and uses a different partition.
fn lsf_scheme(sfc: u16, is_right: bool) -> usize {
    if is_right {
        match sfc >> 1 {
            0..=179 => 3,
            180..=243 => 4,
            _ => 5,
        }
    } else {
        match sfc {
            0..=399 => 0,
            400..=499 => 1,
            _ => 2,
        }
    }
}

fn granule(
    t: &mut Tally,
    h: &FrameHeader,
    si: &SideInfo,
    gr: usize,
    ch: usize,
    coeffs: &[i32; GRANULE_LINES],
    sf: &scalefactors::ScaleFactors,
) {
    let gi = &si.granules[gr][ch];
    let short = gi.window_switching && gi.block_type == BlockType::Short;
    let block = match (short, gi.mixed_block, gi.block_type) {
        (true, true, _) => "mixed",
        (true, false, _) => "short",
        (_, _, BlockType::Start) => "start",
        (_, _, BlockType::Stop) => "stop",
        _ => "long",
    };
    bump(t, format!("dec.block.{block}"), 1);
    if std::env::var_os("COVERAGE_GRANULES").is_some() {
        println!(
            "  granule gr={gr} ch={ch} block={block} mixflag={} sbg={:?} preflag={} sfs={} gain={} sfc={}",
            u8::from(gi.mixed_block),
            gi.subblock_gain,
            u8::from(gi.preflag),
            u8::from(gi.scalefac_scale),
            gi.global_gain,
            gi.scalefac_compress
        );
        if ch == 1 && scalefactors::is_intensity_right(h, ch) {
            let lim = scalefactors::lsf_intensity_illegal(gi);
            println!(
                "    is_pos long {:?}\n    illegal    {:?}",
                sf.long, lim.long
            );
        }
    }
    if gi.preflag {
        bump(t, "dec.preflag", 1);
    }
    if gi.scalefac_scale {
        bump(t, "dec.scalefac_scale", 1);
    }
    if gi.subblock_gain.iter().any(|&g| g != 0) {
        bump(t, "dec.subblock_gain", 1);
    }
    if h.version == MpegVersion::V1 {
        if gr == 1 {
            for b in 0..4 {
                if si.scfsi[ch][b] {
                    bump(t, format!("dec.scfsi.band{b}"), 1);
                }
            }
        }
    } else {
        let is_on = matches!(
            h.channel_mode,
            ChannelMode::JointStereo {
                intensity_stereo: true,
                ..
            }
        );
        bump(
            t,
            format!(
                "dec.lsf_scheme.{}",
                lsf_scheme(gi.scalefac_compress, is_on && ch == 1)
            ),
            1,
        );
    }

    // Huffman tables, weighted by the pairs actually decoded under each.
    let bv2 = (gi.big_values as usize * 2).min(GRANULE_LINES);
    let (r1, r2) = regions(gi, h.sample_rate, bv2);
    let spans = [(0, r1), (r1, r2), (r2, bv2)];
    for (reg, &(a, b)) in spans.iter().enumerate() {
        let tsel = gi.table_select[reg] as usize;
        if b > a && tsel != 0 {
            bump(t, format!("freq.table.{tsel:02}"), ((b - a) / 2) as u64);
            if LINBITS[tsel.min(31)] > 0 {
                let big = coeffs[a..b].iter().filter(|v| v.abs() >= 15).count() as u64;
                if big > 0 {
                    bump(t, format!("freq.escape.{tsel:02}"), big);
                }
            }
        }
    }
    let peak = coeffs[..bv2]
        .iter()
        .map(|v| v.unsigned_abs())
        .max()
        .unwrap_or(0);
    let bucket = match peak {
        0 => "0",
        1..=14 => "1-14",
        15..=270 => "15-270",
        271..=2062 => "271-2062",
        _ => "2063-8206",
    };
    bump(t, format!("info.peak_level.{bucket}"), 1);
    let nz_end = coeffs.iter().rposition(|&v| v != 0).map_or(0, |i| i + 1);
    if nz_end > bv2 {
        bump(
            t,
            if gi.count1table_select {
                "freq.count1.B"
            } else {
                "freq.count1.A"
            },
            1,
        );
    }

    // Highest scalefactor band carrying a non-zero line: the frequency reach.
    if nz_end > 0 {
        let last = nz_end - 1;
        if short && !gi.mixed_block {
            let s = tables::sfb_short_offsets(h.sample_rate);
            let line = last / 3; // short lines are window-interleaved per band
            let b = (0..13).rev().find(|&b| line >= s[b] as usize).unwrap_or(0);
            bump(t, format!("freq.top_sfb.short.{b:02}"), 1);
        } else {
            let l = tables::sfb_long_offsets(h.sample_rate);
            // Band 21 is [l[21], 576): the region above the last scaled band.
            let b = (0..22).rev().find(|&b| last >= l[b] as usize).unwrap_or(0);
            bump(t, format!("freq.top_sfb.long.{b:02}"), 1);
        }
    }

    // Intensity positions are the right channel's scalefactors (MPEG-1: 7 = off).
    if ch == 1 && h.version == MpegVersion::V1 {
        if let ChannelMode::JointStereo {
            intensity_stereo: true,
            ..
        } = h.channel_mode
        {
            let vals: Vec<u8> = if short {
                sf.short.iter().flatten().copied().collect()
            } else {
                sf.long.to_vec()
            };
            for v in vals {
                bump(
                    t,
                    if v == 7 {
                        "dec.is.pos_illegal"
                    } else {
                        "dec.is.pos_legal"
                    },
                    1,
                );
            }
        }
    }
}

/// Census one stream. Mirrors the decoder's own frame loop.
fn census(bytes: &[u8], t: &mut Tally) -> (u64, u64) {
    let (mut frames, mut skipped) = (0u64, 0u64);
    let mut res = Reservoir::default();
    let mut pos = 0usize;
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xFF || bytes[pos + 1] & 0xE0 != 0xE0 {
            pos += 1;
            continue;
        }
        let hb = [bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]];
        // Free format: the decoder refuses bitrate index 0, so count it here
        // straight from the raw header bits (Layer III = layer bits 01).
        if (hb[1] >> 1) & 3 == 1 && hb[2] >> 4 == 0 {
            bump(t, "fmt.free_format", 1);
            skipped += 1;
            pos += 1;
            continue;
        }
        let Ok(h) = FrameHeader::parse(hb) else {
            pos += 1;
            continue;
        };
        let size = h.frame_size();
        if size < 4 || pos + size > bytes.len() {
            break;
        }
        let crc = if h.crc_protected { 2 } else { 0 };
        let si_start = pos + 4 + crc;
        let main_start = si_start + h.side_info_len();
        let Ok(si) = sideinfo::parse(
            &h,
            &bytes[si_start.min(bytes.len())..main_start.min(bytes.len())],
        ) else {
            pos += 1;
            continue;
        };
        frames += 1;
        let v = match h.version {
            MpegVersion::V1 => "V1",
            MpegVersion::V2 => "V2",
            MpegVersion::V2_5 => "V2.5",
        };
        bump(t, format!("fmt.version.{v}"), 1);
        bump(t, format!("fmt.rate.{}", h.sample_rate), 1);
        let fam = if h.version == MpegVersion::V1 {
            "V1"
        } else {
            "LSF"
        };
        bump(t, format!("fmt.bitrate.{fam}.{}", h.bitrate_kbps), 1);
        let (mode, joint) = match h.channel_mode {
            ChannelMode::Mono => ("mono", None),
            ChannelMode::Stereo => ("stereo", None),
            ChannelMode::DualMono => ("dual", None),
            ChannelMode::JointStereo {
                ms_stereo,
                intensity_stereo,
            } => (
                "joint",
                Some(match (ms_stereo, intensity_stereo) {
                    (false, false) => "none",
                    (true, false) => "ms",
                    (false, true) => "is",
                    (true, true) => "ms+is",
                }),
            ),
        };
        bump(t, format!("fmt.mode.{mode}"), 1);
        if let Some(j) = joint {
            bump(t, format!("fmt.joint.{j}"), 1);
        }
        if h.crc_protected {
            bump(t, "fmt.crc", 1);
        }
        if h.emphasis != 0 {
            bump(t, format!("fmt.emphasis.{}", h.emphasis), 1);
        }

        let main = res.assemble(si.main_data_begin, &bytes[main_start..pos + size]);
        let mut bit = 0usize;
        let mut kept: [[scalefactors::ScaleFactors; 2]; 2] = Default::default();
        for gr in 0..h.version.granules() {
            for ch in 0..h.channel_mode.channels() {
                let gi = &si.granules[gr][ch];
                let start = bit;
                let prev = if gr == 1 {
                    Some(kept[0][ch].clone())
                } else {
                    None
                };
                let sf = scalefactors::decode(main, &mut bit, &h, &si, gr, ch, prev.as_ref());
                kept[gr][ch] = sf.clone();
                let end = start + gi.part2_3_length as usize;
                let (coeffs, _) = huffman::decode(main, &mut bit, end, &h, gi);
                bit = end;
                if gi.part2_3_length > 0 {
                    granule(t, &h, &si, gr, ch, &coeffs, &sf);
                }
            }
        }
        pos += size;
    }
    (frames, skipped)
}

/// Decode with the shipping decoder and score against an s16le reference.
fn score(bytes: &[u8], reference: &[u8]) -> String {
    let mut dec = Mp3Decoder::new();
    dec.push(bytes);
    dec.flush();
    let mut ours: Vec<i32> = Vec::new();
    let mut ch = 1usize;
    while let Ok(f) = dec.next_frame() {
        ch = f.channels.max(1) as usize;
        ours.extend(
            f.samples
                .iter()
                .map(|s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i32),
        );
    }
    let refs: Vec<i32> = reference
        .chunks_exact(2)
        .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
        .collect();
    if ours.is_empty() || refs.is_empty() {
        return format!(
            "NO OUTPUT (ours {} / ref {} samples)",
            ours.len(),
            refs.len()
        );
    }
    // Decoders legitimately differ in how many leading frames they emit (a frame
    // whose main_data_begin reaches before the stream start cannot be decoded;
    // some emit silence for it, some drop it). Search whole-granule offsets and
    // score the best alignment, reporting the offset -- an unaligned score would
    // read a framing convention as a decoding defect.
    let g = 576 * ch;
    let mut best = (f64::NEG_INFINITY, 0i64, 0i32, 0usize);
    for k in -8i64..=8 {
        let off = k * g as i64;
        let (a0, b0) = if off >= 0 {
            (off as usize, 0)
        } else {
            (0, (-off) as usize)
        };
        if a0 >= ours.len() || b0 >= refs.len() {
            continue;
        }
        let n = (ours.len() - a0).min(refs.len() - b0);
        let (mut sig, mut err, mut maxe) = (0f64, 0f64, 0i32);
        for i in 0..n {
            let e = ours[a0 + i] - refs[b0 + i];
            maxe = maxe.max(e.abs());
            err += f64::from(e).powi(2);
            sig += f64::from(refs[b0 + i]).powi(2);
        }
        let snr = if err == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (sig / err).log10()
        };
        if snr > best.0 {
            best = (snr, k, maxe, n);
        }
    }
    let (snr, k, maxe, n) = best;
    let off = if k == 0 {
        String::new()
    } else {
        format!("  offset {k:+} granules")
    };
    let len = if ours.len() == refs.len() {
        String::new()
    } else {
        format!(
            "  LENGTH ours {} ref {} (compared {n})",
            ours.len(),
            refs.len()
        )
    };
    let verdict = if maxe <= 1 { "PASS" } else { "FAIL" };
    format!("{verdict} max|e| {maxe} LSB  SNR {snr:.1} dB{off}{len}")
}

fn inputs(args: &[String]) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for a in args {
        let p = Path::new(a);
        if p.is_dir() {
            let mut d: Vec<PathBuf> = std::fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|q| matches!(q.extension().and_then(|e| e.to_str()), Some("mp3" | "bit")))
                .collect();
            d.sort();
            v.extend(d);
        } else {
            v.push(p.to_path_buf());
        }
    }
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: coverage <file.mp3|file.bit|dir> ...   (env COVERAGE_TALLY=1 prints every cell)");
        std::process::exit(2);
    }
    let mut total = Tally::new();
    let mut who: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in inputs(&args) {
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let mut t = Tally::new();
        let (frames, skipped) = census(&bytes, &mut t);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        // COVERAGE_DUMP=<dir>: also write our decode as interleaved s16le, so an
        // external script can compare it against more than one reference.
        if let Ok(dir) = std::env::var("COVERAGE_DUMP") {
            let mut dec = Mp3Decoder::new();
            dec.push(&bytes);
            dec.flush();
            let mut out: Vec<u8> = Vec::new();
            while let Ok(f) = dec.next_frame() {
                for s in &f.samples {
                    out.extend_from_slice(
                        &((s * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
                    );
                }
            }
            let _ = std::fs::write(
                Path::new(&dir).join(p.with_extension("pcm").file_name().unwrap()),
                out,
            );
        }
        let pcm = p.with_extension("pcm");
        let conf = match std::fs::read(&pcm) {
            Ok(r) if !r.is_empty() => score(&bytes, &r),
            _ => String::new(),
        };
        let skip = if skipped > 0 {
            format!("  ({skipped} free-format frames UNDECODABLE)")
        } else {
            String::new()
        };
        println!("{name:<46} {frames:>6} frames{skip}  {conf}");
        for (k, n) in t {
            who.entry(k.clone()).or_default().push(name.clone());
            bump(&mut total, k, n);
        }
    }

    println!("\n== cells reached by NO input (untested paths) ==");
    let missing: Vec<String> = universe()
        .into_iter()
        .filter(|k| !total.contains_key(k))
        .collect();
    for k in &missing {
        println!("  {k}");
    }
    println!(
        "  -- {} of {} cells unreached",
        missing.len(),
        universe().len()
    );

    if std::env::var("COVERAGE_TALLY").as_deref() == Ok("1") {
        println!("\n== every cell (count, inputs) ==");
        for (k, n) in &total {
            println!("  {k:<34} {n:>10}  {}", who[k].len());
        }
    }
}
