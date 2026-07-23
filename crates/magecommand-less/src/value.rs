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
/// (plan §2.18): `Number((v + 2e-16).toFixed(p))`, then `String(value)`, with
/// the tiny-value `toFixed(20)` guard and `-0`→`0` normalization. The fround is
/// **decimal** (JS `toFixed`) rounding, NOT a `*10^p` float multiply — the
/// multiply crosses half boundaries the exact decimal expansion sits below
/// (`179.999999995` must print `179.99999999`, not `180`).
pub fn format_number(v: f64, num_precision: u8) -> String {
    let value = if num_precision > 0 {
        to_fixed(v + 2e-16, num_precision as i32)
    } else {
        v
    };

    if value == 0.0 {
        return "0".to_string(); // also normalizes -0
    }
    if value.is_finite() && value.abs() < 0.000001 {
        // String() would print exponential form — less.js emits toFixed(20)
        // with trailing zeros stripped instead.
        let s = format!("{value:.20}");
        return s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    js_number_string(value)
}

/// Format a dimension like **less.php** `Tree\Dimension::genCSS` rather than
/// less.js. Two independent divergences, both observable in real deploys:
///
///  1. `fround` is `round($v * 10^p) / 10^p` — a FLOAT multiply, not JS's
///     decimal `toFixed`, so it rounds across half boundaries the exact
///     decimal expansion sits below.
///  2. `(string)$value` runs under `Less_Parser`'s `@ini_set('precision', 16)`,
///     i.e. PHP's `%.16G`, NOT JS's shortest-round-trip. This is why a literal
///     `66.6%` deploys as `66.59999999999999%`: the double nearest 66.6 needs
///     16 significant digits to differ from 66.6, and PHP prints all of them.
///
/// The tiny-value branch mirrors `number_format($v, 10)` with trailing zeros
/// stripped, which less.php substitutes to avoid `1e-6` style output.
pub fn format_number_php(v: f64, num_precision: u8) -> String {
    let value = if num_precision > 0 {
        let p = 10f64.powi(num_precision as i32);
        // PHP `round()` is half-away-from-zero, like Rust's `f64::round`.
        (v * p).round() / p
    } else {
        v
    };
    if value == 0.0 {
        return "0".to_string(); // also normalizes -0
    }
    if value.abs() < 0.000001 {
        let s = format!("{value:.10}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        return if s.is_empty() { "0".to_string() } else { s.to_string() };
    }
    php_gcvt(value, 16)
}

/// PHP's `zend_gcvt(value, ndigit, '.', 'E')` — what `(string)$float` uses.
/// Round to `ndigit` significant digits, drop trailing zeros, then choose
/// fixed or exponential notation by the decimal point position.
fn php_gcvt(v: f64, ndigit: usize) -> String {
    let neg = v < 0.0;
    // `{:.*e}` with ndigit-1 fractional digits == ndigit significant digits.
    let s = format!("{:.*e}", ndigit - 1, v.abs());
    let (mant, exp) = s.split_once('e').expect("rust exponential form");
    let exp: i32 = exp.parse().expect("rust exponent");
    let all: String = mant.chars().filter(|c| *c != '.').collect();
    let digits = all.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    // Digits before the decimal point.
    let decpt = exp + 1;

    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if decpt < -3 || decpt > ndigit as i32 {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('E');
        let e = decpt - 1;
        out.push(if e >= 0 { '+' } else { '-' });
        out.push_str(&e.abs().to_string());
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..-decpt {
            out.push('0');
        }
        out.push_str(digits);
    } else if decpt as usize >= digits.len() {
        out.push_str(digits);
        for _ in 0..decpt as usize - digits.len() {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..decpt as usize]);
        out.push('.');
        out.push_str(&digits[decpt as usize..]);
    }
    out
}

/// JS `Number.prototype.toFixed` numerically: round to `places` decimal digits,
/// ties away from zero (the spec's "pick the larger n" on the absolute value).
/// Works on the *exact* decimal expansion of the double (with 40 guard digits),
/// not a `v * 10^p` float multiply — `(0.615).toFixed(2)` is `"0.61"` because
/// the exact value sits below the tie, which the multiply would round across.
pub(crate) fn to_fixed(v: f64, places: i32) -> f64 {
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

/// Emit a float like JavaScript's `String(number)`: `NaN`/`Infinity` spellings,
/// exponential form for |v| ≥ 1e21 (`1e+21`) and 0 < |v| < 1e-6 (`1e-7` — the
/// rgba/hsla alpha join case; dimension genCSS masks this branch with its own
/// `toFixed(20)` guard above), Rust's shortest round-trip `{}` otherwise (which
/// matches JS for the normal range), and integer-valued floats without a
/// decimal point.
pub(crate) fn js_number_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if v != 0.0 && (v.abs() >= 1e21 || v.abs() < 1e-6) {
        // JS switches to exponential: String(1e21) === "1e+21",
        // String(0.0000001) === "1e-7".
        let s = format!("{v:e}"); // shortest mantissa, e.g. "1e21" / "-1.5e22"
        return match s.split_once('e') {
            Some((m, exp)) if !exp.starts_with('-') => format!("{m}e+{exp}"),
            _ => s,
        };
    }
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod php_number_tests {
    use super::format_number_php;

    /// The artifact that makes a Magento deploy's CSS differ from less.js's:
    /// `Less_Parser` runs under `@ini_set('precision', 16)`, so the double
    /// nearest `66.6` prints all 16 significant digits. A literal `66.6%` in a
    /// third-party stylesheet really does deploy as `66.59999999999999%`.
    #[test]
    fn sixteen_significant_digits_leak_the_binary_value() {
        assert_eq!(format_number_php(66.6, 8), "66.59999999999999");
        // 56.2 rounds to a 16-digit form whose tail is zeros, so it prints short.
        assert_eq!(format_number_php(56.2, 8), "56.2");
        assert_eq!(format_number_php(125.0, 8), "125");
        assert_eq!(format_number_php(0.5, 8), "0.5");
        assert_eq!(format_number_php(-0.0, 8), "0");
    }

    /// `fround` is `round($v * 10^p) / 10^p` at `numPrecision = 8`.
    #[test]
    fn fround_is_a_float_multiply_at_eight_places() {
        assert_eq!(format_number_php(1.234_567_891_2, 8), "1.23456789");
        assert_eq!(format_number_php(0.000_000_004, 8), "0");
    }

    /// Below 1e-6 less.php substitutes `number_format($v, 10)` with trailing
    /// zeros stripped, rather than letting PHP print exponential form.
    #[test]
    fn tiny_values_use_the_number_format_branch() {
        assert_eq!(format_number_php(0.000_000_5, 8), "0.0000005");
    }
}
