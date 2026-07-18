//! The dimension unit model (plan §2.18, §9.3).
//!
//! A `Unit` is an **open multiset** of unit strings (numerator + denominator) —
//! lightningcss's closed typed enums cannot represent `px·px/em` or unknown
//! units, so this is a dedicated type. `*`/`/` do unit algebra (multiset concat +
//! `cancel()`); `+`/`-` convert to the first operand's unit.

use smallvec::SmallVec;

/// One side of a unit multiset. `SmallVec<[_; 1]>` keeps the common single-unit
/// case on the stack (plan §9.6).
pub type UnitParts = SmallVec<[Box<str>; 1]>;

/// An open-multiset dimension unit (plan §9.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unit {
    /// Numerator unit strings.
    pub numerator: UnitParts,
    /// Denominator unit strings.
    pub denominator: UnitParts,
    /// `backupUnit` — the guessed output unit under `strictUnits:false`.
    pub backup: Option<Box<str>>,
}

impl Unit {
    /// The empty (unitless) unit.
    pub fn none() -> Self {
        Unit::default()
    }

    /// A simple single-numerator unit, e.g. `px`.
    pub fn single(name: &str) -> Self {
        let mut numerator: UnitParts = SmallVec::new();
        numerator.push(name.into());
        Unit {
            numerator,
            denominator: SmallVec::new(),
            backup: None,
        }
    }

    /// Whether this unit is empty (unitless).
    pub fn is_empty(&self) -> bool {
        self.numerator.is_empty() && self.denominator.is_empty()
    }
}
