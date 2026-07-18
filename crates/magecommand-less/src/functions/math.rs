//! Math fns: `ceil, floor, round(n,places?), sqrt, abs, sin, cos, tan, asin,
//! acos, atan` (plan §2.7). Mirrors less.js `functions/math.js` +
//! `math-helper.js`: the trig functions `unify()` their argument first (so
//! `sin(10deg)` converts deg→rad) and strip/replace the unit per function;
//! `round()` is JS `Number.toFixed` — ties round *away from zero* (§3-G).
//!
//! Error parity (Phase 3 review): `MathHelper` throws for a non-Dimension
//! argument (`argument must be a number`) and the `Dimension` constructor
//! throws on a NaN result (`Dimension is not a number.`, e.g. `sqrt(-1)`);
//! both propagate as compile errors in less.js — they are NOT part of the
//! caught-throw passthrough set (only `min`/`max` swallow).

use super::{dim_node, js_arg_num, FnResult};
use crate::ast::Node;
use crate::error::{ErrorKind, LessError};
use crate::value::to_fixed;

/// What happens to the argument's unit (less.js `mathFunctions` unit column).
pub(super) enum UnitRule {
    /// `null` — keep the argument's unit (`ceil`/`floor`/`sqrt`/`abs`/`round`).
    Keep,
    /// `''` — unify, then drop the unit (`sin`/`cos`/`tan`).
    Strip,
    /// `'rad'` — unify, result is radians (`asin`/`acos`/`atan`).
    Rad,
}

/// less.js `mathHelper(fn, unit, n)` — non-Dimension argument throws.
pub(super) fn unary(args: &[Node], f: fn(f64) -> f64, rule: UnitRule) -> FnResult {
    let Some(Node::Dimension(d)) = args.first() else {
        return Err(LessError::new(ErrorKind::Argument, "argument must be a number"));
    };
    Ok(Some(match rule {
        UnitRule::Keep => dim_node(f(d.value), d.unit.clone())?,
        UnitRule::Strip => dim_node(f(d.unify().value), crate::unit::Unit::none())?,
        UnitRule::Rad => dim_node(f(d.unify().value), crate::unit::Unit::single("rad"))?,
    }))
}

/// less.js `round(n, places?)` — `num.toFixed(places)` then re-parse: decimal
/// rounding with ties away from zero (JS `toFixed`), keeping the unit. A
/// `places` outside `0..=100` is JS `toFixed`'s RangeError (a compile error).
pub(super) fn round(args: &[Node]) -> FnResult {
    let Some(Node::Dimension(d)) = args.first() else {
        return Err(LessError::new(ErrorKind::Argument, "argument must be a number"));
    };
    // JS ToIntegerOrInfinity on `f.value`: truncate toward zero, NaN → 0.
    let places_f = match args.get(1) {
        Some(n) => {
            let v = js_arg_num(n);
            if v.is_nan() { 0.0 } else { v.trunc() }
        }
        None => 0.0,
    };
    if !(0.0..=100.0).contains(&places_f) {
        return Err(LessError::new(
            ErrorKind::Runtime,
            "toFixed() digits argument must be between 0 and 100",
        ));
    }
    Ok(Some(dim_node(to_fixed(d.value, places_f as i32), d.unit.clone())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::Unit;
    use crate::value::Dimension;

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
        let out = unary(&[d], f64::sin, UnitRule::Strip).unwrap().unwrap();
        let Node::Dimension(r) = out else { panic!() };
        assert!((r.value - 0.17364817766693033).abs() < 1e-12);
        assert_eq!(r.unit, Unit::none());
    }

    #[test]
    fn error_parity_wrong_type_and_nan() {
        // less.js: ceil(foo) → "argument must be a number" (propagates).
        let kw = Node::Keyword("foo".into());
        assert!(unary(&[kw], f64::ceil, UnitRule::Keep).is_err());
        // sqrt(-1) → NaN → "Dimension is not a number.".
        let neg = Node::Dimension(Dimension::number(-1.0));
        assert!(unary(&[neg], f64::sqrt, UnitRule::Keep).is_err());
        // round(1.5, -2) → toFixed RangeError.
        let d = Node::Dimension(Dimension::number(1.5));
        let p = Node::Dimension(Dimension::number(-2.0));
        assert!(round(&[d.clone(), p]).is_err());
        // round(1.23456789012, 101) → RangeError too.
        let p = Node::Dimension(Dimension::number(101.0));
        assert!(round(&[d, p]).is_err());
    }
}
