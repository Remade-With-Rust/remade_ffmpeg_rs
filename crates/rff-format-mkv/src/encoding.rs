//! Matroska `ContentEncodings` on the demux side: the per-track compression
//! a writer applied to every frame (scope 1) and/or the CodecPrivate
//! (scope 2), which the demuxer undoes before anything else looks at the
//! bytes.
//!
//! Supported: header stripping (`ContentCompAlgo` 3, the stripped bytes are
//! put back in front) and zlib (`ContentCompAlgo` 0), chained in the order
//! the spec gives (highest `ContentEncodingOrder` is undone first). bzlib and
//! LZO1X compression, unknown encoding types, and encryption are refused:
//! a demuxer that handed out still-compressed or encrypted bytes as frames
//! would feed decoders garbage with no error anywhere.

/// A frame (or CodecPrivate) that inflates past this is treated as corrupt —
/// it bounds what a tiny zlib bomb can make the demuxer allocate.
const MAX_INFLATE: usize = 256 << 20;

/// One encoding to undo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Coding {
    /// Header stripping: these bytes were removed from the front.
    Strip(Vec<u8>),
    /// zlib (RFC 1950) compression.
    Zlib,
}

/// A track's encodings, each list in the order to undo them.
#[derive(Debug, Default)]
pub(crate) struct Codings {
    pub(crate) frames: Vec<Coding>,
    pub(crate) private: Vec<Coding>,
}

const ID_CONTENT_ENCODING: u32 = 0x6240;
const ID_CONTENT_ENCODING_ORDER: u32 = 0x5031;
const ID_CONTENT_ENCODING_SCOPE: u32 = 0x5032;
const ID_CONTENT_ENCODING_TYPE: u32 = 0x5033;
const ID_CONTENT_COMPRESSION: u32 = 0x5034;
const ID_CONTENT_COMP_ALGO: u32 = 0x4254;
const ID_CONTENT_COMP_SETTINGS: u32 = 0x4255;
const ID_CONTENT_ENCRYPTION: u32 = 0x5035;

/// Parse a `ContentEncodings` body. `Err` names why the track can't be read
/// (encryption, or a compression we don't implement).
pub(crate) fn parse(body: &[u8]) -> Result<Codings, String> {
    // (order, scope, coding)
    let mut list: Vec<(u64, u64, Coding)> = Vec::new();
    for (id, enc) in crate::children(body) {
        if id != ID_CONTENT_ENCODING {
            continue;
        }
        let (mut order, mut scope, mut kind) = (0u64, 1u64, 0u64);
        let (mut algo, mut settings): (u64, &[u8]) = (0, &[]);
        let mut encrypted = false;
        for (cid, b) in crate::children(enc) {
            match cid {
                ID_CONTENT_ENCODING_ORDER => order = crate::uint_of(b),
                ID_CONTENT_ENCODING_SCOPE => scope = crate::uint_of(b),
                ID_CONTENT_ENCODING_TYPE => kind = crate::uint_of(b),
                ID_CONTENT_ENCRYPTION => encrypted = true,
                ID_CONTENT_COMPRESSION => {
                    for (pid, p) in crate::children(b) {
                        match pid {
                            ID_CONTENT_COMP_ALGO => algo = crate::uint_of(p),
                            ID_CONTENT_COMP_SETTINGS => settings = p,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if encrypted || kind == 1 {
            return Err(
                "is encrypted (ContentEncryption); encrypted Matroska tracks are \
                        not supported"
                    .into(),
            );
        }
        if kind != 0 {
            return Err(format!(
                "uses ContentEncodingType {kind}, which is not defined"
            ));
        }
        let coding = match algo {
            0 => Coding::Zlib,
            3 if settings.is_empty() => continue, // strips nothing
            3 => Coding::Strip(settings.to_vec()),
            1 => return Err("is bzlib-compressed (ContentCompAlgo 1), not supported".into()),
            2 => return Err("is LZO1X-compressed (ContentCompAlgo 2), not supported".into()),
            n => return Err(format!("uses unknown ContentCompAlgo {n}")),
        };
        list.push((order, scope, coding));
    }
    // The highest ContentEncodingOrder was applied last, so it comes off first.
    list.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = Codings::default();
    for (_, scope, coding) in list {
        if scope & 2 != 0 {
            out.private.push(coding.clone());
        }
        if scope & 1 != 0 {
            out.frames.push(coding);
        }
    }
    Ok(out)
}

/// Undo `codings` on `data`. `None` when a zlib stream is corrupt (or
/// inflates past [`MAX_INFLATE`]).
pub(crate) fn decode(codings: &[Coding], data: &[u8]) -> Option<Vec<u8>> {
    let mut cur = data.to_vec();
    for c in codings {
        cur = match c {
            Coding::Strip(head) => {
                let mut v = Vec::with_capacity(head.len() + cur.len());
                v.extend_from_slice(head);
                v.extend_from_slice(&cur);
                v
            }
            Coding::Zlib => {
                miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&cur, MAX_INFLATE).ok()?
            }
        };
    }
    Some(cur)
}
