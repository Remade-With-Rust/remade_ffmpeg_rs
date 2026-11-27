//! Encode -> decode round trips, header attributes, and the chunk API.

use rusty_exr::{
    AttributeValue, Box2i, Channel, ChannelDesc, Chromaticities, Compression, EncodeOptions, Image,
    LineOrder, Preview, SampleType, Samples, Threads,
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
        .with_channel(
            "G",
            Samples::U32((0..n).map(|_| r.next() as u32 % 1000).collect()),
        )
        .with_channel("R", Samples::F16((0..n).map(|_| r.next() as u16).collect()))
        .with_channel(
            "diffuse.R",
            Samples::F32((0..n).map(|i| smooth(i, 2) * 100.0).collect()),
        )
        .with_channel(
            "Z",
            Samples::F32((0..n).map(|_| f32::from_bits(r.next() as u32)).collect()),
        );
    img.channels.sort_by(|a, b| a.desc.name.cmp(&b.desc.name));
    img
}

fn same_samples(a: &Image, b: &Image, exact_f32: bool) {
    assert_eq!(a.channels.len(), b.channels.len());
    for (x, y) in a.channels.iter().zip(&b.channels) {
        assert_eq!(x.desc, y.desc);
        assert_eq!((x.width, x.height), (y.width, y.height));
        if exact_f32 || x.desc.sample_type != SampleType::F32 {
            match (&x.samples, &y.samples) {
                (Samples::F32(p), Samples::F32(q)) => {
                    assert!(
                        p.iter().zip(q).all(|(u, v)| u.to_bits() == v.to_bits()),
                        "{}",
                        x.desc.name
                    );
                }
                (p, q) => assert_eq!(p, q, "{}", x.desc.name),
            }
        }
    }
}

#[test]
fn lossless_round_trips() {
    for (w, h) in [(1, 1), (3, 2), (17, 33), (64, 64), (97, 5)] {
        let img = image(w, h, u64::from(w * 31 + h));
        for c in [
            Compression::None,
            Compression::Rle,
            Compression::Zips,
            Compression::Zip,
            Compression::Piz,
        ] {
            for order in [LineOrder::Increasing, LineOrder::Decreasing] {
                for threads in [Threads::Single, Threads::Count(3)] {
                    let opts = EncodeOptions {
                        compression: c,
                        line_order: order,
                        threads,
                        ..EncodeOptions::default()
                    };
                    let file = rusty_exr::encode(&img, &opts).unwrap();
                    let back = rusty_exr::Decoder::new(&file)
                        .unwrap()
                        .threads(threads)
                        .decode_part(0)
                        .unwrap();
                    assert_eq!(back.header.compression(), c);
                    assert_eq!(back.header.line_order(), order);
                    same_samples(&img, &back, true);
                }
            }
        }
    }
}

#[test]
fn fast_deflate_round_trips() {
    let img = image(70, 40, 13);
    for c in [Compression::Zips, Compression::Zip, Compression::Pxr24] {
        let opts = EncodeOptions {
            deflate: rusty_exr::Deflate::Fast,
            ..EncodeOptions::new(c)
        };
        let fast = rusty_exr::encode(&img, &opts).unwrap();
        let exr = rusty_exr::encode(&img, &EncodeOptions::new(c)).unwrap();
        let (a, b) = (
            rusty_exr::decode(&fast).unwrap(),
            rusty_exr::decode(&exr).unwrap(),
        );
        same_samples(&a, &b, true);
    }
}

#[test]
fn pxr24_rounds_float_only() {
    let img = image(40, 20, 7);
    let file = rusty_exr::encode(&img, &EncodeOptions::new(Compression::Pxr24)).unwrap();
    let back = rusty_exr::decode(&file).unwrap();
    same_samples(&img, &back, false);
    let (Samples::F32(a), Samples::F32(b)) = (
        &img.channel("B").unwrap().samples,
        &back.channel("B").unwrap().samples,
    ) else {
        panic!("float channel");
    };
    for (x, y) in a.iter().zip(b) {
        assert!((x - y).abs() <= x.abs() * 1e-4 + 1e-30, "{x} {y}");
    }
}

#[test]
fn output_is_deterministic_across_threads() {
    // Large enough (about 2.6 MB) that several workers actually run.
    let img = image(512, 256, 3);
    for c in [Compression::Zip, Compression::Piz, Compression::Pxr24] {
        let a = rusty_exr::encode(
            &img,
            &EncodeOptions {
                compression: c,
                line_order: LineOrder::Increasing,
                threads: Threads::Single,
                ..EncodeOptions::default()
            },
        )
        .unwrap();
        let b = rusty_exr::encode(
            &img,
            &EncodeOptions {
                compression: c,
                line_order: LineOrder::Increasing,
                threads: Threads::Count(5),
                ..EncodeOptions::default()
            },
        )
        .unwrap();
        assert_eq!(a, b, "{c:?}");
        let d1 = rusty_exr::Decoder::new(&a)
            .unwrap()
            .threads(Threads::Single)
            .decode_part(0)
            .unwrap();
        let d5 = rusty_exr::Decoder::new(&a)
            .unwrap()
            .threads(Threads::Count(5))
            .decode_part(0)
            .unwrap();
        same_samples(&d1, &d5, true);
    }
}

#[test]
fn attributes_and_windows_survive() {
    let mut img = image(10, 6, 9);
    img.data_window = Box2i {
        x_min: -4,
        y_min: 3,
        x_max: 5,
        y_max: 8,
    };
    img.header.set(
        "displayWindow",
        AttributeValue::Box2i(Box2i::from_size(20, 20)),
    );
    img.header.set(
        "chromaticities",
        AttributeValue::Chromaticities(Chromaticities::REC709),
    );
    img.header
        .set("owner", AttributeValue::Text(b"Freehold".to_vec()));
    img.header.set(
        "preview",
        AttributeValue::Preview(Preview {
            width: 2,
            height: 1,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
        }),
    );
    img.header.set(
        "cryptomatte/abc/manifest",
        AttributeValue::Text(br#"{"a":"3f800000"}"#.to_vec()),
    );
    img.header.set(
        "myapp.blob",
        AttributeValue::Custom {
            type_name: "myType".into(),
            data: vec![9, 8, 7],
        },
    );
    let file = rusty_exr::encode(&img, &EncodeOptions::new(Compression::Zip)).unwrap();
    let back = rusty_exr::decode(&file).unwrap();
    assert_eq!(back.data_window, img.data_window);
    assert_eq!(back.header.display_window(), Some(Box2i::from_size(20, 20)));
    assert_eq!(back.header.chromaticities(), Some(Chromaticities::REC709));
    assert_eq!(
        back.header.get("owner").and_then(AttributeValue::as_text),
        Some("Freehold")
    );
    assert_eq!(
        back.header.preview().unwrap().rgba,
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    );
    assert_eq!(
        back.header
            .get("cryptomatte/abc/manifest")
            .and_then(AttributeValue::as_text),
        Some(r#"{"a":"3f800000"}"#)
    );
    assert_eq!(
        back.header.get("myapp.blob"),
        Some(&AttributeValue::Custom {
            type_name: "myType".into(),
            data: vec![9, 8, 7]
        })
    );
    same_samples(&img, &back, true);
}

#[test]
fn subsampled_channels_round_trip() {
    let (w, h) = (12u32, 8u32);
    let mut img = Image::new(w, h).with_channel("Y", Samples::F16(vec![0x3c00; (w * h) as usize]));
    for (name, v) in [("BY", 0x3800u16), ("RY", 0x3400)] {
        let mut desc = ChannelDesc::new(name, SampleType::F16);
        desc.x_sampling = 2;
        desc.y_sampling = 2;
        img.channels.push(Channel {
            desc,
            width: 6,
            height: 4,
            samples: Samples::F16(vec![v; 24]),
        });
    }
    img.channels.sort_by(|a, b| a.desc.name.cmp(&b.desc.name));
    for c in [
        Compression::None,
        Compression::Zip,
        Compression::Piz,
        Compression::Rle,
        Compression::Pxr24,
    ] {
        let back =
            rusty_exr::decode(&rusty_exr::encode(&img, &EncodeOptions::new(c)).unwrap()).unwrap();
        same_samples(&img, &back, true);
    }
}

#[test]
fn chunk_api_matches_whole_decode() {
    let img = image(33, 40, 11);
    let file = rusty_exr::encode(&img, &EncodeOptions::new(Compression::Piz)).unwrap();
    let d = rusty_exr::Decoder::new(&file).unwrap();
    let full = d.decode_part(0).unwrap();
    let n = d.chunk_count(0).unwrap();
    assert_eq!(n, 2); // 40 lines, 32 per PIZ block
    for k in 0..n {
        let b = d.decode_chunk(0, k).unwrap();
        for (c, y, row) in b.rows() {
            let ch = &full.channels[c];
            let r = (y - full.data_window.y_min) as usize;
            let bytes: Vec<u8> = match &ch.samples {
                Samples::F16(v) => v[r * ch.width..(r + 1) * ch.width]
                    .iter()
                    .flat_map(|x| x.to_le_bytes())
                    .collect(),
                Samples::F32(v) => v[r * ch.width..(r + 1) * ch.width]
                    .iter()
                    .flat_map(|x| x.to_le_bytes())
                    .collect(),
                Samples::U32(v) => v[r * ch.width..(r + 1) * ch.width]
                    .iter()
                    .flat_map(|x| x.to_le_bytes())
                    .collect(),
            };
            assert_eq!(row, &bytes[..]);
        }
    }
}

#[test]
fn rgba_helper() {
    let img = Image::new(2, 1).with_channel("Y", Samples::F32(vec![0.5, 2.0]));
    assert_eq!(
        img.to_rgba_f32("").unwrap(),
        vec![0.5, 0.5, 0.5, 1.0, 2.0, 2.0, 2.0, 1.0]
    );
    let img = Image::new(1, 1)
        .with_channel("a.R", Samples::F32(vec![1.0]))
        .with_channel("a.G", Samples::F32(vec![2.0]))
        .with_channel("a.B", Samples::F32(vec![3.0]))
        .with_channel("a.A", Samples::F32(vec![0.25]));
    assert_eq!(img.to_rgba_f32("a").unwrap(), vec![1.0, 2.0, 3.0, 0.25]);
    assert!(img.to_rgba_f32("").is_none());
    assert_eq!(img.layers(), vec!["a".to_string()]);
}

#[test]
fn rejects_bad_requests() {
    let img = Image::new(2, 2).with_channel("R", Samples::F16(vec![0; 3]));
    assert!(matches!(
        rusty_exr::encode(&img, &EncodeOptions::default()),
        Err(rusty_exr::Error::Usage(_))
    ));
    let img = Image::new(2, 2).with_channel("R", Samples::F16(vec![0; 4]));
    let usage =
        |o: EncodeOptions| matches!(rusty_exr::encode(&img, &o), Err(rusty_exr::Error::Usage(_)));
    assert!(usage(EncodeOptions {
        line_order: LineOrder::Random,
        ..EncodeOptions::default()
    }));
    assert!(usage(EncodeOptions {
        tiles: Some((0, 64)),
        ..EncodeOptions::default()
    }));
    assert!(usage(EncodeOptions {
        dwa_level: Some(-1.0),
        ..EncodeOptions::new(Compression::Dwaa)
    }));
    assert!(usage(EncodeOptions {
        dwa_level: Some(f32::NAN),
        ..EncodeOptions::new(Compression::Dwab)
    }));
    let mut sub = Image::new(2, 2).with_channel("Y", Samples::F16(vec![0; 4]));
    sub = sub.with_channel("RY", Samples::F16(vec![0; 1]));
    sub.channels[1].desc.x_sampling = 2;
    sub.channels[1].desc.y_sampling = 2;
    sub.channels[1].width = 1;
    sub.channels[1].height = 1;
    assert!(rusty_exr::encode(&sub, &EncodeOptions::default()).is_ok());
    assert!(matches!(
        rusty_exr::encode(
            &sub,
            &EncodeOptions {
                tiles: Some((64, 64)),
                ..EncodeOptions::default()
            }
        ),
        Err(rusty_exr::Error::Usage(_))
    ));
    assert!(rusty_exr::decode(b"not an exr").is_err());
}

fn smooth_rgba(w: u32, h: u32) -> Image {
    let n = (w * h) as usize;
    let v = |i: usize, k: usize| {
        let (x, y) = ((i % w as usize) as f32, (i / w as usize) as f32);
        ((x * 0.07 + k as f32).sin() * (y * 0.05).cos() + 1.2) * 0.8
    };
    let half = |k: usize| Samples::F16((0..n).map(|i| rusty_exr::f32_to_f16(v(i, k))).collect());
    Image::new(w, h)
        .with_channel("A", half(3))
        .with_channel("B", half(2))
        .with_channel("G", half(1))
        .with_channel("R", half(0))
        .with_channel("id", Samples::U32((0..n as u32).collect()))
        .with_channel("Z", Samples::F32((0..n).map(|i| v(i, 4) * 10.0).collect()))
}

fn halves_close(a: &Image, b: &Image, name: &str, tol: f32) {
    let (Samples::F16(p), Samples::F16(q)) = (
        &a.channel(name).unwrap().samples,
        &b.channel(name).unwrap().samples,
    ) else {
        panic!("half channel {name}");
    };
    let worst = p
        .iter()
        .zip(q)
        .map(|(&x, &y)| (rusty_exr::f16_to_f32(x) - rusty_exr::f16_to_f32(y)).abs())
        .fold(0.0f32, f32::max);
    assert!(worst <= tol, "{name}: worst error {worst}");
}

#[test]
fn lossy_round_trips() {
    for (w, h) in [(1, 1), (5, 3), (67, 45), (300, 40)] {
        let img = smooth_rgba(w, h);
        for (c, tol) in [
            (Compression::B44, 0.02),
            (Compression::B44a, 0.02),
            (Compression::Dwaa, 0.1),
            (Compression::Dwab, 0.1),
        ] {
            for tiles in [None, Some((64, 64))] {
                let opts = EncodeOptions {
                    tiles,
                    ..EncodeOptions::new(c)
                };
                let file = rusty_exr::encode(&img, &opts).unwrap();
                let single = rusty_exr::encode(
                    &img,
                    &EncodeOptions {
                        threads: Threads::Single,
                        ..opts
                    },
                )
                .unwrap();
                assert_eq!(file, single, "{c:?} deterministic across threads");
                let back = rusty_exr::decode(&file).unwrap();
                assert_eq!(back.header.compression(), c);
                for n in ["R", "G", "B"] {
                    halves_close(&img, &back, n, tol);
                }
                // B44 stores non-half channels raw; DWA's alpha is lossless RLE.
                halves_close(
                    &img,
                    &back,
                    "A",
                    if c == Compression::B44 || c == Compression::B44a {
                        tol
                    } else {
                        0.0
                    },
                );
                assert_eq!(
                    img.channel("id").unwrap().samples,
                    back.channel("id").unwrap().samples
                );
            }
        }
    }
}

#[test]
fn dwa_level_attribute() {
    let img = smooth_rgba(40, 40);
    let level_of = |o: EncodeOptions| {
        let f = rusty_exr::encode(&img, &o).unwrap();
        let h = rusty_exr::read_headers(&f).unwrap().remove(0);
        match h.get("dwaCompressionLevel") {
            Some(rusty_exr::AttributeValue::Float(v)) => Some(*v),
            _ => None,
        }
    };
    assert_eq!(level_of(EncodeOptions::new(Compression::Dwaa)), None);
    let o = EncodeOptions {
        dwa_level: Some(120.0),
        ..EncodeOptions::new(Compression::Dwab)
    };
    assert_eq!(level_of(o), Some(120.0));
    // A coarser level makes a smaller file.
    let size = |l: f32| {
        rusty_exr::encode(
            &img,
            &EncodeOptions {
                dwa_level: Some(l),
                ..EncodeOptions::new(Compression::Dwaa)
            },
        )
        .unwrap()
        .len()
    };
    assert!(size(400.0) < size(5.0));
}

#[test]
fn tiled_round_trips() {
    for (w, h) in [(1, 1), (3, 2), (65, 70), (130, 64)] {
        let img = image(w, h, u64::from(w * 7 + h));
        for c in [
            Compression::None,
            Compression::Rle,
            Compression::Zips,
            Compression::Zip,
            Compression::Piz,
        ] {
            for tiles in [(64, 64), (37, 19), (1, 1)] {
                if tiles == (1, 1) && w * h > 100 {
                    continue;
                }
                for order in [
                    LineOrder::Random,
                    LineOrder::Increasing,
                    LineOrder::Decreasing,
                ] {
                    let opts = EncodeOptions {
                        compression: c,
                        line_order: order,
                        tiles: Some(tiles),
                        ..EncodeOptions::default()
                    };
                    let file = rusty_exr::encode(&img, &opts).unwrap();
                    let d = rusty_exr::Decoder::new(&file).unwrap();
                    assert_eq!(d.part_kind(0), Some(rusty_exr::PartKind::Tiled));
                    let t = d.header(0).unwrap().tiles().unwrap();
                    assert_eq!((t.x_size, t.y_size), tiles);
                    assert_eq!(d.header(0).unwrap().line_order(), order);
                    same_samples(&img, &d.decode_part(0).unwrap(), true);
                }
            }
        }
    }
}
