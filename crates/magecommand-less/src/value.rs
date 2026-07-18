//! The `Dimension` runtime value (plan §9.3).
//!
//! `f64` end-to-end with its own `numPrecision=8` `fround` at output time —
//! lightningcss's `f32` + shortest-CSS rounding would inject ULP divergence, so
//! this is a dedicated type. Mirrors less.js `tree/dimension.js`.

use crate::unit::{Unit, UnitParts};

/// A number with a (possibly compound) unit (plan §9.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    /// The numeric value (unrounded; `fround` applied only at genCSS time).
    pub value: f64,
    /// The unit multiset.
    pub unit: Unit,
}

/// A `(unit, factor-to-base)` conversion family (less.js `data/unit-conversions.js`).
struct Family {
    units: &'static [(&'static str, f64)],
}

const LENGTH: Family = Family {
    units: &[
        ("m", 1.0),
        ("cm", 0.01),
        ("mm", 0.001),
        ("in", 0.0254),
        ("px", 0.0254 / 96.0),
        ("pt", 0.0254 / 72.0),
        ("pc", 0.0254 / 72.0 * 12.0),
    ],
};
const DURATION: Family = Family {
    units: &[("s", 1.0), ("ms", 0.001)],
};
const ANGLE: Family = Family {
    units: &[
        ("rad", 1.0 / (2.0 * std::f64::consts::PI)),
        ("deg", 1.0 / 360.0),
        ("grad", 1.0 / 400.0),
        ("turn", 1.0),
    ],
};

fn family_of(name: &str) -> Option<&'static str> {
    for (fname, fam) in [("length", &LENGTH), ("duration", &DURATION), ("angle", &ANGLE)] {
        if fam.units.iter().any(|(u, _)| *u == name) {
            return Some(fname);
        }
    }
    None
}

fn family_by_name(name: &str) -> Option<&'static Family> {
    match name {
        "length" => Some(&LENGTH),
        "duration" => Some(&DURATION),
        "angle" => Some(&ANGLE),
        _ => None,
    }
}

fn factor(fam: &Family, name: &str) -> Option<f64> {
    fam.units.iter().find(|(u, _)| *u == name).map(|(_, f)| *f)
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

    /// less.js `Dimension.operate` (per §2.4). `strict` toggles the incompatible-
    /// units throw; returns `Err(bad_units)` when strict and units clash.
    pub fn operate(&self, op: char, other: &Dimension, strict: bool) -> Result<Dimension, String> {
        let mut value = raw_op(op, self.value, other.value);
        let mut unit = self.unit.clone();

        match op {
            '+' | '-' => {
                if unit.numerator.is_empty() && unit.denominator.is_empty() {
                    unit = other.unit.clone();
                    if let Some(b) = &self.unit.backup {
                        unit.backup = Some(b.clone());
                    }
                } else if other.unit.numerator.is_empty() && unit.denominator.is_empty() {
                    // do nothing (mirrors less.js's exact — quirky — condition)
                } else {
                    let converted = other.convert_to_used(&self.unit);
                    if strict && converted.unit.to_unit_string() != unit.to_unit_string() {
                        return Err(format!(
                            "'{}' and '{}'",
                            unit.to_unit_string(),
                            converted.unit.to_unit_string()
                        ));
                    }
                    value = raw_op(op, self.value, converted.value);
                }
            }
            '*' => {
                let mut num: UnitParts = unit.numerator.clone();
                num.extend(other.unit.numerator.iter().cloned());
                let mut den: UnitParts = unit.denominator.clone();
                den.extend(other.unit.denominator.iter().cloned());
                num.sort();
                den.sort();
                unit.numerator = num;
                unit.denominator = den;
                unit.cancel();
            }
            '/' => {
                let mut num: UnitParts = unit.numerator.clone();
                num.extend(other.unit.denominator.iter().cloned());
                let mut den: UnitParts = unit.denominator.clone();
                den.extend(other.unit.numerator.iter().cloned());
                num.sort();
                den.sort();
                unit.numerator = num;
                unit.denominator = den;
                unit.cancel();
            }
            _ => {}
        }
        Ok(Dimension { value, unit })
    }

    /// Convert this dimension so its units match the families used by `target`
    /// (less.js `convertTo(this.unit.usedUnits())` for the `+`/`-` path).
    fn convert_to_used(&self, target: &Unit) -> Dimension {
        let mut conversions: Vec<(&'static str, String)> = Vec::new();
        for u in target.numerator.iter().chain(target.denominator.iter()) {
            if let Some(fam) = family_of(u) {
                if !conversions.iter().any(|(f, _)| *f == fam) {
                    conversions.push((fam, u.to_string()));
                }
            }
        }
        self.convert_to(&conversions)
    }

    /// Convert to a single target unit string (the `convert()` function).
    pub fn convert_to_unit(&self, target: &str) -> Dimension {
        if let Some(fam) = family_of(target) {
            self.convert_to(&[(fam, target.to_string())])
        } else {
            self.clone()
        }
    }

    /// less.js `unify()` — canonicalize to px/s/rad for comparison.
    pub fn unify(&self) -> Dimension {
        self.convert_to(&[
            ("length", "px".into()),
            ("duration", "s".into()),
            ("angle", "rad".into()),
        ])
    }

    /// less.js `convertTo(conversions)`: map each atomic unit through the ratio.
    fn convert_to(&self, conversions: &[(&'static str, String)]) -> Dimension {
        let mut value = self.value;
        let mut unit = self.unit.clone();
        for (fam_name, target_unit) in conversions {
            let fam = match family_by_name(fam_name) {
                Some(f) => f,
                None => continue,
            };
            let tfac = match factor(fam, target_unit) {
                Some(f) => f,
                None => continue,
            };
            for u in unit.numerator.iter_mut() {
                if let Some(f) = factor(fam, u) {
                    value *= f / tfac;
                    *u = target_unit.as_str().into();
                }
            }
            for u in unit.denominator.iter_mut() {
                if let Some(f) = factor(fam, u) {
                    value /= f / tfac;
                    *u = target_unit.as_str().into();
                }
            }
        }
        unit.cancel();
        Dimension { value, unit }
    }

    /// Coerce to a grey color triplet (less.js `Dimension.toColor`).
    pub fn to_color(&self) -> crate::color::Color {
        crate::color::Color {
            rgb: [self.value, self.value, self.value],
            alpha: 1.0,
            original: None,
        }
    }
}

/// The raw arithmetic on values (less.js `Node._operate`).
fn raw_op(op: char, a: f64, b: f64) -> f64 {
    match op {
        '+' => a + b,
        '-' => a - b,
        '*' => a * b,
        '/' => a / b,
        _ => a,
    }
}

/// Format a dimension's numeric value like less.js `Dimension.genCSS`/`fround`
/// (plan §2.18): `Math.round((v + 2e-16) * 10^p) / 10^p`, then `String(value)`,
/// with the tiny-value `toFixed(20)` guard and `-0`→`0` normalization.
pub fn format_number(v: f64, num_precision: u8) -> String {
    let value = if num_precision > 0 {
        let factor = 10f64.powi(num_precision as i32);
        js_round((v + 2e-16) * factor) / factor
    } else {
        v
    };

    if value == 0.0 {
        return "0".to_string(); // also normalizes -0
    }
    if value.abs() < 0.000001 {
        // would print in exponential form — emit fixed then strip trailing zeros.
        let s = format!("{value:.20}");
        return s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    js_number_to_string(value)
}

/// JS `Math.round` — half rounds toward +∞ (not away from zero).
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Emit a float like JavaScript's `String(number)` for the normal-magnitude
/// values LESS produces. Rust's shortest round-trip `{}` matches JS here, and
/// integer-valued floats print without a decimal point.
fn js_number_to_string(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}
