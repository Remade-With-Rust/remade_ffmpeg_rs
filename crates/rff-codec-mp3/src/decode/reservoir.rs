//! The bit reservoir.
//!
//! MP3 main data is not aligned to frame boundaries: a frame's `main_data_begin`
//! points *backwards* by up to 511 bytes into bytes carried over from previous
//! frames. The reservoir holds that tail so each frame's main data can be
//! reassembled into one contiguous bitstream for the Huffman/scalefactor stages.

/// Rolling buffer of recent main-data bytes.
#[derive(Default)]
pub struct Reservoir {
    /// Bytes available from previous frames (the most this needs is 511).
    buf: Vec<u8>,
}

impl Reservoir {
    /// Reassemble this frame's main data: take `main_data_begin` bytes from the
    /// reservoir tail, append the current frame's main data, and refresh the
    /// reservoir for the next frame.
    pub fn assemble(&mut self, main_data_begin: u16, frame_main_data: &[u8]) -> Vec<u8> {
        let begin = main_data_begin as usize;
        let mut out = Vec::with_capacity(begin + frame_main_data.len());
        let tail = self.buf.len().saturating_sub(begin);
        out.extend_from_slice(&self.buf[tail..]);
        out.extend_from_slice(frame_main_data);
        // Keep the last ~511 bytes for the next frame's back-reference.
        self.buf = out.iter().rev().take(512).rev().copied().collect();
        out
    }

    /// Drop carried-over state (seek / discontinuity).
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}
