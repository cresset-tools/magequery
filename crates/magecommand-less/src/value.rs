//! The `Dimension` runtime value (plan §9.3).
//!
//! `f64` end-to-end with its own `numPrecision=8` `fround` at output time —
//! lightningcss's `f32` + shortest-CSS rounding would inject ULP divergence, so
//! this is a dedicated type.

use crate::unit::Unit;

/// A number with a (possibly compound) unit (plan §9.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    /// The numeric value (unrounded; `fround` applied only at genCSS time).
    pub value: f64,
    /// The unit multiset.
    pub unit: Unit,
}

impl Dimension {
    /// A unitless number.
    pub fn number(value: f64) -> Self {
        Dimension {
            value,
            unit: Unit::none(),
        }
    }

    /// A number with a single unit, e.g. `12px`.
    pub fn with_unit(value: f64, unit: &str) -> Self {
        Dimension {
            value,
            unit: Unit::single(unit),
        }
    }
}
