//! Progressive JPEGs from other encoders must decode, not panic.
//!
//! libjpeg, mozjpeg and Photoshop emit DHT segments **per scan**. A progressive
//! file therefore opens with a DC-only scan (`Ss=0, Se=0`) that names an AC
//! table slot which has not been defined yet — the AC tables arrive later, just
//! before the scans that need them.
//!
//! Our own encoder does not do this: it writes all four tables up front and
//! references an AC table even on DC-only scans. So a fixture produced by this
//! crate could never expose the defect, and none did. The fixture below comes
//! from libjpeg (via Pillow) precisely because it has the layout our encoder
//! cannot produce.
//!
//! The regression: an optimization hoisted `ac_table.unwrap()` out of the AC
//! loop. For a DC-only scan the loop never runs, so the unwrap had never been
//! reached — until it was moved in front of it. That shipped in 0.1.6 and
//! 0.1.7 and panicked on the majority of real progressive JPEGs. Reported from
//! a document-processing workload where progressive files are common.

use rusty_jpeg::decode::Decoder;
use std::io::Cursor;

/// A real libjpeg progressive file: DHT(DC) x2, then `SOS ns=3 Ss=0 Se=0`
/// naming AC table 0, which is not defined until after that scan.
const LIBJPEG_PROGRESSIVE: &[u8] = include_bytes!("fixtures/progressive_libjpeg.jpg");

#[test]
fn progressive_dc_scan_before_any_ac_table_does_not_panic() {
    let img = Decoder::new(Cursor::new(LIBJPEG_PROGRESSIVE))
        .decode()
        .expect("libjpeg progressive file must decode");
    assert_eq!(img.len(), 32 * 32 * 3, "unexpected decoded size");

    // Not merely non-panicking: the image must actually be there. An all-zero
    // or all-constant result would satisfy a size check while being wrong.
    let first = img[0];
    assert!(
        img.iter().any(|&p| p != first),
        "decoded progressive image is a flat constant"
    );
}

/// The planar path assembles output separately and has diverged from `decode()`
/// before, so it gets its own check.
#[test]
fn progressive_decodes_through_the_planar_path_too() {
    let mut d = Decoder::new(Cursor::new(LIBJPEG_PROGRESSIVE));
    d.set_single_threaded(true);
    let img = d.decode_planar().expect("planar progressive decode");
    assert_eq!(img.components.len(), 3);
    for c in &img.components {
        let need = c.stride * c.height.saturating_sub(1) + c.width;
        assert!(c.data.len() >= need, "plane smaller than its geometry");
    }
}

/// A scan that genuinely codes AC coefficients without defining an AC table is
/// malformed. That must be an `Err`, never a panic — the fix must not turn one
/// crash into a different one.
#[test]
fn missing_ac_table_on_an_ac_scan_is_an_error_not_a_panic() {
    // Truncating inside the table definitions produces files that reference
    // tables they never define; every outcome must be Ok or Err.
    for cut in (24..LIBJPEG_PROGRESSIVE.len()).step_by(7) {
        let data = &LIBJPEG_PROGRESSIVE[..cut];
        let res = std::panic::catch_unwind(|| {
            let mut d = Decoder::new(Cursor::new(data));
            d.set_single_threaded(true);
            d.decode().is_ok()
        });
        assert!(
            res.is_ok(),
            "panicked on a progressive file truncated to {cut} bytes"
        );
    }
}

/// A progressive frame header must not be able to size an unbounded allocation.
///
/// Progressive keeps every coefficient of the whole image resident, because
/// later scans revisit the same blocks — so that buffer is sized straight from
/// the SOF. A fuzzer reached `malloc(8589934592)` (8 GiB) from a small input:
/// `decoding_buffer_size_limit` existed but was only enforced in
/// `decode_planes`, which runs after every scan has been decoded, long after
/// the allocation it is meant to bound.
#[test]
fn oversized_progressive_frame_is_rejected_before_allocating() {
    // Rewrite the SOF2 dimensions of a real progressive file to 65535x65535.
    // The entropy data no longer matches, which is the point: the guard must
    // fire on the header, before anything large is allocated.
    let mut data = LIBJPEG_PROGRESSIVE.to_vec();
    let mut i = 2;
    let mut patched = false;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = data[i + 1];
        if m == 0xD8 || m == 0xD9 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        let ln = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
        if m == 0xC2 {
            // SOF2 payload: precision(1), height(2), width(2), ...
            data[i + 5] = 0xFF;
            data[i + 6] = 0xFF;
            data[i + 7] = 0xFF;
            data[i + 8] = 0xFF;
            patched = true;
            break;
        }
        i += 2 + ln;
    }
    assert!(patched, "test fixture has no SOF2 to patch");

    let mut d = Decoder::new(Cursor::new(&data));
    d.set_single_threaded(true);
    // 64 MB: far above anything legitimate here, far below the ~8 GiB the
    // unpatched header would demand.
    d.set_max_decoding_buffer_size(64 * 1024 * 1024);

    // Must be an error. Reaching the allocation would OOM rather than fail.
    assert!(
        d.decode().is_err(),
        "a 65535x65535 progressive frame was accepted under a 64 MB limit"
    );
}
