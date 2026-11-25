//! Shared test helpers: digests, the stream manifest, segment writing, and a
//! plain T.88 encoder written from Annex E and the template figures, one
//! pixel at a time, as an independent check of the decoder's windowed code.
#![allow(dead_code)]

use std::path::PathBuf;

use rusty_jbig2::Bitmap1;

pub fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

pub fn read(name: &str) -> Vec<u8> {
    let p = data_dir().join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

pub fn digest(b: &Bitmap1) -> String {
    let mut x = 0xcbf2_9ce4_8422_2325u64;
    let dims = b
        .width
        .to_le_bytes()
        .into_iter()
        .chain(b.height.to_le_bytes());
    for v in dims.chain(b.data.iter().copied()) {
        x = (x ^ u64::from(v)).wrapping_mul(0x100_0000_01b3);
    }
    format!("{x:016x}")
}

pub struct Entry {
    pub name: String,
    pub page: usize,
    pub strict: String,
    pub lenient: String,
}

impl Entry {
    pub fn data(&self) -> Vec<u8> {
        read(&format!("{}.jbig2", self.name))
    }
}

pub fn manifest() -> Vec<Entry> {
    let text = String::from_utf8(read("manifest.txt")).unwrap();
    let mut refs = std::collections::HashMap::new();
    let mut out = Vec::new();
    for l in text.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if let Some(name) = f[0].strip_prefix('@') {
            refs.insert(name.to_string(), f[1].to_string());
            continue;
        }
        let get = |s: &str| match s.strip_prefix("ref:") {
            Some(n) => refs[n].clone(),
            None => s.to_string(),
        };
        out.push(Entry {
            name: f[0].to_string(),
            page: f[1].parse().unwrap(),
            strict: get(f[2]),
            lenient: get(f[3]),
        });
    }
    out
}

/// A small deterministic generator (xorshift).
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }
    pub fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo + 1) as u32) as i32
    }
}

/// An unpacked image with white outside.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Img {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
}

impl Img {
    pub fn new(w: i32, h: i32) -> Self {
        Self {
            w,
            h,
            px: vec![0; (w * h) as usize],
        }
    }
    pub fn get(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            0
        } else {
            self.px[(y * self.w + x) as usize]
        }
    }
    pub fn set(&mut self, x: i32, y: i32, v: u8) {
        self.px[(y * self.w + x) as usize] = v;
    }
    pub fn packed(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for row in self.px.chunks(self.w as usize) {
            for c in row.chunks(8) {
                out.push(c.iter().enumerate().fold(0, |b, (i, v)| b | v << (7 - i)));
            }
        }
        out
    }
    /// Blobs, runs and noise, with repeated rows so typical prediction hits.
    pub fn random(r: &mut Rng, w: i32, h: i32) -> Self {
        let mut m = Self::new(w, h);
        for _ in 0..r.below(6) {
            let (x, y) = (r.range(-8, w), r.range(-8, h));
            let (bw, bh) = (r.range(1, w), r.range(1, h));
            let v = u8::from(r.below(4) != 0);
            for yy in y.max(0)..(y + bh).min(h) {
                for xx in x.max(0)..(x + bw).min(w) {
                    m.set(xx, yy, v);
                }
            }
        }
        for _ in 0..r.below((w * h / 8 + 1) as u32) {
            let (x, y) = (r.range(0, w - 1), r.range(0, h - 1));
            m.set(x, y, 1 ^ m.get(x, y));
        }
        for _ in 0..r.below(h as u32 + 1) {
            let y = r.range(0, h - 1);
            for x in 0..w {
                m.set(x, y, m.get(x, y - 1));
            }
        }
        m
    }
}

/// (Qe, NMPS, NLPS, SWITCH) of Table E.1, as printed.
const QE: [(u32, u8, u8, bool); 47] = {
    const fn q(qe: u32, nm: u8, nl: u8) -> (u32, u8, u8, bool) {
        (qe, nm, nl, false)
    }
    const fn s(qe: u32, nm: u8, nl: u8) -> (u32, u8, u8, bool) {
        (qe, nm, nl, true)
    }
    [
        s(0x5601, 1, 1),
        q(0x3401, 2, 6),
        q(0x1801, 3, 9),
        q(0x0AC1, 4, 12),
        q(0x0521, 5, 29),
        q(0x0221, 38, 33),
        s(0x5601, 7, 6),
        q(0x5401, 8, 14),
        q(0x4801, 9, 14),
        q(0x3801, 10, 14),
        q(0x3001, 11, 17),
        q(0x2401, 12, 18),
        q(0x1C01, 13, 20),
        q(0x1601, 29, 21),
        s(0x5601, 15, 14),
        q(0x5401, 16, 14),
        q(0x5101, 17, 15),
        q(0x4801, 18, 16),
        q(0x3801, 19, 17),
        q(0x3401, 20, 18),
        q(0x3001, 21, 19),
        q(0x2801, 22, 19),
        q(0x2401, 23, 20),
        q(0x2201, 24, 21),
        q(0x1C01, 25, 22),
        q(0x1801, 26, 23),
        q(0x1601, 27, 24),
        q(0x1401, 28, 25),
        q(0x1201, 29, 26),
        q(0x1101, 30, 27),
        q(0x0AC1, 31, 28),
        q(0x09C1, 32, 29),
        q(0x08A1, 33, 30),
        q(0x0521, 34, 31),
        q(0x0441, 35, 32),
        q(0x02A1, 36, 33),
        q(0x0221, 37, 34),
        q(0x0141, 38, 35),
        q(0x0111, 39, 36),
        q(0x0085, 40, 37),
        q(0x0049, 41, 38),
        q(0x0025, 42, 39),
        q(0x0015, 43, 40),
        q(0x0009, 44, 41),
        q(0x0005, 45, 42),
        q(0x0001, 45, 43),
        q(0x5601, 46, 46),
    ]
};

/// The Annex E encoder (Figures E.3 to E.12).
pub struct Encoder {
    a: u32,
    c: u32,
    ct: i32,
    /// `out[0]` is the imaginary byte before the first.
    out: Vec<u8>,
}

/// Context states: (index, MPS).
pub type Contexts = std::collections::HashMap<u32, (u8, u8)>;

impl Encoder {
    pub fn new() -> Self {
        Self {
            a: 0x8000,
            c: 0,
            ct: 12,
            out: vec![0],
        }
    }

    fn byte_out(&mut self) {
        let b = self.out.last_mut().unwrap();
        if *b == 0xFF {
            self.out.push((self.c >> 20) as u8);
            self.c &= 0xF_FFFF;
            self.ct = 7;
            return;
        }
        if self.c >= 0x800_0000 {
            *b += 1;
            if *b == 0xFF {
                self.c &= 0x7FF_FFFF;
                self.out.push((self.c >> 20) as u8);
                self.c &= 0xF_FFFF;
                self.ct = 7;
                return;
            }
        }
        self.out.push((self.c >> 19) as u8);
        self.c &= 0x7_FFFF;
        self.ct = 8;
    }

    fn renorm(&mut self) {
        loop {
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.ct == 0 {
                self.byte_out();
            }
            if self.a & 0x8000 != 0 {
                return;
            }
        }
    }

    pub fn bit(&mut self, cx: &mut Contexts, label: u32, d: u8) {
        let st = cx.entry(label).or_insert((0, 0));
        let (qe, nm, nl, sw) = QE[st.0 as usize];
        self.a -= qe;
        if d == st.1 {
            if self.a & 0x8000 == 0 {
                if self.a < qe {
                    self.a = qe;
                } else {
                    self.c += qe;
                }
                st.0 = nm;
                self.renorm();
            } else {
                self.c += qe;
            }
        } else {
            if self.a < qe {
                self.c += qe;
            } else {
                self.a = qe;
            }
            if sw {
                st.1 ^= 1;
            }
            st.0 = nl;
            self.renorm();
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        let t = self.c + self.a;
        self.c |= 0xFFFF;
        if self.c >= t {
            self.c -= 0x8000;
        }
        self.c <<= self.ct;
        self.byte_out();
        self.c <<= self.ct;
        self.byte_out();
        if *self.out.last().unwrap() != 0xFF {
            self.out.push(0xFF);
        }
        self.out.push(0xAC);
        self.out.remove(0);
        self.out
    }
}

pub type At = Vec<(i32, i32)>;

/// Nominal AT pixels (Table 5), and the fixed pixels of each template.
pub fn nominal_at(t: u8, ext: bool) -> At {
    match (t, ext) {
        (0, false) => vec![(3, -1), (-3, -1), (2, -2), (-2, -2)],
        (0, true) => vec![
            (-2, 0),
            (0, -2),
            (-2, -1),
            (-1, -2),
            (1, -2),
            (2, -1),
            (-3, 0),
            (-4, 0),
            (2, -2),
            (3, -1),
            (-2, -2),
            (-3, -1),
        ],
        (1, _) => vec![(3, -1)],
        _ => vec![(2, -1)],
    }
}

fn fixed(t: u8, ext: bool) -> At {
    let mut v = Vec::new();
    let mut add = |y: i32, xs: std::ops::RangeInclusive<i32>| v.extend(xs.map(|x| (x, y)));
    match (t, ext) {
        (0, false) => {
            add(-2, -1..=1);
            add(-1, -2..=2);
            add(0, -4..=-1);
        }
        (0, true) => {
            add(-1, -1..=1);
            add(0, -1..=-1);
        }
        (1, _) => {
            add(-2, -1..=2);
            add(-1, -2..=2);
            add(0, -3..=-1);
        }
        (2, _) => {
            add(-2, -1..=1);
            add(-1, -2..=1);
            add(0, -2..=-1);
        }
        _ => {
            add(-1, -3..=1);
            add(0, -4..=-1);
        }
    }
    v
}

/// Black pixels of the SLTP contexts (Figures 8 to 11), as offsets.
fn sltp_black(t: u8) -> At {
    match t {
        0 => vec![
            (-2, -2),
            (1, -2),
            (2, -2),
            (-2, -1),
            (-1, -1),
            (2, -1),
            (-3, 0),
            (-1, 0),
        ],
        1 => vec![
            (1, -2),
            (2, -2),
            (-2, -1),
            (-1, -1),
            (2, -1),
            (-3, 0),
            (-1, 0),
        ],
        2 => vec![(1, -2), (-2, -1), (-1, -1), (2, -1), (-1, 0)],
        _ => vec![(-2, -1), (-1, -1), (2, -1), (-3, 0), (-1, 0)],
    }
}

/// Encode a generic region's pixels (6.2).
pub fn encode_generic(m: &Img, t: u8, ext: bool, at: &At, tpgd: bool) -> Vec<u8> {
    let mut e = Encoder::new();
    let mut cx = Contexts::new();
    let fixed = fixed(t, ext);
    // Context bits: fixed pixels, then AT pixels.
    let black = sltp_black(t);
    let sltp = fixed
        .iter()
        .chain(&nominal_at(t, ext))
        .fold(0u32, |c, o| c << 1 | u32::from(black.contains(o)));
    let mut ltp = false;
    for y in 0..m.h {
        if tpgd {
            let same = (0..m.w).all(|x| m.get(x, y) == m.get(x, y - 1));
            e.bit(&mut cx, sltp, u8::from(same != ltp));
            ltp = same;
            if same {
                continue;
            }
        }
        for x in 0..m.w {
            let c = fixed.iter().chain(at).fold(0u32, |c, &(dx, dy)| {
                c << 1 | u32::from(m.get(x + dx, y + dy))
            });
            e.bit(&mut cx, c, m.get(x, y));
        }
    }
    e.finish()
}

/// Encode a refinement of `r` into `m` (6.3), reference offset 0.
pub fn encode_refinement(m: &Img, r: &Img, one: bool, at: &At, tpgr: bool) -> Vec<u8> {
    let mut e = Encoder::new();
    let mut cx = Contexts::new();
    let mut ltp = false;
    // Pixels from the bitmap being coded, then from the reference.
    let (own, reference): (At, At) = if one {
        (
            vec![(-1, -1), (0, -1), (1, -1), (-1, 0)],
            vec![(0, -1), (-1, 0), (0, 0), (1, 0), (0, 1), (1, 1)],
        )
    } else {
        (
            vec![(0, -1), (1, -1), (-1, 0), at[0]],
            vec![
                (0, -1),
                (1, -1),
                (-1, 0),
                (0, 0),
                (1, 0),
                (-1, 1),
                (0, 1),
                (1, 1),
                at[1],
            ],
        )
    };
    // Only the reference's centre pixel is black in Figures 14 and 15.
    let centre = reference.iter().position(|&o| o == (0, 0)).unwrap();
    let sltp = 1u32 << (reference.len() - 1 - centre);
    for y in 0..m.h {
        let flat = |x: i32| {
            let v = r.get(x - 1, y - 1);
            let same = (-1..=1).all(|dy| (-1..=1).all(|dx| r.get(x + dx, y + dy) == v));
            same.then_some(v)
        };
        if tpgr {
            let typical = (0..m.w).all(|x| flat(x).is_none_or(|v| v == m.get(x, y)));
            e.bit(&mut cx, sltp, u8::from(typical != ltp));
            ltp = typical;
        }
        for x in 0..m.w {
            if ltp && flat(x).is_some() {
                continue;
            }
            let mut c = 0;
            for &(dx, dy) in &own {
                c = c << 1 | u32::from(m.get(x + dx, y + dy));
            }
            for &(dx, dy) in &reference {
                c = c << 1 | u32::from(r.get(x + dx, y + dy));
            }
            e.bit(&mut cx, c, m.get(x, y));
        }
    }
    e.finish()
}

/// A segment for page 1 with short-form references.
pub fn segment(number: u32, kind: u8, refs: &[u8], data: &[u8]) -> Vec<u8> {
    segment_len(number, kind, refs, data, data.len() as u32)
}

pub fn segment_len(number: u32, kind: u8, refs: &[u8], data: &[u8], len: u32) -> Vec<u8> {
    let mut v = number.to_be_bytes().to_vec();
    v.push(kind);
    if refs.len() <= 4 {
        v.push((refs.len() as u8) << 5);
    } else {
        v.extend((0xE000_0000u32 | refs.len() as u32).to_be_bytes());
        v.extend(vec![0; (refs.len() + 1).div_ceil(8)]);
    }
    v.extend(refs);
    v.push(1);
    v.extend(len.to_be_bytes());
    v.extend(data);
    v
}

/// Page information segment 0.
pub fn page_info(w: u32, h: u32, flags: u8, striping: u16) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend(w.to_be_bytes());
    d.extend(h.to_be_bytes());
    d.extend([0; 8]);
    d.push(flags);
    d.extend(striping.to_be_bytes());
    segment(0, 48, &[], &d)
}

/// Region segment information field.
pub fn region(w: u32, h: u32, x: u32, y: u32, op: u8) -> Vec<u8> {
    let mut d = Vec::new();
    for v in [w, h, x, y] {
        d.extend(v.to_be_bytes());
    }
    d.push(op);
    d
}

/// Generic region segment data.
pub fn generic_data(
    m: &Img,
    x: u32,
    y: u32,
    op: u8,
    t: u8,
    ext: bool,
    at: &At,
    tpgd: bool,
) -> Vec<u8> {
    let mut d = region(m.w as u32, m.h as u32, x, y, op);
    d.push(t << 1 | u8::from(tpgd) << 3 | u8::from(ext) << 4);
    for &(ax, ay) in at {
        d.push(ax as i8 as u8);
        d.push(ay as i8 as u8);
    }
    d.extend(encode_generic(m, t, ext, at, tpgd));
    d
}
