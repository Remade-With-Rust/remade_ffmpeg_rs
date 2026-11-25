//! Filter nodes of the render tree.
//!
//! The types mirror usvg's `filter` module (see `NOTICE.md`): a [`Filter`] is
//! a region and a list of [`Primitive`]s, every length in user units (both
//! `filterUnits` and `primitiveUnits` are resolved). A CSS filter function
//! (`blur(2px)`) becomes a filter of one primitive.

use rusty_raster::{BlendMode, NonZeroRect};

use crate::tree::{Color, Group};

/// A filter: `<filter>` or one CSS filter function.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Filter {
    /// The element id (empty for a filter function).
    pub id: String,
    /// The filter region, in user space.
    pub rect: NonZeroRect,
    /// A filter function: it has no region, and `rect` only sizes its layer.
    pub(crate) is_function: bool,
    /// The primitives, in document order; at least one.
    pub primitives: Vec<Primitive>,
}

/// One filter primitive.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Primitive {
    /// The primitive subregion, in user space.
    pub rect: NonZeroRect,
    /// `color-interpolation-filters`.
    pub color_interpolation: ColorInterpolation,
    /// The `result` name (generated when the document gives none).
    pub result: String,
    /// The operation.
    pub kind: Kind,
}

/// A filter operation.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Kind {
    /// `feBlend`.
    Blend(Blend),
    /// `feColorMatrix`.
    ColorMatrix(ColorMatrix),
    /// `feComponentTransfer`.
    ComponentTransfer(ComponentTransfer),
    /// `feComposite`.
    Composite(Composite),
    /// `feConvolveMatrix`.
    ConvolveMatrix(ConvolveMatrix),
    /// `feDiffuseLighting`.
    DiffuseLighting(DiffuseLighting),
    /// `feDisplacementMap`.
    DisplacementMap(DisplacementMap),
    /// `feDropShadow`.
    DropShadow(DropShadow),
    /// `feFlood`.
    Flood(Flood),
    /// `feGaussianBlur`.
    GaussianBlur(GaussianBlur),
    /// `feImage`.
    Image(Image),
    /// `feMerge`.
    Merge(Merge),
    /// `feMorphology`.
    Morphology(Morphology),
    /// `feOffset`.
    Offset(Offset),
    /// `feSpecularLighting`.
    SpecularLighting(SpecularLighting),
    /// `feTile`.
    Tile(Tile),
    /// `feTurbulence`.
    Turbulence(Turbulence),
}

impl Filter {
    /// Whether this is a CSS filter function (which does not clip to
    /// `rect`) rather than a `<filter>` element.
    pub fn is_function(&self) -> bool {
        self.is_function
    }
}

impl Kind {
    /// Calls `f` with every input of the primitive.
    pub fn for_each_input<'a>(&'a self, mut f: impl FnMut(&'a Input)) {
        match self {
            Kind::Blend(fe) => {
                f(&fe.input1);
                f(&fe.input2);
            }
            Kind::Composite(fe) => {
                f(&fe.input1);
                f(&fe.input2);
            }
            Kind::DisplacementMap(fe) => {
                f(&fe.input1);
                f(&fe.input2);
            }
            Kind::Merge(fe) => fe.inputs.iter().for_each(f),
            Kind::ColorMatrix(fe) => f(&fe.input),
            Kind::ComponentTransfer(fe) => f(&fe.input),
            Kind::ConvolveMatrix(fe) => f(&fe.input),
            Kind::DiffuseLighting(fe) => f(&fe.input),
            Kind::DropShadow(fe) => f(&fe.input),
            Kind::GaussianBlur(fe) => f(&fe.input),
            Kind::Morphology(fe) => f(&fe.input),
            Kind::Offset(fe) => f(&fe.input),
            Kind::SpecularLighting(fe) => f(&fe.input),
            Kind::Tile(fe) => f(&fe.input),
            Kind::Flood(_) | Kind::Image(_) | Kind::Turbulence(_) => {}
        }
    }
}

/// A primitive input.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    /// The filtered element.
    SourceGraphic,
    /// The filtered element's alpha, on black.
    SourceAlpha,
    /// An earlier primitive, by its `result`.
    Reference(String),
}

/// The colour space a primitive works in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ColorInterpolation {
    /// sRGB.
    SRgb,
    /// Linear RGB (the default).
    #[default]
    LinearRgb,
}

/// `feBlend`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Blend {
    /// `in`.
    pub input1: Input,
    /// `in2`.
    pub input2: Input,
    /// The blend mode (`normal` is `SourceOver`).
    pub mode: BlendMode,
}

/// `feColorMatrix`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ColorMatrix {
    /// `in`.
    pub input: Input,
    /// The matrix.
    pub kind: ColorMatrixKind,
}

/// The matrix of an `feColorMatrix`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ColorMatrixKind {
    /// A full 4x5 matrix.
    Matrix([f32; 20]),
    /// `saturate`.
    Saturate(f32),
    /// `hueRotate`, in degrees.
    HueRotate(f32),
    /// `luminanceToAlpha`.
    LuminanceToAlpha,
}

impl Default for ColorMatrixKind {
    fn default() -> Self {
        let mut m = [0.0; 20];
        for i in 0..4 {
            m[i * 6] = 1.0;
        }
        ColorMatrixKind::Matrix(m)
    }
}

/// `feComponentTransfer`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ComponentTransfer {
    /// `in`.
    pub input: Input,
    /// `feFuncR`.
    pub func_r: TransferFunction,
    /// `feFuncG`.
    pub func_g: TransferFunction,
    /// `feFuncB`.
    pub func_b: TransferFunction,
    /// `feFuncA`.
    pub func_a: TransferFunction,
}

/// A component transfer function.
#[derive(Clone, PartialEq, Debug)]
pub enum TransferFunction {
    /// `identity`.
    Identity,
    /// `table`.
    Table(Vec<f32>),
    /// `discrete`.
    Discrete(Vec<f32>),
    /// `linear`.
    Linear {
        /// `slope`.
        slope: f32,
        /// `intercept`.
        intercept: f32,
    },
    /// `gamma`.
    Gamma {
        /// `amplitude`.
        amplitude: f32,
        /// `exponent`.
        exponent: f32,
        /// `offset`.
        offset: f32,
    },
}

/// `feComposite`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Composite {
    /// `in`.
    pub input1: Input,
    /// `in2`.
    pub input2: Input,
    /// `operator`.
    pub operator: CompositeOperator,
}

/// The operator of an `feComposite`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompositeOperator {
    /// `over`.
    Over,
    /// `in`.
    In,
    /// `out`.
    Out,
    /// `atop`.
    Atop,
    /// `xor`.
    Xor,
    /// `arithmetic`: `k1*i1*i2 + k2*i1 + k3*i2 + k4`.
    Arithmetic {
        /// `k1`.
        k1: f32,
        /// `k2`.
        k2: f32,
        /// `k3`.
        k3: f32,
        /// `k4`.
        k4: f32,
    },
}

/// `feConvolveMatrix`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ConvolveMatrix {
    /// `in`.
    pub input: Input,
    /// `targetX`, less than `columns`.
    pub target_x: u32,
    /// `targetY`, less than `rows`.
    pub target_y: u32,
    /// The kernel width.
    pub columns: u32,
    /// The kernel height.
    pub rows: u32,
    /// The kernel, row by row: `columns * rows` values.
    pub kernel: Vec<f32>,
    /// `divisor`, not zero.
    pub divisor: f32,
    /// `bias`.
    pub bias: f32,
    /// `edgeMode`.
    pub edge_mode: EdgeMode,
    /// `preserveAlpha`.
    pub preserve_alpha: bool,
}

/// What `feConvolveMatrix` reads outside the image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdgeMode {
    /// Transparent black.
    None,
    /// The nearest edge pixel.
    Duplicate,
    /// The opposite side.
    Wrap,
}

/// `feDisplacementMap`: each pixel is taken from `scale * (c - 0.5)` away,
/// `c` being the map's unpremultiplied channel (resvg applies the scale
/// twice, and reads the map premultiplied).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DisplacementMap {
    /// `in`: the image that is moved.
    pub input1: Input,
    /// `in2`: the map.
    pub input2: Input,
    /// `scale`.
    pub scale: f32,
    /// `xChannelSelector`.
    pub x_channel_selector: ColorChannel,
    /// `yChannelSelector`.
    pub y_channel_selector: ColorChannel,
}

/// A colour channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorChannel {
    /// Red.
    R,
    /// Green.
    G,
    /// Blue.
    B,
    /// Alpha.
    A,
}

/// `feDropShadow`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DropShadow {
    /// `in`.
    pub input: Input,
    /// `dx`.
    pub dx: f32,
    /// `dy`.
    pub dy: f32,
    /// The horizontal `stdDeviation`, not negative.
    pub std_dev_x: f32,
    /// The vertical `stdDeviation`, not negative.
    pub std_dev_y: f32,
    /// `flood-color`.
    pub color: Color,
    /// `flood-opacity` times the colour's alpha.
    pub opacity: f32,
}

/// `feFlood`.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Flood {
    /// `flood-color`.
    pub color: Color,
    /// `flood-opacity` times the colour's alpha.
    pub opacity: f32,
}

/// `feGaussianBlur`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GaussianBlur {
    /// `in`.
    pub input: Input,
    /// The horizontal `stdDeviation`, not negative.
    pub std_dev_x: f32,
    /// The vertical `stdDeviation`, not negative.
    pub std_dev_y: f32,
}

/// `feImage`: an element of the document or an embedded image, as a group.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Image {
    /// The content.
    pub root: Group,
}

/// `feMerge`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Merge {
    /// The inputs, bottom first.
    pub inputs: Vec<Input>,
}

/// `feMorphology`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Morphology {
    /// `in`.
    pub input: Input,
    /// `operator`.
    pub operator: MorphologyOperator,
    /// The horizontal radius, not negative.
    pub radius_x: f32,
    /// The vertical radius, not negative.
    pub radius_y: f32,
}

/// The operator of an `feMorphology`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MorphologyOperator {
    /// `erode`.
    Erode,
    /// `dilate`.
    Dilate,
}

/// `feOffset`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Offset {
    /// `in`.
    pub input: Input,
    /// `dx`.
    pub dx: f32,
    /// `dy`.
    pub dy: f32,
}

/// `feTile`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Tile {
    /// `in`.
    pub input: Input,
}

/// `feTurbulence`.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Turbulence {
    /// The horizontal `baseFrequency`, not negative.
    pub base_frequency_x: f32,
    /// The vertical `baseFrequency`, not negative.
    pub base_frequency_y: f32,
    /// `numOctaves`.
    pub num_octaves: u32,
    /// `seed`.
    pub seed: i32,
    /// `stitchTiles`.
    pub stitch_tiles: bool,
    /// `fractalNoise` rather than `turbulence`.
    pub fractal_noise: bool,
}

/// `feDiffuseLighting`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DiffuseLighting {
    /// `in`.
    pub input: Input,
    /// `surfaceScale`.
    pub surface_scale: f32,
    /// `diffuseConstant`.
    pub diffuse_constant: f32,
    /// `lighting-color`.
    pub lighting_color: Color,
    /// The light.
    pub light_source: LightSource,
}

/// `feSpecularLighting`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SpecularLighting {
    /// `in`.
    pub input: Input,
    /// `surfaceScale`.
    pub surface_scale: f32,
    /// `specularConstant`.
    pub specular_constant: f32,
    /// `specularExponent`, `1..=128`.
    pub specular_exponent: f32,
    /// `lighting-color`.
    pub lighting_color: Color,
    /// The light.
    pub light_source: LightSource,
}

/// A light.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LightSource {
    /// `feDistantLight`.
    Distant {
        /// `azimuth`, degrees.
        azimuth: f32,
        /// `elevation`, degrees.
        elevation: f32,
    },
    /// `fePointLight`.
    Point {
        /// Position.
        x: f32,
        /// Position.
        y: f32,
        /// Position.
        z: f32,
    },
    /// `feSpotLight`.
    Spot(SpotLight),
}

/// `feSpotLight`.
#[derive(Clone, Copy, PartialEq, Debug)]
#[non_exhaustive]
pub struct SpotLight {
    /// Position.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Position.
    pub z: f32,
    /// Target.
    pub points_at_x: f32,
    /// Target.
    pub points_at_y: f32,
    /// Target.
    pub points_at_z: f32,
    /// `specularExponent`, not negative.
    pub specular_exponent: f32,
    /// `limitingConeAngle`, degrees.
    pub limiting_cone_angle: Option<f32>,
}
