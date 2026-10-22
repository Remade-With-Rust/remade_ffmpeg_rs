//! Spec-coverage gate beyond what libvorbis emits: randomly generated (but
//! valid) setup headers exercising floor 0, floor 1, residue formats 0/1/2,
//! codebook lookup types 0/1/2 (sequence_p on and off), ordered / sparse /
//! dense length encodings, single-entry books, submaps + mux, multi-step
//! coupling and partition sizes that are not a multiple of the VQ dimension —
//! decoded from random audio packets (every complete Huffman tree decodes any
//! bit string) and compared packet by packet against lewton 0.10.2.
//!
//! Generator limits avoid lewton's own defects (each deliberate, documented
//! in `decode/header.rs`): no residue cascade bit 7 (lewton reads 7 books),
//! floor-0 order >= 2 and amplitude bits <= 24 (lewton underflows/overflows),
//! residue-2 `blocksize * channels` < 65536 (lewton computes it in u16). A
//! lewton panic (debug overflow on garbage floor values) skips that packet.
//!
//! Debug knobs: `SYN_ONLY=<stream>` decodes one stream, `SYN_DUMP=<dir>`
//! writes its headers/packets, `SYN_STATS=1` prints per-packet error.

use rusty_vorbis::decode::Decoder;
use rusty_vorbis::BitWriter;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn chance(&mut self, p: u64) -> bool {
        self.below(100) < p
    }
}

fn ilog(v: u64) -> u32 {
    64 - v.leading_zeros()
}

/// A random complete prefix-code length set with `n >= 2` leaves (max 20).
fn complete_lengths(r: &mut Rng, n: usize) -> Vec<u8> {
    let mut leaves = vec![1u8, 1u8];
    while leaves.len() < n {
        let i = r.below(leaves.len() as u64) as usize;
        if leaves[i] >= 20 {
            continue;
        }
        leaves[i] += 1;
        let l = leaves[i];
        leaves.push(l);
    }
    // Shuffle.
    for i in (1..leaves.len()).rev() {
        let j = r.below(i as u64 + 1) as usize;
        leaves.swap(i, j);
    }
    leaves
}

/// Vorbis float32: `mant * 2^(exp - 788)`, `value = mant / 2^shift`.
fn float32(neg: bool, mant: u32, shift: u32) -> u32 {
    (u32::from(neg) << 31) | ((788 - shift) << 21) | (mant & 0x1f_ffff)
}

struct BookSpec {
    vq: bool,
}

/// Write one codebook; returns its spec.
fn write_book(w: &mut BitWriter, r: &mut Rng, dim: u32, used: usize, vq_kind: u32) -> BookSpec {
    // Lengths: single-entry (len 1) or a complete code, optionally sparse.
    let mut lengths: Vec<u8> = if used == 1 {
        vec![1]
    } else {
        complete_lengths(r, used)
    };
    let sparse = used > 1 && r.chance(30);
    if sparse || used == 1 {
        let extra = r.range(1, 6) as usize;
        for _ in 0..extra {
            let at = r.below(lengths.len() as u64 + 1) as usize;
            lengths.insert(at, 0);
        }
    }
    let ordered = !sparse && used > 1 && r.chance(25);
    if ordered {
        lengths.sort_unstable();
    }
    let entries = lengths.len() as u32;
    let mut dim = dim;
    // Lookup type 1 needs entries with an integral lookup1_values; keep dims small.
    if vq_kind == 1 {
        dim = dim.min(4);
    }
    w.write(0x56_4342, 24);
    w.write(dim, 16);
    w.write(entries, 24);
    w.write(u32::from(ordered), 1);
    if ordered {
        let mut cur = 0usize;
        let mut len = lengths[0];
        w.write(u32::from(len) - 1, 5);
        while cur < lengths.len() {
            let num = lengths[cur..].iter().take_while(|&&l| l == len).count();
            w.write(num as u32, ilog((lengths.len() - cur) as u64));
            cur += num;
            len += 1;
        }
    } else {
        w.write(u32::from(sparse || used == 1), 1);
        for &l in &lengths {
            if sparse || used == 1 {
                if l == 0 {
                    w.write(0, 1);
                } else {
                    w.write(1, 1);
                    w.write(u32::from(l) - 1, 5);
                }
            } else {
                w.write(u32::from(l) - 1, 5);
            }
        }
    }
    w.write(vq_kind, 4);
    if vq_kind != 0 {
        // min in [-2, 0], delta in (0, 0.25]: keeps values sane.
        w.write(float32(true, r.range(0, 32) as u32, 4), 32);
        w.write(float32(false, r.range(1, 16) as u32, 6), 32);
        let value_bits = r.range(1, 5) as u32;
        w.write(value_bits - 1, 4);
        w.write(u32::from(r.chance(30)), 1);
        let lookup_values = if vq_kind == 1 {
            let mut v = 0u64;
            while (v + 1).pow(dim) <= u64::from(entries) {
                v += 1;
            }
            v
        } else {
            u64::from(entries) * u64::from(dim)
        };
        for _ in 0..lookup_values {
            w.write(r.below(1 << value_bits) as u32, value_bits);
        }
    }
    BookSpec { vq: vq_kind != 0 }
}

struct Stream {
    desc: String,
    ident: Vec<u8>,
    comment: Vec<u8>,
    setup: Vec<u8>,
    channels: u8,
}

fn make_stream(r: &mut Rng) -> Stream {
    let channels = r.range(1, 4) as u8;
    // Blocksizes >= 256: lewton's fast (stb_vorbis-derived) IMDCT is wrong at
    // n = 64 and 128 (its own slow IMDCT agrees with ours to ~2e-7 there); the
    // small sizes are covered by `decode::tests` against an f64 direct IMDCT.
    let bs0 = r.range(8, 10) as u8;
    let bs1 = r.range(u64::from(bs0), 12) as u8;
    let ident = rusty_vorbis::write_ident_header(channels, 44_100, bs0, bs1, 0);
    let comment = rusty_vorbis::write_comment_header("synthetic", &[("K", "v")]);

    let mut desc = format!("ch={channels} bs={bs0}/{bs1}");
    let mut w = BitWriter::new();
    for &b in b"\x05vorbis" {
        w.write(u32::from(b), 8);
    }
    // --- codebooks ---
    let mut books: Vec<BookSpec> = Vec::new();
    let nbooks = r.range(6, 12) as usize;
    w.write(nbooks as u32 - 1, 8);
    for i in 0..nbooks {
        let (dim, used, kind) = match i {
            0 => (r.range(1, 3) as u32, r.range(2, 40) as usize, 0), // classbook-ish scalar
            1 => (1, r.range(2, 64) as usize, 0),                    // floor1 y book
            2 => (1, 1, 0),                                          // single-entry book
            _ => {
                let kind = if r.chance(50) { 1 } else { 2 };
                let dim = *[1u32, 2, 3, 4, 8].get(r.below(5) as usize).unwrap();
                let used = r.range(2, 80) as usize;
                (dim, used, kind)
            }
        };
        let b = write_book(&mut w, r, dim, used, kind);
        books.push(b);
    }
    let vq_books: Vec<usize> = (0..books.len()).filter(|&i| books[i].vq).collect();
    // --- time ---
    w.write(0, 6);
    w.write(0, 16);
    // --- floors: one floor1, one floor0 ---
    w.write(1, 6);
    {
        // floor 1
        w.write(1, 16);
        let partitions = r.range(1, 4) as u32;
        w.write(partitions, 5);
        let nclasses = r.range(1, 2) as u32;
        let mut pclass = Vec::new();
        for _ in 0..partitions {
            let c = r.below(u64::from(nclasses)) as u32;
            pclass.push(c);
            w.write(c, 4);
        }
        let mut cdims = Vec::new();
        let nclasses = pclass.iter().max().map_or(0, |&m| m + 1);
        for _ in 0..nclasses {
            let cdim = r.range(1, 3) as u32;
            cdims.push(cdim);
            w.write(cdim - 1, 3);
            let sub = r.range(0, 2) as u32;
            w.write(sub, 2);
            if sub != 0 {
                w.write(0, 8); // masterbook: the scalar book 0
            }
            for _ in 0..(1 << sub) {
                // book + 1: 0 = none, else book 1 (y book) or 2 (single entry)
                let b = *[0u32, 2, 2, 3].get(r.below(4) as usize).unwrap();
                w.write(b, 8);
            }
        }
        let mult = r.range(1, 4) as u32;
        w.write(mult - 1, 2);
        let rangebits = r.range(6, 10) as u32;
        w.write(rangebits, 4);
        let mut used_x = std::collections::BTreeSet::new();
        used_x.insert(0u32);
        used_x.insert(1u32 << rangebits);
        for &c in &pclass {
            for _ in 0..cdims[c as usize] {
                let mut x;
                loop {
                    x = r.below(1 << rangebits) as u32;
                    if used_x.insert(x) {
                        break;
                    }
                }
                w.write(x, rangebits);
            }
        }
    }
    {
        // floor 0
        w.write(0, 16);
        w.write(r.range(2, 12) as u32, 8); // order
        w.write(r.range(8000, 48000) as u32, 16); // rate
        w.write(r.range(64, 256) as u32, 16); // bark map size
        w.write(r.range(4, 24) as u32, 6); // amplitude bits
        w.write(r.range(1, 255) as u32, 8); // amplitude offset
        let nb = r.range(1, 3) as u32;
        w.write(nb - 1, 4);
        for _ in 0..nb {
            // Usually a VQ book; occasionally a scalar one (undecodable packet).
            let b = if r.chance(90) && !vq_books.is_empty() {
                vq_books[r.below(vq_books.len() as u64) as usize]
            } else {
                1
            };
            w.write(b as u32, 8);
        }
    }
    // --- residues: one of each format ---
    w.write(2, 6);
    for kind in 0..3u32 {
        w.write(kind, 16);
        let begin = r.range(0, 64) as u32;
        let end = begin + r.range(0, 1200) as u32;
        w.write(begin, 24);
        w.write(end, 24);
        let psize = if r.chance(70) {
            8 * r.range(1, 8) as u32
        } else {
            r.range(1, 40) as u32
        };
        w.write(psize - 1, 24);
        let ncls = r.range(1, 6) as u32;
        w.write(ncls - 1, 6);
        w.write(0, 8); // classbook 0
        let mut cascades = Vec::new();
        for _ in 0..ncls {
            let cascade = r.below(128) as u32 & if r.chance(30) { 0 } else { 0x7f };
            w.write(cascade & 7, 3);
            let high = cascade >> 3;
            w.write(u32::from(high != 0), 1);
            if high != 0 {
                w.write(high, 5);
            }
            cascades.push(cascade);
        }
        for c in cascades {
            for j in 0..8 {
                if c & (1 << j) != 0 {
                    w.write(vq_books[r.below(vq_books.len() as u64) as usize] as u32, 8);
                }
            }
        }
    }
    // --- mappings: one plain, one with submaps + coupling ---
    let cbits = ilog(u64::from(channels) - 1);
    w.write(1, 6);
    for m in 0..2 {
        w.write(0, 16);
        let submaps = if m == 1 { r.range(1, 3) as u32 } else { 1 };
        w.write(u32::from(submaps > 1), 1);
        if submaps > 1 {
            w.write(submaps - 1, 4);
        }
        let steps = if channels > 1 && r.chance(70) {
            r.range(1, 3) as u32
        } else {
            0
        };
        desc += &format!(" | map{m}: submaps={submaps} steps={steps}");
        w.write(u32::from(steps > 0), 1);
        if steps > 0 {
            w.write(steps - 1, 8);
            for _ in 0..steps {
                // magnitude < angle: lewton's `dual_mut_idx` returns the pair
                // in index order, swapping the roles when magnitude > angle (a
                // lewton defect libvorbis never triggers; we follow the spec).
                let mag = r.below(u64::from(channels) - 1) as u32;
                let ang = r.range(u64::from(mag) + 1, u64::from(channels) - 1) as u32;
                w.write(mag, cbits);
                w.write(ang, cbits);
            }
        }
        w.write(0, 2);
        if submaps > 1 {
            for _ in 0..channels {
                w.write(r.below(u64::from(submaps)) as u32, 4);
            }
        }
        for _ in 0..submaps {
            w.write(0, 8);
            let (f, rr) = (r.below(2) as u32, r.below(3) as u32);
            desc += &format!(" [floor{} res{rr}]", 1 - f);
            w.write(f, 8); // floor index 0 = floor1, 1 = floor0
            w.write(rr, 8); // residue 0..2
        }
    }
    // --- modes ---
    w.write(1, 6);
    for (bf, map) in [(0u32, 0u32), (1, 1)] {
        w.write(bf, 1);
        w.write(0, 16);
        w.write(0, 16);
        w.write(map, 8);
    }
    w.write(1, 1); // framing
    Stream {
        desc,
        ident,
        comment,
        setup: w.into_bytes(),
        channels,
    }
}

#[test]
fn synthetic_streams_match_lewton() {
    use lewton::audio::{read_audio_packet_generic, PreviousWindowRight};
    use lewton::header::{read_header_ident, read_header_setup};

    // lewton panics (debug overflow) are expected on some garbage packets; keep
    // the output quiet.
    static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            default_hook(info);
        }
    }));
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut compared, mut both_err, mut lewton_panics, mut nonzero, mut nonfinite) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let streams = 300;
    for s in 0..streams {
        let st = make_stream(&mut r);
        let ours = Decoder::new(&st.ident, &st.comment, &st.setup);
        let ident = read_header_ident(&st.ident).unwrap();
        let lsetup = read_header_setup(
            &st.setup,
            ident.audio_channels,
            (ident.blocksize_0, ident.blocksize_1),
        );
        let (mut dec, lsetup) = match (ours, lsetup) {
            (Ok(d), Ok(l)) => (d, l),
            (Err(_), Err(_)) => continue,
            (o, l) => panic!(
                "stream {s}: header verdicts differ: ours {:?} lewton {:?}",
                o.err(),
                l.err()
            ),
        };
        let mut pwr = PreviousWindowRight::new();
        let ch = st.channels as usize;
        let mut buf = vec![0.0f32; dec.max_frames_per_packet() * ch];
        for p in 0..40 {
            let len = r.range(1, 600) as usize;
            let mut pkt: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
            pkt[0] &= !1; // audio packet
            if std::env::var("SYN_ONLY").is_ok_and(|v| v.parse::<usize>().unwrap() != s) {
                continue;
            }
            if let Ok(dir) = std::env::var("SYN_DUMP") {
                let d = std::path::Path::new(&dir);
                std::fs::create_dir_all(d).unwrap();
                if p == 0 {
                    std::fs::write(d.join("h0"), &st.ident).unwrap();
                    std::fs::write(d.join("h1"), &st.comment).unwrap();
                    std::fs::write(d.join("h2"), &st.setup).unwrap();
                }
                std::fs::write(d.join(format!("p{p:02}")), &pkt).unwrap();
            }
            let ours = dec.decode_interleaved(&pkt, &mut buf);
            QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
            let lew = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                read_audio_packet_generic::<Vec<Vec<f32>>>(&ident, &lsetup, &pkt, &mut pwr)
            }));
            QUIET.store(false, std::sync::atomic::Ordering::Relaxed);
            match (ours, lew) {
                (_, Err(_)) => {
                    lewton_panics += 1;
                    dec.reset();
                    pwr = PreviousWindowRight::new();
                }
                (Ok(n), Ok(Ok(l))) => {
                    assert_eq!(
                        n,
                        l.first().map_or(0, Vec::len),
                        "stream {s} packet {p}: frames"
                    );
                    // A degenerate floor-0 curve overflows to inf; inf*0 / inf-inf
                    // then depend on transform op order — not comparable.
                    if !l.iter().flatten().all(|v| v.is_finite())
                        || !buf[..n * ch].iter().all(|v| v.is_finite())
                    {
                        nonfinite += 1;
                        dec.reset();
                        pwr = PreviousWindowRight::new();
                        continue;
                    }
                    let peak = l.iter().flatten().fold(1.0f32, |a, &b| a.max(b.abs()));
                    if std::env::var("SYN_STATS").is_ok() {
                        let mut md = 0.0f32;
                        let mut per = Vec::new();
                        for c in 0..ch {
                            let mut mc = 0.0f32;
                            for f in 0..n {
                                mc = mc.max((buf[f * ch + c] - l[c][f]).abs());
                            }
                            per.push(format!("{mc:.2e}"));
                            md = md.max(mc);
                        }
                        if md / peak > 1e-3 {
                            println!("PER {per:?}");
                            for f in 0..n.min(8) {
                                println!(
                                    "  f{f}: ours {:?} lewton {:?}",
                                    (0..ch).map(|c| buf[f * ch + c]).collect::<Vec<_>>(),
                                    (0..ch).map(|c| l[c][f]).collect::<Vec<_>>()
                                );
                            }
                        }
                        println!(
                            "REL {:.3e} peak {peak} s{s} p{p} n{n} pkt0={:02x} {}",
                            md / peak,
                            pkt[0],
                            st.desc
                        );
                    }
                    for c in 0..ch {
                        for f in 0..n {
                            let (a, b) = (buf[f * ch + c], l[c][f]);
                            if a.is_finite() || b.is_finite() {
                                assert!(
                                    (a - b).abs() <= 2e-5 * peak,
                                    "stream {s} packet {p} ch {c} frame {f}: {a} vs {b} (peak {peak}) [{}] mode bit {}",
                                    st.desc,
                                    (pkt[0] >> 1) & 1
                                );
                            }
                        }
                    }
                    if l.iter().flatten().any(|&v| v != 0.0) {
                        nonzero += 1;
                    }
                    compared += 1;
                }
                (Err(_), Ok(Err(_))) => {
                    both_err += 1;
                    dec.reset();
                    pwr = PreviousWindowRight::new();
                }
                (o, Ok(l)) => panic!(
                    "stream {s} packet {p}: verdicts differ: ours {:?} lewton {:?}",
                    o.map_err(|e| e.to_string()),
                    l.map(|_| ()).map_err(|e| format!("{e:?}"))
                ),
            }
        }
    }
    println!(
        "{streams} streams: {compared} packets compared ({nonzero} with nonzero audio), {both_err} rejected by both, {lewton_panics} lewton panics skipped, {nonfinite} non-finite skipped"
    );
    assert!(compared > streams * 20 && nonzero > streams * 5);
}
