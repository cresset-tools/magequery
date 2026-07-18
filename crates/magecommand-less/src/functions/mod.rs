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
//! A registered function returning `None` (wrong argument types — less.js's
//! caught `throw`) also falls through to the passthrough rule; that is how
//! `saturate(5%)` / `contrast(30%)` survive as plain CSS filters and how
//! incompatible-unit `min()`/`max()` emit literally (§4.8).

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
use crate::error::LessError;
use crate::value::Dimension;

/// Dispatch a built-in function by (lowercased) name over already-evaluated
/// arguments. Returns `Ok(None)` for an unregistered name — or a registered one
/// whose arguments don't fit — so the caller falls through to the passthrough
/// rule (plan §2.7/§4.8).
pub fn dispatch(name: &str, args: &[Node], np: u8) -> Result<Option<Node>, LessError> {
    // less.js turns named-color keywords into `Color` nodes at *parse* time; we
    // keep them as keywords until an operation needs them, so the function
    // boundary applies the same coercion (`lighten(blue, 10%)`, `iscolor(red)`).
    let coerced: Vec<Node> = args.iter().cloned().map(coerce_keyword_color).collect();
    let a = coerced.as_slice();
    let out = match name {
        // --- string (plan §2.14) ---
        "e" => string::e(a),
        "escape" => string::escape(a),
        "%" => string::format(a, np),
        "replace" => string::replace(a, np),

        // --- list ---
        "length" => list::length(a),
        "extract" => list::extract(a),
        "range" => list::range(a),
        "~" => list::tilde(a),
        "_self" => a.first().cloned(),

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
        "min" => number::min_max(a, true, np),
        "max" => number::min_max(a, false, np),
        "convert" => number::convert(a),
        "pi" => Some(Node::Dimension(Dimension::number(std::f64::consts::PI))),
        "mod" => number::modulo(a),
        "pow" => number::pow(a),

        // --- type checks / units (plan §2.6) ---
        "isruleset" => Some(types::bool_keyword(matches!(
            a.first(),
            Some(Node::DetachedRuleset { .. })
        ))),
        "iscolor" => Some(types::bool_keyword(matches!(a.first(), Some(Node::Color(_))))),
        "isnumber" => Some(types::bool_keyword(matches!(a.first(), Some(Node::Dimension(_))))),
        "isstring" => Some(types::bool_keyword(matches!(a.first(), Some(Node::Quoted { .. })))),
        "iskeyword" => Some(types::bool_keyword(matches!(a.first(), Some(Node::Keyword(_))))),
        "isurl" => Some(types::bool_keyword(matches!(a.first(), Some(Node::Url(_))))),
        "ispixel" => Some(types::bool_keyword(types::is_unit(a.first(), "px"))),
        "isem" => Some(types::bool_keyword(types::is_unit(a.first(), "em"))),
        "ispercentage" => Some(types::bool_keyword(types::is_unit(a.first(), "%"))),
        "isunit" => Some(types::bool_keyword(types::isunit(a))),
        "unit" => types::unit(a, np),
        "get-unit" => types::get_unit(a),

        // --- color definition ---
        "rgb" => color_def::rgb(a),
        "rgba" => color_def::rgba(a),
        "hsl" => color_def::hsl(a),
        "hsla" => color_def::hsla(a),
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
    Ok(out)
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

/// less.js color.js `number()` — a `%` dimension scales to `0..1`, else value.
pub(crate) fn number(n: &Node) -> Option<f64> {
    if let Node::Dimension(d) = n {
        Some(if d.unit.is("%") {
            d.value / 100.0
        } else {
            d.value
        })
    } else {
        None
    }
}

/// less.js color.js `scaled(n, size)` — a `%` dimension scales to `0..size`.
pub(crate) fn scaled(n: &Node, size: f64) -> Option<f64> {
    if let Node::Dimension(d) = n {
        if d.unit.is("%") {
            return Some(d.value * size / 100.0);
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

/// Clamp to `0..=1` (less.js color.js `clamp`).
pub(crate) fn clamp01(v: f64) -> f64 {
    v.max(0.0).min(1.0)
}
