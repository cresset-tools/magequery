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
    /// The file's NORMALIZED source — the base for locating errors raised
    /// while evaluating its rules (§5.5). Empty when unknown (bare `eval()`
    /// callers; locations then degrade to the message-only rendering).
    pub source: std::sync::Arc<str>,
}

impl FileScope {
    /// The entry file's scope, derived from the options. A non-empty
    /// `rootpath` without a trailing `/` gets one appended — core less.js
    /// normalizes at parse setup (parse.js:52; review F11).
    pub fn entry(opts: &LessOptions) -> FileScope {
        let filename = opts.filename.clone().unwrap_or_default();
        let current_directory = match filename.rfind('/') {
            Some(i) => filename[..=i].to_string(),
            None => String::new(),
        };
        let mut rootpath = opts.rootpath.clone().unwrap_or_default();
        if !rootpath.is_empty() && !rootpath.ends_with('/') {
            rootpath.push('/');
        }
        FileScope {
            filename,
            current_directory,
            rootpath,
            reference: false,
            source: std::sync::Arc::from(""),
        }
    }
}

/// Stage-1 import resolution over a mutable rule tree. Returns an error only
/// for a genuinely failing non-`(optional)` fetch or a parse failure.
///
/// The walk is **breadth-first by file visit** (review F13): less.js's
/// `ImportSequencer` runs `onImported` callbacks in registration order, and a
/// fetched file's own imports register only when its root is visited — so all
/// of a file's imports claim their once-slots before any import discovered
/// inside a fetched subtree. A root-level import therefore beats a nested one
/// to the once-slot (flipping both output order and, under `(reference)`,
/// visibility, vs the old depth-first walk).
pub(crate) fn resolve_imports(
    rules: &mut Vec<Node>,
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
    entry_source: std::sync::Arc<str>,
) -> Result<(), LessError> {
    let mut entry = FileScope::entry(opts);
    entry.source = entry_source;
    let mut pass = ImportPass {
        opts,
        resolver,
        parsed: FxHashMap::default(),
        sources: FxHashMap::default(),
        fetched: FxHashSet::default(),
        queue: std::collections::VecDeque::new(),
    };
    if !entry.filename.is_empty() {
        pass.fetched.insert(normalize_key(&entry.filename));
    }
    // Stamp the entry file's url/resource nodes with its file info (§2.18).
    let entry_tag = std::sync::Arc::new(crate::ast::FileTag {
        rootpath: entry.rootpath.clone(),
        directory: entry.current_directory.clone(),
    });
    stamp_urls(rules, &entry_tag);
    // Phase 1: every regular (non-variable-path) import, BFS by file.
    pass.visit_list(rules, &entry, false, 0, &mut Vec::new())?;
    pass.drain_queue(rules)?;
    // Phase 2: variable-path imports, one at a time in document order — each
    // resolution may introduce a subtree whose regular imports are expanded
    // (queue drained) before the next variable import. The ROOT frame used
    // for path interpolation is SNAPSHOT at the first variable import —
    // modeling less.js's `Ruleset.variables()` memo, which freezes before
    // any variable import's subtree attaches (review F4: chained
    // variable-path imports must fail like less.js).
    let mut root_snapshot: Option<Vec<Node>> = None;
    loop {
        let Some(loc) = find_var_import(rules, &mut Vec::new()) else {
            break;
        };
        if root_snapshot.is_none() {
            root_snapshot = Some(rules.clone());
        }
        let snap = root_snapshot.clone().expect("just set");
        pass.resolve_var_import(rules, &loc, &entry, snap)?;
        pass.drain_queue(rules)?;
    }
    Ok(())
}

/// Recursively stamp `url(...)` values and resource-function calls with the
/// file they were written in (§2.18, review F3/F8). Already-stamped nodes and
/// resolved import subtrees (stamped at their own fetch) are left alone; any
/// node kind the walk doesn't know keeps the old current-eval-file fallback.
pub(crate) fn stamp_urls(rules: &mut [Node], tag: &std::sync::Arc<crate::ast::FileTag>) {
    for node in rules.iter_mut() {
        stamp_node(node, tag);
    }
}

fn is_resource_fn(name: &str) -> bool {
    name.eq_ignore_ascii_case("data-uri")
        || name.eq_ignore_ascii_case("image-size")
        || name.eq_ignore_ascii_case("image-width")
        || name.eq_ignore_ascii_case("image-height")
}

fn stamp_node(node: &mut Node, tag: &std::sync::Arc<crate::ast::FileTag>) {
    match node {
        Node::WithFile { .. } | Node::ImportResolved(_) | Node::Import { .. } => {}
        Node::Url(_) => {
            let inner = std::mem::replace(node, Node::Anonymous(String::new()));
            *node = Node::WithFile {
                inner: Box::new(inner),
                tag: tag.clone(),
            };
        }
        Node::Call { name, args, .. } => {
            if is_resource_fn(name) {
                let inner = std::mem::replace(node, Node::Anonymous(String::new()));
                *node = Node::WithFile {
                    inner: Box::new(inner),
                    tag: tag.clone(),
                };
            } else {
                for a in args.iter_mut() {
                    stamp_node(a, tag);
                }
            }
        }
        Node::Root(rules)
        | Node::DetachedRuleset { rules, .. }
        | Node::Value(rules)
        | Node::Expression(rules) => stamp_urls(rules, tag),
        Node::Ruleset(r) => stamp_urls(&mut r.rules, tag),
        Node::Declaration(d) => stamp_node(&mut d.value, tag),
        Node::VariableDecl { value, .. } => stamp_node(value, tag),
        Node::MixinDefinition(d) => {
            for p in d.params.iter_mut() {
                if let Some(def) = &mut p.default {
                    stamp_node(def, tag);
                }
            }
            stamp_urls(&mut d.rules, tag);
        }
        Node::MixinCall(c) => {
            for a in c.args.iter_mut() {
                stamp_node(&mut a.value, tag);
            }
        }
        Node::AtRule(a) => {
            if let AtRuleBlock::Rules(rules) = &mut a.block {
                stamp_urls(rules, tag);
            }
        }
        Node::Paren { inner, .. } | Node::Negative(inner) => stamp_node(inner, tag),
        Node::Assignment { value, .. } => stamp_node(value, tag),
        Node::Operation { left, right, .. } => {
            stamp_node(left, tag);
            stamp_node(right, tag);
        }
        _ => {}
    }
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
    /// full_path → normalized source (error locations/excerpts, §5.5).
    sources: FxHashMap<String, std::sync::Arc<str>>,
    /// less.js `recursionDetector` ∪ `files`: canonical paths already fetched.
    fetched: FxHashSet<String>,
    /// Pending file visits (locations of freshly attached `ImportResolved`
    /// subtrees), FIFO — the less.js sequencer's registration order (F13).
    queue: std::collections::VecDeque<(Loc, bool, usize)>,
}

impl<'a> ImportPass<'a> {
    /// The import-nesting depth cap (review F7): a `(multiple)` self-cycle
    /// otherwise grows the queue forever (less.js infinite-loops on the same
    /// input; the crate's contract is a clean error).
    fn depth_cap(&self) -> usize {
        self.opts.max_eval_depth.unwrap_or(64)
    }

    /// One FILE VISIT (less.js `ImportVisitor.run`/the subtree visit inside
    /// `onImported`): fetch every regular import of this file's tree in
    /// document order — descending into rulesets/at-rules, but NOT into
    /// freshly fetched subtrees, which are QUEUED as their own visits.
    /// `in_multiple` = an enclosing import was `(multiple)` — less.js sets
    /// `context.importMultiple` for the whole subtree, so nested imports skip
    /// the once-dedup too (verified: a twice-`(multiple)`-imported file
    /// re-emits its own once-imports each time).
    fn visit_list(
        &mut self,
        rules: &mut Vec<Node>,
        scope: &FileScope,
        in_multiple: bool,
        depth: usize,
        trail: &mut Loc,
    ) -> Result<(), LessError> {
        for (idx, node) in rules.iter_mut().enumerate() {
            // `//@magento_import` (plan §7.1): only ever parsed under
            // `magento_mode`; the resolver expands it to the concrete,
            // load-order-sorted `@import` list, spliced IN PLACE (a synthetic
            // container at the directive's position — position-preserving,
            // §7.3), each entry then fetched like a hand-written import.
            if let Node::MagentoImport { path, reference, span } = node {
                let raw = import_path_string(path).unwrap_or_default();
                let from = FileInfo {
                    filename: scope.filename.clone(),
                    current_directory: scope.current_directory.clone(),
                    rootpath: scope.rootpath.clone(),
                    ..Default::default()
                };
                let entries = self
                    .resolver
                    .magento_import(&raw, *reference, &from)
                    .map_err(|e| {
                        LessError::at(
                            ErrorKind::Import,
                            format!("//@magento_import '{raw}': {e}"),
                            scope.filename.clone(),
                            span.start,
                        )
                        .located(&scope.source)
                    })?;
                let span = *span;
                let inner: Vec<Node> = entries
                    .iter()
                    .map(|en| Node::Import {
                        path: Box::new(Node::Quoted {
                            escaped: false,
                            quote: '\'',
                            value: en.import_path.clone(),
                        }),
                        options: if en.reference {
                            vec!["reference".to_string()]
                        } else {
                            Vec::new()
                        },
                        features: None,
                        error: None,
                        span,
                    })
                    .collect();
                *node = Node::ImportResolved(Box::new(ImportResolved {
                    rules: inner,
                    // The container "is" the declaring file: errors while
                    // fetching the entries anchor at the directive's span in
                    // the declaring source.
                    source: scope.source.clone(),
                    inline: None,
                    full_path: scope.filename.clone(),
                    skip: false,
                    // Exempt the synthetic container from once-dedup (its
                    // path is the declaring file's, which is already in the
                    // fetched/once sets); the ENTRIES dedup normally.
                    multiple: true,
                    reference: false,
                    features: None,
                    current_directory: scope.current_directory.clone(),
                    rootpath: scope.rootpath.clone(),
                    layer_css: false,
                    path: None,
                    span,
                }));
                if let Node::ImportResolved(ir) = node {
                    if !ir.rules.is_empty() {
                        let mut loc = trail.clone();
                        loc.push((idx, 0));
                        // in_multiple stays FALSE: the entries' own imports
                        // dedup normally.
                        self.queue.push_back((loc, false, depth + 1));
                    }
                }
                continue;
            }
            if let Node::Import { path, options, .. } = node {
                if import_is_css(path, options) || is_variable_import(path) {
                    continue;
                }
                let Some(p) = import_path_string(path) else {
                    continue;
                };
                let resolved = self.fetch(&p, node, scope, in_multiple)?;
                *node = resolved;
                if let Node::ImportResolved(ir) = node {
                    if !ir.skip && ir.inline.is_none() && !ir.rules.is_empty() {
                        let mut loc = trail.clone();
                        loc.push((idx, 0));
                        self.queue.push_back((loc, ir.multiple, depth + 1));
                    }
                }
                continue;
            }
            // A pre-existing resolved subtree was already visited — skip.
            if matches!(node, Node::ImportResolved(_)) {
                continue;
            }
            for (li, list) in child_lists_mut(node).into_iter().enumerate() {
                trail.push((idx, li));
                self.visit_list(list, scope, in_multiple, depth, trail)?;
                trail.pop();
            }
        }
        Ok(())
    }

    /// Drain the pending file-visit queue FIFO (the BFS of F13). Import node
    /// replacement is 1:1, so queued locations stay valid across visits.
    fn drain_queue(&mut self, rules: &mut Vec<Node>) -> Result<(), LessError> {
        while let Some((loc, multiple, depth)) = self.queue.pop_front() {
            if depth > self.depth_cap() {
                return Err(LessError::new(
                    ErrorKind::Import,
                    "import recursion limit exceeded",
                ));
            }
            // Navigate to the resolved node.
            let mut list: &mut Vec<Node> = rules;
            for &(node_idx, list_idx) in &loc[..loc.len() - 1] {
                let node = &mut list[node_idx];
                let children = child_lists_mut(node);
                list = children.into_iter().nth(list_idx).expect("stable loc");
            }
            let node = &mut list[loc[loc.len() - 1].0];
            let Node::ImportResolved(ir) = node else { continue };
            let scope = FileScope {
                filename: ir.full_path.clone(),
                current_directory: ir.current_directory.clone(),
                rootpath: ir.rootpath.clone(),
                reference: ir.reference,
                source: ir.source.clone(),
            };
            let mut trail = loc.clone();
            self.visit_list(&mut ir.rules, &scope, multiple, depth, &mut trail)?;
        }
        Ok(())
    }

    /// Resolve one located variable-path import: interpolate its path with a
    /// frame stack built from the enclosing raw rule lists (innermost-first,
    /// with the ROOT frame replaced by the fixed `root_snapshot` — see
    /// [`resolve_imports`], review F4), then fetch and replace in place. An
    /// uninterpolatable path is marked CSS with the failure SAVED on the node:
    /// at eval the recomputed path decides — css-shaped re-emits literally,
    /// anything else rethrows the saved error (less.js `Import.eval`, F2).
    fn resolve_var_import(
        &mut self,
        rules: &mut Vec<Node>,
        loc: &Loc,
        entry: &FileScope,
        root_snapshot: Vec<Node>,
    ) -> Result<(), LessError> {
        // Collect the enclosing rule lists (outermost-first) + the owning file
        // scope by navigating the trail read-only.
        let mut frames_outer_first: Vec<Vec<Node>> = vec![root_snapshot];
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
                        source: ir.source.clone(),
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
            Ok(raw.clone())
        };
        let p = match interpolated {
            Ok(p) => p,
            Err(e) => {
                // less.js: a path that fails to evaluate at import time is
                // treated as css with the error SAVED on the node — the eval
                // pass rethrows it unless the re-evaluated path is css-shaped
                // (F2). Marking css also terminates the fixpoint.
                let mut opts2 = options.clone();
                opts2.push("css".to_string());
                let saved = e.message.clone();
                if let Node::Import { options, error, .. } = node {
                    *options = opts2;
                    *error = Some(saved);
                }
                return Ok(());
            }
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
        // Queue the fetched subtree's own regular imports (drained by the
        // caller before the next variable import).
        if let Node::ImportResolved(ir) = node {
            if !ir.skip && ir.inline.is_none() && !ir.rules.is_empty() {
                let mut l = loc.clone();
                let last = l.len() - 1;
                l[last] = (l[last].0, 0);
                self.queue.push_back((l, ir.multiple, 1));
            }
        }
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
            path,
            options,
            features,
            span,
            ..
        } = node
        else {
            unreachable!("fetch() is only called on Import nodes");
        };
        // less.js `layerCss` (§2.9, review F1): a LESS import whose feature
        // list is a single expression opening `layer(...)` re-emits as a
        // literal CSS `@import` at eval — but the file is still fetched and
        // its subtree visited (once-slots are consumed).
        let layer_css = features
            .as_deref()
            .map(|f| match f {
                Node::Anonymous(s) => is_layer_feature(s),
                _ => false,
            })
            .unwrap_or(false);
        let orig_path = if layer_css { Some(path.clone()) } else { None };
        let has = |o: &str| options.iter().any(|x| x == o);
        let inline = has("inline");
        let multiple = has("multiple") || in_multiple;
        let optional = has("optional");
        // `own_reference` = this statement says `(reference)`; `reference`
        // additionally inherits the enclosing file's reference-ness (for scope
        // propagation + the resolver hint). Only the DIRECT flag goes on the
        // node: inherited hiding is the enclosing import's visibility blanket
        // at eval time, so a mixin replayed from a visible call site emits
        // (less.js `_setVisibilityToReplacement`).
        let own_reference = has("reference");
        let reference = own_reference || scope.reference;

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
                    reference: own_reference,
                    features: features.clone(),
                    current_directory: scope.current_directory.clone(),
                    rootpath: scope.rootpath.clone(),
                    layer_css: false,
                    path: None,
                    span: *span,
                    source: std::sync::Arc::from(""),
                })))
            }
            Err(e) => {
                // less.js node file manager: `'<path>' wasn't found. Tried - <list>`
                // (FileError), anchored at the @import statement in the
                // importing file.
                let msg = match &e {
                    crate::resolver::ImportError::NotFound(tried) => {
                        format!("'{raw_path}' wasn't found. Tried - {tried}")
                    }
                    other => format!("'{raw_path}' wasn't found. {other}"),
                };
                return Err(
                    LessError::at(ErrorKind::Import, msg, scope.filename.clone(), span.start)
                        .located(&scope.source),
                );
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
            let entry_scope = FileScope::entry(self.opts);
            format!(
                "{}{}",
                entry_scope.rootpath,
                path_diff(&current_directory, &entry_scope.current_directory)
            )
        } else {
            scope.rootpath.clone()
        };

        match resolved.payload {
            ImportPayload::Inline(src) | ImportPayload::Css(src) if inline => {
                // less.js strips a UTF-8 BOM from every loaded file's
                // contents (import-manager.js:82) — inline payloads included
                // (review F14). CRLF is kept.
                let payload = src.strip_prefix('\u{feff}').unwrap_or(&src).to_string();
                Ok(Node::ImportResolved(Box::new(ImportResolved {
                    rules: Vec::new(),
                    inline: Some(payload),
                    full_path,
                    skip: false,
                    multiple,
                    reference: own_reference,
                    features: features.clone(),
                    current_directory,
                    rootpath,
                    layer_css: false,
                    path: None,
                    span: *span,
                    source: std::sync::Arc::from(""),
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
                        reference: own_reference,
                        features: features.clone(),
                        current_directory,
                        rootpath,
                        layer_css: false,
                        path: None,
                        span: *span,
                        source: std::sync::Arc::from(""),
                    })));
                }
                self.fetched.insert(full_path.clone());

                let file_source: std::sync::Arc<str>;
                let mut rules: Vec<Node> = if let Some(cached) = self.parsed.get(&full_path) {
                    file_source = self
                        .sources
                        .get(&full_path)
                        .cloned()
                        .unwrap_or_else(|| std::sync::Arc::from(""));
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
                            let mut rules = rules;
                            stamp_urls(
                                &mut rules,
                                &std::sync::Arc::new(crate::ast::FileTag {
                                    rootpath: rootpath.clone(),
                                    directory: current_directory.clone(),
                                }),
                            );
                            return self.finish_fetch(
                                rules,
                                full_path,
                                multiple,
                                own_reference,
                                features.clone(),
                                current_directory,
                                rootpath,
                                layer_css,
                                orig_path,
                                *span,
                                std::sync::Arc::from(""),
                            );
                        }
                    };
                    let file = FileInfo {
                        filename: full_path.clone(),
                        current_directory: current_directory.clone(),
                        ..Default::default()
                    };
                    // `(optional)` swallows ANY error of the target file —
                    // parse errors included (less.js import-manager.js:49,
                    // review F10) — turning it into an empty-rules skip.
                    let norm: std::sync::Arc<str> =
                        std::sync::Arc::from(crate::lex::normalize_source(&src).as_ref());
                    let parsed = match crate::parser::parse(&src, file, self.opts) {
                        Ok(p) => p,
                        Err(_) if optional => {
                            return Ok(Node::ImportResolved(Box::new(ImportResolved {
                                rules: Vec::new(),
                                inline: None,
                                full_path,
                                skip: true,
                                multiple,
                                reference: own_reference,
                                features: features.clone(),
                                current_directory,
                                rootpath,
                                layer_css: false,
                                path: None,
                                span: *span,
                                source: std::sync::Arc::from(""),
                            })))
                        }
                        Err(e) => return Err(e),
                    };
                    let rules = match parsed.as_ref() {
                        Node::Root(r) => r.clone(),
                        other => vec![other.clone()],
                    };
                    self.parsed.insert(full_path.clone(), rules.clone());
                    self.sources.insert(full_path.clone(), norm.clone());
                    file_source = norm;
                    rules
                };

                // Stamp this file's url/resource nodes with ITS file info
                // (§2.18, F3/F8) — per import statement, since rootpath can
                // differ per importer chain under rewriteUrls=off. The
                // subtree's own imports are QUEUED by the caller (BFS, F13),
                // not walked here.
                stamp_urls(
                    &mut rules,
                    &std::sync::Arc::new(crate::ast::FileTag {
                        rootpath: rootpath.clone(),
                        directory: current_directory.clone(),
                    }),
                );

                self.finish_fetch(
                    rules,
                    full_path,
                    multiple,
                    own_reference,
                    features.clone(),
                    current_directory,
                    rootpath,
                    layer_css,
                    orig_path,
                    *span,
                    file_source,
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
        layer_css: bool,
        path: Option<Box<Node>>,
        span: crate::ast::Span,
        source: std::sync::Arc<str>,
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
            layer_css,
            path,
            span,
            source,
        })))
    }
}

/// Whether an import's feature list is the single-expression `layer(...)`
/// form that takes less.js's `layerCss` literal re-emit path (review F1):
/// no top-level comma, and the first entry is `layer` followed by a paren
/// (a trailing feature like `layer(a) screen` still qualifies; a bare
/// `layer` keyword or a comma list does not).
fn is_layer_feature(feat: &str) -> bool {
    // Top-level comma → a feature LIST → @media wrap, not layerCss.
    let b = feat.as_bytes();
    let mut depth = 0i32;
    for &c in b {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => return false,
            _ => {}
        }
    }
    let t = feat.trim_start();
    match t.strip_prefix("layer") {
        Some(rest) => rest.trim_start().starts_with('('),
        None => false,
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
