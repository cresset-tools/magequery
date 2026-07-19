//! # magecommand-less
//!
//! A pure-Rust LESS compiler for magecommand's `static deploy` (the Luma CSS
//! path). No JS engine, no PHP, no node — one self-contained crate, hand-written
//! in the `php.rs`/`graphql.rs` tradition. See `magecommand-less-plan.md` for the
//! full compatibility plan.
//!
//! ## Status
//!
//! **Scaffold (Phase 0, step 1).** The public API surface, options, error model,
//! and `ImportResolver` boundary from plan §9 are defined; the pipeline modules
//! (lexer, parser, eval, genCSS) are compiling stubs. [`compile`] currently
//! performs a source-normalizing passthrough (BOM/CRLF handling, §H2). See
//! `NOTES.md` for what is and is not yet implemented.
//!
//! ## Pipeline (plan §9.1)
//!
//! ```text
//! source → tokenize → parser::parse → Arc<Node> (immutable)
//!        → eval::Evaluator → lower_to_css → genCSS → Css → (downstream) minify
//! ```
//!
//! The two-phase `parse`→`eval` split keeps the parsed [`ast::Node`] immutable
//! and shareable (`Arc::clone`) across every theme×locale×area job (§9.6).

pub mod ast;
pub mod color;
pub mod css;
pub mod data;
pub mod error;
pub mod eval;
pub mod functions;
pub mod lex;
pub mod options;
pub mod parser;
pub mod resolver;
pub mod unit;
pub mod value;

#[cfg(feature = "minify")]
pub mod minify;

use std::sync::Arc;

pub use crate::css::{Css, Warning};
pub use crate::error::{ErrorKind, LessError};
pub use crate::options::{CompatProfile, LessOptions, MathMode, RewriteUrls};
pub use crate::resolver::{
    FileInfo, ImportError, ImportOptions, ImportPayload, ImportRequest, ImportResolver,
    MagentoImportEntry, NoopResolver, ResolvedImport,
};

/// Parse LESS `source` into an immutable AST (plan §9.5).
///
/// The returned `Arc<Node>` is `Send + Sync` and may be shared, unevaluated,
/// across many `eval` jobs (a library file parses once per deploy). Source
/// encoding is normalized first: a leading UTF-8 BOM is stripped and CRLF is
/// normalized to LF before tokenizing (§H2).
pub fn parse(source: &str, file: FileInfo, opts: &LessOptions) -> Result<Arc<ast::Node>, LessError> {
    parser::parse(source, file, opts)
}

/// Evaluate a parsed AST to CSS against the given options and import resolver
/// (plan §9.5). Pure over `(Arc<Node>, &LessOptions, &dyn ImportResolver)`, so
/// `jobs.par_iter().map(eval)` parallelizes across (theme × locale × area).
pub fn eval(
    root: &Arc<ast::Node>,
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
) -> Result<Css, LessError> {
    eval::eval(root, opts, resolver)
}

/// Compile LESS `source` straight to [`Css`] (plan §9.5): `parse` then `eval`.
///
/// **Scaffold behavior:** a source-normalizing passthrough — the (BOM/CRLF-
/// normalized) input is returned as the `.code` field. Real lowering lands in
/// later phases.
pub fn compile(
    source: &str,
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
) -> Result<Css, LessError> {
    let file = FileInfo {
        filename: opts.filename.clone().unwrap_or_default(),
        ..FileInfo::default()
    };
    // `banner`/`globalVars` prepend and `modifyVars` append SOURCE text before
    // parsing (less.js `Parser.parse` preText/serializeVars, plan §2.0):
    // globals are overridable (last-declaration-wins favors the sheet), while
    // modify-vars appended last override the sheet.
    let augmented: String;
    let source = if opts.global_vars.is_empty()
        && opts.modify_vars.is_empty()
        && opts.banner.is_none()
    {
        source
    } else {
        let mut pre = opts.banner.clone().unwrap_or_default();
        if !opts.global_vars.is_empty() {
            pre.push_str(&serialize_vars(&opts.global_vars));
            pre.push('\n');
        }
        let post = if opts.modify_vars.is_empty() {
            String::new()
        } else {
            format!("\n{}", serialize_vars(&opts.modify_vars))
        };
        augmented = format!("{pre}{source}{post}");
        &augmented
    };
    let root = parse(source, file, opts)?;
    eval(&root, opts, resolver)
}

/// less.js `Parser.serializeVars`: `@name: value;` runs (the `@` prefix and
/// trailing `;` added when missing).
fn serialize_vars(vars: &[(String, String)]) -> String {
    let mut s = String::new();
    for (name, value) in vars {
        if !name.starts_with('@') {
            s.push('@');
        }
        s.push_str(name);
        s.push_str(": ");
        s.push_str(value);
        if !value.ends_with(';') {
            s.push(';');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_css_ruleset_serializes() {
        let opts = LessOptions::default();
        let out = compile("a { color: red; }", &opts, &NoopResolver).unwrap();
        assert_eq!(out.code.trim_end(), "a {\n  color: red;\n}");
    }

    #[test]
    fn bom_and_crlf_are_normalized_before_parsing() {
        let opts = LessOptions::default();
        let out = compile("\u{feff}a {\r\n color: red;\r\n}", &opts, &NoopResolver).unwrap();
        assert_eq!(out.code.trim_end(), "a {\n  color: red;\n}");
    }

    #[test]
    fn line_comments_are_stripped_block_kept() {
        let opts = LessOptions::default();
        let out = compile("// gone\n/* kept */\na { b: c; }", &opts, &NoopResolver).unwrap();
        assert_eq!(out.code.trim_end(), "/* kept */\na {\n  b: c;\n}");
    }

    #[test]
    fn multiple_selectors_and_values() {
        let opts = LessOptions::default();
        let out = compile(".a, .b > i { m: 1px 2px, 3px; }", &opts, &NoopResolver).unwrap();
        assert_eq!(out.code.trim_end(), ".a,\n.b > i {\n  m: 1px 2px, 3px;\n}");
    }

    #[test]
    fn default_profile_is_less_js() {
        let opts = LessOptions::default();
        assert_eq!(opts.profile, CompatProfile::LessJs);
        assert_eq!(opts.math, MathMode::ParensDivision);
        assert_eq!(opts.num_precision, 8);
    }
}
