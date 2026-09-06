//! Decoded pictures: planar Y/Cb/Cr sample storage. All bit depths use `u16`
//! samples for now (the 8-bit `u8` plane domain is a Phase 6 speed brick);
//! the output side hands out either width.

use std::sync::Arc;

use crate::pic::Motion;

/// One sample plane.
#[derive(Debug, Clone)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub data: Vec<u16>,
}

impl Plane {
    pub fn new(width: usize, height: usize) -> Self {
        Plane { width, height, stride: width, data: vec![0; width * height] }
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> u16 {
        self.data[y * self.stride + x]
    }

    #[inline]
    pub fn set(&mut self, x: usize, y: usize, v: u16) {
        self.data[y * self.stride + x] = v;
    }

    #[inline]
    pub fn row(&self, y: usize) -> &[u16] {
        &self.data[y * self.stride..y * self.stride + self.width]
    }

    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut [u16] {
        &mut self.data[y * self.stride..y * self.stride + self.width]
    }
}

/// A decoded picture, at the coded (uncropped) size.
#[derive(Debug, Clone)]
pub struct Picture {
    pub planes: [Plane; 3],
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    /// 0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub chroma_format_idc: u8,
    /// Crop rectangle in luma samples: left, top, width, height.
    pub crop: (usize, usize, usize, usize),
    pub poc: i32,
    /// Motion field compressed to 16×16 (§8.5.3.2.9 collocated motion), row-major.
    pub motion16: Vec<Motion>,
    /// Width of `motion16` in 16×16 units (0 = no motion field).
    pub motion16_w: usize,
}

impl Picture {
    pub fn new(width: usize, height: usize, chroma_format_idc: u8, bit_depth_luma: u8, bit_depth_chroma: u8) -> Self {
        let (cw, ch) = match chroma_format_idc {
            0 => (0, 0),
            1 => (width.div_ceil(2), height.div_ceil(2)),
            2 => (width.div_ceil(2), height),
            _ => (width, height),
        };
        Picture {
            planes: [Plane::new(width, height), Plane::new(cw, ch), Plane::new(cw, ch)],
            bit_depth_luma,
            bit_depth_chroma,
            chroma_format_idc,
            crop: (0, 0, width, height),
            poc: 0,
            motion16: Vec::new(),
            motion16_w: 0,
        }
    }

    pub fn width(&self) -> usize {
        self.planes[0].width
    }

    pub fn height(&self) -> usize {
        self.planes[0].height
    }
}

/// An output frame: a shared decoded picture plus its presentation metadata.
#[derive(Debug, Clone)]
pub struct Frame {
    pub picture: Arc<Picture>,
    pub poc: i32,
    /// The `pts` handed in with the access unit that produced this picture, if any.
    pub pts: Option<i64>,
    /// Cropped output geometry in luma samples.
    pub width: usize,
    pub height: usize,
}

impl Frame {
    pub fn bit_depth(&self) -> u8 {
        self.picture.bit_depth_luma
    }

    /// Writes the cropped picture as planar YUV: `u8` samples for 8-bit
    /// content, little-endian `u16` otherwise (the conformance md5 layout).
    pub fn write_yuv(&self, out: &mut Vec<u8>) {
        let p = &self.picture;
        let (cx, cy, cw, ch) = p.crop;
        let (sw, shh) = match p.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let wide = p.bit_depth_luma > 8 || p.bit_depth_chroma > 8;
        let mut put = |plane: &Plane, x0: usize, y0: usize, w: usize, h: usize| {
            for y in y0..y0 + h {
                let row = &plane.data[y * plane.stride + x0..y * plane.stride + x0 + w];
                if wide {
                    for &v in row {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                } else {
                    out.extend(row.iter().map(|&v| v as u8));
                }
            }
        };
        put(&p.planes[0], cx, cy, cw, ch);
        if p.chroma_format_idc != 0 {
            put(&p.planes[1], cx / sw, cy / shh, cw / sw, ch / shh);
            put(&p.planes[2], cx / sw, cy / shh, cw / sw, ch / shh);
        }
    }
}
