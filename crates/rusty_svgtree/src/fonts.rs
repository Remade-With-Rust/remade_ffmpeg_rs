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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::text::*;

const OPSZ: Tag = Tag::new(b"opsz");

/// Fonts loaded by the application. Documents cannot load any.
///
/// A face's [`FontId`] is its position in load order, so it stays the same
/// for as long as the `Fonts` lives (faces are never removed), and a clone
/// gives the same ids.
///
/// An application with its own font matching (names in any case, PostScript
/// or legacy names, faces added when a document first asks for them) sets
/// [`Fonts::set_select`] and [`Fonts::set_fallback`]: they are called in
/// place of the built-in matching, with a [`FontsDb`] that can run it, list
/// the faces and add new ones.
#[derive(Default)]
pub struct Fonts {
    /// Behind a lock so that a callback can add faces while text is laid
    /// out (layout holds `&Fonts`).
    db: RwLock<Database>,
    /// What [`FontProvider::font_data`] gave, so that a font file is read
    /// once and every call shares its bytes.
    data: Mutex<HashMap<FontId, FontData>>,
    select: Option<Arc<SelectFn>>,
    fallback: Option<Arc<FallbackFn>>,
}

/// A [`Fonts::set_select`] callback.
pub type SelectFn = dyn Fn(&Font, &mut FontsDb) -> Option<FontId> + Send + Sync;

/// A [`Fonts::set_fallback`] callback.
pub type FallbackFn = dyn Fn(char, &[FontId], &mut FontsDb) -> Option<FontId> + Send + Sync;

impl Clone for Fonts {
    fn clone(&self) -> Fonts {
        Fonts {
            db: RwLock::new(read(&self.db).clone()),
            data: Mutex::new(lock(&self.data).clone()),
            select: self.select.clone(),
            fallback: self.fallback.clone(),
        }
    }
}

impl std::fmt::Debug for Fonts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fonts")
            .field("faces", &self.len())
            .field("select", &self.select.is_some())
            .field("fallback", &self.fallback.is_some())
            .finish()
    }
}

/// A poisoned lock still holds a usable value (every change is made whole).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The font database, as a [`Fonts::set_select`] or [`Fonts::set_fallback`]
/// callback sees it: the built-in matching, the faces, and adding faces.
pub struct FontsDb<'a> {
    db: &'a mut Database,
}

/// What a [`FontsDb`] knows about a face.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FaceInfo<'a> {
    /// Its id.
    pub id: FontId,
    /// Its family names (the typographic family, else the family), in every
    /// language the font has them in; the first is the US English one.
    pub families: Vec<&'a str>,
    /// Its PostScript name.
    pub post_script_name: &'a str,
    /// Its weight, 1 to 1000.
    pub weight: u16,
    /// Its style.
    pub style: FontStyle,
    /// Its width.
    pub stretch: FontStretch,
    /// Whether it is monospaced.
    pub monospaced: bool,
    /// The face's index in its file.
    pub index: u32,
    /// The file it was loaded from, if it was loaded from one.
    pub path: Option<&'a std::path::Path>,
}

impl FontsDb<'_> {
    /// The built-in matching: what [`Fonts`] selects without a callback.
    pub fn query(&self, font: &Font) -> Option<FontId> {
        query(self.db, font)
    }

    /// The built-in fallback.
    pub fn default_fallback(&self, c: char, used: &[FontId]) -> Option<FontId> {
        fallback(self.db, c, used)
    }

    /// Every face, in id order.
    pub fn faces(&self) -> impl Iterator<Item = FaceInfo<'_>> + '_ {
        self.db.faces().enumerate().map(|(n, f)| face_info(n, f))
    }

    /// One face.
    pub fn face(&self, id: FontId) -> Option<FaceInfo<'_>> {
        let n = usize::try_from(id.0).ok()?;
        self.db.faces().nth(n).map(|f| face_info(n, f))
    }

    /// Whether a face maps `c` to a glyph.
    pub fn has_char(&self, id: FontId, c: char) -> bool {
        let Some(id) = find(self.db, id) else {
            return false;
        };
        let has = self.db.with_face_data(id, |data, index| {
            let font = FontRef::from_index(data, index).ok()?;
            font.charmap().map(c)
        });
        matches!(has, Some(Some(_)))
    }

    /// Calls `f` with a face's file and its index in it.
    pub fn with_face_data<T>(&self, id: FontId, f: impl FnOnce(&[u8], u32) -> T) -> Option<T> {
        self.db.with_face_data(find(self.db, id)?, f)
    }

    /// Adds the faces of a font file held in memory, without a copy; returns
    /// their ids (none if it is not a font).
    pub fn add_shared(&mut self, data: Arc<dyn AsRef<[u8]> + Send + Sync>) -> Vec<FontId> {
        let ids = self.db.load_font_source(fontdb::Source::Binary(data));
        ids.iter().filter_map(|i| id_of(self.db, *i)).collect()
    }

    /// Adds the faces of a font file, read when they are first used; returns
    /// their ids (none if it is not a font).
    pub fn add_file(&mut self, path: impl Into<std::path::PathBuf>) -> Vec<FontId> {
        let ids = self.db.load_font_source(fontdb::Source::File(path.into()));
        ids.iter().filter_map(|i| id_of(self.db, *i)).collect()
    }
}

fn face_info(n: usize, f: &fontdb::FaceInfo) -> FaceInfo<'_> {
    FaceInfo {
        id: FontId(n as u64),
        families: f.families.iter().map(|(name, _)| name.as_str()).collect(),
        post_script_name: &f.post_script_name,
        weight: f.weight.0,
        style: match f.style {
            fontdb::Style::Normal => FontStyle::Normal,
            fontdb::Style::Italic => FontStyle::Italic,
            fontdb::Style::Oblique => FontStyle::Oblique,
        },
        stretch: match f.stretch {
            fontdb::Stretch::UltraCondensed => FontStretch::UltraCondensed,
            fontdb::Stretch::ExtraCondensed => FontStretch::ExtraCondensed,
            fontdb::Stretch::Condensed => FontStretch::Condensed,
            fontdb::Stretch::SemiCondensed => FontStretch::SemiCondensed,
            fontdb::Stretch::Normal => FontStretch::Normal,
            fontdb::Stretch::SemiExpanded => FontStretch::SemiExpanded,
            fontdb::Stretch::Expanded => FontStretch::Expanded,
            fontdb::Stretch::ExtraExpanded => FontStretch::ExtraExpanded,
            fontdb::Stretch::UltraExpanded => FontStretch::UltraExpanded,
        },
        monospaced: f.monospaced,
        index: f.index,
        path: match &f.source {
            fontdb::Source::File(p) => Some(p.as_path()),
            #[allow(unreachable_patterns)]
            _ => None,
        },
    }
}

/// Our ids are positions in the database, which is never removed from.
fn find(db: &Database, id: FontId) -> Option<ID> {
    let n = usize::try_from(id.0).ok()?;
    db.faces().nth(n).map(|f| f.id)
}

fn id_of(db: &Database, id: ID) -> Option<FontId> {
    db.faces()
        .position(|f| f.id == id)
        .map(|n| FontId(n as u64))
}

impl Fonts {
    /// No fonts.
    pub fn new() -> Fonts {
        Fonts::default()
    }

    /// Loads the fonts installed on the system.
    pub fn load_system_fonts(&mut self) {
        self.db_mut().load_system_fonts();
    }

    /// Loads every font file in a directory, recursively.
    pub fn load_fonts_dir(&mut self, dir: impl AsRef<std::path::Path>) {
        self.db_mut().load_fonts_dir(dir);
    }

    /// Loads a font file's data.
    pub fn load_font_data(&mut self, data: Vec<u8>) {
        self.db_mut().load_font_data(data);
    }

    /// Loads a font file held in memory, without a copy; returns the ids of
    /// its faces.
    pub fn load_shared_font_data(
        &mut self,
        data: Arc<dyn AsRef<[u8]> + Send + Sync>,
    ) -> Vec<FontId> {
        self.db().add_shared(data)
    }

    /// The family a generic name stands for. `FontFamily::Named` is ignored.
    pub fn set_generic_family(&mut self, generic: &FontFamily, family: &str) {
        let db = self.db_mut();
        match generic {
            FontFamily::Serif => db.set_serif_family(family),
            FontFamily::SansSerif => db.set_sans_serif_family(family),
            FontFamily::Cursive => db.set_cursive_family(family),
            FontFamily::Fantasy => db.set_fantasy_family(family),
            FontFamily::Monospace => db.set_monospace_family(family),
            FontFamily::Named(_) => {}
        }
    }

    /// Selects fonts with `f` instead of the built-in matching (which `f`
    /// can still call, as [`FontsDb::query`]). `f` may add faces; what it
    /// returns is the font of the request, or `None` for none.
    pub fn set_select(
        &mut self,
        f: impl Fn(&Font, &mut FontsDb) -> Option<FontId> + Send + Sync + 'static,
    ) {
        self.select = Some(Arc::new(f));
    }

    /// Finds fallback fonts with `f` instead of the built-in rule (which `f`
    /// can still call, as [`FontsDb::default_fallback`]): a font that has
    /// the character and is not in `used`, closest to the first of them.
    pub fn set_fallback(
        &mut self,
        f: impl Fn(char, &[FontId], &mut FontsDb) -> Option<FontId> + Send + Sync + 'static,
    ) {
        self.fallback = Some(Arc::new(f));
    }

    /// The database, to list or add faces outside layout.
    pub fn db(&mut self) -> FontsDb<'_> {
        FontsDb { db: self.db_mut() }
    }

    fn db_mut(&mut self) -> &mut Database {
        self.db
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many font faces there are.
    pub fn len(&self) -> usize {
        read(&self.db).len()
    }

    /// Whether there is no font.
    pub fn is_empty(&self) -> bool {
        read(&self.db).is_empty()
    }

    fn with<T>(&self, id: FontId, f: impl FnOnce(&[u8], u32) -> Option<T>) -> Option<T> {
        let db = read(&self.db);
        db.with_face_data(find(&db, id)?, f)?
    }
}

/// The built-in selection.
fn query(db: &Database, font: &Font) -> Option<FontId> {
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
    let id = db.query(&Query {
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
    id_of(db, id)
}

/// The built-in fallback.
fn fallback(db: &Database, c: char, used: &[FontId]) -> Option<FontId> {
    let base = db.face(find(db, *used.first()?)?)?;
    for (n, face) in db.faces().enumerate() {
        let id = FontId(n as u64);
        if used.contains(&id)
            || (base.style != face.style
                && base.weight != face.weight
                && base.stretch != face.stretch)
        {
            continue;
        }
        let has = db.with_face_data(face.id, |data, index| {
            let font = FontRef::from_index(data, index).ok()?;
            font.charmap().map(c)
        });
        if matches!(has, Some(Some(_))) {
            return Some(id);
        }
    }
    None
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
        match &self.select {
            Some(f) => f(
                font,
                &mut FontsDb {
                    db: &mut write(&self.db),
                },
            ),
            None => query(&read(&self.db), font),
        }
    }

    fn fallback(&self, c: char, used: &[FontId]) -> Option<FontId> {
        match &self.fallback {
            Some(f) => f(
                c,
                used,
                &mut FontsDb {
                    db: &mut write(&self.db),
                },
            ),
            None => fallback(&read(&self.db), c, used),
        }
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

    /// A face loaded from bytes shares them; one loaded from a file is read
    /// on the first call, and later calls share that copy.
    fn font_data(&self, font: FontId) -> Option<FontData> {
        if let Some(d) = lock(&self.data).get(&font) {
            return Some(d.clone());
        }
        let db = read(&self.db);
        let id = find(&db, font)?;
        let (source, index) = db.face_source(id)?;
        let bytes: Arc<dyn AsRef<[u8]> + Send + Sync> = match source {
            fontdb::Source::Binary(b) => b,
            _ => Arc::new(db.with_face_data(id, |data, _| data.to_vec())?),
        };
        drop(db);
        let d = FontData::new(bytes, index);
        lock(&self.data).insert(font, d.clone());
        Some(d)
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
