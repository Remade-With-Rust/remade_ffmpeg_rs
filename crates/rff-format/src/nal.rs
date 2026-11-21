//! How an H.264 / HEVC packet frames its NAL units, for the muxers that store
//! length-prefixed NALs (MP4, Matroska, DASH).
//!
//! rff's packet contract is Annex-B (start codes), but a packet can also
//! arrive length-prefixed, as the container stored it: from a demuxer in
//! [`Demuxer::set_raw_samples`](crate::Demuxer::set_raw_samples) mode, from
//! FLV, or from an editor that keeps container samples. The two forms must
//! never be confused. A 4-byte NAL length of 256 to 511 is `00 00 01 xx`,
//! which reads as a start code, and a length-prefixed sample with no such
//! length has no "start code" at all, so splitting it as Annex-B loses part
//! or all of it.
//!
//! [`NalSampleWriter`] decides each track's framing from its first packet and
//! the stream's extradata. A length-prefixed form is only possible when the
//! extradata is an `avcC` / `hvcC` record, whose `lengthSizeMinusOne` gives
//! the length size. A packet is taken as length-prefixed when its lengths
//! account for every byte exactly and each NAL is well formed (a valid header,
//! no start code inside), and as Annex-B when it starts with a start code.
//! Anything else is an error, as is a later packet that does not fit the
//! track's framing, so no byte is ever dropped silently.

use crate::avc::split_annexb;

/// How a packet's NAL units are delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalFraming {
    /// Each NAL is preceded by a `00 00 01` / `00 00 00 01` start code
    /// (rff's packet contract).
    AnnexB,
    /// Each NAL is preceded by its big-endian length in this many bytes
    /// (1, 2 or 4): the AVCC / HVCC form MP4 and Matroska store.
    LengthPrefixed(usize),
}

/// The NAL length size (1..=4) an `avcC` (`hevc == false`) or `hvcC`
/// (`hevc == true`) record declares, or `None` when `extradata` is not such a
/// record (Annex-B parameter sets, or nothing).
pub fn record_nal_length_size(extradata: &[u8], hevc: bool) -> Option<usize> {
    // Both records start with configurationVersion = 1; Annex-B starts with 0.
    if extradata.first() != Some(&1) {
        return None;
    }
    let cfg = if hevc {
        crate::hvc::parse_hvcc(extradata)?
    } else {
        crate::avc::parse_avcc(extradata)?
    };
    Some(cfg.nal_len)
}

/// A copy of an `avcC` / `hvcC` record that declares 4-byte NAL lengths
/// (`lengthSizeMinusOne` = 3), everything else verbatim. `None` when
/// `extradata` is not such a record.
pub fn record_with_4byte_lengths(extradata: &[u8], hevc: bool) -> Option<Vec<u8>> {
    record_nal_length_size(extradata, hevc)?;
    let mut record = extradata.to_vec();
    let at = if hevc { 21 } else { 4 };
    record[at] |= 0x03;
    Some(record)
}

/// The NAL units of a length-prefixed sample, or `None` when its lengths do
/// not account for every byte exactly (a length runs past the end, or bytes
/// are left over that cannot hold a length).
pub fn split_length_prefixed(data: &[u8], nal_len: usize) -> Option<Vec<&[u8]>> {
    if !(1..=4).contains(&nal_len) {
        return None;
    }
    let mut nals = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (len, tail) = (rest.get(..nal_len)?, &rest[nal_len..]);
        let len = len.iter().fold(0usize, |n, &b| (n << 8) | b as usize);
        nals.push(tail.get(..len)?);
        rest = &tail[len..];
    }
    Some(nals)
}

/// Whether `data` begins with an Annex-B start code.
pub fn starts_with_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// A NAL unit as it can appear in a conformant stream: a header with the
/// forbidden bit clear (HEVC: and a non-zero `nuh_temporal_id_plus1`; H.264:
/// a non-zero type) and no start code inside, which emulation prevention
/// rules out.
fn nal_well_formed(nal: &[u8], hevc: bool) -> bool {
    let header_ok = if hevc {
        nal.len() >= 2 && nal[0] & 0x80 == 0 && nal[1] & 0x07 != 0
    } else {
        !nal.is_empty() && nal[0] & 0x80 == 0 && nal[0] & 0x1F != 0
    };
    header_ok && !nal.windows(3).any(|w| w == [0, 0, 1])
}

/// Whether `data` reads as a length-prefixed sample with `nal_len`-byte
/// lengths: the lengths cover every byte exactly and each NAL is well formed.
pub fn is_length_prefixed(data: &[u8], nal_len: usize, hevc: bool) -> bool {
    match split_length_prefixed(data, nal_len) {
        Some(nals) => !nals.is_empty() && nals.iter().all(|n| nal_well_formed(n, hevc)),
        None => false,
    }
}

/// The framing of one packet: length-prefixed when the stream's record
/// declares a length size and the packet reads exactly as such, else Annex-B
/// when it starts with a start code (or is empty). `None` when it is neither.
pub fn detect_framing(
    data: &[u8],
    record_nal_len: Option<usize>,
    hevc: bool,
) -> Option<NalFraming> {
    if let Some(n) = record_nal_len {
        if is_length_prefixed(data, n, hevc) {
            return Some(NalFraming::LengthPrefixed(n));
        }
    }
    (data.is_empty() || starts_with_start_code(data)).then_some(NalFraming::AnnexB)
}

/// The in-band parameter sets an Annex-B packet carried, which the muxer
/// lifts out of the sample into its `avcC` / `hvcC` (H.264 has no VPS).
#[derive(Debug, Clone, Copy, Default)]
pub struct ParamSets<'a> {
    pub vps: Option<&'a [u8]>,
    pub sps: Option<&'a [u8]>,
    pub pps: Option<&'a [u8]>,
}

/// What [`NalSampleWriter::write`] made of a packet.
#[derive(Debug, Clone, Copy)]
pub enum Stored<'a> {
    /// The packet is already stored as it is (4-byte length-prefixed): keep
    /// its bytes unchanged.
    Verbatim,
    /// The stored sample was appended to the output buffer. `params` are the
    /// parameter sets an Annex-B packet carried, not written to the sample.
    Converted { params: ParamSets<'a> },
}

/// One H.264 / HEVC track's conversion from packets to stored samples
/// (4-byte length-prefixed NALs). See the module docs.
#[derive(Debug, Clone)]
pub struct NalSampleWriter {
    hevc: bool,
    record_nal_len: Option<usize>,
    framing: Option<NalFraming>,
}

impl NalSampleWriter {
    /// A writer for an H.264 (`hevc == false`) or HEVC track with this
    /// extradata.
    pub fn new(extradata: &[u8], hevc: bool) -> NalSampleWriter {
        NalSampleWriter {
            hevc,
            record_nal_len: record_nal_length_size(extradata, hevc),
            framing: None,
        }
    }

    /// The track's framing, once its first packet has settled it.
    pub fn framing(&self) -> Option<NalFraming> {
        self.framing
    }

    /// Store one packet. Length-prefixed packets keep every byte, their
    /// parameter sets included: with 4-byte lengths the packet is the sample
    /// ([`Stored::Verbatim`]); otherwise each NAL is re-prefixed with a 4-byte
    /// length into `out`. Annex-B packets are converted into `out` as before,
    /// their parameter sets returned rather than written. The error says why
    /// a packet could not be stored without losing bytes.
    pub fn write<'a>(&mut self, data: &'a [u8], out: &mut Vec<u8>) -> Result<Stored<'a>, String> {
        let framing = match self.framing {
            Some(f) => f,
            // An empty packet stores nothing either way and settles nothing.
            None if data.is_empty() => return Ok(Stored::Verbatim),
            None => {
                let f =
                    detect_framing(data, self.record_nal_len, self.hevc).ok_or_else(
                        || match self.record_nal_len {
                            Some(n) => format!(
                                "the first packet is neither Annex-B (no leading start code) nor \
                             {n}-byte length-prefixed as the extradata record declares"
                            ),
                            None => "the first packet does not start with an Annex-B start code, \
                                 and the extradata is no avcC/hvcC record that could make it \
                                 length-prefixed"
                                .to_string(),
                        },
                    )?;
                // 1-byte NALs each behind `00 00 00 01` read the same both
                // ways and are stored as they are either way: leave the
                // framing to a packet that tells them apart.
                if f == NalFraming::LengthPrefixed(4)
                    && starts_with_start_code(data)
                    && split_length_prefixed(data, 4).as_deref() == Some(&split_annexb(data)[..])
                {
                    return Ok(Stored::Verbatim);
                }
                self.framing = Some(f);
                f
            }
        };
        match framing {
            NalFraming::LengthPrefixed(n) => {
                let nals = split_length_prefixed(data, n).ok_or_else(|| {
                    format!(
                        "a {}-byte packet is not consistently {n}-byte length-prefixed as the \
                         track's earlier packets are (a NAL length runs past the end or bytes \
                         are left over)",
                        data.len()
                    )
                })?;
                if n == 4 {
                    return Ok(Stored::Verbatim);
                }
                out.reserve(data.len() + nals.len() * (4 - n));
                for nal in nals {
                    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    out.extend_from_slice(nal);
                }
                Ok(Stored::Converted {
                    params: ParamSets::default(),
                })
            }
            NalFraming::AnnexB => {
                if !data.is_empty() && !starts_with_start_code(data) {
                    return Err(format!(
                        "a {}-byte packet does not start with an Annex-B start code, as the \
                         track's earlier packets do (a length-prefixed packet in an Annex-B \
                         track?); its leading bytes would be lost",
                        data.len()
                    ));
                }
                let mut params = ParamSets::default();
                for nal in split_annexb(data) {
                    let kind = nal
                        .first()
                        .map(|&b| if self.hevc { (b >> 1) & 0x3F } else { b & 0x1F });
                    match (self.hevc, kind) {
                        (false, Some(7)) | (true, Some(33)) => params.sps = Some(nal),
                        (false, Some(8)) | (true, Some(34)) => params.pps = Some(nal),
                        (true, Some(32)) => params.vps = Some(nal),
                        _ => {
                            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                            out.extend_from_slice(nal);
                        }
                    }
                }
                Ok(Stored::Converted { params })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(nals: &[&[u8]], n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for nal in nals {
            v.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - n..]);
            v.extend_from_slice(nal);
        }
        v
    }

    fn slice(header: &[u8], len: usize) -> Vec<u8> {
        let mut v = header.to_vec();
        v.extend((0..len - header.len()).map(|i| (i % 250) as u8 + 3));
        v
    }

    const AVCC: &[u8] = &[
        1, 100, 0, 31, 0xFF, 0xE1, 0, 3, 0x67, 0xAA, 0xBB, 1, 0, 2, 0x68, 0xCC,
    ];

    #[test]
    fn record_length_size() {
        assert_eq!(record_nal_length_size(AVCC, false), Some(4));
        let mut two = AVCC.to_vec();
        two[4] = 0xFD;
        assert_eq!(record_nal_length_size(&two, false), Some(2));
        assert_eq!(
            record_with_4byte_lengths(&two, false).as_deref(),
            Some(AVCC)
        );
        // Annex-B extradata is no record.
        assert_eq!(
            record_nal_length_size(&[0, 0, 0, 1, 0x67, 0xAA], false),
            None
        );
        assert_eq!(record_nal_length_size(&[], true), None);
    }

    #[test]
    fn a_length_that_reads_as_a_start_code_is_length_prefixed() {
        // 300-byte slice: the 4-byte length is 00 00 01 2C.
        let s = lp(&[&slice(&[0x65], 300)], 4);
        assert_eq!(&s[..4], &[0, 0, 1, 0x2C]);
        assert_eq!(
            detect_framing(&s, Some(4), false),
            Some(NalFraming::LengthPrefixed(4))
        );
        // A 1-byte NAL first (end of sequence): 00 00 00 01.
        let s = lp(&[&[0x0A], &slice(&[0x41], 40)], 4);
        assert_eq!(&s[..4], &[0, 0, 0, 1]);
        assert_eq!(
            detect_framing(&s, Some(4), false),
            Some(NalFraming::LengthPrefixed(4))
        );
        // Without a record it can only be Annex-B.
        let s = lp(&[&slice(&[0x65], 300)], 4);
        assert_eq!(detect_framing(&s, None, false), Some(NalFraming::AnnexB));
    }

    #[test]
    fn annexb_stays_annexb() {
        let mut a = vec![0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xCC, 0, 0, 1];
        a.extend(slice(&[0x65], 300));
        assert_eq!(detect_framing(&a, Some(4), false), Some(NalFraming::AnnexB));
        assert_eq!(detect_framing(&a, None, false), Some(NalFraming::AnnexB));
        assert_eq!(
            detect_framing(&[], Some(4), false),
            Some(NalFraming::AnnexB)
        );
        // A 2-byte record never matches a start-code-led packet (length 0).
        assert_eq!(detect_framing(&a, Some(2), false), Some(NalFraming::AnnexB));
    }

    #[test]
    fn neither_form_is_refused() {
        let s = [0x12, 0x34, 0x56, 0x78, 0x9A];
        assert_eq!(detect_framing(&s, Some(4), false), None);
        assert_eq!(detect_framing(&s, None, true), None);
        let mut w = NalSampleWriter::new(AVCC, false);
        assert!(w.write(&s, &mut Vec::new()).is_err());
    }

    #[test]
    fn writer_keeps_length_prefixed_bytes() {
        let mut w = NalSampleWriter::new(AVCC, false);
        let a = lp(
            &[&[0x67, 0xAA, 0xBB], &[0x68, 0xCC], &slice(&[0x65], 300)],
            4,
        );
        let mut out = Vec::new();
        assert!(matches!(w.write(&a, &mut out), Ok(Stored::Verbatim)));
        assert!(out.is_empty());
        assert_eq!(w.framing(), Some(NalFraming::LengthPrefixed(4)));
        // A later packet whose lengths do not add up is an error.
        let mut bad = lp(&[&slice(&[0x41], 50)], 4);
        bad.push(0x99);
        assert!(w.write(&bad, &mut out).is_err());
        // And so is an Annex-B packet in a length-prefixed track.
        assert!(w.write(&[0, 0, 0, 1, 0x41, 0x9A, 0xBC], &mut out).is_err());
    }

    #[test]
    fn writer_reprefixes_2byte_lengths() {
        let mut rec = AVCC.to_vec();
        rec[4] = 0xFD; // 2-byte lengths
        let mut w = NalSampleWriter::new(&rec, false);
        let nals: [&[u8]; 2] = [&[0x06, 5, 0x80], &slice(&[0x65], 300)];
        let mut out = Vec::new();
        assert!(matches!(
            w.write(&lp(&nals, 2), &mut out),
            Ok(Stored::Converted { .. })
        ));
        assert_eq!(out, lp(&nals, 4));
    }

    #[test]
    fn writer_converts_annexb_and_lifts_parameter_sets() {
        let mut w = NalSampleWriter::new(AVCC, false);
        let a = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xCC, 0, 0, 0, 1, 0x65, 1, 2,
        ];
        let mut out = Vec::new();
        let Ok(Stored::Converted { params }) = w.write(&a, &mut out) else {
            panic!("converted")
        };
        assert_eq!(params.sps, Some(&[0x67, 0xAA][..]));
        assert_eq!(params.pps, Some(&[0x68, 0xCC][..]));
        assert_eq!(out, [0, 0, 0, 3, 0x65, 1, 2]);
        // A packet that does not start with a start code would lose bytes.
        assert!(w.write(&[0, 0, 0, 3, 0x41, 1, 2], &mut out).is_err());
    }

    #[test]
    fn an_ambiguous_first_packet_leaves_the_framing_open() {
        let mut w = NalSampleWriter::new(AVCC, false);
        // AUD-less 1-byte NALs: as Annex-B or as 4-byte lengths, the same.
        let both = [0, 0, 0, 1, 0x0A, 0, 0, 0, 1, 0x0B];
        assert!(matches!(
            w.write(&both, &mut Vec::new()),
            Ok(Stored::Verbatim)
        ));
        assert!(matches!(
            w.write(&[], &mut Vec::new()),
            Ok(Stored::Verbatim)
        ));
        assert_eq!(w.framing(), None);
        let a = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xCC, 0, 0, 0, 1, 0x65, 1, 2,
        ];
        assert!(matches!(
            w.write(&a, &mut Vec::new()),
            Ok(Stored::Converted { .. })
        ));
        assert_eq!(w.framing(), Some(NalFraming::AnnexB));
    }

    #[test]
    fn hevc_length_prefixed() {
        // An HEVC IDR slice (type 19: 0x26 0x01) 400 bytes long: 00 00 01 90.
        let s = lp(&[&slice(&[0x26, 0x01], 400)], 4);
        assert_eq!(&s[..3], &[0, 0, 1]);
        let mut w = NalSampleWriter {
            hevc: true,
            record_nal_len: Some(4),
            framing: None,
        };
        assert!(matches!(w.write(&s, &mut Vec::new()), Ok(Stored::Verbatim)));
        // A zero temporal id is no HEVC NAL header.
        assert!(!is_length_prefixed(&lp(&[&[0x26, 0x00, 7]], 4), 4, true));
    }
}
