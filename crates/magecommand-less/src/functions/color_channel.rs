//! Color channels: `hue, saturation, lightness, hsvhue, hsvsaturation,
//! hsvvalue, red, green, blue, alpha, luma, luminance` (plan §2.7). Mirrors
//! less.js `functions/color.js` — channel values are NOT pre-rounded (fround at
//! genCSS time handles display rounding).

use super::as_color;
use crate::ast::Node;
use crate::value::Dimension;

/// HSL channel selector.
pub(super) enum Hsl {
    Hue,
    Saturation,
    Lightness,
}

/// HSV channel selector.
pub(super) enum Hsv {
    Hue,
    Saturation,
    Value,
}

pub(super) fn hsl_channel(args: &[Node], ch: Hsl) -> Option<Node> {
    let (h, s, l, _a) = as_color(args.first()?)?.to_hsl();
    Some(Node::Dimension(match ch {
        Hsl::Hue => Dimension::number(h),
        Hsl::Saturation => Dimension::with_unit(s * 100.0, "%"),
        Hsl::Lightness => Dimension::with_unit(l * 100.0, "%"),
    }))
}

pub(super) fn hsv_channel(args: &[Node], ch: Hsv) -> Option<Node> {
    let (h, s, v, _a) = as_color(args.first()?)?.to_hsv();
    Some(Node::Dimension(match ch {
        Hsv::Hue => Dimension::number(h),
        Hsv::Saturation => Dimension::with_unit(s * 100.0, "%"),
        Hsv::Value => Dimension::with_unit(v * 100.0, "%"),
    }))
}

pub(super) fn rgb_channel(args: &[Node], idx: usize) -> Option<Node> {
    let c = as_color(args.first()?)?;
    Some(Node::Dimension(Dimension::number(c.rgb[idx])))
}

/// less.js `alpha(color)` — `toHSL().a`, a unitless dimension.
pub(super) fn alpha(args: &[Node]) -> Option<Node> {
    let c = as_color(args.first()?)?;
    Some(Node::Dimension(Dimension::number(c.alpha)))
}

/// less.js `luma(color)` — gamma-corrected, alpha-scaled, in `%`.
pub(super) fn luma(args: &[Node]) -> Option<Node> {
    let c = as_color(args.first()?)?;
    Some(Node::Dimension(Dimension::with_unit(
        c.luma() * c.alpha * 100.0,
        "%",
    )))
}

/// less.js `luminance(color)` — the linear (non-gamma) variant, in `%`.
pub(super) fn luminance(args: &[Node]) -> Option<Node> {
    let c = as_color(args.first()?)?;
    let l = 0.2126 * c.rgb[0] / 255.0 + 0.7152 * c.rgb[1] / 255.0 + 0.0722 * c.rgb[2] / 255.0;
    Some(Node::Dimension(Dimension::with_unit(
        l * c.alpha * 100.0,
        "%",
    )))
}
