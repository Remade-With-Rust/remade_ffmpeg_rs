//! `hvc1` items: the `hvcC` parameter sets plus the item's length-prefixed
//! NAL units, fed to `rusty_h265` one NAL at a time (no Annex-B copy).

use std::panic::{catch_unwind, AssertUnwindSafe};

use rff_core::{Error, Result};
use rff_format::heif::HeifFile;
use rusty_h265::{Decoder, Frame};

use crate::image::{blit_plane, Chroma, Image, Plane};

/// What `hvcC` and its SPS say about an item, read before decoding so the
/// size limits apply before any picture is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HvcInfo {
    pub nal_len: usize,
    pub chroma: Chroma,
    pub bit_depth: u8,
    pub width: u32,
    pub height: u32,
    /// VUI colour, when the SPS signals it: `(primaries, transfer, matrix, full_range)`.
    pub vui: Option<(u16, u16, u16, bool)>,
}

/// The NAL arrays of an `hvcC` record (ISO/IEC 14496-15 §8.3.3), in order,
/// and the length-prefix size of the item's NAL units.
pub(crate) fn hvcc_nals(hvcc: &[u8]) -> Result<(usize, Vec<&[u8]>)> {
    let bad = || Error::invalid("heif: malformed `hvcC`");
    if hvcc.len() < 23 {
        return Err(bad());
    }
    let nal_len = (hvcc[21] & 3) as usize + 1;
    if nal_len == 3 {
        return Err(bad());
    }
    let arrays = hvcc[22] as usize;
    let mut out = Vec::new();
    let mut i = 23;
    for _ in 0..arrays {
        let n = u16::from_be_bytes([
            *hvcc.get(i + 1).ok_or_else(bad)?,
            *hvcc.get(i + 2).ok_or_else(bad)?,
        ]);
        i += 3;
        for _ in 0..n {
            let len = u16::from_be_bytes([
                *hvcc.get(i).ok_or_else(bad)?,
                *hvcc.get(i + 1).ok_or_else(bad)?,
            ]) as usize;
            i += 2;
            out.push(hvcc.get(i..i + len).ok_or_else(bad)?);
            i += len;
        }
    }
    Ok((nal_len, out))
}

/// Read the SPS in `hvcC` for the coded size, chroma format, bit depth and
/// VUI colour.
pub(crate) fn hvc_info(hvcc: &[u8]) -> Result<HvcInfo> {
    let (nal_len, nals) = hvcc_nals(hvcc)?;
    let sps = nals
        .iter()
        .find(|n| n.len() > 2 && (n[0] >> 1) & 0x3f == 33)
        .ok_or_else(|| Error::invalid("heif: `hvcC` carries no SPS"))?;
    let rbsp = rusty_h265::nal::unescape(&sps[2..]);
    let sps = catch_unwind(AssertUnwindSafe(|| rusty_h265::ps::parse_sps(&rbsp.data)))
        .map_err(|_| Error::invalid("heif: SPS parser panicked (recovered)"))?
        .map_err(|e| Error::invalid(format!("heif: bad SPS: {e}")))?;
    let chroma = Chroma::from_idc(sps.chroma_format_idc)
        .ok_or_else(|| Error::invalid("heif: bad chroma_format_idc"))?;
    let vui = sps.vui.as_ref().map(|v| {
        // `colour_description_present_flag` = 0 leaves the three codes at 0,
        // and primaries 0 is reserved, so 0 means "not signalled".
        if v.colour_primaries == 0 {
            (2, 2, 2, v.video_full_range_flag)
        } else {
            (
                v.colour_primaries as u16,
                v.transfer_characteristics as u16,
                v.matrix_coeffs as u16,
                v.video_full_range_flag,
            )
        }
    });
    Ok(HvcInfo {
        nal_len,
        chroma,
        bit_depth: sps.bit_depth_luma,
        width: sps.width,
        height: sps.height,
        vui,
    })
}

/// Split length-prefixed NAL units.
fn length_prefixed(data: &[u8], nal_len: usize) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let head = data
            .get(i..i + nal_len)
            .ok_or_else(|| Error::invalid("heif: truncated NAL length"))?;
        let len = head.iter().fold(0usize, |v, &b| (v << 8) | b as usize);
        i += nal_len;
        let nal = data
            .get(i..i.saturating_add(len))
            .ok_or_else(|| Error::invalid("heif: NAL runs past the item"))?;
        if !nal.is_empty() {
            out.push(nal);
        }
        i += len;
    }
    Ok(out)
}

/// Decode `hvc1` item `id` with a fresh decoder.
pub(crate) fn decode_frame(file: &HeifFile, id: u32) -> Result<Frame> {
    ItemDecoder::new().decode(file, id)
}

/// An HEVC decoder kept across a run of items (the tiles of a grid).
///
/// A fresh `rusty_h265::Decoder` per tile re-allocates and zeroes its picture
/// pool and filter scratch, and re-parses the parameter sets, 48 times for a
/// 12 MP iPhone photo. Every tile is an IDR picture, so one decoder decodes
/// them in sequence with no state carried from one to the next; the
/// parameter sets are re-sent only when the tile's `hvcC` differs. Any error
/// or contained panic drops the decoder, so a damaged tile cannot leave
/// state behind for the next.
pub(crate) struct ItemDecoder {
    dec: Option<Decoder>,
    /// The `hvcC` whose parameter sets `dec` holds, and what its SPS says.
    hvcc: Vec<u8>,
    info: Option<HvcInfo>,
}

impl ItemDecoder {
    pub(crate) fn new() -> Self {
        ItemDecoder {
            dec: None,
            hvcc: Vec::new(),
            info: None,
        }
    }

    /// Decode `hvc1` item `id` (after the items it is predicted from,
    /// `pred`) and return the last picture in output order.
    pub(crate) fn decode(&mut self, file: &HeifFile, id: u32) -> Result<Frame> {
        let hvcc = file
            .hvcc(id)
            .ok_or_else(|| Error::invalid(format!("heif: hvc1 item {id} has no `hvcC`")))?;
        let same = self.dec.is_some() && self.hvcc == hvcc;
        let info = match (same, self.info) {
            (true, Some(info)) => info,
            _ => {
                let info = hvc_info(hvcc)?;
                file.check_size(info.width, info.height)?;
                info
            }
        };
        let deps = file.refs_from(id, b"pred");
        if deps.len() > 64 {
            return Err(Error::invalid("heif: too many `pred` references"));
        }
        let mut datas = Vec::with_capacity(deps.len() + 1);
        for &d in &deps {
            if d == id || file.item(d).map(|it| it.item_type) != Some(*b"hvc1") {
                return Err(Error::invalid(format!("heif: bad `pred` reference {d}")));
            }
            datas.push(file.item_data(d)?);
        }
        datas.push(file.item_data(id)?);

        let mut dec = match self.dec.take() {
            Some(d) if same => d,
            _ => Decoder::new(),
        };
        let run = |dec: &mut Decoder| -> Result<Option<Frame>> {
            if !same {
                for nal in hvcc_nals(hvcc)?.1 {
                    dec.push_nal(nal, None).map_err(map_err)?;
                }
            }
            for data in &datas {
                for nal in length_prefixed(data, info.nal_len)? {
                    dec.push_nal(nal, None).map_err(map_err)?;
                }
            }
            dec.flush();
            let mut last = None;
            while let Ok(f) = dec.next_frame() {
                last = Some(f);
            }
            Ok(last)
        };
        let r = catch_unwind(AssertUnwindSafe(|| run(&mut dec)));
        if let Ok(Ok(Some(_))) = &r {
            // Keep the decoder (and what it holds) for the next item.
            if !same {
                self.hvcc.clear();
                self.hvcc.extend_from_slice(hvcc);
            }
            self.info = Some(info);
            self.dec = Some(dec);
        } else {
            self.hvcc.clear();
            self.info = None;
        }
        match r {
            Ok(Ok(Some(f))) => Ok(f),
            Ok(Ok(None)) => Err(Error::invalid(format!(
                "heif: item {id} decoded no picture"
            ))),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::invalid(
                "heif: HEVC decoder panicked on malformed input (recovered)",
            )),
        }
    }
}

fn map_err(e: rusty_h265::Error) -> Error {
    match e {
        rusty_h265::Error::Unsupported(m) => Error::Unsupported(format!("heif: HEVC {m}")),
        other => Error::InvalidData(format!("heif: HEVC: {other}")),
    }
}

/// The cropped geometry of a decoded frame: luma `(x, y, w, h)` and chroma.
fn frame_layout(f: &Frame) -> Result<(Chroma, (usize, usize, usize, usize))> {
    let pic = &f.picture;
    let chroma = Chroma::from_idc(pic.chroma_format_idc)
        .ok_or_else(|| Error::invalid("heif: bad chroma format"))?;
    Ok((chroma, pic.crop))
}

/// A decoded frame's size: its conformance window.
pub(crate) fn frame_size(f: &Frame) -> (u32, u32) {
    (f.picture.crop.2 as u32, f.picture.crop.3 as u32)
}

/// Copy a decoded frame (its conformance window) into a new [`Image`].
pub(crate) fn frame_to_image(f: &Frame) -> Result<Image> {
    let (chroma, (_, _, cw, ch)) = frame_layout(f)?;
    let mut img = Image {
        width: cw as u32,
        height: ch as u32,
        chroma,
        bit_depth: f.picture.bit_depth_luma,
        planes: Vec::new(),
    };
    let (ccw, cch) = chroma.chroma_size(cw as u32, ch as u32);
    for i in 0..chroma.planes() {
        let (w, h) = if i == 0 {
            (cw as u32, ch as u32)
        } else {
            (ccw, cch)
        };
        img.planes.push(Plane::filled(w, h, 0));
    }
    blit_frame(f, &mut img, 0, 0)?;
    Ok(img)
}

/// Copy a decoded frame's conformance window into `dst` at luma `(x, y)`,
/// clipped to `dst`.
pub(crate) fn blit_frame(f: &Frame, dst: &mut Image, x: u32, y: u32) -> Result<()> {
    let (chroma, (cx, cy, cw, ch)) = frame_layout(f)?;
    if chroma != dst.chroma || f.picture.bit_depth_luma != dst.bit_depth {
        return Err(Error::invalid(
            "heif: tiles disagree on chroma format or bit depth",
        ));
    }
    let (sx, sy) = chroma.shift();
    for (i, dp) in dst.planes.iter_mut().enumerate() {
        let sp = &f.picture.planes[i];
        let (px, py, pw, ph, ox, oy) = if i == 0 {
            (cx, cy, cw, ch, x, y)
        } else {
            (
                cx >> sx,
                cy >> sy,
                (cw as u32).div_ceil(1 << sx) as usize,
                (ch as u32).div_ceil(1 << sy) as usize,
                x >> sx,
                y >> sy,
            )
        };
        let start = py * sp.stride + px;
        // The conformance window lies inside the coded picture (the decoder
        // guarantees it), so these slices are in bounds; `get` keeps a
        // malformed one an error rather than a panic.
        let need = if ph == 0 {
            0
        } else {
            (ph - 1) * sp.stride + pw
        };
        let src = sp
            .data
            .get(start..start + need)
            .ok_or_else(|| Error::invalid("heif: conformance window outside the picture"))?;
        blit_plane(dp, pw as u32, ph as u32, src, sp.stride, ox, oy);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefix_split_and_truncation() {
        let d = [0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC];
        assert_eq!(
            length_prefixed(&d, 4).unwrap(),
            vec![&[0xAA, 0xBB][..], &[0xCC][..]]
        );
        assert!(length_prefixed(&d[..5], 4).is_err());
        assert!(length_prefixed(&d[..2], 4).is_err());
        assert!(hvcc_nals(&[1; 10]).is_err());
    }
}
