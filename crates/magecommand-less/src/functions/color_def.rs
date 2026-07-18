//! Color constructors: `rgb, rgba, hsl, hsla, hsv, hsva, argb(→#AARRGGBB),
//! color(string)` (plan §2.7). Mirrors less.js `functions/color.js`: the
//! comma-less `rgb(0 128 255 / 50%)` form arrives as one Expression whose last
//! item is the deferred `/` Operation (a 4th space item WITHOUT a slash is
//! silently ignored — less.js unpacks `val[0..2]` with no length check); a
//! Color first argument to `rgb[a]`/`hsl[a]` reuses its channels; alpha is NOT
//! clamped by `rgb[a]` (an out-of-range alpha ≥ 1 later prints as hex).
//!
//! Error parity: `rgba`/`hsla` bodies are try/caught in less.js — any bad or
//! MISSING argument (incl. `rgba(1, 2, 3)` without alpha) re-emits the call
//! (`Ok(None)`), never defaults. `hsv`/`hsva`/`argb`/`color` have NO catch:
//! their throws are compile errors.

use super::{as_color, clamp01, number, scaled, undef_err, FnResult};
use crate::ast::Node;
use crate::color::Color;
use crate::error::{ErrorKind, LessError};

/// The alpha channel a constructor was given.
enum AlphaSpec<'a> {
    /// A node (or a missing one — less.js `number(undefined)` throws → caught).
    Node(Option<&'a Node>),
    /// A fixed value (`rgb`/`hsl`'s `a = 1`, `hsv`'s `1.0`).
    Fixed(f64),
}

/// Unpack the space-separated `f(a b c / alpha)` form from an Expression:
/// `(a, b?, c?, alpha?)`. less.js takes `val[0]`, `val[1]`, `val[2]` with no
/// length check — extra items are dropped, missing ones are `undefined` (which
/// the caught `number()` throw turns into a passthrough).
fn space_args(items: &[Node]) -> (Option<&Node>, Option<&Node>, Option<&Node>, Option<&Node>) {
    let a = items.first();
    let b = items.get(1);
    let mut c = items.get(2);
    let mut alpha = None;
    if let Some(Node::Operation { op, left, right, .. }) = c {
        if op == "/" {
            c = Some(&**left);
            alpha = Some(&**right);
        }
    }
    (a, b, c, alpha)
}

/// less.js `rgb(r, g, b)` (incl. the space-separated single-Expression form).
pub(super) fn rgb(args: &[Node]) -> Option<Node> {
    let (r, g, b, a) = match args.first() {
        Some(Node::Expression(items)) => space_args(items),
        _ => (args.first(), args.get(1), args.get(2), None),
    };
    let alpha = match a {
        Some(n) => AlphaSpec::Node(Some(n)),
        None => AlphaSpec::Fixed(1.0),
    };
    let c = build_rgba(r?, g, b, alpha)?;
    Some(Node::Color(Color {
        original: Some("rgb".into()),
        ..c
    }))
}

/// less.js `rgba(...)` — also accepts `rgba(color, alpha?)`; a missing alpha on
/// the numeric form re-emits the call (no `a = 1` default).
pub(super) fn rgba(args: &[Node]) -> Option<Node> {
    let c = build_rgba(
        args.first()?,
        args.get(1),
        args.get(2),
        AlphaSpec::Node(args.get(3)),
    )?;
    Some(Node::Color(c))
}

fn build_rgba(r: &Node, g: Option<&Node>, b: Option<&Node>, a: AlphaSpec) -> Option<Color> {
    if let Some(color) = as_color(r) {
        // `rgba(#55FF5599)` / `rgba(color, 0.5)` — `g` doubles as the alpha.
        let alpha = match g {
            Some(n) => number(Some(n)).ok()?,
            None => color.alpha,
        };
        return Some(Color {
            rgb: color.rgb,
            alpha,
            original: Some("rgba".into()),
        });
    }
    let rgb = [
        scaled(Some(r), 255.0).ok()?,
        scaled(g, 255.0).ok()?,
        scaled(b, 255.0).ok()?,
    ];
    let alpha = match a {
        AlphaSpec::Node(n) => number(n).ok()?,
        AlphaSpec::Fixed(v) => v,
    };
    Some(Color {
        rgb,
        alpha,
        original: Some("rgba".into()),
    })
}

/// less.js `hsl(h, s, l)` (incl. the space-separated form).
pub(super) fn hsl(args: &[Node]) -> Option<Node> {
    let (h, s, l, a) = match args.first() {
        Some(Node::Expression(items)) => space_args(items),
        _ => (args.first(), args.get(1), args.get(2), None),
    };
    let alpha = match a {
        Some(n) => AlphaSpec::Node(Some(n)),
        None => AlphaSpec::Fixed(1.0),
    };
    let c = build_hsla(h?, s, l, alpha)?;
    Some(Node::Color(Color {
        original: Some("hsl".into()),
        ..c
    }))
}

/// less.js `hsla(...)` — also accepts `hsla(color, alpha?)`; a missing alpha on
/// the numeric form re-emits the call.
pub(super) fn hsla(args: &[Node]) -> Option<Node> {
    let c = build_hsla(
        args.first()?,
        args.get(1),
        args.get(2),
        AlphaSpec::Node(args.get(3)),
    )?;
    Some(Node::Color(c))
}

/// less.js `hsla` core — HSL→RGB, channels kept as unrounded floats.
fn build_hsla(h: &Node, s: Option<&Node>, l: Option<&Node>, a: AlphaSpec) -> Option<Color> {
    if let Some(color) = as_color(h) {
        let alpha = match s {
            Some(n) => number(Some(n)).ok()?,
            None => color.alpha,
        };
        return Some(Color {
            rgb: color.rgb,
            alpha,
            original: Some("hsla".into()),
        });
    }
    let h = (number(Some(h)).ok()? % 360.0) / 360.0;
    let s = clamp01(number(s).ok()?);
    let l = clamp01(number(l).ok()?);
    let alpha = match a {
        AlphaSpec::Node(n) => clamp01(number(n).ok()?),
        AlphaSpec::Fixed(v) => clamp01(v),
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

/// less.js `hsv(h, s, v)` — NO try/catch: bad/missing args are compile errors.
pub(super) fn hsv(args: &[Node]) -> FnResult {
    hsva_impl(args.first(), args.get(1), args.get(2), AlphaSpec::Fixed(1.0))
}

/// less.js `hsva(h, s, v, a)` — all four go through `number()` (a missing alpha
/// throws `color functions take numbers as parameters`).
pub(super) fn hsva(args: &[Node]) -> FnResult {
    hsva_impl(
        args.first(),
        args.get(1),
        args.get(2),
        AlphaSpec::Node(args.get(3)),
    )
}

fn hsva_impl(h: Option<&Node>, s: Option<&Node>, v: Option<&Node>, a: AlphaSpec) -> FnResult {
    let h = ((number(h)? % 360.0) / 360.0) * 360.0;
    let s = number(s)?;
    let v = number(v)?;
    let a = match a {
        AlphaSpec::Node(n) => number(n)?,
        AlphaSpec::Fixed(f) => f,
    };
    // JS `Math.floor((h / 60) % 6)` keeps the sign — a negative hue indexes
    // `perm[-1]`, whose `[0]` access is less.js's propagated TypeError.
    let i = ((h / 60.0) % 6.0).floor() as i64;
    let f = (h / 60.0) - i as f64;
    if !(0..=5).contains(&i) {
        return Err(undef_err("0"));
    }
    let i = i as usize;
    let vs = [v, v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s)];
    const PERM: [[usize; 3]; 6] = [
        [0, 3, 1],
        [2, 0, 1],
        [1, 0, 3],
        [1, 2, 0],
        [3, 1, 0],
        [0, 1, 2],
    ];
    Ok(Some(Node::Color(Color {
        rgb: [
            vs[PERM[i][0]] * 255.0,
            vs[PERM[i][1]] * 255.0,
            vs[PERM[i][2]] * 255.0,
        ],
        alpha: a,
        original: Some("rgba".into()),
    })))
}

/// less.js `argb(color)` → anonymous `#AARRGGBB`; a non-color is the propagated
/// `color.toARGB is not a function` TypeError.
pub(super) fn argb(args: &[Node]) -> FnResult {
    match args.first() {
        Some(Node::Color(c)) => Ok(Some(Node::Anonymous(c.to_argb()))),
        Some(_) => Err(LessError::new(
            ErrorKind::Runtime,
            "color.toARGB is not a function",
        )),
        None => Err(undef_err("toARGB")),
    }
}

/// less.js `color(c)` — a quoted 3/4/6/8-digit hex (kept verbatim as the
/// original form), a color (original cleared → hex output), or a **quoted color
/// keyword** (`color("plum")` → `#dda0dd`, `color('transparent')` →
/// `rgba(0, 0, 0, 0)`); anything else throws.
pub(super) fn color(args: &[Node]) -> FnResult {
    match args.first() {
        Some(Node::Quoted { value, .. }) => {
            let is_hex = value
                .strip_prefix('#')
                .map(|hex| {
                    matches!(hex.len(), 3 | 4 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
                })
                .unwrap_or(false);
            if is_hex {
                if let Some(mut c) = Color::from_hex(value) {
                    c.original = Some(value.as_str().into());
                    return Ok(Some(Node::Color(c)));
                }
            }
            // less.js falls back to `Color.fromKeyword(c.value)`, then clears
            // the keyword so the color prints as hex (or rgba for transparent).
            if let Some(mut c) = Color::from_keyword(value) {
                c.original = None;
                return Ok(Some(Node::Color(c)));
            }
            Err(color_arg_err())
        }
        Some(Node::Color(c)) => Ok(Some(Node::Color(Color {
            original: None,
            ..c.clone()
        }))),
        Some(_) => Err(color_arg_err()),
        None => Err(undef_err("value")),
    }
}

fn color_arg_err() -> LessError {
    LessError::new(
        ErrorKind::Argument,
        "argument must be a color keyword or 3|4|6|8 digit hex e.g. #FFF",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Dimension;

    fn num(v: f64) -> Node {
        Node::Dimension(Dimension::number(v))
    }
    fn pct(v: f64) -> Node {
        Node::Dimension(Dimension::with_unit(v, "%"))
    }

    #[test]
    fn argb_rounds_alpha_up() {
        // rgba(255, 238, 170, 0.1) → #1affeeaa (alpha 25.5 rounds to 26).
        let c = build_rgba(
            &num(255.0),
            Some(&num(238.0)),
            Some(&num(170.0)),
            AlphaSpec::Node(Some(&num(0.1))),
        )
        .unwrap();
        assert_eq!(c.to_argb(), "#1affeeaa");
    }

    #[test]
    fn rgba_from_hex_color_keeps_channels() {
        let c = Node::Color(Color::from_hex("#55FF5599").unwrap());
        let out = rgba(&[c]).unwrap();
        let Node::Color(c) = out else { panic!() };
        assert_eq!(c.to_css(8), "rgba(85, 255, 85, 0.6)");
    }

    #[test]
    fn space_separated_rgb_with_alpha() {
        let expr = Node::Expression(vec![
            num(0.0),
            num(128.0),
            Node::Operation {
                op: "/".into(),
                left: Box::new(num(255.0)),
                right: Box::new(pct(50.0)),
                spaced: true,
            },
        ]);
        let out = rgb(&[expr]).unwrap();
        let Node::Color(c) = out else { panic!() };
        assert_eq!(c.to_css(8), "rgba(0, 128, 255, 0.5)");
    }

    #[test]
    fn hsl_clamps_and_wraps() {
        // hsl(380, 150%, 150%) → hsl(0, 0%, 100%) (h wraps, s/l clamp → white).
        let out = hsl(&[num(380.0), pct(150.0), pct(150.0)]).unwrap();
        let Node::Color(c) = out else { panic!() };
        assert_eq!(c.to_css(8), "hsl(0, 0%, 100%)");
    }

    #[test]
    fn missing_alpha_reemits_and_extra_space_item_drops() {
        // rgba(1, 2, 3) — no alpha → passthrough (C1), never alpha=1.
        assert!(rgba(&[num(1.0), num(2.0), num(3.0)]).is_none());
        // hsla(90, 50%, 50%) likewise.
        assert!(hsla(&[num(90.0), pct(50.0), pct(50.0)]).is_none());
        // rgb(0 128 255 96) — 4th space item ignored (C12).
        let expr = Node::Expression(vec![num(0.0), num(128.0), num(255.0), num(96.0)]);
        let out = rgb(&[expr]).unwrap();
        let Node::Color(c) = out else { panic!() };
        assert_eq!(c.to_css(8), "#0080ff");
    }

    #[test]
    fn rgb_single_color_arg_and_color_keyword_fallback() {
        // rgb(#123456) → the color, value 'rgb' (C2).
        let c = Node::Color(Color::from_hex("#123456").unwrap());
        let out = rgb(&[c]).unwrap();
        let Node::Color(c) = out else { panic!() };
        assert_eq!(c.to_css(8), "#123456");
        // color("plum") → #dda0dd; color('transparent') → rgba(0, 0, 0, 0) (C4).
        let q = |s: &str| Node::Quoted { escaped: false, quote: '\'', value: s.into() };
        let Ok(Some(Node::Color(c))) = color(&[q("plum")]) else { panic!() };
        assert_eq!(c.to_css(8), "#dda0dd");
        let Ok(Some(Node::Color(c))) = color(&[q("transparent")]) else { panic!() };
        assert_eq!(c.to_css(8), "rgba(0, 0, 0, 0)");
        // color(banana-ish) → Argument error.
        assert!(color(&[q("banana")]).is_err());
    }

    #[test]
    fn hsv_error_parity() {
        // hsv(-60, 0.5, 0.5) → perm[-1] TypeError (C10).
        assert!(hsv(&[num(-60.0), num(0.5), num(0.5)]).is_err());
        // hsva(90, 0.5, 0.5) — missing alpha throws (C19).
        assert!(hsva(&[num(90.0), num(0.5), num(0.5)]).is_err());
        // hsv(90, 0.5) — missing v throws.
        assert!(hsv(&[num(90.0), num(0.5)]).is_err());
    }
}
