//! H.264 bitstream helpers shared by demuxers (FFmpeg's `h264_mp4toannexb`
//! bitstream filter plus a minimal SPS reader).
//!
//! rff's packet contract for H.264 is **Annex-B**: every NAL prefixed with a
//! `00 00 00 01` start code, SPS/PPS repeated before each keyframe, and
//! `Stream::extradata` left empty. Containers that store AVCC (length-prefixed
//! NALs + an `avcC` config record) — MP4 and Matroska — use [`parse_avcc`] +
//! [`avcc_to_annexb`] to normalise, so consumers see one format regardless of
//! the container. [`sps_dimensions`] recovers coded width/height for containers
//! whose headers carry none (MPEG-TS).

/// Per-track H.264 config for AVCC→Annex-B conversion, parsed from an `avcC`
/// (AVCDecoderConfigurationRecord) box.
pub struct AvcConfig {
    /// Bytes in each NAL length prefix (1, 2 or 4).
    pub nal_len: usize,
    /// The SPS + PPS NALs, already start-code prefixed, ready to prepend to
    /// keyframe packets.
    pub headers_annexb: Vec<u8>,
}

/// Parse an `avcC` box into the NAL length size and Annex-B SPS/PPS headers.
pub fn parse_avcc(avcc: &[u8]) -> Option<AvcConfig> {
    if avcc.len() < 6 {
        return None;
    }
    let nal_len = (avcc[4] & 0x03) as usize + 1;
    let mut headers = Vec::new();
    let mut i = 5;
    let num_sps = avcc[i] & 0x1F;
    i += 1;
    for _ in 0..num_sps {
        let len = be16(avcc, i)? as usize;
        i += 2;
        let nal = avcc.get(i..i + len)?;
        headers.extend_from_slice(&[0, 0, 0, 1]);
        headers.extend_from_slice(nal);
        i += len;
    }
    let num_pps = *avcc.get(i)?;
    i += 1;
    for _ in 0..num_pps {
        let len = be16(avcc, i)? as usize;
        i += 2;
        let nal = avcc.get(i..i + len)?;
        headers.extend_from_slice(&[0, 0, 0, 1]);
        headers.extend_from_slice(nal);
        i += len;
    }
    Some(AvcConfig {
        nal_len,
        headers_annexb: headers,
    })
}

/// Convert one AVCC sample (a series of `nal_len`-byte length + NAL) to Annex-B
/// (each NAL prefixed with a `00 00 00 01` start code), appending to `out`.
pub fn avcc_to_annexb(sample: &[u8], nal_len: usize, out: &mut Vec<u8>) {
    let mut i = 0;
    while i + nal_len <= sample.len() {
        let mut len = 0usize;
        for _ in 0..nal_len {
            len = (len << 8) | sample[i] as usize;
            i += 1;
        }
        if i + len > sample.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[i..i + len]);
        i += len;
    }
}

/// Find the first SPS NAL (type 7) in an Annex-B stream. Returns the NAL
/// including its header byte, without the start code.
pub fn find_sps_annexb(data: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    let mut nal_start: Option<usize> = None;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = nal_start {
                // End of the previous NAL (trim the 0 of a 4-byte start code).
                let end = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
                if data[s] & 0x1F == 7 {
                    return Some(&data[s..end]);
                }
            }
            nal_start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let s = nal_start?;
    (*data.get(s)? & 0x1F == 7).then(|| &data[s..])
}

/// Decode coded width/height from an SPS NAL (header byte included), applying
/// the frame cropping rectangle. Handles High-profile chroma/bit-depth fields
/// and scaling matrices. Returns `None` on any malformed or truncated field.
pub fn sps_dimensions(nal: &[u8]) -> Option<(u32, u32)> {
    if nal.first()? & 0x1F != 7 {
        return None;
    }
    // RBSP: strip emulation-prevention bytes (00 00 03 → 00 00).
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0u32;
    for &b in &nal[1..] {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }

    let mut r = Bits::new(&rbsp);
    let profile_idc = r.bits(8)?;
    r.bits(16)?; // constraint flags + reserved + level_idc
    r.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1; // 4:2:0 unless the profile spells it out
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()? == 1;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            // seq_scaling_matrix_present: skip each present scaling list.
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            let n = r.ue()?;
            for _ in 0..n {
                r.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag

    let width_mbs = r.ue()? + 1;
    let height_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag

    let mut width = width_mbs.checked_mul(16)?;
    let mut height = (2 - frame_mbs_only)
        .checked_mul(height_map_units)?
        .checked_mul(16)?;
    if r.bit()? == 1 {
        // frame_cropping: units depend on the chroma sampling (Table 6-1).
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let chroma_array_type = if separate_colour_plane { 0 } else { chroma_format_idc };
        let (sub_w, sub_h) = match chroma_array_type {
            1 => (2, 2), // 4:2:0
            2 => (2, 1), // 4:2:2
            _ => (1, 1), // monochrome / 4:4:4
        };
        let crop_y = sub_h * (2 - frame_mbs_only);
        width = width.checked_sub((left + right).checked_mul(sub_w)?)?;
        height = height.checked_sub((top + bottom).checked_mul(crop_y)?)?;
    }
    (width > 0 && height > 0).then_some((width, height))
}

/// Split an Annex-B bitstream into NAL units (start codes removed). The
/// mux-side inverse of [`avcc_to_annexb`], shared by every container that
/// stores AVCC (MP4, Matroska).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut nal_start: Option<usize> = None;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = nal_start {
                // A leading zero before this start code belongs to a 4-byte code.
                let end = if i > s && data[i - 1] == 0 { i - 1 } else { i };
                if end > s {
                    nals.push(&data[s..end]);
                }
            }
            i += 3;
            nal_start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = nal_start {
        if s < data.len() {
            nals.push(&data[s..]);
        }
    }
    nals
}

/// Build an AVCDecoderConfigurationRecord (the `avcC` payload, no box header)
/// from the SPS and PPS NALs. MP4 wraps it in an `avcC` box; Matroska stores it
/// raw as CodecPrivate.
pub fn build_avcc_record(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(1); // configurationVersion
    b.push(*sps.get(1).unwrap_or(&0x42)); // AVCProfileIndication
    b.push(*sps.get(2).unwrap_or(&0)); // profile_compatibility
    b.push(*sps.get(3).unwrap_or(&30)); // AVCLevelIndication
    b.push(0xFF); // 6 bits reserved + lengthSizeMinusOne = 3 (4-byte lengths)
    b.push(0xE1); // 3 bits reserved + numOfSPS = 1
    b.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    b.extend_from_slice(sps);
    b.push(1); // numOfPPS
    b.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    b.extend_from_slice(pps);
    b
}

/// Consume a `scaling_list()` of `size` entries (we only need the bits gone).
fn skip_scaling_list(r: &mut Bits, size: u32) -> Option<()> {
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            next = (next + r.se()? + 256) % 256;
        }
        if next == 0 {
            break; // remaining entries repeat the last — no more bits follow
        }
    }
    Some(())
}

/// MSB-first bit reader over an RBSP.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize, // in bits
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Bits<'a> {
        Bits { d, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let b = *self.d.get(self.pos / 8)?;
        let v = (b >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb (`ue(v)`).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + if zeros > 0 { self.bits(zeros)? } else { 0 })
    }

    /// Signed Exp-Golomb (`se(v)`).
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

fn be16(d: &[u8], i: usize) -> Option<u16> {
    Some(((*d.get(i)? as u16) << 8) | *d.get(i + 1)? as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_roundtrip_to_annexb() {
        // avcC: ver 1, profile/compat/level, nal_len=4, 1 SPS (3 bytes), 1 PPS (2 bytes).
        let avcc = [
            1, 100, 0, 31, 0xFF, 0xE1, 0, 3, 0x67, 0xAA, 0xBB, 1, 0, 2, 0x68, 0xCC,
        ];
        let cfg = parse_avcc(&avcc).unwrap();
        assert_eq!(cfg.nal_len, 4);
        assert_eq!(
            cfg.headers_annexb,
            &[0, 0, 0, 1, 0x67, 0xAA, 0xBB, 0, 0, 0, 1, 0x68, 0xCC]
        );

        let sample = [0, 0, 0, 2, 0x41, 0x9A, 0, 0, 0, 1, 0x06];
        let mut out = Vec::new();
        avcc_to_annexb(&sample, 4, &mut out);
        assert_eq!(out, &[0, 0, 0, 1, 0x41, 0x9A, 0, 0, 0, 1, 0x06]);
    }

    #[test]
    fn finds_sps_between_start_codes() {
        let data = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68];
        assert_eq!(find_sps_annexb(&data), Some(&[0x67u8, 0xAA][..]));
        // SPS as the last NAL.
        let data = [0, 0, 1, 0x67, 0xAA, 0xBB];
        assert_eq!(find_sps_annexb(&data), Some(&[0x67u8, 0xAA, 0xBB][..]));
        assert_eq!(find_sps_annexb(&[0, 0, 1, 0x68, 0x00]), None);
    }

    #[test]
    fn sps_dimensions_x264_640x360() {
        // x264 High-profile SPS for 640x360 (360 = 368 coded − 8 crop).
        let sps = [
            0x67, 0x64, 0x00, 0x1E, 0xAC, 0xD9, 0x40, 0xA0, 0x2F, 0xF9, 0x70, 0x11, 0x00, 0x00,
            0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x30, 0x0F, 0x16, 0x2D, 0x96,
        ];
        assert_eq!(sps_dimensions(&sps), Some((640, 360)));
    }

    #[test]
    fn sps_dimensions_baseline_176x144() {
        // Baseline QCIF SPS (no cropping, no High-profile fields).
        let sps = [0x67, 0x42, 0xC0, 0x0D, 0x8C, 0x8D, 0x41, 0x62, 0x64, 0x0A, 0x60, 0x80];
        assert_eq!(sps_dimensions(&sps), Some((176, 144)));
    }
}
