//! The parsed AST (plan §9.2).
//!
//! One `Node` enum; self-evaluating leaves (`Dimension`, `Color`, `Quoted`,
//! `Keyword`, `Anonymous`) double as runtime values. The full tree covers all 44
//! less.js `tree/` nodes (plan §1); this scaffold defines the skeleton and the
//! `Span` provenance carried by every node.
//!
//! `Node` is `Send + Sync` so an `Arc<Node>` can be shared, unevaluated, across
//! every compile job (plan §9.6).

/// A byte range into the source, resolved to line/col lazily via
/// [`crate::lex::LineMap`] (plan §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }
}

/// The AST / runtime-value node (plan §9.2).
///
/// Scaffold: only the passthrough leaf and a root container are defined. The
/// remaining ~42 `tree/` nodes (Ruleset, Selector, Declaration, MixinCall,
/// Operation, Import, MagentoImportDirective, …) are added as the parser lands.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Node {
    /// The parse root — a list of top-level rules.
    Root(Vec<Node>),
    /// Passthrough text (self-evaluating leaf). The scaffold wraps whole-source
    /// input in this for the passthrough pipeline.
    Anonymous(String),
}
