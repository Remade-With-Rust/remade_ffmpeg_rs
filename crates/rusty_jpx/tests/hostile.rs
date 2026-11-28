//! Damaged and hostile input: errors, never a panic, a hang or a huge
//! allocation.

use std::time::{Duration, Instant};

use rusty_jpx::{
    decode_jpx, decode_jpx_with, info, ConcealmentKind, DecodeOptions, JpxError, Limits,
};

fn corpus() -> Vec<(String, Vec<u8>)> {
    files_in(&format!("{}/tests/data", env!("CARGO_MANIFEST_DIR")))
}

fn files_in(dir: &str) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "j2k" || x == "jp2"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&p).unwrap())
        })
        .collect();
    files.sort();
    files
}

fn small_limits() -> Limits {
    let mut l = Limits::default();
    l.max_samples = 1 << 18;
    l.max_structures = 1 << 14;
    l
}

/// A truncated file errors, or (when only trailing bytes that carry no image
/// data are gone) decodes to exactly the whole image.
#[test]
fn truncation_errors_or_is_harmless() {
    for (name, file) in corpus() {
        let whole = decode_jpx(&file).unwrap();
        let step = (file.len() / 200).max(1);
        let tail = file.len().saturating_sub(40)..file.len();
        for n in (0..file.len()).step_by(step).chain(tail) {
            let strict = decode_jpx(&file[..n]);
            if let Ok(j) = &strict {
                assert_eq!(j.components, whole.components, "{name} cut at {n}");
            }
            let _ = info(&file[..n]);
            let lenient = DecodeOptions::new().with_allow_truncated(true);
            if let Ok(j) = decode_jpx_with(&file[..n], &lenient) {
                assert_eq!((j.width, j.height), (whole.width, whole.height));
                assert!(j.truncated || j.components == whole.components);
                // Damage is never concealed silently.
                assert_eq!(j.truncated, !j.concealed.is_empty(), "{name} cut at {n}");
                assert_eq!(j.truncated, strict.is_err(), "{name} cut at {n}");
            }
        }
        // Cutting into the packet data is always an error.
        let cut = file.len() * 3 / 4;
        assert!(decode_jpx(&file[..cut]).is_err(), "{name}");
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

#[test]
fn mutations_never_panic() {
    let rounds = std::env::var("JPX_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut slowest = Duration::ZERO;
    // A directory of further seeds, such as the conformance suite.
    let extra = std::env::var("JPX_FUZZ_DIR").map_or(vec![], |d| files_in(&d));
    for (name, file) in corpus().into_iter().chain(extra) {
        for round in 0..rounds {
            let mut d = file.clone();
            for _ in 0..=rng.below(4) {
                // Half of the hits land in the headers.
                let span = if rng.below(2) == 0 {
                    d.len().min(200)
                } else {
                    d.len()
                };
                let i = rng.below(span);
                d[i] = match rng.below(4) {
                    0 => rng.below(256) as u8,
                    1 => 0xFF,
                    2 => 0,
                    _ => d[i] ^ (1 << rng.below(8)),
                };
            }
            if rng.below(8) == 0 {
                d.truncate(rng.below(d.len()));
            }
            let options = DecodeOptions::new()
                .with_limits(small_limits())
                .with_allow_truncated(round % 2 == 0)
                .with_reduce((rng.below(8) == 0).into());
            let t = Instant::now();
            let r = decode_jpx_with(&d, &options);
            let _ = info(&d);
            let t = t.elapsed();
            if let Ok(j) = &r {
                assert_eq!(j.truncated, !j.concealed.is_empty(), "{name} round {round}");
                // What strict mode decodes, lenient mode decodes the same,
                // with nothing concealed.
                if options.allow_truncated {
                    let strict = decode_jpx_with(&d, &options.clone().with_allow_truncated(false));
                    match strict {
                        Ok(s) => assert!(
                            !j.truncated && s == *j,
                            "{name} round {round}: {:?}",
                            j.concealed
                        ),
                        Err(e) => assert!(j.truncated, "{name} round {round}: strict {e}"),
                    }
                }
            }
            assert!(t < Duration::from_secs(30), "{name} round {round}: {t:?}");
            slowest = slowest.max(t);
        }
    }
    println!("slowest {slowest:?}");
}

/// A codestream header: SIZ, COD and QCD for 5/3, then one tile-part.
struct Header {
    size: (u32, u32),
    tile: (u32, u32),
    comps: u16,
    order: u8,
    layers: u16,
    levels: u8,
    /// Code-block exponents minus two.
    block: (u8, u8),
    precinct: Option<u8>,
}

impl Header {
    fn new(size: (u32, u32)) -> Self {
        Header {
            size,
            tile: size,
            comps: 1,
            order: 0,
            layers: 1,
            levels: 0,
            block: (4, 4),
            precinct: None,
        }
    }

    fn write(&self, packets: &[u8]) -> Vec<u8> {
        let mut d = vec![0xFF, 0x4F, 0xFF, 0x51];
        d.extend((38 + 3 * self.comps).to_be_bytes());
        d.extend([0, 0]);
        for v in [
            self.size.0,
            self.size.1,
            0,
            0,
            self.tile.0,
            self.tile.1,
            0,
            0,
        ] {
            d.extend(v.to_be_bytes());
        }
        d.extend(self.comps.to_be_bytes());
        for _ in 0..self.comps {
            d.extend([7, 1, 1]);
        }
        d.extend([0xFF, 0x52]);
        let precincts = self
            .precinct
            .map_or(vec![], |p| vec![p; self.levels as usize + 1]);
        d.extend((12 + precincts.len() as u16).to_be_bytes());
        d.extend([u8::from(self.precinct.is_some()), self.order]);
        d.extend(self.layers.to_be_bytes());
        d.extend([0, self.levels, self.block.0, self.block.1, 0, 1]);
        d.extend(precincts);
        d.extend([0xFF, 0x5C]);
        let steps = 3 * u16::from(self.levels) + 1;
        d.extend((3 + steps).to_be_bytes());
        d.push(0x40);
        d.extend(vec![0x40; steps.into()]);
        d.extend([0xFF, 0x90, 0, 10, 0, 0]);
        d.extend((14 + packets.len() as u32).to_be_bytes());
        d.extend([0, 1, 0xFF, 0x93]);
        d.extend(packets);
        d.extend([0xFF, 0xD9]);
        d
    }
}

fn decode_in_time(d: &[u8]) -> Result<rusty_jpx::Jpx, JpxError> {
    let t = Instant::now();
    let r = decode_jpx_with(d, &DecodeOptions::new().with_allow_truncated(true));
    assert!(t.elapsed() < Duration::from_secs(60), "{:?}", t.elapsed());
    r
}

#[test]
fn an_empty_packet_is_mid_grey() {
    let j = decode_jpx(&Header::new((8, 8)).write(&[0])).unwrap();
    assert!(j.components[0].data.iter().all(|&v| v == 128));
}

#[test]
fn huge_dimensions_hit_the_sample_limit() {
    let h = Header::new((u32::MAX, u32::MAX));
    assert!(matches!(
        decode_in_time(&h.write(&[0])),
        Err(JpxError::Limit(_))
    ));
    let mut h = Header::new((1 << 14, 1 << 14));
    h.comps = 1000;
    assert!(matches!(
        decode_in_time(&h.write(&[0])),
        Err(JpxError::Limit(_))
    ));
}

#[test]
fn huge_tile_and_component_counts() {
    let mut h = Header::new((65535, 1));
    h.tile = (1, 1);
    assert_eq!(decode_jpx(&h.write(&[0])), Err(JpxError::Truncated));
    assert!(decode_in_time(&h.write(&[0])).is_ok());
    h.size = (65536, 1);
    assert!(matches!(
        decode_jpx(&h.write(&[0])),
        Err(JpxError::Format(_))
    ));
    let mut h = Header::new((4, 4));
    h.comps = 16384;
    assert!(matches!(
        decode_jpx(&h.write(&[0])),
        Err(JpxError::Limit(_))
    ));
}

#[test]
fn huge_precinct_counts() {
    for order in 0..5 {
        let mut h = Header::new((8192, 8192));
        h.order = order;
        h.layers = 65535;
        h.precinct = Some(0);
        // 2^26 one-sample precincts.
        assert!(matches!(
            decode_in_time(&h.write(&[0; 64])),
            Err(JpxError::Limit(_))
        ));
        // Under the limit, with far fewer packets than precincts and layers.
        h.size = (1024, 1024);
        h.tile = h.size;
        let d = h.write(&[0; 64]);
        assert!(decode_jpx(&d[..d.len() - 10]).is_err());
        assert!(decode_in_time(&d).is_ok());
    }
}

#[test]
fn smallest_blocks_hit_the_structure_limit() {
    let mut h = Header::new((16384, 16384));
    h.block = (0, 0);
    // One precinct of 2^24 blocks.
    assert!(matches!(
        decode_in_time(&h.write(&[0x80])),
        Err(JpxError::Limit(_))
    ));
}

#[test]
fn many_layers_and_levels_without_data_end_early() {
    let mut h = Header::new((64, 64));
    h.layers = 65535;
    h.levels = 32;
    assert!(decode_in_time(&h.write(&[0; 8])).is_ok());
}

/// Tens of thousands of tile-parts, each of them cheap to write and costly to
/// set up.
#[test]
fn many_tiles_of_many_components() {
    let mut h = Header::new((255, 257));
    h.tile = (1, 1);
    h.comps = 1000;
    let one = h.write(&[]);
    let sot = one.windows(2).position(|w| w == [0xFF, 0x90]).unwrap();
    let mut d = one[..sot].to_vec();
    for t in 0..65535u16 {
        d.extend([0xFF, 0x90, 0, 10]);
        d.extend(t.to_be_bytes());
        d.extend([0, 0, 0, 14, 0, 1, 0xFF, 0x93]);
    }
    assert!(matches!(decode_in_time(&d), Err(JpxError::Limit(_))));
}

fn lenient() -> DecodeOptions {
    DecodeOptions::new().with_allow_truncated(true)
}

/// A code-block whose data runs past the end of the tile, then the packets of
/// the second layer missing, in a codestream that otherwise ends properly:
/// once reported as whole (`truncated` false).
#[test]
fn a_cut_code_block_is_reported_even_at_a_clean_end() {
    let mut h = Header::new((8, 8));
    h.layers = 2;
    // Packet header: not empty, included, no zero bit-planes, one pass, 7
    // bytes; then only 2 of them.
    let d = h.write(&[0b1110_0111, 0, 0]);
    assert_eq!(decode_jpx(&d), Err(JpxError::Truncated));
    let j = decode_jpx_with(&d, &lenient()).unwrap();
    assert!(j.truncated);
    assert!(j
        .concealed
        .iter()
        .any(|c| c.tile == Some(0) && c.kind == ConcealmentKind::CodeBlocksCut));
}

#[test]
fn missing_tiles_are_listed() {
    let mut h = Header::new((4, 1));
    h.tile = (1, 1);
    let d = h.write(&[0]);
    assert_eq!(decode_jpx(&d), Err(JpxError::Truncated));
    let j = decode_jpx_with(&d, &lenient()).unwrap();
    let missing: Vec<_> = j
        .concealed
        .iter()
        .filter(|c| c.kind == ConcealmentKind::TileMissing)
        .map(|c| c.tile)
        .collect();
    assert_eq!(missing, [Some(1), Some(2), Some(3)]);
    // Zero coefficients: the DC level of 8-bit unsigned samples.
    assert_eq!(j.components[0].data[1..], [128; 3]);
    assert_eq!(
        j.concealed[0].to_string(),
        "tile 1: missing, zero coefficients"
    );
}

/// A tile-part whose header is damaged is skipped; the tile-parts after it
/// are still read.
#[test]
fn a_damaged_tile_part_header_is_skipped() {
    let file = std::fs::read(format!(
        "{}/tests/data/rgb8_tiles_rpcl.jp2",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let whole = decode_jpx(&file).unwrap();
    let sots: Vec<usize> = file
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == [0xFF, 0x90, 0, 10])
        .map(|(i, _)| i)
        .collect();
    assert!(sots.len() >= 3, "{}", sots.len());
    let sod = sots[1]
        + file[sots[1]..]
            .windows(2)
            .position(|w| w == [0xFF, 0x93])
            .unwrap();
    let mut d = file.clone();
    d[sod + 1] = 0x00; // SOD is gone: the header runs into the packet data
    assert!(matches!(decode_jpx(&d), Err(JpxError::Format(_))));
    let j = decode_jpx_with(&d, &lenient()).unwrap();
    assert!(j.truncated);
    assert!(matches!(
        j.concealed[0].kind,
        ConcealmentKind::TilePartSkipped(_)
    ));
    // The image is whole but for that tile.
    let (w, h) = (
        whole.components[0].width as usize,
        whole.components[0].height as usize,
    );
    let differing = (0..w * h)
        .filter(|&i| j.components[0].data[i] != whole.components[0].data[i])
        .count();
    assert!(differing > 0 && differing < w * h / 2, "{differing}");
}

#[test]
fn garbage() {
    assert!(decode_jpx(&[]).is_err());
    assert!(decode_jpx(b"hello, world, this is not an image").is_err());
    assert!(decode_jpx(&[0xFF, 0x4F, 0xFF, 0x51]).is_err());
    assert!(decode_jpx(&[0xFF; 4096]).is_err());
    assert!(decode_jpx(&[0; 4096]).is_err());
}
