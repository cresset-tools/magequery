//! Recursive-descent LESS parser (plan §9.1).
//!
//! Hand-written, like less.js's own bespoke parser. Split into `selector`,
//! `expr`, `entities`, and `atrule` sub-parsers. Scaffold: [`parse`] normalizes
//! source encoding (§H2) and wraps the whole input in a passthrough
//! [`Node::Anonymous`] so the pipeline is end-to-end green.

pub mod atrule;
pub mod entities;
pub mod expr;
pub mod selector;

use std::sync::Arc;

use crate::ast::Node;
use crate::error::LessError;
use crate::lex::normalize_source;
use crate::options::LessOptions;
use crate::resolver::FileInfo;

/// Parse LESS `source` into an immutable AST (plan §9.5). Source encoding is
/// normalized first (BOM strip + CRLF→LF, §H2).
pub fn parse(source: &str, file: FileInfo, opts: &LessOptions) -> Result<Arc<Node>, LessError> {
    let _ = (file, opts);
    let normalized = normalize_source(source);
    // Scaffold passthrough: the real tokenizer + recursive descent land in
    // Phase 1. Wrapping the whole source as Anonymous makes `eval` emit it
    // verbatim (a source-normalizing passthrough).
    Ok(Arc::new(Node::Anonymous(normalized.into_owned())))
}
