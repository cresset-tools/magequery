//! lightningcss bridge — `.min.css` ONLY (plan §9.4/§9.7).
//!
//! Confined to a build artifact that is **never part of any diff gate**;
//! our own `genCSS` owns both the diffable expanded `.css` and the byte-exact
//! `compress` output. Gated behind `feature = "minify"` so the default build
//! (and the acceptance gates) never pull in lightningcss.

// Reference the optional dependency under this feature (kept out of the default
// graph). The real `StyleSheet::minify`/`to_css` bridge is wired engine-side.
extern crate lightningcss;

use crate::error::{ErrorKind, LessError};

/// Minify already-compiled CSS to `.min.css` via lightningcss. Intentionally
/// diverges from Magento's `tubalmartin/cssmin` (functional-fast, not byte-parity).
pub fn to_min_css(css: &str) -> Result<String, LessError> {
    let _ = css;
    Err(LessError::new(
        ErrorKind::Runtime,
        "minify::to_min_css not wired yet (scaffold) — lightningcss bridge lands in the engine step",
    ))
}
