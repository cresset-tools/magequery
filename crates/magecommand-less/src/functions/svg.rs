//! `svg-gradient(direction, color[ position], …)` (plan §2.7) — a port of
//! less.js `functions/svg.js`: builds the inline SVG, `encodeURIComponent`s it,
//! and returns `url('data:image/svg+xml,…')`. URL-encoding parity is the whole
//! point (§3-G): the JS reserved set, uppercase hex.

use super::as_color;
use super::string::encode_uri_component;
use crate::ast::Node;
use crate::css::render_value;

pub(super) fn svg_gradient(args: &[Node], np: u8) -> Option<Node> {
    let direction = render_value(args.first()?, np);
    let stops: Vec<Node> = if args.len() == 2 {
        match args.get(1)? {
            Node::Value(items) | Node::Expression(items) if items.len() >= 2 => items.clone(),
            _ => return None,
        }
    } else if args.len() < 3 {
        return None;
    } else {
        args[1..].to_vec()
    };

    let (gradient_type, direction_svg, rectangle) = match direction.as_str() {
        "to bottom" => ("linear", r#"x1="0%" y1="0%" x2="0%" y2="100%""#, RECT_LINEAR),
        "to right" => ("linear", r#"x1="0%" y1="0%" x2="100%" y2="0%""#, RECT_LINEAR),
        "to bottom right" => ("linear", r#"x1="0%" y1="0%" x2="100%" y2="100%""#, RECT_LINEAR),
        "to top right" => ("linear", r#"x1="0%" y1="100%" x2="100%" y2="0%""#, RECT_LINEAR),
        "ellipse" | "ellipse at center" => ("radial", r#"cx="50%" cy="50%" r="75%""#, RECT_RADIAL),
        _ => return None,
    };

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><{gradient_type}Gradient id="g" {direction_svg}>"#
    );
    let count = stops.len();
    for (i, stop) in stops.iter().enumerate() {
        let (color_node, position) = match stop {
            Node::Expression(items) => (items.first()?, items.get(1)),
            other => (other, None),
        };
        let color = as_color(&super::coerce_keyword_color(color_node.clone()))?.clone();
        if position.is_none() && !(i == 0 || i + 1 == count) {
            return None;
        }
        if let Some(p) = position {
            if !matches!(p, Node::Dimension(_)) {
                return None;
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
    Some(Node::Url(Box::new(Node::Quoted {
        escaped: false,
        quote: '\'',
        value: uri,
    })))
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
        let out = svg_gradient(&[dir, black, white], 8).unwrap();
        let Node::Url(q) = out else { panic!() };
        let Node::Quoted { value, .. } = *q else { panic!() };
        assert!(value.starts_with("data:image/svg+xml,%3Csvg%20xmlns%3D%22http"));
        assert!(value.contains("stop-color%3D%22%23000000%22"));
    }
}
