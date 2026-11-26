//! Raw uncompressed video codec — the video analog of [`rff_codec_pcm`].
//!
//! Raw-video packets carry tightly-packed planar pixels, so they are *not*
//! self-describing: the decoder learns width, height, and pixel layout from
//! [`CodecParams`] via [`configure`](rff_codec::Decoder::configure). This is the
//! codec behind the `y4m` / `rawvideo` input path — it lets the CLI feed real
//! uncompressed clips into an encoder (e.g. VP9) for RD measurement.
//!
//! Supported layouts: planar `yuv420p` / `yuv422p` / `yuv444p` at 8 bits, and
//! at 10 and 12 bits (little-endian `u16` samples); packed `rgb24` and `rgba`.

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Register the raw-video codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::RawVideo,
        name: "rawvideo",
        long_name: "raw uncompressed video (planar YUV, packed RGB)",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(RawVideoDecoder::default())),
        encoder: Some(|| Box::new(RawVideoEncoder::default())),
    });
}

/// Per-plane `(row bytes, rows)` for the supported layouts. Chroma dimensions
/// use ceil-division so odd frame sizes round up (matching y4m); samples above
/// 8 bits take two bytes.
fn plane_dims(format: PixelFormat, w: usize, h: usize) -> Result<Vec<(usize, usize)>> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let b = format.bytes_per_sample();
    Ok(match format {
        PixelFormat::Yuv420p | PixelFormat::Yuv420p10 | PixelFormat::Yuv420p12 => {
            vec![(w * b, h), (cw * b, ch), (cw * b, ch)]
        }
        PixelFormat::Yuv422p | PixelFormat::Yuv422p10 | PixelFormat::Yuv422p12 => {
            vec![(w * b, h), (cw * b, h), (cw * b, h)]
        }
        PixelFormat::Yuv444p | PixelFormat::Yuv444p10 | PixelFormat::Yuv444p12 => {
            vec![(w * b, h), (w * b, h), (w * b, h)]
        }
        PixelFormat::Rgb24 => vec![(w * 3, h)],
        PixelFormat::Rgba => vec![(w * 4, h)],
        other => {
            return Err(Error::unsupported(format!(
                "rawvideo: pixel format `{}`",
                other.name()
            )))
        }
    })
}

#[derive(Default)]
struct RawVideoDecoder {
    width: u32,
    height: u32,
    format: Option<PixelFormat>,
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for RawVideoDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        let format = params
            .pixel_format
            .ok_or_else(|| Error::invalid("rawvideo decode: stream is missing a pixel format"))?;
        if params.width == 0 || params.height == 0 {
            return Err(Error::invalid(
                "rawvideo decode: stream is missing dimensions",
            ));
        }
        // Validate the format is one we handle.
        plane_dims(format, params.width as usize, params.height as usize)?;
        self.width = params.width;
        self.height = params.height;
        self.format = Some(format);
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let format = self
            .format
            .ok_or_else(|| Error::invalid("rawvideo decode: not configured"))?;
        let (w, h) = (self.width as usize, self.height as usize);
        let dims = plane_dims(format, w, h)?;
        let expected: usize = dims.iter().map(|(pw, ph)| pw * ph).sum();
        if packet.data.len() < expected {
            return Err(Error::invalid(format!(
                "rawvideo decode: packet has {} bytes, need {} for {}x{} {}",
                packet.data.len(),
                expected,
                w,
                h,
                format.name()
            )));
        }
        // Split the tightly-packed packet into planes; stride == plane width.
        let mut planes = Vec::with_capacity(dims.len());
        let mut strides = Vec::with_capacity(dims.len());
        let mut off = 0usize;
        for (pw, ph) in dims {
            let size = pw * ph;
            planes.push(packet.data[off..off + size].to_vec());
            strides.push(pw);
            off += size;
        }
        self.frame = Some(Frame::Video(VideoFrame {
            width: self.width,
            height: self.height,
            format,
            planes,
            strides,
            pts: packet.pts,
        }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frame.take() {
            return Ok(frame);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

#[derive(Default)]
struct RawVideoEncoder {
    packet: Option<Packet>,
    eof: bool,
}

impl Encoder for RawVideoEncoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "rawvideo encode: audio frame on a video codec",
                ))
            }
        };
        let (w, h) = (vf.width as usize, vf.height as usize);
        let dims = plane_dims(vf.format, w, h)?;
        // Emit tightly-packed planes, stripping any stride padding.
        let mut data = Vec::with_capacity(dims.iter().map(|(pw, ph)| pw * ph).sum());
        for (plane, ((pw, ph), &stride)) in vf.planes.iter().zip(dims.iter().zip(vf.strides.iter()))
        {
            for row in 0..*ph {
                let start = row * stride;
                data.extend_from_slice(&plane[start..start + pw]);
            }
        }
        let mut packet = Packet::from_data(0, data);
        packet.pts = vf.pts;
        self.packet = Some(packet);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.packet.take() {
            return Ok(packet);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_yuv420p() {
        let (w, h) = (4usize, 2usize);
        // Y=8, U=V=(2*1)=2 each -> 12 bytes.
        let data: Vec<u8> = (0..12).collect();
        let mut dec = RawVideoDecoder::default();
        dec.configure(&CodecParams {
            width: w as u32,
            height: h as u32,
            pixel_format: Some(PixelFormat::Yuv420p),
            ..Default::default()
        })
        .unwrap();
        dec.send_packet(&Packet::from_data(0, data.clone()))
            .unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            unreachable!()
        };
        assert_eq!((vf.width, vf.height), (4, 2));
        assert_eq!(vf.planes[0].len(), 8);
        assert_eq!(vf.planes[1].len(), 2);
        assert_eq!(vf.planes[2].len(), 2);

        let mut enc = RawVideoEncoder::default();
        enc.send_frame(&Frame::Video(vf)).unwrap();
        assert_eq!(enc.receive_packet().unwrap().data, data);
    }

    #[test]
    fn high_bit_depth_and_rgba_layouts() {
        for (format, w, h, size, planes) in [
            (
                PixelFormat::Yuv420p10,
                3usize,
                3usize,
                (9 + 2 * 4) * 2,
                3usize,
            ),
            (PixelFormat::Rgba, 3, 2, 24, 1),
        ] {
            let data: Vec<u8> = (0..size as u32).map(|v| v as u8).collect();
            let mut dec = RawVideoDecoder::default();
            dec.configure(&CodecParams {
                width: w as u32,
                height: h as u32,
                pixel_format: Some(format),
                ..Default::default()
            })
            .unwrap();
            dec.send_packet(&Packet::from_data(0, data.clone()))
                .unwrap();
            let Frame::Video(vf) = dec.receive_frame().unwrap() else {
                unreachable!()
            };
            assert_eq!(vf.planes.len(), planes);
            let mut enc = RawVideoEncoder::default();
            enc.send_frame(&Frame::Video(vf)).unwrap();
            assert_eq!(enc.receive_packet().unwrap().data, data);
        }
    }

    #[test]
    fn decode_requires_params() {
        let mut dec = RawVideoDecoder::default();
        assert!(dec.configure(&CodecParams::default()).is_err());
    }
}
