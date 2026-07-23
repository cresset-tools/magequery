//! Number fns: `percentage, min, max, convert(v,unit), pi, mod, pow`
//! (plan §2.7). `min`/`max` port less.js `functions/number.js`'s `minMax`
//! verbatim — the DUAL behavior of §4.8: reduce compatible-unit args to one
//! dimension, flatten list args, and on any incompatible/non-dimension arg give
//! up (→ the caller re-emits a literal CSS `min()`/`max()` with evaluated args).

use super::{dim_node, js_arg_num, undef_err, FnResult};
use crate::ast::Node;
use crate::css::render_value_cz;
use crate::error::{ErrorKind, LessError};
use crate::unit::Unit;
use crate::value::Dimension;

/// less.js `percentage(n)` — `mathHelper(num => num * 100, '%', n)`: the
/// non-null unit means the argument is `unify()`d first; a non-Dimension is
/// MathHelper's propagated `argument must be a number` throw.
pub(super) fn percentage(args: &[Node]) -> FnResult {
    let Some(Node::Dimension(d)) = args.first() else {
        return Err(LessError::new(ErrorKind::Argument, "argument must be a number"));
    };
    Ok(Some(dim_node(d.unify().value * 100.0, Unit::single("%"))?))
}

/// less.js `convert(val, unit)` — `val.convertTo(unit.value)`: the unit is ANY
/// node's `.value` (a Dimension unit-arg gives a number → `convertTo` no-ops,
/// F7); a missing unit is the `undefined.value` TypeError; a non-Dimension val
/// the `val.convertTo is not a function` one.
pub(super) fn convert(args: &[Node]) -> FnResult {
    let d = match args.first() {
        Some(Node::Dimension(d)) => d,
        Some(_) => {
            return Err(LessError::new(
                ErrorKind::Runtime,
                "val.convertTo is not a function",
            ))
        }
        None => return Err(undef_err("convertTo")),
    };
    let unit = match args.get(1) {
        Some(Node::Keyword(k)) => Some(k.clone()),
        Some(Node::Quoted { value, .. }) => Some(value.clone()),
        Some(Node::Anonymous(s)) => Some(s.clone()),
        Some(_) => None, // non-string `.value` → convertTo no-ops
        None => return Err(undef_err("value")),
    };
    Ok(Some(match unit {
        Some(u) => Node::Dimension(d.convert_to_unit(&u)),
        None => Node::Dimension(d.clone()),
    }))
}

/// less.js `mod(a, b)` — JS `%` (truncated remainder) over `.value` coercion
/// (`mod("3", 2)` → 1), first operand's unit; a NaN result is the Dimension
/// constructor's `Dimension is not a number.` throw (`mod(7, 0)`, `mod(foo, 2)`).
pub(super) fn modulo(args: &[Node]) -> FnResult {
    let a = args.first().ok_or_else(|| undef_err("value"))?;
    let b = args.get(1).ok_or_else(|| undef_err("value"))?;
    let unit = match a {
        Node::Dimension(d) => d.unit.clone(),
        _ => Unit::none(),
    };
    Ok(Some(dim_node(js_arg_num(a) % js_arg_num(b), unit)?))
}

/// less.js `pow(x, y)` — `x^y` in x's unit; non-Dimension args throw
/// `arguments must be numbers`; a NaN result (`pow(-1, 0.5)`) throws too.
pub(super) fn pow(args: &[Node]) -> FnResult {
    let (Some(Node::Dimension(x)), Some(Node::Dimension(y))) = (args.first(), args.get(1)) else {
        return Err(LessError::new(ErrorKind::Argument, "arguments must be numbers"));
    };
    Ok(Some(dim_node(x.value.powf(y.value), x.unit.clone())?))
}

/// less.js `minMax(isMin, args)` — a faithful port including the `values['']`
/// bookkeeping and the arg-flattening of list arguments (plan §4.8). `None` =
/// the caught `throw` → literal passthrough.
pub(super) fn min_max(
    args: &[Node],
    is_min: bool,
    np: u8,
    compress: bool,
    keep_zero_units: bool,
    php_numbers: bool,
) -> Option<Node> {
    let mut queue: Vec<Node> = args.to_vec();
    let mut order: Vec<Dimension> = Vec::new();
    // key "" is the unitless slot, like the JS `values` object.
    let mut values: Vec<(String, usize)> = Vec::new();
    let mut unit_static: Option<String> = None;
    let mut unit_clone: Option<String> = None;

    if queue.is_empty() {
        return None;
    }

    let mut i = 0;
    while i < queue.len() {
        let current = queue[i].clone();
        i += 1;
        let d = match &current {
            Node::Dimension(d) => d.clone(),
            Node::Expression(items) | Node::Value(items) => {
                // `Array.prototype.push.apply(args, item.value)` — flatten.
                queue.extend(items.iter().cloned());
                continue;
            }
            _ => return None, // incompatible types → passthrough
        };

        let current_unified = if d.unit.is_empty() && unit_clone.is_some() {
            Dimension {
                value: d.value,
                unit: crate::unit::Unit::single(unit_clone.as_deref().unwrap()),
            }
            .unify()
        } else {
            d.unify()
        };
        let cu = current_unified.unit.to_unit_string();
        let unit = if cu.is_empty() && unit_static.is_some() {
            unit_static.clone().unwrap()
        } else {
            cu
        };
        unit_static = if (!unit.is_empty() && unit_static.is_none())
            || (!unit.is_empty() && order[..].first().map_or(false, |o| o.unify().unit.is_empty()))
        {
            Some(unit.clone())
        } else {
            unit_static
        };
        if !unit.is_empty() && unit_clone.is_none() {
            unit_clone = Some(d.unit.to_unit_string());
        }
        let empty_slot = values.iter().find(|(k, _)| k.is_empty()).map(|(_, j)| *j);
        let j = if empty_slot.is_some() && !unit.is_empty() && Some(&unit) == unit_static.as_ref() {
            empty_slot
        } else {
            values.iter().find(|(k, _)| *k == unit).map(|(_, j)| *j)
        };
        let Some(j) = j else {
            if let Some(us) = &unit_static {
                if unit != *us {
                    return None; // incompatible types → passthrough
                }
            }
            values.push((unit.clone(), order.len()));
            order.push(d);
            continue;
        };
        let reference_unified = if order[j].unit.is_empty() && unit_clone.is_some() {
            Dimension {
                value: order[j].value,
                unit: crate::unit::Unit::single(unit_clone.as_deref().unwrap()),
            }
            .unify()
        } else {
            order[j].unify()
        };
        if (is_min && current_unified.value < reference_unified.value)
            || (!is_min && current_unified.value > reference_unified.value)
        {
            order[j] = d;
        }
    }

    if order.len() == 1 {
        return Some(Node::Dimension(order.pop().unwrap()));
    }
    // The literal passthrough joins with the eval context's separator —
    // less.js `minMax` uses `context.compress ? ',' : ', '` (§C4).
    let rendered: Vec<String> = order
        .iter()
        .map(|d| render_value_cz(&Node::Dimension(d.clone()), np, compress, keep_zero_units, php_numbers))
        .collect();
    Some(Node::Anonymous(format!(
        "{}({})",
        if is_min { "min" } else { "max" },
        rendered.join(if compress { "," } else { ", " })
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dim(v: f64, u: &str) -> Node {
        Node::Dimension(Dimension::with_unit(v, u))
    }

    #[test]
    fn min_reduces_compatible_units() {
        let out = min_max(&[dim(1.0, "cm"), dim(3.0, "mm")], true, 8, false, false, false).unwrap();
        let Node::Dimension(d) = out else { panic!() };
        assert_eq!(d.value, 3.0);
        assert_eq!(d.unit.to_unit_string(), "mm");
    }

    #[test]
    fn min_mixed_groups_render_literally() {
        // less.js: min(6em, 5, 4ex, 3, 2pt, 1) → "min(1, 4ex, 2pt)".
        let out = min_max(
            &[dim(6.0, "em"), dim(5.0, ""), dim(4.0, "ex"), dim(3.0, ""), dim(2.0, "pt"), dim(1.0, "")],
            true,
            8,
            false,
            false,
            false,
        )
        .unwrap();
        let Node::Anonymous(s) = out else { panic!() };
        assert_eq!(s, "min(1, 4ex, 2pt)");
    }

    #[test]
    fn max_keeps_original_spelling_of_winner() {
        // less.js: max(1px, 2, 3em, 4, 5m, 6) → "max(5m, 3em)".
        let out = min_max(
            &[dim(1.0, "px"), dim(2.0, ""), dim(3.0, "em"), dim(4.0, ""), dim(5.0, "m"), dim(6.0, "")],
            false,
            8,
            false,
            false,
            false,
        )
        .unwrap();
        let Node::Anonymous(s) = out else { panic!() };
        assert_eq!(s, "max(5m, 3em)");
    }

    #[test]
    fn non_dimension_arg_passes_through() {
        let call = Node::Call {
            name: "calc".into(),
            args: vec![dim(1.0, "")],
            span: Default::default(),
        };
        assert!(min_max(&[call, dim(1.0, "")], true, 8, false, false, false).is_none());
    }
}
