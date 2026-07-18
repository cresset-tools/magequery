//! The `Color` runtime value (plan §2.18, §9.3).
//!
//! RGBA in `f64` per channel (unclamped until output) plus the *original*
//! literal, kept for round-trip fidelity — incl. `transparent`↔`rgba(…,0)` and
//! named-color preservation (§H3). Color math is per-channel and unclamped until
//! `toCSS`; less.js HSL/HSV/blend intermediate rounding order is reproduced (§3-G).

/// An RGBA color that remembers how it was written (plan §9.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Color {
    /// Red/green/blue, each `0.0..=255.0` at output (unclamped mid-computation).
    pub rgb: [f64; 3],
    /// Alpha, `0.0..=1.0`.
    pub alpha: f64,
    /// The original literal (`#fff`, `red`, `transparent`, …) for round-trip
    /// output, if the color came from source text.
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
}
