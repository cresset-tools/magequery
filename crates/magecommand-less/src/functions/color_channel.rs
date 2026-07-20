//! Color channels: `hue, saturation, lightness, hsvhue, hsvsaturation,
//! hsvvalue, red, green, blue, alpha, luma, luminance` (plan §2.7). Mirrors
//! less.js `functions/color.js` — channel values are NOT pre-rounded (fround at
//! genCSS time handles display rounding).
//!
//! Error parity: none of these are caught in less.js — a non-color argument is
//! a compile error (`toHSL`/`toHSV` throw `Argument cannot be evaluated to a
//! color`; `red`/`green`/`blue`/`luminance` hit the `.rgb[…]` TypeError; `luma`
//! the `.luma is not a function` one).

use super::{dim_node, to_color_err, undef_err, FnResult};
use crate::ast::Node;
use crate::error::{ErrorKind, LessError};
use crate::unit::Unit;

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

pub(super) fn hsl_channel(args: &[Node], ch: Hsl) -> FnResult {
    let (h, s, l, _a) = to_color_err(args.first())?.to_hsl();
    Ok(Some(match ch {
        Hsl::Hue => dim_node(h, Unit::none())?,
        Hsl::Saturation => dim_node(s * 100.0, Unit::single("%"))?,
        Hsl::Lightness => dim_node(l * 100.0, Unit::single("%"))?,
    }))
}

pub(super) fn hsv_channel(args: &[Node], ch: Hsv) -> FnResult {
    let (h, s, v, _a) = to_color_err(args.first())?.to_hsv();
    Ok(Some(match ch {
        Hsv::Hue => dim_node(h, Unit::none())?,
        Hsv::Saturation => dim_node(s * 100.0, Unit::single("%"))?,
        Hsv::Value => dim_node(v * 100.0, Unit::single("%"))?,
    }))
}

pub(super) fn rgb_channel(args: &[Node], idx: usize) -> FnResult {
    let Some(Node::Color(c)) = args.first() else {
        return Err(undef_err("0")); // less.js `color.rgb[0]` TypeError
    };
    Ok(Some(dim_node(c.rgb[idx], Unit::none())?))
}

/// less.js `alpha(color)` — `toHSL().a`, a unitless dimension.
pub(super) fn alpha(args: &[Node]) -> FnResult {
    let c = to_color_err(args.first())?;
    Ok(Some(dim_node(c.alpha, Unit::none())?))
}

/// less.js `luma(color)` — gamma-corrected, alpha-scaled, in `%`.
pub(super) fn luma(args: &[Node]) -> FnResult {
    let Some(Node::Color(c)) = args.first() else {
        return Err(LessError::new(
            ErrorKind::Runtime,
            "color.luma is not a function",
        ));
    };
    Ok(Some(dim_node(c.luma() * c.alpha * 100.0, Unit::single("%"))?))
}

/// less.js `luminance(color)` — the linear (non-gamma) variant, in `%`.
pub(super) fn luminance(args: &[Node]) -> FnResult {
    let Some(Node::Color(c)) = args.first() else {
        return Err(undef_err("0"));
    };
    let l = 0.2126 * c.rgb[0] / 255.0 + 0.7152 * c.rgb[1] / 255.0 + 0.0722 * c.rgb[2] / 255.0;
    Ok(Some(dim_node(l * c.alpha * 100.0, Unit::single("%"))?))
}
