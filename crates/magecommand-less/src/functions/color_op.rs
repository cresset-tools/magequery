//! Color ops: `saturate, desaturate, lighten, darken, fadein, fadeout, fade,
//! spin, mix, tint, shade, greyscale (British only), contrast` (plan §2.7).
//! Mirrors less.js `functions/color.js`: every HSL adjuster goes through the
//! shared `hsla(origColor, hsl)` reconstruction, which keeps the input's
//! `rgb`/`hsl` output form and otherwise flips to `rgb` (→ hex when opaque);
//! the `relative` keyword scales by the current channel value.

use super::{as_color, as_dimension, clamp01, number};
use crate::ast::Node;
use crate::color::Color;
use crate::value::Dimension;

/// Which HSL/alpha channel an adjuster moves, and in which direction.
pub(super) enum Adjust {
    Saturate,
    Desaturate,
    Lighten,
    Darken,
    FadeIn,
    FadeOut,
}

/// The `relative` method keyword (third arg of the adjusters).
fn is_relative(n: Option<&Node>) -> bool {
    matches!(n, Some(Node::Keyword(k)) if k == "relative")
}

/// less.js `hsla(origColor, hsl)` — rebuild from HSL keeping the output form.
fn hsla_like(orig: &Color, h: f64, s: f64, l: f64, a: f64) -> Node {
    let h = (h % 360.0) / 360.0;
    let s = clamp01(s);
    let l = clamp01(l);
    let a = clamp01(a);
    let m2 = if l <= 0.5 { l * (s + 1.0) } else { l + s - l * s };
    let m1 = l * 2.0 - m2;
    let rgb = [
        hue_to_rgb(h + 1.0 / 3.0, m1, m2) * 255.0,
        hue_to_rgb(h, m1, m2) * 255.0,
        hue_to_rgb(h - 1.0 / 3.0, m1, m2) * 255.0,
    ];
    let value = match orig.original.as_deref() {
        Some(v) if v.starts_with("rgb") || v.starts_with("hsl") => v.to_string(),
        _ => "rgb".to_string(),
    };
    Node::Color(Color {
        rgb,
        alpha: a,
        original: Some(value.into()),
    })
}

fn hue_to_rgb(mut h: f64, m1: f64, m2: f64) -> f64 {
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

/// The shared adjuster body (`saturate`/`desaturate`/`lighten`/`darken`/
/// `fadein`/`fadeout`).
pub(super) fn adjust_hsl(args: &[Node], which: Adjust) -> Option<Node> {
    let color = as_color(args.first()?)?;
    let amount = as_dimension(args.get(1)?)?.value;
    let relative = is_relative(args.get(2));
    let (h, mut s, mut l, mut a) = color.to_hsl();
    let delta = |cur: f64| if relative { cur * amount / 100.0 } else { amount / 100.0 };
    match which {
        Adjust::Saturate => s = clamp01(s + delta(s)),
        Adjust::Desaturate => s = clamp01(s - delta(s)),
        Adjust::Lighten => l = clamp01(l + delta(l)),
        Adjust::Darken => l = clamp01(l - delta(l)),
        Adjust::FadeIn => a = clamp01(a + delta(a)),
        Adjust::FadeOut => a = clamp01(a - delta(a)),
    }
    Some(hsla_like(color, h, s, l, a))
}

/// less.js `fade(color, amount)` — set alpha outright.
pub(super) fn fade(args: &[Node]) -> Option<Node> {
    let color = as_color(args.first()?)?;
    let amount = as_dimension(args.get(1)?)?.value;
    let (h, s, l, _) = color.to_hsl();
    Some(hsla_like(color, h, s, l, clamp01(amount / 100.0)))
}

/// less.js `spin(color, amount)` — rotate the hue.
pub(super) fn spin(args: &[Node]) -> Option<Node> {
    let color = as_color(args.first()?)?;
    let amount = as_dimension(args.get(1)?)?.value;
    let (h, s, l, a) = color.to_hsl();
    let hue = (h + amount) % 360.0;
    let hue = if hue < 0.0 { 360.0 + hue } else { hue };
    Some(hsla_like(color, hue, s, l, a))
}

/// less.js `mix(color1, color2, weight=50)` (the SASS-derived formula).
pub(super) fn mix_fn(args: &[Node]) -> Option<Node> {
    let c1 = as_color(args.first()?)?;
    let c2 = as_color(args.get(1)?)?;
    let weight = match args.get(2) {
        Some(n) => as_dimension(n)?.value,
        None => 50.0,
    };
    Some(Node::Color(mix(c1, c2, weight)))
}

pub(super) fn mix(c1: &Color, c2: &Color, weight: f64) -> Color {
    let p = weight / 100.0;
    let w = p * 2.0 - 1.0;
    let a = c1.alpha - c2.alpha;
    let w1 = (if w * a == -1.0 { w } else { (w + a) / (1.0 + w * a) } + 1.0) / 2.0;
    let w2 = 1.0 - w1;
    Color {
        rgb: [
            c1.rgb[0] * w1 + c2.rgb[0] * w2,
            c1.rgb[1] * w1 + c2.rgb[1] * w2,
            c1.rgb[2] * w1 + c2.rgb[2] * w2,
        ],
        alpha: c1.alpha * p + c2.alpha * (1.0 - p),
        original: None,
    }
}

/// less.js `greyscale(color)` = `desaturate(color, 100)` (British only —
/// "grayscale" is deliberately NOT registered).
pub(super) fn greyscale(args: &[Node]) -> Option<Node> {
    let color = args.first()?.clone();
    adjust_hsl(
        &[color, Node::Dimension(Dimension::number(100.0))],
        Adjust::Desaturate,
    )
}

/// less.js `contrast(color, dark?, light?, threshold=0.43)` — the dark/light
/// pair is auto-swapped by luma, and the *given* nodes are returned verbatim
/// (their original spelling survives).
pub(super) fn contrast(args: &[Node]) -> Option<Node> {
    as_color(args.first()?)?; // non-color → passthrough (`contrast(30%)` filter)
    let color = as_color(args.first()?)?;
    let dark = args.get(1).cloned().unwrap_or_else(|| {
        Node::Color(Color::with_original([0.0, 0.0, 0.0], 1.0, "rgba"))
    });
    let light = args.get(2).cloned().unwrap_or_else(|| {
        Node::Color(Color::with_original([255.0, 255.0, 255.0], 1.0, "rgba"))
    });
    let threshold = match args.get(3) {
        Some(n) => number(n)?,
        None => 0.43,
    };
    let (dark, light) = {
        let dl = as_color(&dark)?.luma();
        let ll = as_color(&light)?.luma();
        if dl > ll { (light, dark) } else { (dark, light) }
    };
    Some(if color.luma() < threshold { light } else { dark })
}

/// less.js `tint(color, amount)` = `mix(white, color, amount)`.
pub(super) fn tint(args: &[Node]) -> Option<Node> {
    mix_with(args, [255.0, 255.0, 255.0])
}

/// less.js `shade(color, amount)` = `mix(black, color, amount)`.
pub(super) fn shade(args: &[Node]) -> Option<Node> {
    mix_with(args, [0.0, 0.0, 0.0])
}

fn mix_with(args: &[Node], base: [f64; 3]) -> Option<Node> {
    let color = as_color(args.first()?)?;
    let weight = match args.get(1) {
        Some(n) => as_dimension(n)?.value,
        None => 50.0,
    };
    let anchor = Color::with_original(base, 1.0, "rgb");
    Some(Node::Color(mix(&anchor, color, weight)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Dimension;

    fn color(hex: &str) -> Node {
        Node::Color(Color::from_hex(hex).unwrap())
    }
    fn num(v: f64) -> Node {
        Node::Dimension(Dimension::number(v))
    }
    fn pct(v: f64) -> Node {
        Node::Dimension(Dimension::with_unit(v, "%"))
    }
    fn css(n: Node) -> String {
        let Node::Color(c) = n else { panic!() };
        c.to_css(8)
    }

    #[test]
    fn lighten_darken_relative() {
        assert_eq!(css(adjust_hsl(&[color("#ff0000"), pct(40.0)], Adjust::Lighten).unwrap()), "#ffcccc");
        let rel = Node::Keyword("relative".into());
        assert_eq!(
            css(adjust_hsl(&[color("#ff0000"), pct(40.0), rel], Adjust::Lighten).unwrap()),
            "#ff6666"
        );
        assert_eq!(css(adjust_hsl(&[color("#ff0000"), pct(40.0)], Adjust::Darken).unwrap()), "#330000");
    }

    #[test]
    fn mix_weights() {
        assert_eq!(css(mix_fn(&[color("#ff0000"), color("#ffff00"), num(80.0)]).unwrap()), "#ff3300");
        assert_eq!(css(mix_fn(&[color("#ff0000"), color("#ffff00")]).unwrap()), "#ff8000");
        // mix with transparent halves the alpha, keeping c1's channels.
        let transparent = Node::Color(Color::from_keyword("transparent").unwrap());
        assert_eq!(css(mix_fn(&[color("#ff0000"), transparent]).unwrap()), "rgba(255, 0, 0, 0.5)");
    }

    #[test]
    fn contrast_threshold_and_originals() {
        // Default: white → black, black → white (as #000000/#ffffff hex).
        assert_eq!(css(contrast(&[color("#fff")]).unwrap()), "#000000");
        assert_eq!(css(contrast(&[color("#000")]).unwrap()), "#ffffff");
        // Given dark/light keep their spelling; wrong order is auto-swapped.
        assert_eq!(css(contrast(&[color("#fff"), color("#111111"), color("#eeeeee")]).unwrap()), "#111111");
        assert_eq!(
            css(contrast(&[color("#fff"), color("#eeeeee"), color("#111111"), num(0.5)]).unwrap()),
            "#111111"
        );
        // Threshold sensitivity on #555 (luma ≈ 0.0865).
        assert_eq!(
            css(contrast(&[color("#555"), color("#111111"), color("#eeeeee"), num(0.09)]).unwrap()),
            "#111111"
        );
        assert_eq!(
            css(contrast(&[color("#555"), color("#111111"), color("#eeeeee"), num(0.6)]).unwrap()),
            "#eeeeee"
        );
    }

    #[test]
    fn fadeout_relative_chains() {
        // fadeout(fadeout(red, 50%, relative), 50%, relative) → alpha 0.25.
        let red = Node::Color(Color::from_keyword("red").unwrap());
        let rel = Node::Keyword("relative".into());
        let once = adjust_hsl(&[red, pct(50.0), rel.clone()], Adjust::FadeOut).unwrap();
        let twice = adjust_hsl(&[once, pct(50.0), rel], Adjust::FadeOut).unwrap();
        assert_eq!(css(twice), "rgba(255, 0, 0, 0.25)");
    }
}
