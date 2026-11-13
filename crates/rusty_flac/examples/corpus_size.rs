//! Compression gate over a corpus of real audio: FLAC bytes per file per
//! level, each stream decoded back and checked bit-exact.
//!
//! Usage:
//!   corpus_size <file.wav|file.flac>... [--levels 0,5,8]
//!
//! Accepts PCM WAV (8/16/24-bit, any channel count) and FLAC (decoded with
//! this crate). Prints one row per file and level, then totals; run it on two
//! builds and compare the rows — a change to the encoder's analysis math is
//! gated on no file growing beyond noise and the totals not growing.

// Project convention: encoder benches run under rusty_alloc (what ships).
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

struct Pcm {
    rate: u32,
    channels: u32,
    bps: u32,
    planes: Vec<Vec<i32>>,
}

fn read_wav(bytes: &[u8]) -> Option<Pcm> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut i = 12;
    let (mut fmt, mut data) = (None, None);
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &bytes[i + 8..(i + 8 + len).min(bytes.len())];
        match id {
            b"fmt " => fmt = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        i += 8 + len + (len & 1);
    }
    let (fmt, data) = (fmt?, data?);
    let tag = u16::from_le_bytes([fmt[0], fmt[1]]);
    let channels = u16::from_le_bytes([fmt[2], fmt[3]]) as u32;
    let rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
    let bps = u16::from_le_bytes([fmt[14], fmt[15]]) as u32;
    // PCM, or WAVE_FORMAT_EXTENSIBLE carrying PCM.
    if tag != 1 && !(tag == 0xFFFE && fmt.len() >= 26 && fmt[24] == 1) {
        return None;
    }
    if !matches!(bps, 8 | 16 | 24) || channels == 0 {
        return None;
    }
    let bytes_per = (bps / 8) as usize;
    let frame = bytes_per * channels as usize;
    let n = data.len() / frame;
    let mut planes = vec![Vec::with_capacity(n); channels as usize];
    for f in data.chunks_exact(frame) {
        for (c, plane) in planes.iter_mut().enumerate() {
            let s = &f[c * bytes_per..(c + 1) * bytes_per];
            plane.push(match bps {
                8 => s[0] as i32 - 128,
                16 => i16::from_le_bytes([s[0], s[1]]) as i32,
                _ => i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8,
            });
        }
    }
    Some(Pcm {
        rate,
        channels,
        bps,
        planes,
    })
}

fn read_any(path: &str) -> Option<Pcm> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.starts_with(b"fLaC") {
        let (info, planes) = rusty_flac::decode(&bytes).ok()?;
        return Some(Pcm {
            rate: info.sample_rate,
            channels: info.channels,
            bps: info.bits_per_sample,
            planes,
        });
    }
    read_wav(&bytes)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut levels = vec![0u32, 5, 8];
    let mut files = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--levels" {
            levels = it
                .next()
                .expect("--levels needs a list")
                .split(',')
                .map(|l| l.parse().expect("level"))
                .collect();
        } else {
            files.push(a.clone());
        }
    }
    let mut totals = vec![0u64; levels.len()];
    let mut raw_total = 0u64;
    for path in &files {
        let Some(pcm) = read_any(path) else {
            eprintln!("skip (not PCM WAV / FLAC): {path}");
            continue;
        };
        let n = pcm.planes[0].len();
        let raw = (n * pcm.channels as usize * (pcm.bps as usize / 8)) as u64;
        raw_total += raw;
        let name = std::path::Path::new(path)
            .file_name()
            .unwrap()
            .to_string_lossy();
        let mut row = format!(
            "{name:<34} {:>6}Hz {}ch {:>2}b {:>9}",
            pcm.rate, pcm.channels, pcm.bps, raw
        );
        for (li, &level) in levels.iter().enumerate() {
            let mut enc = rusty_flac::Encoder::new(pcm.rate, pcm.channels, pcm.bps).unwrap();
            enc.set_compression_level(level);
            let planes: Vec<&[i32]> = pcm.planes.iter().map(|p| p.as_slice()).collect();
            enc.push_planar(&planes).unwrap();
            let out = enc.finish();
            let (_, back) = rusty_flac::decode(&out).expect("decode own stream");
            assert!(back == pcm.planes, "NOT LOSSLESS: {path} level {level}");
            totals[li] += out.len() as u64;
            row += &format!(" L{level}={:>9}", out.len());
        }
        println!("{row}");
    }
    let mut t = format!("{:<34} {:>25} {raw_total:>9}", "TOTAL", "");
    for (li, &level) in levels.iter().enumerate() {
        t += &format!(" L{level}={:>9}", totals[li]);
    }
    println!("{t}");
}
