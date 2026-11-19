//! LZW against weezl (the independent LZW codec inside image-tiff), both
//! directions: our encoder -> weezl decoder, weezl encoder -> our decoder.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API

mod common;

use std::io::Cursor;

use common::Rng;
use rusty_tiff::encoder::{colortype, Compression, TiffEncoder};
use rusty_tiff::{Decoder, DecodingResult, Tag};

fn inputs() -> Vec<Vec<u8>> {
    let mut r = Rng(11);
    let mut v = Vec::new();
    for n in (0..600).chain([4093, 4094, 4095, 4096, 65_536, 1_000_000]) {
        let mode = r.next() % 3;
        v.push(
            (0..n)
                .map(|i| match mode {
                    0 => r.next() as u8,
                    1 => (i / 37) as u8,
                    _ => (r.next() % 4) as u8,
                })
                .collect(),
        );
    }
    v
}

#[test]
fn lzw_cross_check_with_weezl() {
    let mut n = 0;
    for data in inputs() {
        // Ours -> weezl, via a one-strip TIFF so only public API is used.
        let mut f = Cursor::new(Vec::new());
        if !data.is_empty() {
            let mut e = TiffEncoder::new(&mut f).unwrap();
            let mut img = e
                .new_image::<colortype::Gray8>(data.len() as u32, 1)
                .unwrap()
                .with_compression(Compression::Lzw);
            img.rows_per_strip(1);
            img.write_data(&data).unwrap();
            let file = f.into_inner();
            let mut d = Decoder::new(Cursor::new(&file)).unwrap();
            let o = d.get_tag_u64(Tag::StripOffsets).unwrap() as usize;
            let c = d.get_tag_u64(Tag::StripByteCounts).unwrap() as usize;
            let mut w = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
            assert_eq!(
                w.decode(&file[o..o + c]).unwrap(),
                data,
                "ours->weezl n={}",
                data.len()
            );

            // Weezl -> ours.
            let mut we = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
            let coded = we.encode(&data).unwrap();
            n += 1;
            let mut out = vec![0u8; data.len()];
            rusty_tiff_lzw_decode(&coded, &mut out);
            assert_eq!(out, data, "weezl->ours n={}", data.len());
        }
    }
    println!(
        "LZW: {n} inputs (0..600 bytes, table-full edges, 64 KiB, 1 MB) agree with weezl both ways"
    );
}

/// Our LZW decoder through the public API: wrap the stream in a minimal TIFF.
fn rusty_tiff_lzw_decode(coded: &[u8], out: &mut [u8]) {
    let w = out.len() as u32;
    let mut t = b"II\x2A\x00".to_vec();
    let ifd = 8 + coded.len() as u32 + (coded.len() as u32 & 1);
    t.extend_from_slice(&ifd.to_le_bytes());
    t.extend_from_slice(coded);
    if coded.len() % 2 == 1 {
        t.push(0);
    }
    let e: [(u16, u16, u32); 7] = [
        (256, 4, w),
        (257, 4, 1),
        (258, 3, 8),
        (259, 3, 5),
        (262, 3, 1),
        (273, 4, 8),
        (279, 4, coded.len() as u32),
    ];
    t.extend_from_slice(&(e.len() as u16).to_le_bytes());
    for (tag, ty, v) in e {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&ty.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&v.to_le_bytes());
    }
    t.extend_from_slice(&[0; 4]);
    let mut d = Decoder::new(Cursor::new(&t)).unwrap();
    match d.read_image().unwrap() {
        DecodingResult::U8(v) => out.copy_from_slice(&v),
        _ => panic!(),
    }
}
