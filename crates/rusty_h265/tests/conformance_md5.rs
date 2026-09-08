//! The standing pixel gate: decode every JCT-VC HEVC_v1 conformance stream
//! in process and compare the whole decoded YUV with the published md5, plus
//! the decoded-picture-hash SEI of every picture.
//!
//! Skipped when `hevc-vectors/` is absent (fetch it with
//! `scripts/fetch-hevc-vectors.sh`). `HEVC_VECTORS` overrides the location;
//! `HEVC_ONLY=PREFIX,PREFIX` restricts the run to matching streams.

use std::path::PathBuf;

use rusty_h265::md5::Md5;
use rusty_h265::Decoder;

fn vectors_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HEVC_VECTORS") {
        return Some(PathBuf::from(p));
    }
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.pop();
    d.pop();
    d.push("hevc-vectors");
    d.exists().then_some(d)
}

fn hex(d: [u8; 16]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn hevc_v1_conformance_bit_exact() {
    let Some(dir) = vectors_dir() else {
        // A missing corpus is a legitimate local skip, but in CI it is a green
        // build that verified nothing. `HEVC_REQUIRE_VECTORS=1` turns the skip
        // into a failure so a broken fetch cannot masquerade as a pass.
        assert!(std::env::var_os("HEVC_REQUIRE_VECTORS").is_none(), "hevc-vectors/ is missing and HEVC_REQUIRE_VECTORS is set");
        eprintln!("hevc-vectors/ not present; skipping");
        return;
    };
    let only: Vec<String> = std::env::var("HEVC_ONLY")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();

    let mut streams: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read hevc-vectors")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "bit"))
        .collect();
    streams.sort();

    let mut pass = 0usize;
    let mut sei_pass = 0usize;
    let mut sei_seen = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut n = 0usize;
    for path in streams {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if !only.is_empty() && !only.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let Ok(expected) = std::fs::read_to_string(dir.join(format!("{name}.yuv.md5"))) else {
            continue;
        };
        let expected = expected.trim().to_lowercase();
        let data = std::fs::read(&path).expect("read bitstream");
        n += 1;

        let mut dec = Decoder::new();
        dec.verify_sei = true;
        let mut errors = Vec::new();
        let mut md5 = Md5::new();
        let mut buf = Vec::new();
        let mut frames = 0u64;
        for nal in rusty_h265::nal::split_annex_b(&data) {
            if let Err(e) = dec.push_nal(nal, None) {
                errors.push(e.to_string());
            }
            while let Ok(f) = dec.next_frame() {
                buf.clear();
                f.write_yuv(&mut buf);
                md5.update(&buf);
                frames += 1;
            }
        }
        dec.flush();
        while let Ok(f) = dec.next_frame() {
            buf.clear();
            f.write_yuv(&mut buf);
            md5.update(&buf);
            frames += 1;
        }
        let got = hex(md5.finalize());
        let s = dec.stats;
        sei_seen += s.sei_checked as usize;
        sei_pass += (s.sei_checked - s.sei_mismatch) as usize;
        if got == expected && errors.is_empty() {
            pass += 1;
        } else {
            failures.push(format!(
                "{name}: md5 {got} != {expected} ({frames} frames, {} errors{}, SEI {}/{})",
                errors.len(),
                errors.first().map(|e| format!(", first: {e}")).unwrap_or_default(),
                s.sei_checked - s.sei_mismatch,
                s.sei_checked
            ));
        }
    }
    eprintln!("HEVC_v1: {pass}/{n} streams bit-exact; SEI picture hashes {sei_pass}/{sei_seen}");
    for f in &failures {
        eprintln!("  {f}");
    }
    // ---- guards against a VACUOUS pass -------------------------------------
    //
    // Every assertion below this point is of the form "nothing went wrong",
    // and every one of them is trivially satisfied by having done nothing at
    // all. Two ways that happened here:
    //
    //   * `HEVC_ONLY=typo` selected no streams, `n` was 0, and the whole gate
    //     went green in 0.01 s — indistinguishable from 147/147 to anyone
    //     reading the exit code.
    //   * `assert_eq!(sei_pass, sei_seen)` is `0 == 0` if the decoded-picture
    //     hash SEI ever stopped being parsed, so the strongest self-check in
    //     the decoder could rot silently and still report success.
    //
    // A test that cannot fail is not a gate. These make the work itself a
    // precondition of passing.
    assert!(n > 0, "no streams matched — the gate would have passed vacuously (HEVC_ONLY={only:?})");
    if only.is_empty() {
        assert!(n > 100, "corpus present but too small: {n}");
        assert!(
            sei_seen > 50,
            "only {sei_seen} pictures carried a decoded-picture-hash SEI over the whole \
             corpus — the self-check has stopped running, and `sei_pass == sei_seen` \
             below would pass on 0 == 0"
        );
    }
    assert_eq!(sei_pass, sei_seen, "decoded-picture-hash SEI mismatches");
    assert!(failures.is_empty(), "{} of {n} streams are not bit-exact", failures.len());
}
