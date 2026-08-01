//! `decode_planar` must be the same decode as `decode`, minus the upsample and
//! colour conversion — so reconstructing RGB from the planes by hand has to
//! reproduce `decode()` closely, and the plane geometry has to match the JPEG's
//! declared subsampling.

use rusty_jpeg::decode::Decoder;
use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
use std::io::Cursor;

/// A deterministic image with real chroma detail (flat content would hide a
/// chroma plane that was silently wrong).
fn source(w: usize, h: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 3;
            rgb[o] = ((x * 7 + y * 3) % 256) as u8;
            rgb[o + 1] = ((x * 3 + y * 11) % 256) as u8;
            rgb[o + 2] = ((x * 13 + y * 5) % 256) as u8;
        }
    }
    rgb
}

fn encode(w: usize, h: usize, sampling: SamplingFactor) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, 95);
    enc.set_sampling_factor(sampling);
    enc.encode(&source(w, h), w as u16, h as u16, ColorType::Rgb)
        .expect("encode");
    out
}

#[test]
fn plane_geometry_matches_the_declared_subsampling() {
    for (sampling, want) in [
        (SamplingFactor::R_4_4_4, (1, 1)),
        (SamplingFactor::R_4_2_2, (2, 1)),
        (SamplingFactor::R_4_2_0, (2, 2)),
    ] {
        let jpeg = encode(64, 48, sampling);
        let planar = Decoder::new(Cursor::new(&jpeg))
            .decode_planar()
            .expect("decode_planar");

        assert_eq!(planar.components.len(), 3, "{sampling:?}");
        assert_eq!((planar.width, planar.height), (64, 48));
        assert_eq!(planar.chroma_subsampling(), Some(want), "{sampling:?}");

        let (sh, sv) = want;
        assert_eq!(planar.components[0].width, 64, "{sampling:?} luma width");
        assert_eq!(planar.components[0].height, 48, "{sampling:?} luma height");
        for c in &planar.components[1..] {
            assert_eq!(c.width, 64usize.div_ceil(sh), "{sampling:?} chroma width");
            assert_eq!(c.height, 48usize.div_ceil(sv), "{sampling:?} chroma height");
            assert!(c.stride >= c.width);
            assert!(c.data.len() >= c.stride * (c.height - 1) + c.width);
        }
    }
}

/// Mean absolute error between `decode()`'s RGB and RGB rebuilt from the planes
/// with nearest-neighbour chroma. `swap_chroma` is the control arm.
fn reconstruction_error(jpeg: &[u8], w: usize, h: usize, swap_chroma: bool) -> f64 {
    let rgb = Decoder::new(Cursor::new(jpeg)).decode().expect("decode");
    let planar = Decoder::new(Cursor::new(jpeg))
        .decode_planar()
        .expect("decode_planar");
    let (sh, sv) = planar.chroma_subsampling().unwrap();

    let yp = &planar.components[0];
    let (cb, cr) = if swap_chroma {
        (&planar.components[2], &planar.components[1])
    } else {
        (&planar.components[1], &planar.components[2])
    };

    let mut total = 0f64;
    for y in 0..h {
        for x in 0..w {
            let yy = yp.data[y * yp.stride + x] as f32;
            let b = cb.data[(y / sv) * cb.stride + (x / sh)] as f32 - 128.0;
            let r = cr.data[(y / sv) * cr.stride + (x / sh)] as f32 - 128.0;
            let want = [
                yy + 1.402 * r,
                yy - 0.344136 * b - 0.714136 * r,
                yy + 1.772 * b,
            ];
            for (c, wv) in want.iter().enumerate() {
                let got = rgb[(y * w + x) * 3 + c] as i32;
                total += (got - wv.round().clamp(0.0, 255.0) as i32).abs() as f64;
            }
        }
    }
    total / (w * h * 3) as f64
}

/// The planes must be the *same samples* the interleaved path starts from.
///
/// At 4:4:4 nothing is upsampled, so rebuilding RGB by hand reproduces
/// `decode()` almost exactly. At 4:2:x the reference upsampler is a real filter
/// while this rebuild is nearest-neighbour, so single pixels at a sharp chroma
/// edge legitimately differ a lot — the *mean* is the meaningful statistic, and
/// the swapped-chroma control arm below is what proves the bar has teeth.
#[test]
fn planes_reconstruct_the_interleaved_decode() {
    for (sampling, tol) in [
        (SamplingFactor::R_4_4_4, 0.5),
        (SamplingFactor::R_4_2_2, 8.0),
        (SamplingFactor::R_4_2_0, 12.0),
    ] {
        let (w, h) = (64usize, 48usize);
        let jpeg = encode(w, h, sampling);

        let err = reconstruction_error(&jpeg, w, h, false);
        assert!(
            err <= tol,
            "{sampling:?}: mean error {err:.2} exceeds {tol}"
        );

        // Control: if the bar were slack, a wrong plane assignment would pass.
        let swapped = reconstruction_error(&jpeg, w, h, true);
        assert!(
            swapped > tol * 2.0,
            "{sampling:?}: swapping Cb/Cr only moved mean error to {swapped:.2}, \
             so this test cannot detect a wrong plane"
        );
    }
}

#[test]
fn grayscale_yields_a_single_plane() {
    let mut out = Vec::new();
    let gray: Vec<u8> = (0..(32 * 32)).map(|i| (i % 256) as u8).collect();
    Encoder::new(&mut out, 90)
        .encode(&gray, 32, 32, ColorType::Luma)
        .expect("encode");
    let planar = Decoder::new(Cursor::new(&out))
        .decode_planar()
        .expect("decode_planar");
    assert_eq!(planar.components.len(), 1);
    assert_eq!(planar.chroma_subsampling(), None);
}

#[test]
fn planar_request_does_not_leak_into_a_later_decode() {
    let jpeg = encode(32, 32, SamplingFactor::R_4_2_0);
    let mut d = Decoder::new(Cursor::new(&jpeg));
    assert!(d.decode_planar().is_ok());
    let mut d2 = Decoder::new(Cursor::new(&jpeg));
    assert!(!d2.decode().unwrap().is_empty());
}

/// `set_single_threaded(true)` must actually select the single-threaded worker
/// for a BASELINE image, not just for progressive ones.
///
/// It did not. The scan-loop call site hardcoded `PreferWorkerKind::Multithreaded`
/// and `get_or_init_worker` caches the worker on first use, so the hardcoded site
/// won and the flag became a no-op. Nothing caught it because the output is
/// identical either way -- only the CPU cost differs, and it differed a lot:
/// the threaded path burned ~38% more CPU on one pinned core and left
/// `reclaim_buffer` (implemented only by the immediate worker) dead, so every
/// MCU row reallocated instead of recycling.
#[test]
fn single_threaded_flag_selects_the_immediate_worker_on_baseline() {
    let jpeg = encode(64, 64, SamplingFactor::R_4_2_0);

    let mut d = Decoder::new(Cursor::new(&jpeg));
    d.set_single_threaded(true);
    let st = d.decode().expect("decode st");
    assert!(
        rusty_jpeg::decode::last_worker_was_immediate(),
        "set_single_threaded(true) did not select the immediate worker"
    );

    let mut d = Decoder::new(Cursor::new(&jpeg));
    d.set_single_threaded(false);
    let mt = d.decode().expect("decode mt");

    // The flag is a cost knob, never a correctness one.
    assert_eq!(st, mt, "single-threaded decode diverged from multithreaded");
}
