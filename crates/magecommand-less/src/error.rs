//! Error model (plan §4.6, §5.5, §F3).
//!
//! magequery's split: [`LessError`] halts a file (returns `Err`); non-fatal
//! problems become [`crate::css::Warning`]s collected on [`crate::css::Css`].
//!
//! The full less.js error-kind set is 7 and all are required up front (§F3).
//! `Display` reproduces `LessError.prototype.toString()` (less.js
//! `less-error.js`) **byte-exactly** — it is the surface the 74 `tests-error`
//! fixtures gate:
//!
//! ```text
//! <Type>Error: <message> in <file> on line <L>, column <C>:
//! <L-1> <source line before>
//! <L> <offending line>
//! <L+1> <source line after>
//! ```
//!
//! The exact rules, ported from the source (not guessed):
//! - the label is `${type}Error` — `Syntax` renders `SyntaxError:`; the crate's
//!   `Import` kind carries less.js's `'File'` type (file-manager errors), so it
//!   renders `FileError:`;
//! - ` in <file>` appears iff the error has a filename; ` on line L, column C:`
//!   iff it also has a resolved line (less.js prints its 0-based column `+ 1`;
//!   [`LessError::column`] is stored 1-based and printed as-is);
//! - the excerpt is `lines[line-2]` / `lines[line-1]` / `lines[line]` of the
//!   file's `split('\n')` — a missing neighbour (line 1 / EOF) is *skipped*,
//!   but an EMPTY string line still renders as `N ` with a trailing space (a
//!   file ending in `\n` splits into a final `""` element);
//! - the gutter is the plain line number + one space — less.js does **not**
//!   right-align it (`9 …` / `10 …`);
//! - each rendered excerpt row ends with `\n` (the whole string ends `…\n`);
//! - an error with NO filename renders `<Kind>Error: <message>\n\n` — two
//!   trailing newlines: less.js's `this.line` is `undefined` (not `null`) when
//!   no file map was attached, so `line !== null` still enters the excerpt
//!   branch and emits the bare `'\n'` terminator (verified against the
//!   `namespace-*-not-found` goldens).

use std::fmt;

use crate::lex::LineMap;

/// The 7 less.js error kinds (plan §F3). All required up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Parse,
    Name,
    Operation,
    Syntax,
    Import,
    Runtime,
    Argument,
}

impl ErrorKind {
    /// less.js's `e.type` string for this kind.
    ///
    /// `Import` maps to `"File"` — the type less.js's file managers attach to
    /// unresolvable `@import`s (`less-node/file-manager.js`), and the only
    /// import-specific type the error corpus exercises.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Parse => "Parse",
            ErrorKind::Name => "Name",
            ErrorKind::Operation => "Operation",
            ErrorKind::Syntax => "Syntax",
            ErrorKind::Import => "File",
            ErrorKind::Runtime => "Runtime",
            ErrorKind::Argument => "Argument",
        }
    }

    /// The rendered label: `${type}Error` (less.js `toString()`).
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::Parse => "ParseError",
            ErrorKind::Name => "NameError",
            ErrorKind::Operation => "OperationError",
            ErrorKind::Syntax => "SyntaxError",
            ErrorKind::Import => "FileError",
            ErrorKind::Runtime => "RuntimeError",
            ErrorKind::Argument => "ArgumentError",
        }
    }
}

/// A fatal compile error with byte-exact `Display` (plan §5.5).
///
/// Construction is two-phase, mirroring less.js: error *sites* attach
/// `(kind, message, filename, index)`; the location fields (`line`, `column`,
/// `extract`) are filled by [`LessError::locate`] wherever the file's source
/// text is at hand (the parser at construction; the evaluator via its
/// file-scope stack). An error that is never located renders without the
/// ` on line …` tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessError {
    /// Which of the 7 kinds.
    pub kind: ErrorKind,
    /// Human-readable message (the part after `<Kind>Error: `).
    pub message: String,
    /// Source file the error is in (absent for location-less errors).
    pub filename: Option<String>,
    /// Byte offset into the (normalized) source of `filename` — the raw
    /// position error sites record; resolved by [`LessError::locate`].
    pub index: Option<usize>,
    /// 1-based line (set by [`LessError::locate`]).
    pub line: Option<usize>,
    /// 1-based column as displayed (= less.js's 0-based column + 1).
    pub column: Option<usize>,
    /// The 3-line source excerpt: `lines[line-2]`, `lines[line-1]`,
    /// `lines[line]` of the source `split('\n')`. `None` = out of range
    /// (skipped in the rendering); `Some("")` = a real empty line (rendered
    /// as `N ` with a trailing space).
    pub extract: [Option<String>; 3],
    /// less.js `Call.eval`'s wrap guard (its `hasOwnProperty('line')` check):
    /// an error already wrapped as ``Error evaluating function `name`: …``
    /// (or otherwise finalized) must not be re-wrapped/re-anchored by an
    /// enclosing call. Not part of the rendered output.
    pub wrapped: bool,
}

impl LessError {
    /// A location-less error (renders `<Kind>Error: <message>` + two `\n`s —
    /// e.g. the namespace lookup errors, plan §2.12/§5.5).
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        LessError {
            kind,
            message: message.into(),
            filename: None,
            index: None,
            line: None,
            column: None,
            extract: [None, None, None],
            wrapped: false,
        }
    }

    /// An error site with provenance: file + byte index, located later.
    pub fn at(
        kind: ErrorKind,
        message: impl Into<String>,
        filename: impl Into<String>,
        index: usize,
    ) -> Self {
        let filename = filename.into();
        LessError {
            kind,
            message: message.into(),
            filename: (!filename.is_empty()).then_some(filename),
            index: Some(index),
            line: None,
            column: None,
            extract: [None, None, None],
            wrapped: false,
        }
    }

    /// Fill `line`/`column`/`extract` from the (normalized) source of
    /// [`LessError::filename`]. No-op when already located or when the error
    /// has no index. Idempotent.
    pub fn locate(&mut self, source: &str) {
        if self.line.is_some() || source.is_empty() {
            return;
        }
        let Some(index) = self.index else { return };
        // Columns in UTF-16 code units — less.js parity (review F4/R2).
        let (line, column) = LineMap::new(source).line_col_utf16(index, source);
        self.line = Some(line);
        self.column = Some(column);
        let lines: Vec<&str> = source.split('\n').collect();
        let get = |i: isize| -> Option<String> {
            usize::try_from(i)
                .ok()
                .and_then(|i| lines.get(i))
                .map(|s| s.to_string())
        };
        self.extract = [
            get(line as isize - 2),
            get(line as isize - 1),
            get(line as isize),
        ];
    }

    /// [`LessError::locate`] against a builder-style value (parser use).
    pub fn located(mut self, source: &str) -> Self {
        self.locate(source);
        self
    }
}

impl fmt::Display for LessError {
    /// Byte-exact `LessError.prototype.toString()` (see the module docs).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.label(), self.message)?;
        if let Some(file) = &self.filename {
            write!(f, " in {file}")?;
        }
        match (self.filename.is_some(), self.line) {
            (true, Some(line)) => {
                write!(f, " on line {line}, column {}:", self.column.unwrap_or(1))?;
                f.write_str("\n")?;
                let mut any = false;
                if let Some(prev) = &self.extract[0] {
                    write!(f, "{} {prev}\n", line - 1)?;
                    any = true;
                }
                if let Some(cur) = &self.extract[1] {
                    write!(f, "{line} {cur}\n")?;
                    any = true;
                }
                if let Some(next) = &self.extract[2] {
                    write!(f, "{} {next}\n", line + 1)?;
                    any = true;
                }
                // less.js emits `rows.join('\n') + '\n'` — zero rows still
                // terminate with one newline.
                if !any {
                    f.write_str("\n")?;
                }
                Ok(())
            }
            // Filename but no resolved line: `… in <file>\n` (less.js's line
            // resolved to `null` — the excerpt branch is skipped entirely).
            (true, None) => f.write_str("\n"),
            // No filename: less.js's `undefined !== null` quirk — the excerpt
            // branch runs empty and terminates, yielding two newlines.
            (false, _) => f.write_str("\n\n"),
        }
    }
}

impl std::error::Error for LessError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_at(src: &str, index: usize) -> LessError {
        LessError::at(ErrorKind::Syntax, "boom", "f.less", index).located(src)
    }

    #[test]
    fn renders_middle_line_with_both_neighbours() {
        let e = err_at("a {\n  b: c;\n}\n", 6); // 'b'
        assert_eq!(
            e.to_string(),
            "SyntaxError: boom in f.less on line 2, column 3:\n1 a {\n2   b: c;\n3 }\n"
        );
    }

    #[test]
    fn line_one_has_no_previous_line() {
        let e = err_at("x\ny\n", 0);
        assert_eq!(
            e.to_string(),
            "SyntaxError: boom in f.less on line 1, column 1:\n1 x\n2 y\n"
        );
    }

    #[test]
    fn last_line_of_unterminated_file_has_no_next_line() {
        // No trailing newline: split('\n') has no final "" element.
        let e = err_at("x\ny", 2);
        assert_eq!(
            e.to_string(),
            "SyntaxError: boom in f.less on line 2, column 1:\n1 x\n2 y\n"
        );
    }

    #[test]
    fn trailing_newline_yields_empty_next_line_with_trailing_space() {
        // less.js: "x\n".split('\n') == ["x", ""] — the "" renders `2 `.
        let e = err_at("x\n", 0);
        assert_eq!(
            e.to_string(),
            "SyntaxError: boom in f.less on line 1, column 1:\n1 x\n2 \n"
        );
    }

    #[test]
    fn gutter_is_not_right_aligned_across_width_change() {
        let src = "1\n2\n3\n4\n5\n6\n7\n8\n9\nten\n11\n";
        let e = err_at(src, src.find("ten").unwrap());
        assert_eq!(
            e.to_string(),
            "SyntaxError: boom in f.less on line 10, column 1:\n9 9\n10 ten\n11 11\n"
        );
    }

    #[test]
    fn tabs_in_the_excerpt_pass_through_verbatim() {
        let e = err_at("a {\n\tb:c;\n}\n", 5); // the 'b' after the tab
        assert_eq!(
            e.to_string(),
            "SyntaxError: boom in f.less on line 2, column 2:\n1 a {\n2 \tb:c;\n3 }\n"
        );
    }

    #[test]
    fn location_less_error_renders_two_trailing_newlines() {
        let e = LessError::new(ErrorKind::Name, "property \"x\" not found");
        assert_eq!(e.to_string(), "NameError: property \"x\" not found\n\n");
    }

    #[test]
    fn filename_without_line_renders_single_newline() {
        let mut e = LessError::new(ErrorKind::Import, "'x' wasn't found");
        e.filename = Some("f.less".to_string());
        assert_eq!(e.to_string(), "FileError: 'x' wasn't found in f.less\n");
    }

    #[test]
    fn kind_labels() {
        assert_eq!(ErrorKind::Parse.label(), "ParseError");
        assert_eq!(ErrorKind::Import.label(), "FileError");
        assert_eq!(ErrorKind::Argument.label(), "ArgumentError");
    }
}
