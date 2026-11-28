//! Decoding into caller buffers (`decode_part_into`, `decode_level_into`,
//! `decode_bands`, `decode_chunk_into`) and encoding from borrowed planes
//! (`encode_ref`), each checked against the owned API.

use rusty_exr::{
    Channel, ChannelDesc, ChannelRef, ChannelSink, Compression, DecodeScratch, Decoder,
    EncodeOptions, Error, Image, ImageRef, SampleType, Samples, SamplesRef, Threads,
};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// HALF, FLOAT and UINT channels, smooth and noisy (NaNs and infinities
/// among the random bit patterns).
fn image(w: u32, h: u32, seed: u64) -> Image {
    let mut r = Rng(seed | 1);
    let n = (w * h) as usize;
    let smooth = |i: usize, k: usize| ((i % w as usize) as f32 * 0.1 + k as f32).sin() * 4.0;
    let mut img = Image::new(w, h)
        .with_channel(
            "A",
            Samples::F16(
                (0..n)
                    .map(|i| rusty_exr::f32_to_f16(smooth(i, 0)))
                    .collect(),
            ),
        )
        .with_channel("B", Samples::F32((0..n).map(|i| smooth(i, 1)).collect()))
        .with_channel("G", Samples::U32((0..n).map(|_| r.next() as u32).collect()))
        .with_channel("R", Samples::F16((0..n).map(|_| r.next() as u16).collect()))
        .with_channel(
            "Z",
            Samples::F32((0..n).map(|_| f32::from_bits(r.next() as u32)).collect()),
        );
    img.channels.sort_by(|a, b| a.desc.name.cmp(&b.desc.name));
    img
}

const ALL: [Compression; 10] = [
    Compression::None,
    Compression::Rle,
    Compression::Zips,
    Compression::Zip,
    Compression::Piz,
    Compression::Pxr24,
    Compression::B44,
    Compression::B44a,
    Compression::Dwaa,
    Compression::Dwab,
];

/// A caller plane of any sample type.
#[derive(Debug, Clone, PartialEq)]
enum Plane {
    F16(Vec<u16>),
    F32(Vec<u32>),
    U32(Vec<u32>),
}

impl Plane {
    fn new(ty: SampleType, n: usize, fill: u32) -> Self {
        match ty {
            SampleType::F16 => Self::F16(vec![fill as u16; n]),
            SampleType::F32 => Self::F32(vec![fill; n]),
            SampleType::U32 => Self::U32(vec![fill; n]),
        }
    }
    fn sink(&mut self) -> ChannelSink<'_> {
        match self {
            Self::F16(v) => ChannelSink::f16(v),
            // f32 planes are kept as bits so NaNs compare exactly; a
            // `Vec<u32>` cannot be an f32 sink, so convert at the edges.
            Self::F32(_) => unreachable!("use f32_sink"),
            Self::U32(v) => ChannelSink::u32(v),
        }
    }
    /// Row `r` (`w` samples) of a plane with `stride`, as bits.
    fn row(&self, r: usize, w: usize, stride: usize) -> Vec<u32> {
        let s = r * stride..r * stride + w;
        match self {
            Self::F16(v) => v[s].iter().map(|&x| u32::from(x)).collect(),
            Self::F32(v) | Self::U32(v) => v[s].to_vec(),
        }
    }
}

fn bits(s: &Samples) -> Vec<u32> {
    match s {
        Samples::F16(v) => v.iter().map(|&x| u32::from(x)).collect(),
        Samples::F32(v) => v.iter().map(|x| x.to_bits()).collect(),
        Samples::U32(v) => v.clone(),
    }
}

fn f32_bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// Decode into planes of each channel's own type, packed rows.
fn into_own_type(d: &Decoder<'_>, full: &Image) -> Vec<Vec<u32>> {
    let mut f32s: Vec<Vec<f32>> = Vec::new();
    let mut others: Vec<Plane> = Vec::new();
    for c in &full.channels {
        let n = c.width * c.height;
        if c.desc.sample_type == SampleType::F32 {
            f32s.push(vec![0.0; n]);
        } else {
            others.push(Plane::new(c.desc.sample_type, n, 0));
        }
    }
    {
        let (mut fi, mut oi) = (f32s.iter_mut(), others.iter_mut());
        let mut sinks: Vec<ChannelSink> = full
            .channels
            .iter()
            .map(|c| {
                if c.desc.sample_type == SampleType::F32 {
                    ChannelSink::f32(fi.next().unwrap())
                } else {
                    oi.next().unwrap().sink()
                }
            })
            .collect();
        d.decode_part_into(0, &mut sinks).unwrap();
    }
    let (mut fi, mut oi) = (f32s.iter(), others.iter());
    full.channels
        .iter()
        .map(|c| {
            if c.desc.sample_type == SampleType::F32 {
                f32_bits(fi.next().unwrap())
            } else {
                oi.next().unwrap().row(0, c.width * c.height, 0)
            }
        })
        .collect()
}

#[test]
fn into_matches_decode_part() {
    let img = image(37, 29, 5);
    for comp in ALL {
        for tiles in [None, Some((16u32, 8u32))] {
            let mut o = EncodeOptions::new(comp);
            o.tiles = tiles;
            let file = rusty_exr::encode(&img, &o).unwrap();
            for t in [Threads::Single, Threads::Count(3)] {
                let d = Decoder::new(&file).unwrap().threads(t);
                let full = d.decode_part(0).unwrap();
                let what = format!("{comp:?} {tiles:?} {t:?}");

                // Own sample types.
                let got = into_own_type(&d, &full);
                for (g, c) in got.iter().zip(&full.channels) {
                    assert_eq!(g, &bits(&c.samples), "own type {what} {}", c.desc.name);
                }

                // Everything into f32 == Samples::to_f32.
                let mut planes: Vec<Vec<f32>> = full
                    .channels
                    .iter()
                    .map(|c| vec![0.0; c.width * c.height])
                    .collect();
                let mut sinks: Vec<ChannelSink> =
                    planes.iter_mut().map(|p| ChannelSink::f32(p)).collect();
                d.decode_part_into(0, &mut sinks).unwrap();
                for (p, c) in planes.iter().zip(&full.channels) {
                    assert_eq!(f32_bits(p), f32_bits(&c.samples.to_f32()), "f32 {what}");
                }

                // FLOAT into f16 == f32_to_f16; strided rows keep padding.
                let stride = full.width() + 5;
                let mut halves: Vec<Vec<u16>> = full
                    .channels
                    .iter()
                    .map(|c| vec![0xabcd; (c.height - 1) * stride + c.width])
                    .collect();
                let mut sinks: Vec<ChannelSink> = halves
                    .iter_mut()
                    .zip(&full.channels)
                    .map(|(p, c)| match c.desc.sample_type {
                        SampleType::U32 => ChannelSink::skip(),
                        _ => ChannelSink::f16(p).with_stride(stride),
                    })
                    .collect();
                d.decode_part_into(0, &mut sinks).unwrap();
                for (p, c) in halves.iter().zip(&full.channels) {
                    let want: Vec<u16> = match &c.samples {
                        Samples::F16(v) => v.clone(),
                        Samples::F32(v) => v.iter().map(|&x| rusty_exr::f32_to_f16(x)).collect(),
                        Samples::U32(_) => {
                            assert!(p.iter().all(|&x| x == 0xabcd), "skipped {what}");
                            continue;
                        }
                    };
                    for r in 0..c.height {
                        assert_eq!(
                            &p[r * stride..r * stride + c.width],
                            &want[r * c.width..(r + 1) * c.width],
                            "f16 {what} {} row {r}",
                            c.desc.name
                        );
                        if r + 1 < c.height {
                            assert!(p[r * stride + c.width..(r + 1) * stride]
                                .iter()
                                .all(|&x| x == 0xabcd));
                        }
                    }
                }

                // Bands, run in reverse order on one thread.
                let mut planes: Vec<Vec<f32>> = full
                    .channels
                    .iter()
                    .map(|c| vec![0.0; c.width * c.height])
                    .collect();
                let mut sinks: Vec<ChannelSink> =
                    planes.iter_mut().map(|p| ChannelSink::f32(p)).collect();
                let mut bands = d.decode_bands(0, 0, 0, &mut sinks).unwrap();
                bands.reverse();
                let mut scratch = DecodeScratch::default();
                for b in bands {
                    b.decode(&mut scratch).unwrap();
                }
                drop(sinks);
                for (p, c) in planes.iter().zip(&full.channels) {
                    assert_eq!(f32_bits(p), f32_bits(&c.samples.to_f32()), "bands {what}");
                }

                // Chunks into windows of whole planes.
                let w = full.width();
                let y_min = full.data_window.y_min;
                let x_min = full.data_window.x_min;
                let mut planes: Vec<Vec<f32>> = full
                    .channels
                    .iter()
                    .map(|_| vec![0.0; w * full.height()])
                    .collect();
                for k in 0..d.chunk_count(0).unwrap() {
                    let r = d.chunk_rect(0, k).unwrap();
                    let at = (r.y0 - y_min) as usize * w + (r.x0 - x_min) as usize;
                    let mut sinks: Vec<ChannelSink> = planes
                        .iter_mut()
                        .map(|p| ChannelSink::f32(&mut p[at..]).with_stride(w))
                        .collect();
                    assert_eq!(
                        d.decode_chunk_into(0, k, &mut sinks, &mut scratch).unwrap(),
                        r
                    );
                }
                for (p, c) in planes.iter().zip(&full.channels) {
                    assert_eq!(f32_bits(p), f32_bits(&c.samples.to_f32()), "chunks {what}");
                }
            }
        }
    }
}

#[test]
fn subsampled_into_strided_f32() {
    let (w, h) = (12u32, 8u32);
    let mut img = Image::new(w, h).with_channel(
        "Y",
        Samples::F16((0..96).map(|i| 0x3c00 + i as u16).collect()),
    );
    for (name, base) in [("BY", 0x3800u16), ("RY", 0x3400)] {
        let mut desc = ChannelDesc::new(name, SampleType::F16);
        desc.x_sampling = 2;
        desc.y_sampling = 2;
        img.channels.push(Channel {
            desc,
            width: 6,
            height: 4,
            samples: Samples::F16((0..24).map(|i| base + i as u16).collect()),
        });
    }
    img.channels.sort_by(|a, b| a.desc.name.cmp(&b.desc.name));
    for c in [
        Compression::None,
        Compression::Zip,
        Compression::Piz,
        Compression::Rle,
    ] {
        let file = rusty_exr::encode(&img, &EncodeOptions::new(c)).unwrap();
        let d = Decoder::new(&file).unwrap();
        let sizes = d.plane_sizes(0, 0, 0).unwrap();
        assert_eq!(sizes, vec![(6, 4), (6, 4), (12, 8)]);
        let mut planes: Vec<Vec<f32>> = sizes.iter().map(|&(w, h)| vec![-1.0; w * h + 7]).collect();
        let mut sinks: Vec<ChannelSink> = planes
            .iter_mut()
            .zip(&sizes)
            .map(|(p, &(w, _))| ChannelSink::f32(p).with_stride(w + 1))
            .collect();
        d.decode_part_into(0, &mut sinks).unwrap();
        for ((p, &(cw, ch)), want) in planes.iter().zip(&sizes).zip(&img.channels) {
            let want = want.samples.to_f32();
            for r in 0..ch {
                assert_eq!(
                    &p[r * (cw + 1)..r * (cw + 1) + cw],
                    &want[r * cw..(r + 1) * cw]
                );
            }
        }
    }
}

#[test]
fn mip_levels_into() {
    let file = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/mip_rgba16_64.exr"
    ))
    .unwrap();
    let d = Decoder::new(&file).unwrap();
    let (nx, _) = d.level_count(0).unwrap();
    assert!(nx > 1);
    for l in 0..nx {
        let full = d.decode_level(0, l, l).unwrap();
        let got = {
            let mut planes: Vec<Vec<f32>> = d
                .plane_sizes(0, l, l)
                .unwrap()
                .iter()
                .map(|&(w, h)| vec![0.0; w * h])
                .collect();
            let mut sinks: Vec<ChannelSink> =
                planes.iter_mut().map(|p| ChannelSink::f32(p)).collect();
            d.decode_level_into(0, l, l, &mut sinks).unwrap();
            drop(sinks);
            planes
        };
        for (p, c) in got.iter().zip(&full.channels) {
            assert_eq!(f32_bits(p), f32_bits(&c.samples.to_f32()), "level {l}");
        }
    }
}

#[test]
fn sinks_are_checked() {
    let img = image(8, 4, 3); // A F16, B F32, G U32, R F16, Z F32
    let file = rusty_exr::encode(&img, &EncodeOptions::new(Compression::Zip)).unwrap();
    let d = Decoder::new(&file).unwrap();
    let usage = |r: rusty_exr::Result<()>| matches!(r, Err(Error::Usage(_)));
    let mut a = vec![0f32; 32];
    // Wrong count.
    assert!(usage(
        d.decode_part_into(0, &mut [ChannelSink::f32(&mut a)])
    ));
    let skips = || -> Vec<ChannelSink<'static>> { (0..5).map(|_| ChannelSink::skip()).collect() };
    // UINT into an f16 plane, HALF into a u32 plane: refused.
    let mut h = vec![0u16; 32];
    let mut s = skips();
    s[2] = ChannelSink::f16(&mut h);
    assert!(usage(d.decode_part_into(0, &mut s)));
    let mut u = vec![0u32; 32];
    let mut s = skips();
    s[0] = ChannelSink::u32(&mut u);
    assert!(usage(d.decode_part_into(0, &mut s)));
    // Too short, and a stride below the width.
    let mut short = vec![0f32; 31];
    let mut s = skips();
    s[1] = ChannelSink::f32(&mut short);
    assert!(usage(d.decode_part_into(0, &mut s)));
    let mut s = skips();
    s[1] = ChannelSink::f32(&mut a).with_stride(7);
    assert!(usage(d.decode_part_into(0, &mut s)));
    // Exactly (h - 1) * stride + w with a stride is enough.
    let mut exact = vec![0f32; 3 * 10 + 8];
    let mut s = skips();
    s[1] = ChannelSink::f32(&mut exact).with_stride(10);
    d.decode_part_into(0, &mut s).unwrap();
    // Everything skipped is fine (the data is still validated).
    d.decode_part_into(0, &mut skips()).unwrap();
    // Chunk API: bad index.
    let mut sc = DecodeScratch::default();
    assert!(matches!(
        d.decode_chunk_into(0, 99, &mut skips(), &mut sc),
        Err(Error::Usage(_))
    ));
}

#[test]
fn truncated_file_errors_the_same() {
    let img = image(40, 33, 9);
    let file = rusty_exr::encode(&img, &EncodeOptions::new(Compression::Zip)).unwrap();
    let cut = &file[..file.len() - 300];
    let d = Decoder::new(cut).unwrap().threads(Threads::Count(2));
    let want = d.decode_part(0).unwrap_err().to_string();
    let mut planes: Vec<Vec<f32>> = (0..5).map(|_| vec![0.0; 40 * 33]).collect();
    let mut sinks: Vec<ChannelSink> = planes.iter_mut().map(|p| ChannelSink::f32(p)).collect();
    assert_eq!(
        d.decode_part_into(0, &mut sinks).unwrap_err().to_string(),
        want
    );
}

#[test]
fn encode_ref_matches_encode() {
    let img = image(37, 29, 7);
    let r = ImageRef {
        header: img.header.clone(),
        data_window: img.data_window,
        channels: img.channels.iter().map(ChannelRef::from).collect(),
    };
    for comp in ALL {
        for tiles in [None, Some((16u32, 8u32))] {
            for threads in [Threads::Single, Threads::Count(3)] {
                let mut o = EncodeOptions::new(comp);
                o.tiles = tiles;
                o.threads = threads;
                assert_eq!(
                    rusty_exr::encode(&img, &o).unwrap(),
                    rusty_exr::encode_ref(&r, &o).unwrap(),
                    "{comp:?} {tiles:?} {threads:?}"
                );
            }
        }
    }
    // The builder: same file as the owned builder.
    let (w, h) = (16u32, 9u32);
    let v: Vec<f32> = (0..w * h).map(|i| i as f32 * 0.25).collect();
    let owned = Image::new(w, h)
        .with_channel("R", Samples::F32(v.clone()))
        .with_channel("G", Samples::F32(v.clone()));
    let borrowed = ImageRef::new(w, h)
        .with_channel("R", SamplesRef::F32(&v))
        .with_channel("G", SamplesRef::F32(&v));
    let o = EncodeOptions::default();
    assert_eq!(
        rusty_exr::encode(&owned, &o).unwrap(),
        rusty_exr::encode_ref(&borrowed, &o).unwrap()
    );
    // Validation is shared: a short plane is refused.
    let bad = ImageRef::new(w, h).with_channel("R", SamplesRef::F32(&v[1..]));
    assert!(matches!(
        rusty_exr::encode_ref(&bad, &o),
        Err(Error::Usage(_))
    ));
}
