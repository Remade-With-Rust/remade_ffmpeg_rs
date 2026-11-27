//! The writer's bytes against the `exr` crate's (1.74.2).
//!
//! For every case in `support/gen.rs` the expected length and FNV-1a hash
//! are those of the file `exr` writes for the same image and settings
//! (single-threaded, which fixes its chunk order for `LineOrder::Random`).
//! The images use channel names of at most 31 bytes, where the two writers
//! agree on every byte.
//!
//! DWA output depends on DCT basis constants that `exr` derives from the
//! platform's `cosf` (three of them differ in the last bit between glibc
//! and the Windows CRT) and, off x86, on a different `exr` DCT kernel. The
//! DWA expectations below were taken with `exr` on both x86-64 platforms,
//! where they agree for these images, and are checked only there.

#[path = "support/gen.rs"]
mod gen;

use rusty_exr::Compression;

/// `(length, fnv)` of `exr`'s file for each non-DWA case, in
/// `gen::cases()` order.
const EXR_COMMON: &[(usize, u64)] = &[
    (19031, 0xbdfae166c1fde71a),
    (89823, 0xd9a4f9d209be93cd),
    (18479, 0x9019fda07d61c509),
    (89528, 0xc57ed08f2d933ce6),
    (15800, 0xfe3967e6c7fb2c0d),
    (85583, 0x85d4ac8b00b14bbb),
    (12757, 0x97df366dfe2b1c31),
    (76896, 0x64c6f3f96a0564dc),
    (16002, 0x3858804be5593f3f),
    (87138, 0xdfcc2983fc6f376b),
    (12069, 0xd6653dac904142a2),
    (49457, 0xf93cb05ebc5f5ed7),
    (9375, 0xc50960356d4e4a0f),
    (75647, 0x26b938eab557cc31),
    (9331, 0x281d41c32751a7ab),
    (75647, 0xad1b2e181a03be4a),
    (5061, 0x4533430bfa33b454),
    (5061, 0xcd8f16ae01ed5133),
    (69895, 0xceb0b45f9257a17b),
    (50103, 0x6b9d1de9d42c5044),
    (58098, 0xe785c79987018f67),
    (33824, 0xa6c7c3ced04e4d64),
    (88578, 0xfbedd024019bc17f),
    (18518, 0x661113069cec8515),
];

#[cfg(any(
    all(target_arch = "x86_64", windows),
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu")
))]
const EXR_DWA: &[(usize, u64)] = &[
    (14081, 0xfcfa3060e32ce54d),
    (46824, 0xe34c80a5b1013010),
    (13587, 0x2e57434b9b5eac8b),
    (42823, 0xa5cf03ca646bd90c),
    (7697, 0xed8e96fb165159f9),
    (7480, 0x6f0931cf216175c4),
    (16824, 0xc9f5c6554168a165),
    (36366, 0x6b159a4e3e132d5c),
    (50017, 0x5525a329d6aeaf77),
];
#[cfg(not(any(
    all(target_arch = "x86_64", windows),
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu")
)))]
const EXR_DWA: &[(usize, u64)] = &[];

fn is_dwa(c: Compression) -> bool {
    matches!(c, Compression::Dwaa | Compression::Dwab)
}

#[test]
fn writer_matches_exr_bytes() {
    let cases = gen::cases();
    let (mut common, mut dwa) = (EXR_COMMON.iter(), EXR_DWA.iter());
    let mut checked = 0;
    for c in &cases {
        let expected = if is_dwa(c.compression) {
            dwa.next()
        } else {
            common.next()
        };
        let Some(&(len, hash)) = expected else {
            continue;
        };
        let img = gen::image(c.kind, c.w, c.h);
        let file = rusty_exr::encode(&img, &gen::options(c)).unwrap();
        assert_eq!(
            (file.len(), gen::fnv(&file)),
            (len, hash),
            "{} {}x{} {:?} tiles {:?} level {:?}",
            c.kind,
            c.w,
            c.h,
            c.compression,
            c.tiles,
            c.dwa_level
        );
        checked += 1;
    }
    assert!(checked >= EXR_COMMON.len());
}
