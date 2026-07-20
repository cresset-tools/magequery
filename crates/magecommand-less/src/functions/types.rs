//! Type/data fns: the `is*` checks, `unit(dim,unit?)`, `get-unit` (plan §2.6,
//! §2.7). Mirrors less.js `functions/types.js`; `isdefined` lives in the
//! evaluator (it must not evaluate its argument).

use super::{as_dimension, FnResult};
use crate::ast::Node;
use crate::css::render_value;
use crate::error::{ErrorKind, LessError};
use crate::value::Dimension;

/// A `true`/`false` keyword node.
pub(super) fn bool_keyword(b: bool) -> Node {
    Node::Keyword(if b { "true" } else { "false" }.to_string())
}

/// `n` is a dimension with the given unit (case-insensitive).
pub(super) fn is_unit(n: Option<&Node>, unit: &str) -> bool {
    matches!(n, Some(Node::Dimension(d)) if d.unit.is(unit))
}

/// `isunit(value, unit)` — the unit arg may be a keyword, quoted, or `%`.
pub(super) fn isunit(args: &[Node]) -> bool {
    let unit = match args.get(1) {
        Some(Node::Keyword(k)) => k.clone(),
        Some(Node::Quoted { value, .. }) => value.clone(),
        Some(Node::Anonymous(s)) => s.clone(),
        _ => return false,
    };
    if unit.is_empty() {
        // `isunit(32, '')` — true iff the dimension is unitless.
        return matches!(args.first(), Some(Node::Dimension(d)) if d.unit.is_empty());
    }
    is_unit(args.first(), &unit)
}

/// less.js `unit(val, unit?)` — set/strip the unit keeping the numeric value.
/// A Keyword unit contributes its name; anything else contributes its
/// **context-less** `toCSS()` (a *quoted* unit keeps its quotes, and a
/// Dimension unit keeps its FULL float digits — no `numPrecision` fround, F8).
/// A non-Dimension first argument is less.js's propagated Argument throw, with
/// the parenthesis hint for a deferred Operation (`unit(10/3, px)`).
pub(super) fn unit(args: &[Node]) -> FnResult {
    let d = match args.first() {
        Some(Node::Dimension(d)) => d,
        other => {
            let hint = if matches!(other, Some(Node::Operation { .. })) {
                ". Have you forgotten parenthesis?"
            } else {
                ""
            };
            return Err(LessError::new(
                ErrorKind::Argument,
                format!("the first argument to unit must be a number{hint}"),
            ));
        }
    };
    let unit_str = match args.get(1) {
        None => String::new(),
        Some(Node::Keyword(k)) => k.clone(),
        Some(other) => render_value(other, 0),
    };
    Ok(Some(Node::Dimension(Dimension::with_unit(d.value, &unit_str))))
}

/// less.js `get-unit(n)` — the unit as anonymous text (empty when unitless).
pub(super) fn get_unit(args: &[Node]) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    Some(Node::Anonymous(d.unit.to_unit_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isunit_empty_string_means_unitless() {
        let d = Node::Dimension(Dimension::number(32.0));
        let empty = Node::Quoted {
            escaped: false,
            quote: '\'',
            value: String::new(),
        };
        assert!(isunit(&[d, empty]));
    }

    #[test]
    fn unit_sets_and_strips() {
        let d = Node::Dimension(Dimension::with_unit(12.0, "px"));
        let out = unit(&[d.clone()]).unwrap().unwrap();
        assert!(matches!(&out, Node::Dimension(r) if r.unit.is_empty() && r.value == 12.0));
        let em = Node::Keyword("em".into());
        let out = unit(&[d, em]).unwrap().unwrap();
        assert!(matches!(&out, Node::Dimension(r) if r.unit.is("em")));
        // Non-number first arg errors (less.js Argument throw), colors too.
        let c = Node::Color(crate::color::Color::from_hex("#ffffff").unwrap());
        let px = Node::Keyword("px".into());
        assert!(unit(&[c, px]).is_err());
        // A Dimension unit arg keeps FULL digits: unit(5, 1.234567891px) (F8).
        let five = Node::Dimension(Dimension::number(5.0));
        let long = Node::Dimension(Dimension::with_unit(1.234567891, "px"));
        let out = unit(&[five, long]).unwrap().unwrap();
        assert!(matches!(&out, Node::Dimension(r) if r.unit.is("1.234567891px")));
    }
}
