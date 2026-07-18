//! The `Color` runtime value (plan §2.18, §9.3).
//!
//! RGBA in `f64` per channel (unclamped until output) plus the *original*
//! literal, kept for round-trip fidelity — incl. `transparent`↔`rgba(…,0)` and
//! named-color preservation (§H3). Color math is per-channel and unclamped until
//! `toCSS`. Mirrors less.js `tree/color.js`.

use crate::data::colors::named_color;

/// An RGBA color that remembers how it was written (plan §9.3). `original` is
/// less.js's `value` field: a `#hex`/keyword literal is emitted verbatim, while
/// an `rgb`/`hsl` marker steers function-form output.
#[derive(Debug, Clone, PartialEq)]
pub struct Color {
    /// Red/green/blue, each `0.0..=255.0` at output (unclamped mid-computation).
    pub rgb: [f64; 3],
    /// Alpha, `0.0..=1.0`.
    pub alpha: f64,
    /// The original literal (`#fff`, `red`, `transparent`, `rgb`, `hsl`, …) for
    /// round-trip output, if the color came from source text / a color function.
    pub original: Option<Box<str>>,
}

impl Color {
    /// An opaque color from 8-bit channels.
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color {
            rgb: [r as f64, g as f64, b as f64],
            alpha: 1.0,
            original: None,
        }
    }

    /// A color from channels + alpha, tagged with its original literal.
    pub fn with_original(rgb: [f64; 3], alpha: f64, original: &str) -> Self {
        Color {
            rgb,
            alpha,
            original: Some(original.into()),
        }
    }

    /// Parse a `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa` literal, preserving the
    /// original spelling for round-trip (less.js parser passes `originalForm`).
    pub fn from_hex(original: &str) -> Option<Color> {
        let hex = original.strip_prefix('#').unwrap_or(original);
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let (rgb, alpha): ([f64; 3], f64) = match hex.len() {
            6 => {
                let a = parse_pairs(hex, 3)?;
                ([a[0], a[1], a[2]], 1.0)
            }
            8 => {
                let a = parse_pairs(hex, 4)?;
                ([a[0], a[1], a[2]], a[3] / 255.0)
            }
            3 => {
                let a = parse_singles(hex, 3)?;
                ([a[0], a[1], a[2]], 1.0)
            }
            4 => {
                let a = parse_singles(hex, 4)?;
                ([a[0], a[1], a[2]], a[3] / 255.0)
            }
            _ => return None,
        };
        Some(Color {
            rgb,
            alpha,
            original: Some(original.into()),
        })
    }

    /// less.js `Color.fromKeyword`: a named color (or `transparent`) → a Color
    /// tagged with the keyword so it round-trips as the keyword.
    pub fn from_keyword(keyword: &str) -> Option<Color> {
        let key = keyword.to_ascii_lowercase();
        if let Some([r, g, b]) = named_color(&key) {
            return Some(Color {
                rgb: [r as f64, g as f64, b as f64],
                alpha: 1.0,
                original: Some(keyword.into()),
            });
        }
        if key == "transparent" {
            return Some(Color {
                rgb: [0.0, 0.0, 0.0],
                alpha: 0.0,
                original: Some(keyword.into()),
            });
        }
        None
    }

    /// less.js `Color.operate` — per-channel, alpha = `a*(1-b)+b`. The result
    /// drops the original literal (an operated color prints as hex/rgba).
    pub fn operate(&self, op: char, other: &Color) -> Color {
        let alpha = self.alpha * (1.0 - other.alpha) + other.alpha;
        let mut rgb = [0.0; 3];
        for c in 0..3 {
            rgb[c] = match op {
                '+' => self.rgb[c] + other.rgb[c],
                '-' => self.rgb[c] - other.rgb[c],
                '*' => self.rgb[c] * other.rgb[c],
                '/' => self.rgb[c] / other.rgb[c],
                _ => self.rgb[c],
            };
        }
        Color {
            rgb,
            alpha,
            original: None,
        }
    }

    /// less.js `toHSL` → `(h in 0..360, s, l, a)`.
    pub fn to_hsl(&self) -> (f64, f64, f64, f64) {
        let r = self.rgb[0] / 255.0;
        let g = self.rgb[1] / 255.0;
        let b = self.rgb[2] / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        let d = max - min;
        let (h, s);
        if max == min {
            h = 0.0;
            s = 0.0;
        } else {
            s = if l > 0.5 {
                d / (2.0 - max - min)
            } else {
                d / (max + min)
            };
            let hh = if max == r {
                (g - b) / d + if g < b { 6.0 } else { 0.0 }
            } else if max == g {
                (b - r) / d + 2.0
            } else {
                (r - g) / d + 4.0
            };
            h = hh / 6.0;
        }
        (h * 360.0, s, l, self.alpha)
    }

    /// less.js `toCSS`, expanded (non-compress) form (plan §2.18, §4).
    pub fn to_css(&self, num_precision: u8) -> String {
        let alpha = fround(self.alpha, num_precision);

        // Decide the output function from the original literal (less.js `value`).
        let mut color_function: Option<&str> = None;
        if let Some(orig) = &self.original {
            if orig.starts_with("rgb") {
                if alpha < 1.0 {
                    color_function = Some("rgba");
                }
            } else if orig.starts_with("hsl") {
                color_function = Some(if alpha < 1.0 { "hsla" } else { "hsl" });
            } else {
                // named color / hex literal — emit verbatim.
                return orig.to_string();
            }
        } else if alpha < 1.0 {
            color_function = Some("rgba");
        }

        match color_function {
            Some("rgba") => {
                let mut parts: Vec<String> = self
                    .rgb
                    .iter()
                    .map(|c| clamp(js_round(*c), 255.0).to_string())
                    .collect();
                parts.push(format_alpha(clamp(alpha, 1.0)));
                format!("rgba({})", parts.join(", "))
            }
            Some("hsl") | Some("hsla") => {
                let (h, s, l, _) = self.to_hsl();
                let mut args = vec![
                    format_alpha(fround(h, num_precision)),
                    format!("{}%", format_alpha(fround(s * 100.0, num_precision))),
                    format!("{}%", format_alpha(fround(l * 100.0, num_precision))),
                ];
                if color_function == Some("hsla") {
                    args.push(format_alpha(clamp(alpha, 1.0)));
                }
                format!("{}({})", color_function.unwrap(), args.join(", "))
            }
            _ => self.to_rgb_hex(),
        }
    }

    /// less.js `toRGB` — the 6-digit lowercase hex form, channels clamped/rounded.
    pub fn to_rgb_hex(&self) -> String {
        let mut s = String::from("#");
        for c in &self.rgb {
            let v = clamp(js_round(*c), 255.0) as u32;
            s.push_str(&format!("{v:02x}"));
        }
        s
    }

    /// less.js `toHSV` → `(h in 0..360, s, v, a)`.
    pub fn to_hsv(&self) -> (f64, f64, f64, f64) {
        let r = self.rgb[0] / 255.0;
        let g = self.rgb[1] / 255.0;
        let b = self.rgb[2] / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let v = max;
        let d = max - min;
        let s = if max == 0.0 { 0.0 } else { d / max };
        let h = if max == min {
            0.0
        } else {
            let hh = if max == r {
                (g - b) / d + if g < b { 6.0 } else { 0.0 }
            } else if max == g {
                (b - r) / d + 2.0
            } else {
                (r - g) / d + 4.0
            };
            hh / 6.0
        };
        (h * 360.0, s, v, self.alpha)
    }

    /// less.js `Color.luma()` — gamma-corrected rec709 relative luminance.
    pub fn luma(&self) -> f64 {
        let f = |c: f64| {
            let c = c / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(self.rgb[0]) + 0.7152 * f(self.rgb[1]) + 0.0722 * f(self.rgb[2])
    }

    /// less.js `toARGB` — `#AARRGGBB`, each channel rounded and clamped.
    pub fn to_argb(&self) -> String {
        let mut s = String::from("#");
        for c in [self.alpha * 255.0, self.rgb[0], self.rgb[1], self.rgb[2]] {
            let v = clamp(js_round(c), 255.0) as u32;
            s.push_str(&format!("{v:02x}"));
        }
        s
    }
}

fn parse_pairs(hex: &str, n: usize) -> Option<[f64; 4]> {
    let mut out = [0.0; 4];
    for i in 0..n {
        let byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        out[i] = byte as f64;
    }
    Some(out)
}

fn parse_singles(hex: &str, n: usize) -> Option<[f64; 4]> {
    let mut out = [0.0; 4];
    let bytes = hex.as_bytes();
    for i in 0..n {
        let c = bytes[i] as char;
        let byte = u8::from_str_radix(&format!("{c}{c}"), 16).ok()?;
        out[i] = byte as f64;
    }
    Some(out)
}

fn clamp(v: f64, max: f64) -> f64 {
    v.max(0.0).min(max)
}

/// JS `Math.round` — half toward +∞.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

fn fround(v: f64, num_precision: u8) -> f64 {
    if num_precision == 0 {
        return v;
    }
    let factor = 10f64.powi(num_precision as i32);
    js_round((v + 2e-16) * factor) / factor
}

/// Format a rounded scalar (alpha / hsl component) the way less.js joins them:
/// integers unadorned, else shortest decimal.
fn format_alpha(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Helper: get the `[f64;3]` at 8-bit integer resolution (for hsl rounding parity
/// where less.js rounds channels at construction time via `parseInt`).
impl Color {
    pub fn is_transparent_keyword(&self) -> bool {
        self.original.as_deref() == Some("transparent")
    }
}
