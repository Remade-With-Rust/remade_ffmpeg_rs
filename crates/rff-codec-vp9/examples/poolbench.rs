//! Interleaved A/B for the snapshot buffer pool (`frameenc`'s thread-local free-lists).
//!
//! Two separate runs cannot answer "is the pool faster?" — measured across processes the
//! same change read −10.6% once and +16.9% the next time, with stages the pool does not
//! touch (`part_ctx`, `mode_map`) moving just as much. That is machine state, not the
//! change. So this bench ALTERNATES the arms inside one process, one clip loaded once:
//!
//!   rep 0: ON, OFF   rep 1: ON, OFF   ...
//!
//! and reports best-of-N per arm. Best-of-N (not mean) because the fastest pass is the one
//! least disturbed by the scheduler and by turbo/thermal excursions, so it is the most
//! repeatable point of the distribution. Interleaving means any drift over the run hits
//! both arms equally instead of landing entirely on whichever ran second.
//!
//! Usage: `... --example poolbench -- <in.y4m> <crf> <speed> [reps] [frames] [lever]`
//! `lever` is `pool` (default), `modemap`, or `null`.
//!
//! `null` sets NOTHING, so both arms are the identical encoder. Its output is the
//! harness's own noise floor — the number below which a reported win or loss means
//! nothing. Run it before believing any small result.

use std::time::Instant;

use rff_codec::CodecRegistry;
use rff_core::{CodecId, Dictionary, Frame, VideoFrame};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (path, crf, speed) = (&a[1], &a[2], &a[3]);
    let reps: usize = a.get(4).map_or(5, |s| s.parse().unwrap());
    let want: usize = a.get(5).map_or(usize::MAX, |s| s.parse().unwrap());
    let lever = a.get(6).map_or("pool", |s| s.as_str());

    let frames = read_y4m(path, want);
    let (w, h) = (frames[0].width as usize, frames[0].height as usize);

    let mut best = [f64::MAX; 2]; // [pool ON, pool OFF]
    let mut bytes = [0usize; 2];
    for rep in 0..reps {
        // ABBA, not AABB. Running arm 0 first in every rep gives it a systematic edge:
        // the `null` control (identical arms) measured +2.9/-0.4/+3.4/-0.4% across four
        // clips — a mean of +1.4% for whichever arm went first, which is pure artefact.
        // Swapping the order on odd reps cancels it.
        let order: [(usize, bool); 2] = if rep % 2 == 0 {
            [(0, true), (1, false)]
        } else {
            [(1, false), (0, true)]
        };
        for (arm, on) in order {
            // `on` = the NEW arm in both cases: pooled buffers / packed-key FxHash map.
            match lever {
                "modemap" => rff_codec_vp9::set_modemap_std(!on),
                // Control arm: change nothing, so any spread is pure measurement noise.
                "null" => {}
                _ => rff_codec_vp9::set_snap_pool(on),
            }
            let (dt, n) = encode_once(&frames, crf, speed);
            if dt < best[arm] {
                best[arm] = dt;
            }
            bytes[arm] = n;
        }
    }

    // Byte totals are printed as a self-check: the pool only changes where a buffer comes
    // from, so any difference here means the arms are not encoding the same thing.
    let (on, off) = (best[0], best[1]);
    println!(
        "{:<22} {}x{} {} frames  speed={speed} crf={crf}  reps={reps}",
        std::path::Path::new(path)
            .file_stem()
            .unwrap()
            .to_string_lossy(),
        w,
        h,
        frames.len(),
    );
    println!(
        "  {lever} NEW  {on:7.3} s   ({:6.1} fps)  {} B",
        frames.len() as f64 / on,
        bytes[0]
    );
    println!(
        "  {lever} OLD  {off:7.3} s   ({:6.1} fps)  {} B",
        frames.len() as f64 / off,
        bytes[1]
    );
    println!(
        "  => {lever} NEW is {:+.1}% {}   [size check: {}]",
        100.0 * (off / on - 1.0),
        if on < off { "FASTER" } else { "SLOWER" },
        if bytes[0] == bytes[1] {
            "same bytes"
        } else {
            "MISMATCH — arms differ!"
        },
    );
}

fn encode_once(frames: &[VideoFrame], crf: &str, speed: &str) -> (f64, usize) {
    let mut reg = CodecRegistry::new();
    rff_codec_vp9::register(&mut reg);
    let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
    let mut opts = Dictionary::new();
    opts.set("crf", crf);
    opts.set("cpu-used", speed);
    enc.configure(&opts).unwrap();

    let mut total = 0usize;
    let t0 = Instant::now();
    for f in frames {
        enc.send_frame(&Frame::Video(f.clone())).unwrap();
        while let Ok(p) = enc.receive_packet() {
            total += p.data.len();
        }
    }
    enc.flush();
    while let Ok(p) = enc.receive_packet() {
        total += p.data.len();
    }
    (t0.elapsed().as_secs_f64(), total)
}

/// Minimal 4:2:0 8-bit y4m reader (same parse as `speedbench`).
fn read_y4m(path: &str, want: usize) -> Vec<VideoFrame> {
    let buf = std::fs::read(path).unwrap();
    let nl = buf.iter().position(|&b| b == b'\n').unwrap();
    let header = std::str::from_utf8(&buf[..nl]).unwrap();
    let (mut w, mut h) = (0usize, 0usize);
    for tok in header.split_whitespace() {
        match tok.as_bytes().first() {
            Some(b'W') => w = tok[1..].parse().unwrap(),
            Some(b'H') => h = tok[1..].parse().unwrap(),
            _ => {}
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let fsize = w * h + 2 * cw * ch;
    let mut frames = Vec::new();
    let mut pos = nl + 1;
    while pos < buf.len() && frames.len() < want {
        let Some(fnl) = buf[pos..].iter().position(|&b| b == b'\n').map(|i| pos + i) else {
            break;
        };
        let start = fnl + 1;
        if start + fsize > buf.len() {
            break;
        }
        frames.push(VideoFrame {
            width: w as u32,
            height: h as u32,
            format: rff_core::PixelFormat::Yuv420p,
            planes: vec![
                buf[start..start + w * h].to_vec(),
                buf[start + w * h..start + w * h + cw * ch].to_vec(),
                buf[start + w * h + cw * ch..start + fsize].to_vec(),
            ],
            strides: vec![w, cw, cw],
            pts: None,
        });
        pos = start + fsize;
    }
    frames
}
