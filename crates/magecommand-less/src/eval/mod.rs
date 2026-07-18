//! The tree-rewriting evaluator (plan §4).
//!
//! Pipeline order is load-bearing (from less.js `transform-tree.js`):
//! import resolution → `root.eval` → JoinSelector → MarkVisibleSelectors →
//! Extend → ToCSS → genCSS. Split into `context` (frame stack), `scope`,
//! `mixin`, `operation`, `import`, `extend`, `merge` sub-passes.
//!
//! Scaffold: [`eval`] emits a passthrough `Anonymous` node's text verbatim.

pub mod context;
pub mod extend;
pub mod import;
pub mod merge;
pub mod mixin;
pub mod operation;
pub mod scope;

use std::sync::Arc;

use crate::ast::Node;
use crate::css::Css;
use crate::error::LessError;
use crate::options::LessOptions;
use crate::resolver::ImportResolver;

/// Evaluate a parsed AST to [`Css`] (plan §9.5). Scaffold passthrough.
pub fn eval(
    root: &Arc<Node>,
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
) -> Result<Css, LessError> {
    let _ = (opts, resolver);
    let code = match root.as_ref() {
        Node::Anonymous(text) => text.clone(),
        Node::Root(_) => String::new(),
    };
    Ok(Css::from_code(code))
}
