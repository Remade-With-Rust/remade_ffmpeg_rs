//! AV1 bitstream helpers shared by containers (MP4 `av1C`, Matroska
//! CodecPrivate): locate the sequence-header OBU and build the
//! AV1CodecConfigurationRecord around it.

/// Read an unsigned LEB128 value; returns `(value, bytes_used)`.
fn read_leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let byte = *data.get(i)?;
        v |= ((byte & 0x7f) as u64) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Find the AV1 sequence-header OBU (type 1) in a temporal unit; return its bytes.
pub fn find_seq_header_obu(data: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i < data.len() {
        let start = i;
        let header = data[i];
        i += 1;
        let obu_type = (header >> 3) & 0x0f;
        if (header >> 2) & 1 == 1 {
            i += 1; // extension header
        }
        let len = if (header >> 1) & 1 == 1 {
            let (l, used) = read_leb128(data.get(i..)?)?;
            i += used;
            l as usize
        } else {
            data.len() - i
        };
        if i + len > data.len() {
            return None;
        }
        i += len;
        if obu_type == 1 {
            return Some(&data[start..i]);
        }
    }
    None
}

/// Best-effort AV1CodecConfigurationRecord (the `av1C` payload, no box header)
/// with the sequence header embedded as configOBUs. Fixed fields assume 8-bit
/// 4:2:0 (the common case); compliant decoders read the embedded sequence
/// header regardless.
pub fn config_record(sample: &[u8]) -> Option<Vec<u8>> {
    let seq = find_seq_header_obu(sample)?;
    // marker(1)|version(7)=1, profile 0 + level 0, then 8-bit/4:2:0 flags.
    let mut b = vec![0x81u8, 0x00, 0x0C, 0x00];
    b.extend_from_slice(seq);
    Some(b)
}

/// The colour range a sequence header signals (`color_config.color_range`),
/// from the first sequence-header OBU in `data` (a temporal unit, or the
/// configOBUs of an `av1C`). `None` if there is none or it is malformed.
pub fn color_range(data: &[u8]) -> Option<rff_core::ColorRange> {
    let obu = find_seq_header_obu(data)?;
    let header = *obu.first()?;
    let mut i = 1 + usize::from((header >> 2) & 1 == 1);
    if (header >> 1) & 1 == 1 {
        i += read_leb128(obu.get(i..)?)?.1;
    }
    let full = seq_header_full_range(obu.get(i..)?)?;
    Some(if full {
        rff_core::ColorRange::Full
    } else {
        rff_core::ColorRange::Limited
    })
}

/// MSB-first reader for [`seq_header_full_range`].
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let b = *self.d.get(self.pos / 8)?;
            v = (v << 1) | u32::from((b >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    /// `uvlc()` (AV1 §4.10.3).
    fn uvlc(&mut self) -> Option<()> {
        let mut zeros = 0;
        while self.f(1)? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Some(());
            }
        }
        self.f(zeros)?;
        Some(())
    }
}

/// Walk `sequence_header_obu()` (AV1 §5.5) to `color_config()` and return
/// its `color_range`.
fn seq_header_full_range(payload: &[u8]) -> Option<bool> {
    let mut r = Bits { d: payload, pos: 0 };
    let profile = r.f(3)?;
    r.f(1)?; // still_picture
    let reduced = r.f(1)? == 1;
    if reduced {
        r.f(5)?; // seq_level_idx[0]
    } else {
        let mut decoder_model_info = false;
        let mut buffer_delay_len = 0;
        if r.f(1)? == 1 {
            // timing_info()
            r.f(32)?; // num_units_in_display_tick
            r.f(32)?; // time_scale
            if r.f(1)? == 1 {
                r.uvlc()?; // num_ticks_per_picture_minus_1
            }
            decoder_model_info = r.f(1)? == 1;
            if decoder_model_info {
                buffer_delay_len = r.f(5)? + 1;
                r.f(32)?; // num_units_in_decoding_tick
                r.f(5)?; // buffer_removal_time_length_minus_1
                r.f(5)?; // frame_presentation_time_length_minus_1
            }
        }
        let initial_display_delay = r.f(1)? == 1;
        for _ in 0..=r.f(5)? {
            r.f(12)?; // operating_point_idc
            if r.f(5)? > 7 {
                r.f(1)?; // seq_tier
            }
            if decoder_model_info && r.f(1)? == 1 {
                r.f(buffer_delay_len)?; // decoder_buffer_delay
                r.f(buffer_delay_len)?; // encoder_buffer_delay
                r.f(1)?; // low_delay_mode_flag
            }
            if initial_display_delay && r.f(1)? == 1 {
                r.f(4)?; // initial_display_delay_minus_1
            }
        }
    }
    let width_bits = r.f(4)? + 1;
    let height_bits = r.f(4)? + 1;
    r.f(width_bits)?;
    r.f(height_bits)?;
    if !reduced && r.f(1)? == 1 {
        r.f(4)?; // delta_frame_id_length_minus_2
        r.f(3)?; // additional_frame_id_length_minus_1
    }
    r.f(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        r.f(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = r.f(1)? == 1;
        if order_hint {
            r.f(2)?; // enable_jnt_comp, enable_ref_frame_mvs
        }
        let force_screen_content = if r.f(1)? == 1 { 2 } else { r.f(1)? };
        if force_screen_content > 0 && r.f(1)? == 0 {
            r.f(1)?; // seq_force_integer_mv
        }
        if order_hint {
            r.f(3)?; // order_hint_bits_minus_1
        }
    }
    r.f(3)?; // enable_superres, enable_cdef, enable_restoration
             // color_config()
    let high_bitdepth = r.f(1)? == 1;
    if profile == 2 && high_bitdepth {
        r.f(1)?; // twelve_bit
    }
    let mono = profile != 1 && r.f(1)? == 1;
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if r.f(1)? == 1 {
        cp = r.f(8)?;
        tc = r.f(8)?;
        mc = r.f(8)?;
    }
    if !mono && cp == 1 && tc == 13 && mc == 0 {
        return Some(true); // sRGB: full range, not coded
    }
    Some(r.f(1)? == 1)
}

#[cfg(test)]
mod color_range_tests {
    use super::*;
    use rff_core::ColorRange;

    /// A reduced still-picture sequence header OBU, profile 0, 8-bit,
    /// no colour description, with `color_range` = `full`.
    fn reduced_seq_header(full: bool) -> Vec<u8> {
        // seq_profile 000, still 1, reduced 1, level 00000, wbits-1 0000 (1 bit),
        // hbits-1 0000, max_w-1 0, max_h-1 0, 128sb/filter_intra/edge 000,
        // superres/cdef/restoration 000, high_bitdepth 0, mono 0,
        // colour_description 0, color_range.
        let bits = [
            "000",
            "1",
            "1",     // seq_profile, still_picture, reduced_still_picture_header
            "00000", // seq_level_idx[0]
            "0000",
            "0000", // frame_width_bits_minus_1, frame_height_bits_minus_1
            "0",
            "0",   // max_frame_width_minus_1, max_frame_height_minus_1
            "000", // 128x128 superblock, filter intra, intra edge filter
            "000", // superres, cdef, restoration
            "0",
            "0",
            "0", // high_bitdepth, mono_chrome, color_description_present
            if full { "1" } else { "0" },
        ]
        .concat();
        let mut payload: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .map(|c| {
                let mut b = 0u8;
                for i in 0..8 {
                    b = (b << 1) | u8::from(c.get(i) == Some(&b'1'));
                }
                b
            })
            .collect();
        payload.push(0x80); // trailing bits (not read)
        let mut obu = vec![0x0A, payload.len() as u8]; // type 1, has_size
        obu.extend_from_slice(&payload);
        obu
    }

    #[test]
    fn reads_the_range_of_a_reduced_header() {
        assert_eq!(
            color_range(&reduced_seq_header(true)),
            Some(ColorRange::Full)
        );
        assert_eq!(
            color_range(&reduced_seq_header(false)),
            Some(ColorRange::Limited)
        );
        assert_eq!(color_range(&[]), None);
    }
}
