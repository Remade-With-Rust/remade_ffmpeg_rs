//! A [`FontProvider`] over font files, with `fontdb` (selection), `harfrust`
//! (shaping) and `skrifa` (outlines, metrics).
//!
//! The house has no font stack yet (thoth#6). This module is the one place
//! that uses a third-party one, and only under the `fonts` feature, which is
//! off by default. The selection, metric and colour-glyph rules are derived
//! from usvg 0.48.1 `text/` (Copyright the Resvg Authors, Apache-2.0 OR MIT;
//! see `NOTICE.md`).

use fontdb::{Database, Family, Query, ID};
use harfrust::{Feature, ShapeOptions, ShaperData, ShaperInstance, UnicodeBuffer, Variation};
use skrifa::bitmap::{BitmapData, BitmapFormat};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::prelude::{LocationRef, Size};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, MetadataProvider, Tag};

use crate::text::*;

const OPSZ: Tag = Tag::new(b"opsz");

/// Fonts loaded by the application. Documents cannot load any.
#[derive(Clone, Debug, Default)]
pub struct Fonts {
    db: Database,
}

impl Fonts {
    /// No fonts.
    pub fn new() -> Fonts {
        Fonts::default()
    }

    /// Loads the fonts installed on the system.
    pub fn load_system_fonts(&mut self) {
        self.db.load_system_fonts();
    }

    /// Loads every font file in a directory, recursively.
    pub fn load_fonts_dir(&mut self, dir: impl AsRef<std::path::Path>) {
        self.db.load_fonts_dir(dir);
    }

    /// Loads a font file's data.
    pub fn load_font_data(&mut self, data: Vec<u8>) {
        self.db.load_font_data(data);
    }

    /// The family a generic name stands for. `FontFamily::Named` is ignored.
    pub fn set_generic_family(&mut self, generic: &FontFamily, family: &str) {
        match generic {
            FontFamily::Serif => self.db.set_serif_family(family),
            FontFamily::SansSerif => self.db.set_sans_serif_family(family),
            FontFamily::Cursive => self.db.set_cursive_family(family),
            FontFamily::Fantasy => self.db.set_fantasy_family(family),
            FontFamily::Monospace => self.db.set_monospace_family(family),
            FontFamily::Named(_) => {}
        }
    }

    /// How many font faces there are.
    pub fn len(&self) -> usize {
        self.db.len()
    }

    /// Whether there is no font.
    pub fn is_empty(&self) -> bool {
        self.db.is_empty()
    }

    fn ids(&self) -> impl Iterator<Item = ID> + '_ {
        self.db.faces().map(|f| f.id)
    }

    /// Our ids are positions in the database, which is never removed from.
    fn find(&self, id: FontId) -> Option<ID> {
        let n = usize::try_from(id.0).ok()?;
        self.ids().nth(n)
    }

    fn id_of(&self, id: ID) -> Option<FontId> {
        self.ids().position(|i| i == id).map(|n| FontId(n as u64))
    }

    fn with<T>(&self, id: FontId, f: impl FnOnce(&[u8], u32) -> Option<T>) -> Option<T> {
        self.db.with_face_data(self.find(id)?, f)?
    }
}

fn axis_settings(variations: &[FontVariation]) -> impl Iterator<Item = (Tag, f32)> + '_ {
    variations
        .iter()
        .map(|v| (Tag::from_be_bytes(v.tag), v.value))
}

struct Pen(rusty_raster::PathBuilder);

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, cy0, x, y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.cubic_to(cx0, cy0, cx1, cy1, x, y);
    }

    fn close(&mut self) {
        self.0.close();
    }
}

impl FontProvider for Fonts {
    fn select(&self, font: &Font) -> Option<FontId> {
        let mut families: Vec<Family> = font
            .families
            .iter()
            .map(|f| match f {
                FontFamily::Serif => Family::Serif,
                FontFamily::SansSerif => Family::SansSerif,
                FontFamily::Cursive => Family::Cursive,
                FontFamily::Fantasy => Family::Fantasy,
                FontFamily::Monospace => Family::Monospace,
                FontFamily::Named(s) => Family::Name(s),
            })
            .collect();
        families.push(Family::Serif);
        let id = self.db.query(&Query {
            families: &families,
            weight: fontdb::Weight(font.weight),
            stretch: match font.stretch {
                FontStretch::UltraCondensed => fontdb::Stretch::UltraCondensed,
                FontStretch::ExtraCondensed => fontdb::Stretch::ExtraCondensed,
                FontStretch::Condensed => fontdb::Stretch::Condensed,
                FontStretch::SemiCondensed => fontdb::Stretch::SemiCondensed,
                FontStretch::Normal => fontdb::Stretch::Normal,
                FontStretch::SemiExpanded => fontdb::Stretch::SemiExpanded,
                FontStretch::Expanded => fontdb::Stretch::Expanded,
                FontStretch::ExtraExpanded => fontdb::Stretch::ExtraExpanded,
                FontStretch::UltraExpanded => fontdb::Stretch::UltraExpanded,
            },
            style: match font.style {
                FontStyle::Normal => fontdb::Style::Normal,
                FontStyle::Italic => fontdb::Style::Italic,
                FontStyle::Oblique => fontdb::Style::Oblique,
            },
        })?;
        self.id_of(id)
    }

    fn fallback(&self, c: char, used: &[FontId]) -> Option<FontId> {
        let base = self.db.face(self.find(*used.first()?)?)?;
        for (n, face) in self.db.faces().enumerate() {
            let id = FontId(n as u64);
            if used.contains(&id)
                || (base.style != face.style
                    && base.weight != face.weight
                    && base.stretch != face.stretch)
            {
                continue;
            }
            let has = self.db.with_face_data(face.id, |data, index| {
                let font = FontRef::from_index(data, index).ok()?;
                font.charmap().map(c)
            });
            if matches!(has, Some(Some(_))) {
                return Some(id);
            }
        }
        None
    }

    fn metrics(&self, font: FontId, variations: &[FontVariation]) -> Option<FontMetrics> {
        self.with(font, |data, index| {
            let font = FontRef::from_index(data, index).ok()?;
            let location = font.axes().location(axis_settings(variations));
            let m = font.metrics(Size::unscaled(), &location);
            let em = m.units_per_em;
            if !(16..=16384).contains(&em) {
                return None;
            }
            let positive = |v: f32| u16::try_from(v.round() as i32).ok().filter(|v| *v != 0);
            let x_height = match m.x_height.filter(|x| *x > 0.0).and_then(positive) {
                Some(h) => h,
                // As Firefox.
                None => positive((m.ascent - m.descent) * 0.45)?,
            };
            let thickness = em / 12;
            let (underline_position, underline_thickness) = match m.underline {
                Some(u) => (
                    u.offset.round() as i16,
                    positive(u.thickness).unwrap_or(thickness),
                ),
                None => (-(em as i16) / 9, thickness),
            };
            // Generic offsets, where the font does not have its own.
            let mut subscript_offset = (f32::from(em) / 0.2).round() as i16;
            let mut superscript_offset = (f32::from(em) / 0.4).round() as i16;
            if let Ok(os2) = font.os2() {
                subscript_offset = os2.y_subscript_y_offset();
                superscript_offset = os2.y_superscript_y_offset();
            }
            let coords = location.coords();
            if let (Ok(mvar), false) = (font.mvar(), coords.is_empty()) {
                use skrifa::raw::tables::mvar::tags::{SBYO, SPYO};
                let delta = |tag| {
                    let d = mvar.metric_delta(tag, coords).unwrap_or_default();
                    d.to_f32().round() as i16
                };
                subscript_offset = subscript_offset.saturating_add(delta(SBYO));
                superscript_offset = superscript_offset.saturating_add(delta(SPYO));
            }
            Some(FontMetrics {
                units_per_em: em,
                ascent: m.ascent.round() as i16,
                descent: m.descent.round() as i16,
                x_height,
                underline_position,
                underline_thickness,
                line_through_position: match m.strikeout {
                    Some(s) => s.offset.round() as i16,
                    None => x_height as i16 / 2,
                },
                subscript_offset,
                superscript_offset,
            })
        })
    }

    fn shape(&self, font: FontId, r: &ShapeRequest) -> Vec<ShapedGlyph> {
        self.with(font, |data, index| {
            let font = harfrust::FontRef::from_index(data, index).ok()?;
            let mut variations: Vec<Variation> = r
                .variations
                .iter()
                .map(|v| Variation {
                    tag: harfrust::Tag::from_be_bytes(v.tag),
                    value: v.value,
                })
                .collect();
            if r.optical_sizing
                && !r.variations.iter().any(|v| &v.tag == b"opsz")
                && font.axes().get_by_tag(OPSZ).is_some()
            {
                variations.push(Variation {
                    tag: OPSZ,
                    value: r.font_size,
                });
            }
            let instance = (!variations.is_empty())
                .then(|| ShaperInstance::from_variations(&font, &variations));
            let shaper_data = ShaperData::new(&font);
            let shaper = shaper_data
                .shaper(&font)
                .instance(instance.as_ref())
                .build();
            let mut features = Vec::new();
            if r.small_caps {
                features.push(Feature::new(harfrust::Tag::new(b"smcp"), 1, ..));
            }
            if !r.kerning {
                features.push(Feature::new(harfrust::Tag::new(b"kern"), 0, ..));
            }
            let mut buffer = UnicodeBuffer::new();
            buffer.push_str(r.text);
            buffer.set_direction(if r.rtl {
                harfrust::Direction::RightToLeft
            } else {
                harfrust::Direction::LeftToRight
            });
            buffer.guess_segment_properties();
            let out = shaper.shape(buffer, ShapeOptions::new().features(&features));
            let glyphs = out
                .glyph_infos()
                .iter()
                .zip(out.glyph_positions())
                .map(|(i, p)| ShapedGlyph {
                    glyph: i.glyph_id,
                    cluster: i.cluster as usize,
                    dx: p.x_offset,
                    dy: p.y_offset,
                    advance: p.x_advance,
                })
                .collect();
            Some(glyphs)
        })
        .unwrap_or_default()
    }

    fn glyph(
        &self,
        font: FontId,
        glyph: u32,
        variations: &[FontVariation],
        optical_size: Option<f32>,
    ) -> Option<GlyphData> {
        self.with(font, |data, index| {
            let font = FontRef::from_index(data, index).ok()?;
            let id = skrifa::GlyphId::new(glyph);
            let mut variations = variations.to_vec();
            if let Some(size) = optical_size {
                if !variations.iter().any(|v| &v.tag == b"opsz")
                    && font.axes().get_by_tag(OPSZ).is_some()
                {
                    variations.push(FontVariation {
                        tag: *b"opsz",
                        value: size,
                    });
                }
            }
            let location = font.axes().location(axis_settings(&variations));
            if let Some(g) = font.color_glyphs().get(id) {
                if let Some(data) = crate::colr::paint(&font, &g, LocationRef::from(&location)) {
                    return Some(GlyphData::Svg {
                        data,
                        id: None,
                        y_up: true,
                    });
                }
            }
            if let Some(svg) = svg_glyph(&font, id) {
                return Some(svg);
            }
            if let Some(b) = bitmap_glyph(&font, id) {
                return Some(GlyphData::Png(b));
            }
            let outline = font.outline_glyphs().get(id)?;
            let mut pen = Pen(rusty_raster::PathBuilder::new());
            outline
                .draw(
                    DrawSettings::unhinted(Size::unscaled(), &location),
                    &mut pen,
                )
                .ok()?;
            pen.0.finish().map(GlyphData::Outline)
        })
    }
}

/// A glyph of the `SVG` table. Its document may hold several.
fn svg_glyph(font: &FontRef, id: skrifa::GlyphId) -> Option<GlyphData> {
    let table = font.svg().ok()?;
    let data = table.glyph_data(id).ok()??.to_vec();
    let list = table.svg_document_list().ok()?;
    let record = list.document_records().iter().find(|r| {
        (r.start_glyph_id.get().to_u32()..=r.end_glyph_id.get().to_u32()).contains(&id.to_u32())
    })?;
    let shared = record.start_glyph_id != record.end_glyph_id;
    Some(GlyphData::Svg {
        data,
        id: shared.then(|| format!("glyph{}", id.to_u32())),
        y_up: false,
    })
}

/// The largest PNG bitmap (`sbix`, `CBDT`).
fn bitmap_glyph(font: &FontRef, id: skrifa::GlyphId) -> Option<GlyphBitmap> {
    let strikes = font.bitmap_strikes();
    let image = strikes.glyph_for_size(Size::unscaled(), id)?;
    let BitmapData::Png(data) = image.data else {
        return None;
    };
    let sbix = strikes.format() == Some(BitmapFormat::Sbix);
    let origin = || {
        let m = font.glyph_metrics(Size::unscaled(), LocationRef::default());
        m.bounds(id).map_or((0.0, 0.0), |b| {
            (f32::from(b.x_min as i16), f32::from(b.y_min as i16))
        })
    };
    Some(GlyphBitmap {
        data: data.to_vec(),
        width: image.width,
        height: image.height,
        x: f32::from(image.inner_bearing_x as i16),
        y: f32::from(image.inner_bearing_y as i16),
        pixels_per_em: f32::from(image.ppem_x as u16),
        sbix_origin: sbix.then(origin),
    })
}
