//! A `Packet` is a chunk of *compressed* data for one stream — the unit that
//! flows demuxer → decoder and encoder → muxer. Analogous to `AVPacket`.

use crate::rational::Rational;

/// Per-packet flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketFlags {
    /// This packet begins a keyframe (random-access point).
    pub keyframe: bool,
}

/// A compressed packet belonging to a single stream.
#[derive(Debug, Clone, Default)]
pub struct Packet {
    /// Index of the stream this packet belongs to within its container.
    pub stream_index: usize,
    /// Presentation timestamp, in the stream's `time_base` units. `None` if unset.
    pub pts: Option<i64>,
    /// Decode timestamp, in the stream's `time_base` units. `None` if unset.
    pub dts: Option<i64>,
    /// Duration of this packet, in `time_base` units (0 if unknown).
    pub duration: i64,
    /// The stream time base these timestamps are expressed in.
    pub time_base: Rational,
    /// The compressed payload.
    pub data: Vec<u8>,
    /// Packet flags (keyframe, ...).
    pub flags: PacketFlags,
}

impl Packet {
    /// Construct an empty packet for the given stream.
    pub fn new(stream_index: usize) -> Packet {
        Packet {
            stream_index,
            time_base: Rational::new(1, 1000),
            ..Default::default()
        }
    }

    /// Construct a packet wrapping an existing payload.
    pub fn from_data(stream_index: usize, data: Vec<u8>) -> Packet {
        Packet {
            data,
            ..Packet::new(stream_index)
        }
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags.keyframe
    }
}
