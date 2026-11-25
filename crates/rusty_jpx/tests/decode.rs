//! Streams written by opj_compress 2.5.4 from synthetic images, against what
//! OpenJPEG 2.5.4 decodes them to (`.ref`: samples as little-endian 16-bit
//! words, component after component). See `tests/data/README.md`.

use rusty_jpx::{decode_jpx, decode_jpx_with, info, ChannelRole, ColorSpace, DecodeOptions, Jpx};

fn data(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn samples(j: &Jpx) -> Vec<i32> {
    let all = j.components.iter().flat_map(|c| c.data.iter().copied());
    all.collect()
}

fn max_error(name: &str, j: &Jpx) -> u32 {
    let stem = name.split('.').next().unwrap();
    let reference = data(&format!("{stem}.ref"));
    let got = samples(j);
    assert_eq!(got.len() * 2, reference.len(), "{name}");
    let pairs = got.iter().zip(reference.chunks_exact(2));
    // Compare modulo 2^16: the reference words do not carry their signedness.
    pairs
        .map(|(&g, r)| {
            let r = u16::from_le_bytes([r[0], r[1]]);
            let d = (g as u16).wrapping_sub(r) as i16;
            u32::from(d.unsigned_abs())
        })
        .max()
        .unwrap()
}

const REVERSIBLE: [&str; 20] = [
    "gray8.j2k",
    "rgb8_tiles_rpcl.jp2",
    "all_styles.j2k",
    "bypass.j2k",
    "termall_reset.j2k",
    "causal_segsym.j2k",
    "s12_pcrl.j2k",
    "u16_cprl.j2k",
    "s16.j2k",
    "bit2.j2k",
    "layers_rlcp.j2k",
    "poc.j2k",
    "sop_eph.j2k",
    "plt_tlm_tp.j2k",
    "roi.j2k",
    "offset.j2k",
    "sub420.jp2",
    "rgba8.j2k",
    "gray_alpha8.j2k",
    "index8.j2k",
];

#[test]
fn reversible_streams_match_openjpeg_exactly() {
    for name in REVERSIBLE {
        let j = decode_jpx(&data(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(max_error(name, &j), 0, "{name}");
        assert!(!j.truncated);
    }
}

#[test]
fn irreversible_streams_are_within_one() {
    for name in ["i97_rgb8.j2k", "i97_u12.j2k"] {
        let j = decode_jpx(&data(name)).unwrap();
        assert!(max_error(name, &j) <= 1, "{name}");
    }
}

#[test]
fn info_reads_headers_only() {
    let full = data("rgb8_tiles_rpcl.jp2");
    let i = info(&full).unwrap();
    assert_eq!((i.width, i.height), (41, 37));
    assert_eq!(i.components.len(), 3);
    assert_eq!(i.color, ColorSpace::Srgb);
    assert_eq!((i.tiles, i.max_reduce, i.reversible), (9, 2, true));
    // The tile data is not needed.
    let cut = find_all(&full, [0xFF, 0x90])[0];
    assert_eq!(info(&full[..cut + 2]).unwrap(), i);
}

#[test]
fn reduced_resolution() {
    let file = data("gray8.j2k");
    for (reduce, w, h) in [(1, 21, 19), (2, 11, 10), (3, 6, 5)] {
        let j = decode_jpx_with(&file, &DecodeOptions::new().with_reduce(reduce)).unwrap();
        assert_eq!((j.width, j.height), (w, h));
        assert_eq!(j.components[0].data.len(), (w * h) as usize);
    }
    assert!(decode_jpx_with(&file, &DecodeOptions::new().with_reduce(4)).is_err());
    assert!(decode_jpx_with(&file, &DecodeOptions::new().with_reduce(255)).is_err());
}

#[test]
fn subsampled_components_upsample_to_image_size() {
    let j = decode_jpx(&data("sub420.jp2")).unwrap();
    assert_eq!((j.components[1].width, j.components[1].height), (20, 18));
    let up = j.upsampled(1).unwrap();
    assert_eq!(up.len(), 40 * 36);
    for (i, v) in up.iter().enumerate() {
        let (x, y) = (i % 40, i / 40);
        assert_eq!(*v, j.components[1].data[y / 2 * 20 + x / 2]);
    }
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = (body.len() as u32 + 8).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn jp2(header: &[Vec<u8>], codestream: &[u8]) -> Vec<u8> {
    let mut f = boxed(b"jP  ", &[0x0D, 0x0A, 0x87, 0x0A]);
    f.extend(boxed(b"ftyp", b"jp2 \0\0\0\0jp2 "));
    f.extend(boxed(b"jp2h", &header.concat()));
    f.extend(boxed(b"jp2c", codestream));
    f
}

fn colr(v: u32) -> Vec<u8> {
    let mut b = vec![1, 0, 0];
    b.extend(v.to_be_bytes());
    boxed(b"colr", &b)
}

fn words(w: &[u16]) -> Vec<u8> {
    w.iter().flat_map(|v| v.to_be_bytes()).collect()
}

#[test]
fn channel_definitions_order_colours_and_find_alpha() {
    let cs = data("rgba8.j2k");
    let plain = decode_jpx(&cs).unwrap();
    assert!(!plain.has_alpha && plain.alpha().is_none());
    // Channels: 0 = blue, 1 = opacity, 2 = red, 3 = green.
    let cdef = words(&[4, 0, 0, 3, 1, 1, 0, 2, 0, 1, 3, 0, 2]);
    let j = decode_jpx(&jp2(&[colr(16), boxed(b"cdef", &cdef)], &cs)).unwrap();
    assert!(j.has_alpha);
    let order: Vec<_> = j.components.iter().map(|c| &c.data).collect();
    let want = [2, 3, 0, 1].map(|i| &plain.components[i].data);
    assert_eq!(order, want);
    assert_eq!(j.alpha().unwrap().data, plain.components[1].data);
    assert_eq!(j.colors().count(), 3);
    assert_eq!(j.components[3].role, ChannelRole::Opacity);

    let cdef = words(&[2, 0, 0, 1, 1, 2, 0]);
    let cs = data("gray_alpha8.j2k");
    let j = decode_jpx(&jp2(&[colr(17), boxed(b"cdef", &cdef)], &cs)).unwrap();
    assert_eq!(j.color, ColorSpace::Grey);
    assert_eq!(j.alpha().unwrap().role, ChannelRole::PremultipliedOpacity);
    let wrapped = jp2(&[boxed(b"cdef", &cdef)], &cs);
    assert!(info(&wrapped).unwrap().has_alpha);
}

#[test]
fn palette_is_expanded() {
    let cs = data("index8.j2k");
    let index = decode_jpx(&cs).unwrap().components.remove(0).data;
    // Seven entries of an 8-bit and a 16-bit column; the mapping swaps them
    // and adds the index itself.
    let mut pclr = vec![0, 7, 2, 7, 15];
    for i in 0..7u8 {
        pclr.extend([i * 10, i, 0x80 + i]);
    }
    let cmap = [0, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0];
    let header = [boxed(b"pclr", &pclr), boxed(b"cmap", &cmap), colr(16)];
    let file = jp2(&header, &cs);
    let j = decode_jpx(&file).unwrap();
    assert!(info(&file).unwrap().has_palette);
    assert_eq!(j.components.len(), 3);
    assert_eq!((j.components[0].bits, j.components[1].bits), (16, 8));
    for (i, &v) in index.iter().enumerate() {
        assert_eq!(j.components[0].data[i], v * 256 + 0x80 + v);
        assert_eq!(j.components[1].data[i], v * 10);
        assert_eq!(j.components[2].data[i], v);
    }
    let options = DecodeOptions::new().with_apply_palette(false);
    let raw = decode_jpx_with(&file, &options).unwrap();
    assert_eq!(raw.components.len(), 1);
    // A column that does not exist.
    let bad = [boxed(b"pclr", &pclr), boxed(b"cmap", &[0, 0, 1, 2])];
    assert!(decode_jpx(&jp2(&bad, &cs)).is_err());
}

#[test]
fn icc_profile_and_colour_spaces() {
    let cs = data("gray8.j2k");
    let mut body = vec![2, 0, 0];
    body.extend(b"not really a profile");
    let j = decode_jpx(&jp2(&[boxed(b"colr", &body)], &cs)).unwrap();
    assert_eq!(j.color, ColorSpace::Icc);
    assert_eq!(j.icc.as_deref(), Some(&b"not really a profile"[..]));
    for (v, want) in [
        (12, ColorSpace::Cmyk),
        (18, ColorSpace::Sycc),
        (99, ColorSpace::Other(99)),
    ] {
        assert_eq!(decode_jpx(&jp2(&[colr(v)], &cs)).unwrap().color, want);
    }
}

/// Positions of a two-byte marker.
fn find_all(d: &[u8], marker: [u8; 2]) -> Vec<usize> {
    let hits = d.windows(2).enumerate().filter(|(_, w)| *w == marker);
    hits.map(|(i, _)| i).collect()
}

/// Marker segments with an index byte, written last to first: the index
/// decides the order.
fn segments(marker: u16, payload: &[u8]) -> Vec<u8> {
    let chunks: Vec<_> = payload.chunks(50).enumerate().collect();
    let mut out = Vec::new();
    for (i, c) in chunks.into_iter().rev() {
        out.extend(marker.to_be_bytes());
        out.extend((c.len() as u16 + 3).to_be_bytes());
        out.push(i as u8);
        out.extend_from_slice(c);
    }
    out
}

/// Move the packet headers of `sop_eph.j2k` (found by their SOP and EPH
/// markers) into PPT or PPM marker segments.
fn pack_headers(main_header: bool) -> Vec<u8> {
    let d = data("sop_eph.j2k");
    let sot = find_all(&d, [0xFF, 0x90])[0];
    let sod = find_all(&d, [0xFF, 0x93])[0];
    let eoc = d.len() - 2;
    let (mut headers, mut body) = (Vec::new(), Vec::new());
    let starts = find_all(&d[..eoc], [0xFF, 0x91]);
    for (i, &s) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(eoc);
        let eph = s + find_all(&d[s..end], [0xFF, 0x92])[0] + 2;
        headers.extend_from_slice(&d[s + 6..eph]);
        // Keep SOP in the body for the odd packets only.
        if i % 2 == 1 {
            body.extend_from_slice(&d[s..s + 6]);
        }
        body.extend_from_slice(&d[eph..end]);
    }
    let mut out = d[..sot].to_vec();
    if main_header {
        let mut payload = (headers.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(&headers);
        out.extend(segments(0xFF60, &payload));
    }
    let mut part = d[sot..sod].to_vec();
    if !main_header {
        part.extend(segments(0xFF61, &headers));
    }
    part.extend([0xFF, 0x93]);
    part.extend(body);
    let len = part.len() as u32;
    part[6..10].copy_from_slice(&len.to_be_bytes());
    out.extend(part);
    out.extend([0xFF, 0xD9]);
    out
}

#[test]
fn packed_packet_headers() {
    for main_header in [false, true] {
        let j = decode_jpx(&pack_headers(main_header)).unwrap();
        assert_eq!(max_error("sop_eph.j2k", &j), 0);
    }
}

#[test]
fn part_15_is_refused() {
    let mut d = data("gray8.j2k");
    // Code-block style bit 6 in COD.
    let cod = find_all(&d, [0xFF, 0x52])[0];
    d[cod + 12] |= 0x40;
    assert!(matches!(
        decode_jpx(&d),
        Err(rusty_jpx::JpxError::Unsupported(_))
    ));
}
