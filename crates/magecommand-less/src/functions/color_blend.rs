//! Color blending: `multiply, screen, overlay, softlight, hardlight,
//! difference, exclusion, average, negation` (plan §2.7). A faithful port of
//! less.js `functions/color-blending.js` (W3C compositing-1 formulas plus the
//! non-w3c `average`/`negation`), including the alpha-compositing correction.

use super::as_color;
use crate::ast::Node;
use crate::color::Color;

/// The blend mode (the per-channel `mode(cb, cs)` function).
#[derive(Clone, Copy)]
pub(super) enum Mode {
    Multiply,
    Screen,
    Overlay,
    SoftLight,
    HardLight,
    Difference,
    Exclusion,
    Average,
    Negation,
}

fn apply(mode: Mode, cb: f64, cs: f64) -> f64 {
    match mode {
        Mode::Multiply => cb * cs,
        Mode::Screen => cb + cs - cb * cs,
        Mode::Overlay => {
            let cb = cb * 2.0;
            if cb <= 1.0 {
                apply(Mode::Multiply, cb, cs)
            } else {
                apply(Mode::Screen, cb - 1.0, cs)
            }
        }
        Mode::SoftLight => {
            let mut d = 1.0;
            let mut e = cb;
            if cs > 0.5 {
                e = 1.0;
                d = if cb > 0.25 {
                    cb.sqrt()
                } else {
                    ((16.0 * cb - 12.0) * cb + 4.0) * cb
                };
            }
            cb - (1.0 - 2.0 * cs) * e * (d - cb)
        }
        Mode::HardLight => apply(Mode::Overlay, cs, cb),
        Mode::Difference => (cb - cs).abs(),
        Mode::Exclusion => cb + cs - 2.0 * cb * cs,
        Mode::Average => (cb + cs) / 2.0,
        Mode::Negation => 1.0 - (cb + cs - 1.0).abs(),
    }
}

/// less.js `colorBlend(mode, color1, color2)`.
pub(super) fn blend(args: &[Node], mode: Mode) -> Option<Node> {
    let c1 = as_color(args.first()?)?; // backdrop
    let c2 = as_color(args.get(1)?)?; // source
    let ab = c1.alpha;
    let as_ = c2.alpha;
    let ar = as_ + ab * (1.0 - as_);
    let mut rgb = [0.0; 3];
    for i in 0..3 {
        let cb = c1.rgb[i] / 255.0;
        let cs = c2.rgb[i] / 255.0;
        let mut cr = apply(mode, cb, cs);
        if ar != 0.0 {
            cr = (as_ * cs + ab * (cb - as_ * (cb + cs - cr))) / ar;
        }
        rgb[i] = cr * 255.0;
    }
    Some(Node::Color(Color {
        rgb,
        alpha: ar,
        original: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(hex: &str) -> Node {
        Node::Color(Color::from_hex(hex).unwrap())
    }
    fn css(n: Node) -> String {
        let Node::Color(c) = n else { panic!() };
        c.to_css(8)
    }

    #[test]
    fn blend_modes_match_less_js() {
        let a = || color("#f60000");
        let b = || color("#0000f6");
        assert_eq!(css(blend(&[a(), a()], Mode::Multiply).unwrap()), "#ed0000");
        assert_eq!(css(blend(&[a(), b()], Mode::Screen).unwrap()), "#f600f6");
        assert_eq!(css(blend(&[a(), b()], Mode::Overlay).unwrap()), "#ed0000");
        assert_eq!(css(blend(&[a(), color("#ffffff")], Mode::SoftLight).unwrap()), "#fa0000");
        assert_eq!(css(blend(&[a(), b()], Mode::HardLight).unwrap()), "#0000ed");
        assert_eq!(css(blend(&[a(), b()], Mode::Difference).unwrap()), "#f600f6");
        assert_eq!(css(blend(&[a(), b()], Mode::Exclusion).unwrap()), "#f600f6");
        assert_eq!(css(blend(&[a(), b()], Mode::Average).unwrap()), "#7b007b");
        assert_eq!(css(blend(&[a(), color("#313131")], Mode::Negation).unwrap()), "#d73131");
    }
}
