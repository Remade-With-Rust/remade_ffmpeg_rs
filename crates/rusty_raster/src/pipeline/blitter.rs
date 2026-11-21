// Copyright 2016 Google Inc.
// Copyright 2020 Yevhenii Reizner
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{BlendMode, Color, LengthU32, Paint, PixmapRef, PremultipliedColorU8, Shader};
use crate::{ALPHA_U8_OPAQUE, ALPHA_U8_TRANSPARENT};

use crate::alpha_runs::AlphaRun;
use crate::blitter::{Blitter, Mask};
use crate::color::AlphaU8;
use crate::geom::ScreenIntRect;
use crate::mask::SubMaskRef;
use crate::pipeline::{self, RasterPipeline, RasterPipelineBuilder};
use crate::pixmap::SubPixmapMut;

pub struct RasterPipelineBlitter<'a, 'b: 'a> {
    mask: Option<SubMaskRef<'a>>,
    pixmap_src: PixmapRef<'a>,
    pixmap: &'a mut SubPixmapMut<'b>,
    memset2d_color: Option<PremultipliedColorU8>,
    blit_anti_h_rp: RasterPipeline,
    blit_rect_rp: RasterPipeline,
    blit_mask_rp: RasterPipeline,
    is_mask: bool,
}

impl<'a, 'b: 'a> RasterPipelineBlitter<'a, 'b> {
    pub fn new(
        paint: &Paint<'a>,
        mask: Option<SubMaskRef<'a>>,
        pixmap: &'a mut SubPixmapMut<'b>,
    ) -> Option<Self> {
        // Make sure that `mask` has the same size as `pixmap`.
        if let Some(mask) = mask {
            if mask.size.width() != pixmap.size.width()
                || mask.size.height() != pixmap.size.height()
            {
                // tiny-skia logs "Pixmap and Mask are expected to have the same size" here.
                return None;
            }
        }

        // Fast-reject.
        // This is basically SkInterpretXfermode().
        match paint.blend_mode {
            // `Destination` keep the pixmap unchanged. Nothing to do here.
            BlendMode::Destination => return None,
            BlendMode::DestinationIn if paint.shader.is_opaque() && paint.is_solid_color() => {
                return None
            }
            _ => {}
        }

        // We can strength-reduce SourceOver into Source when opaque.
        let mut blend_mode = paint.blend_mode;
        if paint.shader.is_opaque() && blend_mode == BlendMode::SourceOver && mask.is_none() {
            blend_mode = BlendMode::Source;
        }

        // When we're drawing a constant color in Source mode, we can sometimes just memset.
        let mut memset2d_color = None;
        if paint.is_solid_color() && blend_mode == BlendMode::Source && mask.is_none() {
            // Unlike Skia, our shader cannot be constant.
            // Therefore there is no need to run a raster pipeline to get shader's color.
            if let Shader::SolidColor(ref color) = paint.shader {
                memset2d_color = Some(color.premultiply().to_color_u8());
            }
        };

        // Clear is just a transparent color memset.
        if blend_mode == BlendMode::Clear && !paint.anti_alias && mask.is_none() {
            blend_mode = BlendMode::Source;
            memset2d_color = Some(PremultipliedColorU8::TRANSPARENT);
        }

        let blit_anti_h_rp = {
            let mut p = RasterPipelineBuilder::new();
            p.set_force_hq_pipeline(paint.force_hq_pipeline);
            if !paint.shader.push_stages(paint.colorspace, &mut p) {
                return None;
            }

            if mask.is_some() {
                p.push(pipeline::Stage::MaskU8);
            }

            if blend_mode.should_pre_scale_coverage() {
                p.push(pipeline::Stage::Scale1Float);
                p.push(pipeline::Stage::LoadDestination);
                if let Some(stage) = paint.colorspace.expand_dest_stage() {
                    p.push(stage);
                }
                if let Some(blend_stage) = blend_mode.to_stage() {
                    p.push(blend_stage);
                }
            } else {
                p.push(pipeline::Stage::LoadDestination);
                if let Some(stage) = paint.colorspace.expand_dest_stage() {
                    p.push(stage);
                }
                if let Some(blend_stage) = blend_mode.to_stage() {
                    p.push(blend_stage);
                }

                p.push(pipeline::Stage::Lerp1Float);
            }

            if let Some(stage) = paint.colorspace.compress_stage() {
                p.push(stage);
            }
            p.push(pipeline::Stage::Store);

            p.compile()
        };

        let blit_rect_rp = {
            let mut p = RasterPipelineBuilder::new();
            p.set_force_hq_pipeline(paint.force_hq_pipeline);
            if !paint.shader.push_stages(paint.colorspace, &mut p) {
                return None;
            }

            if mask.is_some() {
                p.push(pipeline::Stage::MaskU8);
            }

            if blend_mode == BlendMode::SourceOver && mask.is_none() {
                if let Some(stage) = paint.colorspace.compress_stage() {
                    p.push(stage);
                }
                // TODO: ignore when dither_rate is non-zero
                p.push(pipeline::Stage::SourceOverRgba);
            } else {
                if blend_mode != BlendMode::Source {
                    p.push(pipeline::Stage::LoadDestination);
                    if let Some(blend_stage) = blend_mode.to_stage() {
                        if let Some(stage) = paint.colorspace.expand_dest_stage() {
                            p.push(stage);
                        }
                        p.push(blend_stage);
                    }
                }

                if let Some(stage) = paint.colorspace.compress_stage() {
                    p.push(stage);
                }
                p.push(pipeline::Stage::Store);
            }

            p.compile()
        };

        let blit_mask_rp = {
            let mut p = RasterPipelineBuilder::new();
            p.set_force_hq_pipeline(paint.force_hq_pipeline);
            if !paint.shader.push_stages(paint.colorspace, &mut p) {
                return None;
            }

            if mask.is_some() {
                p.push(pipeline::Stage::MaskU8);
            }

            if blend_mode.should_pre_scale_coverage() {
                p.push(pipeline::Stage::ScaleU8);
                p.push(pipeline::Stage::LoadDestination);
                if let Some(stage) = paint.colorspace.expand_dest_stage() {
                    p.push(stage);
                }
                if let Some(blend_stage) = blend_mode.to_stage() {
                    p.push(blend_stage);
                }
            } else {
                p.push(pipeline::Stage::LoadDestination);
                if let Some(stage) = paint.colorspace.expand_dest_stage() {
                    p.push(stage);
                }
                if let Some(blend_stage) = blend_mode.to_stage() {
                    p.push(blend_stage);
                }

                p.push(pipeline::Stage::LerpU8);
            }

            if let Some(stage) = paint.colorspace.compress_stage() {
                p.push(stage);
            }
            p.push(pipeline::Stage::Store);

            p.compile()
        };

        let pixmap_src = match paint.shader {
            Shader::Pattern(ref patt) => patt.pixmap,
            // Just a dummy one.
            _ => PixmapRef::from_bytes(&[0, 0, 0, 0], 1, 1).unwrap(),
        };

        Some(RasterPipelineBlitter {
            mask,
            pixmap_src,
            pixmap,
            memset2d_color,
            blit_anti_h_rp,
            blit_rect_rp,
            blit_mask_rp,
            is_mask: false,
        })
    }

    pub fn new_mask(pixmap: &'a mut SubPixmapMut<'b>) -> Option<Self> {
        let color = Color::WHITE.premultiply();

        let memset2d_color = Some(color.to_color_u8());

        let blit_anti_h_rp = {
            let mut p = RasterPipelineBuilder::new();
            p.push_uniform_color(color);
            p.push(pipeline::Stage::LoadDestinationU8);
            p.push(pipeline::Stage::Lerp1Float);
            p.push(pipeline::Stage::StoreU8);
            p.compile()
        };

        let blit_rect_rp = {
            let mut p = RasterPipelineBuilder::new();
            p.push_uniform_color(color);
            p.push(pipeline::Stage::StoreU8);
            p.compile()
        };

        let blit_mask_rp = {
            let mut p = RasterPipelineBuilder::new();
            p.push_uniform_color(color);
            p.push(pipeline::Stage::LoadDestinationU8);
            p.push(pipeline::Stage::LerpU8);
            p.push(pipeline::Stage::StoreU8);
            p.compile()
        };

        Some(RasterPipelineBlitter {
            mask: None,
            pixmap_src: PixmapRef::from_bytes(&[0, 0, 0, 0], 1, 1).unwrap(),
            pixmap,
            memset2d_color,
            blit_anti_h_rp,
            blit_rect_rp,
            blit_mask_rp,
            is_mask: true,
        })
    }
}

impl RasterPipelineBlitter<'_, '_> {
    /// Clips `rect` to the destination. Every scan converter is supposed to
    /// stay inside its clip; this guard is what turns a fixed-point overflow
    /// on hostile geometry into clipped output instead of an out-of-bounds
    /// panic (as in tiny-skia). For in-bounds rects it is the identity.
    #[inline]
    fn clip(&self, rect: &ScreenIntRect) -> Option<ScreenIntRect> {
        let w = self.pixmap.size.width();
        let h = self.pixmap.size.height();
        if rect.x() >= w || rect.y() >= h {
            return None;
        }
        let right = rect.x().saturating_add(rect.width()).min(w);
        let bottom = rect.y().saturating_add(rect.height()).min(h);
        if right == rect.right() && bottom == rect.bottom() {
            return Some(*rect);
        }
        ScreenIntRect::from_xywh(rect.x(), rect.y(), right - rect.x(), bottom - rect.y())
    }

    /// `blit_anti_h` into a coverage mask (`new_mask`), without the pipeline.
    ///
    /// The mask pipeline for a partial-coverage run is `uniform_color(white)`,
    /// `load_dst_u8`, `lerp_1_float`, `store_u8`: a lowp run of four stages
    /// over 16 lanes and four channels, for a run that is usually one pixel
    /// wide and a result that is one byte per pixel. Its arithmetic, on the
    /// one channel that is stored, is
    ///
    /// `c = (coverage * 255 + 0.5) as u16` with `coverage = alpha / 255`
    /// (both as `f32`, as `blit_anti_h` and `lerp_1_float` compute them), then
    /// `d' = div255(d * (255 - c) + 255 * c)` with `div255(v) = (v + 255) >> 8`,
    ///
    /// which is done here per byte. Every term stays below 2^16, so the
    /// pipeline's `u16` lanes never wrapped and this is the same integer.
    fn blit_anti_h_mask(&mut self, mut x: u32, y: u32, aa: &[AlphaU8], runs: &[AlphaRun]) {
        let mut offset = 0;
        let mut run_opt = runs.first().copied().flatten();
        while let Some(run) = run_opt {
            let width = LengthU32::from(run);
            let Some(&alpha) = aa.get(offset) else {
                break;
            };

            match alpha {
                ALPHA_U8_TRANSPARENT => {}
                ALPHA_U8_OPAQUE => {
                    self.blit_h(x, y, width);
                }
                alpha => {
                    let coverage = alpha as f32 * (1.0 / 255.0);
                    let c = u32::from((coverage * 255.0 + 0.5) as u16);
                    let rect =
                        ScreenIntRect::from_xywh(x, y, width.get(), 1).and_then(|r| self.clip(&r));
                    if let Some(rect) = rect {
                        let start = self.pixmap.offset(rect.x() as usize, rect.y() as usize);
                        let end = start + rect.width() as usize;
                        if let Some(row) = self.pixmap.data.get_mut(start..end) {
                            let add = 255 * c + 255;
                            let keep = 255 - c;
                            for d in row {
                                *d = ((u32::from(*d) * keep + add) >> 8) as u8;
                            }
                        }
                    }
                }
            }

            x = x.saturating_add(width.get());
            offset += usize::from(run.get());
            run_opt = runs.get(offset).copied().flatten();
        }
    }
}

impl Blitter for RasterPipelineBlitter<'_, '_> {
    fn blit_h(&mut self, x: u32, y: u32, width: LengthU32) {
        if let Some(r) = ScreenIntRect::from_xywh(x, y, width.get(), 1) {
            self.blit_rect(&r);
        }
    }

    fn blit_anti_h(&mut self, mut x: u32, y: u32, aa: &mut [AlphaU8], runs: &mut [AlphaRun]) {
        if self.is_mask {
            self.blit_anti_h_mask(x, y, aa, runs);
            return;
        }

        let mask_ctx = self.mask.map(|c| c.mask_ctx()).unwrap_or_default();

        let mut aa_offset = 0;
        let mut run_offset = 0;
        let mut run_opt = runs.first().copied().flatten();
        while let Some(run) = run_opt {
            let width = LengthU32::from(run);
            let Some(&alpha) = aa.get(aa_offset) else {
                break;
            };

            match alpha {
                ALPHA_U8_TRANSPARENT => {}
                ALPHA_U8_OPAQUE => {
                    self.blit_h(x, y, width);
                }
                alpha => {
                    self.blit_anti_h_rp.ctx.current_coverage = alpha as f32 * (1.0 / 255.0);

                    let rect =
                        ScreenIntRect::from_xywh(x, y, width.get(), 1).and_then(|r| self.clip(&r));
                    if let Some(rect) = rect {
                        self.blit_anti_h_rp.run(
                            &rect,
                            pipeline::AAMaskCtx::default(),
                            mask_ctx,
                            self.pixmap_src,
                            self.pixmap,
                        );
                    }
                }
            }

            x = x.saturating_add(width.get());
            run_offset += usize::from(run.get());
            aa_offset += usize::from(run.get());
            run_opt = runs.get(run_offset).copied().flatten();
        }
    }

    fn blit_v(&mut self, x: u32, y: u32, height: LengthU32, alpha: AlphaU8) {
        let Some(bounds) = ScreenIntRect::from_xywh(x, y, 1, height.get()) else {
            return;
        };

        let mask = Mask {
            image: [alpha, alpha],
            bounds,
            row_bytes: 0, // so we reuse the 1 "row" for all of height
        };

        self.blit_mask(&mask, &bounds);
    }

    fn blit_anti_h2(&mut self, x: u32, y: u32, alpha0: AlphaU8, alpha1: AlphaU8) {
        let Some(bounds) = ScreenIntRect::from_xywh(x, y, 2, 1) else {
            return;
        };

        let mask = Mask {
            image: [alpha0, alpha1],
            bounds,
            row_bytes: 2,
        };

        self.blit_mask(&mask, &bounds);
    }

    fn blit_anti_v2(&mut self, x: u32, y: u32, alpha0: AlphaU8, alpha1: AlphaU8) {
        let Some(bounds) = ScreenIntRect::from_xywh(x, y, 1, 2) else {
            return;
        };

        let mask = Mask {
            image: [alpha0, alpha1],
            bounds,
            row_bytes: 1,
        };

        self.blit_mask(&mask, &bounds);
    }

    fn blit_rect(&mut self, rect: &ScreenIntRect) {
        let Some(rect) = self.clip(rect) else {
            return;
        };
        let rect = &rect;
        if let Some(c) = self.memset2d_color {
            if self.is_mask {
                for y in 0..rect.height() {
                    let start = self
                        .pixmap
                        .offset(rect.x() as usize, (rect.y() + y) as usize);
                    let end = start + rect.width() as usize;
                    self.pixmap.data[start..end]
                        .iter_mut()
                        .for_each(|p| *p = c.alpha());
                }
            } else {
                for y in 0..rect.height() {
                    let start = self
                        .pixmap
                        .offset(rect.x() as usize, (rect.y() + y) as usize);
                    let end = start + rect.width() as usize;
                    let c = [c.red(), c.green(), c.blue(), c.alpha()];
                    self.pixmap.data[start * 4..end * 4]
                        .chunks_exact_mut(4)
                        .for_each(|p| p.copy_from_slice(&c));
                }
            }

            return;
        }

        let mask_ctx = self.mask.map(|c| c.mask_ctx()).unwrap_or_default();

        self.blit_rect_rp.run(
            rect,
            pipeline::AAMaskCtx::default(),
            mask_ctx,
            self.pixmap_src,
            self.pixmap,
        );
    }

    fn blit_mask(&mut self, mask: &Mask, clip: &ScreenIntRect) {
        let Some(clip) = self.clip(clip) else {
            return;
        };
        let clip = &clip;
        let aa_mask_ctx = pipeline::AAMaskCtx {
            pixels: mask.image,
            stride: mask.row_bytes,
            shift: (mask.bounds.left() + mask.bounds.top() * mask.row_bytes) as usize,
        };

        let mask_ctx = self.mask.map(|c| c.mask_ctx()).unwrap_or_default();

        self.blit_mask_rp
            .run(clip, aa_mask_ctx, mask_ctx, self.pixmap_src, self.pixmap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::IntSizeExt;
    use core::num::NonZeroU16;

    /// The pipeline `new_mask` builds for a partial-coverage run, run on one
    /// row: the oracle for `blit_anti_h_mask`.
    fn pipeline_lerp(row: &mut [u8], alpha: u8) {
        let mut mask = crate::Mask::from_vec(
            row.to_vec(),
            crate::IntSize::from_wh(row.len() as u32, 1).unwrap(),
        )
        .unwrap();
        let mut sub = mask.as_subpixmap();
        let mut p = RasterPipelineBuilder::new();
        p.push_uniform_color(Color::WHITE.premultiply());
        p.push(pipeline::Stage::LoadDestinationU8);
        p.push(pipeline::Stage::Lerp1Float);
        p.push(pipeline::Stage::StoreU8);
        let mut p = p.compile();
        p.ctx.current_coverage = alpha as f32 * (1.0 / 255.0);
        let rect = sub.size.to_screen_int_rect(0, 0);
        p.run(
            &rect,
            pipeline::AAMaskCtx::default(),
            pipeline::MaskCtx::default(),
            PixmapRef::from_bytes(&[0, 0, 0, 0], 1, 1).unwrap(),
            &mut sub,
        );
        row.copy_from_slice(mask.data());
    }

    /// One `blit_anti_h` on a mask row, all of it one run of `alpha`.
    fn blitter_row(row: &mut [u8], alpha: u8) {
        let width = row.len() as u32;
        let mut mask =
            crate::Mask::from_vec(row.to_vec(), crate::IntSize::from_wh(width, 1).unwrap())
                .unwrap();
        let mut sub = mask.as_subpixmap();
        let mut blitter = RasterPipelineBlitter::new_mask(&mut sub).unwrap();
        let mut aa = vec![0u8; row.len() + 1];
        let mut runs: Vec<AlphaRun> = vec![None; row.len() + 1];
        aa[0] = alpha;
        runs[0] = NonZeroU16::new(width as u16);
        blitter.blit_anti_h(0, 0, &mut aa, &mut runs);
        row.copy_from_slice(mask.data());
    }

    #[test]
    fn mask_anti_h_matches_pipeline() {
        // 256 destination bytes, plus a 16-pixel group boundary and a tail.
        let dest: Vec<u8> = (0..=255u8).chain(0..21).collect();
        for alpha in 0..=255u8 {
            let mut want = dest.clone();
            match alpha {
                0 => {}
                255 => want.iter_mut().for_each(|d| *d = 255),
                _ => pipeline_lerp(&mut want, alpha),
            }
            let mut got = dest.clone();
            blitter_row(&mut got, alpha);
            assert_eq!(got, want, "alpha {alpha}");
        }
    }
}
