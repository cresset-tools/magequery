//! The structural scanner: a byte cursor that skips what the parser never
//! reads — comments, strings (with interpolation), heredocs, attributes,
//! `?>` HTML gaps, and whole `{…}` bodies — without losing its place.
//!
//! Bodies are ~90% of a PHP file's bytes; skipping them correctly at speed is
//! this module's whole job. `memchr` does the byte-level sprinting inside
//! comments/strings; a byte-class table drives the code-mode loop.

use memchr::{memchr, memchr2, memchr3};

/// Bytes that code-mode scanning must stop at. Everything else is skipped in
/// runs.
const fn special_table() -> [bool; 256] {
    let mut t = [false; 256];
    t[b'{' as usize] = true;
    t[b'}' as usize] = true;
    t[b'\'' as usize] = true;
    t[b'"' as usize] = true;
    t[b'`' as usize] = true;
    t[b'/' as usize] = true;
    t[b'#' as usize] = true;
    t[b'<' as usize] = true;
    t[b'?' as usize] = true;
    t
}
const SPECIAL: [bool; 256] = special_table();

pub(crate) struct Cursor<'a> {
    pub src: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        Cursor { src, pos: 0 }
    }

    pub fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    pub fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    pub fn starts_with(&self, needle: &[u8]) -> bool {
        self.src[self.pos..].starts_with(needle)
    }

    /// Case-insensitive `starts_with` for PHP's case-insensitive keywords.
    pub fn starts_with_ci(&self, needle: &[u8]) -> bool {
        let end = self.pos + needle.len();
        end <= self.src.len() && self.src[self.pos..end].eq_ignore_ascii_case(needle)
    }

    pub fn bump(&mut self) {
        self.pos += 1;
    }

    // ---- identifiers -----------------------------------------------------

    pub fn at_ident_start(&self) -> bool {
        matches!(self.peek(), Some(b) if b == b'_' || b.is_ascii_alphabetic() || b >= 0x80)
    }

    fn is_ident_byte(b: u8) -> bool {
        b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80
    }

    /// Read an identifier (PHP: `[a-zA-Z_\x80-\xff][a-zA-Z0-9_\x80-\xff]*`).
    /// Returns `None` if not at one.
    pub fn read_ident(&mut self) -> Option<&'a str> {
        if !self.at_ident_start() {
            return None;
        }
        let start = self.pos;
        while matches!(self.peek(), Some(b) if Self::is_ident_byte(b)) {
            self.bump();
        }
        // PHP identifiers beyond ASCII are legal but effectively unused in
        // the Magento ecosystem; lossy conversion keeps the API String-based.
        Some(std::str::from_utf8(&self.src[start..self.pos]).unwrap_or(""))
    }

    /// Consume `word` if the next token is exactly that keyword
    /// (case-insensitive, not a prefix of a longer identifier).
    pub fn eat_keyword(&mut self, word: &str) -> bool {
        if !self.starts_with_ci(word.as_bytes()) {
            return false;
        }
        let end = self.pos + word.len();
        if matches!(self.src.get(end), Some(&b) if Self::is_ident_byte(b)) {
            return false;
        }
        self.pos = end;
        true
    }

    // ---- insignificant bytes ----------------------------------------------

    /// Skip whitespace, comments, and attributes are NOT skipped here — the
    /// parser decides what to do at `#[`. A bare `#` (not `#[`) is a line
    /// comment.
    pub fn skip_insignificant(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => self.bump(),
                Some(b'/') if self.peek_at(1) == Some(b'/') => self.skip_line_comment(),
                Some(b'/') if self.peek_at(1) == Some(b'*') => self.skip_block_comment(),
                Some(b'#') if self.peek_at(1) != Some(b'[') => self.skip_line_comment(),
                _ => return,
            }
        }
    }

    pub fn skip_line_comment(&mut self) {
        match memchr(b'\n', &self.src[self.pos..]) {
            Some(i) => self.pos += i + 1,
            None => self.pos = self.src.len(),
        }
    }

    pub fn skip_block_comment(&mut self) {
        self.pos += 2; // "/*"
        let mut at = self.pos;
        while let Some(i) = memchr(b'*', &self.src[at..]) {
            let star = at + i;
            if self.src.get(star + 1) == Some(&b'/') {
                self.pos = star + 2;
                return;
            }
            at = star + 1;
        }
        self.pos = self.src.len(); // unterminated: consume the rest
    }

    // ---- strings -----------------------------------------------------------

    /// At the opening `'`. Skips past the closing quote.
    pub fn skip_single_quoted(&mut self) {
        self.bump();
        loop {
            match memchr2(b'\'', b'\\', &self.src[self.pos..]) {
                Some(i) => {
                    let hit = self.pos + i;
                    if self.src[hit] == b'\\' {
                        self.pos = (hit + 2).min(self.src.len());
                    } else {
                        self.pos = hit + 1;
                        return;
                    }
                }
                None => {
                    self.pos = self.src.len();
                    return;
                }
            }
        }
    }

    /// At the opening `"` (or backtick, pass `quote`). Skips past the close,
    /// entering `{$…}` / `${…}` complex interpolation recursively — the trap:
    /// `"{$a["x"]}"` contains a nested double-quoted string, so the naive
    /// "next unescaped quote" would end the outer string early.
    pub fn skip_double_quoted(&mut self, quote: u8) {
        self.bump();
        loop {
            match memchr3(quote, b'\\', b'{', &self.src[self.pos..]) {
                Some(i) => {
                    let hit = self.pos + i;
                    match self.src[hit] {
                        b'\\' => self.pos = (hit + 2).min(self.src.len()),
                        b'{' => {
                            self.pos = hit + 1;
                            if self.src.get(hit + 1) == Some(&b'$') {
                                // `{$expr}` — a full expression island.
                                self.skip_code_until_close_brace();
                            }
                            // A lone `{` is literal text; `${` arrives as
                            // `$` (not our needle) then `{` — the code
                            // island check above covers `{$`; the legacy
                            // `${name}` form contains no nested quotes.
                        }
                        _ => {
                            self.pos = hit + 1;
                            return;
                        }
                    }
                }
                None => {
                    self.pos = self.src.len();
                    return;
                }
            }
        }
    }

    /// At `<<<`. Skips a heredoc/nowdoc to past its terminator line.
    pub fn skip_heredoc(&mut self) {
        self.pos += 3;
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.bump();
        }
        let quote = match self.peek() {
            Some(q @ (b'\'' | b'"')) => {
                self.bump();
                Some(q)
            }
            _ => None,
        };
        let label_start = self.pos;
        while matches!(self.peek(), Some(b) if Self::is_ident_byte(b)) {
            self.bump();
        }
        let label: Vec<u8> = self.src[label_start..self.pos].to_vec();
        if quote.is_some() {
            self.bump(); // closing quote of the label
        }
        if label.is_empty() {
            return; // not actually a heredoc; let the caller carry on
        }
        // Scan line starts for `[ \t]* LABEL` followed by a non-ident byte
        // (7.3+ allows indentation and a trailing `;`/`,`/`)`).
        loop {
            match memchr(b'\n', &self.src[self.pos..]) {
                Some(i) => self.pos += i + 1,
                None => {
                    self.pos = self.src.len();
                    return;
                }
            }
            let mut at = self.pos;
            while matches!(self.src.get(at), Some(b' ' | b'\t')) {
                at += 1;
            }
            if self.src[at..].starts_with(&label)
                && !matches!(self.src.get(at + label.len()), Some(&b) if Self::is_ident_byte(b))
            {
                self.pos = at + label.len();
                return;
            }
        }
    }

    // ---- attributes ---------------------------------------------------------

    /// At `#[`. Skips the whole attribute group, string- and nesting-aware
    /// (`#[Foo("]", [1, 2])]`).
    pub fn skip_attribute(&mut self) {
        self.pos += 2;
        let mut depth = 1usize;
        while let Some(b) = self.peek() {
            match b {
                b'[' => {
                    depth += 1;
                    self.bump();
                }
                b']' => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                b'\'' => self.skip_single_quoted(),
                b'"' => self.skip_double_quoted(b'"'),
                b'/' if self.peek_at(1) == Some(b'/') => self.skip_line_comment(),
                b'/' if self.peek_at(1) == Some(b'*') => self.skip_block_comment(),
                _ => self.bump(),
            }
        }
    }

    // ---- code-mode skipping --------------------------------------------------

    /// Positioned just after an opening `{`: skip to just after its matching
    /// `}`. This is THE body skipper — string/comment/heredoc/HTML-aware.
    pub fn skip_code_until_close_brace(&mut self) {
        let mut depth = 1usize;
        while self.pos < self.src.len() {
            // Sprint over uninteresting bytes.
            while self.pos < self.src.len() && !SPECIAL[self.src[self.pos] as usize] {
                self.pos += 1;
            }
            let Some(b) = self.peek() else { return };
            match b {
                b'{' => {
                    depth += 1;
                    self.bump();
                }
                b'}' => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                b'\'' => self.skip_single_quoted(),
                b'"' => self.skip_double_quoted(b'"'),
                b'`' => self.skip_double_quoted(b'`'),
                b'/' => {
                    if self.peek_at(1) == Some(b'/') {
                        self.skip_line_comment();
                    } else if self.peek_at(1) == Some(b'*') {
                        self.skip_block_comment();
                    } else {
                        self.bump();
                    }
                }
                b'#' => {
                    if self.peek_at(1) == Some(b'[') {
                        self.skip_attribute();
                    } else {
                        self.skip_line_comment();
                    }
                }
                b'<' => {
                    if self.starts_with(b"<<<") {
                        self.skip_heredoc();
                    } else {
                        self.bump();
                    }
                }
                b'?' => {
                    if self.peek_at(1) == Some(b'>') {
                        // `?>` inside a function body: raw HTML until the
                        // next open tag. Braces in HTML don't count.
                        self.pos += 2;
                        self.skip_html_until_open_tag();
                    } else {
                        self.bump();
                    }
                }
                _ => self.bump(),
            }
        }
    }

    /// In HTML mode (before `<?php` at file start, or after `?>`): skip to
    /// just after the next PHP open tag.
    pub fn skip_html_until_open_tag(&mut self) {
        let mut at = self.pos;
        while let Some(i) = memchr(b'<', &self.src[at..]) {
            let lt = at + i;
            if self.src.get(lt + 1) == Some(&b'?') {
                // `<?php`, `<?=`, or short `<?`.
                if self.src[lt + 2..].len() >= 3 && self.src[lt + 2..lt + 5].eq_ignore_ascii_case(b"php") {
                    self.pos = lt + 5;
                } else if self.src.get(lt + 2) == Some(&b'=') {
                    self.pos = lt + 3;
                } else {
                    self.pos = lt + 2;
                }
                return;
            }
            at = lt + 1;
        }
        self.pos = self.src.len();
    }

    /// Skip one top-level statement we don't care about (registration.php
    /// calls, `declare`, guard `if` blocks, …): consume until a `;` at
    /// depth 0, or a balanced `{…}` block ends at depth 0 (whichever the
    /// statement's shape provides).
    pub fn skip_statement(&mut self) {
        loop {
            self.skip_insignificant();
            match self.peek() {
                None => return,
                Some(b';') => {
                    self.bump();
                    return;
                }
                Some(b'{') => {
                    self.bump();
                    self.skip_code_until_close_brace();
                    // `if (…) { … } else { … }` — keep going only for
                    // else/elseif chains; otherwise the block ends it.
                    self.skip_insignificant();
                    if self.starts_with_ci(b"else") || self.starts_with_ci(b"elseif") {
                        continue;
                    }
                    return;
                }
                Some(b'}') => return, // let the caller see the closer
                Some(b'(') => {
                    self.bump();
                    self.skip_parens_body(1);
                }
                Some(b'\'') => self.skip_single_quoted(),
                Some(b'"') => self.skip_double_quoted(b'"'),
                Some(b'`') => self.skip_double_quoted(b'`'),
                Some(b'<') if self.starts_with(b"<<<") => self.skip_heredoc(),
                Some(b'#') if self.peek_at(1) == Some(b'[') => self.skip_attribute(),
                Some(b'?') if self.peek_at(1) == Some(b'>') => {
                    self.pos += 2;
                    self.skip_html_until_open_tag();
                    return;
                }
                _ => self.bump(),
            }
        }
    }

    /// Inside `(`…`)` at the given depth: skip to just past the matching
    /// close. String/comment/nesting-aware.
    pub fn skip_parens_body(&mut self, mut depth: usize) {
        while let Some(b) = self.peek() {
            match b {
                b'(' => {
                    depth += 1;
                    self.bump();
                }
                b')' => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                b'{' => {
                    self.bump();
                    self.skip_code_until_close_brace();
                }
                b'\'' => self.skip_single_quoted(),
                b'"' => self.skip_double_quoted(b'"'),
                b'`' => self.skip_double_quoted(b'`'),
                b'<' if self.starts_with(b"<<<") => self.skip_heredoc(),
                b'/' if self.peek_at(1) == Some(b'/') => self.skip_line_comment(),
                b'/' if self.peek_at(1) == Some(b'*') => self.skip_block_comment(),
                b'#' if self.peek_at(1) == Some(b'[') => self.skip_attribute(),
                b'#' => self.skip_line_comment(),
                _ => self.bump(),
            }
        }
    }

    /// Capture raw expression text (a default value, a const value) until one
    /// of `stops` appears at nesting depth 0. Strings are copied verbatim,
    /// comments dropped, whitespace runs collapsed to one space. The cursor
    /// ends ON the stop byte.
    pub fn capture_expr_until(&mut self, stops: &[u8]) -> String {
        let mut out = Vec::new();
        let mut depth = 0usize;
        while let Some(b) = self.peek() {
            if depth == 0 && stops.contains(&b) {
                break;
            }
            match b {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    out.push(b);
                    self.bump();
                }
                b')' | b']' | b'}' => {
                    if depth == 0 {
                        break; // unbalanced closer belongs to the caller
                    }
                    depth -= 1;
                    out.push(b);
                    self.bump();
                }
                b'\'' | b'"' | b'`' => {
                    let start = self.pos;
                    if b == b'\'' {
                        self.skip_single_quoted();
                    } else {
                        self.skip_double_quoted(b);
                    }
                    out.extend_from_slice(&self.src[start..self.pos]);
                }
                b'<' if self.starts_with(b"<<<") => {
                    let start = self.pos;
                    self.skip_heredoc();
                    out.extend_from_slice(&self.src[start..self.pos]);
                }
                b'/' if self.peek_at(1) == Some(b'/') => self.skip_line_comment(),
                b'/' if self.peek_at(1) == Some(b'*') => self.skip_block_comment(),
                b'#' if self.peek_at(1) != Some(b'[') => self.skip_line_comment(),
                _ if b.is_ascii_whitespace() => {
                    if out.last().is_some_and(|&l| l != b' ') {
                        out.push(b' ');
                    }
                    self.bump();
                }
                _ => {
                    out.push(b);
                    self.bump();
                }
            }
        }
        let mut s = String::from_utf8_lossy(&out).into_owned();
        while s.ends_with(' ') {
            s.pop();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip_body(src: &str) -> usize {
        // Position just after the first `{`.
        let open = src.find('{').unwrap();
        let mut c = Cursor::new(src.as_bytes());
        c.pos = open + 1;
        c.skip_code_until_close_brace();
        c.pos
    }

    #[test]
    fn plain_braces_balance() {
        let src = "{ if (x) { y(); } } tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn braces_in_strings_dont_count() {
        let src = r#"{ $a = "}"; $b = '}'; } tail"#;
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn interpolated_string_with_nested_quotes() {
        // The classic trap: the inner "x" must not close the outer string.
        let src = r#"{ $s = "{$a["x"]}"; } tail"#;
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn interpolation_with_brace_in_nested_string() {
        let src = r#"{ $s = "{$a["}"]}"; } tail"#;
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn escaped_quotes() {
        let src = r#"{ $s = "a\"}b"; $t = 'c\'}d'; } tail"#;
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn heredoc_swallows_braces() {
        let src = "{ $s = <<<EOT\n } fake\nEOT;\n} tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn nowdoc_indented_terminator() {
        let src = "{ $s = <<<'EOT'\n}}}\n    EOT;\n} tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn heredoc_terminator_must_be_word_boundary() {
        // `EOTX` on a line must not terminate an `EOT` heredoc.
        let src = "{ $s = <<<EOT\nEOTX\nEOT;\n} tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn comments_hide_braces() {
        let src = "{ // }\n /* } */ # }\n} tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn attribute_with_bracket_string() {
        let src = "{ $f = #[Foo(\"]\", [1,2])] fn() => 1; } tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn html_gap_inside_body() {
        let src = "{ ?> } not php <?php } tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn backtick_string() {
        let src = "{ $out = `ls }`; } tail";
        assert_eq!(&src[skip_body(src)..], " tail");
    }

    #[test]
    fn capture_expr_collapses_whitespace_and_keeps_strings() {
        let mut c = Cursor::new(b"[1,\n   2, 'a,b'] , rest");
        let expr = c.capture_expr_until(&[b',', b')']);
        assert_eq!(expr, "[1, 2, 'a,b']");
        assert_eq!(c.peek(), Some(b','));
    }

    #[test]
    fn capture_expr_stops_at_top_level_only() {
        let mut c = Cursor::new(b"foo(1, 2), rest");
        let expr = c.capture_expr_until(&[b',']);
        assert_eq!(expr, "foo(1, 2)");
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let mut c = Cursor::new(b"CLASS Foo");
        assert!(c.eat_keyword("class"));
        let mut c2 = Cursor::new(b"classes");
        assert!(!c2.eat_keyword("class"));
    }
}
