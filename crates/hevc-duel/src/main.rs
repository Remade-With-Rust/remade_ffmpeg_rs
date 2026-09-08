//! `hevc-duel` — two HEVC decoders, one clip, side by side, playing in real time.
//!
//! # What this is, and what it is not
//!
//! A **demo**. Both decoders run at once on one machine and contend for cores,
//! cache and memory bandwidth, so nothing here is a published number. Those come
//! from `tools/bench/codec-bench.ps1`: one arm at a time, pinned to one core at
//! High priority, ABBA-interleaved, paired win rate with a z-score, work parity
//! checked.
//!
//! What it shows that a table cannot: the two decoders emitting **the same
//! pixels**, frame by frame, live, while you watch the clip play.
//!
//! # Real-time playback, and how speed is still measurable underneath it
//!
//! The clip plays at its own frame rate — 600 frames of 720p30 takes twenty
//! seconds, as it should. The reader paces itself to that rate, and because a
//! pipe holds far less than one 1.3 MB frame, the decoder upstream is paced with
//! it. Both panes therefore show the same picture at the same moment.
//!
//! That creates a measurement problem, and the first two attempts at it were
//! both wrong:
//!
//! * **Wall time between frames** is just the pacing interval. Useless.
//! * **Time spent in `read()`** looked plausible and was worse than useless: with
//!   the reader paced, the decoder runs ahead and fills the pipe, so `read()`
//!   returns out of the buffer and times BUFFERING. Our binary writes through a
//!   1 MiB `BufWriter` and ffmpeg writes in smaller pieces, and that asymmetry
//!   alone reported us at 21.9x real time against ffmpeg's 6.6x — three times
//!   faster, when every pinned measurement puts us 1.4x-2.0x slower.
//!
//! The quantity that survives is **CPU time**, sampled from each child with
//! `GetProcessTimes`. It accrues only while the process is actually running, so
//! it is immune to pacing and to pipe buffering alike: a decoder that spends 3
//! seconds of CPU delivering 20 seconds of video is running at 6.7x real time,
//! whether or not anything throttled it. Duty cycle is CPU over wall; headroom is
//! its reciprocal.
//!
//! # Display path
//!
//! The reader loop does one thing per frame: read, hash, stamp. A per-arm encoder
//! thread converts whatever frame is current to half-size JPEG on a fixed
//! cadence, and `/mjpeg/{i}` streams those as `multipart/x-mixed-replace`, which
//! browsers paint natively in an `<img>`. Nothing about the browser can reach the
//! decoders.
//!
//! # Usage
//!
//! ```text
//! hevc-duel --stream S.hevc --clip-fps 30 \
//!   --arm "rusty_h265 0.3.0|target/release/rusty_h265.exe|{S}|--pipe|x" \
//!   --arm "ffmpeg 8.1.2|ffmpeg|-v|error|-f|hevc|-i|{S}|-f|rawvideo|-pix_fmt|yuv420p|-"
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HTML: &str = include_str!("page.html");

/// CPU time (user + kernel) consumed by a child, in microseconds.
///
/// The only measurement here that pacing cannot distort — see the module note.
#[cfg(windows)]
fn child_cpu_us(child: &Child) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: the handle is owned by `child` and outlives the call; all four
    // FILETIME outputs are valid, writable and distinct.
    let ok = unsafe { GetProcessTimes(child.as_raw_handle() as _, &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return 0;
    }
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    (ft(k) + ft(u)) / 10 // 100 ns units -> us
}

#[cfg(not(windows))]
fn child_cpu_us(_child: &Child) -> u64 {
    0
}

struct Arm {
    name: String,
    argv: Vec<String>,
    frames: AtomicU64,
    /// Wall microseconds since this arm started.
    elapsed_us: AtomicU64,
    /// CPU microseconds the decoder has actually consumed.
    cpu_us: AtomicU64,
    running: AtomicBool,
    done: AtomicBool,
    failed: Mutex<Option<String>>,
    latest: Mutex<Vec<u8>>,
    hashes: Mutex<Vec<u64>>,
    jpeg: Mutex<Arc<Vec<u8>>>,
    jpeg_gen: AtomicU64,
}

impl Arm {
    fn new(name: String, argv: Vec<String>) -> Self {
        Arm {
            name,
            argv,
            frames: AtomicU64::new(0),
            elapsed_us: AtomicU64::new(0),
            cpu_us: AtomicU64::new(0),
            running: AtomicBool::new(false),
            done: AtomicBool::new(false),
            failed: Mutex::new(None),
            latest: Mutex::new(Vec::new()),
            hashes: Mutex::new(Vec::new()),
            jpeg: Mutex::new(Arc::new(Vec::new())),
            jpeg_gen: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.elapsed_us.store(0, Ordering::Relaxed);
        self.cpu_us.store(0, Ordering::Relaxed);
        self.done.store(false, Ordering::Relaxed);
        *self.failed.lock().unwrap() = None;
        self.hashes.lock().unwrap().clear();
    }
}

/// FNV-1a: a cheap per-frame fingerprint, well under the cost of the frame.
fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn run_arm(arm: Arc<Arm>, frame_len: usize, repeat: usize, pace_ns: u64) {
    arm.reset();
    arm.running.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    let mut cpu_base = 0u64;
    for _ in 0..repeat {
        match run_once(&arm, frame_len, t0, pace_ns, cpu_base) {
            Some(cpu) => cpu_base = cpu,
            None => break,
        }
    }
    arm.done.store(true, Ordering::Relaxed);
    arm.running.store(false, Ordering::Relaxed);
}

/// One pass over the clip. Returns the arm's cumulative CPU microseconds, or
/// `None` if it failed. `cpu_base` carries CPU across passes, because each pass
/// is a new process whose own counter starts at zero.
fn run_once(arm: &Arc<Arm>, frame_len: usize, t0: Instant, pace_ns: u64, cpu_base: u64) -> Option<u64> {
    let mut cmd = Command::new(&arm.argv[0]);
    cmd.args(&arm.argv[1..]).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            *arm.failed.lock().unwrap() = Some(format!("spawn {}: {e}", arm.argv[0]));
            return None;
        }
    };
    let mut out = child.stdout.take().expect("piped");
    let mut buf = vec![0u8; frame_len];
    let mut last_cpu = 0u64;
    loop {
        let mut got = 0;
        while got < frame_len {
            match out.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => {
                    *arm.failed.lock().unwrap() = Some(format!("read: {e}"));
                    break;
                }
            }
        }
        if got < frame_len {
            break;
        }
        let h = fnv1a(&buf);
        arm.hashes.lock().unwrap().push(h);
        arm.latest.lock().unwrap().copy_from_slice_or_set(&buf);
        let n = arm.frames.fetch_add(1, Ordering::Relaxed) + 1;
        arm.elapsed_us.store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
        // Sample the child's CPU a few times a second rather than per frame: it
        // is a syscall, and the value is tick-quantised anyway.
        if n % 8 == 0 {
            last_cpu = child_cpu_us(&child);
            arm.cpu_us.store(cpu_base + last_cpu, Ordering::Relaxed);
        }
        // Hold the clip to its own frame rate, scheduled against the arm's start
        // so one late frame does not push every later one back.
        if pace_ns > 0 {
            let due = Duration::from_nanos(pace_ns.saturating_mul(n));
            if let Some(w) = due.checked_sub(t0.elapsed()) {
                std::thread::sleep(w);
            }
        }
    }
    let final_cpu = child_cpu_us(&child).max(last_cpu);
    arm.cpu_us.store(cpu_base + final_cpu, Ordering::Relaxed);
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let _ = child.wait();
    if arm.frames.load(Ordering::Relaxed) == 0 {
        *arm.failed.lock().unwrap() = Some(if err.is_empty() { "no frames".into() } else { err });
        return None;
    }
    Some(cpu_base + final_cpu)
}

trait SetFrom {
    fn copy_from_slice_or_set(&mut self, src: &[u8]);
}
impl SetFrom for Vec<u8> {
    fn copy_from_slice_or_set(&mut self, src: &[u8]) {
        if self.len() == src.len() {
            self.copy_from_slice(src);
        } else {
            *self = src.to_vec();
        }
    }
}

/// yuv420p -> RGB24 at half resolution, BT.601 limited range.
///
/// Half size is a 4x cut in conversion and encode, and an output pixel then maps
/// exactly onto one chroma sample, so chroma needs no resampling. Display only.
fn yuv_to_rgb_half(yuv: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (ow, oh) = (w / 2, h / 2);
    let (yp, up) = (0usize, w * h);
    let vp = up + cw * ch;
    let mut rgb = vec![0u8; ow * oh * 3];
    for j in 0..oh {
        for i in 0..ow {
            let r0 = yp + (2 * j) * w + 2 * i;
            let r1 = r0 + w;
            let y = (yuv[r0] as i32 + yuv[r0 + 1] as i32 + yuv[r1] as i32 + yuv[r1 + 1] as i32 + 2) >> 2;
            let u = yuv[up + j * cw + i] as i32 - 128;
            let v = yuv[vp + j * cw + i] as i32 - 128;
            let c = (y - 16) * 298;
            let o = (j * ow + i) * 3;
            rgb[o] = (((c + 409 * v + 128) >> 8).clamp(0, 255)) as u8;
            rgb[o + 1] = (((c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255)) as u8;
            rgb[o + 2] = (((c + 516 * u + 128) >> 8).clamp(0, 255)) as u8;
        }
    }
    (rgb, ow, oh)
}

fn run_encoder(arm: Arc<Arm>, w: usize, h: usize, period: Duration) {
    let mut last = u64::MAX;
    loop {
        std::thread::sleep(period);
        let n = arm.frames.load(Ordering::Relaxed);
        if n == last {
            continue;
        }
        last = n;
        let yuv = { arm.latest.lock().unwrap().clone() };
        if yuv.len() < w * h {
            continue;
        }
        let (rgb, ow, oh) = yuv_to_rgb_half(&yuv, w, h);
        let mut out = Vec::with_capacity(64 << 10);
        let enc = rusty_jpeg::encode::Encoder::new(&mut out, 72);
        if enc.encode(&rgb, ow as u16, oh as u16, rusty_jpeg::encode::ColorType::Rgb).is_ok() {
            *arm.jpeg.lock().unwrap() = Arc::new(out);
            arm.jpeg_gen.fetch_add(1, Ordering::Release);
        }
    }
}

fn send(mut s: TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let val = |k: &str| -> Option<String> { a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned() };
    let Some(stream) = val("--stream") else {
        eprintln!("usage: hevc-duel --stream S.hevc --arm \"name|exe|arg|arg\" --arm \"...\" [--clip-fps 30] [--port 8099]");
        std::process::exit(2);
    };
    let port: u16 = val("--port").and_then(|p| p.parse().ok()).unwrap_or(8099);
    let repeat: usize = val("--repeat").and_then(|p| p.parse().ok()).unwrap_or(1);
    let clip_fps: f64 = val("--clip-fps").and_then(|p| p.parse().ok()).unwrap_or(30.0);
    // Real time by default: the clip plays at its own rate. `--free` lets both
    // arms run flat out instead, which is quicker to watch but is not playback.
    let free = a.iter().any(|x| x == "--free");
    let pace_ns: u64 = if free || clip_fps <= 0.0 { 0 } else { (1e9 / clip_fps) as u64 };

    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-f", "hevc", "-count_frames", "-show_entries", "stream=width,height,nb_read_frames", "-of", "csv=p=0", &stream])
        .output()
        .expect("ffprobe (needed for frame geometry)");
    let dims = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    let (w, h, clip_frames): (usize, usize, usize) = match dims.split(',').collect::<Vec<_>>()[..] {
        [x, y, n] => (x.trim().parse().unwrap(), y.trim().parse().unwrap(), n.trim().parse().unwrap_or(0)),
        _ => panic!("ffprobe returned {dims:?} for {stream}"),
    };
    let frame_len: usize = w * h + 2 * (w.div_ceil(2) * h.div_ceil(2));
    let src_bytes = std::fs::metadata(&stream).map(|m| m.len()).unwrap_or(0);

    let arms: Vec<Arc<Arm>> = a
        .iter()
        .enumerate()
        .filter(|(_, x)| x.as_str() == "--arm")
        .filter_map(|(i, _)| a.get(i + 1))
        .map(|spec| {
            let mut parts: Vec<String> = spec.split('|').map(|p| p.replace("{S}", &stream)).collect();
            let name = parts.remove(0);
            Arc::new(Arm::new(name, parts))
        })
        .collect();
    assert!(arms.len() >= 2, "need at least two --arm specs");

    println!("hevc-duel: {stream}  {w}x{h}  {clip_frames} frames ({:.1}s at {clip_fps} fps)", clip_frames as f64 / clip_fps);
    for arm in &arms {
        println!("  arm: {:<22} {}", arm.name, arm.argv.join(" "));
    }
    println!("\n  open  http://127.0.0.1:{port}/     then press Play\n");

    let period = Duration::from_millis(33);
    for arm in &arms {
        let a3 = Arc::clone(arm);
        std::thread::spawn(move || run_encoder(a3, w, h, period));
    }

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    for conn in listener.incoming() {
        let Ok(mut s) = conn else { continue };
        let arms = arms.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let Ok(n) = s.read(&mut buf) else { return };
            if n == 0 {
                return;
            }
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");

            if path.starts_with("/start") {
                let busy = arms.iter().any(|x| x.running.load(Ordering::Relaxed));
                if !busy {
                    for arm in &arms {
                        let a2 = Arc::clone(arm);
                        std::thread::spawn(move || run_arm(a2, frame_len, repeat, pace_ns));
                    }
                }
                let body: &[u8] = if busy { b"{\"started\":false}" } else { b"{\"started\":true}" };
                send(s, "200 OK", "application/json", body);
                return;
            }

            if let Some(idx) = path.strip_prefix("/mjpeg/").and_then(|p| p.split('?').next()).and_then(|p| p.parse::<usize>().ok()) {
                let Some(arm) = arms.get(idx) else {
                    send(s, "404 Not Found", "text/plain", b"no such arm");
                    return;
                };
                let head = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=f\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
                if s.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let mut seen = u64::MAX;
                loop {
                    let generation = arm.jpeg_gen.load(Ordering::Acquire);
                    if generation == seen {
                        std::thread::sleep(Duration::from_millis(8));
                        continue;
                    }
                    seen = generation;
                    let img = { Arc::clone(&arm.jpeg.lock().unwrap()) };
                    if img.is_empty() {
                        continue;
                    }
                    let part = format!("--f\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", img.len());
                    if s.write_all(part.as_bytes()).is_err() || s.write_all(&img).is_err() || s.write_all(b"\r\n").is_err() {
                        return; // the tab closed; not an error
                    }
                }
            }

            if path.starts_with("/stats") {
                let hs: Vec<Vec<u64>> = arms.iter().map(|x| x.hashes.lock().unwrap().clone()).collect();
                let common = hs.iter().map(|v| v.len()).min().unwrap_or(0);
                let (mut agree, mut first_bad) = (0usize, -1i64);
                for k in 0..common {
                    if hs.iter().all(|v| v[k] == hs[0][k]) {
                        agree += 1;
                    } else if first_bad < 0 {
                        first_bad = k as i64;
                    }
                }
                let any_run = arms.iter().any(|x| x.running.load(Ordering::Relaxed));
                let started = arms.iter().any(|x| x.frames.load(Ordering::Relaxed) > 0);
                let mut j = format!(
                    "{{\"w\":{w},\"h\":{h},\"src_bytes\":{src_bytes},\"frame_bytes\":{frame_len},\"clip_fps\":{clip_fps},\"clip_frames\":{clip_frames},\"paced\":{},\"running\":{any_run},\"started\":{started},\"agree\":{agree},\"compared\":{common},\"first_mismatch\":{first_bad},\"arms\":[",
                    pace_ns > 0
                );
                for (i, arm) in arms.iter().enumerate() {
                    let f = arm.frames.load(Ordering::Relaxed);
                    let us = arm.elapsed_us.load(Ordering::Relaxed).max(1);
                    let cpu = arm.cpu_us.load(Ordering::Relaxed);
                    let fail = arm.failed.lock().unwrap().clone().unwrap_or_default();
                    if i > 0 {
                        j.push(',');
                    }
                    j.push_str(&format!(
                        "{{\"name\":\"{}\",\"frames\":{f},\"ms\":{:.1},\"cpu_ms\":{:.1},\"duty\":{:.5},\"cpu_per_frame\":{:.3},\"play_fps\":{:.2},\"done\":{},\"error\":\"{}\"}}",
                        esc(&arm.name),
                        us as f64 / 1000.0,
                        cpu as f64 / 1000.0,
                        cpu as f64 / us as f64,
                        if f > 0 { cpu as f64 / 1000.0 / f as f64 } else { 0.0 },
                        f as f64 * 1e6 / us as f64,
                        arm.done.load(Ordering::Relaxed),
                        esc(fail.lines().next().unwrap_or(""))
                    ));
                }
                j.push_str("]}");
                send(s, "200 OK", "application/json", j.as_bytes());
                return;
            }
            send(s, "200 OK", "text/html; charset=utf-8", HTML.as_bytes());
        });
    }
}
