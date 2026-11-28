//! 4:0:0 (monochrome) decode, gated bit-exact against FFmpeg 9.1.
//!
//! Each fixture is a small 4:0:0 stream; the expected md5 is of FFmpeg's
//! `-pix_fmt gray` / `gray10le` output over the whole stream, which is the
//! layout `Frame::write_yuv` writes for a monochrome picture. Every fixture
//! also carries an MD5 decoded-picture-hash SEI per picture (x265 `hash=1`),
//! which the decoder checks independently of FFmpeg.
//!
//! The full corpus (x265 and kvazaar: intra/inter, QP 4..51, WPP, tiles,
//! slices, deblocking and SAO on/off, odd sizes, 8 and 10 bit) is described in
//! the PR that added this; these are two small members of it.

use rusty_h265::md5::Md5;
use rusty_h265::Decoder;

const FIXTURES: &[(&str, &[u8], &str, usize)] = &[
    // 33x18 (conformance-window crop), x265 inter: B-frames, WPP, SAO, deblocking.
    ("x265_tiny_inter_8b", include_bytes!("fixtures/mono400_x265_tiny_inter_8b.hevc"), "b24b71d47b965f27b6af2f850913a3d5", 11),
    // 408x232 10-bit, x265 inter at QP 47: heavy deblocking and SAO.
    (
        "x265_inter_qp47_10b",
        include_bytes!("fixtures/mono400_x265_inter_qp47_10b.hevc"),
        "5b75898e231ff383abe0325e6d61c380",
        24,
    ),
];

fn hex(d: [u8; 16]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn monochrome_streams_are_bit_exact() {
    for &(name, data, want, want_frames) in FIXTURES {
        let mut dec = Decoder::new();
        dec.verify_sei = true;
        let mut md5 = Md5::new();
        let mut buf = Vec::new();
        let mut frames = 0usize;
        let mut drain = |dec: &mut Decoder| {
            while let Ok(f) = dec.next_frame() {
                assert_eq!(f.picture.chroma_format_idc, 0, "{name}");
                assert!(f.picture.planes[1].data.is_empty() && f.picture.planes[2].data.is_empty(), "{name}: chroma planes allocated");
                buf.clear();
                f.write_yuv(&mut buf);
                assert_eq!(buf.len(), f.width * f.height * if f.bit_depth() > 8 { 2 } else { 1 }, "{name}: luma only");
                md5.update(&buf);
                frames += 1;
            }
        };
        for nal in rusty_h265::nal::split_annex_b(data) {
            dec.push_nal(nal, None).unwrap_or_else(|e| panic!("{name}: {e}"));
            drain(&mut dec);
        }
        dec.flush();
        drain(&mut dec);
        assert_eq!(frames, want_frames, "{name}: frame count");
        assert_eq!(hex(md5.finalize()), want, "{name}: not bit-exact with FFmpeg");
        let s = dec.stats;
        assert_eq!(s.sei_checked as usize, want_frames, "{name}: every picture carries a hash SEI");
        assert_eq!(s.sei_mismatch, 0, "{name}: decoded-picture-hash SEI mismatch");
    }
}
