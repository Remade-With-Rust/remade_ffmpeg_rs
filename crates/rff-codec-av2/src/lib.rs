//! In-house **AV2 decoder**, backed by the pure-Rust
//! [`rusty_av2d`] engine — no C, no FFI.
//!
//! This crate is the thin `rff` adapter: [`register`] wires `rusty_av2d` into a
//! [`CodecRegistry`], and a private wrapper translates between the rff
//! `Decoder` trait and `rusty_av2d`'s push/pull API (`Packet`/`Frame` ↔
//! byte buffers/planes, and the `Again` control-flow error one-to-one). All
//! codec logic — the decoder that is byte-identical to AOM's `avmdec` across a
//! 45-clip conformance corpus — lives in `rusty_av2d`.
//!
//! **Decode only.** There is no AV2 encoder; [`Codec::encoder`] is `None`.
//!
//! AV2 is not a finalized standard: correctness is defined against the AVM
//! reference as of the pinned `rusty_av2d` release, and the bitstream may still
//! change. See that crate's `STATUS.md` for the full scope statement.

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder};
use rff_core::{CodecId, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_av2d::{PixelLayout, PlanarImageComponent, Rav1dError};

// The full engine, for callers that want the native API directly.
pub use rusty_av2d;

/// Register the AV2 decoder into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Av2,
        name: "av2",
        long_name: "AOMedia Video 2",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(Av2Decoder::new())),
        encoder: None,
    });
}

/// Map a `rusty_av2d` error onto the rff error space. `TryAgain` is control
/// flow (FFmpeg's `EAGAIN` convention) and must map exactly — the send/receive
/// loops key on it.
fn map_err(e: Rav1dError) -> Error {
    match e {
        Rav1dError::TryAgain => Error::Again,
        Rav1dError::InvalidArgument => Error::InvalidData("av2: invalid bitstream".into()),
        Rav1dError::UnsupportedBitstream => {
            Error::Unsupported("av2: unsupported bitstream feature".into())
        }
        other => Error::InvalidData(format!("av2: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

struct Av2Decoder {
    /// `None` until the first packet: constructing the engine can fail, and
    /// [`Decoder`] construction in the registry is infallible.
    inner: Option<rusty_av2d::Decoder>,
    /// Set when the engine accepted only part of a packet. The engine asserts
    /// if `send_data` is called again while data is still pending, so the
    /// remainder must be drained through `send_pending_data` first.
    pending: bool,
    /// Set by [`Decoder::flush`]; makes `receive_frame` report `Eof` rather
    /// than `Again` once the engine has no more buffered pictures.
    draining: bool,
}

impl Av2Decoder {
    fn new() -> Self {
        Self {
            inner: None,
            pending: false,
            draining: false,
        }
    }

    fn engine(&mut self) -> Result<&mut rusty_av2d::Decoder> {
        if self.inner.is_none() {
            self.inner = Some(rusty_av2d::Decoder::new().map_err(map_err)?);
        }
        Ok(self.inner.as_mut().expect("just initialized"))
    }
}

impl Decoder for Av2Decoder {
    fn configure(&mut self, _params: &CodecParams) -> Result<()> {
        // The AV2 bitstream carries its own size / bit-depth / colour
        // configuration in the sequence header; container hints are not needed.
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let pts = packet.pts;
        let data = packet.data.clone().into_boxed_slice();

        // Finish any partially-consumed previous packet before offering a new
        // one — `send_data` asserts (panics) if data is still pending.
        if self.pending {
            let r = self.engine()?.send_pending_data();
            match r {
                Ok(()) => self.pending = false,
                Err(Rav1dError::TryAgain) => return Err(Error::Again),
                Err(e) => {
                    self.pending = false;
                    return Err(map_err(e));
                }
            }
        }

        let r = self.engine()?.send_data(data, None, pts, None);
        match r {
            Ok(()) => Ok(()),
            Err(Rav1dError::TryAgain) => {
                // Partially accepted: the engine holds the remainder. Pull
                // frames out, then re-offer via the branch above.
                self.pending = true;
                Err(Error::Again)
            }
            Err(e) => Err(map_err(e)),
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let draining = self.draining;
        let Some(engine) = self.inner.as_mut() else {
            // Nothing was ever sent.
            return Err(if draining { Error::Eof } else { Error::Again });
        };
        match engine.get_picture() {
            Ok(pic) => picture_to_frame(&pic),
            // No picture ready: more input needed, unless we're draining, in
            // which case the stream is done.
            Err(Rav1dError::TryAgain) if draining => Err(Error::Eof),
            Err(e) => Err(map_err(e)),
        }
    }

    fn flush(&mut self) {
        self.draining = true;
    }
}

/// Map a `rusty_av2d` [`Picture`](rusty_av2d::Picture) onto an rff
/// [`VideoFrame`].
///
/// The engine's planes are stride-padded for alignment; rff frames carry an
/// explicit stride, so rows are passed through verbatim rather than repacked.
fn picture_to_frame(pic: &rusty_av2d::Picture) -> Result<Frame> {
    let layout = pic.pixel_layout();
    let format = match (layout, pic.bit_depth()) {
        (PixelLayout::I420, 8) => PixelFormat::Yuv420p,
        (PixelLayout::I422, 8) => PixelFormat::Yuv422p,
        (PixelLayout::I444, 8) => PixelFormat::Yuv444p,
        (PixelLayout::I420, 10) => PixelFormat::Yuv420p10,
        (PixelLayout::I422, 10) => PixelFormat::Yuv422p10,
        (PixelLayout::I444, 10) => PixelFormat::Yuv444p10,
        (PixelLayout::I420, 12) => PixelFormat::Yuv420p12,
        (PixelLayout::I422, 12) => PixelFormat::Yuv422p12,
        (PixelLayout::I444, 12) => PixelFormat::Yuv444p12,
        // Monochrome has no rff pixel format, and no clip in the conformance
        // corpus exercises it — refuse rather than fabricate neutral chroma.
        (PixelLayout::I400, _) => {
            return Err(Error::Unsupported("av2: monochrome (4:0:0) output".into()))
        }
        (_, depth) => {
            return Err(Error::Unsupported(format!(
                "av2: {depth}-bit output is not mapped to a pixel format"
            )))
        }
    };

    let mut planes = Vec::with_capacity(3);
    let mut strides = Vec::with_capacity(3);
    for c in [
        PlanarImageComponent::Y,
        PlanarImageComponent::U,
        PlanarImageComponent::V,
    ] {
        planes.push(pic.plane(c).to_vec());
        strides.push(pic.stride(c) as usize);
    }

    Ok(Frame::Video(VideoFrame {
        width: pic.width(),
        height: pic.height(),
        format,
        planes,
        strides,
        pts: pic.timestamp(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_a_decode_only_av2_codec() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        let codec = reg.by_id(CodecId::Av2).expect("av2 registered");
        assert_eq!(codec.name, "av2");
        assert_eq!(codec.media_type, MediaType::Video);
        assert!(codec.decoder.is_some());
        assert!(
            codec.encoder.is_none(),
            "there is no AV2 encoder; the registry must not advertise one"
        );
    }

    #[test]
    fn try_again_maps_to_the_control_flow_error() {
        // The send/receive loops key on `Again`; if this ever maps to a plain
        // decode error the pipeline deadlocks instead of pulling frames.
        assert!(matches!(map_err(Rav1dError::TryAgain), Error::Again));
    }

    #[test]
    fn garbage_input_errors_rather_than_panicking() {
        let mut dec = Av2Decoder::new();
        let packet = Packet {
            data: vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33],
            ..Default::default()
        };
        // Either outcome is fine — what must not happen is a panic.
        let _ = dec.send_packet(&packet);
        let _ = dec.receive_frame();
    }
}

#[cfg(test)]
mod fuzz_smoke {
    use super::*;
    use rff_codec::Decoder as _;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 33) as u8
        }
        fn range(&mut self, n: usize) -> usize {
            (self.next() >> 33) as usize % n.max(1)
        }
    }

    /// Random bytes straight into `send_packet` must never panic: the pipeline
    /// hands the decoder whatever the container yielded, and a container can be
    /// lying. The workspace-wide `fuzz_decode` harness caught a real underflow
    /// here (a corrupt tile-size prefix); this keeps a fast check close to the
    /// adapter, including the long buffers that exercise size fields.
    #[test]
    fn random_bytes_never_panic() {
        let mut rng = Rng(0xFACE_B00C_1234_5678);
        for i in 0..1500 {
            let len = if i % 10 == 0 {
                rng.range(65536)
            } else {
                rng.range(4096)
            } + 1;
            let data: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
            let mut dec = Av2Decoder::new();
            let pkt = Packet {
                data,
                ..Default::default()
            };
            let _ = dec.send_packet(&pkt);
            for _ in 0..8 {
                if dec.receive_frame().is_err() {
                    break;
                }
            }
            dec.flush();
            let _ = dec.receive_frame();
        }
    }
}
