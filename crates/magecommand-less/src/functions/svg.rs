//! `svg-gradient(direction, color[ position], …)` (plan §2.7) — a port of
//! less.js `functions/svg.js`: builds the inline SVG, `encodeURIComponent`s it,
//! and returns `url('data:image/svg+xml,…')`. URL-encoding parity is the whole
//! point (§3-G): the JS reserved set, uppercase hex.

use super::as_color;
use super::string::encode_uri_component;
use crate::ast::Node;
use crate::css::render_value;
use crate::error::{ErrorKind, LessError};

/// less.js `throwArgumentDescriptor` — the shared bad-arguments error.
fn arg_descriptor() -> LessError {
    LessError::new(
        ErrorKind::Argument,
        "svg-gradient expects direction, start_color [start_position], [color position,]..., end_color [end_position] or direction, color list",
    )
}

pub(super) fn svg_gradient(args: &[Node], np: u8) -> Result<Option<Node>, LessError> {
    let Some(first) = args.first() else { return Err(arg_descriptor()) };
    let direction = render_value(first, np);
    // Argument-shape checks BEFORE the direction check, exactly less.js's
    // order: a 2-arg call with a real list of <2 stops fails here; a 2-arg
    // call whose second argument is NOT a list defers (JS reads `.value`, a
    // string, whose length passes) and fails per-stop after the direction
    // check (svg-gradient3/6 assert the DIRECTION error).
    let stops: Option<Vec<Node>> = if args.len() == 2 {
        match &args[1] {
            Node::Value(items) | Node::Expression(items) => {
                if items.len() < 2 {
                    return Err(arg_descriptor());
                }
                Some(items.clone())
            }
            _ => None,
        }
    } else if args.len() < 3 {
        return Err(arg_descriptor());
    } else {
        Some(args[1..].to_vec())
    };

    let (gradient_type, direction_svg, rectangle) = match direction.as_str() {
        "to bottom" => ("linear", r#"x1="0%" y1="0%" x2="0%" y2="100%""#, RECT_LINEAR),
        "to right" => ("linear", r#"x1="0%" y1="0%" x2="100%" y2="0%""#, RECT_LINEAR),
        "to bottom right" => ("linear", r#"x1="0%" y1="0%" x2="100%" y2="100%""#, RECT_LINEAR),
        "to top right" => ("linear", r#"x1="0%" y1="100%" x2="100%" y2="0%""#, RECT_LINEAR),
        "ellipse" | "ellipse at center" => ("radial", r#"cx="50%" cy="50%" r="75%""#, RECT_RADIAL),
        _ => {
            return Err(LessError::new(
                ErrorKind::Argument,
                "svg-gradient direction must be 'to bottom', 'to right', 'to bottom right', 'to top right' or 'ellipse at center'",
            ))
        }
    };
    let Some(stops) = stops else { return Err(arg_descriptor()) };

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><{gradient_type}Gradient id="g" {direction_svg}>"#
    );
    let count = stops.len();
    for (i, stop) in stops.iter().enumerate() {
        let (color_node, position) = match stop {
            Node::Expression(items) => match items.first() {
                Some(c) => (c, items.get(1)),
                None => return Err(arg_descriptor()),
            },
            other => (other, None),
        };
        let coerced = super::coerce_keyword_color(color_node.clone());
        let Some(color) = as_color(&coerced).cloned() else {
            return Err(arg_descriptor());
        };
        if position.is_none() && !(i == 0 || i + 1 == count) {
            return Err(arg_descriptor());
        }
        if let Some(p) = position {
            if !matches!(p, Node::Dimension(_)) {
                return Err(arg_descriptor());
            }
        }
        let position_value = match position {
            Some(p) => render_value(p, np),
            None if i == 0 => "0%".to_string(),
            None => "100%".to_string(),
        };
        let alpha = color.alpha;
        svg.push_str(&format!(
            r#"<stop offset="{position_value}" stop-color="{}"{}/>"#,
            color.to_rgb_hex(),
            if alpha < 1.0 {
                format!(r#" stop-opacity="{}""#, js_num(alpha))
            } else {
                String::new()
            }
        ));
    }
    svg.push_str(&format!(
        r#"</{gradient_type}Gradient><rect {rectangle} fill="url(#g)" /></svg>"#
    ));

    let uri = format!("data:image/svg+xml,{}", encode_uri_component(&svg));
    Ok(Some(Node::Url(Box::new(Node::Quoted {
        escaped: false,
        quote: '\'',
        value: uri,
    }))))
}

const RECT_LINEAR: &str = r#"x="0" y="0" width="1" height="1""#;
const RECT_RADIAL: &str = r#"x="-50" y="-50" width="101" height="101""#;

/// JS `String(number)` for the stop-opacity interpolation.
fn js_num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;

    #[test]
    fn simple_two_stop_gradient() {
        let dir = Node::Expression(vec![Node::Keyword("to".into()), Node::Keyword("bottom".into())]);
        let black = Node::Color(Color::from_keyword("black").unwrap());
        let white = Node::Color(Color::from_keyword("white").unwrap());
        let out = svg_gradient(&[dir, black, white], 8).unwrap().unwrap();
        let Node::Url(q) = out else { panic!() };
        let Node::Quoted { value, .. } = *q else { panic!() };
        assert!(value.starts_with("data:image/svg+xml,%3Csvg%20xmlns%3D%22http"));
        assert!(value.contains("stop-color%3D%22%23000000%22"));
    }
}
