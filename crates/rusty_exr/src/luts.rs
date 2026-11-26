//! Lookup tables shared with OpenEXR.
//!
//! OpenEXR builds four 64K-entry half-to-half tables at run time with the C
//! library's `expf`/`logf`/`powf` (B44's log/exp transfer for `pLinear`
//! channels, DWA's perceptual transfer). The results depend on the libm, so
//! computing them here with Rust's float functions would match OpenEXR on
//! some platforms and not others. Instead the tables are the exact output
//! of OpenEXR 3.5.5 on glibc x86-64 (its reference build), stored as
//! delta-coded byte planes under zlib (10 KB) and expanded once on first
//! use. A test pins their checksum.

use std::sync::OnceLock;

static BLOB: &[u8] = include_bytes!("luts.z");

// The encode-side tables are used by the B44/DWA writers.
#[allow(dead_code)]
pub(crate) struct Luts {
    /// B44 decode: `convertToLinear` (8 * log(x)).
    pub b44_log: Vec<u16>,
    /// B44 encode: `convertFromLinear` (exp(x / 8)).
    pub b44_exp: Vec<u16>,
    /// DWA decode: perceptual to linear.
    pub dwa_to_linear: Vec<u16>,
    /// DWA encode: linear to perceptual.
    pub dwa_to_nonlinear: Vec<u16>,
}

fn build() -> Luts {
    let mut raw = vec![0u8; 4 * 2 * 65536];
    crate::compression::inflate_exact(BLOB, &mut raw).expect("embedded tables inflate");
    let mut tables: Vec<Vec<u16>> = Vec::with_capacity(4);
    for t in raw.chunks_exact(2 * 65536) {
        let (lo, hi) = t.split_at(65536);
        let mut prev = 0u16;
        let tab: Vec<u16> = lo
            .iter()
            .zip(hi)
            .map(|(&l, &h)| {
                prev = prev.wrapping_add(u16::from_le_bytes([l, h]));
                prev
            })
            .collect();
        tables.push(tab);
    }
    let mut it = tables.into_iter();
    Luts {
        b44_log: it.next().expect("4 tables"),
        b44_exp: it.next().expect("4 tables"),
        dwa_to_linear: it.next().expect("4 tables"),
        dwa_to_nonlinear: it.next().expect("4 tables"),
    }
}

pub(crate) fn get() -> &'static Luts {
    static LUTS: OnceLock<Luts> = OnceLock::new();
    LUTS.get_or_init(build)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tables_match_openexr() {
        let l = super::get();
        // Spot values from OpenEXR 3.5.5 (glibc): log(1.0) = 0, exp(0) = 1.0,
        // the DWA transfer fixes 0 and 1.0, NaN/Inf map to 0.
        assert_eq!(l.b44_log[0x3c00], 0x0000);
        assert_eq!(l.b44_exp[0x0000], 0x3c00);
        assert_eq!(l.dwa_to_linear[0x3c00], 0x3c00);
        assert_eq!(l.dwa_to_nonlinear[0x3c00], 0x3c00);
        assert_eq!(l.dwa_to_linear[0x7c00], 0);
        assert_eq!(l.b44_exp[0x7e00], 0);
        // FNV-1a over all four tables (pins the whole blob).
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for t in [
            &l.b44_log,
            &l.b44_exp,
            &l.dwa_to_linear,
            &l.dwa_to_nonlinear,
        ] {
            for v in t {
                for b in v.to_le_bytes() {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
        }
        assert_eq!(h, TABLES_FNV);
    }

    const TABLES_FNV: u64 = 0xb78f_c5e6_035e_d928;
}
