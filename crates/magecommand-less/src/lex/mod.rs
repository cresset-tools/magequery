//! Hand-written tokenizer + scanning cursor + [`LineMap`] (plan §9.1, §H2).
//!
//! `cssparser` cannot tokenize LESS (`//` comments, `;`-separated mixin args,
//! `when` guards, `@{}` interpolation, `~""`, `&:extend`, detached rulesets), so
//! the front-end is hand-written in the `php.rs`/`graphql.rs` tradition.
//!
//! Two layers:
//! - [`tokenize`] turns source into a flat [`Token`] stream (the "tokenizer"
//!   deliverable — useful for tests and coarse scans).
//! - [`Cursor`] is the character-level scanning layer the recursive-descent
//!   [`crate::parser`] actually drives (LESS is deeply context-sensitive — the
//!   same `-` is subtraction, a sign, or part of an identifier depending on
//!   surrounding whitespace, §2.4 — so the parser needs raw cursor control, the
//!   way less.js's own `parser-input.js` works).
//!
//! [`normalize_source`] (BOM strip + CRLF→LF, §H2) and [`LineMap`] (byte offset →
//! 1-based line/col via binary search, for exact error provenance, §5.5) round it
//! out.

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

// ---------------------------------------------------------------------------
// Token stream (the coarse tokenizer deliverable)
// ---------------------------------------------------------------------------

/// A lexical token kind (plan §9.1). Coarse but faithful to LESS's surface: it
/// distinguishes the constructs `cssparser` cannot (`//` line comments, `@{}`
/// interpolation, `~"…"` escaped strings, at-keywords).
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// An identifier / keyword run (`color`, `-webkit-transform`, `nth-child`).
    Ident,
    /// An at-keyword (`@media`) or a bare variable ref (`@color`). The `@` is
    /// included in the slice.
    AtKeyword,
    /// A `@{name}` interpolation.
    InterpVar,
    /// A `#hash` (id selector or hex color).
    Hash,
    /// A numeric literal with an optional unit (`12px`, `.5`, `-3`, `50%`).
    Number,
    /// A quoted string (`"…"` / `'…'`), optionally `~`-escaped.
    Str,
    /// A `/* … */` block comment.
    BlockComment,
    /// A `// …` line comment (runs to EOL).
    LineComment,
    /// Runs of whitespace.
    Whitespace,
    /// A single punctuation/delimiter char (`{ } ( ) [ ] ; : , > + ~ * / . & = | …`).
    Delim,
}

/// One token: its kind and byte span into the (normalized) source.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    /// The token's source slice.
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

/// Tokenize `src` (which should already be [`normalize_source`]d) into a flat
/// token stream. This is the coarse "tokenizer" surface; the parser drives a
/// [`Cursor`] instead, but the two share the same scanning primitives.
pub fn tokenize(src: &str) -> Vec<Token> {
    let mut cur = Cursor::new(src);
    let mut out = Vec::new();
    while let Some(tok) = cur.next_token() {
        out.push(tok);
    }
    out
}

// ---------------------------------------------------------------------------
// Cursor — the scanning layer the parser drives
// ---------------------------------------------------------------------------

/// A character-level scanning cursor over the source bytes (plan §9.1). Mirrors
/// less.js's `parser-input.js`: the parser peeks/consumes chars and scans runs
/// (idents, numbers, strings, comments) directly, because LESS tokenization is
/// context-sensitive.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    src: &'a str,
    bytes: &'a [u8],
    /// Current byte offset.
    pub i: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(src: &'a str) -> Self {
        Cursor {
            src,
            bytes: src.as_bytes(),
            i: 0,
        }
    }

    /// The full source.
    pub fn src(&self) -> &'a str {
        self.src
    }

    /// The byte at the current position, or `None` at EOF.
    #[inline]
    pub fn cur(&self) -> Option<u8> {
        self.bytes.get(self.i).copied()
    }

    /// The byte `n` positions ahead of the cursor.
    #[inline]
    pub fn peek(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.i + n).copied()
    }

    /// True at end of input.
    #[inline]
    pub fn eof(&self) -> bool {
        self.i >= self.bytes.len()
    }

    /// Advance one byte.
    #[inline]
    pub fn bump(&mut self) {
        if self.i < self.bytes.len() {
            self.i += 1;
        }
    }

    /// Consume a specific ASCII char, returning true if matched.
    pub fn eat(&mut self, c: u8) -> bool {
        if self.cur() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// Consume an exact ASCII string prefix, returning true if matched.
    pub fn eat_str(&mut self, s: &str) -> bool {
        if self.src[self.i..].starts_with(s) {
            self.i += s.len();
            true
        } else {
            false
        }
    }

    /// The remaining source from the cursor.
    pub fn rest(&self) -> &'a str {
        &self.src[self.i..]
    }

    /// Skip a run of whitespace; return whether any was skipped.
    pub fn skip_whitespace(&mut self) -> bool {
        let start = self.i;
        while let Some(b) = self.cur() {
            if b.is_ascii_whitespace() {
                self.i += 1;
            } else {
                break;
            }
        }
        self.i != start
    }

    /// Skip whitespace **and** comments (both kinds). Returns whether the cursor
    /// moved. The parser uses this between tokens where comments are insignificant.
    pub fn skip_trivia(&mut self) -> bool {
        let start = self.i;
        loop {
            self.skip_whitespace();
            if self.at_line_comment() || self.at_block_comment() {
                self.scan_comment();
            } else {
                break;
            }
        }
        self.i != start
    }

    /// True if the previous byte (before the cursor) is whitespace.
    pub fn prev_is_whitespace(&self) -> bool {
        self.i > 0 && self.bytes[self.i - 1].is_ascii_whitespace()
    }

    /// True if the cursor sits at the start of a `//` line comment.
    pub fn at_line_comment(&self) -> bool {
        self.cur() == Some(b'/') && self.peek(1) == Some(b'/')
    }

    /// True if the cursor sits at the start of a `/* */` block comment.
    pub fn at_block_comment(&self) -> bool {
        self.cur() == Some(b'/') && self.peek(1) == Some(b'*')
    }

    /// Scan a comment (either kind) from the cursor; returns `(text, is_line)`.
    /// Assumes the cursor is at `//` or `/*`.
    pub fn scan_comment(&mut self) -> (&'a str, bool) {
        let start = self.i;
        if self.at_line_comment() {
            self.i += 2;
            while let Some(b) = self.cur() {
                if b == b'\n' {
                    break;
                }
                self.i += 1;
            }
            (&self.src[start..self.i], true)
        } else {
            // block comment
            self.i += 2;
            while self.i < self.bytes.len() {
                if self.bytes[self.i] == b'*' && self.peek(1) == Some(b'/') {
                    self.i += 2;
                    break;
                }
                self.i += 1;
            }
            (&self.src[start..self.i], false)
        }
    }

    /// Scan a quoted string starting at `"` or `'` (respecting `\` escapes).
    /// Returns the slice **including** the quotes. Assumes cursor at the quote.
    pub fn scan_string(&mut self) -> &'a str {
        let start = self.i;
        let quote = self.bytes[self.i];
        self.i += 1;
        while let Some(b) = self.cur() {
            if b == b'\\' {
                self.i += 2;
                continue;
            }
            self.i += 1;
            if b == quote {
                break;
            }
        }
        &self.src[start..self.i]
    }

    /// Scan an identifier run: `[A-Za-z0-9_-]`, escapes `\.`, and non-ASCII.
    /// Returns the slice (possibly empty).
    pub fn scan_ident(&mut self) -> &'a str {
        let start = self.i;
        while let Some(b) = self.cur() {
            match b {
                b'\\' => {
                    self.i += 2;
                }
                b'-' | b'_' => self.i += 1,
                _ if b.is_ascii_alphanumeric() => self.i += 1,
                _ if b >= 0x80 => self.i += 1, // non-ASCII (UTF-8 continuation-safe: only advances into a full char below)
                _ => break,
            }
        }
        // If we advanced into the middle of a multibyte char, extend to a char boundary.
        while self.i < self.bytes.len() && !self.src.is_char_boundary(self.i) {
            self.i += 1;
        }
        &self.src[start..self.i]
    }

    /// Scan a numeric literal `[+-]?(\d+\.\d+|\.\d+|\d+)` plus an optional unit
    /// (`px`, `%`, `deg`). Returns `(number_slice, unit_slice)`. Assumes the
    /// cursor is positioned so a number is present.
    pub fn scan_number(&mut self) -> (&'a str, &'a str) {
        let start = self.i;
        if matches!(self.cur(), Some(b'+') | Some(b'-')) {
            self.i += 1;
        }
        while matches!(self.cur(), Some(b) if b.is_ascii_digit()) {
            self.i += 1;
        }
        if self.cur() == Some(b'.') && matches!(self.peek(1), Some(b) if b.is_ascii_digit()) {
            self.i += 1;
            while matches!(self.cur(), Some(b) if b.is_ascii_digit()) {
                self.i += 1;
            }
        }
        // Scientific notation (e.g. 1e-10).
        if matches!(self.cur(), Some(b'e') | Some(b'E'))
            && matches!(self.peek(1), Some(b) if b.is_ascii_digit() || b == b'+' || b == b'-')
        {
            self.i += 1;
            if matches!(self.cur(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            while matches!(self.cur(), Some(b) if b.is_ascii_digit()) {
                self.i += 1;
            }
        }
        let num = &self.src[start..self.i];
        let unit_start = self.i;
        if self.cur() == Some(b'%') {
            self.i += 1;
        } else {
            while matches!(self.cur(), Some(b) if b.is_ascii_alphabetic()) {
                self.i += 1;
            }
        }
        (num, &self.src[unit_start..self.i])
    }

    /// True if a numeric literal begins at the cursor.
    pub fn at_number(&self) -> bool {
        match self.cur() {
            Some(b) if b.is_ascii_digit() => true,
            Some(b'.') => matches!(self.peek(1), Some(d) if d.is_ascii_digit()),
            Some(b'+') | Some(b'-') => {
                let n = self.peek(1);
                matches!(n, Some(d) if d.is_ascii_digit())
                    || (n == Some(b'.') && matches!(self.peek(2), Some(d) if d.is_ascii_digit()))
            }
            _ => false,
        }
    }

    /// Produce the next coarse [`Token`] (used by [`tokenize`]).
    pub fn next_token(&mut self) -> Option<Token> {
        if self.eof() {
            return None;
        }
        let start = self.i;
        let b = self.bytes[self.i];
        let kind = if b.is_ascii_whitespace() {
            self.skip_whitespace();
            TokenKind::Whitespace
        } else if self.at_line_comment() {
            self.scan_comment();
            TokenKind::LineComment
        } else if self.at_block_comment() {
            self.scan_comment();
            TokenKind::BlockComment
        } else if b == b'"' || b == b'\'' {
            self.scan_string();
            TokenKind::Str
        } else if b == b'~' && matches!(self.peek(1), Some(b'"') | Some(b'\'')) {
            self.i += 1;
            self.scan_string();
            TokenKind::Str
        } else if b == b'@' && self.peek(1) == Some(b'{') {
            self.i += 2;
            while let Some(c) = self.cur() {
                self.i += 1;
                if c == b'}' {
                    break;
                }
            }
            TokenKind::InterpVar
        } else if b == b'@' {
            self.i += 1;
            self.scan_ident();
            TokenKind::AtKeyword
        } else if b == b'#' {
            self.i += 1;
            self.scan_ident();
            TokenKind::Hash
        } else if self.at_number() {
            self.scan_number();
            TokenKind::Number
        } else if b == b'-' || b == b'_' || b.is_ascii_alphabetic() || b >= 0x80 {
            self.scan_ident();
            if self.i == start {
                self.i += 1; // never stall
                TokenKind::Delim
            } else {
                TokenKind::Ident
            }
        } else {
            self.i += 1;
            TokenKind::Delim
        };
        Some(Token {
            kind,
            start,
            end: self.i,
        })
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

    #[test]
    fn tokenize_classifies_the_less_surface() {
        let toks = tokenize("a { color: #fff; } // trailing\n@x: 2px;");
        let kinds: Vec<_> = toks
            .iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| t.kind.clone())
            .collect();
        assert_eq!(kinds[0], TokenKind::Ident); // a
        assert_eq!(kinds[1], TokenKind::Delim); // {
        assert!(kinds.contains(&TokenKind::Hash)); // #fff
        assert!(kinds.contains(&TokenKind::LineComment));
        assert!(kinds.contains(&TokenKind::AtKeyword)); // @x
        assert!(kinds.contains(&TokenKind::Number)); // 2px
    }

    #[test]
    fn scan_number_splits_unit() {
        let mut c = Cursor::new("12.5px rest");
        let (n, u) = c.scan_number();
        assert_eq!(n, "12.5");
        assert_eq!(u, "px");
    }

    #[test]
    fn scan_string_handles_escapes() {
        let mut c = Cursor::new(r#""a\"b" x"#);
        assert_eq!(c.scan_string(), r#""a\"b""#);
    }
}
