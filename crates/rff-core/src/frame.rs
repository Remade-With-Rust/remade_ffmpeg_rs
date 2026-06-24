//! A `Frame` is a chunk of *raw, decoded* data — pixels for video, samples for
//! audio. It flows decoder → (filters) → encoder. Analogous to `AVFrame`.

use crate::media::{PixelFormat, SampleFormat};

/// A decoded raw video frame.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Plane buffers (e.g. Y, U, V for planar YUV). Packed formats use one plane.
    pub planes: Vec<Vec<u8>>,
    /// Bytes per row for each plane (the "stride"; may exceed `width` for alignment).
    pub strides: Vec<usize>,
    /// Presentation timestamp in the source stream's time base. `None` if unset.
    pub pts: Option<i64>,
}

/// A decoded raw audio frame (one block of samples across all channels).
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// Sample buffers. Interleaved formats use one buffer; planar formats use
    /// one per channel.
    pub planes: Vec<Vec<u8>>,
    /// Number of samples *per channel* in this frame.
    pub samples: usize,
    /// Presentation timestamp in the source stream's time base. `None` if unset.
    pub pts: Option<i64>,
}

/// Either a video or audio frame. Decoders emit these; encoders consume them.
#[derive(Debug, Clone)]
pub enum Frame {
    Video(VideoFrame),
    Audio(AudioFrame),
}

impl Frame {
    pub fn pts(&self) -> Option<i64> {
        match self {
            Frame::Video(v) => v.pts,
            Frame::Audio(a) => a.pts,
        }
    }
}
