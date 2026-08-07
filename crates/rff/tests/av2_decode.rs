//! End-to-end AV2: demux an IVF, decode through the registry, and check the
//! output against the raw YUV the reference decoder produced for the same clip.
//!
//! The fixture pair lives in `testdata/`; if it is absent the test skips, so a
//! fresh clone without the (binary) fixtures still passes.

use rff_core::{CodecId, Frame};

fn testdata(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

#[test]
fn decodes_av2_ivf_byte_identical_to_the_reference() {
    let ivf = testdata("av2_424x240.ivf");
    let reference = testdata("av2_424x240.yuv");
    if !ivf.exists() || !reference.exists() {
        eprintln!("skipping: AV2 fixtures not present");
        return;
    }

    let engine = rff::Engine::new();

    // The IVF fourcc must resolve to AV2, not the VP9 default.
    let file = std::fs::File::open(&ivf).expect("open ivf");
    let mut demuxer = engine
        .formats
        .open_demuxer("ivf", Box::new(file))
        .expect("ivf demuxer");
    let streams = demuxer.read_header().expect("ivf header");
    assert_eq!(
        streams[0].codec_id,
        CodecId::Av2,
        "IVF fourcc AV02 must map to CodecId::Av2"
    );

    let mut decoder = engine
        .codecs
        .find_decoder(CodecId::Av2)
        .expect("av2 decoder registered");

    let mut out: Vec<u8> = Vec::new();
    let mut frames = 0usize;
    loop {
        match demuxer.read_packet() {
            Ok(pkt) => {
                let _ = decoder.send_packet(&pkt);
                while let Ok(Frame::Video(v)) = decoder.receive_frame() {
                    append_planes(&mut out, &v);
                    frames += 1;
                }
            }
            Err(_) => break,
        }
    }
    decoder.flush();
    while let Ok(Frame::Video(v)) = decoder.receive_frame() {
        append_planes(&mut out, &v);
        frames += 1;
    }

    let expected = std::fs::read(&reference).expect("reference yuv");
    assert!(frames > 0, "decoded no frames");
    assert_eq!(
        out.len(),
        expected.len(),
        "decoded {frames} frames: output size {} != reference {}",
        out.len(),
        expected.len()
    );
    assert!(
        out == expected,
        "decoded {frames} frames but output is not byte-identical to the reference"
    );
}

/// Copy the visible rows out of each (stride-padded) plane, which is what the
/// reference decoder's raw dump contains.
fn append_planes(out: &mut Vec<u8>, v: &rff_core::VideoFrame) {
    for (i, plane) in v.planes.iter().enumerate() {
        let stride = v.strides[i];
        let (w, h) = if i == 0 {
            (v.width as usize, v.height as usize)
        } else {
            // 4:2:0 in the fixture.
            (v.width as usize / 2, v.height as usize / 2)
        };
        for row in 0..h {
            let start = row * stride;
            out.extend_from_slice(&plane[start..start + w]);
        }
    }
}
