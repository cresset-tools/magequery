//! Structural PHP parser for magecommand.
//!
//! A DI compiler needs declarations, never method bodies. This crate parses
//! exactly that subset: namespace + use imports, class/interface/trait/enum
//! headers, constructor parameters (incl. promoted properties), public method
//! signatures, trait uses, and class constants. Bodies are *skipped*, not
//! parsed — `memchr`-accelerated scanning past string/comment/heredoc interiors
//! at memory bandwidth, with a scalar recursive-descent over the structural
//! remainder.
//!
//! Design contract (CAS prerequisite): parsing is a pure function of the input
//! bytes. No filesystem access, no environment, no PHP execution — ever. A
//! construct this parser cannot classify is a hard [`ParseIssue`], never a
//! silent guess.
//!
//! The parser itself lands with milestone M1 (see `.plans/magecommand.md`);
//! this crate currently holds the output model.

/// Kind of a PHP type declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClassKind {
    Class,
    Interface,
    Trait,
    Enum,
}

/// A construct the parser could not confidently classify.
///
/// There is no fallback tier behind this parser: an issue is surfaced to the
/// caller as a hard diagnostic naming the file and byte offset.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ParseIssue {
    /// Byte offset into the source where classification failed.
    pub offset: usize,
    /// Human-readable description of the unhandled construct.
    pub message: String,
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_compiles() {}
}
