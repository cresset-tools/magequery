//! List fns: `length, extract(list,i) (1-based), range(start?,end,step?)` and
//! the `~(…)` paren-escape (plan §2.7). Mirrors less.js `functions/list.js`
//! (`getItemsFromNode`: non-array values count as a 1-item list).
//!
//! `each(list, ruleset)` is DEFERRED to the detached-ruleset phase: its body is
//! a ruleset *call* (anonymous-mixin params, `&` re-eval) — the machinery
//! `@dr()` calls introduce (see NOTES.md).

use crate::ast::Node;
use crate::value::Dimension;

/// The list items of a node (`getItemsFromNode`).
fn items_of(node: &Node) -> Vec<Node> {
    match node {
        Node::Value(v) | Node::Expression(v) => v.clone(),
        other => vec![other.clone()],
    }
}

/// less.js `length(values)`.
pub(super) fn length(args: &[Node]) -> Option<Node> {
    let n = items_of(args.first()?).len();
    Some(Node::Dimension(Dimension::number(n as f64)))
}

/// less.js `extract(values, index)` — 1-based; out of range → `None`
/// (→ passthrough, like the JS `undefined` return).
pub(super) fn extract(args: &[Node]) -> Option<Node> {
    let values = items_of(args.first()?);
    let index = match args.get(1)? {
        Node::Dimension(d) => d.value,
        _ => return None,
    };
    let idx = index as i64 - 1;
    if idx < 0 {
        return None;
    }
    values.get(idx as usize).cloned()
}

/// less.js `range(start?, end, step?)` — inclusive, end's unit on every item.
pub(super) fn range(args: &[Node]) -> Option<Node> {
    let (from, to, step) = match (args.first(), args.get(1), args.get(2)) {
        (Some(Node::Dimension(a)), Some(Node::Dimension(b)), step) => {
            let s = match step {
                Some(Node::Dimension(s)) => s.value,
                _ => 1.0,
            };
            (a.value, b.clone(), s)
        }
        (Some(Node::Dimension(a)), None, _) => (1.0, a.clone(), 1.0),
        _ => return None,
    };
    let mut list = Vec::new();
    let mut i = from;
    while i <= to.value {
        list.push(Node::Dimension(Dimension {
            value: i,
            unit: to.unit.clone(),
        }));
        i += step;
    }
    Some(Node::Expression(list))
}

/// less.js `'~'(...expr)` — the parenthesized-list escape: one arg passes
/// through, several become a comma list.
pub(super) fn tilde(args: &[Node]) -> Option<Node> {
    if args.len() == 1 {
        return args.first().cloned();
    }
    Some(Node::Value(args.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_is_one_based_and_scalar_is_singleton() {
        let list = Node::Expression(vec![
            Node::Keyword("a".into()),
            Node::Keyword("b".into()),
        ]);
        let idx = Node::Dimension(Dimension::number(2.0));
        assert!(matches!(
            extract(&[list.clone(), idx]).unwrap(),
            Node::Keyword(k) if k == "b"
        ));
        // A non-list value is a 1-item list.
        let one = Node::Keyword("solo".into());
        assert!(matches!(length(&[one]).unwrap(), Node::Dimension(d) if d.value == 1.0));
    }

    #[test]
    fn range_takes_end_unit() {
        let out = range(&[
            Node::Dimension(Dimension::number(10.0)),
            Node::Dimension(Dimension::with_unit(30.0, "px")),
            Node::Dimension(Dimension::number(10.0)),
        ])
        .unwrap();
        let Node::Expression(items) = out else { panic!() };
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], Node::Dimension(d) if d.value == 10.0 && d.unit.is("px")));
    }
}
