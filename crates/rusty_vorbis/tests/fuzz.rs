//! Robustness: mutated / truncated / random header and audio packets must
//! never panic (run in the debug profile, so integer overflow panics too),
//! and hostile headers must not be able to request unbounded allocation.

#[path = "common/ogg.rs"]
mod ogg;

use rusty_vorbis::decode::Decoder;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(r: &mut Rng, src: &[u8]) -> Vec<u8> {
    let mut v = src.to_vec();
    match r.below(6) {
        0 => {
            for _ in 0..=r.below(8) {
                if !v.is_empty() {
                    let i = r.below(v.len());
                    v[i] ^= 1 << r.below(8);
                }
            }
        }
        1 => {
            let n = r.below(v.len() + 1);
            v.truncate(n);
        }
        2 => {
            let i = r.below(v.len() + 1);
            let extra: Vec<u8> = (0..r.below(64)).map(|_| r.next() as u8).collect();
            v.splice(i..i, extra);
        }
        3 => {
            // Random bytes after the fixed prefix.
            let keep = 7.min(v.len());
            v.truncate(keep);
            v.extend((0..r.below(600)).map(|_| r.next() as u8));
        }
        4 => {
            // Overwrite a byte with an extreme value.
            if !v.is_empty() {
                let i = r.below(v.len());
                v[i] = [0x00, 0xff, 0x7f, 0x80][r.below(4)];
            }
        }
        _ => {
            for _ in 0..=r.below(3) {
                if !v.is_empty() {
                    let i = r.below(v.len());
                    v[i] = r.next() as u8;
                }
            }
        }
    }
    v
}

fn fixture_packets(name: &str) -> Vec<Vec<u8>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    ogg::read_packets(&std::fs::read(path).unwrap())
        .into_iter()
        .map(|p| p.data)
        .collect()
}

/// Decode a packet list; returns (decoded ok, errored).
fn run(dec: &mut Decoder, packets: &[Vec<u8>], buf: &mut Vec<f32>) -> (usize, usize) {
    buf.resize(dec.max_frames_per_packet() * dec.channels(), 0.0);
    let (mut ok, mut err) = (0, 0);
    for p in packets {
        match dec.decode_interleaved(p, buf) {
            Ok(n) => {
                ok += 1;
                assert!(n <= dec.max_frames_per_packet());
            }
            Err(_) => err += 1,
        }
        let _ = dec.packet_frames(p);
    }
    (ok, err)
}

#[test]
fn mutated_headers_and_packets_never_panic() {
    let base = fixture_packets("short.ogg");
    let mut r = Rng(0xDEAD_BEEF_CAFE_F00D);
    let mut buf = Vec::new();
    let (mut built, mut rejected, mut decoded) = (0usize, 0usize, 0usize);
    let iters = 3000;
    for it in 0..iters {
        let mut h = [base[0].clone(), base[1].clone(), base[2].clone()];
        // Mutate one or more headers (the setup most often).
        let which = r.below(10);
        let targets: &[usize] = match which {
            0 => &[0],
            1 => &[1],
            2 => &[0, 2],
            _ => &[2],
        };
        for &t in targets {
            h[t] = mutate(&mut r, &h[t]);
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match Decoder::new(&h[0], &h[1], &h[2]) {
                Ok(mut dec) => {
                    // Audio: originals, some mutated, some random.
                    let mut pk: Vec<Vec<u8>> = base[3..].to_vec();
                    for p in pk.iter_mut() {
                        if r.below(2) == 0 {
                            *p = mutate(&mut r, p);
                        }
                    }
                    pk.push(Vec::new());
                    pk.push((0..r.below(300)).map(|_| r.next() as u8 & 0xfe).collect());
                    let (ok, _) = run(&mut dec, &pk, &mut buf);
                    (1usize, ok)
                }
                Err(_) => (0, 0),
            }
        }));
        match res {
            Ok((b, ok)) => {
                built += b;
                rejected += 1 - b;
                decoded += ok;
            }
            Err(_) => panic!("panic on fuzz iteration {it}"),
        }
    }
    println!("{iters} iterations: {built} headers accepted, {rejected} rejected, {decoded} packets decoded");
    assert!(built > 100 && rejected > 100);
}

#[test]
fn mutated_audio_packets_never_panic() {
    let base = fixture_packets("short.ogg");
    let mut dec = Decoder::new(&base[0], &base[1], &base[2]).unwrap();
    let mut r = Rng(0x0123_4567_89AB_CDEF);
    let mut buf = Vec::new();
    let (mut ok, mut err) = (0, 0);
    for _ in 0..4000 {
        let src = &base[3 + r.below(base.len() - 3)];
        let p = mutate(&mut r, src);
        let (o, e) = run(&mut dec, std::slice::from_ref(&p), &mut buf);
        ok += o;
        err += e;
        if r.below(20) == 0 {
            dec.reset();
        }
    }
    println!("audio fuzz: {ok} decoded, {err} rejected");
    assert!(ok > 1000);
}

/// A tiny setup header declaring an ordered codebook with 2^24 entries of
/// length 24 (a complete code, a handful of header bits) must be refused by the
/// allocation cap rather than allocate/build gigabytes of tables.
#[test]
fn hostile_ordered_codebook_is_bounded() {
    use rusty_vorbis::BitWriter;
    let base = fixture_packets("short.ogg");
    let mut w = BitWriter::new();
    for &b in b"\x05vorbis" {
        w.write(u32::from(b), 8);
    }
    w.write(0, 8); // one codebook
    w.write(0x56_4342, 24);
    w.write(1, 16); // dim
    w.write((1 << 24) - 1, 24); // entries (max)
    w.write(1, 1); // ordered
    w.write(23, 5); // first length 24
    w.write((1 << 24) - 1, 24); // all entries at that length
    w.write(1, 4); // lookup type 1
    w.write(0, 32);
    w.write(0, 32);
    w.write(0, 4);
    w.write(0, 1);
    for _ in 0..64 {
        w.write(0, 32);
    }
    let setup = w.into_bytes();
    let t = std::time::Instant::now();
    assert!(Decoder::new(&base[0], &base[1], &setup).is_err());
    assert!(t.elapsed().as_secs() < 5);
}
