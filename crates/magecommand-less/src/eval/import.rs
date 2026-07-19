//! Two-stage `@import` (plan §2.9, §4.5) — **stage 1**: the pre-eval
//! fetch/parse pass (less.js `ImportVisitor` + `ImportManager`).
//!
//! Walks the parsed tree, resolves every non-CSS `@import` through the
//! [`ImportResolver`], parses the payload, and replaces the `Node::Import`
//! with a [`Node::ImportResolved`] carrying the file's rules at the import's
//! source position. Stage 2 (eval, `eval/mod.rs`) splices/evaluates those
//! position-preservingly.
//!
//! Semantics mirrored from less.js 4.6.7 (verified by probe):
//! - **once (default)**: the FIRST fetch of a canonical path attaches rules;
//!   any further non-`(multiple)` import of the same path hard-skips
//!   (`recursionDetector`) — which also silently terminates **import cycles**
//!   (`A imports A`; the fetched-set is populated *before* the subtree walk).
//! - **variable-path deferral** (`ImportSequencer.addVariableImport`): an
//!   import whose path needs variable interpolation — or any non-quoted path,
//!   matching `isVariableImport` — is processed only after every regular
//!   import (tree-wide) has been fetched, in document order, so its variables
//!   can come from files imported *later* in source (the `variables()` peek).
//! - **css passthrough**: a path matching `[#.&?]css([?;].*)?$` (or `(css)`)
//!   is never fetched — stage 2 re-emits the literal `@import`.
//! - `(optional)` misses hard-skip; `(inline)` payloads attach verbatim and
//!   are not entered into the fetched-set (less.js doesn't cache inlines).
//! - `(reference)` propagates to every transitively imported file.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{AtRuleBlock, ImportResolved, Node};
use crate::error::{ErrorKind, LessError};
use crate::options::{LessOptions, RewriteUrls};
use crate::resolver::{FileInfo, ImportOptions, ImportPayload, ImportRequest, ImportResolver};

/// The file whose rules are being walked/evaluated — the base for its relative
/// imports and generated urls (less.js `fileInfo`).
#[derive(Debug, Clone)]
pub(crate) struct FileScope {
    /// Canonical filename (the once-dedup key for the entry file).
    pub filename: String,
    /// Directory of `filename`.
    pub current_directory: String,
    /// `rootpath` in effect for urls generated from this file (§2.18).
    pub rootpath: String,
    /// The file was reached through a `(reference)` import.
    pub reference: bool,
}

impl FileScope {
    /// The entry file's scope, derived from the options.
    pub fn entry(opts: &LessOptions) -> FileScope {
        let filename = opts.filename.clone().unwrap_or_default();
        let current_directory = match filename.rfind('/') {
            Some(i) => filename[..=i].to_string(),
            None => String::new(),
        };
        FileScope {
            filename,
            current_directory,
            rootpath: opts.rootpath.clone().unwrap_or_default(),
            reference: false,
        }
    }
}

/// Stage-1 import resolution over a mutable rule tree. Returns an error only
/// for a genuinely failing non-`(optional)` fetch or a parse failure.
pub(crate) fn resolve_imports(
    rules: &mut Vec<Node>,
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
) -> Result<(), LessError> {
    let entry = FileScope::entry(opts);
    let mut pass = ImportPass {
        opts,
        resolver,
        parsed: FxHashMap::default(),
        fetched: FxHashSet::default(),
    };
    if !entry.filename.is_empty() {
        pass.fetched.insert(normalize_key(&entry.filename));
    }
    // Phase 1: every regular (non-variable-path) import, recursively.
    pass.walk_list(rules, &entry, false)?;
    // Phase 2: variable-path imports, one at a time in document order — each
    // resolution may introduce a subtree whose regular imports are expanded
    // immediately (inside `fetch`) and whose own variable imports join the
    // next round.
    loop {
        let Some(loc) = find_var_import(rules, &mut Vec::new()) else {
            break;
        };
        pass.resolve_var_import(rules, &loc, &entry)?;
    }
    Ok(())
}

/// The mutable child rule-lists of a node the import walk descends into
/// (mirrors less.js `ImportVisitor`'s visit set).
fn child_lists_mut(node: &mut Node) -> Vec<&mut Vec<Node>> {
    match node {
        Node::Ruleset(r) => vec![&mut r.rules],
        Node::MixinDefinition(d) => vec![&mut d.rules],
        Node::DetachedRuleset { rules, .. } => vec![rules],
        Node::AtRule(a) => match &mut a.block {
            AtRuleBlock::Rules(rules) => vec![rules],
            AtRuleBlock::None => vec![],
        },
        Node::ImportResolved(ir) => vec![&mut ir.rules],
        Node::VariableDecl { value, .. } => match value.as_mut() {
            Node::DetachedRuleset { rules, .. } => vec![rules],
            _ => vec![],
        },
        _ => vec![],
    }
}

/// Read-only view of the same child lists.
fn child_lists(node: &Node) -> Vec<&Vec<Node>> {
    match node {
        Node::Ruleset(r) => vec![&r.rules],
        Node::MixinDefinition(d) => vec![&d.rules],
        Node::DetachedRuleset { rules, .. } => vec![rules],
        Node::AtRule(a) => match &a.block {
            AtRuleBlock::Rules(rules) => vec![rules],
            AtRuleBlock::None => vec![],
        },
        Node::ImportResolved(ir) => vec![&ir.rules],
        Node::VariableDecl { value, .. } => match value.as_ref() {
            Node::DetachedRuleset { rules, .. } => vec![rules],
            _ => vec![],
        },
        _ => vec![],
    }
}

/// The raw string of an import path node (`"x"`, `url("x")`, `url(x)`).
pub(crate) fn import_path_string(path: &Node) -> Option<String> {
    match path {
        Node::Quoted { value, .. } => Some(value.clone()),
        Node::Url(inner) => import_path_string(inner),
        Node::Anonymous(s) => {
            // `url(x)` may keep the whole call as raw text.
            let t = s.trim();
            if let Some(rest) = t.strip_prefix("url(") {
                let inner = rest.strip_suffix(')').unwrap_or(rest).trim();
                Some(inner.trim_matches(|c| c == '"' || c == '\'').to_string())
            } else {
                Some(t.trim_matches(|c| c == '"' || c == '\'').to_string())
            }
        }
        Node::Keyword(s) => Some(s.clone()),
        _ => None,
    }
}

/// less.js `Import.isVariableImport`: a quoted path is variable iff it
/// interpolates; any NON-quoted path (bare `url(x)`, keyword) counts as
/// variable and is deferred with the sequencer.
fn is_variable_import(path: &Node) -> bool {
    match path {
        Node::Quoted { value, .. } => value.contains("@{"),
        Node::Url(inner) => is_variable_import(inner),
        Node::Anonymous(s) => {
            let t = s.trim();
            let inner = t
                .strip_prefix("url(")
                .map(|r| r.strip_suffix(')').unwrap_or(r).trim())
                .unwrap_or(t);
            if inner.starts_with('"') || inner.starts_with('\'') {
                inner.contains("@{")
            } else {
                true
            }
        }
        _ => true,
    }
}

/// less.js's css-path test: `/[#.&?]css([?;].*)?$/`.
pub(crate) fn is_css_path(p: &str) -> bool {
    let bytes = p.as_bytes();
    let mut i = 0usize;
    while let Some(off) = p[i..].find("css") {
        let at = i + off;
        let pre_ok = at > 0 && matches!(bytes[at - 1], b'#' | b'.' | b'&' | b'?');
        let after = at + 3;
        let post_ok = after == p.len() || matches!(bytes[after], b'?' | b';');
        if pre_ok && post_ok {
            return true;
        }
        i = at + 1;
    }
    false
}

/// Whether an import statement is a CSS passthrough (never fetched):
/// `(css)` forces it, `(less)`/`(inline)` forbid it, else the path decides.
pub(crate) fn import_is_css(path: &Node, options: &[String]) -> bool {
    if options.iter().any(|o| o == "inline") {
        return false;
    }
    if options.iter().any(|o| o == "less") {
        return false;
    }
    if options.iter().any(|o| o == "css") {
        return true;
    }
    match import_path_string(path) {
        Some(p) => is_css_path(&p),
        None => false,
    }
}

/// A document-order location of a rule node: indices of `(node, child_list)`
/// hops from the root list, ending with the node's index in its final list.
type Loc = Vec<(usize, usize)>;

/// Find the first still-unresolved variable-path `@import` in document order.
/// `trail` accumulates `(node_idx, list_idx)` hops.
fn find_var_import(rules: &[Node], trail: &mut Vec<(usize, usize)>) -> Option<Loc> {
    for (idx, node) in rules.iter().enumerate() {
        if let Node::Import { path, options, .. } = node {
            if !import_is_css(path, options) && is_variable_import(path) {
                let mut loc = trail.clone();
                loc.push((idx, usize::MAX));
                return Some(loc);
            }
        }
        for (li, list) in child_lists(node).into_iter().enumerate() {
            trail.push((idx, li));
            if let Some(found) = find_var_import(list, trail) {
                trail.pop();
                return Some(found);
            }
            trail.pop();
        }
    }
    None
}

struct ImportPass<'a> {
    opts: &'a LessOptions,
    resolver: &'a dyn ImportResolver,
    /// full_path → parsed rules (one parse per file, `(multiple)` clones).
    parsed: FxHashMap<String, Vec<Node>>,
    /// less.js `recursionDetector` ∪ `files`: canonical paths already fetched.
    fetched: FxHashSet<String>,
}

impl<'a> ImportPass<'a> {
    /// Phase-1 walk: fetch every regular import in `rules`, recursively
    /// (including into freshly attached subtrees, which `fetch` walks).
    /// `in_multiple` = an enclosing import was `(multiple)` — less.js sets
    /// `context.importMultiple` for the whole subtree, so nested imports skip
    /// the once-dedup too (verified: a twice-`(multiple)`-imported file
    /// re-emits its own once-imports each time).
    fn walk_list(
        &mut self,
        rules: &mut Vec<Node>,
        scope: &FileScope,
        in_multiple: bool,
    ) -> Result<(), LessError> {
        for node in rules.iter_mut() {
            if let Node::Import { path, options, .. } = node {
                if import_is_css(path, options) || is_variable_import(path) {
                    continue;
                }
                let Some(p) = import_path_string(path) else {
                    continue;
                };
                let resolved = self.fetch(&p, node, scope, in_multiple)?;
                *node = resolved;
                continue;
            }
            let (child_scope, child_multiple) = match node {
                Node::ImportResolved(ir) => (
                    Some(FileScope {
                        filename: ir.full_path.clone(),
                        current_directory: ir.current_directory.clone(),
                        rootpath: ir.rootpath.clone(),
                        reference: ir.reference,
                    }),
                    ir.multiple,
                ),
                _ => (None, in_multiple),
            };
            let s = child_scope.as_ref().unwrap_or(scope);
            for list in child_lists_mut(node) {
                self.walk_list(list, s, child_multiple)?;
            }
        }
        Ok(())
    }

    /// Resolve one located variable-path import: interpolate its path with a
    /// frame stack built from the enclosing raw rule lists (innermost-first),
    /// then fetch and replace in place. An uninterpolatable path falls back to
    /// the less.js behavior: the import is marked CSS and re-emitted literally.
    fn resolve_var_import(
        &mut self,
        rules: &mut Vec<Node>,
        loc: &Loc,
        entry: &FileScope,
    ) -> Result<(), LessError> {
        // Collect the enclosing rule lists (outermost-first) + the owning file
        // scope by navigating the trail read-only.
        let mut frames_outer_first: Vec<Vec<Node>> = vec![rules.clone()];
        let mut scope = entry.clone();
        {
            let mut list: &Vec<Node> = &*rules;
            for &(node_idx, list_idx) in &loc[..loc.len() - 1] {
                let node = &list[node_idx];
                if let Node::ImportResolved(ir) = node {
                    scope = FileScope {
                        filename: ir.full_path.clone(),
                        current_directory: ir.current_directory.clone(),
                        rootpath: ir.rootpath.clone(),
                        reference: ir.reference,
                    };
                }
                let children = child_lists(node);
                list = children[list_idx];
                frames_outer_first.push(list.clone());
            }
        }

        // Navigate mutably to the import node.
        let node = {
            let mut list: &mut Vec<Node> = rules;
            for &(node_idx, list_idx) in &loc[..loc.len() - 1] {
                let node = &mut list[node_idx];
                let children = child_lists_mut(node);
                list = children.into_iter().nth(list_idx).expect("stable loc");
            }
            &mut list[loc[loc.len() - 1].0]
        };
        let Node::Import { path, options, .. } = &*node else {
            return Ok(());
        };
        let raw = import_path_string(path).unwrap_or_default();

        let interpolated = if raw.contains("@{") {
            super::interpolate_standalone(&raw, &frames_outer_first, self.opts, self.resolver)
        } else {
            Some(raw.clone())
        };
        let Some(p) = interpolated else {
            // less.js: a path that can't evaluate at import time is treated as
            // css (emitted literally; its saved error only rethrows for
            // less-forced imports). Mark it so the fixpoint terminates.
            let mut opts2 = options.clone();
            opts2.push("css".to_string());
            if let Node::Import { options, .. } = node {
                *options = opts2;
            }
            return Ok(());
        };
        if is_css_path(&p) && !options.iter().any(|o| o == "less" || o == "inline") {
            // Interpolated into a css path: literal re-emit (keep as Import,
            // stage 2 interpolates again for output).
            let mut opts2 = options.clone();
            opts2.push("css".to_string());
            if let Node::Import { options, .. } = node {
                *options = opts2;
            }
            return Ok(());
        }
        let resolved = self.fetch(&p, node, &scope, false)?;
        *node = resolved;
        Ok(())
    }

    /// Fetch + parse one import (`node` must be `Node::Import`), returning the
    /// replacement [`Node::ImportResolved`]. Marks the fetched-set BEFORE
    /// walking the subtree, so cyclic once-imports terminate silently.
    fn fetch(
        &mut self,
        raw_path: &str,
        node: &Node,
        scope: &FileScope,
        in_multiple: bool,
    ) -> Result<Node, LessError> {
        let Node::Import {
            options,
            features,
            span,
            ..
        } = node
        else {
            unreachable!("fetch() is only called on Import nodes");
        };
        let has = |o: &str| options.iter().any(|x| x == o);
        let inline = has("inline");
        let multiple = has("multiple") || in_multiple;
        let optional = has("optional");
        let reference = has("reference") || scope.reference;

        let req = ImportRequest {
            path: raw_path.to_string(),
            from: FileInfo {
                filename: scope.filename.clone(),
                current_directory: scope.current_directory.clone(),
                rootpath: scope.rootpath.clone(),
                ..Default::default()
            },
            options: ImportOptions {
                reference,
                inline,
                css: if has("css") {
                    Some(true)
                } else if has("less") {
                    Some(false)
                } else {
                    None
                },
                once: has("once"),
                multiple,
                optional,
                layer: None,
            },
        };
        let resolved = match self.resolver.resolve(&req) {
            Ok(r) => r,
            Err(_) if optional => {
                return Ok(Node::ImportResolved(Box::new(ImportResolved {
                    rules: Vec::new(),
                    inline: None,
                    full_path: raw_path.to_string(),
                    skip: true,
                    multiple,
                    reference,
                    features: features.clone(),
                    current_directory: scope.current_directory.clone(),
                    rootpath: scope.rootpath.clone(),
                    span: *span,
                })))
            }
            Err(e) => {
                return Err(LessError::new(
                    ErrorKind::Import,
                    format!("'{raw_path}' wasn't found. {e}"),
                ))
            }
        };
        // Lexically normalized — the once-dedup key must treat
        // `a/deeper/../c.less` and `a/c.less` as one file.
        let full_path = normalize_key(&resolved.file.filename);
        let current_directory = resolved.file.current_directory.clone();

        // Rootpath for urls generated from the imported file (§2.18): with
        // rewriting on, options.rootpath + the relative dir diff from the
        // entry; otherwise inherited from the importer (less.js
        // `ImportManager.push`).
        let rootpath = if self.opts.rewrite_urls != RewriteUrls::Off {
            let entry_dir = FileScope::entry(self.opts).current_directory;
            format!(
                "{}{}",
                self.opts.rootpath.clone().unwrap_or_default(),
                path_diff(&current_directory, &entry_dir)
            )
        } else {
            scope.rootpath.clone()
        };

        match resolved.payload {
            ImportPayload::Inline(src) | ImportPayload::Css(src) if inline => {
                Ok(Node::ImportResolved(Box::new(ImportResolved {
                    rules: Vec::new(),
                    inline: Some(src.to_string()),
                    full_path,
                    skip: false,
                    multiple,
                    reference,
                    features: features.clone(),
                    current_directory,
                    rootpath,
                    span: *span,
                })))
            }
            payload => {
                // Once-dedup / cycle guard: a non-(multiple) re-fetch of an
                // already-fetched path hard-skips.
                if !multiple && self.fetched.contains(&full_path) {
                    return Ok(Node::ImportResolved(Box::new(ImportResolved {
                        rules: Vec::new(),
                        inline: None,
                        full_path,
                        skip: true,
                        multiple,
                        reference,
                        features: features.clone(),
                        current_directory,
                        rootpath,
                        span: *span,
                    })));
                }
                self.fetched.insert(full_path.clone());

                let mut rules: Vec<Node> = if let Some(cached) = self.parsed.get(&full_path) {
                    cached.clone()
                } else {
                    let src: std::sync::Arc<str> = match payload {
                        ImportPayload::Less(s)
                        | ImportPayload::Css(s)
                        | ImportPayload::Inline(s) => s,
                        ImportPayload::Ast(node) => {
                            let rules = match node.as_ref() {
                                Node::Root(r) => r.clone(),
                                other => vec![other.clone()],
                            };
                            self.parsed.insert(full_path.clone(), rules.clone());
                            return self.finish_fetch(
                                rules,
                                full_path,
                                multiple,
                                reference,
                                features.clone(),
                                current_directory,
                                rootpath,
                                *span,
                            );
                        }
                    };
                    let file = FileInfo {
                        filename: full_path.clone(),
                        current_directory: current_directory.clone(),
                        ..Default::default()
                    };
                    let parsed = crate::parser::parse(&src, file, self.opts)?;
                    let rules = match parsed.as_ref() {
                        Node::Root(r) => r.clone(),
                        other => vec![other.clone()],
                    };
                    self.parsed.insert(full_path.clone(), rules.clone());
                    rules
                };

                // Walk the freshly attached subtree NOW (regular imports;
                // nested variable imports join the outer fixpoint).
                let child_scope = FileScope {
                    filename: full_path.clone(),
                    current_directory: current_directory.clone(),
                    rootpath: rootpath.clone(),
                    reference,
                };
                self.walk_list(&mut rules, &child_scope, multiple)?;

                self.finish_fetch(
                    rules,
                    full_path,
                    multiple,
                    reference,
                    features.clone(),
                    current_directory,
                    rootpath,
                    *span,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_fetch(
        &mut self,
        rules: Vec<Node>,
        full_path: String,
        multiple: bool,
        reference: bool,
        features: Option<Box<Node>>,
        current_directory: String,
        rootpath: String,
        span: crate::ast::Span,
    ) -> Result<Node, LessError> {
        Ok(Node::ImportResolved(Box::new(ImportResolved {
            rules,
            inline: None,
            full_path,
            skip: false,
            multiple,
            reference,
            features,
            current_directory,
            rootpath,
            span,
        })))
    }
}

/// Lexical path normalization for dedup keys: collapse `.` and `x/..` hops
/// (matching what the less.js file managers' absolute-path resolution yields).
pub(crate) fn normalize_key(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." => {}
            ".." => {
                if out.is_empty() || out.last() == Some(&"..") {
                    out.push(segment);
                } else {
                    out.pop();
                }
            }
            _ => out.push(segment),
        }
    }
    out.join("/")
}

/// less.js `AbstractFileManager.pathDiff`: the relative path from `base_dir`
/// to `dir` (both directory paths), as `../`-and-segment hops.
pub(crate) fn path_diff(dir: &str, base_dir: &str) -> String {
    let dirs: Vec<&str> = dir.trim_end_matches('/').split('/').collect();
    let bases: Vec<&str> = base_dir.trim_end_matches('/').split('/').collect();
    let mut i = 0usize;
    while i < dirs.len() && i < bases.len() && dirs[i] == bases[i] {
        i += 1;
    }
    let mut out = String::new();
    for _ in i..bases.len() {
        out.push_str("../");
    }
    for d in &dirs[i..] {
        out.push_str(d);
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_path_regex_matches_less_js() {
        assert!(is_css_path("foo.css"));
        assert!(is_css_path("foo.css?query"));
        assert!(is_css_path("foo.css;jsessionid=x"));
        assert!(is_css_path("foo?css"));
        assert!(is_css_path("foo#css"));
        assert!(!is_css_path("foo.css.less"));
        assert!(!is_css_path("foo.less"));
        assert!(!is_css_path("css"));
        assert!(!is_css_path("foocss"));
    }

    #[test]
    fn path_diff_walks_directories() {
        assert_eq!(path_diff("/a/b/c/", "/a/b/"), "c/");
        assert_eq!(path_diff("/a/", "/a/b/"), "../");
        assert_eq!(path_diff("/a/x/", "/a/b/"), "../x/");
        assert_eq!(path_diff("/a/b/", "/a/b/"), "");
    }
}
