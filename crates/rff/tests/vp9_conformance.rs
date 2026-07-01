//! VP9 conformance — decode each official libvpx test vector and compare its
//! per-frame I420 MD5 to the published `.md5`. This is the bit-exactness gate
//! that protects the decoder (and anything that refactors it, e.g. threading).
//!
//! The vectors are large + external, so this is `#[ignore]`d by default. Point
//! it at a directory of `name.webm` + `name.webm.md5` pairs:
//!
//! ```sh
//! VP9_VECTORS_DIR=/path/to/vectors \
//!   cargo test -p rff --test vp9_conformance -- --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use rff::Engine;
use rff_codec::{CodecParams, Decoder};
use rff_core::{CodecId, Frame, MediaType, PixelFormat, VideoFrame};

/// libvpx-style per-frame MD5: each plane (Y, U, V) at display size, tightly
/// packed, read through the frame's stride. 8-bit planar YUV only (the
/// `.i420/.i422/.i444` vectors); high-bit-depth vectors are skipped here.
fn frame_md5(f: &VideoFrame) -> Option<String> {
    let (cx, cy) = match f.format {
        PixelFormat::Yuv420p => (1usize, 1usize),
        PixelFormat::Yuv422p => (1, 0),
        PixelFormat::Yuv444p => (0, 0),
        _ => return None,
    };
    let (w, h) = (f.width as usize, f.height as usize);
    let dims = [
        (w, h),
        ((w + cx) >> cx, (h + cy) >> cy),
        ((w + cx) >> cx, (h + cy) >> cy),
    ];
    let mut md5 = Md5::new();
    for (p, (pw, ph)) in dims.iter().enumerate() {
        let (plane, stride) = (&f.planes[p], f.strides[p]);
        for row in 0..*ph {
            md5.update(&plane[row * stride..row * stride + pw]);
        }
    }
    Some(md5.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn drain(decoder: &mut dyn Decoder, hashes: &mut Vec<String>) {
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(v)) => {
                if let Some(h) = frame_md5(&v) {
                    hashes.push(h);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// Decode a `.webm` vector to its sequence of per-frame MD5s.
fn decode_vector(engine: &Engine, webm: &Path) -> Vec<String> {
    let file = fs::File::open(webm).unwrap();
    let mut demuxer = engine
        .formats
        .open_demuxer("matroska", Box::new(file))
        .unwrap();
    let streams = demuxer.read_header().unwrap();
    let vidx = streams
        .iter()
        .position(|s| s.media_type == MediaType::Video)
        .unwrap();

    let mut decoder = engine.codecs.find_decoder(CodecId::Vp9).unwrap();
    let _ = decoder.configure(&CodecParams {
        codec_id: CodecId::Vp9,
        ..Default::default()
    });

    let mut hashes = Vec::new();
    loop {
        match demuxer.read_packet() {
            Ok(pkt) if pkt.stream_index == vidx => {
                let _ = decoder.send_packet(&pkt);
                drain(&mut *decoder, &mut hashes);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    decoder.flush();
    drain(&mut *decoder, &mut hashes);
    hashes
}

fn md5_sidecar(webm: &Path) -> PathBuf {
    let mut s = webm.to_path_buf().into_os_string();
    s.push(".md5");
    PathBuf::from(s)
}

#[test]
#[ignore = "needs VP9_VECTORS_DIR with libvpx test vectors"]
fn vp9_conformance() {
    let dir = std::env::var("VP9_VECTORS_DIR")
        .expect("set VP9_VECTORS_DIR to a directory of name.webm + name.webm.md5");
    let engine = Engine::new();

    let mut vectors: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "webm"))
        .collect();
    vectors.sort();

    let (mut total, mut passed) = (0usize, 0usize);
    let mut failures = Vec::new();
    for webm in &vectors {
        let md5path = md5_sidecar(webm);
        if !md5path.exists() {
            continue;
        }
        let expected: Vec<String> = fs::read_to_string(&md5path)
            .unwrap()
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect();
        let got = decode_vector(&engine, webm);
        let name = webm.file_name().unwrap().to_string_lossy();
        total += 1;
        if got == expected {
            passed += 1;
            println!("PASS  {name}  ({} frames)", got.len());
        } else {
            let matched = got
                .iter()
                .zip(&expected)
                .take_while(|(a, b)| a == b)
                .count();
            println!(
                "FAIL  {name}  got {} / expected {} frames, first {matched} match",
                got.len(),
                expected.len()
            );
            failures.push(name.to_string());
        }
    }

    println!("\nVP9 conformance: {passed}/{total} vectors bit-exact");
    assert!(
        failures.is_empty(),
        "{} vectors failed: {failures:?}",
        failures.len()
    );
}
