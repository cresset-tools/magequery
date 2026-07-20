//! Built-in function registry (plan §2.7), mirroring less.js v4.6.7's
//! `lib/less/functions/*` byte-semantics (verified against the vendored source,
//! not remembered formulas).
//!
//! The default `Call.eval` fallthrough — unknown CSS functions re-emit
//! `name(evaluated-args)` verbatim (plan §2.7/§4.8) — is handled by the caller
//! ([`crate::eval`]) when [`dispatch`] returns `None`. Functions with lazy /
//! condition arguments (`if`, `boolean`, `isdefined`) and the file-reading
//! resource functions (`data-uri`, `image-size`) are dispatched in the
//! evaluator, which owns the scope frames and the [`crate::resolver`] boundary.
//!
//! A registered function returning `Ok(None)` falls through to the passthrough
//! rule — but ONLY where less.js itself null-returns or catches: `min`/`max`
//! (their whole body is try/caught), `rgb[a]`/`hsl[a]` (internal try/catch),
//! the `saturate(5%)` / `contrast(30%)` `!color.rgb → null` filter carve-outs,
//! and out-of-range `extract()`. Every OTHER wrong-arity/wrong-type call
//! **throws** in less.js and must be a compile `Err` here, wrapped as
//! ``Error evaluating function `name`: <message>`` (Phase 3 review, C9/F4/F16).

pub mod boolean;
pub mod color_blend;
pub mod color_channel;
pub mod color_def;
pub mod color_op;
pub mod data_uri;
pub mod default_fn;
pub mod list;
pub mod math;
pub mod misc;
pub mod number;
pub mod passthrough;
pub mod string;
pub mod svg;
pub mod types;

use crate::ast::Node;
use crate::color::Color;
use crate::error::{ErrorKind, LessError};
use crate::unit::Unit;
use crate::value::Dimension;

/// A registered function's outcome: `Ok(None)` = fall through to the
/// passthrough rule; `Err` = less.js's propagated throw (a compile error,
/// wrapped by [`dispatch`] as ``Error evaluating function `name`: …``).
pub(crate) type FnResult = Result<Option<Node>, LessError>;

/// Dispatch a built-in function by (lowercased) name over already-evaluated
/// arguments. Returns `Ok(None)` for an unregistered name — or a registered one
/// whose non-fit is one less.js *catches* — so the caller falls through to the
/// passthrough rule (plan §2.7/§4.8). Registered functions whose throw
/// propagates in less.js return `Err`.
pub fn dispatch(
    name: &str,
    args: &[Node],
    np: u8,
    compress: bool,
    keep_zero_units: bool,
) -> Result<Option<Node>, LessError> {
    // less.js turns named-color keywords into `Color` nodes at *parse* time; we
    // keep them as keywords until an operation needs them, so the function
    // boundary applies the same coercion (`lighten(blue, 10%)`, `iscolor(red)`).
    let coerced: Vec<Node> = args.iter().cloned().map(coerce_keyword_color).collect();
    let a = coerced.as_slice();
    let out: FnResult = match name {
        // --- string (plan §2.14) ---
        "e" => string::e(a),
        "escape" => string::escape(a),
        "%" => string::format(a),
        "replace" => string::replace(a),

        // --- list ---
        "length" => list::length(a),
        "extract" => list::extract(a),
        "range" => Ok(list::range(a)),
        "~" => Ok(list::tilde(a)),
        "_self" => Ok(a.first().cloned()),

        // --- math ---
        "ceil" => math::unary(a, f64::ceil, math::UnitRule::Keep),
        "floor" => math::unary(a, f64::floor, math::UnitRule::Keep),
        "sqrt" => math::unary(a, f64::sqrt, math::UnitRule::Keep),
        "abs" => math::unary(a, f64::abs, math::UnitRule::Keep),
        "tan" => math::unary(a, f64::tan, math::UnitRule::Strip),
        "sin" => math::unary(a, f64::sin, math::UnitRule::Strip),
        "cos" => math::unary(a, f64::cos, math::UnitRule::Strip),
        "atan" => math::unary(a, f64::atan, math::UnitRule::Rad),
        "asin" => math::unary(a, f64::asin, math::UnitRule::Rad),
        "acos" => math::unary(a, f64::acos, math::UnitRule::Rad),
        "round" => math::round(a),

        // --- number ---
        "percentage" => number::percentage(a),
        "min" => Ok(number::min_max(a, true, np, compress, keep_zero_units)),
        "max" => Ok(number::min_max(a, false, np, compress, keep_zero_units)),
        "convert" => number::convert(a),
        "pi" => Ok(Some(Node::Dimension(Dimension::number(std::f64::consts::PI)))),
        "mod" => number::modulo(a),
        "pow" => number::pow(a),

        // --- type checks / units (plan §2.6) ---
        "isruleset" => Ok(Some(types::bool_keyword(matches!(
            a.first(),
            Some(Node::DetachedRuleset { .. })
                | Some(Node::Closure { .. })
        )))),
        "iscolor" => Ok(Some(types::bool_keyword(matches!(a.first(), Some(Node::Color(_)))))),
        "isnumber" => Ok(Some(types::bool_keyword(matches!(a.first(), Some(Node::Dimension(_)))))),
        "isstring" => Ok(Some(types::bool_keyword(matches!(a.first(), Some(Node::Quoted { .. }))))),
        "iskeyword" => Ok(Some(types::bool_keyword(matches!(a.first(), Some(Node::Keyword(_)))))),
        "isurl" => Ok(Some(types::bool_keyword(matches!(a.first(), Some(Node::Url(_)))))),
        "ispixel" => Ok(Some(types::bool_keyword(types::is_unit(a.first(), "px")))),
        "isem" => Ok(Some(types::bool_keyword(types::is_unit(a.first(), "em")))),
        "ispercentage" => Ok(Some(types::bool_keyword(types::is_unit(a.first(), "%")))),
        "isunit" => Ok(Some(types::bool_keyword(types::isunit(a)))),
        "unit" => types::unit(a),
        "get-unit" => Ok(types::get_unit(a)),

        // --- color definition ---
        "rgb" => Ok(color_def::rgb(a)),
        "rgba" => Ok(color_def::rgba(a)),
        "hsl" => Ok(color_def::hsl(a)),
        "hsla" => Ok(color_def::hsla(a)),
        "hsv" => color_def::hsv(a),
        "hsva" => color_def::hsva(a),
        "argb" => color_def::argb(a),
        "color" => color_def::color(a),

        // --- color channels ---
        "hue" => color_channel::hsl_channel(a, color_channel::Hsl::Hue),
        "saturation" => color_channel::hsl_channel(a, color_channel::Hsl::Saturation),
        "lightness" => color_channel::hsl_channel(a, color_channel::Hsl::Lightness),
        "hsvhue" => color_channel::hsv_channel(a, color_channel::Hsv::Hue),
        "hsvsaturation" => color_channel::hsv_channel(a, color_channel::Hsv::Saturation),
        "hsvvalue" => color_channel::hsv_channel(a, color_channel::Hsv::Value),
        "red" => color_channel::rgb_channel(a, 0),
        "green" => color_channel::rgb_channel(a, 1),
        "blue" => color_channel::rgb_channel(a, 2),
        "alpha" => color_channel::alpha(a),
        "luma" => color_channel::luma(a),
        "luminance" => color_channel::luminance(a),

        // --- color operations ---
        "saturate" => color_op::adjust_hsl(a, color_op::Adjust::Saturate),
        "desaturate" => color_op::adjust_hsl(a, color_op::Adjust::Desaturate),
        "lighten" => color_op::adjust_hsl(a, color_op::Adjust::Lighten),
        "darken" => color_op::adjust_hsl(a, color_op::Adjust::Darken),
        "fadein" => color_op::adjust_hsl(a, color_op::Adjust::FadeIn),
        "fadeout" => color_op::adjust_hsl(a, color_op::Adjust::FadeOut),
        "fade" => color_op::fade(a),
        "spin" => color_op::spin(a),
        "mix" => color_op::mix_fn(a),
        "greyscale" => color_op::greyscale(a),
        "contrast" => color_op::contrast(a),
        "tint" => color_op::tint(a),
        "shade" => color_op::shade(a),

        // --- color blending ---
        "multiply" => color_blend::blend(a, color_blend::Mode::Multiply),
        "screen" => color_blend::blend(a, color_blend::Mode::Screen),
        "overlay" => color_blend::blend(a, color_blend::Mode::Overlay),
        "softlight" => color_blend::blend(a, color_blend::Mode::SoftLight),
        "hardlight" => color_blend::blend(a, color_blend::Mode::HardLight),
        "difference" => color_blend::blend(a, color_blend::Mode::Difference),
        "exclusion" => color_blend::blend(a, color_blend::Mode::Exclusion),
        "average" => color_blend::blend(a, color_blend::Mode::Average),
        "negation" => color_blend::blend(a, color_blend::Mode::Negation),

        // --- misc/resource ---
        "svg-gradient" => svg::svg_gradient(a, np),

        _ => return Ok(None),
    };
    // The ``Error evaluating function `name`: …`` wrap now lives at the eval
    // call site (`Ctx::eval_call`), mirroring less.js `Call.eval` — it must
    // cover ARGUMENT-evaluation errors too and carries the call's index.
    out
}

/// Build a `Dimension` node, mirroring the less.js constructor's NaN throw
/// (`Dimension is not a number.` — how `sqrt(-1)`, `mod(7, 0)`, `pow(-1, 0.5)`
/// become compile errors; Infinity is allowed).
pub(crate) fn dim_node(value: f64, unit: Unit) -> Result<Node, LessError> {
    if value.is_nan() {
        return Err(LessError::new(ErrorKind::Runtime, "Dimension is not a number."));
    }
    Ok(Node::Dimension(Dimension { value, unit }))
}

/// JS's arithmetic coercion of a node's `.value` (`amount.value / 100`,
/// `a.value % b.value`): a Dimension gives its number, a string-ish node's text
/// goes through `Number()` (`"20"` → 20, `"banana"` → NaN), anything else is
/// NaN — which then propagates per less.js (`#NaNNaNNaN` channels, NaN-throwing
/// Dimension constructions).
pub(crate) fn js_arg_num(n: &Node) -> f64 {
    match n {
        Node::Dimension(d) => d.value,
        Node::Quoted { value, .. } => js_parse_number(value),
        Node::Keyword(k) => js_parse_number(k),
        Node::Anonymous(s) => js_parse_number(s),
        Node::Color(c) => match &c.original {
            Some(o) => js_parse_number(o),
            None => f64::NAN,
        },
        _ => f64::NAN,
    }
}

/// JS `Number(string)`: trimmed; empty → 0; else parse or NaN.
fn js_parse_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// The JS `Cannot read properties of undefined (reading '<prop>')` TypeError —
/// what less.js hits when a required argument is missing.
pub(crate) fn undef_err(prop: &str) -> LessError {
    LessError::new(
        ErrorKind::Runtime,
        format!("Cannot read properties of undefined (reading '{prop}')"),
    )
}

/// The node as a `Color`, or less.js's `toHSL`/`toHSV` throw
/// (`Argument cannot be evaluated to a color`; a MISSING argument is the
/// `undefined.toHSL` TypeError instead).
pub(crate) fn to_color_err<'a>(n: Option<&'a Node>) -> Result<&'a Color, LessError> {
    match n {
        Some(Node::Color(c)) => Ok(c),
        Some(_) => Err(LessError::new(
            ErrorKind::Runtime,
            "Argument cannot be evaluated to a color",
        )),
        None => Err(undef_err("toHSL")),
    }
}

/// A named-color keyword → `Color` (less.js does this at parse time).
pub(crate) fn coerce_keyword_color(node: Node) -> Node {
    if let Node::Keyword(k) = &node {
        if let Some(c) = Color::from_keyword(k) {
            return Node::Color(c);
        }
    }
    node
}

/// less.js color.js `number()` — a `%` dimension scales to `0..1`, else value;
/// anything else throws (`color functions take numbers as parameters`).
pub(crate) fn number(n: Option<&Node>) -> Result<f64, LessError> {
    if let Some(Node::Dimension(d)) = n {
        Ok(if d.unit.is("%") {
            d.value / 100.0
        } else {
            d.value
        })
    } else {
        Err(LessError::new(
            ErrorKind::Argument,
            "color functions take numbers as parameters",
        ))
    }
}

/// less.js color.js `scaled(n, size)` — a `%` dimension scales to `0..size`.
pub(crate) fn scaled(n: Option<&Node>, size: f64) -> Result<f64, LessError> {
    if let Some(Node::Dimension(d)) = n {
        if d.unit.is("%") {
            return Ok(d.value * size / 100.0);
        }
    }
    number(n)
}

/// The node as a `Dimension`, if it is one.
pub(crate) fn as_dimension(n: &Node) -> Option<&Dimension> {
    match n {
        Node::Dimension(d) => Some(d),
        _ => None,
    }
}

/// The node as a `Color`, if it is one (keywords were already coerced).
pub(crate) fn as_color(n: &Node) -> Option<&Color> {
    match n {
        Node::Color(c) => Some(c),
        _ => None,
    }
}

/// Clamp to `0..=1` (less.js color.js `clamp`). JS `Math.min`/`Math.max`
/// propagate NaN (Rust's `f64::max` would swallow it) — NaN must survive to
/// print `#NaNNaNNaN` like less.js.
pub(crate) fn clamp01(v: f64) -> f64 {
    if v.is_nan() { v } else { v.max(0.0).min(1.0) }
}
