//! Type/data fns: the `is*` checks, `unit(dim,unit?)`, `get-unit` (plan §2.6,
//! §2.7). Mirrors less.js `functions/types.js`; `isdefined` lives in the
//! evaluator (it must not evaluate its argument).

use super::as_dimension;
use crate::ast::Node;
use crate::css::render_value;
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
/// A Keyword unit contributes its name; anything else contributes its `toCSS()`
/// (so a *quoted* unit keeps its quotes — faithful to less.js).
pub(super) fn unit(args: &[Node], np: u8) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    let unit_str = match args.get(1) {
        None => String::new(),
        Some(Node::Keyword(k)) => k.clone(),
        Some(other) => render_value(other, np),
    };
    Some(Node::Dimension(Dimension::with_unit(d.value, &unit_str)))
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
        let out = unit(&[d.clone()], 8).unwrap();
        assert!(matches!(&out, Node::Dimension(r) if r.unit.is_empty() && r.value == 12.0));
        let em = Node::Keyword("em".into());
        let out = unit(&[d, em], 8).unwrap();
        assert!(matches!(&out, Node::Dimension(r) if r.unit.is("em")));
    }
}
