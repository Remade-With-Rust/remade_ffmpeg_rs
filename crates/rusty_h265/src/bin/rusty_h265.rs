//! Conformance-harness front end: `rusty_h265 <in.bit|.hevc> <out.yuv>`.
//!
//! Decodes an Annex-B elementary stream and writes the cropped pictures in
//! output order as planar 4:2:0 (`u8` for 8-bit, `u16` LE otherwise), the
//! layout `tools/hevc/conform.py` hashes. Prints one `key=value` stats line
//! on stdout so the harness can read frame and error counts.

// Primary allocator for this binary: our rusty_alloc, the pure-Rust mimalloc
// remake, which is what `rff-cli` ships. Timing this harness under the system
// allocator instead would not be comparable to the product — and several of the
// decoder's wins are removed allocations, which is exactly the thing an
// allocator swap changes the price of.
#[cfg(feature = "bench-alloc")]
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: rusty_h265 <in.bit> <out.yuv> [--headers-only]");
        std::process::exit(2);
    }
    let data = match std::fs::read(&args[1]) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("read {}: {e}", args[1]);
            std::process::exit(2);
        }
    };
    let headers_only = args.iter().any(|a| a == "--headers-only");
    let verify_sei = args.iter().any(|a| a == "--verify-sei");
    // `-` as the output path writes nothing: the decode-only arm, so a
    // measurement is not dominated by the YUV write (codec-measurement §4).
    let mut out: Box<dyn Write> = if args[2] == "-" {
        Box::new(std::io::sink())
    } else {
        Box::new(std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("create output")))
    };
    let t0 = std::time::Instant::now();
    let mut dec = rusty_h265::Decoder::new();
    dec.headers_only = headers_only;
    dec.verify_sei = verify_sei;
    let mut first_err: Option<String> = None;
    for nal in rusty_h265::nal::split_annex_b(&data) {
        if let Err(e) = dec.push_nal(nal, None) {
            dec.stats.errors += 1;
            first_err.get_or_insert_with(|| e.to_string());
        }
        drain(&mut dec, &mut out);
    }
    dec.flush();
    let (w, h, bd) = drain(&mut dec, &mut out);
    out.flush().expect("flush output");
    let ms = t0.elapsed().as_millis();
    let s = dec.stats;
    let first_sei = dec.sei_results.first().map_or("none", |r| if r.1 { "ok" } else { "bad" });
    println!(
        "frames={} errors={} decode_ms={} width={} height={} bit_depth={} pictures={} slices={} skipped_rasl={} generated_refs={} sei_checked={} sei_mismatch={} first_sei={} alloc={} isa={}",
        FRAMES.with(|f| f.get()),
        s.errors,
        ms,
        w,
        h,
        bd,
        s.pictures,
        s.slices,
        s.skipped_rasl,
        s.generated_refs,
        s.sei_checked,
        s.sei_mismatch,
        first_sei,
        // The measurement harness refuses to time a binary that is not the
        // one that ships: CLAUDE.md requires every performance number to come
        // from a rusty_alloc build, and this whole campaign was measured under
        // the system allocator before anyone checked. A comment in the harness
        // did not prevent that; a field it can assert on does.
        if cfg!(feature = "bench-alloc") { "rusty" } else { "system" },
        rusty_h265::accel::describe().rsplit(": ").next().unwrap_or("?"),
    );
    if let Some(e) = first_err {
        eprintln!("first error: {e}");
    }
    // REACHABILITY (codec-vectorize-kernel): a kernel with a test and a
    // benchmark but a zero here is not deployed, whatever the call graph says.
    if std::env::var_os("RH265_CENSUS").is_some() {
        eprintln!("{}", rusty_h265::accel::describe());
        for (name, v) in rusty_h265::accel::census::snapshot() {
            eprintln!("census {name} = {v}");
        }
    }

    // A decode that produced nothing must not look like success.
    //
    // This exited 0 after `frames=0 errors=60` on a Range-Extensions stream it
    // legitimately cannot decode -- and a benchmark harness timed that, saw
    // 6 ms against ffmpeg's 734 ms, and reported us **46x faster**. The number
    // was not wrong about the clock; it was wrong about what had happened, and
    // nothing in the exit status said so. Scripts read exit codes.
    if FRAMES.with(|f| f.get()) == 0 || s.errors > 0 {
        std::process::exit(1);
    }
}

thread_local! {
    static FRAMES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn drain(dec: &mut rusty_h265::Decoder, out: &mut impl Write) -> (usize, usize, u8) {
    let mut geom = (0, 0, 0);
    let mut buf = Vec::new();
    while let Ok(frame) = dec.next_frame() {
        buf.clear();
        frame.write_yuv(&mut buf);
        out.write_all(&buf).expect("write output");
        geom = (frame.width, frame.height, frame.bit_depth());
        FRAMES.with(|f| f.set(f.get() + 1));
    }
    geom
}
