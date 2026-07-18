//! Output CSS tree + genCSS (plan §4.7, §9.4).
//!
//! Our own `genCSS` owns **both** the diffable expanded `.css` and the
//! byte-exact `compress` output (the latter gated by the less.js
//! `compress`/`compression` fixtures, §C4) — neither is delegated to
//! lightningcss, which is confined to `.min.css` (plan §9.4).

use crate::error::LessError;

/// A non-fatal problem collected during compile (plan §4.6): `extend '…' has no
/// matches`, optional-import miss, ignored `@plugin`/`@dumpLineNumbers`,
/// deprecations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// The message.
    pub message: String,
    /// Source file, if known.
    pub filename: Option<String>,
    /// 1-based line, if known.
    pub line: Option<usize>,
}

/// The compile result (plan §9.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Css {
    /// The rendered CSS. (§9.5 field name is `code`.)
    pub code: String,
    /// Absolute paths of every file imported during compilation.
    pub imports: Vec<String>,
    /// Source map JSON, when `sourceMap` is enabled (OUT for now — §8).
    pub source_map: Option<String>,
    /// Non-fatal diagnostics.
    pub warnings: Vec<Warning>,
}

impl Css {
    /// A result carrying just rendered code (no imports/warnings).
    pub fn from_code(code: impl Into<String>) -> Self {
        Css {
            code: code.into(),
            ..Css::default()
        }
    }
}

/// Serialize a CSS identifier with proper escaping, via cssparser (plan §9:
/// cssparser is used only for CSS-side serialization helpers, never to tokenize
/// LESS). A thin wrapper so the rest of genCSS stays cssparser-agnostic.
// Not yet called by a non-test genCSS path (the serializer lands in later
// phases); the unit test below already exercises it.
#[allow(dead_code)]
pub(crate) fn serialize_ident(name: &str) -> Result<String, LessError> {
    let mut out = String::with_capacity(name.len());
    cssparser::serialize_identifier(name, &mut out)
        .map_err(|_| LessError::new(crate::error::ErrorKind::Runtime, "identifier serialization failed"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_ident_escapes_leading_digit() {
        // cssparser escapes a leading digit so the identifier round-trips.
        assert_eq!(serialize_ident("1a").unwrap(), "\\31 a");
        assert_eq!(serialize_ident("foo-bar").unwrap(), "foo-bar");
    }
}
