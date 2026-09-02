//! The chip-facing surface, exercised on the host: a decoder that reads a
//! slice (and scales), an encoder that writes into a caller buffer, the
//! packed-YUYV input, and a golden hash that pins the coded bytes across every
//! build arm (`std` + SIMD, `platform_independent`, and the `no_std` code
//! paths under `--no-default-features`). One value in three builds is the
//! host-equals-chip gate: the same source must code to the same bytes.

use rusty_jpeg::decode::{Decoder, Error};
use rusty_jpeg::encode::{
    ColorType, Encoder, PlanarYcbcrImage, SamplingFactor, SliceWriter, YuyvImage,
};

/// Deterministic test picture: smooth ramps plus a little texture, so every
/// stage has non-trivial coefficients.
fn rgb(w: u16, h: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
    for y in 0..u32::from(h) {
        for x in 0..u32::from(w) {
            v.push((x * 255 / u32::from(w)) as u8);
            v.push((y * 255 / u32::from(h)) as u8);
            v.push(((x * 7 + y * 13) % 251) as u8);
        }
    }
    v
}

fn encode_rgb(w: u16, h: u16, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    Encoder::new(&mut out, quality)
        .encode(&rgb(w, h), w, h, ColorType::Rgb)
        .expect("encode");
    out
}

/// FNV-1a, so the golden is one `u64` and needs no dependency.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[test]
fn decoder_reads_a_slice() {
    let jpeg = encode_rgb(96, 64, 80);
    let mut d = Decoder::new(&jpeg[..]);
    let px = d.decode().expect("decode from slice");
    let info = d.info().unwrap();
    assert_eq!((info.width, info.height), (96, 64));
    assert_eq!(px.len(), 96 * 64 * 3);
}

/// With `std`, a slice and a `Cursor` are the same `Source`; the pixels must
/// not depend on which one fed the decoder.
#[cfg(feature = "std")]
#[test]
fn slice_and_cursor_sources_agree() {
    let jpeg = encode_rgb(96, 64, 80);
    let a = Decoder::new(&jpeg[..]).decode().unwrap();
    let b = Decoder::new(std::io::Cursor::new(&jpeg)).decode().unwrap();
    assert_eq!(a, b);
}

/// DCT-domain scaling is what makes a sensor-sized JPEG affordable on a chip:
/// ask for a quarter and the IDCT runs at 2x2 per block.
#[test]
fn decoder_scales_from_a_slice() {
    let jpeg = encode_rgb(256, 192, 80);
    let mut d = Decoder::new(&jpeg[..]);
    d.read_info().expect("read_info");
    let (w, h) = d.scale(64, 48).expect("scale");
    assert_eq!((w, h), (64, 48), "1/4 scale");
    let px = d.decode().expect("decode scaled");
    assert_eq!(px.len(), 64 * 48 * 3);

    // And 1/8, the smallest the IDCT supports.
    let mut d = Decoder::new(&jpeg[..]);
    d.read_info().unwrap();
    assert_eq!(d.scale(1, 1).unwrap(), (32, 24));
    assert_eq!(d.decode().unwrap().len(), 32 * 24 * 3);
}

/// A truncated frame is a distinct error from a corrupt one — a receiver drops
/// the frame and resynchronises rather than declaring the stream broken.
#[test]
fn truncated_data_is_unexpected_eof() {
    let jpeg = encode_rgb(64, 64, 80);
    let cut = &jpeg[..jpeg.len() / 2];
    match Decoder::new(cut).decode() {
        Err(Error::UnexpectedEof) => {}
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

/// The caller-owned sink: same bytes as the `Vec` sink, and the length comes
/// back through the writer.
#[test]
fn slice_writer_matches_the_vec_sink() {
    let (w, h) = (64u16, 48u16);
    let pixels = rgb(w, h);
    let mut expect = Vec::new();
    Encoder::new(&mut expect, 75)
        .encode(&pixels, w, h, ColorType::Rgb)
        .unwrap();

    let mut buf = vec![0u8; usize::from(w) * usize::from(h) * 3 + 4096];
    let cap = buf.len();
    let mut sink = SliceWriter::new(&mut buf);
    assert_eq!(sink.capacity(), cap);
    Encoder::new(&mut sink, 75)
        .encode(&pixels, w, h, ColorType::Rgb)
        .unwrap();
    let n = sink.written();
    assert_eq!(sink.as_slice(), &expect[..]);
    assert_eq!(&buf[..n], &expect[..]);

    // And by value, giving the narrowed buffer back.
    let mut buf2 = vec![0u8; expect.len()];
    let mut sink = SliceWriter::new(&mut buf2);
    Encoder::new(&mut sink, 75)
        .encode(&pixels, w, h, ColorType::Rgb)
        .unwrap();
    let written = sink.into_written();
    assert_eq!(written, &expect[..], "an exactly-sized buffer is enough");
}

#[test]
fn slice_writer_overflow_is_buffer_too_small() {
    let (w, h) = (64u16, 48u16);
    let mut buf = [0u8; 200];
    let mut sink = SliceWriter::new(&mut buf);
    let err = Encoder::new(&mut sink, 75)
        .encode(&rgb(w, h), w, h, ColorType::Rgb)
        .unwrap_err();
    assert!(
        matches!(err, rusty_jpeg::encode::EncodingError::BufferTooSmall),
        "got {err:?}"
    );
}

/// Without `std` a bare `&mut [u8]` is a sink too, advancing past what it
/// wrote and refusing to overflow.
#[cfg(not(feature = "std"))]
#[test]
fn mut_slice_is_a_sink_without_std() {
    let (w, h) = (32u16, 32u16);
    let pixels = rgb(w, h);
    let mut expect = Vec::new();
    Encoder::new(&mut expect, 75)
        .encode(&pixels, w, h, ColorType::Rgb)
        .unwrap();

    let mut buf = vec![0u8; 8192];
    let total = buf.len();
    let mut tail: &mut [u8] = &mut buf;
    Encoder::new(&mut tail, 75)
        .encode(&pixels, w, h, ColorType::Rgb)
        .unwrap();
    let n = total - tail.len();
    assert_eq!(&buf[..n], &expect[..]);

    let mut small = [0u8; 100];
    let mut tail: &mut [u8] = &mut small;
    let err = Encoder::new(&mut tail, 75)
        .encode(&pixels, w, h, ColorType::Rgb)
        .unwrap_err();
    assert!(matches!(
        err,
        rusty_jpeg::encode::EncodingError::BufferTooSmall
    ));
}

/// Synthetic 4:2:2 source: distinct Y, Cb, Cr so any channel mix-up shows.
fn yuv422(w: u16, h: u16) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (wu, hu) = (usize::from(w), usize::from(h));
    let cw = wu.div_ceil(2);
    let mut y = Vec::with_capacity(wu * hu);
    let mut cb = Vec::with_capacity(cw * hu);
    let mut cr = Vec::with_capacity(cw * hu);
    for r in 0..hu {
        for c in 0..wu {
            y.push(((c * 3 + r * 5) % 256) as u8);
        }
        for c in 0..cw {
            cb.push(((c * 11 + r * 2) % 256) as u8);
            cr.push(((c * 7 + r * 17 + 100) % 256) as u8);
        }
    }
    (y, cb, cr)
}

fn pack_yuyv(y: &[u8], cb: &[u8], cr: &[u8], w: u16, h: u16) -> Vec<u8> {
    let (wu, hu) = (usize::from(w), usize::from(h));
    let cw = wu.div_ceil(2);
    let mut out = Vec::with_capacity(cw * 4 * hu);
    for r in 0..hu {
        for c in 0..cw {
            let y0 = y[r * wu + 2 * c];
            let y1 = if 2 * c + 1 < wu {
                y[r * wu + 2 * c + 1]
            } else {
                0
            };
            out.extend_from_slice(&[y0, cb[r * cw + c], y1, cr[r * cw + c]]);
        }
    }
    out
}

/// Packed YUYV must code to exactly the bytes the planar 4:2:2 path codes to:
/// both hand the encoder the same samples, so nothing may differ.
#[test]
fn yuyv_matches_planar_422_byte_for_byte() {
    for (w, h) in [(64u16, 32u16), (33, 17), (1, 1), (2, 3)] {
        let (y, cb, cr) = yuv422(w, h);
        let cw = usize::from(w).div_ceil(2);
        let packed = pack_yuyv(&y, &cb, &cr, w, h);

        let mut a = Vec::new();
        let img = YuyvImage::new(&packed, YuyvImage::row_bytes(w), w, h).unwrap();
        let mut enc = Encoder::new(&mut a, 85);
        enc.set_sampling_factor(img.sampling_factor());
        enc.encode_image(img).unwrap();

        let mut b = Vec::new();
        let planar =
            PlanarYcbcrImage::new(&y, &cb, &cr, [usize::from(w), cw, cw], w, h, (2, 1)).unwrap();
        let mut enc = Encoder::new(&mut b, 85);
        enc.set_sampling_factor(SamplingFactor::R_4_2_2);
        enc.encode_image(planar).unwrap();

        assert_eq!(a, b, "{w}x{h}");
    }
}

/// `ColorType::Yuyv` through `encode` is the same path as the buffer type, with
/// the encoder's own default sampling factor; and a padded stride is honoured.
#[test]
fn yuyv_color_type_and_stride() {
    let (w, h) = (40u16, 24u16);
    let (y, cb, cr) = yuv422(w, h);
    let packed = pack_yuyv(&y, &cb, &cr, w, h);

    let mut a = Vec::new();
    Encoder::new(&mut a, 75)
        .encode(&packed, w, h, ColorType::Yuyv)
        .unwrap();

    let mut b = Vec::new();
    Encoder::new(&mut b, 75)
        .encode_image(YuyvImage::new(&packed, YuyvImage::row_bytes(w), w, h).unwrap())
        .unwrap();
    assert_eq!(a, b);

    // Rows padded to 128 bytes: same picture, so same bytes.
    let row = YuyvImage::row_bytes(w);
    let stride = 128;
    let mut padded = vec![0xEEu8; stride * usize::from(h)];
    for r in 0..usize::from(h) {
        padded[r * stride..r * stride + row].copy_from_slice(&packed[r * row..(r + 1) * row]);
    }
    let mut c = Vec::new();
    Encoder::new(&mut c, 75)
        .encode_image(YuyvImage::new(&padded, stride, w, h).unwrap())
        .unwrap();
    assert_eq!(a, c);

    // Too little data, a short stride, or a zero dimension are refused up front.
    assert!(YuyvImage::new(&packed[..packed.len() - 1], row, w, h).is_none());
    assert!(YuyvImage::new(&packed, row - 4, w, h).is_none());
    assert!(YuyvImage::new(&packed, row, 0, h).is_none());
    assert!(matches!(
        Encoder::new(&mut Vec::new(), 75).encode(&packed[..10], w, h, ColorType::Yuyv),
        Err(rusty_jpeg::encode::EncodingError::BadImageData { .. })
    ));

    // The decoder reads it back as a picture of the right size, at the
    // encoder's default 4:2:0 below quality 90.
    let mut d = Decoder::new(&a[..]);
    let planar = d.decode_planar().unwrap();
    assert_eq!(planar.chroma_subsampling(), Some((2, 2)));
    let info = d.info().unwrap();
    assert_eq!((info.width, info.height), (w, h));
}

/// The cross-arm golden: the same picture must code to the same bytes on
/// every build that uses the same kernels, and CI runs this file on all of
/// them.
///
/// There are two rows because the SIMD kernels are **not** bit-identical to
/// the scalar ones: the AVX2 forward DCT and the SSSE3 inverse DCT round
/// differently from their scalar twins (a ±1 LSB matter; every SIMD path is
/// gated against its own twin, `avx2_matches_scalar` and
/// `avx2_pair_matches_ssse3_twice`, but SSSE3 was never the scalar IDCT). The
/// scalar row is the one a chip produces, and it is what a `std` build
/// without `simd` (or with `platform_independent`) produces too — so **the
/// host oracle for a device is the scalar build**, `--no-default-features
/// --features std`, and this row is the host-equals-chip gate. The SIMD row
/// is checked only where those kernels actually run (x86-64 with AVX2).
///
/// A new value in either row is a bitstream change and needs a CHANGES entry.
#[test]
fn golden_bytes_are_identical_across_build_arms() {
    let baseline = encode_rgb(120, 88, 75);
    let mut fancy = Vec::new();
    {
        let mut enc = Encoder::new(&mut fancy, 92);
        enc.set_progressive(true);
        enc.set_optimized_huffman_tables(true);
        enc.set_restart_interval(4);
        enc.encode(&rgb(120, 88), 120, 88, ColorType::Rgb).unwrap();
    }
    let mut trellis = Vec::new();
    {
        let mut enc = Encoder::new(&mut trellis, 60);
        enc.set_trellis(true);
        enc.encode(&rgb(120, 88), 120, 88, ColorType::Rgb).unwrap();
    }
    // A fixed input for the decoder, so its row does not depend on the encoder's.
    let fixture = include_bytes!("fixtures/progressive_libjpeg.jpg");
    let decoded = Decoder::new(&fixture[..]).decode().unwrap();
    let got = [
        fnv1a(&baseline),
        fnv1a(&fancy),
        fnv1a(&trellis),
        fnv1a(&decoded),
    ];
    // (baseline q75, progressive+optimized+DRI q92, trellis q60, decoded fixture)
    #[cfg(not(feature = "simd"))]
    const SCALAR: [u64; 4] = [
        12050974125811034566,
        6676770829851380247,
        3819004929965713182,
        1368567530356461428,
    ];
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    const SIMD_X86_64: [u64; 4] = [
        14979219519507235267,
        9529290799696629874,
        9013122496169227150,
        11562011784721235997,
    ];
    #[cfg(not(feature = "simd"))]
    assert_eq!(got, SCALAR, "scalar kernels: coded bytes changed");
    #[cfg(feature = "simd")]
    {
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") {
            assert_eq!(got, SIMD_X86_64, "x86-64 SIMD kernels: coded bytes changed");
            return;
        }
        eprintln!("simd build without the x86-64 AVX2 kernels; unpinned: {got:?}");
    }
}
