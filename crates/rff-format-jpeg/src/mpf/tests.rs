use super::*;
use alloc::vec;

/// A structurally complete JPEG (no real pixels): SOI, APP0 JFIF, the given
/// extra segments, SOF0, a DHT-shaped filler, SOS, entropy data with byte
/// stuffing and a restart marker, EOI. `tag` makes each image distinct.
fn jpeg(tag: u8, extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    seg(&mut v, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    for (m, body) in extra {
        seg(&mut v, *m, body);
    }
    // SOF0: 8-bit, 16x8, one component.
    seg(&mut v, 0xC0, &[8, 0, 8, 0, 16, 1, 1, 0x11, 0]);
    seg(
        &mut v,
        0xC4,
        &[0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, tag],
    );
    seg(&mut v, 0xDA, &[1, 1, 0, 0, 63, 0]);
    v.extend_from_slice(&[
        tag, 0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0x00, tag,
    ]);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn seg(v: &mut Vec<u8>, marker: u8, body: &[u8]) {
    v.extend_from_slice(&[0xFF, marker]);
    v.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(body);
}

fn iso_app2() -> (u8, Vec<u8>) {
    let mut b = ISO21496_IDENTIFIER.to_vec();
    b.extend_from_slice(&[0, 0, 0, 0]); // versions
    b.extend_from_slice(&[0x12; 20]); // a payload (gain map image)
    (0xE2, b)
}

fn hdrgm_xmp() -> (u8, Vec<u8>) {
    let mut b = XMP_IDENTIFIER.to_vec();
    b.extend_from_slice(b"<x:xmpmeta xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\"/>");
    (0xE1, b)
}

fn bytes<'a>(file: &'a [u8], img: &LocatedImage) -> &'a [u8] {
    &file[img.range.clone()]
}

#[test]
fn write_then_locate_round_trips() {
    let a = jpeg(1, &[]);
    let b = jpeg(2, &[]);
    let c = jpeg(3, &[]);
    let file = write(&[
        WriteImage::new(&a),
        WriteImage::new(&b),
        WriteImage::new(&c),
    ])
    .unwrap();
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.source, LayoutSource::Index);
    assert_eq!(layout.images.len(), 3);
    assert_eq!(layout.first, 0);
    assert!(layout.warnings.is_empty(), "{:?}", layout.warnings);
    assert_eq!(bytes(&file, &layout.images[1]), &b[..]);
    assert_eq!(bytes(&file, &layout.images[2]), &c[..]);
    assert_eq!(layout.images[0].range, 0..file.len() - b.len() - c.len());
    assert_eq!(layout.images[0].mp_type, MpType::PRIMARY);
    assert_eq!(layout.images[1].mp_type, MpType::UNDEFINED);
    let idx = layout.index.unwrap();
    assert_eq!(idx.byte_order, ByteOrder::Big);
    assert_eq!(idx.version, Some(*b"0100"));
    // The primary is the input with one 56 + 16n byte APP2 segment
    // (plus marker) spliced in after APP0.
    assert_eq!(layout.images[0].range.len(), a.len() + 2 + 56 + 48);
    let splice = 2 + 18; // SOI + APP0
    assert_eq!(&file[..splice], &a[..splice]);
    assert_eq!(&file[splice..splice + 2], &[0xFF, 0xE2]);
    assert_eq!(
        &file[splice + 2 + 56 + 48..layout.images[0].range.end],
        &a[splice..]
    );
}

#[test]
fn a_single_image_is_written_unchanged() {
    let a = jpeg(1, &[]);
    assert_eq!(write(&[WriteImage::new(&a)]).unwrap(), a);
    let layout = locate(&a, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.source, LayoutSource::Single);
    assert_eq!(layout.images[0].range, 0..a.len());
}

#[test]
fn gain_map_gets_type_dependency_and_ultra_hdr_segments() {
    let primary = jpeg(1, &[]);
    let gain = jpeg(2, &[hdrgm_xmp(), iso_app2()]);
    let file = write(&[WriteImage::new(&primary), WriteImage::new(&gain)]).unwrap();
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.images[1].mp_type, MpType::GAIN_MAP);
    assert!(!layout.images[1].type_inferred);
    assert!(layout.has_gain_map_metadata, "ISO 21496-1 version segment");
    let e = &layout.index.as_ref().unwrap().entries;
    assert_eq!(e[0].attribute, ATTR_DEPENDENT_PARENT | MpType::PRIMARY.0);
    assert_eq!(e[0].dependents, [2, 0]);
    assert_eq!(e[1].attribute, ATTR_DEPENDENT_CHILD | MpType::GAIN_MAP.0);
    assert_eq!(bytes(&file, &layout.images[1]), &gain[..]);
    // The GContainer directory names the gain map's length.
    let p = bytes(&file, &layout.images[0]);
    let want = alloc::format!("Item:Length=\"{}\"", gain.len());
    assert!(contains(p, want.as_bytes()));

    // A primary that has XMP of its own gets no GContainer directory.
    let mut own_xmp = XMP_IDENTIFIER.to_vec();
    own_xmp.extend_from_slice(b"<x:xmpmeta/>");
    let primary = jpeg(1, &[(0xE1, own_xmp)]);
    let file = write(&[WriteImage::new(&primary), WriteImage::new(&gain)]).unwrap();
    assert!(!contains(&file, b"GainMap"));
}

#[test]
fn remux_replaces_the_stale_index_and_is_idempotent() {
    let a = jpeg(1, &[]);
    let b = jpeg(2, &[iso_app2()]);
    let c = jpeg(3, &[]);
    let once = write(&[
        WriteImage::new(&a),
        WriteImage::new(&b),
        WriteImage::new(&c),
    ])
    .unwrap();
    let layout = locate(&once, TrailingPolicy::Split).unwrap();
    let parts: Vec<WriteImage<'_>> = layout
        .images
        .iter()
        .map(|i| WriteImage {
            data: &once[i.range.clone()],
            mp_type: Some(i.mp_type),
            representative: false,
        })
        .collect();
    let twice = write(&parts).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn explicit_types_and_flags_are_written() {
    let a = jpeg(1, &[]);
    let b = jpeg(2, &[]);
    let file = write(&[
        WriteImage {
            data: &a,
            mp_type: Some(MpType::DISPARITY),
            representative: true,
        },
        WriteImage {
            data: &b,
            mp_type: Some(MpType::DISPARITY),
            representative: false,
        },
    ])
    .unwrap();
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    let e = &layout.index.unwrap().entries;
    assert_eq!(e[0].attribute, ATTR_REPRESENTATIVE | 0x02_0002);
    assert_eq!(e[1].attribute, 0x02_0002);
    assert_eq!(e[0].dependents, [0, 0]);
}

#[test]
fn too_many_dependents_or_gain_maps_are_refused() {
    let a = jpeg(1, &[]);
    let t = |ty| WriteImage {
        data: &a,
        mp_type: Some(ty),
        representative: false,
    };
    let three = [
        WriteImage::new(&a),
        t(MpType::LARGE_THUMBNAIL_VGA),
        t(MpType::LARGE_THUMBNAIL_FULL_HD),
        t(MpType::ORIGINAL),
    ];
    assert_eq!(write(&three), Err(MpfError::TooManyDependents));
    let two_gain = [
        WriteImage::new(&a),
        t(MpType::GAIN_MAP),
        t(MpType::GAIN_MAP),
    ];
    assert_eq!(write(&two_gain), Err(MpfError::TooManyDependents));
    assert_eq!(write(&[]), Err(MpfError::NoImages));
    assert_eq!(
        write(&[WriteImage::new(&a), WriteImage::new(b"\xFF\xD8garbage")]),
        Err(MpfError::BadImage(1))
    );
}

#[test]
fn image_count_is_capped_by_one_segment() {
    let a = jpeg(1, &[]);
    let many = vec![WriteImage::new(&a); MAX_IMAGES + 1];
    assert_eq!(write(&many), Err(MpfError::TooManyImages));
    let file = write(&many[..MAX_IMAGES]).unwrap();
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.images.len(), MAX_IMAGES);
}

/// Build an MPF file by hand with a little-endian index.
fn little_endian_file(entries: &[(u32, u32)], tail: &[&[u8]], number: u32) -> Vec<u8> {
    let mut tiff = b"II*\0".to_vec();
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&2u16.to_le_bytes());
    tiff.extend_from_slice(&TAG_NUMBER_OF_IMAGES.to_le_bytes());
    tiff.extend_from_slice(&TIFF_LONG.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&number.to_le_bytes());
    tiff.extend_from_slice(&TAG_MP_ENTRY.to_le_bytes());
    tiff.extend_from_slice(&TIFF_UNDEFINED.to_le_bytes());
    tiff.extend_from_slice(&((entries.len() * 16) as u32).to_le_bytes());
    tiff.extend_from_slice(&(8 + 2 + 24 + 4u32).to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    for &(attr, size) in entries {
        tiff.extend_from_slice(&attr.to_le_bytes());
        tiff.extend_from_slice(&size.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes()); // patched below
        tiff.extend_from_slice(&[0; 4]);
    }
    let mut app2 = MPF_IDENTIFIER.to_vec();
    app2.extend_from_slice(&tiff);
    let mut file = jpeg(1, &[(0xE2, app2)]);
    // Patch sizes/offsets now that the primary's length is known.
    let tiff_base = 2 + 18 + 4 + 4;
    let table = tiff_base + 8 + 2 + 24 + 4;
    let mut at = file.len();
    file[table + 4..table + 8].copy_from_slice(&(at as u32).to_le_bytes());
    for (k, t) in tail.iter().enumerate() {
        let e = table + (k + 1) * 16;
        file[e + 4..e + 8].copy_from_slice(&(t.len() as u32).to_le_bytes());
        file[e + 8..e + 12].copy_from_slice(&((at - tiff_base) as u32).to_le_bytes());
        at += t.len();
    }
    for t in tail {
        file.extend_from_slice(t);
    }
    file
}

#[test]
fn little_endian_index_is_read() {
    let b = jpeg(2, &[]);
    let c = jpeg(3, &[]);
    let file = little_endian_file(
        &[
            (0x02_0002 | ATTR_REPRESENTATIVE, 0),
            (0x02_0002, 0),
            (0x02_0002, 0),
        ],
        &[&b, &c],
        3,
    );
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.source, LayoutSource::Index);
    assert_eq!(layout.index.as_ref().unwrap().byte_order, ByteOrder::Little);
    assert_eq!(layout.images.len(), 3);
    assert_eq!(layout.images[0].range, 0..file.len() - b.len() - c.len());
    assert_eq!(bytes(&file, &layout.images[1]), &b[..]);
    assert_eq!(bytes(&file, &layout.images[2]), &c[..]);
    assert!(layout.images.iter().all(|i| i.mp_type == MpType::DISPARITY));
}

#[test]
fn excess_entries_are_tolerated_and_short_tables_are_not() {
    let b = jpeg(2, &[]);
    // Three records, NumberOfImages 2: the third is ignored (FFmpeg too).
    let file = little_endian_file(&[(0x03_0000, 0), (0, 0), (0, 0)], &[&b, &b], 2);
    let layout = locate(&file, TrailingPolicy::Ignore).unwrap();
    assert_eq!(layout.images.len(), 2);
    // NumberOfImages larger than the table: the index is unusable; the
    // trailing JPEGs are still found by the lenient policy.
    let file = little_endian_file(&[(0x03_0000, 0), (0, 0)], &[&b], 1000);
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.source, LayoutSource::Trailing);
    assert_eq!(layout.images.len(), 2);
    assert!(!layout.warnings.is_empty());
    let layout = locate(&file, TrailingPolicy::Ignore).unwrap();
    assert_eq!(layout.source, LayoutSource::Single);
}

#[test]
fn parse_index_rejects_hostile_ifds_without_allocating() {
    assert_eq!(parse_index(b""), Err(MpfError::BadTiffHeader));
    assert_eq!(
        parse_index(b"XX*\0\0\0\0\x08"),
        Err(MpfError::BadTiffHeader)
    );
    // IFD offset past the end.
    assert_eq!(parse_index(b"MM\0*\xFF\xFF\xFF\xFF"), Err(MpfError::BadIfd));
    // 65535 tags claimed in a 10-byte segment.
    assert_eq!(
        parse_index(b"MM\0*\0\0\0\x08\xFF\xFF"),
        Err(MpfError::BadIfd)
    );
    // No tags at all.
    assert_eq!(
        parse_index(b"MM\0*\0\0\0\x08\0\0"),
        Err(MpfError::MissingTag)
    );
    // NumberOfImages = u32::MAX with a 16-byte table.
    let mut t = b"MM\0*\0\0\0\x08\0\x02".to_vec();
    t.extend_from_slice(&[0xB0, 0x01, 0, 4, 0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFF]);
    t.extend_from_slice(&[0xB0, 0x02, 0, 7, 0, 0, 0, 16, 0, 0, 0, 34]);
    t.extend_from_slice(&[0; 16]);
    assert_eq!(parse_index(&t), Err(MpfError::BadImageCount));
    // MPEntry data running off the end.
    t[30 - 4..30].copy_from_slice(&[0, 0, 0, 0xFF]);
    assert_eq!(parse_index(&t), Err(MpfError::BadIfd));
}

#[test]
fn trailing_policy_splits_back_to_back_jpegs() {
    let a = jpeg(1, &[]);
    let b = jpeg(2, &[]);
    let c = jpeg(3, &[]);
    let mut file = a.clone();
    file.extend_from_slice(&b);
    file.extend_from_slice(&[0, 0, 0]); // padding is skipped
    file.extend_from_slice(&c);
    file.extend_from_slice(b"trailer that is not a JPEG");
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.source, LayoutSource::Trailing);
    assert_eq!(layout.images.len(), 3);
    assert_eq!(bytes(&file, &layout.images[0]), &a[..]);
    assert_eq!(bytes(&file, &layout.images[1]), &b[..]);
    assert_eq!(bytes(&file, &layout.images[2]), &c[..]);
    let layout = locate(&file, TrailingPolicy::Ignore).unwrap();
    assert_eq!(layout.source, LayoutSource::Single);
    assert_eq!(layout.images[0].range, 0..file.len());

    // A non-JPEG trailer alone (a motion photo's MP4) stays with the image.
    let mut file = a.clone();
    file.extend_from_slice(b"\0\0\0\x18ftypmp42");
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.source, LayoutSource::Single);
    assert_eq!(layout.images[0].range, 0..file.len());

    // A truncated trailing JPEG is kept as far as it goes.
    let mut file = a.clone();
    file.extend_from_slice(&b[..b.len() - 5]);
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.images.len(), 2);
    assert!(layout.images[1].truncated);
}

#[test]
fn jpeg_end_skips_embedded_thumbnails_and_follows_every_scan() {
    let thumb = jpeg(9, &[]);
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(&thumb);
    let mut img = jpeg(1, &[(0xE1, exif)]);
    // A second scan (progressive), then EOI.
    let eoi = img.len() - 2;
    img.truncate(eoi);
    seg(&mut img, 0xDA, &[1, 1, 0, 1, 63, 0]);
    img.extend_from_slice(&[0x77, 0xFF, 0xFF, 0x00, 0x88, 0xFF, 0xFF, 0xD9]);
    assert_eq!(jpeg_end(&img, 0), Some(img.len()));
    let mut two = img.clone();
    two.extend_from_slice(&thumb);
    assert_eq!(jpeg_end(&two, 0), Some(img.len()));
    assert_eq!(jpeg_end(&two, img.len()), Some(two.len()));
    for cut in 0..img.len() {
        assert_eq!(jpeg_end(&img[..cut], 0), None, "cut {cut}");
    }
}

#[test]
fn bad_entries_are_dropped_and_overlaps_truncated() {
    let b = jpeg(2, &[]);
    let c = jpeg(3, &[]);
    let file = write(&[
        WriteImage::new(&jpeg(1, &[])),
        WriteImage::new(&b),
        WriteImage::new(&c),
    ])
    .unwrap();
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    let table = {
        // The MPEntry table: tiff_base + 50.
        let tiff = file.windows(4).position(|w| w == b"MM\0*").unwrap();
        tiff + 50
    };
    let c_off = layout.images[2].range.start;

    // Image 1 claims to be 10 bytes longer: cut where image 2 starts.
    let mut f = file.clone();
    let size = &mut f[table + 16 + 4..table + 16 + 8];
    let grown = u32::from_be_bytes(size.try_into().unwrap()) + 10;
    size.copy_from_slice(&grown.to_be_bytes());
    let l = locate(&f, TrailingPolicy::Split).unwrap();
    assert_eq!(l.images[1].range.end, c_off);
    assert_eq!(l.warnings.len(), 1);

    // Image 2's offset past the end of the file: dropped.
    let mut f = file.clone();
    f[table + 32 + 8..table + 32 + 12].copy_from_slice(&u32::MAX.to_be_bytes());
    let l = locate(&f, TrailingPolicy::Split).unwrap();
    assert_eq!(l.images.len(), 2);

    // Image 2's offset one byte off (not at an SOI): dropped.
    let mut f = file.clone();
    let off = &mut f[table + 32 + 8..table + 32 + 12];
    let o = u32::from_be_bytes(off.try_into().unwrap()) + 1;
    off.copy_from_slice(&o.to_be_bytes());
    let l = locate(&f, TrailingPolicy::Split).unwrap();
    assert_eq!(l.images.len(), 2);

    // Image 2 runs past the end of the file: kept, truncated.
    let mut f = file.clone();
    f.truncate(f.len() - 4);
    let l = locate(&f, TrailingPolicy::Split).unwrap();
    assert_eq!(l.images.len(), 3);
    assert!(l.images[2].truncated);
    assert_eq!(l.images[2].range.end, f.len());

    // Two first images: the index is unusable.
    let mut f = file.clone();
    f[table + 16 + 8..table + 16 + 12].copy_from_slice(&[0; 4]);
    let l = locate(&f, TrailingPolicy::Split).unwrap();
    assert_eq!(l.source, LayoutSource::Trailing);
    assert_eq!(l.images.len(), 3, "the EOI split still finds them");
}

#[test]
fn untyped_second_image_with_gain_map_metadata_is_the_gain_map() {
    // libultrahdr: ISO 21496-1 version segment in the primary, MP Type 0.
    let gain = jpeg(2, &[iso_app2()]);
    let primary = jpeg(1, &[]);
    let file = write(&[
        WriteImage::new(&primary),
        WriteImage {
            data: &gain,
            mp_type: Some(MpType::UNDEFINED),
            representative: false,
        },
    ])
    .unwrap();
    // Without a gain-map dependency the writer adds no ISO segment; add one.
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.images[1].mp_type, MpType::UNDEFINED);
    let primary_iso = jpeg(
        1,
        &[(0xE2, {
            let mut b = ISO21496_IDENTIFIER.to_vec();
            b.extend_from_slice(&[0; 4]);
            b
        })],
    );
    let file = write(&[
        WriteImage::new(&primary_iso),
        WriteImage {
            data: &gain,
            mp_type: Some(MpType::UNDEFINED),
            representative: false,
        },
    ])
    .unwrap();
    let layout = locate(&file, TrailingPolicy::Split).unwrap();
    assert_eq!(layout.images[1].mp_type, MpType::GAIN_MAP);
    assert!(layout.images[1].type_inferred);
}

#[test]
fn mp_type_names_round_trip() {
    for (t, name, _) in MpType::NAMED {
        assert_eq!(t.name(), Some(name));
        assert_eq!(MpType::from_name(name), Some(t));
        assert_eq!(MpType::from_name(&alloc::format!("0x{:06x}", t.0)), Some(t));
    }
    assert_eq!(MpType(0x123456).name(), None);
    assert_eq!(alloc::format!("{}", MpType(0x123456)), "0x123456");
    assert_eq!(MpType::from_name("0x1000000"), None);
    assert!(MpType::GAIN_MAP.is_dependent_kind());
    assert!(MpType::LARGE_THUMBNAIL_4K.is_dependent_kind());
    assert!(!MpType::DISPARITY.is_dependent_kind());
}

/// Every image range lies in the file and the images never overlap.
fn check_sane(file: &[u8], layout: &Layout) {
    assert!(!layout.images.is_empty());
    assert!(layout.first < layout.images.len());
    let mut ranges: Vec<_> = layout.images.iter().map(|i| i.range.clone()).collect();
    ranges.sort_by_key(|r| r.start);
    for r in &ranges {
        assert!(
            r.start < r.end && r.end <= file.len(),
            "{r:?} in {}",
            file.len()
        );
    }
    for w in ranges.windows(2) {
        assert!(w[0].end <= w[1].start, "{:?} overlaps {:?}", w[0], w[1]);
    }
    // Whatever was found survives a re-mux: every image after the first
    // comes back byte-identical (the fuzz target checks the same).
    let images: Vec<WriteImage<'_>> = layout
        .images
        .iter()
        .map(|i| WriteImage {
            data: &file[i.range.clone()],
            mp_type: Some(i.mp_type),
            representative: false,
        })
        .collect();
    if let Ok(out) = write(&images) {
        let again = locate(&out, TrailingPolicy::Ignore).unwrap();
        assert_eq!(again.images.len(), images.len());
        for (img, src) in again.images.iter().zip(&images).skip(1) {
            assert_eq!(&out[img.range.clone()], src.data);
        }
    }
}

#[test]
fn truncation_sweep_never_panics() {
    let file = write(&[
        WriteImage::new(&jpeg(1, &[hdrgm_xmp()])),
        WriteImage::new(&jpeg(2, &[iso_app2()])),
        WriteImage::new(&jpeg(3, &[])),
    ])
    .unwrap();
    for cut in 0..=file.len() {
        let f = &file[..cut];
        for policy in [TrailingPolicy::Split, TrailingPolicy::Ignore] {
            match locate(f, policy) {
                Ok(l) => check_sane(f, &l),
                Err(e) => assert!(cut < 2 && e == MpfError::NotJpeg, "cut {cut}: {e}"),
            }
        }
    }
}

#[test]
fn corruption_sweep_never_panics() {
    let file = write(&[
        WriteImage::new(&jpeg(1, &[])),
        WriteImage::new(&jpeg(2, &[iso_app2()])),
        WriteImage::new(&jpeg(3, &[])),
    ])
    .unwrap();
    let index_end = file.windows(4).position(|w| w == b"MM\0*").unwrap() + 50 + 48;
    for at in 0..index_end {
        for v in [0x00, 0x01, 0x7F, 0x80, 0xFE, 0xFF] {
            let mut f = file.clone();
            f[at] = v;
            if let Ok(l) = locate(&f, TrailingPolicy::Split) {
                check_sane(&f, &l);
            }
        }
    }
}

#[test]
fn a_first_image_size_shorter_than_its_header_is_recomputed() {
    let b = jpeg(2, &[]);
    let file = write(&[WriteImage::new(&jpeg(1, &[])), WriteImage::new(&b)]).unwrap();
    let want = locate(&file, TrailingPolicy::Ignore).unwrap().images[0]
        .range
        .clone();
    let table = file.windows(4).position(|w| w == b"MM\0*").unwrap() + 50;
    let mut f = file.clone();
    f[table + 4..table + 8].copy_from_slice(&10u32.to_be_bytes());
    let l = locate(&f, TrailingPolicy::Ignore).unwrap();
    assert_eq!(l.source, LayoutSource::Index);
    assert_eq!(l.images[0].range, want);
    assert_eq!(bytes(&f, &l.images[1]), &b[..]);
}

#[test]
fn stray_bytes_in_the_header_still_read_as_one_image() {
    let mut img = jpeg(1, &[]);
    img.insert(2 + 18, 0x00); // a stray byte after APP0
    let l = locate(&img, TrailingPolicy::Split).unwrap();
    assert_eq!(l.source, LayoutSource::Single);
    assert_eq!(l.images[0].range, 0..img.len());
    assert_eq!(l.warnings.len(), 1);
    assert_eq!(
        locate(b"\xFF\xD9", TrailingPolicy::Split),
        Err(MpfError::NotJpeg)
    );
}
