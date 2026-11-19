//! Tag access: ICC, XMP, EXIF IFD, SubIFDs and DNG tags, cross-checked
//! against image-tiff's tag reader.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API

mod common;

use std::io::Cursor;

use rusty_tiff::encoder::{colortype, Compression, TiffEncoder};
use rusty_tiff::tags::photometric;
use rusty_tiff::{Decoder, DecodingResult, Tag, Value};

/// A DNG-shaped file: an RGB thumbnail in IFD0 with DNG tags, ICC, XMP and
/// an EXIF IFD; the CFA raw image and a reduced-resolution image as SubIFDs.
fn dng_like() -> (Vec<u8>, Vec<u16>, Vec<u8>, Vec<u8>) {
    let raw: Vec<u16> = (0..64 * 48).map(|i| ((i * 37) % 4096) as u16).collect();
    let preview: Vec<u8> = (0..32 * 24 * 3).map(|i| (i % 256) as u8).collect();
    let thumb: Vec<u8> = (0..16 * 12 * 3).map(|i| (i * 3 % 256) as u8).collect();
    let mut f = Cursor::new(Vec::new());
    {
        let mut e = TiffEncoder::new(&mut f).unwrap();
        let mut r = e.new_subimage::<colortype::Gray16>(64, 48).unwrap();
        r.photometric(photometric::CFA);
        r.add_tag(Tag::NewSubfileType, Value::Long(vec![0]));
        r.add_tag(Tag::CfaRepeatPatternDim, Value::Short(vec![2, 2]));
        r.add_tag(Tag::CfaPattern, Value::Byte(vec![0, 1, 1, 2]));
        r.add_tag(Tag::BlackLevel, Value::Short(vec![64]));
        r.add_tag(Tag::WhiteLevel, Value::Long(vec![4095]));
        let raw_off = r.write_data(&raw).unwrap();
        let prev_off = e
            .new_subimage::<colortype::RGB8>(32, 24)
            .unwrap()
            .with_compression(Compression::Lzw)
            .write_data(&preview)
            .unwrap();
        let exif = e
            .write_directory(&[
                (Tag::Unknown(33434), Value::Rational(vec![[1, 250]])), // ExposureTime
                (Tag::Unknown(34855), Value::Short(vec![400])),         // ISO
                (
                    Tag::Unknown(36867),
                    Value::Ascii("2026:11:19 12:00:00".into()),
                ),
            ])
            .unwrap();
        let mut t = e.new_image::<colortype::RGB8>(16, 12).unwrap();
        t.add_tag(Tag::NewSubfileType, Value::Long(vec![1]));
        t.add_tag(
            Tag::SubIfds,
            Value::Ifd(vec![raw_off as u32, prev_off as u32]),
        );
        t.add_tag(Tag::ExifIfd, Value::Ifd(vec![exif as u32]));
        t.add_tag(Tag::DngVersion, Value::Byte(vec![1, 4, 0, 0]));
        t.add_tag(Tag::UniqueCameraModel, Value::Ascii("Rusty Cam".into()));
        t.add_tag(
            Tag::ColorMatrix1,
            Value::SRational(vec![[10000, 10000], [-2000, 10000], [-500, 10000]]),
        );
        t.add_tag(Tag::Make, Value::Ascii("Mata".into()));
        t.add_tag(Tag::Xmp, Value::Byte(b"<x:xmpmeta/>".to_vec()));
        t.icc_profile((0..600u32).map(|i| i as u8).collect());
        t.write_data(&thumb).unwrap();
    }
    (f.into_inner(), raw, preview, thumb)
}

#[test]
fn dng_structure_and_tags() {
    let (file, raw, preview, thumb) = dng_like();
    let mut d = Decoder::new(Cursor::new(&file)).unwrap();
    assert_eq!(d.read_image().unwrap(), DecodingResult::U8(thumb));
    assert_eq!(
        d.icc_profile().unwrap().unwrap(),
        (0..600u32).map(|i| i as u8).collect::<Vec<_>>()
    );
    assert_eq!(d.xmp().unwrap().unwrap(), b"<x:xmpmeta/>");
    assert_eq!(
        d.get_tag(Tag::DngVersion).unwrap(),
        Value::Byte(vec![1, 4, 0, 0])
    );
    assert_eq!(
        d.get_tag_ascii_string(Tag::UniqueCameraModel).unwrap(),
        "Rusty Cam"
    );
    assert_eq!(
        d.get_tag_f64_vec(Tag::ColorMatrix1).unwrap(),
        vec![1.0, -0.2, -0.05]
    );
    assert!(d.find_tag(Tag::Copyright).unwrap().is_none());
    assert!(d.get_tag(Tag::Copyright).is_err());

    let exif = d.exif_ifd().unwrap().unwrap();
    assert_eq!(
        d.find_tag_in(&exif, Tag::Unknown(33434)).unwrap(),
        Some(Value::Rational(vec![[1, 250]]))
    );
    assert_eq!(
        d.find_tag_in(&exif, Tag::Unknown(34855)).unwrap(),
        Some(Value::Short(vec![400]))
    );

    let subs = d.sub_ifds().unwrap();
    assert_eq!(subs.len(), 2);
    d.seek_to_ifd(subs[0]).unwrap();
    assert_eq!(d.info().unwrap().photometric, photometric::CFA);
    assert_eq!(d.get_tag_u32(Tag::WhiteLevel).unwrap(), 4095);
    assert_eq!(
        d.get_tag(Tag::CfaPattern).unwrap(),
        Value::Byte(vec![0, 1, 1, 2])
    );
    assert_eq!(d.read_image().unwrap(), DecodingResult::U16(raw));
    d.seek_to_ifd(subs[1]).unwrap();
    assert_eq!(d.read_image().unwrap(), DecodingResult::U8(preview));
    // Back to the main chain.
    d.seek_to_image(0).unwrap();
    assert_eq!(d.dimensions().unwrap(), (16, 12));
    assert!(!d.more_images());
}

/// Every tag value of IFD0, the EXIF IFD and both SubIFDs reads the same in
/// image-tiff (numeric values compared as f64 lists, strings and bytes exactly).
#[test]
fn tag_values_match_image_tiff() {
    use tiff::decoder::ifd::Value as TV;
    use tiff::tags::{IfdPointer, Tag as TT};
    fn flat(v: &TV, out: &mut Vec<f64>, s: &mut String) {
        match v {
            TV::Byte(x) => out.push(f64::from(*x)),
            TV::Short(x) => out.push(f64::from(*x)),
            TV::SignedByte(x) => out.push(f64::from(*x)),
            TV::SignedShort(x) => out.push(f64::from(*x)),
            TV::Signed(x) => out.push(f64::from(*x)),
            TV::SignedBig(x) => out.push(*x as f64),
            TV::Unsigned(x) => out.push(f64::from(*x)),
            TV::UnsignedBig(x) => out.push(*x as f64),
            TV::Float(x) => out.push(f64::from(*x)),
            TV::Double(x) => out.push(*x),
            TV::Rational(a, b) => out.push(f64::from(*a) / f64::from(*b)),
            TV::SRational(a, b) => out.push(f64::from(*a) / f64::from(*b)),
            TV::Ifd(x) => out.push(f64::from(*x)),
            TV::IfdBig(x) => out.push(*x as f64),
            TV::Ascii(x) => s.push_str(x),
            TV::List(l) => l.iter().for_each(|e| flat(e, out, s)),
            _ => out.push(f64::NAN),
        }
    }
    let (file, ..) = dng_like();
    let mut ours = Decoder::new(Cursor::new(&file)).unwrap();
    let mut theirs = tiff::decoder::Decoder::new(Cursor::new(&file)).unwrap();
    let mut ifd_offsets = vec![ours.current_ifd().offset()];
    ifd_offsets.extend(ours.sub_ifds().unwrap());
    ifd_offsets.push(ours.exif_ifd().unwrap().unwrap().offset());
    let mut compared = 0;
    for off in ifd_offsets {
        let ifd = ours.read_ifd(off).unwrap();
        let dir = theirs.read_directory(IfdPointer(off)).unwrap();
        for tag in ifd.tags().collect::<Vec<_>>() {
            let mine = ours.find_tag_in(&ifd, tag).unwrap().unwrap();
            let tv = theirs
                .read_directory_tags(&dir)
                .find_tag(TT::from_u16_exhaustive(tag.to_u16()))
                .unwrap()
                .unwrap();
            let (mut nums, mut s) = (Vec::new(), String::new());
            flat(&tv, &mut nums, &mut s);
            match &mine {
                Value::Ascii(m) => assert_eq!(m, s.trim_end_matches('\0'), "{tag:?}"),
                _ => assert_eq!(mine.to_f64_vec().unwrap(), nums, "{tag:?}"),
            }
            compared += 1;
        }
    }
    println!("tags: {compared} tag values identical to image-tiff across 4 IFDs");
}

#[test]
fn bigtiff_and_big_endian_tags() {
    let mut f = Cursor::new(Vec::new());
    {
        let mut e = TiffEncoder::new_big(&mut f).unwrap();
        let mut i = e.new_image::<colortype::CMYK16>(8, 4).unwrap();
        i.icc_profile(vec![9; 3000]);
        i.add_tag(Tag::Unknown(65000), Value::Long8(vec![1 << 40, 7]));
        i.add_tag(Tag::Unknown(65001), Value::Double(vec![1.5, -2.25]));
        i.write_data(&[500u16; 8 * 4 * 4]).unwrap();
    }
    let file = f.into_inner();
    let mut d = Decoder::new(Cursor::new(&file)).unwrap();
    assert!(d.is_bigtiff());
    assert_eq!(d.icc_profile().unwrap().unwrap(), vec![9; 3000]);
    assert_eq!(
        d.get_tag(Tag::Unknown(65000)).unwrap(),
        Value::Long8(vec![1 << 40, 7])
    );
    assert_eq!(
        d.get_tag(Tag::Unknown(65001)).unwrap(),
        Value::Double(vec![1.5, -2.25])
    );
    assert_eq!(d.colortype().unwrap(), rusty_tiff::ColorType::CMYK(16));
    // image-tiff agrees on the pixels.
    assert_eq!(common::ours(&file).unwrap(), common::theirs(&file).unwrap());

    // Big-endian: the converted twin reads the same tags.
    let mut f = Cursor::new(Vec::new());
    {
        let mut e = TiffEncoder::new(&mut f).unwrap();
        let mut i = e.new_image::<colortype::Gray16>(4, 4).unwrap();
        i.add_tag(Tag::Unknown(65002), Value::SLong(vec![-5, 70000]));
        i.add_tag(Tag::Software, Value::Ascii("rusty_tiff".into()));
        i.write_data(&[0x1234u16; 16]).unwrap();
    }
    let be = common::to_big_endian(&f.into_inner()).unwrap();
    let mut d = Decoder::new(Cursor::new(&be)).unwrap();
    assert_eq!(d.byte_order(), rusty_tiff::ByteOrder::BigEndian);
    assert_eq!(
        d.get_tag(Tag::Unknown(65002)).unwrap(),
        Value::SLong(vec![-5, 70000])
    );
    assert_eq!(d.get_tag_ascii_string(Tag::Software).unwrap(), "rusty_tiff");
    assert_eq!(
        d.read_image().unwrap(),
        DecodingResult::U16(vec![0x1234; 16])
    );
}
