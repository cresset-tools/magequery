//! The dimension unit model (plan §2.18, §9.3).
//!
//! A `Unit` is an **open multiset** of unit strings (numerator + denominator) —
//! lightningcss's closed typed enums cannot represent `px·px/em` or unknown
//! units, so this is a dedicated type. `*`/`/` do unit algebra (multiset concat +
//! `cancel()`); `+`/`-` convert to the first operand's unit. Mirrors less.js
//! `tree/unit.js`.

use smallvec::SmallVec;

/// One side of a unit multiset. `SmallVec<[_; 1]>` keeps the common single-unit
/// case on the stack (plan §9.6).
pub type UnitParts = SmallVec<[Box<str>; 1]>;

/// An open-multiset dimension unit (plan §9.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unit {
    /// Numerator unit strings (kept sorted, like less.js).
    pub numerator: UnitParts,
    /// Denominator unit strings (kept sorted).
    pub denominator: UnitParts,
    /// `backupUnit` — the guessed output unit under `strictUnits:false`.
    pub backup: Option<Box<str>>,
}

impl Unit {
    /// The empty (unitless) unit.
    pub fn none() -> Self {
        Unit::default()
    }

    /// A simple single-numerator unit, e.g. `px`. The backup unit defaults to it,
    /// mirroring less.js's `Unit` constructor.
    pub fn single(name: &str) -> Self {
        if name.is_empty() {
            return Unit::none();
        }
        let mut numerator: UnitParts = SmallVec::new();
        numerator.push(name.into());
        Unit {
            numerator,
            denominator: SmallVec::new(),
            backup: Some(name.into()),
        }
    }

    /// Whether this unit is empty (unitless).
    pub fn is_empty(&self) -> bool {
        self.numerator.is_empty() && self.denominator.is_empty()
    }

    /// less.js `isSingular`: at most one numerator, no denominator.
    pub fn is_singular(&self) -> bool {
        self.numerator.len() <= 1 && self.denominator.is_empty()
    }

    /// less.js `toString`: `a*b/c/d`.
    pub fn to_unit_string(&self) -> String {
        let mut s = self
            .numerator
            .iter()
            .map(|u| u.as_ref())
            .collect::<Vec<_>>()
            .join("*");
        for d in &self.denominator {
            s.push('/');
            s.push_str(d);
        }
        s
    }

    /// less.js `is(unitString)` — case-insensitive equality with `toString`.
    pub fn is(&self, s: &str) -> bool {
        self.to_unit_string().eq_ignore_ascii_case(s)
    }

    /// less.js `isLength`: the rendered unit is a known CSS length keyword.
    pub fn is_length(&self) -> bool {
        matches!(
            self.rendered().to_ascii_lowercase().as_str(),
            "px" | "em" | "ex" | "ch" | "rem" | "in" | "cm" | "mm" | "pc" | "pt" | "vw" | "vh"
                | "vmin" | "vmax"
        )
    }

    /// The single unit string genCSS would emit (numerator, else backup, else
    /// denominator). `strictUnits` off is assumed here.
    pub fn rendered(&self) -> String {
        if self.numerator.len() == 1 {
            self.numerator[0].to_string()
        } else if let Some(b) = &self.backup {
            b.to_string()
        } else if let Some(d) = self.denominator.first() {
            d.to_string()
        } else {
            String::new()
        }
    }

    /// Emit the unit into the output string (less.js `Unit.genCSS`). In strict
    /// mode only a singular numerator is emitted.
    pub fn gen_css(&self, strict_units: bool, out: &mut String) {
        if self.numerator.len() == 1 {
            out.push_str(&self.numerator[0]);
        } else if !strict_units {
            if let Some(b) = &self.backup {
                out.push_str(b);
            } else if let Some(d) = self.denominator.first() {
                out.push_str(d);
            }
        }
    }

    /// Cancel matching numerator/denominator units (less.js `Unit.cancel`).
    /// A missing `backup` adopts the first numerator — less.js sets
    /// `backupUnit` inside `cancel()`, which is what makes `(1 * 10px) * 14cm`
    /// print `140px` (the multi-unit result falls back to px).
    pub fn cancel(&mut self) {
        use std::collections::BTreeMap;
        let mut counter: BTreeMap<Box<str>, i32> = BTreeMap::new();
        for u in &self.numerator {
            if self.backup.is_none() {
                self.backup = Some(u.clone());
            }
            *counter.entry(u.clone()).or_insert(0) += 1;
        }
        for u in &self.denominator {
            *counter.entry(u.clone()).or_insert(0) -= 1;
        }
        self.numerator.clear();
        self.denominator.clear();
        for (u, c) in counter {
            if c > 0 {
                for _ in 0..c {
                    self.numerator.push(u.clone());
                }
            } else if c < 0 {
                for _ in 0..(-c) {
                    self.denominator.push(u.clone());
                }
            }
        }
        self.numerator.sort();
        self.denominator.sort();
    }
}
