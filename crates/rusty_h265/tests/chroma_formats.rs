//! 4:2:2 and 4:4:4 decode, gated bit-exact against FFmpeg 9.1.
//!
//! Each fixture is a small libx265 stream (the five newest encode FFmpeg's
//! synthetic `testsrc2` / `mandelbrot` sources); the expected md5 is of FFmpeg's
//! `-pix_fmt yuv422p` / `yuv444p[10le|12le]` output over the whole stream,
//! which is the layout `Frame::write_yuv` writes. All but the two oldest
//! fixtures also carry an MD5 decoded-picture-hash SEI per picture (x265
//! `hash=1`), which the decoder checks independently of FFmpeg.
//!
//! The full corpus (78 libx265 streams: intra/inter, QP 4..51, WPP, slices,
//! deblocking and SAO on/off, transform skip, constrained intra, scaling
//! lists, lossless, odd sizes, 8/10/12 bit) and the JCT-VC RExt conformance
//! streams are described in the PR that added this; these are members of it.

use rusty_h265::md5::Md5;
use rusty_h265::Decoder;

struct Fixture {
    name: &'static str,
    data: &'static [u8],
    chroma_format_idc: u8,
    bit_depth: u8,
    md5: &'static str,
    frames: usize,
    sei: bool,
}

const FIXTURES: &[Fixture] = &[
    // One intra picture each, no hash SEI (the original refusal fixtures).
    Fixture {
        name: "422_intra_8b",
        data: include_bytes!("fixtures/yuv422_x265_intra_8b.hevc"),
        chroma_format_idc: 2,
        bit_depth: 8,
        md5: "30a3f30f028939099ba30d1ebe267c97",
        frames: 1,
        sei: false,
    },
    Fixture {
        name: "444_intra_8b",
        data: include_bytes!("fixtures/yuv444_x265_intra_8b.hevc"),
        chroma_format_idc: 3,
        bit_depth: 8,
        md5: "8a6b852b1f2047801c549bbecaf6cf38",
        frames: 1,
        sei: false,
    },
    // 34x18 (conformance-window crop), inter with B-frames, WPP, SAO, deblocking.
    Fixture {
        name: "422_odd34x18_10b",
        data: include_bytes!("fixtures/yuv422_x265_odd34x18_10b.hevc"),
        chroma_format_idc: 2,
        bit_depth: 10,
        md5: "3f9c7f073f73239ff878980e2d0f0552",
        frames: 6,
        sei: true,
    },
    // 33x18, 12-bit: odd width needs no chroma rounding in 4:4:4.
    Fixture {
        name: "444_odd33x18_12b",
        data: include_bytes!("fixtures/yuv444_x265_odd33x18_12b.hevc"),
        chroma_format_idc: 3,
        bit_depth: 12,
        md5: "a20e12dcf60b42bc5a6d70c633164fc0",
        frames: 6,
        sei: true,
    },
    // 16x16 CTUs, constrained intra, deep transform trees
    // (the 4:2:2 second chroma cbf and the 4x8 chroma of 4x4 luma blocks).
    Fixture {
        name: "422_tskip_ctu16_8b",
        data: include_bytes!("fixtures/yuv422_x265_tskip_ctu16_8b.hevc"),
        chroma_format_idc: 2,
        bit_depth: 8,
        md5: "388de868e591e997c3ac7df7a2a4348f",
        frames: 6,
        sei: true,
    },
    // cu_qp_delta (adaptive quantisation) with chroma QP offsets -5 / +4 and
    // deblocking offsets.
    Fixture {
        name: "444_aq_cqp_10b",
        data: include_bytes!("fixtures/yuv444_x265_aq_cqp_10b.hevc"),
        chroma_format_idc: 3,
        bit_depth: 10,
        md5: "0ba0e1c99a9378ad4a85fc249c9c6eb8",
        frames: 8,
        sei: true,
    },
    // Default scaling lists, 12-bit 4:2:2 (the Sony A7 IV HEIF profile uses
    // scaling lists in 4:2:2).
    Fixture {
        name: "422_scaling_12b",
        data: include_bytes!("fixtures/yuv422_x265_scaling_12b.hevc"),
        chroma_format_idc: 2,
        bit_depth: 12,
        md5: "34d647e926a844b0b3d0790da7e7df9c",
        frames: 5,
        sei: true,
    },
];

fn hex(d: [u8; 16]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn chroma_422_and_444_streams_are_bit_exact() {
    for fx in FIXTURES {
        let name = fx.name;
        let mut dec = Decoder::new();
        dec.verify_sei = true;
        let mut md5 = Md5::new();
        let mut buf = Vec::new();
        let mut frames = 0usize;
        let mut drain = |dec: &mut Decoder| {
            while let Ok(f) = dec.next_frame() {
                let p = &f.picture;
                assert_eq!(p.chroma_format_idc, fx.chroma_format_idc, "{name}");
                assert_eq!(p.bit_depth_luma, fx.bit_depth, "{name}");
                // Chroma plane geometry: half width in 4:2:2, full in 4:4:4,
                // full height in both.
                let (w, h) = (p.planes[0].width, p.planes[0].height);
                let cw = if fx.chroma_format_idc == 2 { w.div_ceil(2) } else { w };
                assert_eq!((p.planes[1].width, p.planes[1].height), (cw, h), "{name}: chroma plane size");
                buf.clear();
                f.write_yuv(&mut buf);
                md5.update(&buf);
                frames += 1;
            }
        };
        for nal in rusty_h265::nal::split_annex_b(fx.data) {
            dec.push_nal(nal, None).unwrap_or_else(|e| panic!("{name}: {e}"));
            drain(&mut dec);
        }
        dec.flush();
        drain(&mut dec);
        assert_eq!(frames, fx.frames, "{name}: frame count");
        assert_eq!(hex(md5.finalize()), fx.md5, "{name}: not bit-exact with FFmpeg");
        let s = dec.stats;
        if fx.sei {
            assert_eq!(s.sei_checked as usize, fx.frames, "{name}: every picture carries a hash SEI");
        }
        assert_eq!(s.sei_mismatch, 0, "{name}: decoded-picture-hash SEI mismatch");
    }
}
