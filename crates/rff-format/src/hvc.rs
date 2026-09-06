//! HEVC bitstream helpers for containers: the `hvcC`
//! (HEVCDecoderConfigurationRecord, ISO/IEC 14496-15 §8.3.3) parser and
//! builder, plus a minimal SPS reader.
//!
//! rff's packet contract for HEVC is the same as for H.264: **Annex-B**, with
//! parameter sets repeated before each keyframe and `Stream::extradata` left
//! empty. `hvc1`/`hev1` (MP4) and `V_MPEGH/ISO/HEVC` (Matroska) store
//! length-prefixed NALs plus an `hvcC` record, so demuxers normalise with
//! [`parse_hvcc`] and [`crate::avc::avcc_to_annexb`] (the length-prefix
//! conversion is codec-independent).

use crate::avc::AvcConfig;

/// Parse an `hvcC` record into the NAL length size and Annex-B VPS/SPS/PPS
/// headers, in the same shape the AVCC path produces.
pub fn parse_hvcc(hvcc: &[u8]) -> Option<AvcConfig> {
    if hvcc.len() < 23 || hvcc[0] != 1 {
        return None;
    }
    let nal_len = (hvcc[21] & 0x03) as usize + 1;
    let num_arrays = hvcc[22] as usize;
    let mut headers = Vec::new();
    let mut i = 23;
    for _ in 0..num_arrays {
        let nal_type = *hvcc.get(i)? & 0x3f;
        i += 1;
        let count = be16(hvcc, i)? as usize;
        i += 2;
        for _ in 0..count {
            let len = be16(hvcc, i)? as usize;
            i += 2;
            let nal = hvcc.get(i..i + len)?;
            // Only the parameter sets are worth prepending (32 VPS, 33 SPS,
            // 34 PPS); a record may also carry prefix SEI arrays.
            if matches!(nal_type, 32 | 33 | 34) {
                headers.extend_from_slice(&[0, 0, 0, 1]);
                headers.extend_from_slice(nal);
            }
            i += len;
        }
    }
    if headers.is_empty() {
        return None;
    }
    Some(AvcConfig { nal_len, headers_annexb: headers })
}

/// The SPS fields an `hvcC` record has to repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsInfo {
    pub profile_space: u8,
    pub tier_flag: bool,
    pub profile_idc: u8,
    pub profile_compatibility_flags: u32,
    pub constraint_flags: [u8; 6],
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub max_sub_layers: u8,
    pub temporal_id_nesting: bool,
    pub width: u32,
    pub height: u32,
}

/// Reads the fields above out of an HEVC SPS NAL (with or without its two-byte
/// NAL header, emulation prevention still in place).
pub fn sps_info(nal: &[u8]) -> Option<SpsInfo> {
    // Strip the NAL header when present (nal_unit_type 33).
    let payload = if !nal.is_empty() && (nal[0] >> 1) & 0x3f == 33 { nal.get(2..)? } else { nal };
    let rbsp = unescape(payload);
    let mut r = Bits::new(&rbsp);
    r.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as u8;
    let temporal_id_nesting = r.bit()?;
    // profile_tier_level(1, max_sub_layers_minus1)
    let profile_space = r.bits(2)? as u8;
    let tier_flag = r.bit()?;
    let profile_idc = r.bits(5)? as u8;
    let profile_compatibility_flags = r.bits(32)?;
    let mut constraint_flags = [0u8; 6];
    for c in constraint_flags.iter_mut() {
        *c = r.bits(8)? as u8;
    }
    let level_idc = r.bits(8)? as u8;
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_profile[i] = r.bit()?;
        sub_level[i] = r.bit()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1 as usize..8 {
            r.bits(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            r.bits(32)?;
            r.bits(32)?;
            r.bits(24)?;
        }
        if sub_level[i] {
            r.bits(8)?;
        }
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()? as u8;
    if chroma_format_idc == 3 {
        r.bit()?; // separate_colour_plane_flag
    }
    let width = r.ue()?;
    let height = r.ue()?;
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let bit_depth_luma = r.ue()? as u8 + 8;
    let bit_depth_chroma = r.ue()? as u8 + 8;
    let (sw, sh) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    Some(SpsInfo {
        profile_space,
        tier_flag,
        profile_idc,
        profile_compatibility_flags,
        constraint_flags,
        level_idc,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
        max_sub_layers: max_sub_layers_minus1 + 1,
        temporal_id_nesting,
        width: width.saturating_sub(sw * (cl + cr)),
        height: height.saturating_sub(sh * (ct + cb)),
    })
}

/// Cropped picture size from an HEVC SPS NAL (the TS demuxer's need).
pub fn sps_dimensions(nal: &[u8]) -> Option<(u32, u32)> {
    sps_info(nal).map(|s| (s.width, s.height))
}

/// Build an `hvcC` record from Annex-B parameter sets (4-byte NAL lengths).
pub fn build_hvcc_record(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let info = sps_info(sps)?;
    let mut out = Vec::with_capacity(64 + vps.len() + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push((info.profile_space << 6) | ((info.tier_flag as u8) << 5) | info.profile_idc);
    out.extend_from_slice(&info.profile_compatibility_flags.to_be_bytes());
    out.extend_from_slice(&info.constraint_flags);
    out.push(info.level_idc);
    out.extend_from_slice(&[0xf0, 0x00]); // reserved + min_spatial_segmentation_idc = 0
    out.push(0xfc); // reserved + parallelismType = 0
    out.push(0xfc | info.chroma_format_idc);
    out.push(0xf8 | (info.bit_depth_luma - 8));
    out.push(0xf8 | (info.bit_depth_chroma - 8));
    out.extend_from_slice(&[0, 0]); // avgFrameRate = 0 (unspecified)
    // constantFrameRate=0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne=3
    out.push((info.max_sub_layers << 3) | ((info.temporal_id_nesting as u8) << 2) | 3);
    let arrays: [(u8, &[u8]); 3] = [(32, vps), (33, sps), (34, pps)];
    let present = arrays.iter().filter(|(_, n)| !n.is_empty()).count();
    out.push(present as u8);
    for (t, nal) in arrays {
        if nal.is_empty() {
            continue;
        }
        out.push(0x80 | t); // array_completeness = 1
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Some(out)
}

/// Finds the first NAL of `nal_type` in an Annex-B stream (header included).
pub fn find_nal_annexb(data: &[u8], nal_type: u8) -> Option<&[u8]> {
    for nal in crate::avc::split_annexb(data) {
        if !nal.is_empty() && (nal[0] >> 1) & 0x3f == nal_type {
            return Some(nal);
        }
    }
    None
}

fn be16(d: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]))
}

fn unescape(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

struct Bits<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits { d, p: 0 }
    }
    fn bit(&mut self) -> Option<bool> {
        let b = *self.d.get(self.p / 8)?;
        let v = (b >> (7 - self.p % 8)) & 1;
        self.p += 1;
        Some(v == 1)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u32;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut lz = 0;
        while !self.bit()? {
            lz += 1;
            if lz > 31 {
                return None;
            }
        }
        if lz == 0 {
            return Some(0);
        }
        Some((1u32 << lz) - 1 + self.bits(lz)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real VPS/SPS/PPS of the JCT-VC `IPRED_A_docomo_2` stream
    /// (832x480 Main profile, level_idc 153), NAL headers included.
    const VPS: &[u8] = &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0xf0, 0x24,
    ];
    const SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x03, 0x00, 0x99, 0xa0, 0x06, 0x82, 0x01, 0xe1, 0xfe, 0x5f, 0x92, 0x46, 0xd9, 0x6c,
        0x80,
    ];
    const PPS: &[u8] = &[0x44, 0x01, 0xc1, 0x90, 0x95, 0x81, 0x12];

    #[test]
    fn reads_geometry_and_profile() {
        let i = sps_info(SPS).expect("sps");
        assert_eq!((i.width, i.height), (832, 480));
        assert_eq!(i.level_idc, 153);
        assert_eq!(i.max_sub_layers, 1);
        assert_eq!(i.chroma_format_idc, 1);
        assert_eq!(i.bit_depth_luma, 8);
        assert_eq!(i.bit_depth_chroma, 8);
        assert_eq!(i.profile_idc, 1);
        assert_eq!(sps_dimensions(SPS), Some((832, 480)));
        // and with the NAL header already stripped by the caller
        assert_eq!(sps_dimensions(&SPS[2..]), Some((832, 480)));
    }

    #[test]
    fn hvcc_roundtrip() {
        let (vps, pps) = (VPS, PPS);
        let rec = build_hvcc_record(vps, SPS, pps).expect("build");
        assert_eq!(rec[0], 1);
        assert_eq!(rec[21] & 3, 3, "4-byte NAL length");
        assert_eq!(rec[22], 3, "three arrays");
        let cfg = parse_hvcc(&rec).expect("parse");
        assert_eq!(cfg.nal_len, 4);
        // VPS, SPS and PPS each prefixed with a 4-byte start code.
        let mut want = Vec::new();
        for n in [vps, SPS, pps] {
            want.extend_from_slice(&[0, 0, 0, 1]);
            want.extend_from_slice(n);
        }
        assert_eq!(cfg.headers_annexb, want);
    }

    #[test]
    fn rejects_junk() {
        assert!(parse_hvcc(&[]).is_none());
        assert!(parse_hvcc(&[0u8; 40]).is_none());
        assert!(sps_info(&[0x42, 0x01]).is_none());
    }
}