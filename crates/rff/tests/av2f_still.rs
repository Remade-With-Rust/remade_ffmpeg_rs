//! End-to-end AV2F: sniff the container, demux it, decode the AV2 still through
//! the registry, and check the pixels against what the reference decoder (avmdec)
//! produced for the same picture. Then mux it back and confirm the file the
//! engine writes is the file it reads.
//!
//! AV2F is EXPERIMENTAL — its four-character codes are ours, not AOM's. The
//! committed `.av2f` fixture therefore doubles as a byte-layout pin: if the
//! container writer changes, this test fails rather than silently minting a new
//! dialect.
//!
//! The fixture pair lives in `testdata/`; if it is absent the test skips, so a
//! fresh clone without the (binary) fixtures still passes.

use rff_core::{CodecId, Frame};

fn testdata(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

fn fixtures() -> Option<(Vec<u8>, Vec<u8>)> {
    let file = testdata("av2_still_432x240.av2f");
    let reference = testdata("av2_still_432x240.yuv");
    if !file.exists() || !reference.exists() {
        eprintln!("skipping: AV2F fixtures not present");
        return None;
    }
    Some((
        std::fs::read(&file).expect("read av2f"),
        std::fs::read(&reference).expect("read reference yuv"),
    ))
}

#[test]
fn decodes_av2f_byte_identical_to_the_reference() {
    let Some((av2f, expected)) = fixtures() else {
        return;
    };
    let engine = rff::Engine::new();

    // Content sniffing must pick AV2F from the bytes alone, with no filename.
    let format = engine.formats.probe(&av2f).expect("probe must detect av2f");
    assert_eq!(format.name, "av2f");

    let mut demuxer = engine
        .formats
        .open_demuxer("av2f", Box::new(std::io::Cursor::new(av2f.clone())))
        .expect("av2f demuxer");
    let streams = demuxer.read_header().expect("av2f header");
    assert_eq!(streams.len(), 1, "a still image carries one stream");
    assert_eq!(streams[0].codec_id, CodecId::Av2);
    assert_eq!((streams[0].width, streams[0].height), (432, 240));

    let mut decoder = engine
        .codecs
        .find_decoder(CodecId::Av2)
        .expect("av2 decoder registered");

    let packet = demuxer.read_packet().expect("the still picture");
    assert!(packet.flags.keyframe);
    let _ = decoder.send_packet(&packet);
    decoder.flush();

    let mut out: Vec<u8> = Vec::new();
    let mut frames = 0usize;
    while let Ok(Frame::Video(v)) = decoder.receive_frame() {
        append_planes(&mut out, &v);
        frames += 1;
    }

    assert_eq!(frames, 1, "an AV2F file decodes to exactly one picture");
    assert_eq!(
        out.len(),
        expected.len(),
        "output size {} != reference {}",
        out.len(),
        expected.len()
    );
    assert!(
        out == expected,
        "decoded picture is not byte-identical to the avmdec reference"
    );

    // A still image is one picture; there is nothing after it.
    assert!(demuxer.read_packet().is_err());
}

#[test]
fn muxes_back_to_the_committed_fixture() {
    let Some((av2f, _)) = fixtures() else {
        return;
    };
    let engine = rff::Engine::new();

    // Pull the payload out through the demuxer...
    let mut demuxer = engine
        .formats
        .open_demuxer("av2f", Box::new(std::io::Cursor::new(av2f.clone())))
        .expect("av2f demuxer");
    let streams = demuxer.read_header().expect("av2f header");
    let packet = demuxer.read_packet().expect("the still picture");

    // ...and write it straight back out.
    let sink = SharedBuf::default();
    {
        let mut muxer = engine
            .formats
            .open_muxer("av2f", Box::new(sink.clone()))
            .expect("av2f muxer");
        muxer.write_header(&streams).expect("write header");
        muxer.write_packet(&packet).expect("write packet");
        muxer.write_trailer().expect("write trailer");
    }

    let written = sink.take();
    assert_eq!(
        written, av2f,
        "the engine must write back the exact bytes it read — a difference here \
         means the container layout drifted from the committed fixture"
    );
}

#[test]
fn rejects_an_avif_file() {
    let engine = rff::Engine::new();
    let avif = testdata("avif_sample.avif");
    if !avif.exists() {
        // Synthesize the discriminating part: same box, different brand.
        let mut bytes = vec![0, 0, 0, 32];
        bytes.extend_from_slice(b"ftypavif");
        bytes.extend_from_slice(&[0; 20]);
        let picked = engine.formats.probe(&bytes).map(|f| f.name);
        assert_ne!(picked, Some("av2f"), "av2f must not claim AVIF files");
        return;
    }
    let bytes = std::fs::read(&avif).expect("read avif");
    let picked = engine.formats.probe(&bytes).map(|f| f.name);
    assert_ne!(picked, Some("av2f"), "av2f must not claim AVIF files");
}

#[test]
fn a_truncated_file_errors_rather_than_panicking() {
    let Some((av2f, _)) = fixtures() else {
        return;
    };
    let engine = rff::Engine::new();
    // Every prefix must either parse or fail cleanly — never panic.
    for cut in (0..av2f.len()).step_by(7) {
        let mut demuxer = engine
            .formats
            .open_demuxer("av2f", Box::new(std::io::Cursor::new(av2f[..cut].to_vec())))
            .expect("av2f demuxer");
        let _ = demuxer.read_header();
    }
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

/// A `Write` sink whose bytes stay readable after the muxer consumes it.
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedBuf {
    fn take(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
