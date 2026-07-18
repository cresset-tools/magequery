//! Math fns: `ceil, floor, round(n,places?), sqrt, abs, sin, cos, tan, asin,
//! acos, atan` (plan §2.7). Mirrors less.js `functions/math.js` +
//! `math-helper.js`: the trig functions `unify()` their argument first (so
//! `sin(10deg)` converts deg→rad) and strip/replace the unit per function;
//! `round()` is JS `Number.toFixed` — ties round *away from zero* (§3-G).

use super::as_dimension;
use crate::ast::Node;
use crate::value::Dimension;

/// What happens to the argument's unit (less.js `mathFunctions` unit column).
pub(super) enum UnitRule {
    /// `null` — keep the argument's unit (`ceil`/`floor`/`sqrt`/`abs`/`round`).
    Keep,
    /// `''` — unify, then drop the unit (`sin`/`cos`/`tan`).
    Strip,
    /// `'rad'` — unify, result is radians (`asin`/`acos`/`atan`).
    Rad,
}

/// less.js `mathHelper(fn, unit, n)`.
pub(super) fn unary(args: &[Node], f: fn(f64) -> f64, rule: UnitRule) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    Some(Node::Dimension(match rule {
        UnitRule::Keep => Dimension {
            value: f(d.value),
            unit: d.unit.clone(),
        },
        UnitRule::Strip => Dimension::number(f(d.unify().value)),
        UnitRule::Rad => Dimension::with_unit(f(d.unify().value), "rad"),
    }))
}

/// less.js `round(n, places?)` — `num.toFixed(places)` then re-parse: decimal
/// rounding with ties away from zero (JS `toFixed`), keeping the unit.
pub(super) fn round(args: &[Node]) -> Option<Node> {
    let d = as_dimension(args.first()?)?;
    let places = match args.get(1) {
        Some(Node::Dimension(p)) => p.value as i32,
        _ => 0,
    };
    Some(Node::Dimension(Dimension {
        value: to_fixed(d.value, places),
        unit: d.unit.clone(),
    }))
}

/// JS `Number.prototype.toFixed` numerically: round to `places` decimal digits,
/// ties away from zero (the spec's "pick the larger n" on the absolute value).
/// Works on the *exact* decimal expansion of the double (with 40 guard digits),
/// not a `v * 10^p` float multiply — `(0.615).toFixed(2)` is `"0.61"` because
/// the exact value sits below the tie, which the multiply would round across.
pub(super) fn to_fixed(v: f64, places: i32) -> f64 {
    if !v.is_finite() {
        return v;
    }
    let places = places.max(0) as usize;
    let neg = v < 0.0;
    let s = format!("{:.*}", places + 40, v.abs());
    let (int_part, frac) = s.split_once('.').unwrap_or((s.as_str(), ""));
    let cut = places.min(frac.len());
    let mut digits: Vec<u8> = int_part
        .bytes()
        .chain(frac[..cut].bytes())
        .map(|b| b - b'0')
        .collect();
    if frac[cut..].bytes().next().is_some_and(|d| d >= b'5') {
        // Round up (away from zero), propagating the carry.
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let int_len = digits.len() - cut;
    let mut out = String::with_capacity(digits.len() + 2);
    for (i, d) in digits.iter().enumerate() {
        if i == int_len {
            out.push('.');
        }
        out.push((b'0' + d) as char);
    }
    let val: f64 = out.parse().unwrap_or(0.0);
    if neg { -val } else { val }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::Unit;

    #[test]
    fn round_half_away_from_zero() {
        // JS (2.5).toFixed(0) === "3", (-2.5).toFixed(0) === "-3".
        assert_eq!(to_fixed(2.5, 0), 3.0);
        assert_eq!(to_fixed(-2.5, 0), -3.0);
        assert_eq!(to_fixed(10.0 / 3.0, 3), 3.333);
        // Binary float below the tie: (0.615).toFixed(2) === "0.61".
        assert_eq!(to_fixed(0.615, 2), 0.61);
    }

    #[test]
    fn trig_unifies_units() {
        // sin(10deg) → converts to rad first, result unitless.
        let d = Node::Dimension(Dimension::with_unit(10.0, "deg"));
        let out = unary(&[d], f64::sin, UnitRule::Strip).unwrap();
        let Node::Dimension(r) = out else { panic!() };
        assert!((r.value - 0.17364817766693033).abs() < 1e-12);
        assert_eq!(r.unit, Unit::none());
    }
}
