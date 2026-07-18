//! Hand-written tokenizer + [`LineMap`] (plan §9.1, §H2).
//!
//! `cssparser` cannot tokenize LESS (`//` comments, `;`-separated mixin args,
//! `when` guards, `@{}` interpolation, `~""`, `&:extend`, detached rulesets), so
//! the front-end is hand-written in the `php.rs`/`graphql.rs` tradition.
//!
//! Two pieces are real already because everything downstream depends on them:
//! [`normalize_source`] (BOM strip + CRLF→LF, §H2) and [`LineMap`] (byte offset →
//! 1-based line/col via binary search, for exact error provenance, plan §5.5).
//! The token stream itself is a stub until Phase 1.

use std::borrow::Cow;

/// Normalize *source* encoding before tokenizing (plan §H2, §5.5):
/// strip a leading UTF-8 BOM and normalize CRLF → LF. This is independent of the
/// `\r\n`→`\n` normalization applied to *expected* fixture files by the harness.
pub fn normalize_source(src: &str) -> Cow<'_, str> {
    let stripped = src.strip_prefix('\u{feff}').unwrap_or(src);
    if stripped.contains('\r') {
        Cow::Owned(stripped.replace("\r\n", "\n").replace('\r', "\n"))
    } else if stripped.len() != src.len() {
        Cow::Borrowed(stripped)
    } else {
        Cow::Borrowed(src)
    }
}

/// Byte offset → 1-based line/column, via binary search over newline offsets
/// (magequery's `LineMap` pattern). Built once per source; queried lazily.
#[derive(Debug, Clone)]
pub struct LineMap {
    /// Byte offsets of each `\n` in the (already normalized) source.
    newlines: Vec<usize>,
    /// Total source length in bytes (to clamp out-of-range queries).
    len: usize,
}

impl LineMap {
    /// Build a line map over `src` (which should already be [`normalize_source`]d).
    pub fn new(src: &str) -> Self {
        let newlines = src
            .bytes()
            .enumerate()
            .filter_map(|(i, b)| (b == b'\n').then_some(i))
            .collect();
        LineMap {
            newlines,
            len: src.len(),
        }
    }

    /// The 1-based `(line, column)` for a byte `offset`. Column counts bytes from
    /// the line start (1-based), matching less.js's column arithmetic; multibyte
    /// column refinement, if ever needed, is a later concern.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let offset = offset.min(self.len);
        // Number of newlines strictly before `offset` = 0-based line index.
        let line_idx = self.newlines.partition_point(|&nl| nl < offset);
        let line_start = if line_idx == 0 {
            0
        } else {
            self.newlines[line_idx - 1] + 1
        };
        (line_idx + 1, offset - line_start + 1)
    }

    /// Total number of lines.
    pub fn line_count(&self) -> usize {
        self.newlines.len() + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_bom_and_crlf() {
        assert_eq!(normalize_source("\u{feff}a\r\nb\rc"), "a\nb\nc");
        // No-op fast path preserves the borrow.
        assert!(matches!(normalize_source("clean\n"), Cow::Borrowed(_)));
    }

    #[test]
    fn line_col_maps_offsets() {
        let src = "a;\nbb;\nccc";
        let lm = LineMap::new(src);
        assert_eq!(lm.line_col(0), (1, 1)); // 'a'
        assert_eq!(lm.line_col(3), (2, 1)); // first 'b'
        assert_eq!(lm.line_col(4), (2, 2)); // second 'b'
        assert_eq!(lm.line_col(7), (3, 1)); // first 'c'
        assert_eq!(lm.line_count(), 3);
    }
}
