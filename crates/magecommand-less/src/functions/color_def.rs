//! Color constructors: `rgb, rgba, hsl, hsla, hsv, hsva, argb(→#AARRGGBB),
//! color(string)` (plan §2.7). Mirrors less.js `functions/color.js`: the
//! comma-less `rgb(0 128 255 / 50%)` form arrives as one Expression whose last
//! item is the deferred `/` Operation; a Color first argument to `rgba`/`hsla`
//! reuses its channels with a new alpha; alpha is NOT clamped by `rgb[a]` (an
//! out-of-range alpha ≥ 1 later prints as hex, the `rgba(0,30,0,238)` case).

use super::{as_color, clamp01, number, scaled};
use crate::ast::Node;
use crate::color::Color;

/// Unpack the space-separated `f(a b c / alpha)` form: `(a, b, c, alpha?)`.
fn space_args(first: &Node) -> Option<(Node, Node, Node, Option<Node>)> {
    if let Node::Expression(items) = first {
        if items.len() == 3 {
            if let Node::Operation { op, left, right, .. } = &items[2] {
                if op == "/" {
                    return Some((
                        items[0].clone(),
                        items[1].clone(),
                        (**left).clone(),
                        Some((**right).clone()),
                    ));
                }
            }
            return Some((items[0].clone(), items[1].clone(), items[2].clone(), None));
        }
    }
    None
}

/// less.js `rgb(r, g, b)` (incl. the space-separated single-Expression form).
pub(super) fn rgb(args: &[Node]) -> Option<Node> {
    let (r, g, b, a) = match space_args(args.first()?) {
        Some((r, g, b, a)) => (r, g, b, a),
        None => (
            args.first()?.clone(),
            args.get(1)?.clone(),
            args.get(2)?.clone(),
            None,
        ),
    };
    let c = build_rgba(&r, Some(&g), Some(&b), a.as_ref())?;
    Some(Node::Color(Color {
        original: Some("rgb".into()),
        ..c
    }))
}

/// less.js `rgba(...)` — also accepts `rgba(color, alpha?)`.
pub(super) fn rgba(args: &[Node]) -> Option<Node> {
    let c = build_rgba(
        args.first()?,
        args.get(1),
        args.get(2),
        args.get(3),
    )?;
    Some(Node::Color(c))
}

fn build_rgba(r: &Node, g: Option<&Node>, b: Option<&Node>, a: Option<&Node>) -> Option<Color> {
    if let Some(color) = as_color(r) {
        // `rgba(#55FF5599)` / `rgba(color, 0.5)`.
        let alpha = match g {
            Some(n) => number(n)?,
            None => color.alpha,
        };
        return Some(Color {
            rgb: color.rgb,
            alpha,
            original: Some("rgba".into()),
        });
    }
    let rgb = [scaled(r, 255.0)?, scaled(g?, 255.0)?, scaled(b?, 255.0)?];
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

/// less.js `hsl(h, s, l)` (incl. the space-separated form).
pub(super) fn hsl(args: &[Node]) -> Option<Node> {
    let (h, s, l, a) = match space_args(args.first()?) {
        Some(t) => t,
        None => (
            args.first()?.clone(),
            args.get(1)?.clone(),
            args.get(2)?.clone(),
            None,
        ),
    };
    let c = build_hsla(&h, Some(&s), Some(&l), a.as_ref())?;
    Some(Node::Color(Color {
        original: Some("hsl".into()),
        ..c
    }))
}

/// less.js `hsla(...)` — also accepts `hsla(color, alpha?)`.
pub(super) fn hsla(args: &[Node]) -> Option<Node> {
    let c = build_hsla(args.first()?, args.get(1), args.get(2), args.get(3))?;
    Some(Node::Color(c))
}

/// less.js `hsla` core — HSL→RGB, channels kept as unrounded floats.
fn build_hsla(h: &Node, s: Option<&Node>, l: Option<&Node>, a: Option<&Node>) -> Option<Color> {
    if let Some(color) = as_color(h) {
        let alpha = match s {
            Some(n) => number(n)?,
            None => color.alpha,
        };
        return Some(Color {
            rgb: color.rgb,
            alpha,
            original: Some("hsla".into()),
        });
    }
    let h = (number(h)? % 360.0) / 360.0;
    let s = clamp01(number(s?)?);
    let l = clamp01(number(l?)?);
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

/// less.js `hsv(h, s, v)`.
pub(super) fn hsv(args: &[Node]) -> Option<Node> {
    hsva_impl(args.first()?, args.get(1)?, args.get(2)?, None)
}

/// less.js `hsva(h, s, v, a)`.
pub(super) fn hsva(args: &[Node]) -> Option<Node> {
    hsva_impl(args.first()?, args.get(1)?, args.get(2)?, args.get(3))
}

fn hsva_impl(h: &Node, s: &Node, v: &Node, a: Option<&Node>) -> Option<Node> {
    let h = ((number(h)? % 360.0) / 360.0) * 360.0;
    let s = number(s)?;
    let v = number(v)?;
    let a = match a {
        Some(n) => number(n)?,
        None => 1.0,
    };
    let i = ((h / 60.0) % 6.0).floor() as usize;
    let f = (h / 60.0) - i as f64;
    let vs = [v, v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s)];
    const PERM: [[usize; 3]; 6] = [
        [0, 3, 1],
        [2, 0, 1],
        [1, 0, 3],
        [1, 2, 0],
        [3, 1, 0],
        [0, 1, 2],
    ];
    Some(Node::Color(Color {
        rgb: [
            vs[PERM[i][0]] * 255.0,
            vs[PERM[i][1]] * 255.0,
            vs[PERM[i][2]] * 255.0,
        ],
        alpha: a,
        original: Some("rgba".into()),
    }))
}

/// less.js `argb(color)` → anonymous `#AARRGGBB`.
pub(super) fn argb(args: &[Node]) -> Option<Node> {
    let c = as_color(args.first()?)?;
    Some(Node::Anonymous(c.to_argb()))
}

/// less.js `color(c)` — a quoted 3/4/6/8-digit hex (kept verbatim as the
/// original form), a color (original cleared → hex output), or a color keyword
/// (original cleared → hex output).
pub(super) fn color(args: &[Node]) -> Option<Node> {
    match args.first()? {
        Node::Quoted { value, .. } => {
            let hex = value.strip_prefix('#')?;
            if !matches!(hex.len(), 3 | 4 | 6 | 8) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let mut c = Color::from_hex(value)?;
            c.original = Some(value.as_str().into());
            Some(Node::Color(c))
        }
        Node::Color(c) => Some(Node::Color(Color {
            original: None,
            ..c.clone()
        })),
        _ => None,
    }
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
        let c = build_rgba(&num(255.0), Some(&num(238.0)), Some(&num(170.0)), Some(&num(0.1))).unwrap();
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
}
