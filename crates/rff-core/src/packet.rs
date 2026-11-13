//! A `Packet` is a chunk of *compressed* data for one stream — the unit that
//! flows demuxer → decoder and encoder → muxer. Analogous to `AVPacket`.

use crate::rational::Rational;

/// Per-packet flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketFlags {
    /// This packet begins a keyframe (random-access point).
    pub keyframe: bool,
    /// The last packet of its logical stream (Ogg: the final packet of the
    /// end-of-stream page). Audio decoders trim this packet's output so the
    /// stream ends at its [`Packet::end_position`], as FFmpeg does.
    pub end_of_stream: bool,
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
    /// Where the stream's decoded output ENDS once this packet is decoded, in
    /// `time_base` units, when the container records it — Ogg's granule
    /// position, which sits on the last packet completing each page (`None`
    /// on the others, and in containers without one). Unlike `pts` it marks an
    /// end, not a start: decoders count samples from it to trim end padding
    /// off the [`end_of_stream`](PacketFlags::end_of_stream) packet.
    pub end_position: Option<i64>,
    /// Samples (per channel, at the stream's sample rate) to drop from the
    /// END of this packet's decoded output; 0 = none. The container's record
    /// of the encoder's end padding on a stream's last packet — Matroska's
    /// `DiscardPadding`, FFmpeg's `AV_PKT_DATA_SKIP_SAMPLES` end field — for
    /// containers whose timestamps are too coarse to carry it as an
    /// [`end_position`](Self::end_position). As in FFmpeg, audio decoders drop
    /// that many samples when it fits in what the packet decodes to and
    /// ignore it otherwise.
    pub discard_padding: u32,
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
