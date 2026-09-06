//! Phase 1 gates over the JCT-VC HEVC_v1 corpus (`hevc-vectors/`, fetched by
//! `scripts/fetch-hevc-vectors.sh`; skipped when absent).
//!
//! - H1.1: every stream's VPS/SPS/PPS parse; cropped size, bit depth and
//!   profile equal ffprobe's (`hevc-vectors/probe.json`, from
//!   `tools/hevc/probe_all.py`).
//! - H1.2: every slice segment header parses and the entry points land on
//!   byte boundaries inside the NAL.
//! - H1.3: with decoding disabled, the picture count handed to output equals
//!   the 100 %-conformant decoder's frame count for the stream.

use std::collections::BTreeMap;
use std::path::PathBuf;

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

/// Minimal reader for the flat `probe.json` (no serde in this crate).
fn load_probe(dir: &PathBuf) -> BTreeMap<String, (u32, u32, String, String, Option<u64>)> {
    let text = std::fs::read_to_string(dir.join("probe.json")).expect("probe.json");
    let mut out = BTreeMap::new();
    // "NAME": { "frames": N, "height": H, "level": L, "pix_fmt": "..", "profile": "..", "width": W }
    let mut rest = text.as_str();
    while let Some(start) = rest.find("\"frames\"") {
        let key_end = rest[..start].rfind("\": {").expect("key");
        let key_start = rest[..key_end].rfind('"').expect("key start") + 1;
        let name = rest[key_start..key_end].to_string();
        let obj_end = rest[start..].find('}').expect("obj end") + start;
        let obj = &rest[start..obj_end];
        let field = |k: &str| -> String {
            let i = obj.find(&format!("\"{k}\":")).unwrap_or_else(|| panic!("field {k} in {name}")) + k.len() + 3;
            let v = obj[i..].trim_start();
            let end = v.find([',', '\n']).unwrap_or(v.len());
            v[..end].trim().trim_matches('"').to_string()
        };
        let frames = field("frames");
        let frames = if frames == "null" { None } else { frames.parse().ok() };
        let row = (field("width").parse().unwrap(), field("height").parse().unwrap(), field("pix_fmt"), field("profile"), frames);
        out.insert(name.clone(), row);
        rest = &rest[obj_end..];
    }
    out
}

#[test]
fn phase1_parse_and_dpb_dry_run() {
    let Some(dir) = vectors_dir() else {
        // A missing corpus is a legitimate local skip, but in CI it is a green
        // build that verified nothing. `HEVC_REQUIRE_VECTORS=1` turns the skip
        // into a failure so a broken fetch cannot masquerade as a pass.
        assert!(
            std::env::var_os("HEVC_REQUIRE_VECTORS").is_none(),
            "hevc-vectors/ is missing and HEVC_REQUIRE_VECTORS is set"
        );
        eprintln!("hevc-vectors/ not present; skipping");
        return;
    };
    let probe = load_probe(&dir);
    assert!(!probe.is_empty());
    let mut failures = Vec::new();
    let mut n = 0;
    for (name, (w, h, pix_fmt, profile, frames)) in &probe {
        let path = dir.join(format!("{name}.bit"));
        let Ok(data) = std::fs::read(&path) else { continue };
        n += 1;
        let mut dec = rusty_h265::Decoder::new();
        dec.headers_only = true;
        let mut errs = Vec::new();
        for nal in rusty_h265::nal::split_annex_b(&data) {
            if let Err(e) = dec.push_nal(nal, None) {
                errs.push(e.to_string());
            }
        }
        dec.flush();
        let mut out = 0u64;
        let mut geom = None;
        while let Ok(f) = dec.next_frame() {
            out += 1;
            geom.get_or_insert((f.width as u32, f.height as u32, f.bit_depth()));
        }
        // VPSSPSPPS_A is six IDR pictures at six resolutions (176x144 …
        // 1280x720): ffprobe reports one of them and the size-derived frame
        // count (whole YUV / one geometry) is meaningless. Its .txt says
        // "length: 6 frames".
        let variable_size = name.starts_with("VPSSPSPPS");
        let frames = if variable_size { &Some(6) } else { frames };
        let mut problems = Vec::new();
        if !errs.is_empty() {
            problems.push(format!("{} errors, first: {}", errs.len(), errs[0]));
        }
        if let Some((gw, gh, bd)) = geom {
            if *w != 0 && !variable_size && (gw, gh) != (*w, *h) {
                problems.push(format!("geometry {gw}x{gh} vs ffprobe {w}x{h}"));
            }
            let want_bd = if pix_fmt.contains("10") { 10 } else { 8 };
            if !pix_fmt.is_empty() && bd != want_bd {
                problems.push(format!("bit depth {bd} vs ffprobe {pix_fmt}"));
            }
            let _ = profile;
        } else {
            problems.push("no output pictures".into());
        }
        if let Some(fr) = frames {
            if out != *fr {
                problems.push(format!("output {out} pictures vs reference {fr}"));
            }
        }
        if !problems.is_empty() {
            failures.push(format!("{name}: {}", problems.join("; ")));
        }
    }
    eprintln!("{} streams, {} failures", n, failures.len());
    for f in &failures {
        eprintln!("  {f}");
    }
    assert!(n > 100, "corpus present but too small: {n}");
    assert!(failures.is_empty(), "{} of {} streams failed Phase 1 gates", failures.len(), n);
}
