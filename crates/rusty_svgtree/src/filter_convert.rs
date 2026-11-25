//! `filter` attributes and `<filter>` elements to [`Filter`] nodes.
//!
//! Derived from usvg 0.48.1 `parser/filter.rs` (Copyright 2022 the Resvg
//! Authors, Apache-2.0 OR MIT; see `NOTICE.md`), reworked for this crate's
//! document and bounded: cycles through `feImage` are broken, invalid
//! numbers clamp, and every element converted is charged to the conversion
//! budget.

use std::collections::HashSet;
use std::sync::Arc;

use rusty_raster::{BlendMode, NonZeroRect};

use crate::convert::{
    approx_zero, bbox_transform, convert_element, convert_length, current_color, image_quality,
    parse_blend_mode, place_image, resolve_number, split, units_attr, Cache, State, Units,
    MAX_REF_DEPTH,
};
use crate::filter::*;
use crate::svgtree::{AId, EId, SvgNode};
use crate::tree::{Color, Group};
use crate::values::{self, FilterFunc, Length, Unit};

const BLACK: Color = Color {
    red: 0,
    green: 0,
    blue: 0,
};
const WHITE: Color = Color {
    red: 255,
    green: 255,
    blue: 255,
};

/// `v` if it is finite and not negative.
fn positive(v: f32) -> Option<f32> {
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// `val` clamped to `min..=max`; NaN becomes `min`.
fn bound(min: f32, val: f32, max: f32) -> f32 {
    if val > max {
        max
    } else if val >= min {
        val
    } else {
        min
    }
}

impl SvgNode<'_> {
    fn number_attr(&self, name: AId) -> Option<f32> {
        values::number(self.attribute(name)?).map(|v| v as f32)
    }

    fn numbers_attr(&self, name: AId) -> Option<Vec<f32>> {
        let (list, ok) = values::number_list(self.attribute(name)?);
        ok.then(|| list.into_iter().map(|v| v as f32).collect())
    }
}

/// A `filter` attribute whose references are all invalid: the element must
/// not be drawn.
pub(crate) struct InvalidFilter;

/// Resolves a node's `filter` attribute.
pub(crate) fn convert(
    node: SvgNode,
    state: &State,
    object_bbox: Option<NonZeroRect>,
    cache: &mut Cache,
) -> Result<Vec<Arc<Filter>>, InvalidFilter> {
    let Some(value) = node.attribute(AId::Filter) else {
        return Ok(Vec::new());
    };
    // A malformed list is ignored as a whole from its first error on.
    let (funcs, ok) = values::filter_functions(value);
    let mut filters = Vec::new();
    let mut has_invalid_urls = false;
    for func in funcs {
        match func {
            FilterFunc::Url(id) => {
                let link = node.document().element_by_id(id);
                match link.and_then(|l| convert_url(l, state, object_bbox, cache)) {
                    Some(f) => filters.push(f),
                    None => has_invalid_urls = true,
                }
            }
            func => filters.extend(convert_function_filter(node, &func, state, object_bbox)),
        }
    }
    if !ok {
        return Ok(Vec::new());
    }
    if filters.is_empty() && has_invalid_urls {
        return Err(InvalidFilter);
    }
    Ok(filters)
}

fn convert_function_filter(
    node: SvgNode,
    func: &FilterFunc,
    state: &State,
    object_bbox: Option<NonZeroRect>,
) -> Option<Arc<Filter>> {
    // A filter function has no region of its own; as usvg, a fairly large
    // one is used.
    let rect = match func {
        FilterFunc::Blur(_) | FilterFunc::DropShadow { .. } => {
            NonZeroRect::from_xywh(-0.5, -0.5, 2.0, 2.0)
        }
        _ => NonZeroRect::from_xywh(-0.1, -0.1, 1.2, 1.2),
    }?;
    let rect = bbox_transform(rect, object_bbox?)?;
    Some(Arc::new(Filter {
        id: String::new(),
        rect,
        is_function: true,
        primitives: vec![Primitive {
            rect,
            // Functions, unlike elements, work in sRGB.
            color_interpolation: ColorInterpolation::SRgb,
            result: "result".to_string(),
            kind: convert_function(node, func, state),
        }],
    }))
}

fn convert_url(
    node: SvgNode,
    state: &State,
    object_bbox: Option<NonZeroRect>,
    cache: &mut Cache,
) -> Option<Arc<Filter>> {
    let units = units_attr(node, AId::FilterUnits, Units::ObjectBoundingBox);
    let primitive_units = units_attr(node, AId::PrimitiveUnits, Units::UserSpaceOnUse);
    let cacheable = units == Units::UserSpaceOnUse && primitive_units == Units::UserSpaceOnUse;
    if cacheable {
        if let Some(f) = cache.filters.get(&node.id()) {
            return Some(f.clone());
        }
    }
    if node.tag_name() != Some(EId::Filter)
        || cache.active.contains(&node.id())
        || cache.active.len() >= MAX_REF_DEPTH
    {
        return None;
    }
    let percent = |v: f64| Length::new(v, Unit::Percent);
    let mut rect = NonZeroRect::from_xywh(
        resolve_number(node, AId::X, units, state, percent(-10.0)),
        resolve_number(node, AId::Y, units, state, percent(-10.0)),
        resolve_number(node, AId::Width, units, state, percent(120.0)),
        resolve_number(node, AId::Height, units, state, percent(120.0)),
    )?;
    if units == Units::ObjectBoundingBox {
        rect = bbox_transform(rect, object_bbox?)?;
    }
    let source = node
        .href_iter()
        .take_while(|l| l.tag_name() == Some(EId::Filter))
        .find(|l| l.has_children())?;
    cache.active.push(node.id());
    let primitives = convert_primitives(source, primitive_units, state, object_bbox, rect, cache);
    cache.active.pop();
    if primitives.is_empty() {
        return None;
    }
    let filter = Arc::new(Filter {
        id: node.element_id().to_string(),
        rect,
        is_function: false,
        primitives,
    });
    if cacheable {
        cache.filters.insert(node.id(), filter.clone());
    }
    Some(filter)
}

/// The primitives converted so far, with their `result` names.
struct Prims {
    list: Vec<Primitive>,
    names: HashSet<String>,
    /// The names the document gave (a generated one must not repeat them).
    given: HashSet<String>,
    next_index: usize,
}

impl Prims {
    /// The input an attribute names: an unknown reference falls back to the
    /// previous primitive, and so does a missing attribute.
    fn input(&self, node: SvgNode, name: AId) -> Input {
        match node.attribute(name) {
            Some("SourceAlpha") => return Input::SourceAlpha,
            // The background and paint inputs are not supported.
            Some(
                "SourceGraphic" | "BackgroundImage" | "BackgroundAlpha" | "FillPaint"
                | "StrokePaint",
            ) => return Input::SourceGraphic,
            Some(s) if self.names.contains(s) => return Input::Reference(s.to_string()),
            _ => {}
        }
        match self.list.last() {
            Some(prev) => Input::Reference(prev.result.clone()),
            None => Input::SourceGraphic,
        }
    }

    fn result_name(&mut self, node: SvgNode) -> String {
        if let Some(s) = node.attribute(AId::Result) {
            self.given.insert(s.to_string());
            self.next_index += 1;
            return s.to_string();
        }
        loop {
            let name = format!("result{}", self.next_index);
            self.next_index += 1;
            if !self.given.contains(&name) {
                return name;
            }
        }
    }
}

fn convert_primitives(
    filter: SvgNode,
    units: Units,
    state: &State,
    object_bbox: Option<NonZeroRect>,
    region: NonZeroRect,
    cache: &mut Cache,
) -> Vec<Primitive> {
    let mut prims = Prims {
        list: Vec::new(),
        names: HashSet::new(),
        given: HashSet::new(),
        next_index: 1,
    };
    let scale = if units == Units::ObjectBoundingBox {
        match object_bbox {
            Some(b) => (b.width(), b.height()),
            None => return Vec::new(),
        }
    } else {
        (1.0, 1.0)
    };
    for fe in filter.children() {
        let Some(tag) = fe.tag_name() else {
            continue;
        };
        // A filter in object units is converted anew for every user: its
        // children and its number lists count.
        let lists = [AId::KernelMatrix, AId::Values, AId::StdDeviation];
        let text: usize = lists
            .iter()
            .filter_map(|a| fe.attribute(*a))
            .map(str::len)
            .sum();
        let children = fe
            .children()
            .map(|c| 1 + c.attribute(AId::TableValues).map_or(0, |t| t.len() / 16));
        if !cache.spend_many(1 + text / 16 + children.sum::<usize>()) {
            break;
        }
        let Some(rect) = primitive_region(fe, tag, units, state, object_bbox, region) else {
            break;
        };
        let kind = match tag {
            EId::FeImage => Some(convert_image(fe, rect, state, cache).unwrap_or(TRANSPARENT)),
            _ => convert_kind(fe, tag, scale, &prims),
        };
        let Some(kind) = kind else {
            continue;
        };
        let color_interpolation = match fe
            .find_attribute(AId::ColorInterpolationFilters)
            .map(values::trim)
        {
            Some("sRGB") => ColorInterpolation::SRgb,
            _ => ColorInterpolation::LinearRgb,
        };
        let result = prims.result_name(fe);
        prims.names.insert(result.clone());
        prims.list.push(Primitive {
            rect,
            color_interpolation,
            result,
            kind,
        });
    }
    prims.list
}

/// What a malformed primitive becomes: a transparent image.
const TRANSPARENT: Kind = Kind::Flood(Flood {
    color: BLACK,
    opacity: 0.0,
});

/// Any primitive but `feImage`; `None` for an element that is not one.
fn convert_kind(fe: SvgNode, tag: EId, scale: (f32, f32), prims: &Prims) -> Option<Kind> {
    let num = |name: AId, def: f32| fe.number_attr(name).unwrap_or(def);
    Some(match tag {
        EId::FeDropShadow => {
            let (std_dev_x, std_dev_y) = std_dev(fe, scale, "2 2");
            let (color, opacity) = flood(fe);
            Kind::DropShadow(DropShadow {
                input: prims.input(fe, AId::In),
                dx: num(AId::Dx, 2.0) * scale.0,
                dy: num(AId::Dy, 2.0) * scale.1,
                std_dev_x,
                std_dev_y,
                color,
                opacity,
            })
        }
        EId::FeGaussianBlur => {
            let (std_dev_x, std_dev_y) = std_dev(fe, scale, "0 0");
            Kind::GaussianBlur(GaussianBlur {
                input: prims.input(fe, AId::In),
                std_dev_x,
                std_dev_y,
            })
        }
        EId::FeOffset => Kind::Offset(Offset {
            input: prims.input(fe, AId::In),
            dx: num(AId::Dx, 0.0) * scale.0,
            dy: num(AId::Dy, 0.0) * scale.1,
        }),
        EId::FeBlend => Kind::Blend(Blend {
            mode: fe
                .attribute(AId::Mode)
                .and_then(parse_blend_mode)
                .unwrap_or(BlendMode::SourceOver),
            input1: prims.input(fe, AId::In),
            input2: prims.input(fe, AId::In2),
        }),
        EId::FeFlood => {
            let (color, opacity) = flood(fe);
            Kind::Flood(Flood { color, opacity })
        }
        EId::FeComposite => Kind::Composite(Composite {
            operator: composite_operator(fe),
            input1: prims.input(fe, AId::In),
            input2: prims.input(fe, AId::In2),
        }),
        EId::FeMerge => Kind::Merge(Merge {
            inputs: fe.children().map(|c| prims.input(c, AId::In)).collect(),
        }),
        EId::FeTile => Kind::Tile(Tile {
            input: prims.input(fe, AId::In),
        }),
        EId::FeComponentTransfer => component_transfer(fe, prims),
        EId::FeColorMatrix => Kind::ColorMatrix(ColorMatrix {
            kind: color_matrix(fe).unwrap_or_default(),
            input: prims.input(fe, AId::In),
        }),
        EId::FeConvolveMatrix => convolve_matrix(fe, prims).unwrap_or(TRANSPARENT),
        EId::FeMorphology => morphology(fe, scale, prims),
        EId::FeDisplacementMap => displacement_map(fe, scale, prims),
        EId::FeTurbulence => turbulence(fe),
        EId::FeDiffuseLighting => match light_source(fe) {
            Some(light_source) => Kind::DiffuseLighting(DiffuseLighting {
                input: prims.input(fe, AId::In),
                surface_scale: num(AId::SurfaceScale, 1.0),
                diffuse_constant: num(AId::DiffuseConstant, 1.0),
                lighting_color: lighting_color(fe),
                light_source,
            }),
            None => TRANSPARENT,
        },
        EId::FeSpecularLighting => specular(fe, prims).unwrap_or(TRANSPARENT),
        _ => return None,
    })
}

fn primitive_region(
    fe: SvgNode,
    tag: EId,
    units: Units,
    state: &State,
    object_bbox: Option<NonZeroRect>,
    region: NonZeroRect,
) -> Option<NonZeroRect> {
    let get = |name: AId| -> Option<f32> {
        let l = values::length(fe.attribute(name)?)?;
        Some(convert_length(l, fe, name, units, state))
    };
    let (x, y, w, h) = (get(AId::X), get(AId::Y), get(AId::Width), get(AId::Height));
    if units == Units::ObjectBoundingBox {
        let r = NonZeroRect::from_xywh(
            x.unwrap_or(0.0),
            y.unwrap_or(0.0),
            w.unwrap_or(1.0),
            h.unwrap_or(1.0),
        )?;
        return if matches!(tag, EId::FeFlood | EId::FeImage) {
            bbox_transform(r, object_bbox?)
        } else {
            // As usvg (wrong under rotation and skew).
            bbox_transform(region, r)
        };
    }
    NonZeroRect::from_xywh(
        x.unwrap_or(region.x()),
        y.unwrap_or(region.y()),
        w.unwrap_or(region.width()),
        h.unwrap_or(region.height()),
    )
}

fn flood(fe: SvgNode) -> (Color, f32) {
    let c = fe
        .attribute(AId::FloodColor)
        .and_then(values::color)
        .unwrap_or(values::Color::BLACK);
    let mut opacity = 1.0;
    let color = split(c, &mut opacity);
    let flood_opacity = fe
        .attribute(AId::FloodOpacity)
        .and_then(values::opacity)
        .unwrap_or(1.0);
    (color, opacity * flood_opacity)
}

fn std_dev(fe: SvgNode, scale: (f32, f32), default: &str) -> (f32, f32) {
    let text = fe.attribute(AId::StdDeviation).unwrap_or(default);
    let (list, _) = values::number_list(text);
    // More than two values is an error.
    let (x, y) = match list.as_slice() {
        [x, y] => (*x, *y),
        [x] => (*x, *x),
        _ => (0.0, 0.0),
    };
    (
        positive(x as f32 * scale.0).unwrap_or(0.0),
        positive(y as f32 * scale.1).unwrap_or(0.0),
    )
}

fn composite_operator(fe: SvgNode) -> CompositeOperator {
    match fe.attribute(AId::Operator) {
        Some("in") => CompositeOperator::In,
        Some("out") => CompositeOperator::Out,
        Some("atop") => CompositeOperator::Atop,
        Some("xor") => CompositeOperator::Xor,
        Some("arithmetic") => CompositeOperator::Arithmetic {
            k1: fe.number_attr(AId::K1).unwrap_or(0.0),
            k2: fe.number_attr(AId::K2).unwrap_or(0.0),
            k3: fe.number_attr(AId::K3).unwrap_or(0.0),
            k4: fe.number_attr(AId::K4).unwrap_or(0.0),
        },
        _ => CompositeOperator::Over,
    }
}

fn convert_image(
    fe: SvgNode,
    subregion: NonZeroRect,
    state: &State,
    cache: &mut Cache,
) -> Option<Kind> {
    let mut root = Group::default();
    if let Some(node) = fe.href() {
        convert_element(node, state, cache, &mut root);
        if root.children.is_empty() {
            return None;
        }
    } else {
        let (kind, size) = crate::image::load_href(fe.attribute(AId::Href)?, state.opt)?;
        place_image(
            kind,
            true,
            image_quality(fe),
            fe.aspect(),
            size,
            subregion.translate_to(0.0, 0.0)?,
            &mut root,
        )?;
    }
    root.calculate_bounding_boxes();
    Some(Kind::Image(Image { root }))
}

fn component_transfer(fe: SvgNode, prims: &Prims) -> Kind {
    let mut kind = ComponentTransfer {
        input: prims.input(fe, AId::In),
        func_r: TransferFunction::Identity,
        func_g: TransferFunction::Identity,
        func_b: TransferFunction::Identity,
        func_a: TransferFunction::Identity,
    };
    for child in fe.children() {
        let Some(func) = transfer_function(child) else {
            continue;
        };
        match child.tag_name() {
            Some(EId::FeFuncR) => kind.func_r = func,
            Some(EId::FeFuncG) => kind.func_g = func,
            Some(EId::FeFuncB) => kind.func_b = func,
            Some(EId::FeFuncA) => kind.func_a = func,
            _ => {}
        }
    }
    Kind::ComponentTransfer(kind)
}

fn transfer_function(node: SvgNode) -> Option<TransferFunction> {
    let num = |name: AId, def: f32| node.number_attr(name).unwrap_or(def);
    let table = || node.numbers_attr(AId::TableValues).unwrap_or_default();
    Some(match node.attribute(AId::Type)? {
        "identity" => TransferFunction::Identity,
        "table" => TransferFunction::Table(table()),
        "discrete" => TransferFunction::Discrete(table()),
        "linear" => TransferFunction::Linear {
            slope: num(AId::Slope, 1.0),
            intercept: num(AId::Intercept, 0.0),
        },
        "gamma" => TransferFunction::Gamma {
            amplitude: num(AId::Amplitude, 1.0),
            exponent: num(AId::Exponent, 1.0),
            offset: num(AId::Offset, 0.0),
        },
        _ => return None,
    })
}

fn color_matrix(fe: SvgNode) -> Option<ColorMatrixKind> {
    let values = || fe.numbers_attr(AId::Values);
    match fe.attribute(AId::Type) {
        Some("saturate") => {
            let n = values()?.first().copied().unwrap_or(1.0);
            Some(ColorMatrixKind::Saturate(bound(0.0, n, 1.0)))
        }
        Some("hueRotate") => Some(ColorMatrixKind::HueRotate(
            values()?.first().copied().unwrap_or(0.0),
        )),
        Some("luminanceToAlpha") => Some(ColorMatrixKind::LuminanceToAlpha),
        _ => values()?
            .as_slice()
            .try_into()
            .ok()
            .map(ColorMatrixKind::Matrix),
    }
}

fn convolve_matrix(fe: SvgNode, prims: &Prims) -> Option<Kind> {
    let (mut columns, mut rows) = (3u32, 3u32);
    if let Some(text) = fe.attribute(AId::Order) {
        let (list, _) = values::number_list(text);
        let x = list.first().map_or(3, |n| *n as i32);
        let y = list.get(1).map_or(x, |n| *n as i32);
        if x > 0 && y > 0 {
            columns = x as u32;
            rows = y as u32;
        }
    }
    let kernel = fe.numbers_attr(AId::KernelMatrix)?;
    // A kernel of the wrong size disables the primitive.
    if kernel.len() as u64 != u64::from(columns) * u64::from(rows) {
        return None;
    }
    let mut sum: f32 = kernel.iter().sum();
    // Rounded, against float noise.
    sum = (sum * 1_000_000.0).round() / 1_000_000.0;
    if approx_zero(sum) {
        sum = 1.0;
    }
    let divisor = fe.number_attr(AId::Divisor).unwrap_or(sum);
    if approx_zero(divisor) || !divisor.is_finite() {
        return None;
    }
    let target = |name: AId, order: u32| -> Option<u32> {
        let t = match fe.number_attr(name) {
            Some(v) => i64::from(v as i32),
            None => i64::from(order / 2),
        };
        u32::try_from(t).ok().filter(|t| *t < order)
    };
    let target_x = target(AId::TargetX, columns)?;
    let target_y = target(AId::TargetY, rows)?;
    Some(Kind::ConvolveMatrix(ConvolveMatrix {
        input: prims.input(fe, AId::In),
        target_x,
        target_y,
        columns,
        rows,
        kernel,
        divisor,
        bias: fe.number_attr(AId::Bias).unwrap_or(0.0),
        edge_mode: match fe.attribute(AId::EdgeMode) {
            Some("none") => EdgeMode::None,
            Some("wrap") => EdgeMode::Wrap,
            _ => EdgeMode::Duplicate,
        },
        preserve_alpha: fe.attribute(AId::PreserveAlpha) == Some("true"),
    }))
}

fn displacement_map(fe: SvgNode, scale: (f32, f32), prims: &Prims) -> Kind {
    let channel = |name: AId| match fe.attribute(name) {
        Some("R") => ColorChannel::R,
        Some("G") => ColorChannel::G,
        Some("B") => ColorChannel::B,
        _ => ColorChannel::A,
    };
    Kind::DisplacementMap(DisplacementMap {
        input1: prims.input(fe, AId::In),
        input2: prims.input(fe, AId::In2),
        scale: fe.number_attr(AId::Scale).unwrap_or(0.0) * ((scale.0 + scale.1) / 2.0),
        x_channel_selector: channel(AId::XChannelSelector),
        y_channel_selector: channel(AId::YChannelSelector),
    })
}

fn specular(fe: SvgNode, prims: &Prims) -> Option<Kind> {
    let light_source = light_source(fe)?;
    let specular_exponent = fe.number_attr(AId::SpecularExponent).unwrap_or(1.0);
    // An exponent out of range disables the primitive.
    if !(1.0..=128.0).contains(&specular_exponent) {
        return None;
    }
    Some(Kind::SpecularLighting(SpecularLighting {
        input: prims.input(fe, AId::In),
        surface_scale: fe.number_attr(AId::SurfaceScale).unwrap_or(1.0),
        specular_constant: fe.number_attr(AId::SpecularConstant).unwrap_or(1.0),
        specular_exponent,
        lighting_color: lighting_color(fe),
        light_source,
    }))
}

fn lighting_color(fe: SvgNode) -> Color {
    // The colour's alpha does not affect lighting.
    let c = match fe.attribute(AId::LightingColor).map(values::trim) {
        // A missing `color` is black here, not white.
        Some("currentColor") => current_color(fe),
        Some(v) => match values::color(v) {
            Some(c) => c,
            None => return WHITE,
        },
        None => return WHITE,
    };
    split(c, &mut 1.0)
}

fn light_source(parent: SvgNode) -> Option<LightSource> {
    let node = parent.children().find(|n| {
        matches!(
            n.tag_name(),
            Some(EId::FeDistantLight | EId::FePointLight | EId::FeSpotLight)
        )
    })?;
    let get = |name: AId| node.number_attr(name).unwrap_or(0.0);
    Some(match node.tag_name()? {
        EId::FeDistantLight => LightSource::Distant {
            azimuth: get(AId::Azimuth),
            elevation: get(AId::Elevation),
        },
        EId::FePointLight => LightSource::Point {
            x: get(AId::X),
            y: get(AId::Y),
            z: get(AId::Z),
        },
        _ => LightSource::Spot(SpotLight {
            x: get(AId::X),
            y: get(AId::Y),
            z: get(AId::Z),
            points_at_x: get(AId::PointsAtX),
            points_at_y: get(AId::PointsAtY),
            points_at_z: get(AId::PointsAtZ),
            specular_exponent: positive(node.number_attr(AId::SpecularExponent).unwrap_or(1.0))
                .unwrap_or(1.0),
            limiting_cone_angle: node.number_attr(AId::LimitingConeAngle),
        }),
    })
}

/// One or two values; anything else counts as none.
fn pair(list: &[f32]) -> (f32, f32) {
    match list {
        [x, y] => (*x, *y),
        [x] => (*x, *x),
        _ => (0.0, 0.0),
    }
}

fn morphology(fe: SvgNode, scale: (f32, f32), prims: &Prims) -> Kind {
    let mut radius = (scale.0, scale.1);
    if let Some(list) = fe.numbers_attr(AId::Radius) {
        let (mut rx, mut ry) = pair(&list);
        // A zero radius counts as 1, as Chrome and Safari do.
        if approx_zero(rx) {
            rx = 1.0;
        }
        if approx_zero(ry) {
            ry = 1.0;
        }
        if rx.is_sign_positive() && ry.is_sign_positive() {
            radius = (rx * scale.0, ry * scale.1);
        }
    }
    Kind::Morphology(Morphology {
        input: prims.input(fe, AId::In),
        operator: match fe.attribute(AId::Operator) {
            Some("dilate") => MorphologyOperator::Dilate,
            _ => MorphologyOperator::Erode,
        },
        radius_x: positive(radius.0).unwrap_or(0.0),
        radius_y: positive(radius.1).unwrap_or(0.0),
    })
}

fn turbulence(fe: SvgNode) -> Kind {
    let mut freq = (0.0, 0.0);
    if let Some(list) = fe.numbers_attr(AId::BaseFrequency) {
        let (x, y) = pair(&list);
        if x.is_sign_positive() && y.is_sign_positive() {
            freq = (positive(x).unwrap_or(0.0), positive(y).unwrap_or(0.0));
        }
    }
    let octaves = fe.number_attr(AId::NumOctaves).unwrap_or(1.0);
    Kind::Turbulence(Turbulence {
        base_frequency_x: freq.0,
        base_frequency_y: freq.1,
        num_octaves: octaves.max(0.0).round() as u32,
        seed: fe.number_attr(AId::Seed).unwrap_or(0.0).trunc() as i32,
        stitch_tiles: fe.attribute(AId::StitchTiles) == Some("stitch"),
        fractal_noise: fe.attribute(AId::Type) == Some("fractalNoise"),
    })
}

/// A colour matrix in the shape of `grayscale()` and `sepia()`: the first
/// terms alone at full amount, the identity at none.
fn mix_matrix(terms: [(f32, f32); 9], amount: f64) -> Kind {
    let t = 1.0 - amount.min(1.0) as f32;
    let mut m = [0.0f32; 20];
    for (i, (base, delta)) in terms.into_iter().enumerate() {
        m[i / 3 * 5 + i % 3] = base + delta * t;
    }
    m[18] = 1.0;
    Kind::ColorMatrix(ColorMatrix {
        input: Input::SourceGraphic,
        kind: ColorMatrixKind::Matrix(m),
    })
}

fn transfer(rgb: &TransferFunction, a: TransferFunction) -> Kind {
    Kind::ComponentTransfer(ComponentTransfer {
        input: Input::SourceGraphic,
        func_r: rgb.clone(),
        func_g: rgb.clone(),
        func_b: rgb.clone(),
        func_a: a,
    })
}

fn convert_function(node: SvgNode, func: &FilterFunc, state: &State) -> Kind {
    let len = |l: Length, name: AId| convert_length(l, node, name, Units::UserSpaceOnUse, state);
    let matrix = |kind: ColorMatrixKind| {
        Kind::ColorMatrix(ColorMatrix {
            input: Input::SourceGraphic,
            kind,
        })
    };
    match *func {
        FilterFunc::Blur(l) => {
            let dev = positive(len(l, AId::Dx)).unwrap_or(0.0);
            Kind::GaussianBlur(GaussianBlur {
                input: Input::SourceGraphic,
                std_dev_x: dev,
                std_dev_y: dev,
            })
        }
        FilterFunc::DropShadow {
            color,
            dx,
            dy,
            std_dev,
        } => {
            let dev = positive(len(std_dev, AId::Dx)).unwrap_or(0.0);
            let mut opacity = 1.0;
            let color = split(color.unwrap_or_else(|| current_color(node)), &mut opacity);
            Kind::DropShadow(DropShadow {
                input: Input::SourceGraphic,
                dx: len(dx, AId::Dx),
                dy: len(dy, AId::Dy),
                std_dev_x: dev,
                std_dev_y: dev,
                color,
                opacity,
            })
        }
        FilterFunc::Grayscale(a) => mix_matrix(
            [
                (0.2126, 0.7874),
                (0.7152, -0.7152),
                (0.0722, -0.0722),
                (0.2126, -0.2126),
                (0.7152, 0.2848),
                (0.0722, -0.0722),
                (0.2126, -0.2126),
                (0.7152, -0.7152),
                (0.0722, 0.9278),
            ],
            a,
        ),
        FilterFunc::Sepia(a) => mix_matrix(
            [
                (0.393, 0.607),
                (0.769, -0.769),
                (0.189, -0.189),
                (0.349, -0.349),
                (0.686, 0.314),
                (0.168, -0.168),
                (0.272, -0.272),
                (0.534, -0.534),
                (0.131, 0.869),
            ],
            a,
        ),
        FilterFunc::Saturate(a) => {
            matrix(ColorMatrixKind::Saturate(positive(a as f32).unwrap_or(0.0)))
        }
        FilterFunc::HueRotate(deg) => matrix(ColorMatrixKind::HueRotate(deg as f32)),
        FilterFunc::Invert(a) => {
            let a = a.min(1.0) as f32;
            transfer(
                &TransferFunction::Table(vec![a, 1.0 - a]),
                TransferFunction::Identity,
            )
        }
        FilterFunc::Opacity(a) => transfer(
            &TransferFunction::Identity,
            TransferFunction::Table(vec![0.0, a.min(1.0) as f32]),
        ),
        FilterFunc::Brightness(a) => transfer(
            &TransferFunction::Linear {
                slope: a as f32,
                intercept: 0.0,
            },
            TransferFunction::Identity,
        ),
        FilterFunc::Contrast(a) => {
            let a = a as f32;
            transfer(
                &TransferFunction::Linear {
                    slope: a,
                    intercept: -(0.5 * a) + 0.5,
                },
                TransferFunction::Identity,
            )
        }
        FilterFunc::Url(_) => TRANSPARENT,
    }
}
