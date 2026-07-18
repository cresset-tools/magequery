//! Built-in function registry (plan §2.7). The less.php `Functions.php` set is
//! the Magento oracle; Luma-called ~20 are staged first, then the tail.
//!
//! The default `Call.eval` fallthrough — unknown CSS functions re-emit
//! `name(evaluated-args)` verbatim (plan §2.7/§4.8) — is handled by the caller
//! ([`crate::eval`]) when [`dispatch`] returns `None`. This step (milestone 1)
//! implements the small subset the VARIABLES / NESTING / OPERATIONS gates need:
//! the color constructors (`rgb`/`rgba`/`hsl`/`hsla`), `unit`, and the core math
//! (`floor`/`ceil`/`round`/`abs`/`sqrt`/`percentage`/`min`/`max`). The full
//! registry (color ops/channels/blends, string, list, data-uri, …) is Phase 3.

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
/// arguments. Returns `Ok(None)` for an unregistered name so the caller falls
/// through to the passthrough rule (plan §2.7).
pub fn dispatch(name: &str, args: &[Node], _np: u8) -> Result<Option<Node>, LessError> {
    let out = match name {
        "rgb" => color_rgb(args),
        "rgba" => color_rgba(args),
        "hsl" => color_hsl(args),
        "hsla" => color_hsla(args),
        "unit" => func_unit(args),
        "floor" => math1(args, f64::floor),
        "ceil" => math1(args, f64::ceil),
        "abs" => math1(args, f64::abs),
        "sqrt" => math1(args, f64::sqrt),
        "round" => func_round(args),
        "percentage" => func_percentage(args),
        "min" => func_minmax(args, true),
        "max" => func_minmax(args, false),
        // Color channels (guard/Luma subset).
        "hue" => color_channel(args, Channel::Hue),
        "saturation" => color_channel(args, Channel::Saturation),
        "lightness" => color_channel(args, Channel::Lightness),
        "red" => rgb_channel(args, 0),
        "green" => rgb_channel(args, 1),
        "blue" => rgb_channel(args, 2),
        // Type-check functions (plan §2.6).
        "iscolor" => Some(bool_keyword(is_color(args.first()))),
        "isnumber" => Some(bool_keyword(matches!(args.first(), Some(Node::Dimension(_))))),
        "isstring" => Some(bool_keyword(matches!(args.first(), Some(Node::Quoted { .. })))),
        "iskeyword" => Some(bool_keyword(matches!(args.first(), Some(Node::Keyword(_))))),
        "isurl" => Some(bool_keyword(matches!(args.first(), Some(Node::Url(_))))),
        "ispixel" => Some(bool_keyword(is_unit(args.first(), "px"))),
        "isem" => Some(bool_keyword(is_unit(args.first(), "em"))),
        "ispercentage" => Some(bool_keyword(is_unit(args.first(), "%"))),
        "isunit" => Some(bool_keyword(func_isunit(args))),
        // String: `e()` / `escape()` unquote (subset).
        "e" => func_e(args),
        _ => return Ok(None),
    };
    Ok(out)
}

/// A color-channel selector for [`color_channel`].
enum Channel {
    Hue,
    Saturation,
    Lightness,
}

/// less.js color-channel functions `hue`/`saturation`/`lightness` (HSL space).
fn color_channel(args: &[Node], ch: Channel) -> Option<Node> {
    let Node::Color(c) = args.first()? else { return None };
    let (h, s, l, _a) = c.to_hsl();
    Some(match ch {
        Channel::Hue => Node::Dimension(Dimension::number(h.round())),
        Channel::Saturation => Node::Dimension(Dimension::with_unit((s * 100.0).round(), "%")),
        Channel::Lightness => Node::Dimension(Dimension::with_unit((l * 100.0).round(), "%")),
    })
}

fn rgb_channel(args: &[Node], idx: usize) -> Option<Node> {
    let Node::Color(c) = args.first()? else { return None };
    Some(Node::Dimension(Dimension::number(c.rgb[idx].round())))
}

fn bool_keyword(b: bool) -> Node {
    Node::Keyword(if b { "true" } else { "false" }.to_string())
}

fn is_unit(n: Option<&Node>, unit: &str) -> bool {
    matches!(n, Some(Node::Dimension(d)) if d.unit.is(unit))
}

/// `iscolor` — a `Color`, or a keyword that names one (`green`, `red`, …), since
/// named colors may still be carried as keywords until an operation coerces them.
fn is_color(n: Option<&Node>) -> bool {
    match n {
        Some(Node::Color(_)) => true,
        Some(Node::Keyword(k)) => Color::from_keyword(k).is_some(),
        _ => false,
    }
}

/// `isunit(value, unit)` — the value is a dimension with the given unit keyword.
fn func_isunit(args: &[Node]) -> bool {
    let unit = match args.get(1) {
        Some(Node::Keyword(k)) => k.clone(),
        Some(Node::Quoted { value, .. }) => value.clone(),
        Some(Node::Anonymous(s)) => s.clone(),
        _ => return false,
    };
    matches!(args.first(), Some(Node::Dimension(d)) if d.unit.is(&unit))
}

/// `e(str)` / `escape` unquote — return the string contents raw (escaped).
fn func_e(args: &[Node]) -> Option<Node> {
    match args.first()? {
        Node::Quoted { value, .. } => Some(Node::Anonymous(value.clone())),
        Node::Anonymous(s) => Some(Node::Anonymous(s.clone())),
        other => Some(Node::Anonymous(crate::css::render_value(other, 8))),
    }
}

/// less.js color `number()` — a `%` dimension scales to `0..1`, else the value.
fn number(n: &Node) -> Option<f64> {
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

/// less.js color `scaled(n, size)` — a `%` dimension scales to `0..size`.
fn scaled(n: &Node, size: f64) -> Option<f64> {
    if let Node::Dimension(d) = n {
        if d.unit.is("%") {
            return Some(d.value * size / 100.0);
        }
    }
    number(n)
}

fn as_dimension(n: &Node) -> Option<&Dimension> {
    match n {
        Node::Dimension(d) => Some(d),
        _ => None,
    }
}

fn color_rgb(args: &[Node]) -> Option<Node> {
    let c = build_rgba(args.first()?, args.get(1)?, args.get(2)?, None)?;
    Some(Node::Color(Color {
        original: Some("rgb".into()),
        ..c
    }))
}

fn color_rgba(args: &[Node]) -> Option<Node> {
    let a = args.get(3);
    let c = build_rgba(args.first()?, args.get(1)?, args.get(2)?, a)?;
    Some(Node::Color(c))
}

fn build_rgba(r: &Node, g: &Node, b: &Node, a: Option<&Node>) -> Option<Color> {
    let rgb = [scaled(r, 255.0)?, scaled(g, 255.0)?, scaled(b, 255.0)?];
    let alpha = match a {
        Some(n) => number(n)?,
        None => 1.0,
    };
    Some(Color {
        rgb,
        alpha,
        original: Some("rgba".into()),
    })
}

fn color_hsl(args: &[Node]) -> Option<Node> {
    let c = build_hsla(args.first()?, args.get(1)?, args.get(2)?, None)?;
    Some(Node::Color(Color {
        original: Some("hsl".into()),
        ..c
    }))
}

fn color_hsla(args: &[Node]) -> Option<Node> {
    let c = build_hsla(args.first()?, args.get(1)?, args.get(2)?, args.get(3))?;
    Some(Node::Color(c))
}

/// less.js `hsla` — HSL→RGB (channels kept as unrounded floats for later math).
fn build_hsla(h: &Node, s: &Node, l: &Node, a: Option<&Node>) -> Option<Color> {
    let h = (number(h)? % 360.0) / 360.0;
    let s = clamp01(number(s)?);
    let l = clamp01(number(l)?);
    let alpha = match a {
        Some(n) => clamp01(number(n)?),
        None => 1.0,
    };
    let m2 = if l <= 0.5 { l * (s + 1.0) } else { l + s - l * s };
    let m1 = l * 2.0 - m2;
    let rgb = [
        hsl_hue(h + 1.0 / 3.0, m1, m2) * 255.0,
        hsl_hue(h, m1, m2) * 255.0,
        hsl_hue(h - 1.0 / 3.0, m1, m2) * 255.0,
    ];
    Some(Color {
        rgb,
        alpha,
        original: Some("hsla".into()),
    })
}

fn hsl_hue(mut h: f64, m1: f64, m2: f64) -> f64 {
    if h < 0.0 {
        h += 1.0;
    } else if h > 1.0 {
        h -= 1.0;
    }
    if h * 6.0 < 1.0 {
        m1 + (m2 - m1) * h * 6.0
    } else if h * 2.0 < 1.0 {
        m2
    } else if h * 3.0 < 2.0 {
        m1 + (m2 - m1) * (2.0 / 3.0 - h) * 6.0
    } else {
        m1
    }
}

fn clamp01(v: f64) -> f64 {
    v.max(0.0).min(1.0)
}

/// less.js `unit(dim, unit?)` — set/strip the unit, keeping the numeric value.
fn func_unit(args: &[Node]) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    let unit_str = match args.get(1) {
        Some(Node::Keyword(k)) => k.clone(),
        Some(Node::Quoted { value, .. }) => value.clone(),
        Some(Node::Anonymous(s)) => s.clone(),
        Some(Node::Dimension(u)) => u.unit.rendered(),
        _ => String::new(),
    };
    Some(Node::Dimension(Dimension::with_unit(d.value, &unit_str)))
}

/// A one-argument math function preserving the argument's unit (less.js `_math`).
fn math1(args: &[Node], f: fn(f64) -> f64) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    Some(Node::Dimension(Dimension {
        value: f(d.value),
        unit: d.unit.clone(),
    }))
}

/// less.js `round(n, places?)`.
fn func_round(args: &[Node]) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    let places = match args.get(1) {
        Some(Node::Dimension(p)) => p.value as i32,
        _ => 0,
    };
    let factor = 10f64.powi(places);
    // JS Math.round semantics (half toward +∞).
    let rounded = ((d.value * factor) + 0.5).floor() / factor;
    Some(Node::Dimension(Dimension {
        value: rounded,
        unit: d.unit.clone(),
    }))
}

/// less.js `percentage(n)` — `n * 100%`.
fn func_percentage(args: &[Node]) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    Some(Node::Dimension(Dimension::with_unit(d.value * 100.0, "%")))
}

/// less.js `min`/`max` — reduce over compatible-unit args (plan §4.8). Falls back
/// to `None` (→ passthrough as literal CSS `min()`/`max()`) on incompatible units
/// or non-dimension args.
fn func_minmax(args: &[Node], want_min: bool) -> Option<Node> {
    let mut best: Option<Dimension> = None;
    for a in args {
        let d = as_dimension(a)?;
        match &best {
            None => best = Some(d.clone()),
            Some(cur) => {
                // Compare in the current best's unit family.
                let du = if d.unit.is_empty() || cur.unit.is_empty() {
                    d.value
                } else {
                    d.unify().value
                };
                let cu = if d.unit.is_empty() || cur.unit.is_empty() {
                    cur.value
                } else {
                    cur.unify().value
                };
                let replace = if want_min { du < cu } else { du > cu };
                if replace {
                    best = Some(d.clone());
                }
            }
        }
    }
    best.map(Node::Dimension)
}
