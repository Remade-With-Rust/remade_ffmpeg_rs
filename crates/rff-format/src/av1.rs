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
