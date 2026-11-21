//! Parsers for SVG attribute micro-syntaxes: numbers, lengths, colours,
//! paints, transforms, `viewBox`, `preserveAspectRatio` and path data.
//!
//! Every parser is total: malformed input returns `None` (or stops at the
//! first error, for path data, as the SVG error-handling rules require). No
//! parser recurses, and none allocates more than its input length implies.

use crate::colors::NAMED_COLORS;

/// A byte cursor over an attribute value.
#[derive(Clone, Copy)]
pub(crate) struct Stream<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Stream<'a> {
    pub(crate) fn new(s: &'a str) -> Self {
        Stream {
            s: s.as_bytes(),
            pos: 0,
        }
    }

    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.s.len()
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    pub(crate) fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.s.len());
    }

    pub(crate) fn skip_spaces(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn starts_with(&self, t: &[u8]) -> bool {
        self.s[self.pos..].starts_with(t)
    }

    fn starts_with_ci(&self, t: &[u8]) -> bool {
        let rest = &self.s[self.pos..];
        rest.len() >= t.len() && rest[..t.len()].eq_ignore_ascii_case(t)
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(crate) fn rest(&self) -> &'a str {
        // The cursor only ever stops on ASCII boundaries.
        std::str::from_utf8(&self.s[self.pos..]).unwrap_or("")
    }

    fn skip_list_separator(&mut self) {
        self.skip_spaces();
        if self.eat(b',') {
            self.skip_spaces();
        }
    }

    /// An SVG number: `[+-]? (\d+ (\.\d*)? | \.\d+) ([eE][+-]?\d+)?`.
    /// An `e` followed by `m` or `x` is a unit, not an exponent.
    pub(crate) fn parse_number(&mut self) -> Option<f64> {
        self.skip_spaces();
        let start = self.pos;
        // The token is scanned over a local slice, the cursor written back
        // once: the digit loops then hold everything in registers.
        let mut rest = &self.s[start..];
        let neg = match rest.first() {
            Some(b'-') => {
                rest = &rest[1..];
                true
            }
            Some(b'+') => {
                rest = &rest[1..];
                false
            }
            _ => false,
        };
        // The decimal mantissa and exponent are accumulated while scanning,
        // so the common case needs no second pass over the text (see
        // `exact_decimal`). The mantissa wraps past `MAX_EXACT_DIGITS`
        // digits; such a token goes to the general parser instead.
        let mut mant = 0u64;
        let mut exp10 = 0i32;
        let int_digits = scan_digits(&mut rest, &mut mant);
        let mut digits = int_digits;
        if let [b'.', tail @ ..] = rest {
            if matches!(tail.first(), Some(b'0'..=b'9')) {
                rest = tail;
                let frac_digits = scan_digits(&mut rest, &mut mant);
                digits += frac_digits;
                // Unused (see below) when it does not fit.
                exp10 = -(frac_digits.min(MAX_EXACT_DIGITS + 1) as i32);
            } else if digits > 0 {
                rest = tail;
            }
        }
        if digits == 0 {
            self.pos = start;
            return None;
        }
        if let [b'e' | b'E', tail @ ..] = rest {
            if !matches!(tail.first(), Some(b'm' | b'x')) {
                let mut tail = tail;
                let eneg = match tail.first() {
                    Some(b'-') => {
                        tail = &tail[1..];
                        true
                    }
                    Some(b'+') => {
                        tail = &tail[1..];
                        false
                    }
                    _ => false,
                };
                let mut ed = 0;
                let mut e = 0i32;
                while let [c @ b'0'..=b'9', t @ ..] = tail {
                    tail = t;
                    ed += 1;
                    // Saturates far outside the exact range.
                    e = (e * 10 + i32::from(c - b'0')).min(100_000);
                }
                if ed != 0 {
                    rest = tail;
                    if eneg {
                        exp10 -= e;
                    } else {
                        exp10 += e;
                    }
                }
            }
        }
        self.pos = self.s.len() - rest.len();
        if digits <= MAX_EXACT_DIGITS {
            if let Some(v) = exact_decimal(mant, exp10) {
                return Some(if neg { -v } else { v });
            }
        }
        self.parse_number_text(start)
    }

    /// The general case: the scanned token through the standard library's
    /// correctly rounded parser.
    #[cold]
    #[inline(never)]
    fn parse_number_text(&self, start: usize) -> Option<f64> {
        let text = std::str::from_utf8(&self.s[start..self.pos]).ok()?;
        let v: f64 = text.parse().ok()?;
        if v.is_finite() {
            Some(v)
        } else {
            None
        }
    }

    pub(crate) fn parse_list_number(&mut self) -> Option<f64> {
        let n = self.parse_number()?;
        self.skip_list_separator();
        Some(n)
    }

    fn parse_number_or_percent(&mut self) -> Option<f64> {
        let n = self.parse_number()?;
        if self.eat(b'%') {
            Some(n / 100.0)
        } else {
            Some(n)
        }
    }

    fn parse_list_number_or_percent(&mut self) -> Option<f64> {
        let n = self.parse_number_or_percent()?;
        self.skip_list_separator();
        Some(n)
    }

    pub(crate) fn parse_length(&mut self) -> Option<Length> {
        let n = self.parse_number()?;
        let unit = if self.eat(b'%') {
            Unit::Percent
        } else if self.starts_with(b"px") {
            self.advance(2);
            Unit::Px
        } else if self.starts_with(b"em") {
            self.advance(2);
            Unit::Em
        } else if self.starts_with(b"ex") {
            self.advance(2);
            Unit::Ex
        } else if self.starts_with(b"in") {
            self.advance(2);
            Unit::In
        } else if self.starts_with(b"cm") {
            self.advance(2);
            Unit::Cm
        } else if self.starts_with(b"mm") {
            self.advance(2);
            Unit::Mm
        } else if self.starts_with(b"pt") {
            self.advance(2);
            Unit::Pt
        } else if self.starts_with(b"pc") {
            self.advance(2);
            Unit::Pc
        } else {
            Unit::None
        };
        Some(Length { number: n, unit })
    }

    fn consume_ident(&mut self) -> &'a str {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.s[start..self.pos]).unwrap_or("")
    }
}

/// The most decimal digits a `u64` mantissa always holds (`10^18 < 2^64`).
const MAX_EXACT_DIGITS: usize = 18;

/// Consumes the leading ASCII digits of `rest` into the decimal `mant`
/// (wrapping once past `MAX_EXACT_DIGITS` digits in all) and returns how many
/// there were.
#[inline]
fn scan_digits(rest: &mut &[u8], mant: &mut u64) -> usize {
    let s = *rest;
    let mut m = *mant;
    let mut n = 0;
    while let Some(&c) = s.get(n) {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            break;
        }
        m = m.wrapping_mul(10).wrapping_add(u64::from(d));
        n += 1;
    }
    *mant = m;
    *rest = &s[n..];
    n
}

/// Powers of ten that an `f64` holds exactly.
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// `mant * 10^exp10` when one IEEE operation gives the correctly rounded
/// result (Clinger's fast path): the mantissa and the power of ten are both
/// exact in an `f64`, so their product or quotient is rounded once, to the
/// same value the standard library's parser returns for the same digits.
/// `None` outside that range.
#[inline]
fn exact_decimal(mant: u64, exp10: i32) -> Option<f64> {
    if mant > (1 << 53) || !(-22..=22).contains(&exp10) {
        return None;
    }
    let v = mant as f64;
    Some(if exp10 < 0 {
        v / POW10[(-exp10) as usize]
    } else {
        v * POW10[exp10 as usize]
    })
}

/// CSS length units.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Unit {
    None,
    Px,
    Em,
    Ex,
    In,
    Cm,
    Mm,
    Pt,
    Pc,
    Percent,
}

/// A number with a unit.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Length {
    pub number: f64,
    pub unit: Unit,
}

impl Length {
    pub(crate) const fn new(number: f64, unit: Unit) -> Self {
        Length { number, unit }
    }

    pub(crate) const fn zero() -> Self {
        Length::new(0.0, Unit::None)
    }
}

/// A whole-string number (trailing spaces allowed).
pub(crate) fn number(text: &str) -> Option<f64> {
    let mut s = Stream::new(text);
    let n = s.parse_number()?;
    s.skip_spaces();
    s.at_end().then_some(n)
}

/// A whole-string length.
pub(crate) fn length(text: &str) -> Option<Length> {
    let mut s = Stream::new(text);
    let l = s.parse_length()?;
    s.skip_spaces();
    s.at_end().then_some(l)
}

/// A comma/space separated length list (`stroke-dasharray`). `None` on any
/// malformed item.
pub(crate) fn length_list(text: &str) -> Option<Vec<Length>> {
    let mut s = Stream::new(text);
    let mut out = Vec::new();
    s.skip_spaces();
    while !s.at_end() {
        out.push(s.parse_length()?);
        s.skip_list_separator();
    }
    Some(out)
}

/// An opacity: a number or a percentage, clamped to `0..=1`.
pub(crate) fn opacity(text: &str) -> Option<f32> {
    let l = length(text)?;
    let v = match l.unit {
        Unit::None => l.number,
        Unit::Percent => l.number / 100.0,
        _ => return None,
    };
    Some(v.clamp(0.0, 1.0) as f32)
}

/// RGBA, non-premultiplied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub(crate) const BLACK: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
}

fn hex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

fn round_u8(v: f64) -> u8 {
    // `as u8` saturates, and NaN maps to 0: total for any input.
    v.round() as u8
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> Color {
    let t2 = if lightness <= 0.5 {
        lightness * (saturation + 1.0)
    } else {
        lightness + saturation - (lightness * saturation)
    };
    let t1 = lightness * 2.0 - t2;
    let red = hue_to_rgb(t1, t2, hue + 2.0);
    let green = hue_to_rgb(t1, t2, hue);
    let blue = hue_to_rgb(t1, t2, hue - 2.0);
    Color {
        r: (red * 255.0).round() as u8,
        g: (green * 255.0).round() as u8,
        b: (blue * 255.0).round() as u8,
        a: 255,
    }
}

fn hue_to_rgb(t1: f32, t2: f32, mut hue: f32) -> f32 {
    if hue < 0.0 {
        hue += 6.0;
    }
    if hue >= 6.0 {
        hue -= 6.0;
    }
    if hue < 1.0 {
        (t2 - t1) * hue + t1
    } else if hue < 3.0 {
        t2
    } else if hue < 4.0 {
        (t2 - t1) * (4.0 - hue) + t1
    } else {
        t1
    }
}

impl Stream<'_> {
    fn parse_angle_deg(&mut self) -> Option<f64> {
        let n = self.parse_number()?;
        let v = if self.starts_with(b"deg") {
            self.advance(3);
            n
        } else if self.starts_with(b"grad") {
            self.advance(4);
            n * 180.0 / 200.0
        } else if self.starts_with(b"rad") {
            self.advance(3);
            n.to_degrees()
        } else if self.starts_with(b"turn") {
            self.advance(4);
            n * 360.0
        } else {
            n
        };
        Some(v)
    }

    pub(crate) fn parse_color(&mut self) -> Option<Color> {
        self.skip_spaces();
        let mut color = Color::BLACK;
        if self.eat(b'#') {
            let start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let h = &self.s[start..self.pos];
            match h.len() {
                6 | 8 => {
                    color.r = (hex(h[0]) << 4) | hex(h[1]);
                    color.g = (hex(h[2]) << 4) | hex(h[3]);
                    color.b = (hex(h[4]) << 4) | hex(h[5]);
                    if h.len() == 8 {
                        color.a = (hex(h[6]) << 4) | hex(h[7]);
                    }
                }
                3 | 4 => {
                    color.r = hex(h[0]) * 17;
                    color.g = hex(h[1]) * 17;
                    color.b = hex(h[2]) * 17;
                    if h.len() == 4 {
                        color.a = hex(h[3]) * 17;
                    }
                }
                _ => return None,
            }
            return Some(color);
        }
        let name = self.consume_ident().to_ascii_lowercase();
        if name == "rgb" || name == "rgba" {
            if !self.eat(b'(') {
                return None;
            }
            let value = self.parse_number()?;
            let is_percent = self.eat(b'%');
            self.skip_list_separator();
            if is_percent {
                color.r = round_u8((value / 100.0) * 255.0);
                color.g = round_u8(self.parse_list_number_or_percent()? * 255.0);
                color.b = round_u8(self.parse_list_number_or_percent()? * 255.0);
            } else {
                color.r = round_u8(value);
                color.g = round_u8(self.parse_list_number()?);
                color.b = round_u8(self.parse_list_number()?);
            }
            self.skip_spaces();
            if self.peek() != Some(b')') {
                if self.eat(b'/') {
                    self.skip_spaces();
                }
                color.a = round_u8(self.parse_list_number()? * 255.0);
            }
            self.skip_spaces();
            if !self.eat(b')') {
                return None;
            }
        } else if name == "hsl" || name == "hsla" {
            if !self.eat(b'(') {
                return None;
            }
            let mut hue = self.parse_angle_deg()?;
            self.skip_list_separator();
            hue = ((hue % 360.0) + 360.0) % 360.0;
            let sat = self.parse_list_number_or_percent()?.clamp(0.0, 1.0);
            let light = self.parse_list_number_or_percent()?.clamp(0.0, 1.0);
            color = hsl_to_rgb(hue as f32 / 60.0, sat as f32, light as f32);
            self.skip_spaces();
            if self.peek() != Some(b')') {
                if self.eat(b'/') {
                    self.skip_spaces();
                }
                color.a = round_u8(self.parse_list_number_or_percent()? * 255.0);
            }
            self.skip_spaces();
            if !self.eat(b')') {
                return None;
            }
        } else {
            let idx = NAMED_COLORS
                .binary_search_by(|(n, _)| (*n).cmp(name.as_str()))
                .ok()?;
            let c = NAMED_COLORS[idx].1;
            color = Color {
                r: c[0],
                g: c[1],
                b: c[2],
                a: c[3],
            };
        }
        Some(color)
    }
}

/// A whole-string colour.
pub(crate) fn color(text: &str) -> Option<Color> {
    let mut s = Stream::new(text);
    let c = s.parse_color()?;
    s.skip_spaces();
    s.at_end().then_some(c)
}

/// A parsed `fill`/`stroke` value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Paint<'a> {
    None,
    CurrentColor,
    Color(Color),
    /// `url(#id)` with an optional fallback.
    Url(&'a str, Option<PaintFallback>),
    ContextFill,
    ContextStroke,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PaintFallback {
    None,
    CurrentColor,
    Color(Color),
}

/// Parses a paint value.
pub(crate) fn paint(text: &str) -> Option<Paint<'_>> {
    let t = text.trim();
    match t {
        "none" => return Some(Paint::None),
        "currentColor" => return Some(Paint::CurrentColor),
        "context-fill" => return Some(Paint::ContextFill),
        "context-stroke" => return Some(Paint::ContextStroke),
        _ => {}
    }
    if t.starts_with("url(") {
        let mut s = Stream::new(t);
        let id = s.parse_func_iri()?;
        s.skip_spaces();
        let fallback = if s.at_end() {
            None
        } else {
            let rest = s.rest().trim();
            match rest {
                "none" => Some(PaintFallback::None),
                "currentColor" => Some(PaintFallback::CurrentColor),
                _ => Some(PaintFallback::Color(color(rest)?)),
            }
        };
        return Some(Paint::Url(id, fallback));
    }
    color(t).map(Paint::Color)
}

impl<'a> Stream<'a> {
    /// `url(#id)` or `url('#id')`; returns the id.
    pub(crate) fn parse_func_iri(&mut self) -> Option<&'a str> {
        self.skip_spaces();
        if !self.starts_with(b"url(") {
            return None;
        }
        self.advance(4);
        self.skip_spaces();
        let quote = match self.peek() {
            Some(q @ (b'\'' | b'"')) => {
                self.advance(1);
                Some(q)
            }
            _ => None,
        };
        if !self.eat(b'#') {
            return None;
        }
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b')' || Some(c) == quote || (quote.is_none() && c.is_ascii_whitespace()) {
                break;
            }
            self.pos += 1;
        }
        let id = std::str::from_utf8(&self.s[start..self.pos]).ok()?;
        if let Some(q) = quote {
            if !self.eat(q) {
                return None;
            }
        }
        self.skip_spaces();
        if !self.eat(b')') {
            return None;
        }
        Some(id)
    }
}

/// `url(#id)` as a whole string.
pub(crate) fn func_iri(text: &str) -> Option<&str> {
    let mut s = Stream::new(text);
    s.parse_func_iri()
}

/// An IRI (`href`): `#id`, possibly surrounded by spaces.
pub(crate) fn iri(text: &str) -> Option<&str> {
    let t = text.trim();
    t.strip_prefix('#').filter(|s| !s.is_empty())
}

/// An affine matrix `[a b c d e f]` in f64, as parsed.
pub(crate) type Matrix = [f64; 6];

fn mat_mul(a: Matrix, b: Matrix) -> Matrix {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

/// A `transform` list. `None` on a syntax error.
pub(crate) fn transform(text: &str) -> Option<Matrix> {
    let mut s = Stream::new(text);
    let mut m: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    loop {
        s.skip_spaces();
        if s.eat(b',') {
            continue;
        }
        if s.at_end() {
            break;
        }
        let name = s.consume_ident();
        s.skip_spaces();
        if !s.eat(b'(') {
            return None;
        }
        let mut args = [0.0f64; 6];
        let mut n = 0;
        loop {
            s.skip_spaces();
            if s.eat(b')') {
                break;
            }
            if n == 6 {
                return None;
            }
            args[n] = s.parse_number()?;
            n += 1;
            s.skip_list_separator();
        }
        let t: Matrix = match (name, n) {
            ("matrix", 6) => args,
            ("translate", 1) => [1.0, 0.0, 0.0, 1.0, args[0], 0.0],
            ("translate", 2) => [1.0, 0.0, 0.0, 1.0, args[0], args[1]],
            ("scale", 1) => [args[0], 0.0, 0.0, args[0], 0.0, 0.0],
            ("scale", 2) => [args[0], 0.0, 0.0, args[1], 0.0, 0.0],
            ("rotate", 1) => {
                let (sn, cs) = args[0].to_radians().sin_cos();
                [cs, sn, -sn, cs, 0.0, 0.0]
            }
            ("rotate", 3) => {
                let (sn, cs) = args[0].to_radians().sin_cos();
                let r = [cs, sn, -sn, cs, 0.0, 0.0];
                let t1 = [1.0, 0.0, 0.0, 1.0, args[1], args[2]];
                let t2 = [1.0, 0.0, 0.0, 1.0, -args[1], -args[2]];
                mat_mul(mat_mul(t1, r), t2)
            }
            ("skewX", 1) => [1.0, 0.0, args[0].to_radians().tan(), 1.0, 0.0, 0.0],
            ("skewY", 1) => [1.0, args[0].to_radians().tan(), 0.0, 1.0, 0.0, 0.0],
            _ => return None,
        };
        m = mat_mul(m, t);
    }
    Some(m)
}

/// `viewBox="x y w h"`; `None` unless `w > 0 && h > 0`.
pub(crate) fn view_box(text: &str) -> Option<[f64; 4]> {
    let mut s = Stream::new(text);
    let x = s.parse_list_number()?;
    let y = s.parse_list_number()?;
    let w = s.parse_list_number()?;
    let h = s.parse_list_number()?;
    (w > 0.0 && h > 0.0).then_some([x, y, w, h])
}

/// `preserveAspectRatio` alignment.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Align {
    None,
    XMinYMin,
    XMidYMin,
    XMaxYMin,
    XMinYMid,
    #[default]
    XMidYMid,
    XMaxYMid,
    XMinYMax,
    XMidYMax,
    XMaxYMax,
}

/// `preserveAspectRatio`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct AspectRatio {
    pub align: Align,
    pub slice: bool,
}

pub(crate) fn aspect_ratio(text: &str) -> Option<AspectRatio> {
    let mut it = text.split_ascii_whitespace();
    let mut first = it.next()?;
    if first == "defer" {
        first = it.next()?;
    }
    let align = match first {
        "none" => Align::None,
        "xMinYMin" => Align::XMinYMin,
        "xMidYMin" => Align::XMidYMin,
        "xMaxYMin" => Align::XMaxYMin,
        "xMinYMid" => Align::XMinYMid,
        "xMidYMid" => Align::XMidYMid,
        "xMaxYMid" => Align::XMaxYMid,
        "xMinYMax" => Align::XMinYMax,
        "xMidYMax" => Align::XMidYMax,
        "xMaxYMax" => Align::XMaxYMax,
        _ => return None,
    };
    let slice = match it.next() {
        None | Some("meet") => false,
        Some("slice") => true,
        Some(_) => return None,
    };
    if it.next().is_some() {
        return None;
    }
    Some(AspectRatio { align, slice })
}

/// A point list (`points` on polyline/polygon). Stops at the first error and
/// drops an unpaired trailing coordinate, as the spec requires.
pub(crate) fn points(text: &str) -> Vec<(f64, f64)> {
    let mut s = Stream::new(text);
    let mut out = Vec::new();
    loop {
        s.skip_spaces();
        if s.at_end() {
            break;
        }
        let Some(x) = s.parse_list_number() else {
            break;
        };
        let Some(y) = s.parse_list_number() else {
            break;
        };
        out.push((x, y));
    }
    out
}

/// A path command after simplification: absolute, no H/V/S/T, arcs as cubics.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Seg {
    MoveTo(f64, f64),
    LineTo(f64, f64),
    Quad(f64, f64, f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

#[derive(Clone, Copy, PartialEq)]
enum Raw {
    M(bool, f64, f64),
    L(bool, f64, f64),
    H(bool, f64),
    V(bool, f64),
    C(bool, [f64; 6]),
    S(bool, [f64; 4]),
    Q(bool, [f64; 4]),
    T(bool, f64, f64),
    A(bool, [f64; 3], bool, bool, f64, f64),
    Z,
}

fn parse_flag(s: &mut Stream) -> Option<bool> {
    s.skip_spaces();
    let c = s.peek()?;
    if c == b'0' || c == b'1' {
        s.advance(1);
        if s.peek() == Some(b',') {
            s.advance(1);
        }
        s.skip_spaces();
        Some(c == b'1')
    } else {
        None
    }
}

fn is_cmd(c: u8) -> bool {
    matches!(
        c.to_ascii_lowercase(),
        b'm' | b'z' | b'l' | b'h' | b'v' | b'c' | b's' | b'q' | b't' | b'a'
    )
}

/// Parses one raw (unsimplified) segment at the cursor. `prev` is the
/// previous command letter (an implicitly repeated `M`/`m` reads as `L`/`l`
/// and is remembered as `M`/`m`). `None` at the end of the data and at the
/// first error.
fn parse_raw_seg(s: &mut Stream, prev: &mut Option<u8>) -> Option<Raw> {
    s.skip_spaces();
    let c = s.peek()?;
    let cmd;
    let mut implicit_m = false;
    if is_cmd(c) {
        if prev.is_none() && !c.eq_ignore_ascii_case(&b'm') {
            return None;
        }
        cmd = c;
        s.advance(1);
    } else if matches!(c, b'0'..=b'9' | b'.' | b'-' | b'+') {
        match *prev {
            Some(p) if !p.eq_ignore_ascii_case(&b'z') => {
                // Implicit repetition; a repeated M/m is an L/l.
                if p == b'M' {
                    cmd = b'L';
                    implicit_m = true;
                } else if p == b'm' {
                    cmd = b'l';
                    implicit_m = true;
                } else {
                    cmd = p;
                }
            }
            _ => return None,
        }
    } else {
        return None;
    }
    let abs = cmd.is_ascii_uppercase();
    let seg = match cmd.to_ascii_lowercase() {
        b'm' => Raw::M(abs, s.parse_list_number()?, s.parse_list_number()?),
        b'l' => Raw::L(abs, s.parse_list_number()?, s.parse_list_number()?),
        b'h' => Raw::H(abs, s.parse_list_number()?),
        b'v' => Raw::V(abs, s.parse_list_number()?),
        b'c' => {
            let mut a = [0.0; 6];
            for v in &mut a {
                *v = s.parse_list_number()?;
            }
            Raw::C(abs, a)
        }
        b's' | b'q' => {
            let mut a = [0.0; 4];
            for v in &mut a {
                *v = s.parse_list_number()?;
            }
            if cmd.eq_ignore_ascii_case(&b's') {
                Raw::S(abs, a)
            } else {
                Raw::Q(abs, a)
            }
        }
        b't' => Raw::T(abs, s.parse_list_number()?, s.parse_list_number()?),
        b'a' => {
            let rx = s.parse_list_number()?;
            let ry = s.parse_list_number()?;
            let rot = s.parse_list_number()?;
            let la = parse_flag(s)?;
            let sw = parse_flag(s)?;
            let x = s.parse_list_number()?;
            let y = s.parse_list_number()?;
            Raw::A(abs, [rx, ry, rot], la, sw, x, y)
        }
        _ => Raw::Z,
    };
    *prev = Some(if implicit_m {
        if abs {
            b'M'
        } else {
            b'm'
        }
    } else {
        cmd
    });
    Some(seg)
}

/// What the simplifier carries from one segment to the next.
struct Simplifier {
    /// The current point.
    px: f64,
    py: f64,
    /// The start of the current subpath.
    mx: f64,
    my: f64,
    /// The previous raw segment, made absolute, for S/T reflection.
    prev_raw: Raw,
    /// The last simplified segment was a `Close`.
    prev_close: bool,
    /// The last smooth-quadratic control point (svgtypes' `prev_tx/ty`).
    last_t: (f64, f64),
}

impl Simplifier {
    fn new() -> Self {
        Simplifier {
            px: 0.0,
            py: 0.0,
            mx: 0.0,
            my: 0.0,
            prev_raw: Raw::M(true, 0.0, 0.0),
            prev_close: false,
            last_t: (0.0, 0.0),
        }
    }

    /// Simplifies one raw segment into `emit`.
    fn push(&mut self, seg: Raw, emit: &mut impl FnMut(Seg)) {
        let (px, py) = (self.px, self.py);
        let (mx, my) = (self.mx, self.my);
        let mut last = None;
        let mut put = |s: Seg| {
            last = Some(s);
            emit(s);
        };
        if self.prev_close && !matches!(seg, Raw::M(..) | Raw::Z) {
            put(Seg::MoveTo(mx, my));
        }
        match seg {
            Raw::M(abs, mut x, mut y) => {
                if !abs {
                    if self.prev_close {
                        x += mx;
                        y += my;
                    } else {
                        x += px;
                        y += py;
                    }
                }
                put(Seg::MoveTo(x, y));
                self.prev_raw = seg;
            }
            Raw::L(abs, mut x, mut y) => {
                if !abs {
                    x += px;
                    y += py;
                }
                put(Seg::LineTo(x, y));
                self.prev_raw = seg;
            }
            Raw::H(abs, mut x) => {
                if !abs {
                    x += px;
                }
                put(Seg::LineTo(x, py));
                self.prev_raw = seg;
            }
            Raw::V(abs, mut y) => {
                if !abs {
                    y += py;
                }
                put(Seg::LineTo(px, y));
                self.prev_raw = seg;
            }
            Raw::C(abs, mut a) => {
                if !abs {
                    for i in 0..3 {
                        a[i * 2] += px;
                        a[i * 2 + 1] += py;
                    }
                }
                put(Seg::Cubic(a[0], a[1], a[2], a[3], a[4], a[5]));
                self.prev_raw = Raw::C(true, a);
            }
            Raw::S(abs, mut a) => {
                let (x1, y1) = match self.prev_raw {
                    Raw::C(_, c) => (c[4] * 2.0 - c[2], c[5] * 2.0 - c[3]),
                    Raw::S(_, c) => (c[2] * 2.0 - c[0], c[3] * 2.0 - c[1]),
                    _ => (px, py),
                };
                if !abs {
                    a[0] += px;
                    a[1] += py;
                    a[2] += px;
                    a[3] += py;
                }
                put(Seg::Cubic(x1, y1, a[0], a[1], a[2], a[3]));
                self.prev_raw = Raw::S(true, a);
            }
            Raw::Q(abs, mut a) => {
                if !abs {
                    a[0] += px;
                    a[1] += py;
                    a[2] += px;
                    a[3] += py;
                }
                put(Seg::Quad(a[0], a[1], a[2], a[3]));
                self.prev_raw = Raw::Q(true, a);
            }
            Raw::T(abs, mut x, mut y) => {
                // svgtypes tracks the last T control point separately.
                let (x1, y1) = match self.prev_raw {
                    Raw::Q(_, c) => (c[2] * 2.0 - c[0], c[3] * 2.0 - c[1]),
                    Raw::T(..) => (px * 2.0 - self.last_t.0, py * 2.0 - self.last_t.1),
                    _ => (px, py),
                };
                self.last_t = (x1, y1);
                if !abs {
                    x += px;
                    y += py;
                }
                put(Seg::Quad(x1, y1, x, y));
                self.prev_raw = Raw::T(true, x, y);
            }
            Raw::A(abs, [rx, ry, rot], la, sw, mut x, mut y) => {
                if !abs {
                    x += px;
                    y += py;
                }
                arc_to_cubics(px, py, rx, ry, rot, la, sw, x, y, &mut put);
                self.prev_raw = seg;
            }
            Raw::Z => {
                if !self.prev_close {
                    put(Seg::Close);
                }
                self.prev_raw = seg;
            }
        }
        if let Some(last) = last {
            self.prev_close = last == Seg::Close;
            match last {
                Seg::MoveTo(x, y) => {
                    self.px = x;
                    self.py = y;
                    self.mx = x;
                    self.my = y;
                }
                Seg::LineTo(x, y) | Seg::Quad(_, _, x, y) | Seg::Cubic(_, _, _, _, x, y) => {
                    self.px = x;
                    self.py = y;
                }
                Seg::Close => {
                    self.px = mx;
                    self.py = my;
                }
            }
        }
    }
}

/// Parses and simplifies path data the way `svgtypes`' simplifying parser
/// does (absolute coordinates, H/V to L, S/T to C/Q, arcs to cubics, an
/// implicit `M` after `Z`), handing each simplified segment to `emit` as it
/// is parsed: nothing is buffered, so a path costs no memory here beyond
/// what the receiver keeps. Stops at the first error.
pub(crate) fn path_data_with(text: &str, emit: &mut impl FnMut(Seg)) {
    let mut s = Stream::new(text);
    let mut prev = None;
    let mut state = Simplifier::new();
    while let Some(seg) = parse_raw_seg(&mut s, &mut prev) {
        state.push(seg, emit);
    }
}

/// `path_data_with` collected into a `Vec`.
#[cfg(test)]
pub(crate) fn path_data(text: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    path_data_with(text, &mut |s| out.push(s));
    out
}

/// An SVG elliptical arc to cubic Béziers, exactly as `kurbo` 0.13
/// (`Arc::from_svg_arc` + `to_cubic_beziers(0.1)`) computes it, so shapes
/// match resvg's geometry bit for bit. The subdivision count is capped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn arc_to_cubics(
    fx: f64,
    fy: f64,
    rx: f64,
    ry: f64,
    rot_deg: f64,
    large_arc: bool,
    sweep: bool,
    x: f64,
    y: f64,
    out: &mut impl FnMut(Seg),
) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let x_rotation = rot_deg.to_radians();
    if rx.abs() <= 1e-5 || ry.abs() <= 1e-5 || (fx == x && fy == y) {
        out(Seg::LineTo(x, y));
        return;
    }
    let mut rx = rx.abs();
    let mut ry = ry.abs();
    let xr = x_rotation % (2.0 * PI);
    let (sin_phi, cos_phi) = xr.sin_cos();
    let hd_x = (fx - x) * 0.5;
    let hd_y = (fy - y) * 0.5;
    let hs_x = (fx + x) * 0.5;
    let hs_y = (fy + y) * 0.5;
    let p = (
        cos_phi * hd_x + sin_phi * hd_y,
        -sin_phi * hd_x + cos_phi * hd_y,
    );
    let rf = p.0 * p.0 / (rx * rx) + p.1 * p.1 / (ry * ry);
    if rf > 1.0 {
        let scale = rf.sqrt();
        rx *= scale;
        ry *= scale;
    }
    let rxry = rx * ry;
    let rxpy = rx * p.1;
    let rypx = ry * p.0;
    let sum_of_sq = rxpy * rxpy + rypx * rypx;
    if sum_of_sq == 0.0 || !sum_of_sq.is_finite() {
        out(Seg::LineTo(x, y));
        return;
    }
    let sign_coe = if large_arc == sweep { -1.0 } else { 1.0 };
    let coe = sign_coe * ((rxry * rxry - sum_of_sq) / sum_of_sq).abs().sqrt();
    let tcx = coe * rxpy / ry;
    let tcy = -coe * rypx / rx;
    let center = (
        cos_phi * tcx - sin_phi * tcy + hs_x,
        sin_phi * tcx + cos_phi * tcy + hs_y,
    );
    let start_v = ((p.0 - tcx) / rx, (p.1 - tcy) / ry);
    let end_v = ((-p.0 - tcx) / rx, (-p.1 - tcy) / ry);
    let start_angle = start_v.1.atan2(start_v.0);
    let mut sweep_angle = (end_v.1.atan2(end_v.0) - start_angle) % (2.0 * PI);
    if sweep && sweep_angle < 0.0 {
        sweep_angle += 2.0 * PI;
    } else if !sweep && sweep_angle > 0.0 {
        sweep_angle -= 2.0 * PI;
    }

    // kurbo `Arc::append_iter(0.1)`.
    let tolerance = 0.1;
    let sign = sweep_angle.signum();
    let scaled_err = rx.max(ry) / tolerance;
    let n_err = (1.1163 * scaled_err).powf(1.0 / 6.0).max(3.999_999);
    let n = (n_err * sweep_angle.abs() * (1.0 / (2.0 * PI))).ceil();
    if !n.is_finite() || n < 1.0 {
        out(Seg::LineTo(x, y));
        return;
    }
    // A finite cap: a 1e300 radius would otherwise ask for ~1e50 segments.
    let n = n.min(4096.0);
    let angle_step = sweep_angle / n;
    let n = n as usize;
    let arm_len = (4.0 / 3.0) * (0.25 * angle_step).abs().tan() * sign;
    let sample = |angle: f64| -> (f64, f64) {
        let (s, c) = angle.sin_cos();
        let u = rx * c;
        let v = ry * s;
        let (rs, rc) = x_rotation.sin_cos();
        (u * rc - v * rs, u * rs + v * rc)
    };
    let mut angle0 = start_angle;
    let mut p0 = sample(angle0);
    for _ in 0..n {
        let angle1 = angle0 + angle_step;
        let d0 = sample(angle0 + FRAC_PI_2);
        let p1 = (p0.0 + arm_len * d0.0, p0.1 + arm_len * d0.1);
        let p3 = sample(angle1);
        let d1 = sample(angle1 + FRAC_PI_2);
        let p2 = (p3.0 - arm_len * d1.0, p3.1 - arm_len * d1.1);
        angle0 = angle1;
        p0 = p3;
        out(Seg::Cubic(
            center.0 + p1.0,
            center.1 + p1.1,
            center.0 + p2.0,
            center.1 + p2.1,
            center.0 + p3.0,
            center.1 + p3.1,
        ));
    }
}

/// Case-insensitive prefix check used by the data-URI parser.
pub(crate) fn starts_with_ci(text: &str, prefix: &str) -> bool {
    Stream::new(text).starts_with_ci(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(number("10"), Some(10.0));
        assert_eq!(number(" -1.5e2 "), Some(-150.0));
        assert_eq!(number(".5"), Some(0.5));
        assert_eq!(number("1e400"), None);
        assert_eq!(number("abc"), None);
        assert_eq!(length("1em").map(|l| l.unit), Some(Unit::Em));
        assert_eq!(length("10%").map(|l| l.number), Some(10.0));
    }

    /// The scanning fast path against the standard library's parser, bit for
    /// bit, on edge cases and on generated tokens of every shape.
    #[test]
    fn number_fast_path_matches_std() {
        fn check(t: &str) {
            let mut s = Stream::new(t);
            let got = s.parse_number();
            assert!(s.at_end(), "{t}");
            let want = t.parse::<f64>().ok().filter(|v| v.is_finite());
            assert_eq!(got.map(f64::to_bits), want.map(f64::to_bits), "{t}");
        }
        for t in [
            "0",
            "-0",
            "+0",
            "-0.0e5",
            "1.",
            ".5",
            "-.5e-3",
            "9007199254740992",
            "9007199254740993",
            "9007199254740993e1",
            "1e22",
            "1e23",
            "1e-22",
            "1e-23",
            "123456789012345678",
            "999999999999999999",
            "1000000000000000000",
            "1234567890123456789",
            "12345678901234567890",
            "0.000000000000000000001",
            "0.0000000000000000000001",
            "00000000000000000000001",
            "1e00000000000000000002",
            "1e-00000000000000000002",
            "179769313486231570e291",
            "4.9e-324",
            "1e400",
            "1e-400",
            "1e99999999999",
            "2.2250738585072011e-308",
            "0.1",
            "0.3",
            "123.456",
            "5e-1",
        ] {
            check(t);
        }
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..200_000 {
            let r = next();
            let mut t = String::new();
            match r % 3 {
                0 => t.push('-'),
                1 if r % 7 == 0 => t.push('+'),
                _ => {}
            }
            let int_digits = (r >> 4) % 21;
            let frac_digits = (r >> 10) % 21;
            for _ in 0..int_digits {
                t.push(char::from(b'0' + (next() % 10) as u8));
            }
            if frac_digits > 0 || int_digits == 0 {
                t.push('.');
                for _ in 0..frac_digits.max(1) {
                    t.push(char::from(b'0' + (next() % 10) as u8));
                }
            }
            if (r >> 20) % 4 == 0 {
                t.push(if (r >> 22) % 2 == 0 { 'e' } else { 'E' });
                match (r >> 23) % 3 {
                    0 => t.push('-'),
                    1 => t.push('+'),
                    _ => {}
                }
                t.push_str(&((r >> 26) % 40).to_string());
            }
            check(&t);
        }
    }

    #[test]
    fn colors() {
        assert_eq!(
            color("#f00"),
            Some(Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            })
        );
        assert_eq!(color("rgb(0, 128, 255)").map(|c| c.g), Some(128));
        assert_eq!(color("RED").map(|c| c.r), Some(255));
        assert_eq!(color("hsl(120, 100%, 50%)").map(|c| c.g), Some(255));
        assert_eq!(color("nonsense"), None);
    }

    #[test]
    fn paths() {
        let p = path_data("M10 10 h 20 v20 z m 5 5 l 1 1");
        assert_eq!(p[0], Seg::MoveTo(10.0, 10.0));
        assert_eq!(p[1], Seg::LineTo(30.0, 10.0));
        assert_eq!(p[2], Seg::LineTo(30.0, 30.0));
        assert_eq!(p[3], Seg::Close);
        assert_eq!(p[4], Seg::MoveTo(15.0, 15.0));
        assert_eq!(p[5], Seg::LineTo(16.0, 16.0));
        // Stops at the first error, keeps what came before.
        assert_eq!(path_data("M 10 20 L 30 40 L 50").len(), 2);
        assert!(path_data("L 10 10").is_empty());
        // Compact arc flags.
        assert!(path_data("M0 0a10 10 0 1110 10").len() > 1);
    }

    /// The two-pass parser `path_data_with` replaced (every raw segment into a
    /// `Vec`, then every simplified segment into another), kept as its oracle.
    fn parse_raw_two_pass(text: &str) -> Vec<Raw> {
        let mut s = Stream::new(text);
        let mut out = Vec::new();
        let mut prev: Option<u8> = None;
        loop {
            s.skip_spaces();
            let Some(c) = s.peek() else { break };
            let cmd;
            let mut implicit_m = false;
            if is_cmd(c) {
                if prev.is_none() && !c.eq_ignore_ascii_case(&b'm') {
                    break;
                }
                cmd = c;
                s.advance(1);
            } else if matches!(c, b'0'..=b'9' | b'.' | b'-' | b'+') {
                match prev {
                    Some(p) if !p.eq_ignore_ascii_case(&b'z') => {
                        // Implicit repetition; a repeated M/m is an L/l.
                        if p == b'M' {
                            cmd = b'L';
                            implicit_m = true;
                        } else if p == b'm' {
                            cmd = b'l';
                            implicit_m = true;
                        } else {
                            cmd = p;
                        }
                    }
                    _ => break,
                }
            } else {
                break;
            }
            let abs = cmd.is_ascii_uppercase();
            let seg = (|| -> Option<Raw> {
                Some(match cmd.to_ascii_lowercase() {
                    b'm' => Raw::M(abs, s.parse_list_number()?, s.parse_list_number()?),
                    b'l' => Raw::L(abs, s.parse_list_number()?, s.parse_list_number()?),
                    b'h' => Raw::H(abs, s.parse_list_number()?),
                    b'v' => Raw::V(abs, s.parse_list_number()?),
                    b'c' => {
                        let mut a = [0.0; 6];
                        for v in &mut a {
                            *v = s.parse_list_number()?;
                        }
                        Raw::C(abs, a)
                    }
                    b's' | b'q' => {
                        let mut a = [0.0; 4];
                        for v in &mut a {
                            *v = s.parse_list_number()?;
                        }
                        if cmd.eq_ignore_ascii_case(&b's') {
                            Raw::S(abs, a)
                        } else {
                            Raw::Q(abs, a)
                        }
                    }
                    b't' => Raw::T(abs, s.parse_list_number()?, s.parse_list_number()?),
                    b'a' => {
                        let rx = s.parse_list_number()?;
                        let ry = s.parse_list_number()?;
                        let rot = s.parse_list_number()?;
                        let la = parse_flag(&mut s)?;
                        let sw = parse_flag(&mut s)?;
                        let x = s.parse_list_number()?;
                        let y = s.parse_list_number()?;
                        Raw::A(abs, [rx, ry, rot], la, sw, x, y)
                    }
                    _ => Raw::Z,
                })
            })();
            let Some(seg) = seg else { break };
            out.push(seg);
            prev = Some(if implicit_m {
                if abs {
                    b'M'
                } else {
                    b'm'
                }
            } else {
                cmd
            });
        }
        out
    }

    fn path_data_two_pass(text: &str) -> Vec<Seg> {
        let raw = parse_raw_two_pass(text);
        let mut out: Vec<Seg> = Vec::with_capacity(raw.len());
        let (mut px, mut py) = (0.0f64, 0.0f64);
        let (mut mx, mut my) = (0.0f64, 0.0f64);
        // The previous raw segment, made absolute, for S/T reflection.
        let mut prev_raw = Raw::M(true, 0.0, 0.0);
        let mut prev_simple: Option<Seg> = None;
        // The last smooth-quadratic control point (svgtypes' `prev_tx/ty`).
        let mut last_t = (0.0f64, 0.0f64);
        for seg in raw {
            let start_len = out.len();
            if prev_simple == Some(Seg::Close) && !matches!(seg, Raw::M(..) | Raw::Z) {
                out.push(Seg::MoveTo(mx, my));
                prev_simple = Some(Seg::MoveTo(mx, my));
            }
            match seg {
                Raw::M(abs, mut x, mut y) => {
                    if !abs {
                        if prev_simple == Some(Seg::Close) {
                            x += mx;
                            y += my;
                        } else {
                            x += px;
                            y += py;
                        }
                    }
                    out.push(Seg::MoveTo(x, y));
                    prev_raw = seg;
                }
                Raw::L(abs, mut x, mut y) => {
                    if !abs {
                        x += px;
                        y += py;
                    }
                    out.push(Seg::LineTo(x, y));
                    prev_raw = seg;
                }
                Raw::H(abs, mut x) => {
                    if !abs {
                        x += px;
                    }
                    out.push(Seg::LineTo(x, py));
                    prev_raw = seg;
                }
                Raw::V(abs, mut y) => {
                    if !abs {
                        y += py;
                    }
                    out.push(Seg::LineTo(px, y));
                    prev_raw = seg;
                }
                Raw::C(abs, mut a) => {
                    if !abs {
                        for i in 0..3 {
                            a[i * 2] += px;
                            a[i * 2 + 1] += py;
                        }
                    }
                    out.push(Seg::Cubic(a[0], a[1], a[2], a[3], a[4], a[5]));
                    prev_raw = Raw::C(true, a);
                }
                Raw::S(abs, mut a) => {
                    let (x1, y1) = match prev_raw {
                        Raw::C(_, c) => (c[4] * 2.0 - c[2], c[5] * 2.0 - c[3]),
                        Raw::S(_, c) => (c[2] * 2.0 - c[0], c[3] * 2.0 - c[1]),
                        _ => (px, py),
                    };
                    if !abs {
                        a[0] += px;
                        a[1] += py;
                        a[2] += px;
                        a[3] += py;
                    }
                    out.push(Seg::Cubic(x1, y1, a[0], a[1], a[2], a[3]));
                    prev_raw = Raw::S(true, a);
                }
                Raw::Q(abs, mut a) => {
                    if !abs {
                        a[0] += px;
                        a[1] += py;
                        a[2] += px;
                        a[3] += py;
                    }
                    out.push(Seg::Quad(a[0], a[1], a[2], a[3]));
                    prev_raw = Raw::Q(true, a);
                }
                Raw::T(abs, mut x, mut y) => {
                    // svgtypes tracks the last T control point separately.
                    let (x1, y1) = match prev_raw {
                        Raw::Q(_, c) => (c[2] * 2.0 - c[0], c[3] * 2.0 - c[1]),
                        Raw::T(..) => (px * 2.0 - last_t.0, py * 2.0 - last_t.1),
                        _ => (px, py),
                    };
                    last_t = (x1, y1);
                    if !abs {
                        x += px;
                        y += py;
                    }
                    out.push(Seg::Quad(x1, y1, x, y));
                    prev_raw = Raw::T(true, x, y);
                }
                Raw::A(abs, [rx, ry, rot], la, sw, mut x, mut y) => {
                    if !abs {
                        x += px;
                        y += py;
                    }
                    arc_to_cubics(px, py, rx, ry, rot, la, sw, x, y, &mut |s| out.push(s));
                    prev_raw = seg;
                }
                Raw::Z => {
                    if prev_simple != Some(Seg::Close) {
                        out.push(Seg::Close);
                    }
                    prev_raw = seg;
                }
            }
            if out.len() > start_len {
                let last = out[out.len() - 1];
                prev_simple = Some(last);
                match last {
                    Seg::MoveTo(x, y) => {
                        px = x;
                        py = y;
                        mx = x;
                        my = y;
                    }
                    Seg::LineTo(x, y) | Seg::Quad(_, _, x, y) | Seg::Cubic(_, _, _, _, x, y) => {
                        px = x;
                        py = y;
                    }
                    Seg::Close => {
                        px = mx;
                        py = my;
                    }
                }
            }
        }
        out
    }

    /// The streaming parser against the two-pass one it replaced.
    #[test]
    fn path_stream_matches_two_pass() {
        fn check(d: &str) {
            let got = format!("{:?}", path_data(d));
            let want = format!("{:?}", path_data_two_pass(d));
            assert_eq!(got, want, "{d}");
        }
        for d in [
            "",
            "M10 10 h 20 v20 z m 5 5 l 1 1",
            "M 10 20 L 30 40 L 50",
            "L 10 10",
            "M0 0a10 10 0 1110 10",
            "m1 2 3 4 5 6z7 8 9 10",
            "M1 2zL3 4",
            "M1 2zzm3 4zl1 1",
            "M0 0C1 1 2 2 3 3S5 5 6 6s1 1 2 2",
            "M0 0S1 1 2 2",
            "M0 0Q1 1 2 2T4 4t1 1T9 9",
            "M0 0T1 1",
            "M0 0L1 1T2 2",
            "M0 0A5 5 0 0 1 10 0a5 5 30 1 0 -10 0Z",
            "M0 0A0 5 0 0 1 10 0",
            "M0 0A5 5 0 0 1 0 0",
            "M0 0a1e300 1 0 0 1 1 1",
            "M0 0a5 5 0 2 1 1 1",
            "M0 0H10V10h-5v-5Z",
            "M0,0 10,10-5-5.5.5",
            "M0 0 L",
            "M0 0 X 1 1",
            "M0 0zm1 1",
            "M0 0zM1 1",
            "M0 0z 1 1",
            "z",
            "M 1e400 0 L 1 1",
            "M1 1 m2 2 l3 3 M4 4 5 5",
        ] {
            check(d);
        }
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let cmds = b"MmLlHhVvCcSsQqTtAaZz";
        for _ in 0..20_000 {
            let mut d = String::from(if next() % 16 == 0 { "" } else { "M 3 4 " });
            for _ in 0..(next() % 12) {
                let c = cmds[(next() % cmds.len() as u64) as usize];
                d.push(char::from(c));
                let want = match c.to_ascii_lowercase() {
                    b'm' | b'l' | b't' => 2,
                    b'h' | b'v' => 1,
                    b'c' => 6,
                    b's' | b'q' => 4,
                    b'a' => 7,
                    _ => 0,
                };
                // Sometimes one number short, or more (implicit repetition).
                let n = match next() % 10 {
                    0 if want > 0 => want - 1,
                    1 => want * 2,
                    _ => want,
                };
                for i in 0..n {
                    let v = (next() % 2000) as f64 / 10.0 - 100.0;
                    if c.eq_ignore_ascii_case(&b'a') && (i % 7 == 3 || i % 7 == 4) {
                        d.push_str(if next() % 2 == 0 { " 0" } else { " 1" });
                    } else {
                        d.push_str(&format!(" {v}"));
                    }
                }
                d.push(' ');
            }
            check(&d);
        }
    }

    #[test]
    fn transforms() {
        let m = transform("translate(10,20) scale(2)").unwrap();
        assert_eq!(m, [2.0, 0.0, 0.0, 2.0, 10.0, 20.0]);
        assert!(transform("rotate(").is_none());
        assert!(transform("matrix(1 2 3)").is_none());
    }
}
