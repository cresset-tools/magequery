//! Error model (plan §4.6, §5.5, §F3).
//!
//! magequery's split: [`LessError`] halts a file (returns `Err`); non-fatal
//! problems become [`crate::css::Warning`]s collected on [`crate::css::Css`].
//!
//! The full less.js error-kind set is 7 and all are required up front (§F3).
//! The `Display` impl targets the byte-exact `err.toString()` format:
//! `"<Kind>: <message> in <file> on line <L>, column <C>:"` plus a 3-line source
//! excerpt. Errors with no location omit the ` in … on line …` tail.

use std::fmt;

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
    /// The `type` label less.js uses in `toString()` (the `<Type>Error:` prefix).
    ///
    /// NOTE (scaffold): the exact prefixing (`ParseError:` vs `Parse:`) and the
    /// source-excerpt rendering are pinned in the error phase against the
    /// `tests-error` corpus; this is provisional.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Parse => "Parse",
            ErrorKind::Name => "Name",
            ErrorKind::Operation => "Operation",
            ErrorKind::Syntax => "Syntax",
            ErrorKind::Import => "Import",
            ErrorKind::Runtime => "Runtime",
            ErrorKind::Argument => "Argument",
        }
    }
}

/// A fatal compile error with byte-exact `Display` (plan §5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessError {
    /// Which of the 7 kinds.
    pub kind: ErrorKind,
    /// Human-readable message (the part after `<Kind>: `).
    pub message: String,
    /// Source file the error is in (absent for location-less errors).
    pub filename: Option<String>,
    /// 1-based line.
    pub line: Option<usize>,
    /// 1-based column.
    pub column: Option<usize>,
    /// The 3-line source excerpt (line-1 / offending / line+1), rendered by the
    /// harness/`Display`. Empty until the error renderer is wired.
    pub excerpt: Vec<String>,
}

impl LessError {
    /// A location-less error (e.g. some `variable @x is undefined` cases whose
    /// `Display` must omit the ` in … on line …` tail — plan §2.1/§5.5).
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        LessError {
            kind,
            message: message.into(),
            filename: None,
            line: None,
            column: None,
            excerpt: Vec::new(),
        }
    }
}

impl fmt::Display for LessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Scaffold rendering: `<Kind>: <message>` with the location tail when
        // present. The byte-exact `<Type>Error:` prefix + 3-line excerpt land in
        // the error phase (§F3), gated by the 74 error fixtures.
        write!(f, "{}: {}", self.kind.as_str(), self.message)?;
        if let (Some(file), Some(line), Some(col)) = (&self.filename, self.line, self.column) {
            write!(f, " in {} on line {}, column {}:", file, line, col)?;
        }
        Ok(())
    }
}

impl std::error::Error for LessError {}
