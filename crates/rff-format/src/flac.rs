//! FLAC helpers shared by containers (MP4 `dfLa`, Matroska `A_FLAC`
//! CodecPrivate): the frame header's block size and a STREAMINFO rewritten
//! from the frames a muxer actually wrote (total samples, frame and block
//! size bounds), as FFmpeg's FLAC-carrying muxers do at the trailer.

/// The block size (samples per channel) a FLAC frame header declares, or
/// `None` when `frame` does not start with a frame header.
pub fn frame_block_size(frame: &[u8]) -> Option<u32> {
    if frame.len() < 5 || frame[0] != 0xFF || frame[1] & 0xFE != 0xF8 {
        return None;
    }
    let code = frame[2] >> 4;
    // The UTF-8-style coded frame/sample number: its length from the
    // leading ones of the first byte.
    let lead = frame[4].leading_ones() as usize;
    let num_len = match lead {
        0 => 1,
        2..=7 => lead,
        _ => return None,
    };
    let at = 4 + num_len;
    match code {
        1 => Some(192),
        2..=5 => Some(576 << (code - 2)),
        6 => frame.get(at).map(|&b| u32::from(b) + 1),
        7 => frame
            .get(at..at + 2)
            .map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])) + 1),
        8..=15 => Some(256 << (code - 8)),
        _ => None,
    }
}

/// What a muxer learns from the FLAC frames it writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlacStats {
    pub frames: u64,
    pub total_samples: u64,
    pub min_frame: u32,
    pub max_frame: u32,
    /// Smallest block size of every frame but the last (which may be short).
    pub min_block: u32,
    pub max_block: u32,
    last_block: u32,
    /// A frame whose header did not parse: the totals are unknown.
    pub unparsed: bool,
}

impl FlacStats {
    /// Account for one FLAC frame (one packet).
    pub fn push(&mut self, frame: &[u8]) {
        let size = u32::try_from(frame.len()).unwrap_or(u32::MAX);
        let Some(block) = frame_block_size(frame) else {
            self.unparsed = true;
            return;
        };
        if self.frames > 0 {
            // The previous frame was not the last after all.
            self.min_block = match self.min_block {
                0 => self.last_block,
                m => m.min(self.last_block),
            };
        }
        self.frames += 1;
        self.total_samples += u64::from(block);
        self.max_block = self.max_block.max(block);
        self.last_block = block;
        self.min_frame = if self.frames == 1 {
            size
        } else {
            self.min_frame.min(size)
        };
        self.max_frame = self.max_frame.max(size);
    }

    /// `streaminfo` (the 34-byte body) with the block and frame size bounds
    /// and the total sample count from the frames written. The MD5 is kept
    /// only when the original declared the same total (the same audio);
    /// otherwise it is zeroed, which means "unknown". Rate, channels and bit
    /// depth are left alone. Unchanged when no frame was written or a frame
    /// header did not parse.
    pub fn rewrite(&self, streaminfo: &[u8; 34]) -> [u8; 34] {
        let mut si = *streaminfo;
        if self.frames == 0 || self.unparsed {
            return si;
        }
        let min_block = if self.frames == 1 {
            self.last_block
        } else {
            self.min_block
        };
        let old_total =
            u64::from_be_bytes([0, 0, 0, si[13] & 0x0F, si[14], si[15], si[16], si[17]]);
        si[0..2].copy_from_slice(&(min_block.min(0xFFFF) as u16).to_be_bytes());
        si[2..4].copy_from_slice(&(self.max_block.min(0xFFFF) as u16).to_be_bytes());
        si[4..7].copy_from_slice(&self.min_frame.min(0xFF_FFFF).to_be_bytes()[1..]);
        si[7..10].copy_from_slice(&self.max_frame.min(0xFF_FFFF).to_be_bytes()[1..]);
        let total = self.total_samples.min((1 << 36) - 1);
        si[13] = (si[13] & 0xF0) | (total >> 32) as u8;
        si[14..18].copy_from_slice(&(total as u32).to_be_bytes());
        if old_total != total {
            si[18..34].fill(0);
        }
        si
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame header: block size code, coded number bytes, optional size.
    fn frame(code: u8, number: &[u8], extra: &[u8], len: usize) -> Vec<u8> {
        let mut f = vec![0xFF, 0xF8, code << 4 | 9, 0x08];
        f.extend_from_slice(number);
        f.extend_from_slice(extra);
        f.resize(len, 0);
        f
    }

    #[test]
    fn block_sizes_from_every_code() {
        assert_eq!(frame_block_size(&frame(12, &[0], &[], 16)), Some(4096));
        assert_eq!(frame_block_size(&frame(1, &[5], &[], 16)), Some(192));
        assert_eq!(frame_block_size(&frame(3, &[5], &[], 16)), Some(1152));
        assert_eq!(
            frame_block_size(&frame(6, &[0xC2, 0x80], &[99], 16)),
            Some(100)
        );
        assert_eq!(
            frame_block_size(&frame(7, &[1], &[0x01, 0x00], 16)),
            Some(257)
        );
        assert_eq!(frame_block_size(b"not flac"), None);
    }

    #[test]
    fn streaminfo_is_rewritten_from_the_frames() {
        let mut st = FlacStats::default();
        for (i, len) in [(0u8, 900usize), (1, 1200), (2, 700)] {
            st.push(&frame(12, &[i], &[], len));
        }
        st.push(&frame(7, &[3], &[0x03, 0xE7], 300)); // a short last block: 1000
        let mut si = [0u8; 34];
        si[10..14].copy_from_slice(&[0x0B, 0xB8, 0x03, 0x70]); // 48 kHz, 2 ch, 24 bit
        si[18..].fill(0xAB);
        let out = st.rewrite(&si);
        assert_eq!(u16::from_be_bytes([out[0], out[1]]), 4096); // min: not the last
        assert_eq!(u16::from_be_bytes([out[2], out[3]]), 4096);
        assert_eq!(&out[4..7], &[0, 0x01, 0x2C]); // 300
        assert_eq!(&out[7..10], &[0, 0x04, 0xB0]); // 1200
        assert_eq!(&out[10..13], &si[10..13]);
        assert_eq!(out[13] >> 4, si[13] >> 4);
        assert_eq!(
            u32::from_be_bytes([out[14], out[15], out[16], out[17]]),
            3 * 4096 + 1000
        );
        assert_eq!(&out[18..], &[0u8; 16], "a different total: MD5 unknown");
        // Same total: the MD5 stays.
        let mut same = out;
        same[18..].fill(0xCD);
        assert_eq!(&st.rewrite(&same)[18..], &[0xCD; 16]);
    }
}
