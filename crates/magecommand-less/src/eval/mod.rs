//! The tree-rewriting evaluator (plan §4).
//!
//! Pipeline order is load-bearing (from less.js `transform-tree.js`):
//! import resolution → `root.eval` → JoinSelector → MarkVisibleSelectors →
//! Extend → ToCSS → genCSS. This step (STEP 4 / milestone 1) implements the core
//! LESS semantics: **lazy variable resolution** with the scope-chain model
//! (last-declaration-wins, forward references, child-scope override — the Magento
//! theming lever, §4.3), **nested-rule flattening + `&` parent-selector
//! resolution** (JoinSelector), **numeric/dimension/color operations** with unit
//! coercion and the `isMathOn` gate (§2.4), `@{}` interpolation, `@@` variable-
//! variables, and a minimal function/mixin surface needed by the VARIABLES /
//! NESTING / OPERATIONS gates. Mixins-with-guards, the full function library,
//! `@import` resolution, and `:extend` are later phases (plan §6).
//!
//! For cohesion the passes share one [`Ctx`] and are co-located here (the sibling
//! `context`/`scope`/`operation`/… modules keep the plan's ownership docs).

pub mod context;
pub mod extend;
pub mod import;
pub mod merge;
pub mod mixin;
pub mod operation;
pub mod scope;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{AtRuleBlock, Declaration, Element, MixinArg, MixinParam, Node, Selector, Span};
use self::import::FileScope;
use crate::color::Color;
use crate::css::{render_value, Css, Warning};
use crate::error::{ErrorKind, LessError};
use crate::functions;
use crate::options::{LessOptions, MathMode};
use crate::resolver::ImportResolver;
use crate::value::Dimension;

/// A scope frame: the (unevaluated) rule list of an entered ruleset. Variable and
/// mixin lookup scan it directly (last-declaration-wins), so lazy eval + forward
/// references fall out (plan §4.1). Wrapped in a `RefCell` so a mixin call can
/// **inject** its returned variables/mixins/rulesets back into the caller's frame
/// (scope-injection, plan §2.5) — mirroring less.js's in-place `rsRules` splice.
type Frame = Rc<RefCell<Vec<Node>>>;

/// Build a frame from an owned rule list.
fn frame_of(rules: Vec<Node>) -> Frame {
    Rc::new(RefCell::new(rules))
}

/// Deduplicate an assembled call-frame stack by `Rc` identity, keeping the
/// FIRST occurrence. Lookup is innermost-first first-match, so a later
/// duplicate of an earlier frame is unreachable and dropping it cannot change
/// resolution. Without this, a recursive mixin call DOUBLES the frame list per
/// level (its def-scope is a suffix of the current stack and the whole caller
/// stack is appended after it) — exponential memory that OOMs long before the
/// [`MAX_MIXIN_DEPTH`] cap fires. less.js grows linearly here because a
/// `MixinDefinition`'s `frames` snapshot is captured once at definition
/// evaluation (constant size); deduping restores that linear growth.
fn dedup_frames(frames: Vec<Frame>) -> Vec<Frame> {
    let mut seen: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    frames
        .into_iter()
        .filter(|f| seen.insert(Rc::as_ptr(f) as usize))
        .collect()
}

/// The evaluator context (plan §4.1/§4.2): innermost-first frame stack, math
/// state, the parens stack for `isMathOn`, and the `importantScope` stack.
pub struct Ctx<'a> {
    frames: Vec<Frame>,
    opts: &'a LessOptions,
    resolver: &'a dyn ImportResolver,
    math: MathMode,
    math_on: bool,
    parens: usize,
    in_calc: bool,
    important_scope: Vec<Option<String>>,
    evaluating: Vec<String>,
    mixin_depth: usize,
    /// The `default()` guard-function value during the two-subpass mixin-guard
    /// evaluation (plan §2.6). `None` outside a default-classification pass.
    default_value: Option<bool>,
    /// Spans of ruleset-as-mixin bodies currently on the eval stack — the on-stack
    /// identity recursion guard (plan §2.5; MixinDefinitions are exempt).
    active_rulesets: Vec<Span>,
    /// Captured-frame side table for scope-injected closures (plan §4.3): a
    /// `Node::Closure`'s `scope` field indexes this. Kept off `Node` so the AST
    /// stays `Send + Sync`; frames are cheap `Rc` clones frozen at injection.
    closures: Vec<Vec<Frame>>,
    /// `$prop` access events awaiting the less.js `parseValue` important-trim
    /// side effect (plan §2.12 quirk): when a property accessor reads `name` in
    /// `frame` (keyed by `Rc` pointer), every matching declaration *already
    /// emitted* in that frame's block re-renders its `!important` without the
    /// leading space (less.js mutates the evaluated declaration in place).
    pending_trims: Vec<(usize, String)>,
    /// The enclosing nestable at-rules within the current at-rule boundary
    /// (less.js `context.mediaPath`) — drives nested `@media`/`@container`
    /// feature merging (plan §2.13).
    media_path: Vec<MediaFrame>,
    /// The bubbled blocks collected for the current outermost nestable at-rule
    /// (less.js `context.mediaBlocks`), in depth-first entry order; `None`
    /// entries are pruned empties. `None` = no collection in flight.
    media_blocks: Option<Vec<Option<Out>>>,
    /// Canonical paths whose import already emitted (the less.js
    /// `onceFileDetectionMap` skip-closure: first EVAL wins; entry pre-seeded).
    once_imported: rustc_hash::FxHashSet<String>,
    /// The stack of files whose rules are being evaluated (entry at the
    /// bottom) — the base for url rewriting (§2.18) and resource reads.
    file_stack: Vec<FileScope>,
    /// Depth of enclosing `(reference)`-imported bodies (§2.8): output
    /// produced while >0 is visibility-blocked until referenced.
    visibility_blocks: usize,
    warnings: Vec<Warning>,
}

/// A resolved mixin candidate (a definition or ruleset reachable by the call path)
/// together with the definition-scope frames captured for closure semantics.
struct Candidate {
    name: String,
    params: Vec<MixinParam>,
    guard: Option<Node>,
    rules: Vec<Node>,
    def_scope: Vec<Frame>,
    /// Guards of the namespace segments traversed to reach this candidate
    /// (`#ns when (…) > .m()`). less.js `calcDefGroup` AND-evaluates every
    /// `namespace.matchCondition(null)` with the mixin's own guard (§2.6), so a
    /// false namespace guard excludes the mixin even though its args match.
    path_guards: Vec<Node>,
    /// `Some(span)` for a ruleset-as-mixin (subject to the recursion guard);
    /// `None` for a `MixinDefinition` (exempt).
    ruleset_span: Option<Span>,
}

/// An evaluated call argument: optional name (`@x:`) + its value.
#[derive(Clone)]
struct EvArg {
    name: Option<String>,
    value: Node,
}

/// Default eval-depth cap (mixin/detached-ruleset/import recursion), the
/// eval-side runaway guard (plan §2.5). less.js has no explicit cap — a
/// runaway recursion there dies on the JS call stack ("Maximum call stack size
/// exceeded", observed between depth 1000 and 2000 on node 22); we error
/// cleanly instead. 64 stays well inside a 2 MiB test-thread native stack in
/// debug builds (~15 KiB/level measured) and far above any fixture's
/// legitimate recursion. Overridable via [`LessOptions::max_eval_depth`].
const MAX_MIXIN_DEPTH: usize = 64;

/// One output selector path (post JoinSelector) with its `:extend`/visibility
/// state (plan §2.8): `visible` = renders (false inside `(reference)` imports
/// until an extend re-enables a graft); `has_extend` = this path carries an
/// extend, so other extends never match it (less.js `extendList` skip).
#[derive(Clone)]
struct OutSel {
    css: String,
    visible: bool,
    has_extend: bool,
}

impl OutSel {
    fn plain(css: String) -> OutSel {
        OutSel { css, visible: true, has_extend: false }
    }
}

fn plain_sels(paths: Vec<String>) -> Vec<OutSel> {
    paths.into_iter().map(OutSel::plain).collect()
}

/// One flattened output node (post JoinSelector). genCSS emits these at top level.
enum Out {
    Rule {
        selectors: Vec<OutSel>,
        decls: Vec<Node>,
        /// The `:extend`s hanging off this rule's paths (plan §2.8), in
        /// less.js finder order (path-major, then per-path clause order).
        extends: Vec<extend::EvExtend>,
        /// A body `&:extend(…);` was present — every path skips matching
        /// (less.js `extendOnEveryPath`).
        extend_on_every_path: bool,
    },
    At {
        header: String,
        body: AtBody,
    },
    /// A bare run of declarations at the current indent (an at-rule's own
    /// declarations, e.g. `@font-face`/`@page` bodies interleaved with nested
    /// at-rules).
    Decls(Vec<Node>),
    Comment(String),
    /// An at-rule that renders INSIDE the enclosing rule's declaration block
    /// (the less.js `simpleBlock` form — `@starting-style { decls }`, §2.13).
    /// The node is an evaluated [`Node::AtRule`] holding declaration rules.
    Nested(Node),
    /// A verbatim run (an `(inline)` import's payload, §2.9) — emitted raw.
    Verbatim(String),
    /// A visibility-blocked block (`(reference)` import output, §2.8): kept in
    /// the tree so `:extend` can re-enable matched selectors, but not rendered.
    Hidden(Box<Out>),
}

/// One enclosing nestable at-rule on the bubbling path (less.js
/// `context.mediaPath`): its kind (`@media`/`@container`) and its evaluated
/// comma-separated feature queries.
struct MediaFrame {
    kind: String,
    features: Vec<String>,
}

enum AtBody {
    None,
    Rules(Vec<Out>),
    /// A verbatim body — a feature-carrying `(inline)` import's payload
    /// wrapped in its `@media` shell (§2.9).
    Verbatim(String),
}

/// Evaluate a parsed AST to [`Css`] (plan §9.5).
pub fn eval(
    root: &Arc<Node>,
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
) -> Result<Css, LessError> {
    let mut rules = match root.as_ref() {
        Node::Root(r) => r.clone(),
        // A passthrough anonymous root (scaffold callers) — emit verbatim.
        Node::Anonymous(text) => return Ok(Css::from_code(text.clone())),
        other => vec![other.clone()],
    };

    // Stage 1 of the two-stage `@import` (plan §2.9): fetch + parse every
    // non-CSS import up front, attaching each file's rules at the import's
    // source position (`Node::ImportResolved`). Stage 2 (eval, below) splices
    // them position-preservingly.
    if opts.process_imports {
        import::resolve_imports(&mut rules, opts, resolver)?;
    }

    let mut ctx = Ctx::new(opts, resolver);

    // globalVars / modifyVars are prepended/appended rulesets (plan §2.0). Their
    // implementation is deferred; the default harness passes none.
    let mut outs: Vec<Out> = Vec::new();
    ctx.push_frame(frame_of(rules.clone()));
    let (own, children) = ctx.process_body(&rules, None)?;
    // less.js to-css-visitor `checkValidNodes` (firstRoot): a declaration at
    // the stylesheet root is a hard error, never silently dropped (F11).
    if own.iter().any(|n| matches!(n, Node::Declaration(_))) {
        return Err(LessError::new(
            ErrorKind::Syntax,
            "Properties must be inside selector blocks. They cannot be in the root",
        ));
    }
    outs.extend(children);
    ctx.pop_frame();

    // The extend pass (plan §2.8): finder → chaining fixpoint → replace, then
    // `(reference)` visibility resolution — runs on the flattened output tree
    // (post eval + join + visibility marking), before output ordering.
    extend::apply(&mut outs, &mut ctx.warnings)?;

    // Output ordering — the less.js `Ruleset.genCSS` root splice (§2.13/§C):
    // `@charset`s go to the very top; `@import`s float up to just after any
    // LEADING run of comments/charsets/imports (a comment elsewhere stays with
    // its rules); everything else keeps source order. Ported exactly —
    // `importNodeIndex` advances past a comment only while it is still the
    // import insertion point.
    {
        let mut ordered: Vec<Out> = Vec::with_capacity(outs.len());
        let mut charset_idx = 0usize;
        let mut import_idx = 0usize;
        for (i, o) in outs.into_iter().enumerate() {
            match &o {
                Out::Comment(_) => {
                    if import_idx == i {
                        import_idx += 1;
                    }
                    ordered.push(o);
                }
                Out::At { header, .. } if header.starts_with("@charset") => {
                    ordered.insert(charset_idx, o);
                    charset_idx += 1;
                    import_idx += 1;
                }
                Out::At { header, .. } if header.starts_with("@import") => {
                    ordered.insert(import_idx, o);
                    import_idx += 1;
                }
                _ => ordered.push(o),
            }
        }
        outs = ordered;
    }
    // Only the FIRST `@charset` survives (less.js `visitAtRuleWithoutBody`).
    let mut seen_charset = false;
    outs.retain(|o| match o {
        Out::At { header, .. } if header.starts_with("@charset") => {
            !std::mem::replace(&mut seen_charset, true)
        }
        _ => true,
    });

    let code = render_all(&outs, opts.num_precision);
    Ok(Css {
        code,
        imports: Vec::new(),
        source_map: None,
        warnings: ctx.warnings,
    })
}

impl<'a> Ctx<'a> {
    /// A fresh evaluation context over `opts` + `resolver`.
    fn new(opts: &'a LessOptions, resolver: &'a dyn ImportResolver) -> Ctx<'a> {
        let entry = FileScope::entry(opts);
        let mut once_imported = rustc_hash::FxHashSet::default();
        if !entry.filename.is_empty() {
            once_imported.insert(entry.filename.clone());
        }
        Ctx {
            frames: Vec::new(),
            opts,
            resolver,
            math: opts.math,
            math_on: true,
            parens: 0,
            in_calc: false,
            important_scope: Vec::new(),
            evaluating: Vec::new(),
            mixin_depth: 0,
            default_value: None,
            active_rulesets: Vec::new(),
            closures: Vec::new(),
            pending_trims: Vec::new(),
            media_path: Vec::new(),
            media_blocks: None,
            once_imported,
            file_stack: vec![entry],
            visibility_blocks: 0,
            warnings: Vec::new(),
        }
    }

    fn push_frame(&mut self, f: Frame) {
        self.frames.insert(0, f);
    }
    fn pop_frame(&mut self) {
        self.frames.remove(0);
    }

    fn err(&self, kind: ErrorKind, msg: impl Into<String>) -> LessError {
        LessError::new(kind, msg)
    }

    /// The effective eval-depth cap (plan §2.5): the option when set, else
    /// [`MAX_MIXIN_DEPTH`].
    fn max_eval_depth(&self) -> usize {
        self.opts.max_eval_depth.unwrap_or(MAX_MIXIN_DEPTH)
    }

    /// The entry file's directory (for the resource functions' relative reads).
    fn current_dir(&self) -> String {
        let f = self.opts.filename.as_deref().unwrap_or("");
        match f.rfind('/') {
            Some(i) => f[..=i].to_string(),
            None => String::new(),
        }
    }

    /// Whether a variable is defined in any live frame (for `isdefined`).
    fn lookup_defined(&self, name: &str) -> bool {
        let key = name.trim_start_matches('@');
        self.frames.iter().any(|f| frame_has_var(f, key))
    }

    // ------------------------------------------------------------------
    // Body processing: split a rule list into (own declarations, child output)
    // ------------------------------------------------------------------

    /// Evaluate a rule list. `self_paths` = the absolute joined selectors of the
    /// enclosing ruleset (None at root). Returns `(own_declarations, children)`:
    /// declarations belong to this ruleset's own block, children are flattened
    /// output blocks (nested rulesets/at-rules) to emit afterward.
    fn process_body(
        &mut self,
        rules: &[Node],
        self_paths: Option<&[String]>,
    ) -> Result<(Vec<Node>, Vec<Out>), LessError> {
        let mut own: Vec<Node> = Vec::new();
        let mut children: Vec<Out> = Vec::new();
        self.eval_rules(rules, self_paths, &mut own, &mut children)?;
        // Drop trim events targeting frames that are no longer on the stack
        // (guard-only frames never run a rule pass; an `Rc` pointer could be
        // reused, so stale events must not linger).
        if !self.pending_trims.is_empty() {
            let live: Vec<usize> = self
                .frames
                .iter()
                .map(|f| Rc::as_ptr(f) as *const () as usize)
                .collect();
            self.pending_trims.retain(|(fp, _)| live.contains(fp));
        }
        Ok((own, children))
    }

    /// Evaluate `rules`, appending declarations to `own` and nested output blocks
    /// to `children`. Shared by rulesets and mixin-injected bodies.
    ///
    /// Two passes, mirroring less.js `Ruleset.eval` (plan §4.2): **pass 1**
    /// evaluates every mixin call, splicing its returned variables/mixins/rulesets
    /// into the current frame (scope-injection, §2.5) so later declarations — even
    /// ones *earlier* in source, via lazy resolution — can see them; **pass 2**
    /// emits declarations, rulesets and at-rules in source order, replaying each
    /// mixin call's pre-computed output at its position.
    fn eval_rules(
        &mut self,
        rules: &[Node],
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<(), LessError> {
        // Pass 0: eagerly capture detached-ruleset literal values in the current
        // frame (less.js evaluates every declaration during `Ruleset.eval`, so a
        // DR value's `frames` snapshot is the DEFINING scope — the lazy lookup
        // must not re-capture at the call site; plan §2.11).
        if let Some(frame) = self.frames.first().cloned() {
            let needs: Vec<usize> = frame
                .borrow()
                .iter()
                .enumerate()
                .filter_map(|(i, r)| match r {
                    Node::VariableDecl { value, .. }
                        if matches!(value.as_ref(), Node::DetachedRuleset { .. }) =>
                    {
                        Some(i)
                    }
                    _ => None,
                })
                .collect();
            if !needs.is_empty() {
                let scope = self.closures.len() as u64;
                self.closures.push(self.frames.clone());
                let mut fm = frame.borrow_mut();
                for i in needs {
                    if let Node::VariableDecl { value, .. } = &mut fm[i] {
                        let inner = value.clone();
                        *value = Box::new(Node::Closure { inner, scope });
                    }
                }
            }
        }

        // Pass 1: expand mixin + detached-ruleset calls, splicing their returned
        // scope (variables/mixins/rulesets — and declaration clones, which the
        // `$prop` accessor reads) into the current frame AT THE CALL POSITION
        // (less.js splices `rsRules`, so relative order vs later declarations is
        // what makes `$color` last-wins correct); memoize output per position.
        let mut expansions: Vec<Option<(Vec<Node>, Vec<Out>)>> = Vec::with_capacity(rules.len());
        let mut inserted = 0usize;
        for (idx, rule) in rules.iter().enumerate() {
            // A mixin/DR call body evaluates with a FRESH media context
            // (less.js `contexts.Eval` copies neither mediaPath nor mediaBlocks
            // into the call's context): its @media blocks materialize
            // standalone, and are RE-merged against the ambient media path at
            // the call's source position in pass 2 (`absorb_expansion_outs`) —
            // which is what keeps sibling-media output in source order.
            let expansion = match rule {
                Node::MixinCall(call) => {
                    let saved_path = std::mem::take(&mut self.media_path);
                    let saved_blocks = self.media_blocks.take();
                    let mut ex_own = Vec::new();
                    let mut ex_children = Vec::new();
                    let injected =
                        self.expand_mixin_call(call, self_paths, &mut ex_own, &mut ex_children);
                    self.media_path = saved_path;
                    self.media_blocks = saved_blocks;
                    Some((injected?, ex_own, ex_children, true))
                }
                Node::VariableCall { name, span } => {
                    let saved_path = std::mem::take(&mut self.media_path);
                    let saved_blocks = self.media_blocks.take();
                    let mut ex_own = Vec::new();
                    let mut ex_children = Vec::new();
                    let injected = self.expand_variable_call(
                        name,
                        *span,
                        self_paths,
                        &mut ex_own,
                        &mut ex_children,
                    );
                    self.media_path = saved_path;
                    self.media_blocks = saved_blocks;
                    Some((injected?, ex_own, ex_children, false))
                }
                Node::ImportResolved(ir) => {
                    // `strictImports` (§2.9): imports inside a plain ruleset
                    // are not evaluated (less.js `Ruleset.eval` gates
                    // `evalImports` on root/allowImports) — root and at-rule
                    // bodies still import.
                    if self.opts.strict_imports && self_paths.is_some() {
                        expansions.push(None);
                        continue;
                    }
                    // Stage 2 of §2.9: the pre-fetched file's rules evaluate at
                    // this position and splice into the caller's scope, with
                    // the same fresh-media-context + re-merge treatment as a
                    // mixin call.
                    let saved_path = std::mem::take(&mut self.media_path);
                    let saved_blocks = self.media_blocks.take();
                    let mut ex_own = Vec::new();
                    let mut ex_children = Vec::new();
                    let injected =
                        self.expand_resolved_import(ir, self_paths, &mut ex_own, &mut ex_children);
                    self.media_path = saved_path;
                    self.media_blocks = saved_blocks;
                    Some((injected?, ex_own, ex_children, true))
                }
                _ => None,
            };
            match expansion {
                Some((injected, ex_own, ex_children, allow_vars)) => {
                    if let Some(frame) = self.frames.first().cloned() {
                        let mut to_insert: Vec<Node> = Vec::new();
                        for node in injected {
                            let keep = match &node {
                                // A mixin-returned variable only lands if the
                                // caller doesn't declare it; a DR call NEVER
                                // returns variables (less.js `VariableCall`
                                // splice filter, plan §2.11).
                                Node::VariableDecl { name, .. } => {
                                    allow_vars && !frame_has_var(&frame, name)
                                }
                                _ => true,
                            };
                            if keep {
                                to_insert.push(node);
                            }
                        }
                        for d in &ex_own {
                            if matches!(d, Node::Declaration(_)) {
                                to_insert.push(d.clone());
                            }
                        }
                        if !to_insert.is_empty() {
                            let mut fm = frame.borrow_mut();
                            let at = (idx + inserted + 1).min(fm.len());
                            inserted += to_insert.len();
                            fm.splice(at..at, to_insert);
                        }
                    }
                    expansions.push(Some((ex_own, ex_children)));
                }
                None => expansions.push(None),
            }
        }

        // Pass 2: source-order output.
        for (idx, rule) in rules.iter().enumerate() {
            match rule {
                Node::MixinCall(_) | Node::VariableCall { .. } => {
                    if let Some((ex_own, ex_children)) = expansions[idx].take() {
                        own.extend(ex_own);
                        self.absorb_expansion_outs(ex_children, children);
                    }
                }
                Node::VariableDecl { .. }
                | Node::MixinDefinition(_)
                | Node::Comment { line: true, .. }
                | Node::MagentoImport { .. } => {
                    // No direct output (declarations register in the frame; the
                    // magento directive is a later phase).
                }
                Node::ImportResolved(_) => {
                    // The pre-fetched import was expanded in pass 1 — replay its
                    // output here (source position).
                    if let Some((ex_own, ex_children)) = expansions[idx].take() {
                        own.extend(ex_own);
                        self.absorb_expansion_outs(ex_children, children);
                    }
                }
                Node::Import {
                    path, features, options, ..
                } => {
                    // With import processing disabled, a LESS import (by
                    // option or less.js's `[#.&?]css([?;].*)?$` path test) has
                    // no fetched root and evaluates to nothing (less.js
                    // `Import.eval` without an ImportVisitor pass) — only CSS
                    // imports re-emit (process-imports/google).
                    if !self.opts.process_imports {
                        let has = |o: &str| options.iter().any(|x| x == o);
                        let raw = import_path_text(path);
                        let is_css = has("css")
                            || (!has("less") && !has("inline") && import_path_is_css(&raw));
                        if !is_css {
                            continue;
                        }
                    }
                    // A CSS/`url()` import re-emits as a literal `@import`
                    // at-rule, with the path rewritten per §2.18 (less.js
                    // `Import.evalPath`).
                    let evaled = self.eval_value(path)?;
                    let rewritten = self.rewrite_import_path(evaled);
                    let ps = render_value(&rewritten, self.opts.num_precision);
                    let mut header = format!("@import {ps}");
                    if let Some(f) = features {
                        let fs = self.eval_media_features(f)?.join(", ");
                        if !fs.is_empty() {
                            header.push(' ');
                            header.push_str(&fs);
                        }
                    }
                    let out = Out::At {
                        header,
                        body: AtBody::None,
                    };
                    if self_paths.is_some() {
                        // A css import inside a ruleset stays nested in its
                        // declaration block (verified against 4.6.7).
                        own.push(Node::AtRule(crate::ast::AtRule {
                            name: "@import".to_string(),
                            prelude: match out {
                                Out::At { header, .. } => Some(Box::new(Node::Anonymous(
                                    header["@import ".len()..].to_string(),
                                ))),
                                _ => None,
                            },
                            block: AtRuleBlock::None,
                            span: Default::default(),
                        }));
                    } else {
                        children.push(out);
                    }
                }
                Node::Comment { line: false, text, .. } => {
                    if self_paths.is_none() {
                        // A top-level block comment is its own output node.
                        children.push(Out::Comment(text.clone()));
                    } else {
                        own.push(Node::Comment {
                            text: text.clone(),
                            line: false,
                            span: Default::default(),
                        });
                    }
                }
                Node::Declaration(d) => {
                    let evaled = self.eval_declaration(d)?;
                    own.push(evaled);
                }
                // A body `&:extend(…);` — passed up through `own` (also across
                // mixin splices) to the enclosing ruleset, which converts it
                // (plan §2.8). At the stylesheet root it has no paths → dropped.
                Node::ExtendRule(_) => own.push(rule.clone()),
                Node::Ruleset(rs) => {
                    // A single bare-`&` child ruleset (`& when (…)`, `& { … }`) is
                    // **folded** into the parent: its own declarations join the
                    // parent's block in source position (plan §2.2/§4.2).
                    // Track this ruleset as on-stack so a mixin call inside it
                    // resolving back to it is skipped (recursion guard, §2.5).
                    self.active_rulesets.push(rs.span);
                    let r = if rs.selectors.len() == 1 && is_just_parent(&rs.selectors[0]) {
                        let guard_ok = match &rs.selectors[0].guard {
                            Some(g) => self.eval_guard(g),
                            None => Ok(true),
                        };
                        match guard_ok {
                            Ok(true) => {
                                self.push_frame(frame_of(rs.rules.clone()));
                                let r = self.process_body(&rs.rules, self_paths);
                                self.pop_frame();
                                r.map(|(fold_own, fold_children)| {
                                    own.extend(fold_own);
                                    children.extend(fold_children);
                                })
                            }
                            Ok(false) => Ok(()),
                            Err(e) => Err(e),
                        }
                    } else {
                        self.eval_nested_ruleset(&rs.selectors, &rs.rules, self_paths, children)
                    };
                    self.active_rulesets.pop();
                    r?;
                }
                Node::AtRule(a) => {
                    if matches!(a.block, AtRuleBlock::None) && self_paths.is_some() {
                        // A no-block directive inside a ruleset (e.g. `@apply …;`)
                        // stays inline in the declaration block.
                        let prelude = match &a.prelude {
                            Some(p) => Some(self.eval_prelude(p)?),
                            None => None,
                        };
                        own.push(Node::AtRule(crate::ast::AtRule {
                            name: a.name.clone(),
                            prelude: prelude.map(|s| Box::new(Node::Anonymous(s))),
                            block: AtRuleBlock::None,
                            span: Default::default(),
                        }));
                    } else {
                        for out in self.eval_at_rule(a, self_paths)? {
                            match out {
                                // A simpleBlock at-rule renders inside the
                                // enclosing rule's declaration block (§2.13).
                                Out::Nested(node) => own.push(node),
                                other => children.push(other),
                            }
                        }
                    }
                }
                Node::DetachedRuleset { .. } => {}
                Node::Call { name, args } if name.eq_ignore_ascii_case("each") => {
                    self.expand_each(args, self_paths, own, children)?;
                }
                Node::Call { name, args } => {
                    // A statement-level function call (less.js `primary` →
                    // `entities.call()`, e.g. css-escapes' root `e('…');`): the
                    // evaluated result is emitted verbatim at this position.
                    let v = self.eval_call(name, args)?;
                    let text = render_value(&v, self.opts.num_precision);
                    if !text.is_empty() {
                        if self_paths.is_none() {
                            children.push(Out::Comment(text));
                        } else {
                            own.push(Node::Comment {
                                text,
                                line: false,
                                span: Default::default(),
                            });
                        }
                    }
                }
                // Value nodes never appear as statements.
                _ => {}
            }
            self.drain_trims(own);
        }
        self.drain_trims(own);
        Ok(())
    }

    /// Route a mixin/DR expansion's output blocks into the caller (the less.js
    /// re-evaluation of spliced rules, §2.13): a standalone `@media`/
    /// `@container` block re-merges with the ambient media path — entering the
    /// ambient collector at THIS source position — while everything else (and a
    /// mixed-kind block) passes through unchanged.
    fn absorb_expansion_outs(&mut self, outs: Vec<Out>, children: &mut Vec<Out>) {
        for out in outs {
            match out {
                Out::At { header, body } => {
                    let (name, feats) = match header.split_once(' ') {
                        Some((n, rest)) => (
                            n.to_string(),
                            split_top(rest, ',')
                                .into_iter()
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>(),
                        ),
                        None => (header.clone(), Vec::new()),
                    };
                    let base = base_at_name(&name);
                    let nestable = matches!(base.as_str(), "@media" | "@container");
                    let same_kind =
                        nestable && self.media_path.iter().all(|m| m.kind == base);
                    if nestable && !self.media_path.is_empty() && same_kind {
                        let mut lists: Vec<Vec<String>> =
                            self.media_path.iter().map(|m| m.features.clone()).collect();
                        lists.push(feats);
                        let merged = permute_feature_paths(&lists);
                        let merged_header = if merged.is_empty() {
                            name
                        } else {
                            format!("{} {}", name, merged.join(", "))
                        };
                        self.media_blocks
                            .get_or_insert_with(Vec::new)
                            .push(Some(Out::At {
                                header: merged_header,
                                body,
                            }));
                    } else {
                        children.push(Out::At { header, body });
                    }
                }
                other => children.push(other),
            }
        }
    }

    /// Apply pending `$prop` important-trim events targeting the CURRENT frame
    /// to the declarations already emitted in `own` (see `pending_trims`): only
    /// declarations evaluated *before* the access mirror less.js's in-place
    /// mutation; later ones re-normalize at their own eval.
    fn drain_trims(&mut self, own: &mut [Node]) {
        if self.pending_trims.is_empty() {
            return;
        }
        let Some(frame) = self.frames.first() else { return };
        let fp = Rc::as_ptr(frame) as *const () as usize;
        let mut i = 0;
        while i < self.pending_trims.len() {
            if self.pending_trims[i].0 == fp {
                let (_, name) = self.pending_trims.remove(i);
                for d in own.iter_mut() {
                    if let Node::Declaration(dd) = d {
                        if dd.name == name {
                            // The parseValue re-parse: split an Anonymous raw
                            // capture's `!important`, and store the flag
                            // WITHOUT the leading space (less.js writes the
                            // parsed match straight onto the field).
                            split_anon_important(dd);
                            if dd.important.starts_with(' ') {
                                dd.important = dd.important.trim_start().to_string();
                            }
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    /// Evaluate a nested ruleset: join its selectors with the parent, then emit
    /// its own declaration block + its own nested children (JoinSelector, §4).
    fn eval_nested_ruleset(
        &mut self,
        selectors: &[Selector],
        rules: &[Node],
        parent_paths: Option<&[String]>,
        out: &mut Vec<Out>,
    ) -> Result<(), LessError> {
        // Evaluate selectors (guards + interpolation), collect surviving selector
        // strings, then join with the parent context.
        let mut own_sel: Vec<String> = Vec::new();
        let mut sel_extends: Vec<Vec<crate::ast::ExtendTarget>> = Vec::new();
        for sel in selectors {
            if let Some(g) = &sel.guard {
                if !self.eval_guard(g)? {
                    return Ok(()); // guard failed → ruleset emits nothing
                }
            }
            let interpolated = sel
                .elements
                .iter()
                .any(|e| e.value.contains("@{") || e.value.contains("${"));
            let rendered = self.render_selector(sel)?;
            if interpolated && rendered.contains(',') {
                // less.js RE-PARSES interpolated selectors — a comma list in
                // the variable expands to a selector GROUP
                // (`@{inputs} { … }`, parse-interpolation).
                for part in split_top(&rendered, ',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    own_sel.push(part.to_string());
                    sel_extends.push(sel.extend_list.clone());
                }
            } else {
                own_sel.push(rendered);
                sel_extends.push(sel.extend_list.clone());
            }
        }
        let joined = join_selectors(parent_paths, &own_sel);

        self.push_frame(frame_of(rules.to_vec()));
        let (mut decls, children) = self.process_body(rules, Some(&joined))?;
        self.pop_frame();

        // Body `&:extend(…);` statements apply to EVERY path of this ruleset
        // (less.js `extendOnEveryPath`); selector-attached clauses only to the
        // paths of their own selector. Finder order: path-major, per-path the
        // selector's own clauses first, then the body ones (plan §2.8).
        let mut body_targets: Vec<crate::ast::ExtendTarget> = Vec::new();
        decls.retain(|d| match d {
            Node::ExtendRule(ts) => {
                body_targets.extend(ts.iter().cloned());
                false
            }
            _ => true,
        });
        let visible = self.visibility_blocks == 0;
        let per_own = if own_sel.is_empty() { 1 } else { joined.len() / own_sel.len() };
        let mut extends: Vec<extend::EvExtend> = Vec::new();
        let mut osels: Vec<OutSel> = Vec::with_capacity(joined.len());
        for (pi, path) in joined.iter().enumerate() {
            let si = (pi / per_own.max(1)).min(sel_extends.len().saturating_sub(1));
            let own_list = sel_extends.get(si).map(|v| v.as_slice()).unwrap_or(&[]);
            for (j, t) in own_list.iter().chain(body_targets.iter()).enumerate() {
                extends.push(extend::EvExtend {
                    self_sel: path.clone(),
                    target_css: self.render_extend_target(&t.elements)?,
                    all: t.all,
                    visible,
                    first_on_path: j == 0,
                });
            }
            osels.push(OutSel {
                css: path.clone(),
                visible: true,
                has_extend: !body_targets.is_empty() || !own_list.is_empty(),
            });
        }

        if has_visible(&decls) || !extends.is_empty() {
            out.push(Out::Rule {
                selectors: osels,
                decls,
                extends,
                extend_on_every_path: !body_targets.is_empty(),
            });
        }
        out.extend(children);
        Ok(())
    }

    /// Render one `:extend` target's elements to a matchable selector string
    /// (interpolation evaluated, like [`Self::render_selector`]).
    fn render_extend_target(&mut self, els: &[Element]) -> Result<String, LessError> {
        let mut s = String::new();
        for el in els {
            s.push_str(&combinator_css(&el.combinator));
            if el.value.contains("@{") || el.value.contains("${") {
                s.push_str(&self.interpolate_css(&el.value)?);
            } else {
                s.push_str(&el.value);
            }
        }
        Ok(s.trim_start().to_string())
    }

    fn eval_at_rule(
        &mut self,
        a: &crate::ast::AtRule,
        parent_paths: Option<&[String]>,
    ) -> Result<Vec<Out>, LessError> {
        // `@plugin` is a JS-plugin load in less.js (an isPlugin Import) — it
        // never reaches the output. Plugin EXECUTION is out of scope (§8); the
        // statement itself is parse-and-drop.
        if a.name == "@plugin" {
            return Ok(Vec::new());
        }
        let base = base_at_name(&a.name);
        if matches!(base.as_str(), "@media" | "@container")
            && matches!(a.block, AtRuleBlock::Rules(_))
        {
            return self.eval_nestable_at_rule(a, &base, parent_paths);
        }
        let prelude = match &a.prelude {
            Some(p) => Some(self.eval_prelude(p)?),
            None => None,
        };
        let header = match &prelude {
            Some(p) if !p.is_empty() => format!("{} {}", a.name, p),
            _ => a.name.clone(),
        };
        match &a.block {
            AtRuleBlock::None => Ok(vec![Out::At {
                header,
                body: AtBody::None,
            }]),
            AtRuleBlock::Rules(rules) => {
                // less.js `isRooted` (parser directive table, §2.13): a non-rooted
                // at-rule (@supports/@document/@starting-style/@layer) carries the
                // enclosing selector into its body — bare declarations wrap in the
                // parent rule; a rooted one (@font-face/@page/@keyframes/unknown)
                // starts a fresh root — declarations stay bare even when nested.
                let wraps = matches!(
                    base.as_str(),
                    "@supports" | "@document" | "@starting-style" | "@layer"
                );
                let inner_parent = if wraps { parent_paths } else { None };
                // Every at-rule is a media-bubbling boundary (less.js
                // `AtRule.eval` backs up mediaPath/mediaBlocks): an inner @media
                // must not bubble past this block.
                let saved_path = std::mem::take(&mut self.media_path);
                let saved_blocks = self.media_blocks.take();
                self.push_frame(frame_of(rules.to_vec()));
                let res = self.process_body(rules, inner_parent);
                self.pop_frame();
                self.media_path = saved_path;
                self.media_blocks = saved_blocks;
                let (own, children) = res?;

                // The in-place `simpleBlock` form (less.js AtRule constructor +
                // eval, §2.13): a value-less non-rooted at-rule whose evaluated
                // body is pure declarations/comments renders NESTED inside the
                // enclosing rule's block (`@starting-style`, bare `@layer`).
                if wraps
                    && prelude.as_deref().unwrap_or("").is_empty()
                    && parent_paths.is_some()
                    && children.is_empty()
                    && own.iter().all(|n| {
                        matches!(n, Node::Declaration(_) | Node::Comment { .. })
                    })
                {
                    if !has_visible(&own) {
                        return Ok(Vec::new());
                    }
                    return Ok(vec![Out::Nested(Node::AtRule(crate::ast::AtRule {
                        name: a.name.clone(),
                        prelude: None,
                        block: AtRuleBlock::Rules(own),
                        span: Default::default(),
                    }))]);
                }

                // Declarations first (like less.js Ruleset.genCSS), then nested.
                let mut body_outs: Vec<Out> = Vec::new();
                if has_visible(&own) {
                    body_outs.push(match parent_paths {
                        Some(paths) if wraps && !paths.is_empty() => Out::Rule {
                            selectors: plain_sels(paths.to_vec()),
                            decls: own,
                            extends: Vec::new(),
                            extend_on_every_path: false,
                        },
                        _ => Out::Decls(own),
                    });
                }
                body_outs.extend(children);

                // An at-rule with an empty block emits nothing (plan §2.13);
                // comment-only bodies are KEPT (verified against less.js 4.6.7).
                if body_outs.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![Out::At {
                    header,
                    body: AtBody::Rules(body_outs),
                }])
            }
        }
    }

    /// A nestable at-rule (`@media`/`@container`, less.js `Media`/`Container` +
    /// `NestableAtRulePrototype`, plan §2.13): nested same-kind blocks merge
    /// their feature lists with `and` (comma lists cross-multiply) and surface
    /// as SIBLINGS of the outermost block, in depth-first entry order; a
    /// nested block of the OTHER kind stays in place unmerged.
    fn eval_nestable_at_rule(
        &mut self,
        a: &crate::ast::AtRule,
        base: &str,
        parent_paths: Option<&[String]>,
    ) -> Result<Vec<Out>, LessError> {
        let AtRuleBlock::Rules(rules) = &a.block else {
            unreachable!("checked by caller");
        };
        let features = match &a.prelude {
            Some(p) => self.eval_media_features(p)?,
            None => Vec::new(),
        };

        let outermost = self.media_path.is_empty();
        if outermost && self.media_blocks.is_none() {
            self.media_blocks = Some(Vec::new());
        }
        // The merged header: the full path's feature lists cross-multiplied and
        // `and`-joined (less.js `evalNested`) — only when every enclosing
        // nestable block is the same kind.
        let same_kind = self.media_path.iter().all(|m| m.kind == base);
        let merged: Option<Vec<String>> = if outermost {
            Some(features.clone())
        } else if same_kind {
            let mut lists: Vec<Vec<String>> =
                self.media_path.iter().map(|m| m.features.clone()).collect();
            lists.push(features.clone());
            Some(permute_feature_paths(&lists))
        } else {
            None
        };
        // Reserve the output slot NOW — blocks surface in entry order.
        let slot = match &merged {
            Some(_) => {
                let blocks = self.media_blocks.as_mut().expect("collector exists");
                blocks.push(None);
                Some(blocks.len() - 1)
            }
            None => None,
        };

        self.media_path.push(MediaFrame {
            kind: base.to_string(),
            features: features.clone(),
        });
        self.push_frame(frame_of(rules.to_vec()));
        let res = self.process_body(rules, parent_paths);
        self.pop_frame();
        self.media_path.pop();
        let (own, children) = res?;

        let mut body_outs: Vec<Out> = Vec::new();
        if has_visible(&own) {
            body_outs.push(match parent_paths {
                Some(paths) if !paths.is_empty() => Out::Rule {
                    selectors: plain_sels(paths.to_vec()),
                    decls: own,
                    extends: Vec::new(),
                    extend_on_every_path: false,
                },
                _ => Out::Decls(own),
            });
        }
        body_outs.extend(children);

        let header_features = merged.as_ref().unwrap_or(&features);
        let header = if header_features.is_empty() {
            a.name.clone()
        } else {
            format!("{} {}", a.name, header_features.join(", "))
        };
        // An empty `@media` block is pruned; an empty `@container` still
        // renders its shell (less.js prunes only Media — verified vs 4.6.7).
        let out = if body_outs.is_empty() && base == "@media" {
            None
        } else {
            Some(Out::At {
                header,
                body: AtBody::Rules(body_outs),
            })
        };

        match slot {
            Some(i) => {
                self.media_blocks.as_mut().expect("collector exists")[i] = out;
                if outermost {
                    let blocks = self.media_blocks.take().expect("collector exists");
                    Ok(blocks.into_iter().flatten().collect())
                } else {
                    Ok(Vec::new())
                }
            }
            // Mixed kind — the block stays nested at this position.
            None => Ok(out.into_iter().collect()),
        }
    }

    // ------------------------------------------------------------------
    // Declarations
    // ------------------------------------------------------------------

    fn eval_declaration(&mut self, d: &Declaration) -> Result<Node, LessError> {
        // less.js `Declaration.eval` math bypass: under math=always, a `font`
        // declaration's value evaluates in parens-division mode (the font
        // shorthand `0/0` protection — units/no-strict).
        if self.math == MathMode::Always && d.name == "font" {
            self.math = MathMode::ParensDivision;
            let res = self.eval_declaration_inner(d);
            self.math = MathMode::Always;
            return res;
        }
        self.eval_declaration_inner(d)
    }

    fn eval_declaration_inner(&mut self, d: &Declaration) -> Result<Node, LessError> {
        // Resolve interpolation in the property name. less.js's `evalName`
        // genCSSes each piece — a quoted variable KEEPS its quotes
        // (`@{prop}: red` with `@prop: "color"` → `"color": red`, F18).
        let name = if d.name.contains("@{") || d.name.contains("${") {
            self.interpolate_css(&d.name)?
        } else {
            d.name.clone()
        };

        if d.custom {
            // Custom properties (plan §2.16, less.js `permissiveValue`): a value
            // the entity chain parsed IS structured and evaluates normally
            // (`--x: rgba(0, 30, 0, 238)` folds); the raw capture stays
            // unevaluated with only `@{}` interpolation run inside it.
            let raw = match d.value.as_ref() {
                // Interpolation AND bare `@ref` substitution — less.js's
                // permissiveValue keeps Variable entities in raw captures
                // (`--fortran: read (*, iostat=@iostat)`, permissive-parse).
                Node::Anonymous(s) => {
                    let t = self.interpolate(s)?;
                    if t.contains('@') { self.resolve_prelude_vars(&t)? } else { t }
                }
                other => {
                    let v = self.eval_value(other)?;
                    render_value(&v, self.opts.num_precision)
                }
            };
            return Ok(Node::Declaration(Declaration {
                name,
                value: Box::new(Node::Anonymous(raw)),
                important: d.important.clone(),
                merge: d.merge,
                custom: true,
                span: Default::default(),
            }));
        }

        self.important_scope.push(None);
        let value = self.eval_value(&d.value);
        let popped = self.important_scope.pop().flatten();
        let value = value?;
        // less.js `Declaration.eval`: a detached ruleset landing on a real
        // property (e.g. `d: if(true, {…}, {…})`) is a hard error (F16).
        if matches!(value, Node::DetachedRuleset { .. })
            || matches!(&value, Node::Closure { inner, .. }
                if matches!(inner.as_ref(), Node::DetachedRuleset { .. }))
        {
            return Err(self.err(
                ErrorKind::Syntax,
                "Rulesets cannot be evaluated on a property.",
            ));
        }
        let important = if !d.important.is_empty() {
            d.important.clone()
        } else if let Some(imp) = popped {
            imp
        } else {
            String::new()
        };

        Ok(Node::Declaration(Declaration {
            name,
            value: Box::new(value),
            important,
            merge: d.merge,
            custom: false,
            span: Default::default(),
        }))
    }

    // ------------------------------------------------------------------
    // Value evaluation
    // ------------------------------------------------------------------

    fn eval_value(&mut self, node: &Node) -> Result<Node, LessError> {
        match node {
            Node::Value(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.eval_value(it)?);
                }
                Ok(Node::Value(out))
            }
            Node::Expression(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.eval_value(it)?);
                }
                Ok(if out.len() == 1 {
                    out.pop().unwrap()
                } else {
                    Node::Expression(out)
                })
            }
            Node::Variable { name, span } => self.eval_variable(name, *span),
            Node::VariableVariable { name, span } => {
                // @@name: resolve @name to a string, then look up that variable.
                let inner = self.eval_variable(name, *span)?;
                let vname = value_to_plain_string(&inner);
                self.eval_variable(&vname, *span)
            }
            Node::Interpolation { name, span } => {
                // `@{name}` in a value position becomes the variable's string form.
                let v = self.eval_variable(name, *span)?;
                Ok(Node::Anonymous(value_to_plain_string(&v)))
            }
            Node::Negative(inner) => self.eval_negative(inner),
            Node::Operation {
                op,
                left,
                right,
                spaced,
            } => self.eval_operation(op, left, right, *spaced),
            Node::Paren { inner, in_op } => self.eval_paren(inner, *in_op),
            Node::Call { name, args } => self.eval_call(name, args),
            Node::Assignment { key, value } => Ok(Node::Assignment {
                key: key.clone(),
                value: Box::new(self.eval_value(value)?),
            }),
            Node::Url(inner) => {
                let mut v = self.eval_value(inner)?;
                // less.js `URL.eval` (§2.18): prepend the current file's
                // rootpath when the path requires a rewrite (mode-dependent),
                // else normalize; then append `urlArgs`. An unquoted rootpath
                // is escaped (`folder (1)/` → `folder\ \(1\)/`).
                let rootpath = self
                    .file_stack
                    .last()
                    .map(|f| f.rootpath.clone())
                    .unwrap_or_default();
                let quoted = matches!(v, Node::Quoted { .. });
                let val: Option<&mut String> = match &mut v {
                    Node::Quoted { value, .. } => Some(value),
                    Node::Anonymous(t) => Some(t),
                    Node::Keyword(t) => Some(t),
                    _ => None,
                };
                if let Some(val) = val {
                    if self.path_requires_rewrite(val) {
                        let rp = if quoted { rootpath } else { escape_url_path(&rootpath) };
                        *val = rewrite_path(val, &rp);
                    } else {
                        *val = normalize_path(val);
                    }
                    if let Some(args) = &self.opts.url_args {
                        if !args.is_empty() && !val.trim_start().starts_with("data:") {
                            let delim = if val.contains('?') { "&" } else { "?" };
                            let insert = format!("{delim}{args}");
                            match val.find('#') {
                                Some(pos) => val.insert_str(pos, &insert),
                                None => val.push_str(&insert),
                            }
                        }
                    }
                }
                Ok(Node::Url(Box::new(v)))
            }
            Node::Quoted {
                escaped,
                quote,
                value,
            } => {
                // Interpolation runs inside quoted strings (plan §2.14).
                let v = if value.contains("@{") || value.contains("${") || value.contains("@") {
                    self.interpolate(value)?
                } else {
                    value.clone()
                };
                Ok(Node::Quoted {
                    escaped: *escaped,
                    quote: *quote,
                    value: v,
                })
            }
            Node::PropertyAccessor { name, .. } => self.eval_property(name),
            // A detached-ruleset literal captures the frames live at its
            // evaluation site (less.js `DetachedRuleset.eval`, plan §2.11).
            Node::DetachedRuleset { .. } => {
                let scope = self.closures.len() as u64;
                self.closures.push(self.frames.clone());
                Ok(Node::Closure {
                    inner: Box::new(node.clone()),
                    scope,
                })
            }
            // Already captured — evaluating again must NOT re-capture.
            Node::Closure { .. } => Ok(node.clone()),
            // A mixin call in value position (`@p: .mk-map();`) evaluates to its
            // ruleset — a map usable via `[]` lookups / `each()` (plan §2.12).
            Node::MixinCall(call) => {
                let call = call.clone();
                let rules = self.mixin_call_map(&call)?;
                Ok(Node::DetachedRuleset {
                    rules,
                    span: Span::default(),
                })
            }
            // `@dr()` in value position: the called ruleset, evaluated.
            Node::VariableCall { name, span } => {
                let (name, span) = (name.clone(), *span);
                let rules = self.variable_call_map(&name, span)?;
                Ok(Node::DetachedRuleset {
                    rules,
                    span: Span::default(),
                })
            }
            Node::Lookup { target, keys, .. } => {
                let (target, keys) = (target.clone(), keys.clone());
                self.eval_lookup(&target, &keys)
            }
            // Self-evaluating leaves.
            Node::Dimension(_)
            | Node::Color(_)
            | Node::Keyword(_)
            | Node::Anonymous(_) => Ok(node.clone()),
            other => Ok(other.clone()),
        }
    }

    fn eval_variable(&mut self, name: &str, _span: crate::ast::Span) -> Result<Node, LessError> {
        // Strip a leading `@@` handled by caller; here `name` has no `@`.
        let key = name.trim_start_matches('@').to_string();

        if self.evaluating.iter().any(|n| n == &key) {
            return Err(self.err(
                ErrorKind::Name,
                format!("Recursive variable definition for @{key}"),
            ));
        }

        // Find the nearest frame declaring it.
        let mut found: Option<(Node, bool)> = None;
        for frame in &self.frames {
            if let Some((val, important)) = frame_variable(frame, &key) {
                found = Some((val, important));
                break;
            }
        }
        let Some((val, important)) = found else {
            return Err(self.err(ErrorKind::Name, format!("variable @{key} is undefined")));
        };

        if important {
            if let Some(slot) = self.important_scope.last_mut() {
                *slot = Some(" !important".to_string());
            }
        }

        self.evaluating.push(key.clone());
        // Inside `calc()`, less.js wraps the variable's value in a `_SELF` call
        // (`Variable.eval`), whose `Call.eval` switches math back ON — so the
        // variable's own operations fold even though calc suppresses math.
        let result = if self.in_calc {
            let prev = self.math_on;
            self.math_on = true;
            let r = self.eval_value(&val);
            self.math_on = prev;
            r
        } else {
            self.eval_value(&val)
        };
        self.evaluating.pop();
        result
    }

    fn eval_negative(&mut self, inner: &Node) -> Result<Node, LessError> {
        if self.is_math_on('*') {
            let minus_one = Node::Dimension(Dimension::number(-1.0));
            self.eval_binary("*", &minus_one, inner, true)
        } else {
            Ok(Node::Negative(Box::new(self.eval_value(inner)?)))
        }
    }

    fn eval_paren(&mut self, inner: &Node, in_op: bool) -> Result<Node, LessError> {
        // less.js `Expression.eval` for a `parens` sub (§2.4/calc): the literal
        // paren survives ONLY for an operand paren whose math didn't run and
        // whose result is not a folded number — everything else unwraps.
        let math_on_entry = self.is_math_on_plain();
        let double_paren =
            matches!(inner, Node::Paren { in_op: false, .. }) && !self.in_calc;
        self.parens += 1;
        let v = self.eval_value(inner);
        self.parens -= 1;
        let v = v?;
        if in_op && !math_on_entry && !double_paren && !matches!(v, Node::Dimension(_)) {
            Ok(Node::Paren { inner: Box::new(v), in_op: true })
        } else {
            Ok(v)
        }
    }

    fn eval_operation(
        &mut self,
        op: &str,
        left: &Node,
        right: &Node,
        spaced: bool,
    ) -> Result<Node, LessError> {
        let mut res = self.eval_binary(op, left, right, spaced)?;
        // `strictUnits` output rule (less.js `Unit.genCSS` strict branch): a
        // fully-cancelled unit renders EMPTY — never the `backupUnit` guess
        // (`(1px / 1px)` → `1`, units/strict).
        if self.opts.strict_units {
            if let Node::Dimension(d) = &mut res {
                if d.unit.numerator.is_empty() {
                    d.unit.backup = None;
                }
            }
        }
        Ok(res)
    }

    /// The math core (less.js `Operation.eval`): eval operands, and if math is on
    /// for the op, coerce and operate; else return a deferred `Operation` (whose
    /// source spacing is preserved for literal emission).
    fn eval_binary(
        &mut self,
        op: &str,
        left: &Node,
        right: &Node,
        spaced: bool,
    ) -> Result<Node, LessError> {
        // `./` — the legacy forced-division operator: division regardless of
        // the parens-division slash rule (less.js `isMathOn('./')` — only the
        // parens-mode parens requirement still applies).
        let opc = if op == "./" { '/' } else { op.chars().next().unwrap_or('+') };
        let math_on = if op == "./" { self.is_math_on_plain() } else { self.is_math_on(opc) };
        let a = self.eval_value(left)?;
        let b = self.eval_value(right)?;
        if !math_on {
            return Ok(Node::Operation {
                op: op.to_string(),
                left: Box::new(a),
                right: Box::new(b),
                spaced,
            });
        }
        let op = opc;
        // Coerce keyword colors, then dimension↔color.
        let a = coerce_color(a);
        let b = coerce_color(b);
        match (&a, &b) {
            (Node::Dimension(da), Node::Dimension(db)) => match da.operate(op, db, self.opts.strict_units) {
                // less.js's Dimension constructor throws on NaN (`(0 / 0)`,
                // `(Infinity - Infinity)`) — Infinity itself is fine (F2/C20).
                Ok(r) if r.value.is_nan() => {
                    Err(self.err(ErrorKind::Runtime, "Dimension is not a number."))
                }
                Ok(r) => Ok(Node::Dimension(r)),
                Err(bad) => Err(self.err(
                    ErrorKind::Operation,
                    format!(
                        "Incompatible units. Change the units or use the unit function. Bad units: {bad}."
                    ),
                )),
            },
            (Node::Color(ca), Node::Color(cb)) => Ok(Node::Color(ca.operate(op, cb))),
            (Node::Dimension(da), Node::Color(cb)) => Ok(Node::Color(da.to_color().operate(op, cb))),
            (Node::Color(ca), Node::Dimension(db)) => Ok(Node::Color(ca.operate(op, &db.to_color()))),
            _ => {
                // Not both operable — defer (emit literally).
                Ok(Node::Operation {
                    op: op.to_string(),
                    left: Box::new(a),
                    right: Box::new(b),
                    spaced,
                })
            }
        }
    }

    /// less.js `context.isMathOn()` with no operator (Expression.eval's check).
    fn is_math_on_plain(&self) -> bool {
        if !self.math_on {
            return false;
        }
        if self.math == MathMode::Parens {
            return self.parens > 0;
        }
        true
    }

    /// less.js `context.isMathOn(op)` (plan §1/§2.4).
    fn is_math_on(&self, op: char) -> bool {
        if !self.math_on {
            return false;
        }
        if op == '/' && self.math != MathMode::Always && self.parens == 0 {
            return false;
        }
        if self.math == MathMode::Parens {
            return self.parens > 0;
        }
        true
    }

    // ------------------------------------------------------------------
    // Functions
    // ------------------------------------------------------------------

    fn eval_call(&mut self, name: &str, args: &[Node]) -> Result<Node, LessError> {
        let lname = name.to_ascii_lowercase();
        let is_calc = lname == "calc";

        // `default()` — the guard-only function (plan §2.6). Inside a guard it
        // returns the current two-subpass value; outside a guard it is not the
        // guard function and passes through verbatim (re-emitted `default()`).
        if lname == "default" {
            if let Some(v) = self.default_value {
                return Ok(Node::Keyword(if v { "true" } else { "false" }.to_string()));
            }
        }

        // The `evalArgs: false` functions (plan §2.7) — their arguments must NOT
        // be pre-evaluated: `if`'s branches are lazy, `isdefined` must not error
        // on an undefined variable, and both parse their condition arg through
        // the guard grammar (less.js `customFuncCall` routes them to `condition`).
        match lname.as_str() {
            "isdefined" => {
                let defined = matches!(args.first(), Some(Node::Variable { name, .. })
                    if self.lookup_defined(name));
                return Ok(Node::Keyword(if defined { "true" } else { "false" }.to_string()));
            }
            "boolean" => {
                let cond = self.call_condition(args.first())?;
                return Ok(Node::Keyword(if cond { "true" } else { "false" }.to_string()));
            }
            "if" => {
                let branch = if self.call_condition(args.first())? {
                    args.get(1)
                } else {
                    args.get(2)
                };
                return match branch {
                    Some(b) => self.eval_value(b),
                    None => Ok(Node::Anonymous(String::new())),
                };
            }
            _ => {}
        }

        // Evaluate arguments with less.js `Call.eval`'s math context: math turns
        // back ON inside any function's arguments — EXCEPT calc(), which
        // suppresses it (and flags `inCalc` for nested variables; plan §2.4).
        let prev_math = self.math_on;
        let prev_calc = self.in_calc;
        self.math_on = !is_calc;
        if is_calc {
            self.in_calc = true;
        }
        let mut evaled = Vec::with_capacity(args.len());
        for a in args {
            let v = self.eval_value(a);
            match v {
                Ok(v) => evaled.push(v),
                Err(e) => {
                    self.math_on = prev_math;
                    self.in_calc = prev_calc;
                    return Err(e);
                }
            }
        }
        self.math_on = prev_math;
        self.in_calc = prev_calc;

        if !is_calc {
            // Resource functions read files through the resolver boundary
            // (plan §2.7/§C-assets) — dispatched here, where the resolver lives.
            let axis = match lname.as_str() {
                "data-uri" => {
                    let dir = self.current_dir();
                    if let Some(r) = functions::data_uri::data_uri(&evaled, self.resolver, &dir) {
                        return Ok(r);
                    }
                    None
                }
                "image-size" => Some(functions::misc::SizeAxis::Both),
                "image-width" => Some(functions::misc::SizeAxis::Width),
                "image-height" => Some(functions::misc::SizeAxis::Height),
                _ => None,
            };
            if let Some(axis) = axis {
                let dir = self.current_dir();
                if let Some(r) = functions::misc::image_size(&evaled, axis, self.resolver, &dir) {
                    return Ok(r);
                }
            }
            // less.js `functionCaller.call`: comments are filtered out of the
            // args (top level + inside Expressions, re-collapsing singletons)
            // BEFORE a registered function sees them — but the passthrough
            // re-emit below keeps the unfiltered args.
            let filtered = filter_call_args(&evaled);
            // Registered custom functions (the minimal `functionRegistry.add`
            // surface, plan §2.7) shadow the built-ins; `None` falls through.
            for (fname, f) in &self.opts.custom_functions {
                if *fname == lname {
                    if let Some(result) = f(&filtered) {
                        return Ok(result);
                    }
                }
            }
            if let Some(result) = functions::dispatch(&lname, &filtered, self.opts.num_precision)? {
                return Ok(result);
            }
        }

        // calc() and unknown / passthrough CSS functions: re-emit
        // `name(evaluated-args)` (§2.7).
        Ok(Node::Call {
            name: name.to_string(),
            args: evaled,
        })
    }

    /// `each(list, ruleset)` (plan §2.7, less.js `functions/list.js`): iterate
    /// the list (or a detached ruleset's rules), binding `@value`/`@key`/
    /// `@index` — or the anonymous mixin's named params — and evaluate the
    /// callback's rules at the current position like a `&`-ruleset body.
    fn expand_each(
        &mut self,
        args: &[Node],
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<(), LessError> {
        let (Some(list_arg), Some(rs_arg)) = (args.first(), args.get(1)) else {
            return Ok(());
        };
        // The callback: a literal DR, an anonymous mixin, or a variable holding one.
        let resolved;
        let rs_arg = match rs_arg {
            Node::Variable { name, span } => {
                resolved = self.eval_variable(name, *span)?;
                &resolved
            }
            other => other,
        };
        // A captured DR (Closure) unwraps to its literal rules.
        let rs_arg = match rs_arg {
            Node::Closure { inner, .. } => inner.as_ref(),
            other => other,
        };
        let (param_names, rules): (Vec<Option<String>>, Vec<Node>) = match rs_arg {
            Node::DetachedRuleset { rules, .. } => (Vec::new(), rules.clone()),
            Node::MixinDefinition(def) if def.name.is_empty() => (
                def.params.iter().map(|p| p.name.clone()).collect(),
                def.rules.clone(),
            ),
            _ => return Ok(()),
        };
        let pname = |i: usize, def: &str| -> Option<String> {
            match param_names.get(i) {
                Some(Some(n)) => Some(n.trim_start_matches('@').to_string()),
                Some(None) => None,
                None if param_names.is_empty() => Some(def.to_string()),
                None => None,
            }
        };
        let value_name = pname(0, "value");
        let key_name = pname(1, "key");
        let index_name = pname(2, "index");

        // `each(.mixin(), …)` — less.js routes the arg through `mixinLookup`,
        // whose eval yields the mixin's ruleset; iterate its emitted
        // declarations (Phase 3 review F3-residual).
        let iterator: Vec<Node> = if let Node::MixinCall(call) = list_arg {
            let mut ex_own = Vec::new();
            let mut ex_children = Vec::new();
            self.expand_mixin_call(call, self_paths, &mut ex_own, &mut ex_children)?;
            ex_own
        } else {
            let list = self.eval_value(list_arg)?;
            match &list {
                Node::Value(v) | Node::Expression(v) => v.clone(),
                Node::DetachedRuleset { rules, .. } => rules.clone(),
                Node::Closure { inner, .. } => match inner.as_ref() {
                    Node::DetachedRuleset { rules, .. } => rules.clone(),
                    other => vec![other.clone()],
                },
                other => vec![other.clone()],
            }
        };

        for (i, item) in iterator.iter().enumerate() {
            if matches!(item, Node::Comment { .. }) {
                continue;
            }
            let (key, value) = match item {
                Node::Declaration(d) => (
                    Node::Keyword(d.name.clone()),
                    (*d.value).clone(),
                ),
                Node::VariableDecl { .. } => continue,
                other => (
                    Node::Dimension(Dimension::number((i + 1) as f64)),
                    other.clone(),
                ),
            };
            let mut body = rules.clone();
            if let Some(n) = &value_name {
                body.push(var_decl(n, value));
            }
            if let Some(n) = &index_name {
                body.push(var_decl(
                    n,
                    Node::Dimension(Dimension::number((i + 1) as f64)),
                ));
            }
            if let Some(n) = &key_name {
                body.push(var_decl(n, key));
            }
            // The source list's own declarations stay visible below the
            // iteration frame, so a value referencing a sibling entry
            // (`contrast($background-color, …)` in a map) resolves
            // (namespacing-8 / less.js #3368).
            let src_frame = frame_of(iterator.clone());
            self.push_frame(src_frame);
            self.push_frame(frame_of(body.clone()));
            let r = self.eval_rules(&body, self_paths, own, children);
            self.pop_frame();
            self.pop_frame();
            r?;
        }
        Ok(())
    }

    /// Evaluate a function-call condition argument (`if`/`boolean`) through the
    /// guard grammar: the unevaluated arg is rendered back to source text and
    /// fed to the same condition evaluator guards use (plan §2.6/§2.7).
    fn call_condition(&mut self, arg: Option<&Node>) -> Result<bool, LessError> {
        let Some(arg) = arg else { return Ok(false) };
        let text = render_value(arg, self.opts.num_precision);
        self.eval_guard_str(&text)
    }

    // ------------------------------------------------------------------
    // Mixins (plan §2.5): definition + ruleset-as-mixin lookup with namespaces,
    // pattern-matching + overloading (emit-all), parametric binding (defaults,
    // named args, `@arguments`, `@rest...`), guards + `default()`, closures
    // (definition-scope capture), `!important` propagation, scope-injection.
    // ------------------------------------------------------------------

    /// Expand a mixin call: resolve candidates, match args + guards, and emit
    /// **every** surviving definition's body (plan §2.5). Returns the nodes to
    /// **inject** into the caller's frame (the mixin's top-level variables /
    /// mixins / rulesets — scope-injection, §2.5); the CSS output is appended to
    /// `own`/`children`.
    fn expand_mixin_call(
        &mut self,
        call: &crate::ast::MixinCall,
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<Vec<Node>, LessError> {
        let (chosen, args) = self.choose_candidates(call)?;

        // Emit every candidate that survived guard/default() selection.
        let mut injected: Vec<Node> = Vec::new();
        for cand in &chosen {
            let inj = self.emit_candidate(cand, &args, call.important, self_paths, own, children)?;
            injected.extend(inj);
        }
        Ok(injected)
    }

    /// Resolve a mixin call to the candidates that will actually emit (arity/
    /// pattern match + guards + two-subpass `default()` selection, §2.5/§2.6),
    /// together with the evaluated call arguments. Shared by statement calls and
    /// the value-position mixin-as-map form (plan §2.12).
    fn choose_candidates(
        &mut self,
        call: &crate::ast::MixinCall,
    ) -> Result<(Vec<Candidate>, Vec<EvArg>), LessError> {
        if self.mixin_depth > self.max_eval_depth() {
            return Err(self.err(ErrorKind::Runtime, "mixin recursion limit exceeded"));
        }
        let path = mixin_names(&call.path);
        if path.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        // Evaluate call arguments once (values against the caller's frames).
        let args = self.eval_call_args(&call.args)?;

        // Collect candidates: the innermost frame that yields an arg-matching
        // definition wins (less.js `MixinCall.eval`); all its name-matches whose
        // arity/pattern match become candidates.
        let frames = self.frames.clone();
        let mut is_one_found = false;
        let mut chosen: Vec<Candidate> = Vec::new();
        for k in 0..frames.len() {
            let def_scope: Vec<Frame> = frames[k..].to_vec();
            let mut found = find_candidates(&frames[k].borrow(), &path, &def_scope, &self.closures, &[], false);
            // A name hit counts as "found" BEFORE the recursion filter — less.js
            // sets `isOneFound` on the frame `find` result, then `continue`s the
            // on-stack candidates, so a fully-recursive call errors "No matching
            // definition", not "is undefined".
            if !found.is_empty() {
                is_one_found = true;
            }
            // Drop ruleset candidates already on the eval stack (recursion guard;
            // MixinDefinitions are exempt — their `ruleset_span` is `None`).
            found.retain(|c| match c.ruleset_span {
                Some(span) => !self.active_rulesets.contains(&span),
                None => true,
            });
            if found.is_empty() {
                continue;
            }
            let mut matched: Vec<Candidate> = Vec::new();
            for cand in found {
                if self.match_args(&cand, &args)? {
                    matched.push(cand);
                }
            }
            if !matched.is_empty() {
                chosen = matched;
                break;
            }
        }

        if chosen.is_empty() {
            if is_one_found {
                return Err(self.err(
                    ErrorKind::Runtime,
                    format!("No matching definition was found for `{}`", format_call(&path, &args, self.opts.num_precision)),
                ));
            }
            return Err(self.err(
                ErrorKind::Name,
                format!("{} is undefined", path.join(" ")),
            ));
        }

        // Guard + default() classification (two-subpass, §2.6).
        let mut groups: Vec<i32> = Vec::with_capacity(chosen.len());
        for cand in &chosen {
            groups.push(self.calc_def_group(cand, &args)?);
        }
        let mut count = [0usize; 3];
        for g in &groups {
            if *g >= 0 {
                count[*g as usize] += 1;
            }
        }
        let default_result: i32 = if count[0] > 0 {
            2 // defFalse
        } else {
            if count[1] + count[2] > 1 {
                return Err(self.err(
                    ErrorKind::Runtime,
                    format!(
                        "Ambiguous use of `default()` found when matching for `{}`",
                        format_call(&path, &args, self.opts.num_precision)
                    ),
                ));
            }
            1 // defTrue
        };

        let mut keep = groups.iter();
        chosen.retain(|_| {
            let g = *keep.next().unwrap();
            g == 0 || g == default_result
        });
        Ok((chosen, args))
    }

    /// Evaluate the call's arguments to `(name?, value)` pairs (§2.5). A trailing
    /// `...` on a positional argument (`.m(@list...)`) **spreads** the list value
    /// into individual arguments (less.js `arg.expand`).
    fn eval_call_args(&mut self, args: &[MixinArg]) -> Result<Vec<EvArg>, LessError> {
        let mut out = Vec::with_capacity(args.len());
        // less.js parser quirk (faithfully reproduced): the args parser's
        // `expand` local is NEVER reset between arguments, so once one arg
        // spreads (`@x...`), every LATER list-valued arg in the same call
        // spreads too (`.aa(@y, @x..., and again)` splits `and again`;
        // math-*/mixins-args expand-op-9).
        let mut sticky_expand = false;
        for a in args {
            let name = a.name.as_ref().map(|n| n.trim_start_matches('@').to_string());
            // Detect the spread form on an unnamed argument.
            if name.is_none() {
                if let Node::Anonymous(s) = a.value.as_ref() {
                    if let Some(inner) = s.trim().strip_suffix("...") {
                        sticky_expand = true;
                        let value = self.reparse_and_eval(inner.trim())?;
                        match &value {
                            Node::Value(items) | Node::Expression(items) => {
                                for it in items {
                                    out.push(EvArg { name: None, value: it.clone() });
                                }
                            }
                            other => out.push(EvArg { name: None, value: other.clone() }),
                        }
                        continue;
                    }
                }
            }
            let value = self.reparse_arg(a.value.as_ref())?;
            if sticky_expand && name.is_none() {
                if let Node::Value(items) | Node::Expression(items) = &value {
                    for it in items {
                        out.push(EvArg { name: None, value: it.clone() });
                    }
                    continue;
                }
            }
            out.push(EvArg { name, value });
        }
        Ok(out)
    }

    /// less.js `MixinDefinition.matchArgs` — arity + literal-pattern match (§2.5).
    fn match_args(&mut self, cand: &Candidate, args: &[EvArg]) -> Result<bool, LessError> {
        let params = &cand.params;
        let variadic = params.last().map(|p| p.variadic).unwrap_or(false);
        let arity = params.len();
        let optional: Vec<String> = params
            .iter()
            .filter(|p| p.name.is_some() && p.default.is_some() && !p.variadic)
            .filter_map(|p| p.name.as_ref().map(|n| n.trim_start_matches('@').to_string()))
            .collect();
        let required = params
            .iter()
            .filter(|p| p.name.is_none() || p.default.is_none())
            .count();
        let all_args = args.len();
        let required_args = args
            .iter()
            .filter(|a| match &a.name {
                Some(n) => !optional.contains(n),
                None => true,
            })
            .count();
        if !variadic {
            if required_args < required {
                return Ok(false);
            }
            if all_args > arity {
                return Ok(false);
            }
        } else if required_args + 1 < required {
            return Ok(false);
        }
        // Literal-pattern check: positional literal params compare by toCSS.
        let len = required_args.min(arity);
        for (i, p) in params.iter().enumerate().take(len) {
            if p.name.is_none() && !p.variadic {
                let Some(def) = &p.default else { continue };
                let pv = self.reparse_arg(def)?;
                if i >= args.len() {
                    return Ok(false);
                }
                if render_value(&args[i].value, self.opts.num_precision)
                    != render_value(&pv, self.opts.num_precision)
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Classify a candidate for `default()` resolution (§2.6): evaluate its guard
    /// under `default()==false` then `==true`. Returns defNone(0)/defTrue(1)/
    /// defFalse(2), or -1 when the guard fails either way (not a candidate).
    fn calc_def_group(&mut self, cand: &Candidate, args: &[EvArg]) -> Result<i32, LessError> {
        if cand.guard.is_none() && cand.path_guards.is_empty() {
            return Ok(0); // no guard anywhere → always matches (defNone)
        }
        // less.js `calcDefGroup`: AND every traversed namespace's guard (with the
        // `null`-arg namespace binding) with the mixin's own guard, per subpass.
        let mixin_guard = cand.guard.as_ref().map(guard_text);
        let path_guards: Vec<String> = cand.path_guards.iter().map(guard_text).collect();
        let param_frame = self.bind_params(cand, args)?;
        let mut cond = [true, true];
        for (f, slot) in cond.iter_mut().enumerate() {
            self.default_value = Some(f == 1);
            let mut ok = true;
            for pg in &path_guards {
                if !self.with_mixin_frames(cand, &param_frame, |s| s.eval_guard_str(pg))? {
                    ok = false;
                    break;
                }
            }
            if ok {
                if let Some(g) = &mixin_guard {
                    ok = self.with_mixin_frames(cand, &param_frame, |s| s.eval_guard_str(g))?;
                }
            }
            *slot = ok;
        }
        self.default_value = None;
        if cond[0] || cond[1] {
            if cond[0] != cond[1] {
                return Ok(if cond[1] { 1 } else { 2 });
            }
            return Ok(0);
        }
        Ok(-1)
    }

    /// Evaluate the mixin body of one matched candidate; append CSS to `own`/
    /// `children`. Returns nodes to inject into the caller's scope (§2.5).
    fn emit_candidate(
        &mut self,
        cand: &Candidate,
        args: &[EvArg],
        important: bool,
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<Vec<Node>, LessError> {
        // On-stack recursion guard for ruleset-as-mixin (MixinDefinitions exempt).
        if let Some(span) = cand.ruleset_span {
            if self.active_rulesets.contains(&span) {
                return Ok(Vec::new());
            }
        }
        let param_frame = self.bind_params(cand, args)?;
        let body_frame = frame_of(cand.rules.clone());
        let mut new_frames: Vec<Frame> = Vec::with_capacity(self.frames.len() + 3);
        new_frames.push(body_frame);
        new_frames.push(frame_of(param_frame.clone()));
        new_frames.extend(cand.def_scope.iter().cloned());
        new_frames.extend(self.frames.iter().cloned());
        let saved = std::mem::replace(&mut self.frames, dedup_frames(new_frames));
        if let Some(span) = cand.ruleset_span {
            self.active_rulesets.push(span);
        }
        self.mixin_depth += 1;

        let mut sub_own = Vec::new();
        let mut sub_children = Vec::new();
        let res = self.eval_rules(&cand.rules, self_paths, &mut sub_own, &mut sub_children);

        // Collect scope-injection nodes from the body's top level.
        let injected = if res.is_ok() {
            self.collect_injected(&cand.rules)
        } else {
            Vec::new()
        };

        self.mixin_depth -= 1;
        if cand.ruleset_span.is_some() {
            self.active_rulesets.pop();
        }
        self.frames = saved;
        res?;

        if important {
            for d in sub_own.iter_mut() {
                make_important_node(d);
            }
            for c in sub_children.iter_mut() {
                make_important_out(c);
            }
        }
        own.extend(sub_own);
        children.extend(sub_children);
        Ok(injected)
    }

    /// Gather the mixin body's top-level variables (evaluated) + mixins/rulesets
    /// for scope-injection into the caller (§2.5). Called with the mixin frames on
    /// the stack, so variable values resolve in the mixin's context.
    fn collect_injected(&mut self, rules: &[Node]) -> Vec<Node> {
        let mut out = Vec::new();
        for r in rules {
            match r {
                Node::VariableDecl { name, value, important, .. } => {
                    let val = self.eval_value(value).unwrap_or_else(|_| (**value).clone());
                    out.push(Node::VariableDecl {
                        name: name.clone(),
                        value: Box::new(val),
                        important: important.clone(),
                        span: Default::default(),
                    });
                }
                Node::MixinDefinition(_) => {
                    // Freeze the mixin's current eval frames (its bound params +
                    // definition scope) so a later call on this injected inner
                    // definition resolves the enclosing mixin's parameters
                    // (closure over params, §4.3). The frames are stashed in the
                    // side table; the node carries only the index.
                    let scope = self.closures.len() as u64;
                    self.closures.push(self.frames.clone());
                    out.push(Node::Closure { inner: Box::new(r.clone()), scope });
                }
                Node::Ruleset(_) => {
                    // Freeze frames for rulesets too: an interpolated selector
                    // (`.@{name}` under a mixin param) must resolve at a later
                    // lookup from the caller (mixins-interpolated mi-test-d).
                    let scope = self.closures.len() as u64;
                    self.closures.push(self.frames.clone());
                    out.push(Node::Closure { inner: Box::new(r.clone()), scope });
                }
                _ => {}
            }
        }
        out
    }

    /// Expand a detached-ruleset call statement `@dr();` (less.js
    /// `VariableCall` + `DetachedRuleset.callEval`, plan §2.11): evaluate the
    /// DR's rules with **its captured definition frames first, then the
    /// caller's**, emit the output at this position, and return the nodes to
    /// inject into the caller's scope — mixins/rulesets, but NEVER variables.
    fn expand_variable_call(
        &mut self,
        name: &str,
        span: Span,
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<Vec<Node>, LessError> {
        if self.mixin_depth > self.max_eval_depth() {
            return Err(self.err(ErrorKind::Runtime, "mixin recursion limit exceeded"));
        }
        let v = self.eval_variable(name, span)?;
        let (rules, captured) = self.as_detached(v, name)?;

        let body_frame = frame_of(rules.clone());
        let mut new_frames: Vec<Frame> = Vec::with_capacity(self.frames.len() + 2);
        new_frames.push(body_frame);
        if let Some(scope) = captured {
            new_frames.extend(self.closures[scope as usize].iter().cloned());
        }
        new_frames.extend(self.frames.iter().cloned());
        let saved = std::mem::replace(&mut self.frames, dedup_frames(new_frames));
        self.mixin_depth += 1;

        let mut sub_own = Vec::new();
        let mut sub_children = Vec::new();
        let res = self.eval_rules(&rules, self_paths, &mut sub_own, &mut sub_children);
        let injected = if res.is_ok() {
            self.collect_injected(&rules)
        } else {
            Vec::new()
        };

        self.mixin_depth -= 1;
        self.frames = saved;
        res?;

        own.extend(sub_own);
        children.extend(sub_children);
        // "do not pollute the scope at all" — a DR call returns no variables.
        Ok(injected
            .into_iter()
            .filter(|n| !matches!(n, Node::VariableDecl { .. }))
            .collect())
    }

    /// Stage 2 of §2.9: evaluate one pre-fetched import at its source
    /// position. Applies the once skip-closure (first EVAL of a canonical
    /// path wins), wraps feature-carrying imports in a synthetic `@media`,
    /// splices `(inline)` payloads verbatim, and returns the imported file's
    /// top-level variables/mixins/rulesets for the caller-frame splice.
    /// `(reference)` bodies evaluate under a visibility block (§2.8).
    fn expand_resolved_import(
        &mut self,
        ir: &crate::ast::ImportResolved,
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<Vec<Node>, LessError> {
        if self.mixin_depth > self.max_eval_depth() {
            return Err(self.err(ErrorKind::Import, "import recursion limit exceeded"));
        }
        if ir.skip {
            return Ok(Vec::new());
        }
        // The once skip-closure (less.js `onceFileDetectionMap`): the first
        // eval of a path emits; later ones (fetched separately, e.g. via a
        // variable import) skip.
        if !ir.multiple && !self.once_imported.insert(ir.full_path.clone()) {
            return Ok(Vec::new());
        }

        if let Some(content) = &ir.inline {
            // Inside a ruleset the raw payload renders in the declaration
            // block at its source position (an Anonymous rule in less.js) —
            // hidden contexts drop it with the rest of the block's decls.
            if self_paths.is_some() && ir.features.is_none() {
                if !(ir.reference || self.visibility_blocks > 0) {
                    own.push(Node::Anonymous(content.clone()));
                }
                return Ok(Vec::new());
            }
            let out = match &ir.features {
                Some(f) => {
                    let fs = self.eval_media_features(f)?.join(", ");
                    Out::At {
                        header: format!("@media {fs}"),
                        body: AtBody::Verbatim(content.clone()),
                    }
                }
                // Verbatim: keep the payload's own trailing newline — joined
                // with the root separator it yields the blank line less.js
                // emits after an inline import (Anonymous raw content).
                None => Out::Verbatim(content.clone()),
            };
            children.push(if ir.reference || self.visibility_blocks > 0 {
                Out::Hidden(Box::new(out))
            } else {
                out
            });
            return Ok(Vec::new());
        }

        self.file_stack.push(FileScope {
            filename: ir.full_path.clone(),
            current_directory: ir.current_directory.clone(),
            rootpath: ir.rootpath.clone(),
            reference: ir.reference,
        });
        if ir.reference {
            self.visibility_blocks += 1;
        }
        self.mixin_depth += 1;

        let hide = ir.reference;
        let res = (|| -> Result<Vec<Node>, LessError> {
            if let Some(f) = &ir.features {
                // `@import "x" screen` — the file's rules evaluate inside a
                // synthetic `@media screen` at this position; nothing enters
                // the importing scope (verified against 4.6.7).
                let at = crate::ast::AtRule {
                    name: "@media".to_string(),
                    prelude: Some(f.clone()),
                    block: AtRuleBlock::Rules(ir.rules.clone()),
                    span: ir.span,
                };
                for out in self.eval_at_rule(&at, self_paths)? {
                    match out {
                        Out::Nested(node) => own.push(node),
                        other if hide => children.push(Out::Hidden(Box::new(other))),
                        other => children.push(other),
                    }
                }
                return Ok(Vec::new());
            }
            let body_frame = frame_of(ir.rules.clone());
            self.push_frame(body_frame.clone());
            let mut sub_own = Vec::new();
            let mut sub_children = Vec::new();
            let res = self.eval_rules(&ir.rules, self_paths, &mut sub_own, &mut sub_children);
            self.pop_frame();
            res?;
            // A `(reference)` import's own output is visibility-blocked
            // (§2.8): kept for `:extend` to re-enable, never rendered as-is.
            if hide {
                sub_own.clear();
                children.extend(
                    sub_children
                        .into_iter()
                        .map(|o| Out::Hidden(Box::new(o))),
                );
            } else {
                own.extend(sub_own);
                children.extend(sub_children);
            }
            // Collect the importing-scope splice from the EVALUATED body frame
            // (not the raw rules): nested imports spliced their own
            // variables/mixins into it, and pass 0 wrapped DR values into
            // Closures — both must reach the importing scope.
            let injected: Vec<Node> = body_frame
                .borrow()
                .iter()
                .filter(|r| {
                    matches!(
                        r,
                        Node::VariableDecl { .. }
                            | Node::MixinDefinition(_)
                            | Node::Closure { .. }
                            | Node::Ruleset(_)
                    )
                })
                .cloned()
                .collect();
            Ok(injected)
        })();

        self.mixin_depth -= 1;
        if ir.reference {
            self.visibility_blocks -= 1;
        }
        self.file_stack.pop();
        res
    }

    /// less.js `Import.evalPath` (§2.18): rewrite a CSS import's path with the
    /// current file's rootpath when the rewrite mode requires it, else
    /// normalize it.
    fn rewrite_import_path(&self, path: Node) -> Node {
        let scope = self.file_stack.last();
        let rootpath = scope.map(|s| s.rootpath.as_str()).unwrap_or("");
        match path {
            Node::Quoted {
                escaped,
                quote,
                value,
            } => {
                let new = if self.path_requires_rewrite(&value) {
                    rewrite_path(&value, rootpath)
                } else {
                    normalize_path(&value)
                };
                Node::Quoted {
                    escaped,
                    quote,
                    value: new,
                }
            }
            // A `url(...)` path is left untouched (less.js only rewrites
            // non-URL import paths here; URL nodes rewrite in their own eval).
            other => other,
        }
    }

    /// less.js `contexts.Eval.pathRequiresRewrite`: `local` tests explicit
    /// `./`-relativity, everything else (INCLUDING off) plain relativity —
    /// `off` differs upstream only in the per-file rootpath accumulation
    /// (`ImportManager.push` skips the directory diff), not here.
    fn path_requires_rewrite(&self, path: &str) -> bool {
        match self.opts.rewrite_urls {
            crate::options::RewriteUrls::Local => is_path_local_relative(path),
            crate::options::RewriteUrls::Off | crate::options::RewriteUrls::All => {
                is_path_relative(path)
            }
        }
    }

    /// Unwrap an evaluated value into detached-ruleset rules + the captured
    /// scope index (`None` for an already-evaluated/uncaptured body).
    fn as_detached(&self, v: Node, name: &str) -> Result<(Vec<Node>, Option<u64>), LessError> {
        match v {
            Node::Closure { inner, scope } => match *inner {
                Node::DetachedRuleset { rules, .. } => Ok((rules, Some(scope))),
                _ => Err(self.err(
                    ErrorKind::Runtime,
                    format!("Could not evaluate variable call @{name}"),
                )),
            },
            Node::DetachedRuleset { rules, .. } => Ok((rules, None)),
            _ => Err(self.err(
                ErrorKind::Runtime,
                format!("Could not evaluate variable call @{name}"),
            )),
        }
    }

    /// `@dr()` as a map (value position / lookup target): callEval the DR and
    /// return its evaluated rules (plan §2.12).
    fn variable_call_map(&mut self, name: &str, span: Span) -> Result<Vec<Node>, LessError> {
        let v = self.eval_variable(name, span)?;
        let (rules, captured) = self.as_detached(v, name)?;
        self.map_rules_with_frames(&rules, captured)
    }

    /// A value-position mixin call's evaluated ruleset (map form, plan §2.12):
    /// every surviving candidate's body, evaluated in the mixin's own scope, in
    /// source order (declarations AND variables — lookups read both).
    fn mixin_call_map(&mut self, call: &crate::ast::MixinCall) -> Result<Vec<Node>, LessError> {
        let (chosen, args) = self.choose_candidates(call)?;
        let mut out = Vec::new();
        for cand in &chosen {
            let param_frame = self.bind_params(cand, &args)?;
            let body_frame = frame_of(cand.rules.clone());
            let mut new_frames: Vec<Frame> = Vec::with_capacity(self.frames.len() + 3);
            new_frames.push(body_frame);
            new_frames.push(frame_of(param_frame));
            new_frames.extend(cand.def_scope.iter().cloned());
            new_frames.extend(self.frames.iter().cloned());
            let saved = std::mem::replace(&mut self.frames, new_frames);
            self.mixin_depth += 1;
            let res = self.eval_map_rules(&cand.rules);
            self.mixin_depth -= 1;
            self.frames = saved;
            out.extend(res?);
        }
        Ok(out)
    }

    /// Evaluate `rules` as map content with the given captured frames installed.
    fn map_rules_with_frames(
        &mut self,
        rules: &[Node],
        captured: Option<u64>,
    ) -> Result<Vec<Node>, LessError> {
        let body_frame = frame_of(rules.to_vec());
        let mut new_frames: Vec<Frame> = Vec::with_capacity(self.frames.len() + 1);
        new_frames.push(body_frame);
        if let Some(scope) = captured {
            new_frames.extend(self.closures[scope as usize].iter().cloned());
        }
        new_frames.extend(self.frames.iter().cloned());
        let saved = std::mem::replace(&mut self.frames, dedup_frames(new_frames));
        let res = self.eval_map_rules(rules);
        self.frames = saved;
        res
    }

    /// Evaluate a rule list to its map form, in source order: declarations
    /// evaluated, variables evaluated, mixin definitions frozen as closures,
    /// nested rulesets kept raw (frames must already be installed).
    fn eval_map_rules(&mut self, rules: &[Node]) -> Result<Vec<Node>, LessError> {
        let mut out = Vec::new();
        for r in rules {
            match r {
                Node::Declaration(d) => out.push(self.eval_declaration(d)?),
                Node::VariableDecl {
                    name,
                    value,
                    important,
                    ..
                } => {
                    // A permissively-captured raw value resolves its `@refs`
                    // TEXTUALLY (less.js keeps such values as raw sequences
                    // with embedded variables — parens/colons survive:
                    // `@min: (min-width: @val)`, namespacing-media).
                    let val = match value.as_ref() {
                        Node::Anonymous(t) if t.contains('@') && !t.contains('[') => {
                            Node::Anonymous(self.resolve_prelude_vars(t)?)
                        }
                        other => {
                            let v = self.eval_value(other)?;
                            self.resolve_anon_refs(v)?
                        }
                    };
                    out.push(Node::VariableDecl {
                        name: name.clone(),
                        value: Box::new(val),
                        important: important.clone(),
                        span: Span::default(),
                    });
                }
                Node::MixinDefinition(_) => {
                    let scope = self.closures.len() as u64;
                    self.closures.push(self.frames.clone());
                    out.push(Node::Closure {
                        inner: Box::new(r.clone()),
                        scope,
                    });
                }
                Node::Ruleset(_) => out.push(r.clone()),
                // A nested mixin call contributes its returned scope to the
                // map (`.alias() { #ns.mixin(1); }` + `.alias[@a]` —
                // namespacing-4).
                Node::MixinCall(call) => {
                    let mut ex_own = Vec::new();
                    let mut ex_children = Vec::new();
                    let injected =
                        self.expand_mixin_call(call, None, &mut ex_own, &mut ex_children)?;
                    out.extend(injected);
                    out.extend(ex_own);
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// A `[key]` lookup chain over a map value (less.js `NamespaceValue.eval`,
    /// plan §2.12).
    fn eval_lookup(&mut self, target: &Node, keys: &[String]) -> Result<Node, LessError> {
        let mut rules: Vec<Node> = match target {
            Node::VariableCall { name, span } => self.variable_call_map(name, *span)?,
            Node::MixinCall(call) => self.mixin_call_map(call)?,
            other => {
                let v = self.eval_value(other)?;
                let (r, captured) = self.as_detached(v, "?")?;
                self.map_rules_with_frames(&r, captured)?
            }
        };
        let mut current: Option<Node> = None;
        for key in keys {
            if let Some(v) = current.take() {
                // A previous key produced a nested ruleset — descend into it.
                match v {
                    Node::DetachedRuleset { rules: r, .. } => rules = r,
                    Node::Closure { .. } => {
                        let (r, captured) = self.as_detached(v, "?")?;
                        rules = self.map_rules_with_frames(&r, captured)?;
                    }
                    _ => rules = Vec::new(),
                }
            }

            let val = if key.is_empty() {
                // Unnamed `[]` = the last declaration (property OR variable).
                let last = rules.iter().rev().find_map(|r| match r {
                    Node::Declaration(d) => {
                        // The lazy `parseValue` split: a raw capture's trailing
                        // `!important` is NOT part of the looked-up value.
                        let mut dd = d.clone();
                        split_anon_important(&mut dd);
                        Some((*dd.value).clone())
                    }
                    Node::VariableDecl { value, .. } => Some((**value).clone()),
                    _ => None,
                });
                last.ok_or_else(|| self.err(ErrorKind::Name, "property \"\" not found"))?
            } else if key.starts_with('@') {
                let name = if let Some(dynamic) = key.strip_prefix("@@") {
                    let inner = self.eval_variable(dynamic, Span::default())?;
                    value_to_plain_string(&inner)
                } else {
                    key[1..].to_string()
                };
                let found = rules.iter().rev().find_map(|r| match r {
                    Node::VariableDecl { name: n, value, .. } if *n == name => {
                        Some((**value).clone())
                    }
                    _ => None,
                });
                found.ok_or_else(|| {
                    self.err(ErrorKind::Name, format!("variable @{name} not found"))
                })?
            } else {
                let name = if let Some(dynamic) = key.strip_prefix("$@") {
                    let inner = self.eval_variable(dynamic, Span::default())?;
                    value_to_plain_string(&inner)
                } else {
                    key.strip_prefix('$').unwrap_or(key).to_string()
                };
                let decls: Vec<Node> = rules
                    .iter()
                    .filter(|r| matches!(r, Node::Declaration(d) if d.name == name))
                    .map(|r| match r {
                        Node::Declaration(d) => {
                            // parseValue split (see the unnamed branch): the
                            // trailing `!important` never joins the value.
                            let mut dd = d.clone();
                            split_anon_important(&mut dd);
                            Node::Declaration(dd)
                        }
                        other => other.clone(),
                    })
                    .collect();
                if decls.is_empty() {
                    return Err(
                        self.err(ErrorKind::Name, format!("property \"{name}\" not found"))
                    );
                }
                let merged = merge_rules(&decls);
                match merged.last() {
                    Some(Node::Declaration(d)) => (*d.value).clone(),
                    _ => unreachable!("merge_rules keeps declarations"),
                }
            };
            // Map-rule values are already evaluated; re-evaluate defensively for
            // raw nested content (idempotent on finished values). A raw-captured
            // Anonymous value re-parses so the result is OPERABLE
            // (`(@margins[ten]/2)` must divide — namespacing-3).
            let val = match &val {
                Node::Anonymous(text) if !text.contains(':') => self.reparse_and_eval(text)?,
                Node::Anonymous(_) => val.clone(),
                _ => self.eval_value(&val)?,
            };
            current = Some(val);
        }
        // A final ruleset value materializes as its evaluated rules.
        match current {
            Some(v @ Node::Closure { .. }) => {
                let (r, captured) = self.as_detached(v, "?")?;
                let rules = self.map_rules_with_frames(&r, captured)?;
                Ok(Node::DetachedRuleset {
                    rules,
                    span: Span::default(),
                })
            }
            Some(v) => Ok(v),
            None => Ok(Node::DetachedRuleset {
                rules,
                span: Span::default(),
            }),
        }
    }

    /// `$prop` — resolve a property accessor (less.js `Property.eval`, plan
    /// §2.12): the nearest frame declaring the property wins; all its same-name
    /// declarations merge (`+:`/`+_:`), the last one's value is evaluated in the
    /// CURRENT context, its `!important` propagates to the reading declaration,
    /// and the read triggers the parseValue important-trim quirk (see
    /// `pending_trims`).
    fn eval_property(&mut self, name: &str) -> Result<Node, LessError> {
        let key = format!("${name}");
        if self.evaluating.iter().any(|n| n == &key) {
            return Err(self.err(
                ErrorKind::Name,
                format!("Recursive property reference for {key}"),
            ));
        }

        let mut found: Option<(usize, Vec<Node>)> = None;
        for frame in &self.frames {
            let decls: Vec<Node> = frame
                .borrow()
                .iter()
                .filter(|r| matches!(r, Node::Declaration(d) if d.name == name))
                .cloned()
                .collect();
            if !decls.is_empty() {
                found = Some((Rc::as_ptr(frame) as *const () as usize, decls));
                break;
            }
        }
        let Some((fp, decls)) = found else {
            return Err(self.err(
                ErrorKind::Name,
                format!("Property '${name}' is undefined"),
            ));
        };
        self.pending_trims.push((fp, name.to_string()));

        // less.js `parseValue`: lazily-captured Anonymous values split their
        // trailing `!important` out of the raw text before merging.
        let decls: Vec<Node> = decls
            .into_iter()
            .map(|d| match d {
                Node::Declaration(mut dd) => {
                    split_anon_important(&mut dd);
                    Node::Declaration(dd)
                }
                other => other,
            })
            .collect();

        let merged = merge_rules(&decls);
        let Some(Node::Declaration(last)) = merged.last() else {
            unreachable!("merge_rules keeps declarations");
        };
        if !last.important.is_empty() {
            if let Some(slot) = self.important_scope.last_mut() {
                *slot = Some(" !important".to_string());
            }
        }
        let value = (*last.value).clone();
        self.evaluating.push(key);
        // The found value evaluates in the ACCESSING context (less.js
        // `Property.eval` runs `v.value.eval(context)` with the reader's
        // frames); a raw Anonymous capture is re-parsed first.
        let result = self.reparse_arg(&value);
        self.evaluating.pop();
        result
    }

    /// Bind a call's arguments to a candidate's parameters (less.js `evalParams`):
    /// named → positional → defaults, `@rest...` variadic, `@arguments` (§2.5).
    fn bind_params(&mut self, cand: &Candidate, args: &[EvArg]) -> Result<Vec<Node>, LessError> {
        let params = &cand.params;
        let mut frame: Vec<Node> = Vec::new();
        let mut evald: Vec<Option<Node>> = vec![None; params.len()];

        // Named args first.
        let mut remaining: Vec<EvArg> = Vec::new();
        for a in args {
            if let Some(name) = &a.name {
                let mut found = false;
                for (j, p) in params.iter().enumerate() {
                    if evald[j].is_none() {
                        if let Some(pn) = &p.name {
                            if pn.trim_start_matches('@') == name {
                                evald[j] = Some(a.value.clone());
                                frame.push(var_decl(name, a.value.clone()));
                                found = true;
                                break;
                            }
                        }
                    }
                }
                if !found {
                    return Err(self.err(
                        ErrorKind::Runtime,
                        format!("Named argument for {} @{name} not found", cand.name),
                    ));
                }
            } else {
                remaining.push(a.clone());
            }
        }

        let pos: Vec<Node> = remaining.into_iter().map(|a| a.value).collect();
        let mut arg_index = 0usize;
        for (i, p) in params.iter().enumerate() {
            if evald[i].is_some() {
                continue;
            }
            if let Some(pn) = &p.name {
                let key = pn.trim_start_matches('@');
                if p.variadic {
                    let rest: Vec<Node> = pos[arg_index.min(pos.len())..].to_vec();
                    let expr = if rest.len() == 1 {
                        rest[0].clone()
                    } else {
                        Node::Expression(rest.clone())
                    };
                    frame.push(var_decl(key, expr));
                    // `@arguments` includes the variadic-captured tail: grow the
                    // bound-value list past the param count so overflow args
                    // aren't dropped (less.js `evaldArguments[j] = …`, §2.5).
                    for (k, v) in pos.iter().enumerate().skip(arg_index) {
                        if k >= evald.len() {
                            evald.resize(k + 1, None);
                        }
                        evald[k] = Some(v.clone());
                    }
                } else if arg_index < pos.len() {
                    let v = pos[arg_index].clone();
                    frame.push(var_decl(key, v.clone()));
                    evald[i] = Some(v);
                } else if let Some(def) = &p.default {
                    let v = self.eval_default(def, &frame, cand)?;
                    frame.push(var_decl(key, v.clone()));
                    evald[i] = Some(v);
                } else {
                    return Err(self.err(
                        ErrorKind::Runtime,
                        format!("wrong number of arguments for {}", cand.name),
                    ));
                }
            } else if p.variadic {
                // Unnamed variadic `(...)` — no variable binds, but the args
                // still populate `@arguments` (less.js `evaldArguments`).
                for (k, v) in pos.iter().enumerate().skip(arg_index) {
                    if k >= evald.len() {
                        evald.resize(k + 1, None);
                    }
                    evald[k] = Some(v.clone());
                }
            } else if arg_index < pos.len() {
                // A literal pattern param consumes its position into @arguments.
                evald[i] = Some(pos[arg_index].clone());
            }
            arg_index += 1;
        }

        // `@arguments` — the bound values in parameter order, space-joined.
        let arg_values: Vec<Node> = evald.iter().flatten().cloned().collect();
        let arguments = if arg_values.len() == 1 {
            arg_values[0].clone()
        } else {
            Node::Expression(arg_values)
        };
        frame.push(var_decl("arguments", arguments));
        Ok(frame)
    }

    /// Evaluate a parameter default against the mixin's definition scope + the
    /// partially-bound param frame (the theming lever, §4.3 — a default resolves
    /// in the definition's scope, not the caller's).
    fn eval_default(
        &mut self,
        def: &Node,
        partial: &[Node],
        cand: &Candidate,
    ) -> Result<Node, LessError> {
        let mut frames: Vec<Frame> = Vec::with_capacity(self.frames.len() + 2);
        frames.push(frame_of(partial.to_vec()));
        frames.extend(cand.def_scope.iter().cloned());
        frames.extend(self.frames.iter().cloned());
        let saved = std::mem::replace(&mut self.frames, frames);
        let res = self.reparse_arg(def);
        self.frames = saved;
        res
    }

    /// Run `f` with the mixin frame stack (param frame + definition scope + caller)
    /// installed — used for guard evaluation (less.js `matchCondition`).
    fn with_mixin_frames<R>(
        &mut self,
        cand: &Candidate,
        param_frame: &[Node],
        f: impl FnOnce(&mut Self) -> Result<R, LessError>,
    ) -> Result<R, LessError> {
        let mut frames: Vec<Frame> = Vec::with_capacity(self.frames.len() + 2);
        frames.push(frame_of(param_frame.to_vec()));
        frames.extend(cand.def_scope.iter().cloned());
        frames.extend(self.frames.iter().cloned());
        let saved = std::mem::replace(&mut self.frames, dedup_frames(frames));
        let res = f(self);
        self.frames = saved;
        res
    }

    /// Mixin args/defaults are parsed as raw `Anonymous` text (the structural
    /// parser). Re-parse them as a value so they evaluate. Falls back to the raw
    /// anonymous text on a parse failure.
    fn reparse_arg(&mut self, node: &Node) -> Result<Node, LessError> {
        if let Node::Anonymous(s) = node {
            let src = s.trim();
            if src.is_empty() {
                return Ok(Node::Anonymous(String::new()));
            }
            if let Ok(v) = crate::parser::parse_value_fragment(src, self.opts) {
                return self.eval_value(&v);
            }
            return Ok(Node::Anonymous(src.to_string()));
        }
        self.eval_value(node)
    }

    // ------------------------------------------------------------------
    // Guards (plan §2.6): comma-OR, `and`, `not`, comparisons, type-check
    // functions, `default()`.
    // ------------------------------------------------------------------

    /// Evaluate a CSS/selector guard (`sel when (…)`) in the current scope.
    fn eval_guard(&mut self, guard: &Node) -> Result<bool, LessError> {
        let raw = guard_text(guard);
        self.eval_guard_str(&raw)
    }

    /// Evaluate a `when (...)` guard string. The boolean grammar mirrors less.js
    /// exactly (`conditions`/`condition`/`conditionAnd`/`negatedCondition`/
    /// `parenthesisCondition`/`atomicCondition`, §2.6): top-level commas OR whole
    /// conditions; `or` binds looser than `and`; `not` and `( … )` nest to any
    /// depth. Precedence: `,`/`or` < `and` < `not`/parens/atomic.
    fn eval_guard_str(&mut self, raw: &str) -> Result<bool, LessError> {
        let s = raw.trim();
        if s.is_empty() {
            return Ok(true);
        }
        // The outermost guard list: comma-separated conditions are OR'd.
        for clause in split_top(s, ',') {
            if self.eval_guard_or(&clause)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `or`-separated sub-conditions (OR — less.js `condition`'s `or` keyword).
    fn eval_guard_or(&mut self, s: &str) -> Result<bool, LessError> {
        for part in split_word(s, "or") {
            if self.eval_guard_and(&part)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `and`-separated terms (AND — less.js `conditionAnd`).
    fn eval_guard_and(&mut self, s: &str) -> Result<bool, LessError> {
        for part in split_word(s, "and") {
            if !self.eval_guard_term(part.trim())? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// One term: a `not <term>` negation, a parenthesized nested condition, or an
    /// atomic comparison / truthy value (less.js `negatedCondition` →
    /// `parenthesisCondition` → `atomicCondition`). Recurses so nested `not(…)`
    /// and arbitrarily deep parens evaluate correctly.
    fn eval_guard_term(&mut self, atom: &str) -> Result<bool, LessError> {
        let a = atom.trim();
        if let Some(rest) = strip_not(a) {
            return Ok(!self.eval_guard_term(rest.trim())?);
        }
        // A fully-enclosing `( … )` wraps a nested condition (not a value): strip
        // it and recurse into the condition grammar. `strip_outer_parens` returns
        // the inner only when the first `(` balances the last `)`.
        let inner = strip_outer_parens(a);
        if inner.len() != a.len() {
            return self.eval_guard_or(inner);
        }
        // Atomic: a comparison or a bare truthy value (`default()`, a type-check
        // function, a variable that resolves to the keyword `true`).
        self.eval_condition(a)
    }

    /// A condition inside a guard: `L op R`, or a bare truthy value.
    fn eval_condition(&mut self, inner: &str) -> Result<bool, LessError> {
        let inner = inner.trim();
        if let Some((l, op, r)) = split_comparison(inner) {
            let lv = self.reparse_and_eval(l.trim())?;
            let rv = self.reparse_and_eval(r.trim())?;
            let cmp = compare_values(&lv, &rv);
            return Ok(match op {
                "=" | "==" => cmp == Some(0),
                "<" => cmp == Some(-1),
                ">" => cmp == Some(1),
                "<=" | "=<" => matches!(cmp, Some(-1) | Some(0)),
                ">=" => matches!(cmp, Some(1) | Some(0)),
                _ => false,
            });
        }
        // Bare value: true iff it evaluates to the keyword `true` (raw-captured
        // declaration values may surface as Anonymous — same keyword).
        let v = self.reparse_and_eval(inner)?;
        Ok(matches!(&v, Node::Keyword(k) | Node::Anonymous(k) if k.trim() == "true"))
    }

    fn reparse_and_eval(&mut self, src: &str) -> Result<Node, LessError> {
        if let Ok(v) = crate::parser::parse_value_fragment(src, self.opts) {
            self.eval_value(&v)
        } else {
            Ok(Node::Anonymous(src.to_string()))
        }
    }

    // ------------------------------------------------------------------
    // Selectors & interpolation
    // ------------------------------------------------------------------

    fn render_selector(&mut self, sel: &Selector) -> Result<String, LessError> {
        let mut s = String::new();
        for el in &sel.elements {
            s.push_str(&combinator_css(&el.combinator));
            if el.value.contains("@{") || el.value.contains("${") {
                // Selector elements genCSS their evaluated value — a quoted
                // variable keeps its quotes (`.@{v}` with `@v: "sel"` →
                // `."sel"`, F18); escaped `~"…"` still renders raw.
                s.push_str(&self.interpolate_css(&el.value)?);
            } else {
                s.push_str(&el.value);
            }
        }
        Ok(s)
    }

    /// Evaluate a nestable at-rule's prelude to its comma-separated media
    /// queries, each normalized the way less.js's structured `mediaFeatures`
    /// parse + re-render does (plan §2.13): `(key:value)` → `(key: value)`
    /// with the value evaluated, comparison features get ` op ` spacing,
    /// variables (`@var`, `@{var}`, escaped strings) resolve.
    fn eval_media_features(&mut self, node: &Node) -> Result<Vec<String>, LessError> {
        let raw = match node {
            Node::Anonymous(s) => s.clone(),
            other => render_value(&self.eval_value(other)?, self.opts.num_precision),
        };
        let raw = if raw.contains("@{") || raw.contains("${") {
            self.interpolate(&raw)?
        } else {
            raw
        };
        let raw = self.resolve_prelude_vars(&raw)?;
        let mut queries = Vec::new();
        for q in split_top(&raw, ',') {
            let mut q = q.trim().to_string();
            if q.is_empty() {
                continue;
            }
            // A namespaced mixin-call lookup as the whole query
            // (`@media #ns.breakpoint(.valToGet[])[@max]`, namespacing-media):
            // evaluate it through the value machinery first.
            if (q.starts_with('#') || q.starts_with('.')) && q.contains('[') {
                if let Ok(v) = crate::parser::parse_value_fragment(&q, self.opts) {
                    if matches!(v, Node::Lookup { .. }) {
                        if let Ok(ev) = self.eval_value(&v) {
                            q = render_value(&ev, self.opts.num_precision);
                        }
                    }
                }
            }
            queries.push(self.normalize_media_query(&q)?);
        }
        Ok(queries)
    }

    /// One media query: space-joined words and `( … )` feature groups; a word
    /// glued to a paren (`style(…)`, `layer(…)`, `supports(…)`) keeps no space.
    fn normalize_media_query(&mut self, q: &str) -> Result<String, LessError> {
        let bytes = q.as_bytes();
        let mut parts: Vec<(String, bool)> = Vec::new(); // (text, glued-to-previous)
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if b == b'(' {
                // Balanced group.
                let mut depth = 0i32;
                let start = i;
                while i < bytes.len() {
                    match bytes[i] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                let inner = &q[start + 1..i.saturating_sub(1).max(start + 1)];
                // A paren glued to the preceding word stays attached
                // (`style(…)`, `layer(…)`) — EXCEPT after the media keywords,
                // which always force a space (less.js `mediaFeature` spacing).
                let glued = parts
                    .last()
                    .map(|(t, _)| {
                        !t.is_empty()
                            && start > 0
                            && !bytes[start - 1].is_ascii_whitespace()
                            && !matches!(
                                t.to_ascii_lowercase().as_str(),
                                "and" | "or" | "not" | "only"
                            )
                    })
                    .unwrap_or(false);
                let norm = self.normalize_media_feature(inner)?;
                parts.push((format!("({norm})"), glued));
                continue;
            }
            // A word / raw run up to whitespace or `(`.
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'(' {
                i += 1;
            }
            parts.push((q[start..i].to_string(), false));
        }
        let mut out = String::new();
        for (idx, (text, glued)) in parts.iter().enumerate() {
            if idx > 0 && !glued {
                out.push(' ');
            }
            out.push_str(text);
        }
        Ok(out)
    }

    /// The inside of a `( … )` media feature: `key: value` evaluates the value
    /// (escaped strings render raw); anything else (range syntax, nested
    /// conditions) collapses whitespace and normalizes comparison spacing.
    fn normalize_media_feature(&mut self, inner: &str) -> Result<String, LessError> {
        let inner = inner.trim();
        // Top-level `key: value`?
        if let Some(colon) = find_top_level_colon(inner) {
            let (lhs, rhs) = inner.split_at(colon);
            let rhs = rhs[1..].trim();
            let value = match crate::parser::parse_value_fragment(rhs, self.opts) {
                Ok(v) => {
                    let ev = self.eval_value(&v)?;
                    render_value(&ev, self.opts.num_precision)
                }
                Err(_) => rhs.to_string(),
            };
            return Ok(format!("{}: {}", lhs.trim(), value));
        }
        // Range / boolean feature: single-space words, ` op ` comparisons,
        // nested groups normalized recursively.
        let bytes = inner.as_bytes();
        let mut out = String::new();
        let mut i = 0;
        let mut pending_space = false;
        let push_tok = |out: &mut String, tok: &str, pending: &mut bool| {
            if *pending && !out.is_empty() {
                out.push(' ');
            }
            out.push_str(tok);
            *pending = false;
        };
        while i < bytes.len() {
            let b = bytes[i];
            if b.is_ascii_whitespace() {
                pending_space = true;
                i += 1;
                continue;
            }
            match b {
                b'(' => {
                    let mut depth = 0i32;
                    let start = i;
                    while i < bytes.len() {
                        match bytes[i] {
                            b'(' => depth += 1,
                            b')' => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                    let sub = &inner[start + 1..i.saturating_sub(1).max(start + 1)];
                    let norm = self.normalize_media_feature(sub)?;
                    push_tok(&mut out, &format!("({norm})"), &mut pending_space);
                }
                b'<' | b'>' | b'=' => {
                    let start = i;
                    while i < bytes.len() && matches!(bytes[i], b'<' | b'>' | b'=') {
                        i += 1;
                    }
                    pending_space = true;
                    push_tok(&mut out, &inner[start..i], &mut pending_space);
                    pending_space = true;
                }
                _ => {
                    let start = i;
                    while i < bytes.len()
                        && !bytes[i].is_ascii_whitespace()
                        && !matches!(bytes[i], b'(' | b'<' | b'>' | b'=')
                    {
                        i += 1;
                    }
                    push_tok(&mut out, &inner[start..i], &mut pending_space);
                }
            }
        }
        Ok(out)
    }

    fn eval_prelude(&mut self, node: &Node) -> Result<String, LessError> {
        match node {
            Node::Anonymous(s) => {
                let s = if s.contains("@{") || s.contains("${") {
                    self.interpolate(s)?
                } else {
                    s.clone()
                };
                // At-rule preludes reference bare `@var`s (`@namespace @ns "…"`,
                // `@media (min-width: @w)`) — resolve those that are defined.
                self.resolve_prelude_vars(&s)
            }
            other => Ok(render_value(&self.eval_value(other)?, self.opts.num_precision)),
        }
    }

    /// Substitute bare `@name` variable references in a prelude string with their
    /// CSS values (leaving undefined `@…` — e.g. an `@media` keyword — untouched).
    /// Textually resolve `@refs` inside verbatim-captured runs of an evaluated
    /// value (recursing through lists) — map entries referencing sibling
    /// permissive captures (`@max: not all and @min`, namespacing-media).
    fn resolve_anon_refs(&mut self, v: Node) -> Result<Node, LessError> {
        Ok(match v {
            Node::Anonymous(t) if t.contains('@') && !t.contains('[') => {
                Node::Anonymous(self.resolve_prelude_vars(&t)?)
            }
            Node::Expression(items) => Node::Expression(
                items
                    .into_iter()
                    .map(|i| self.resolve_anon_refs(i))
                    .collect::<Result<_, _>>()?,
            ),
            Node::Value(items) => Node::Value(
                items
                    .into_iter()
                    .map(|i| self.resolve_anon_refs(i))
                    .collect::<Result<_, _>>()?,
            ),
            other => other,
        })
    }

    fn resolve_prelude_vars(&mut self, s: &str) -> Result<String, LessError> {
        // Fixpoint: a resolved variable's text may itself contain `@refs`
        // (permissively-captured media features, namespacing-media).
        let mut cur = s.to_string();
        for _ in 0..8 {
            let next = self.resolve_prelude_vars_once(&cur)?;
            if next == cur {
                break;
            }
            cur = next;
        }
        Ok(cur)
    }

    fn resolve_prelude_vars_once(&mut self, s: &str) -> Result<String, LessError> {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'@' && bytes.get(i + 1) != Some(&b'{') {
                let mut j = i + 1;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-' || bytes[j] == b'_')
                {
                    j += 1;
                }
                let name = &s[i + 1..j];
                if !name.is_empty() {
                    // A lookup chain on the variable (`@breakpoints[mobile]`,
                    // namespacing-3): route through the Lookup machinery.
                    if bytes.get(j) == Some(&b'[') {
                        let mut k = j;
                        let mut keys: Vec<String> = Vec::new();
                        while bytes.get(k) == Some(&b'[') {
                            let close = match s[k..].find(']') {
                                Some(c) => k + c,
                                None => break,
                            };
                            keys.push(s[k + 1..close].to_string());
                            k = close + 1;
                        }
                        if !keys.is_empty() {
                            let target = Node::VariableCall {
                                name: name.to_string(),
                                span: Default::default(),
                            };
                            if let Ok(v) = self.eval_lookup(&target, &keys) {
                                out.push_str(&value_to_plain_string(&v));
                                i = k;
                                continue;
                            }
                        }
                    }
                    if let Ok(v) = self.eval_variable(name, Default::default()) {
                        out.push_str(&value_to_plain_string(&v));
                        i = j;
                        continue;
                    }
                }
            }
            // advance one UTF-8 char
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&s[i..i + ch_len]);
            i += ch_len;
        }
        Ok(out)
    }

    /// Resolve `@{name}` (and `${name}`) interpolation in a string, iteratively to
    /// a fixpoint (plan §2.1).
    /// `@{name}` string interpolation, mirroring less.js `Quoted.eval`'s
    /// `iterativeReplace` over `/@\{([\w-]+)\}/g` (plan §2.1/§2.14): only simple
    /// `[\w-]+` names match — so in `@{box-@{suffix}}` the INNER interpolation
    /// resolves first — and passes repeat to a fixpoint, which is what makes
    /// iterated interpolation (`@{box-large}` produced by a pass) resolve too.
    fn interpolate(&mut self, input: &str) -> Result<String, LessError> {
        self.interpolate_with(input, false)
    }

    /// Selector / property-name interpolation: like [`Self::interpolate`] but a
    /// non-escaped Quoted value renders WITH its quotes (less.js genCSSes the
    /// evaluated node there instead of taking `.value` — plan §2.14 boundary).
    fn interpolate_css(&mut self, input: &str) -> Result<String, LessError> {
        self.interpolate_with(input, true)
    }

    fn interpolate_with(&mut self, input: &str, css: bool) -> Result<String, LessError> {
        let mut s = input.to_string();
        for _ in 0..100 {
            let mut out = String::with_capacity(s.len());
            let mut rest = s.as_str();
            let mut replaced = false;
            while let Some((start, is_prop)) = find_interp(rest) {
                let after = &rest[start + 2..];
                // A match needs a `}` with only `[\w-]` name chars before it.
                let end_rel = after.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'));
                match end_rel {
                    Some(e) if e > 0 && after[e..].starts_with('}') => {
                        let name = &after[..e];
                        // `${prop}` reads a property, `@{var}` a variable
                        // (less.js `Quoted.eval`'s two replacement passes).
                        let val = if is_prop {
                            self.eval_property(name)?
                        } else {
                            self.eval_variable(name, Default::default())?
                        };
                        out.push_str(&rest[..start]);
                        if css {
                            out.push_str(&render_value(&val, 0));
                        } else {
                            out.push_str(&value_to_plain_string(&val));
                        }
                        rest = &after[e + 1..];
                        replaced = true;
                    }
                    _ => {
                        // Not a simple name — emit the opener literally, move on.
                        out.push_str(&rest[..start + 2]);
                        rest = after;
                    }
                }
            }
            out.push_str(rest);
            s = out;
            if !replaced {
                break;
            }
        }
        Ok(s)
    }
}

// ----------------------------------------------------------------------
// Frame helpers
// ----------------------------------------------------------------------

/// Scan a frame's rules for the last `@name` declaration (last-wins), returning
/// its unevaluated value + whether it was `!important`.
fn frame_variable(frame: &Frame, name: &str) -> Option<(Node, bool)> {
    let mut result = None;
    for r in frame.borrow().iter() {
        match r {
            Node::VariableDecl {
                name: n,
                value,
                important,
                ..
            } => {
                if n == name {
                    result = Some(((**value).clone(), !important.is_empty()));
                }
            }
            // less.js `Ruleset.variables()` peeks into a not-yet-inlined
            // import's root (§2.9) — how a variable-path import can use
            // variables from a file imported later in source.
            Node::ImportResolved(ir) => {
                for inner in &ir.rules {
                    if let Node::VariableDecl {
                        name: n,
                        value,
                        important,
                        ..
                    } = inner
                    {
                        if n == name {
                            result = Some(((**value).clone(), !important.is_empty()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    result
}

/// Whether a frame already declares `@name` (for scope-injection filtering).
fn frame_has_var(frame: &Frame, name: &str) -> bool {
    frame
        .borrow()
        .iter()
        .any(|r| matches!(r, Node::VariableDecl { name: n, .. } if n == name))
}

/// Normalize a run of selector elements into the mixin lookup names (less.js
/// `Selector.mixinElements`): join `combinator+value`, extract each
/// `[&#*.\w-]([\w-]|\.)*` token, dropping a leading bare `&` (plan §2.5).
fn mixin_names(elements: &[Element]) -> Vec<String> {
    let mut joined = String::new();
    for e in elements {
        joined.push_str(&e.combinator);
        joined.push_str(&e.value);
    }
    let mut names = extract_mixin_tokens(&joined);
    if names.first().map(|s| s == "&").unwrap_or(false) {
        names.remove(0);
    }
    names
}

/// Extract the `[,&#*.\w-]([\w-]|(\\.))*` tokens from a joined selector string.
fn extract_mixin_tokens(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let is_start = |b: u8| matches!(b, b',' | b'&' | b'#' | b'*' | b'.' | b'-' | b'_')
        || b.is_ascii_alphanumeric();
    let is_cont = |b: u8| matches!(b, b'-' | b'_') || b.is_ascii_alphanumeric();
    while i < bytes.len() {
        if is_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else if is_cont(bytes[i]) {
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(s[start..i].to_string());
        } else {
            i += 1;
        }
    }
    out
}

/// Whether a mixin/namespace accepts a **zero-argument** call — less.js
/// `matchArgs(null)`. A namespace segment on a call path (`#ns > .m()`) is only
/// traversed if it does (§2.6), and intermediate namespaces receive no args.
fn accepts_zero_args(params: &[MixinParam]) -> bool {
    params
        .iter()
        .all(|p| p.variadic || (p.name.is_some() && p.default.is_some()))
}

/// Collect mixin candidates matching `path` in a rule list, recursing into
/// namespaces (less.js `Ruleset.find`). Each recursion prepends the namespace's
/// body as a definition-scope frame (closure capture, plan §4.3). `path_guards`
/// accumulates the guards of the namespace segments already traversed, so a
/// leaf candidate carries every `#ns when (…)` guard on its path (§2.6).
/// Interpolate `@{name}` references in a mixin/namespace NAME against the
/// definition-scope frames (less.js evaluates ruleset selectors before they
/// land in frames, so lookup sees resolved names — mixins-interpolated).
fn interp_name(name: &str, frames: &[Frame]) -> String {
    if !name.contains("@{") {
        return name.to_string();
    }
    let mut cur = name.to_string();
    for _ in 0..4 {
        let Some(pos) = cur.find("@{") else { break };
        let Some(end_rel) = cur[pos..].find('}') else { break };
        let end = pos + end_rel;
        let var = cur[pos + 2..end].to_string();
        let mut rep = String::new();
        for f in frames {
            if let Some((val, _)) = frame_variable(f, &var) {
                // Follow variable-to-variable indirection (`@c1: @a1`).
                let mut v = val;
                for _ in 0..8 {
                    let Node::Variable { name: n, .. } = &v else { break };
                    let n = n.trim_start_matches('@').to_string();
                    match frames.iter().find_map(|f| frame_variable(f, &n)) {
                        Some((next, _)) => v = next,
                        None => break,
                    }
                }
                rep = value_to_plain_string(&v);
                break;
            }
        }
        cur.replace_range(pos..=end, &rep);
    }
    cur
}

fn find_candidates(
    rules: &[Node],
    path: &[String],
    def_scope: &[Frame],
    closures: &[Vec<Frame>],
    path_guards: &[Node],
    follow_imports: bool,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for r in rules {
        match r {
            // An un-inlined import's top level is part of this rule list for
            // lookup purposes (less.js peeks inside Import roots, §2.9) — the
            // `#ns { @import (reference) "…" }` namespaced-mixin case.
            Node::ImportResolved(ir) if !ir.skip && follow_imports => {
                out.extend(find_candidates(&ir.rules, path, def_scope, closures, path_guards, true));
            }
            // A scope-injected closure: resolve against the frames frozen at
            // injection (the enclosing mixin's bound params), not the caller's.
            Node::Closure { inner, scope } => {
                if let Node::Ruleset(rs) = inner.as_ref() {
                    let captured = &closures[*scope as usize];
                    for sel in &rs.selectors {
                        let mut joined = String::new();
                        for e in &sel.elements {
                            joined.push_str(&e.combinator);
                            joined.push_str(&e.value);
                        }
                        let joined = interp_name(&joined, captured);
                        let mut names = extract_mixin_tokens(&joined);
                        if names.first().map(|s| s == "&").unwrap_or(false) {
                            names.remove(0);
                        }
                        if let Some(m) = match_prefix(path, &names) {
                            if m == path.len() {
                                out.push(Candidate {
                                    name: joined.clone(),
                                    params: Vec::new(),
                                    guard: sel.guard.as_deref().cloned(),
                                    rules: rs.rules.clone(),
                                    def_scope: captured.to_vec(),
                                    path_guards: path_guards.to_vec(),
                                    ruleset_span: Some(rs.span),
                                });
                            } else {
                                let mut inner_scope = vec![frame_of(rs.rules.clone())];
                                inner_scope.extend(captured.iter().cloned());
                                let child = push_guard(path_guards, sel.guard.as_deref());
                                out.extend(find_candidates(&rs.rules, &path[m..], &inner_scope, closures, &child, true));
                            }
                            break;
                        }
                    }
                }
                if let Node::MixinDefinition(def) = inner.as_ref() {
                    let captured = &closures[*scope as usize];
                    let names = extract_names_dropamp(&interp_name(&def.name, captured));
                    if let Some(m) = match_prefix(path, &names) {
                        if m == path.len() {
                            out.push(Candidate {
                                name: def.name.clone(),
                                params: def.params.clone(),
                                guard: def.guard.as_deref().cloned(),
                                rules: def.rules.clone(),
                                def_scope: captured.to_vec(),
                                path_guards: path_guards.to_vec(),
                                ruleset_span: None,
                            });
                        } else if accepts_zero_args(&def.params) {
                            let mut inner_scope = vec![frame_of(def.rules.clone())];
                            inner_scope.extend(captured.iter().cloned());
                            let child = push_guard(path_guards, def.guard.as_deref());
                            out.extend(find_candidates(&def.rules, &path[m..], &inner_scope, closures, &child, true));
                        }
                    }
                }
            }
            Node::MixinDefinition(def) => {
                let names = extract_names_dropamp(&interp_name(&def.name, def_scope));
                if let Some(m) = match_prefix(path, &names) {
                    if m == path.len() {
                        out.push(Candidate {
                            name: def.name.clone(),
                            params: def.params.clone(),
                            guard: def.guard.as_deref().cloned(),
                            rules: def.rules.clone(),
                            def_scope: def_scope.to_vec(),
                            path_guards: path_guards.to_vec(),
                            ruleset_span: None,
                        });
                    } else if accepts_zero_args(&def.params) {
                        // A parametric namespace is only entered with zero args
                        // (its args aren't the call's args); its guard joins the
                        // path guards.
                        let mut inner_scope = vec![frame_of(def.rules.clone())];
                        inner_scope.extend(def_scope.iter().cloned());
                        let child = push_guard(path_guards, def.guard.as_deref());
                        out.extend(find_candidates(&def.rules, &path[m..], &inner_scope, closures, &child, true));
                    }
                }
            }
            Node::Ruleset(rs) => {
                for sel in &rs.selectors {
                    let mut joined = String::new();
                    for e in &sel.elements {
                        joined.push_str(&e.combinator);
                        joined.push_str(&e.value);
                    }
                    let joined = interp_name(&joined, def_scope);
                    let mut names = extract_mixin_tokens(&joined);
                    if names.first().map(|s| s == "&").unwrap_or(false) {
                        names.remove(0);
                    }
                    if let Some(m) = match_prefix(path, &names) {
                        if m == path.len() {
                            out.push(Candidate {
                                name: rs.selectors[0]
                                    .elements
                                    .first()
                                    .map(|e| e.value.clone())
                                    .unwrap_or_default(),
                                params: Vec::new(),
                                guard: sel.guard.as_deref().cloned(),
                                rules: rs.rules.clone(),
                                def_scope: def_scope.to_vec(),
                                path_guards: path_guards.to_vec(),
                                ruleset_span: Some(rs.span),
                            });
                        } else {
                            // A ruleset namespace has no params (always zero-arg);
                            // its selector guard joins the path guards.
                            let mut inner_scope = vec![frame_of(rs.rules.clone())];
                            inner_scope.extend(def_scope.iter().cloned());
                            let child = push_guard(path_guards, sel.guard.as_deref());
                            out.extend(find_candidates(&rs.rules, &path[m..], &inner_scope, closures, &child, true));
                        }
                        break; // one selector per ruleset matches the prefix
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Append an optional namespace guard to the accumulated path-guard list.
fn push_guard(base: &[Node], guard: Option<&Node>) -> Vec<Node> {
    let mut v = base.to_vec();
    if let Some(g) = guard {
        v.push(g.clone());
    }
    v
}

/// Normalize a mixin-definition name (`.m`, `#ns`) into lookup tokens.
fn extract_names_dropamp(name: &str) -> Vec<String> {
    let mut n = extract_mixin_tokens(name);
    if n.first().map(|s| s == "&").unwrap_or(false) {
        n.remove(0);
    }
    n
}

/// If `def_names` is a prefix of `path`, return its length (elements matched).
fn match_prefix(path: &[String], def_names: &[String]) -> Option<usize> {
    if def_names.is_empty() || def_names.len() > path.len() {
        return None;
    }
    for (a, b) in path.iter().zip(def_names.iter()) {
        if a != b {
            return None;
        }
    }
    Some(def_names.len())
}

/// Split a trailing `!important` out of a declaration whose value is a raw
/// `Anonymous` capture (the less.js lazy-declaration `parseValue` step, plan
/// §2.12): the flag moves to `important` WITHOUT a leading space.
fn split_anon_important(d: &mut Declaration) {
    if !d.important.is_empty() {
        return;
    }
    if let Node::Anonymous(s) = d.value.as_ref() {
        let trimmed = s.trim_end();
        if let Some(head) = trimmed.strip_suffix("important") {
            let head = head.trim_end();
            if let Some(head) = head.strip_suffix('!') {
                d.value = Box::new(Node::Anonymous(head.trim_end().to_string()));
                d.important = "!important".to_string();
            }
        }
    }
}

/// Build a `@name: value` variable declaration node (for a param/injection frame).
fn var_decl(name: &str, value: Node) -> Node {
    Node::VariableDecl {
        name: name.to_string(),
        value: Box::new(value),
        important: String::new(),
        span: Span::default(),
    }
}

/// The raw guard text of a stored guard node.
fn guard_text(guard: &Node) -> String {
    match guard {
        Node::Anonymous(s) => s.clone(),
        other => render_value(other, 8),
    }
}

/// Format a mixin call for a "no matching definition" error message.
fn format_call(path: &[String], args: &[EvArg], np: u8) -> String {
    let mut s = path.join("");
    s.push('(');
    let parts: Vec<String> = args
        .iter()
        .map(|a| {
            let v = render_value(&a.value, np);
            match &a.name {
                Some(n) => format!("@{n}:{v}"),
                None => v,
            }
        })
        .collect();
    s.push_str(&parts.join(", "));
    s.push(')');
    s
}

/// Force `!important` onto every declaration in an output node (`.m() !important`).
fn make_important_node(node: &mut Node) {
    if let Node::Declaration(d) = node {
        if d.important.is_empty() {
            d.important = " !important".to_string();
        }
    }
}

/// Force `!important` through an output block (recursing into rulesets/at-rules).
fn make_important_out(out: &mut Out) {
    match out {
        Out::Rule { decls, .. } => {
            for d in decls.iter_mut() {
                make_important_node(d);
            }
        }
        Out::Decls(decls) => {
            for d in decls.iter_mut() {
                make_important_node(d);
            }
        }
        Out::Nested(node) => {
            if let Node::AtRule(a) = node {
                if let AtRuleBlock::Rules(rules) = &mut a.block {
                    for d in rules.iter_mut() {
                        make_important_node(d);
                    }
                }
            }
        }
        Out::At { body, .. } => {
            if let AtBody::Rules(inner) = body {
                for o in inner.iter_mut() {
                    make_important_out(o);
                }
            }
        }
        Out::Hidden(inner) => make_important_out(inner),
        Out::Comment(_) | Out::Verbatim(_) => {}
    }
}

/// Strip a leading `not` (keyword) from a guard atom, returning the remainder.
fn strip_not(a: &str) -> Option<&str> {
    let a = a.trim_start();
    let rest = a.strip_prefix("not")?;
    // `not` must be a whole word (followed by whitespace or `(`).
    match rest.chars().next() {
        Some(c) if c.is_whitespace() || c == '(' => Some(rest),
        _ => None,
    }
}

/// Strip one balanced outer `( … )` pair from a string.
fn strip_outer_parens(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with('(') && s.ends_with(')') {
        // Verify the first `(` matches the last `)`.
        let bytes = s.as_bytes();
        let mut depth = 0i32;
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return if i == bytes.len() - 1 {
                            s[1..s.len() - 1].trim()
                        } else {
                            s
                        };
                    }
                }
                _ => {}
            }
        }
    }
    s
}

/// Split a condition on a top-level comparison operator (`=<`/`<=`/`>=`/`=`/`<`/
/// `>`), returning `(lhs, op, rhs)` if one is present.
fn split_comparison(s: &str) -> Option<(&str, &'static str, &str)> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'"' | b'\'' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            _ if depth == 0 => {
                // Two-char operators first.
                if s[i..].starts_with("=<") || s[i..].starts_with("<=") {
                    return Some((&s[..i], if s[i..].starts_with("=<") { "=<" } else { "<=" }, &s[i + 2..]));
                }
                if s[i..].starts_with(">=") {
                    return Some((&s[..i], ">=", &s[i + 2..]));
                }
                if bytes[i] == b'=' {
                    return Some((&s[..i], "=", &s[i + 1..]));
                }
                if bytes[i] == b'<' {
                    return Some((&s[..i], "<", &s[i + 1..]));
                }
                if bytes[i] == b'>' {
                    return Some((&s[..i], ">", &s[i + 1..]));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ----------------------------------------------------------------------
// Value → string / comparison helpers
// ----------------------------------------------------------------------

/// Coerce a keyword that names a color into a `Color` (for operations), leaving
/// other nodes unchanged (less.js parses named colors as colors up front).
fn coerce_color(node: Node) -> Node {
    if let Node::Keyword(k) = &node {
        if let Some(c) = Color::from_keyword(k) {
            return Node::Color(c);
        }
    }
    node
}

/// The plain string form of an evaluated value (for interpolation / `@@` / guard
/// equality) — no surrounding quotes for quoted strings (plan §2.1). Numbers
/// render UNROUNDED (less.js `Quoted.eval` calls `v.toCSS()` with no context,
/// so no `numPrecision` fround applies — `@{x}` with `@x: pi()` interpolates as
/// `3.141592653589793`).
fn value_to_plain_string(node: &Node) -> String {
    match node {
        Node::Quoted { value, .. } => value.clone(),
        Node::Keyword(k) => k.clone(),
        Node::Anonymous(s) => s.clone(),
        other => render_value(other, 0),
    }
}

/// Compare two evaluated values for a guard (less.js `Node.compare`, plan §2.6).
/// Returns -1/0/1, or `None` for "not comparable / not equal". Named colors are
/// coerced first so `@c = red` compares as colors.
fn compare_values(a: &Node, b: &Node) -> Option<i32> {
    let a = coerce_color(a.clone());
    let b = coerce_color(b.clone());
    // Array-like values (space `Expression` / comma `Value`) compare element-wise,
    // but only within the SAME list kind (a space list never equals a comma list).
    if let (Some(av), Some(bv)) = (as_list(&a), as_list(&b)) {
        if std::mem::discriminant(&a) != std::mem::discriminant(&b) || av.len() != bv.len() {
            return None;
        }
        for (x, y) in av.iter().zip(bv.iter()) {
            if compare_values(x, y) != Some(0) {
                return None;
            }
        }
        return Some(0);
    }
    if has_compare(&a) && !is_quoted_or_anon(&b) {
        node_compare(&a, &b)
    } else if has_compare(&b) {
        node_compare(&b, &a).map(|c| -c)
    } else if std::mem::discriminant(&a) != std::mem::discriminant(&b) {
        None
    } else if value_to_plain_string(&a) == value_to_plain_string(&b) {
        Some(0)
    } else {
        None
    }
}

/// A space/comma list's items, if `n` is an `Expression`/`Value` of ≥2 items.
fn as_list(n: &Node) -> Option<&[Node]> {
    match n {
        Node::Expression(v) | Node::Value(v) if v.len() >= 2 => Some(v),
        _ => None,
    }
}

/// Nodes with a less.js `compare` method (Dimension/Quoted/Color/Anonymous).
fn has_compare(n: &Node) -> bool {
    matches!(
        n,
        Node::Dimension(_) | Node::Quoted { .. } | Node::Color(_) | Node::Anonymous(_)
    )
}

fn is_quoted_or_anon(n: &Node) -> bool {
    matches!(n, Node::Quoted { .. } | Node::Anonymous(_))
}

/// `a.compare(b)` where `a` is a Dimension/Quoted/Color.
fn node_compare(a: &Node, b: &Node) -> Option<i32> {
    match a {
        Node::Dimension(da) => {
            let Node::Dimension(db) = b else { return None };
            let (ua, ub) = if da.unit.is_empty() || db.unit.is_empty() {
                (da.clone(), db.clone())
            } else {
                let (ua, ub) = (da.unify(), db.unify());
                if ua.unit.to_unit_string() != ub.unit.to_unit_string() {
                    return None;
                }
                (ua, ub)
            };
            Some(match ua.value.partial_cmp(&ub.value) {
                Some(std::cmp::Ordering::Less) => -1,
                Some(std::cmp::Ordering::Greater) => 1,
                _ => 0,
            })
        }
        Node::Quoted { escaped: ea, value: va, .. } => {
            if let Node::Quoted { escaped: eb, value: vb, .. } = b {
                if !ea && !eb {
                    return Some(if va == vb {
                        0
                    } else if va < vb {
                        -1
                    } else {
                        1
                    });
                }
            }
            // Mixed / escaped → toCSS equality.
            if render_value(a, 8) == render_value(b, 8) {
                Some(0)
            } else {
                None
            }
        }
        Node::Color(ca) => {
            let Node::Color(cb) = b else { return None };
            if ca.rgb == cb.rgb && ca.alpha == cb.alpha {
                Some(0)
            } else {
                None
            }
        }
        Node::Anonymous(_) => {
            // less.js `Anonymous.compare` — toCSS equality only.
            if render_value(a, 8) == render_value(b, 8) {
                Some(0)
            } else {
                None
            }
        }
        _ => None,
    }
}

// ----------------------------------------------------------------------
// Selector joining (JoinSelector, string-level)
// ----------------------------------------------------------------------

/// less.js `Selector.isJustParentSelector`: a lone `&` element (descendant/empty
/// combinator) — the marker for a foldable bare-`&` child ruleset (§2.2).
fn is_just_parent(sel: &Selector) -> bool {
    sel.elements.len() == 1
        && sel.elements[0].value == "&"
        && matches!(sel.elements[0].combinator.as_str(), "" | " ")
}

/// less.js `isPathRelative` (§2.18): not protocol/absolute/fragment.
/// less.js `url.js` `escapePath`: backslash-escape `( ) ' "` + whitespace.
fn escape_url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        if matches!(ch, '(' | ')' | '\'' | '"') || ch.is_whitespace() {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The raw path text of an `@import` target (quoted string or `url(...)`).
fn import_path_text(path: &Node) -> String {
    match path {
        Node::Quoted { value, .. } => value.clone(),
        Node::Url(inner) => import_path_text(inner),
        Node::Anonymous(t) | Node::Keyword(t) => t.clone(),
        _ => String::new(),
    }
}

/// less.js `Import` css-path test: `/[#.&?]css([?;].*)?$/`.
fn import_path_is_css(path: &str) -> bool {
    for (i, _) in path.match_indices("css") {
        if i == 0 {
            continue;
        }
        if !matches!(path.as_bytes()[i - 1], b'#' | b'.' | b'&' | b'?') {
            continue;
        }
        let rest = &path[i + 3..];
        if rest.is_empty() || rest.starts_with('?') || rest.starts_with(';') {
            return true;
        }
    }
    false
}

fn is_path_relative(path: &str) -> bool {
    // /^(?:[a-z-]+:|\/|#)/i
    if path.starts_with('/') || path.starts_with('#') {
        return false;
    }
    if let Some(colon) = path.find(':') {
        if colon > 0
            && path[..colon]
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '-')
        {
            return false;
        }
    }
    true
}

/// less.js `isPathLocalRelative`: starts with `.`.
fn is_path_local_relative(path: &str) -> bool {
    path.starts_with('.')
}

/// less.js `contexts.Eval.normalizePath`: collapse `.` and `x/..` segments.
fn normalize_path(path: &str) -> String {
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

/// less.js `contexts.Eval.rewritePath`: prepend the rootpath, normalize, and
/// keep an explicitly-local path explicitly local.
fn rewrite_path(path: &str, rootpath: &str) -> String {
    let mut new_path = normalize_path(&format!("{rootpath}{path}"));
    if is_path_local_relative(path)
        && is_path_relative(rootpath)
        && !is_path_local_relative(&new_path)
    {
        new_path = format!("./{new_path}");
    }
    new_path
}

/// Interpolate `@{var}` references in a raw string against a stack of raw
/// rule lists (outermost first) — the stage-1 import pass's path evaluation
/// (plan §2.9). `None` when a variable can't resolve.
pub(crate) fn interpolate_standalone(
    raw: &str,
    frames_outer_first: &[Vec<Node>],
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
) -> Option<String> {
    let mut ctx = Ctx::new(opts, resolver);
    for list in frames_outer_first {
        ctx.push_frame(frame_of(list.clone()));
    }
    ctx.interpolate(raw).ok()
}

/// Strip a vendor prefix from an at-rule name (`@-moz-document` → `@document`,
/// less.js `nonVendorSpecificName`).
fn base_at_name(name: &str) -> String {
    let bytes = name.as_bytes();
    if bytes.len() > 2 && bytes[1] == b'-' {
        if let Some(dash) = name[2..].find('-') {
            return format!("@{}", &name[2 + dash + 1..]);
        }
    }
    name.to_string()
}

/// Cross-multiply the feature lists of a nested at-rule path and `and`-join
/// each combination (less.js `permute` + `evalNested`): the FIRST list varies
/// fastest — `(a, b and c)` nested with `(d, e)` → `a and d`, `b and c and d`,
/// `a and e`, `b and c and e`.
fn permute_feature_paths(lists: &[Vec<String>]) -> Vec<String> {
    fn permute(lists: &[Vec<String>]) -> Vec<Vec<String>> {
        match lists.len() {
            0 => Vec::new(),
            1 => lists[0].iter().map(|s| vec![s.clone()]).collect(),
            _ => {
                let rest = permute(&lists[1..]);
                let mut result = Vec::with_capacity(rest.len() * lists[0].len());
                for r in &rest {
                    for x in &lists[0] {
                        let mut combo = Vec::with_capacity(r.len() + 1);
                        combo.push(x.clone());
                        combo.extend(r.iter().cloned());
                        result.push(combo);
                    }
                }
                result
            }
        }
    }
    permute(lists)
        .into_iter()
        .map(|combo| combo.join(" and "))
        .collect()
}

/// The byte index of a top-level `:` (outside parens/brackets/strings), if any.
fn find_top_level_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'"' | b'\'' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            b':' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Combine parent paths with a ruleset's own selectors, resolving `&` (§2.2/§4).
fn join_selectors(parent: Option<&[String]>, own: &[String]) -> Vec<String> {
    let parents: Vec<String> = match parent {
        Some(p) if !p.is_empty() => p.to_vec(),
        _ => vec![String::new()],
    };
    let mut out = Vec::new();
    for os in own {
        if os.contains('&') {
            // EACH `&` occurrence expands over the parent paths independently —
            // a selector with N `&`s yields parents^N combinations, leftmost
            // `&` varying slowest (less.js `replaceParentSelector`; the
            // extend-nest amp-test case). Leading-trim only: a trailing `&`
            // replaced by an empty root keeps its descendant space
            // (`.a &` at root → `.a `), matching the element-level join (§2.2).
            let mut acc: Vec<String> = vec![os.clone()];
            while acc.first().map(|s| s.contains('&')).unwrap_or(false) {
                let mut next = Vec::with_capacity(acc.len() * parents.len());
                for s in &acc {
                    for pp in &parents {
                        next.push(s.replacen('&', pp, 1));
                    }
                }
                acc = next;
            }
            out.extend(acc.into_iter().map(|s| s.trim_start().to_string()));
        } else {
            for pp in &parents {
                if pp.is_empty() {
                    out.push(os.clone());
                } else if os.starts_with(char::is_whitespace) {
                    // The child leads with an explicit combinator (` > foo`) — it
                    // already separates from the parent; no descendant space.
                    out.push(format!("{pp}{os}"));
                } else {
                    out.push(format!("{pp} {os}"));
                }
            }
        }
    }
    out
}

/// Combinator spacing (less.js `Combinator.genCSS`): `>`/`+`/`~`/`^`/`^^` get a
/// space on each side in expanded output; ``/` `/`|` do not (§4.7).
fn combinator_css(c: &str) -> String {
    if matches!(c, "" | " " | "|") {
        c.to_string()
    } else {
        format!(" {c} ")
    }
}

// ----------------------------------------------------------------------
// Small string utilities (top-level split honoring nesting)
// ----------------------------------------------------------------------

fn split_top(s: &str, sep: char) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'"' | b'\'' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            c if c as char == sep && depth == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(s[start..].to_string());
    out
}

/// Split on a whole-word separator (`and`) at top nesting level.
fn split_word(s: &str, word: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ if depth == 0 && s[i..].starts_with(word) => {
                let before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                let after_idx = i + word.len();
                let after =
                    after_idx >= bytes.len() || !bytes[after_idx].is_ascii_alphanumeric();
                if before && after {
                    out.push(s[start..i].to_string());
                    i = after_idx;
                    start = i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out.push(s[start..].to_string());
    out
}

/// Find the next interpolation opener — `@{` (variable) or `${` (property) —
/// returning its byte index and whether it is the property form.
fn find_interp(s: &str) -> Option<(usize, bool)> {
    match (s.find("@{"), s.find("${")) {
        (Some(v), Some(p)) if p < v => Some((p, true)),
        (Some(v), _) => Some((v, false)),
        (None, Some(p)) => Some((p, true)),
        (None, None) => None,
    }
}

/// The byte length of the UTF-8 char whose lead byte is `b`.
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

// ----------------------------------------------------------------------
// Output rendering (flat expanded serializer)
// ----------------------------------------------------------------------

fn has_visible(decls: &[Node]) -> bool {
    decls.iter().any(|d| d.is_output_visible())
}

fn render_all(outs: &[Out], np: u8) -> String {
    let mut parts: Vec<String> = Vec::new();
    for o in outs {
        if let Some(s) = render_out(o, 0, np) {
            parts.push(s);
        }
    }
    let mut s = parts.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

fn render_out(out: &Out, indent: usize, np: u8) -> Option<String> {
    let ind = "  ".repeat(indent);
    match out {
        Out::Comment(t) => Some(format!("{ind}{t}")),
        // Nested at-rules are routed into their rule's declaration block by
        // `eval_rules`; render standalone (root position) as a plain block.
        Out::Nested(node) => Some(render_nested_at(node, &ind, np)),
        Out::Decls(decls) => {
            if !has_visible(decls) {
                return None;
            }
            Some(render_decls(decls, &ind, np))
        }
        Out::Rule { selectors, decls, .. } => {
            let vis: Vec<&str> = selectors
                .iter()
                .filter(|s| s.visible)
                .map(|s| s.css.as_str())
                .collect();
            if vis.is_empty() || !has_visible(decls) {
                return None;
            }
            let header = vis.join(&format!(",\n{ind}"));
            let dind = "  ".repeat(indent + 1);
            let body = render_decls(decls, &dind, np);
            Some(format!("{ind}{header} {{\n{body}\n{ind}}}"))
        }
        Out::At { header, body } => match body {
            AtBody::None => Some(format!("{ind}{header};")),
            AtBody::Rules(inner) => {
                let mut parts = Vec::new();
                for o in inner {
                    if let Some(s) = render_out(o, indent + 1, np) {
                        parts.push(s);
                    }
                }
                if parts.is_empty() {
                    return Some(format!("{ind}{header} {{\n{ind}}}"));
                }
                Some(format!("{ind}{header} {{\n{}\n{ind}}}", parts.join("\n")))
            }
            // less.js renders an inline-import payload inside its media shell
            // as `<indent+2><raw content>\n}` — the content verbatim (its own
            // trailing newline included), first line indented only.
            AtBody::Verbatim(content) => {
                let dind = "  ".repeat(indent + 1);
                Some(format!("{ind}{header} {{\n{dind}{content}\n{ind}}}"))
            }
        },
        Out::Verbatim(content) => Some(format!("{ind}{content}")),
        Out::Hidden(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::options::LessOptions;
    use crate::resolver::NoopResolver;

    fn css(src: &str) -> String {
        let opts = LessOptions::default();
        crate::compile(src, &opts, &NoopResolver)
            .unwrap()
            .code
            .trim_end()
            .to_string()
    }

    fn errs(src: &str) -> String {
        let opts = LessOptions::default();
        crate::compile(src, &opts, &NoopResolver)
            .unwrap_err()
            .to_string()
    }

    /// An in-memory `path -> content` resolver for import/extend tests.
    struct MapResolver(Vec<(&'static str, &'static str)>);

    impl crate::resolver::ImportResolver for MapResolver {
        fn resolve(
            &self,
            req: &crate::resolver::ImportRequest,
        ) -> Result<crate::resolver::ResolvedImport, crate::resolver::ImportError> {
            let raw = req.path.as_str();
            let key = if raw.ends_with(".less") || raw.ends_with(".css") {
                raw.to_string()
            } else {
                format!("{raw}.less")
            };
            let Some((_, content)) = self.0.iter().find(|(p, _)| *p == key) else {
                return Err(crate::resolver::ImportError::NotFound(key));
            };
            let file = crate::resolver::FileInfo {
                filename: key.clone(),
                current_directory: String::new(),
                ..Default::default()
            };
            let payload = if req.options.inline {
                crate::resolver::ImportPayload::Inline(std::sync::Arc::from(*content))
            } else if key.ends_with(".css") {
                crate::resolver::ImportPayload::Css(std::sync::Arc::from(*content))
            } else {
                crate::resolver::ImportPayload::Less(std::sync::Arc::from(*content))
            };
            Ok(crate::resolver::ResolvedImport { file, payload })
        }
    }

    fn css_with(files: &[(&'static str, &'static str)], src: &str, opts: &LessOptions) -> String {
        let resolver = MapResolver(files.to_vec());
        crate::compile(src, opts, &resolver)
            .unwrap()
            .code
            .trim_end()
            .to_string()
    }

    #[test]
    fn phase4b_import_once_vs_multiple() {
        let files: &[(&'static str, &'static str)] = &[("a.less", ".a { x: 1; }\n")];
        let opts = LessOptions::default();
        assert_eq!(
            css_with(files, "@import \"a\";\n@import \"a\";", &opts),
            ".a {\n  x: 1;\n}"
        );
        assert_eq!(
            css_with(files, "@import (multiple) \"a\";\n@import (multiple) \"a\";", &opts),
            ".a {\n  x: 1;\n}\n.a {\n  x: 1;\n}"
        );
    }

    #[test]
    fn phase4b_import_optional_missing_is_silent() {
        let opts = LessOptions::default();
        assert_eq!(
            css_with(&[], "@import (optional) \"nope\";\n.b { y: 2; }", &opts),
            ".b {\n  y: 2;\n}"
        );
    }

    #[test]
    fn phase4b_css_import_passthrough_rewrite() {
        // A `.css` import re-emits literally; with rewriting on, the rootpath
        // joins the path (less.js `Import.evalPath`).
        let mut opts = LessOptions::default();
        opts.rewrite_urls = crate::options::RewriteUrls::All;
        opts.rootpath = Some("http://example.com/css/".into());
        assert_eq!(
            css_with(&[], "@import \"theme.css\";", &opts),
            "@import \"http://example.com/css/theme.css\";"
        );
        // Absolute path: untouched.
        assert_eq!(
            css_with(&[], "@import \"/abs/theme.css\";", &opts),
            "@import \"/abs/theme.css\";"
        );
    }

    #[test]
    fn phase4b_reference_extend_visibility() {
        // (reference): hidden until an extend re-enables the matched rule —
        // and ONLY the grafted selector renders.
        let files: &[(&'static str, &'static str)] =
            &[("lib.less", ".abs { color: red; }\n.dark { color: black; }\n")];
        let opts = LessOptions::default();
        assert_eq!(
            css_with(
                files,
                "@import (reference) \"lib\";\n.use:extend(.abs all) {}",
                &opts
            ),
            ".use {\n  color: red;\n}"
        );
    }

    #[test]
    fn phase4b_extend_media_scoping() {
        // An extend inside @media affects only that block; an outer extend
        // reaches INTO media blocks.
        assert_eq!(
            css(".a { c: 1; }\n@media tv { .a { c: 2; } .in:extend(.a all) {} }\n.out:extend(.a all) {}"),
            ".a,\n.out {\n  c: 1;\n}\n@media tv {\n  .a,\n  .in,\n  .out {\n    c: 2;\n  }\n}"
        );
    }

    #[test]
    fn phase4b_rewrite_urls_matrix() {
        let mk = |mode: crate::options::RewriteUrls| {
            let mut o = LessOptions::default();
            o.rewrite_urls = mode;
            o.rootpath = Some("assets/".into());
            css_with(
                &[],
                ".u { a: url(\"./x.png\"); b: url(\"x.png\"); c: url(\"/x.png\"); d: url(\"http://h/x.png\"); }",
                &o,
            )
        };
        // local: only explicitly-relative (`./`) paths rewrite.
        assert_eq!(
            mk(crate::options::RewriteUrls::Local),
            ".u {\n  a: url(\"./assets/x.png\");\n  b: url(\"x.png\");\n  c: url(\"/x.png\");\n  d: url(\"http://h/x.png\");\n}"
        );
        // all AND off (upstream `pathRequiresRewrite` treats them alike —
        // off differs only in per-file rootpath accumulation): every
        // relative path rewrites.
        for mode in [crate::options::RewriteUrls::All, crate::options::RewriteUrls::Off] {
            assert_eq!(
                mk(mode),
                ".u {\n  a: url(\"./assets/x.png\");\n  b: url(\"assets/x.png\");\n  c: url(\"/x.png\");\n  d: url(\"http://h/x.png\");\n}"
            );
        }
    }

    #[test]
    fn lazy_last_wins_and_forward_reference() {
        // Forward reference (@var reads @a defined later) + last-declaration-wins.
        assert_eq!(css("@var: @a;\n@a: 1;\n@a: 2;\n.x { y: @var; }"), ".x {\n  y: 2;\n}");
    }

    #[test]
    fn phase3_review_number_formatting() {
        // C13/F1: decimal (toFixed) fround — never the *1e8 multiply round.
        assert_eq!(css("a { b: 179.999999995; }"), "a {\n  b: 179.99999999;\n}");
        assert_eq!(
            css("a { b: 28.397783365px; c: -12.532202605em; }"),
            "a {\n  b: 28.39778336px;\n  c: -12.53220261em;\n}"
        );
        // C21/F3: JS String() spellings — Infinity and the ≥1e21 exponent form.
        assert_eq!(
            css("a { b: (1 / 0); c: (-1 / 0); d: pow(10, 21); }"),
            "a {\n  b: Infinity;\n  c: -Infinity;\n  d: 1e+21;\n}"
        );
        // C7: tiny rgba alpha joins as String(number) → exponent notation.
        assert_eq!(
            css("a { c: rgba(0, 0, 0, 0.0000001); }"),
            "a {\n  c: rgba(0, 0, 0, 1e-7);\n}"
        );
    }

    #[test]
    fn phase3_review_error_parity() {
        // F2/C20: NaN Dimension construction is a hard error.
        assert!(errs("a { b: (0 / 0); }").contains("Dimension is not a number."));
        assert!(errs("a { b: sqrt(-1); }").contains("Error evaluating function `sqrt`"));
        // F4: wrong-arg-type math errors instead of passthrough.
        assert!(errs("a { b: round(10 / 3); }").contains("argument must be a number"));
        // C9: non-color args to color functions error…
        assert!(errs("a { c: desaturate(3.2); }")
            .contains("Argument cannot be evaluated to a color"));
        // …except the saturate/contrast filter carve-outs.
        assert_eq!(css("a { c: saturate(3.2); }"), "a {\n  c: saturate(3.2);\n}");
        assert_eq!(css("a { c: contrast(30%); }"), "a {\n  c: contrast(30%);\n}");
        // C11: NaN amounts poison channels → #NaNNaNNaN, like less.js.
        assert_eq!(
            css("a { c: lighten(#880000, banana); d: (#000000 / #000000); }"),
            "a {\n  c: #NaNNaNNaN;\n  d: #NaNNaNNaN;\n}"
        );
        // F16: a detached ruleset on a real property is an error.
        assert!(errs("a { d: if(true, {x: 1}, {y: 2}); }")
            .contains("Rulesets cannot be evaluated on a property."));
        // F11: root-level declarations error (also each() bodies at root).
        assert!(errs(".x { color: red; } color: blue;")
            .contains("Properties must be inside selector blocks"));
    }

    #[test]
    fn phase3_review_constructors_and_strings() {
        // C1: missing alpha re-emits; C2: rgb(color); C12: 4th space item drops.
        assert_eq!(
            css("a { c: rgba(1, 2, 3); d: hsla(90, 50%, 50%); }"),
            "a {\n  c: rgba(1, 2, 3);\n  d: hsla(90, 50%, 50%);\n}"
        );
        assert_eq!(
            css("a { c: rgb(#123456); d: rgb(0 128 255 96); }"),
            "a {\n  c: #123456;\n  d: #0080ff;\n}"
        );
        // C4: color() quoted-keyword fallback.
        assert_eq!(
            css("a { c: color('red'); d: color('transparent'); }"),
            "a {\n  c: #ff0000;\n  d: rgba(0, 0, 0, 0);\n}"
        );
        // C6: quoted 'relative' method.
        assert_eq!(
            css("a { c: lighten(#880000, 20%, 'relative'); }"),
            "a {\n  c: #a30000;\n}"
        );
        // C16/C17: e()/escape() read the Color's internal value marker.
        assert_eq!(
            css("a { c: e(hsl(90, 50%, 50%)); d: escape(#fff); e: escape(mix(#f00, #00f)); }"),
            "a {\n  c: hsl;\n  d: %23fff;\n  e: undefined;\n}"
        );
        // F8: no fround inside unit()'s unit-arg / %() args / replace() repl.
        assert_eq!(
            css("a { u: unit(5, 1.234567891px); w: %(\"%a\", 9.876543219px); }"),
            "a {\n  u: 51.234567891px;\n  w: \"9.876543219px\";\n}"
        );
    }

    #[test]
    fn phase3_review_ie_alpha_and_url() {
        // C14: case-normalized ieAlpha; F9-math: @var form interpolates.
        assert_eq!(
            css("a { filter: alpha(Opacity=87); }"),
            "a {\n  filter: alpha(opacity=87);\n}"
        );
        assert_eq!(
            css("@o: 30;\na { filter: alpha(opacity=@o); }"),
            "a {\n  filter: alpha(opacity=30);\n}"
        );
        // C15: rejected forms are parse/eval errors, never corrupted output.
        assert!(errs("a { filter: alpha(opacity=87.5); }").contains("expected ')'"));
        assert!(errs("a { filter: alpha(opacity=); }").contains("Could not parse alpha"));
        assert!(errs("@o: 30;\na { filter: alpha(opacity=@{o}); }")
            .contains("Could not parse alpha"));
        // F6-url: a bare variable inside url() resolves (quotes kept).
        assert_eq!(
            css("@a: 'Trebuchet';\nb { url: url(@a); }"),
            "b {\n  url: url('Trebuchet');\n}"
        );
        // F7-url: trailing whitespace inside an unquoted url is KEPT.
        assert_eq!(
            css("a { b: url(  spaced.png  ); }"),
            "a {\n  b: url(spaced.png  );\n}"
        );
        // F8-url: url(fn(x)) is a parse error like less.js.
        assert!(errs("a { b: url(unknownfn(x)); }").contains("expected ')'"));
    }

    #[test]
    fn phase3_review_dedup_interp_each() {
        // C22/F1-residual: duplicate declarations — earlier identical drops.
        assert_eq!(
            css("html { color: red; b: x; color: red; }"),
            "html {\n  b: x;\n  color: red;\n}"
        );
        // `!important` differs in CSS text → both stay.
        assert_eq!(
            css("html { color: red !important; b: x; color: red; }"),
            "html {\n  color: red !important;\n  b: x;\n  color: red;\n}"
        );
        // F18: quoted-variable interpolation keeps quotes in selectors + names.
        assert_eq!(
            css("@v: \"sel\";\n.@{v} { a: b; }"),
            ".\"sel\" {\n  a: b;\n}"
        );
        assert_eq!(
            css("@prop: \"color\";\nb { @{prop}: red; }"),
            "b {\n  \"color\": red;\n}"
        );
        // …while escaped values still render raw.
        assert_eq!(css("@v: ~\"esc\";\n.@{v} { a: b; }"), ".esc {\n  a: b;\n}");
        // F3-residual: each() over a mixin call iterates its declarations.
        assert_eq!(
            css(".set-2() {\n  one: blue;\n  two: green;\n}\n.s {\n  each(.set-2(), .(@v, @k, @i) {\n    @{k}-@{i}: @v;\n  });\n}"),
            ".s {\n  one-1: blue;\n  two-2: green;\n}"
        );
        // F5/F6-residual: permissive backslash-escaped quote + kept comment.
        assert_eq!(
            css(".r { --value: a/* { ; } */; }"),
            ".r {\n  --value: a /* { ; } */;\n}"
        );
        assert_eq!(
            css(".c {\n  --v: ( x; // i\\'m serious;\n  );\n}"),
            ".c {\n  --v: ( x; // i\\'m serious;\n  );\n}"
        );
    }

    #[test]
    fn unknown_function_passthrough_evaluates_args() {
        // §2.7: unknown calls re-emit with EVALUATED args — math is switched on
        // inside function arguments (less.js `Call.eval`), but division still
        // needs parens in the default mode.
        assert_eq!(css(".x { a: foo(1 + 2); }"), ".x {\n  a: foo(3);\n}");
        assert_eq!(css(".x { a: foo(10/2); }"), ".x {\n  a: foo(10/2);\n}");
        assert_eq!(css(".x { a: foo((10/2)); }"), ".x {\n  a: foo(5);\n}");
        assert_eq!(
            css("@w: 5px;\n.x { a: translateX(@w + 5px); }"),
            ".x {\n  a: translateX(10px);\n}"
        );
    }

    #[test]
    fn if_is_lazy_and_boolean_uses_guard_grammar() {
        // The false branch would error (unknown var) if evaluated eagerly.
        assert_eq!(css(".x { a: if(1 = 1, ok, @nope); }"), ".x {\n  a: ok;\n}");
        // Missing false branch → empty Anonymous.
        assert_eq!(css(".x { --a: if(not(true), 5); }"), ".x {\n  --a: ;\n}");
        assert_eq!(
            css(".x { a: boolean(not(2 > 1) and (true)); b: boolean(not false); }"),
            ".x {\n  a: false;\n  b: true;\n}"
        );
    }

    #[test]
    fn calc_folds_variables_but_not_literal_math() {
        // §2.4: calc suppresses math, but a variable's own value folds (_SELF).
        assert_eq!(
            css("@c: 10px + 20px;\n.x { a: calc(100% - @c); }"),
            ".x {\n  a: calc(100% - 30px);\n}"
        );
        assert_eq!(
            css(".x { a: calc(100% - 30px); }"),
            ".x {\n  a: calc(100% - 30px);\n}"
        );
    }

    #[test]
    fn ie_filter_assignment_args_survive() {
        // §2.17: progid call names + `key=value` assignment args.
        assert_eq!(
            css("@o: 0;\n.x { filter: progid:DXImageTransform.Microsoft.Alpha(opacity=@o); }"),
            ".x {\n  filter: progid:DXImageTransform.Microsoft.Alpha(opacity=0);\n}"
        );
    }

    #[test]
    fn each_binds_value_key_index() {
        assert_eq!(
            css(".x { each(a b, { i-@{index}: @value; }) }"),
            ".x {\n  i-1: a;\n  i-2: b;\n}"
        );
        assert_eq!(
            css("@set: { one: blue; two: green; };\n.x { each(@set, { @{key}: @value; }) }"),
            ".x {\n  one: blue;\n  two: green;\n}"
        );
    }

    #[test]
    fn child_scope_overrides_parent() {
        let out = css("@v: outer;\n.a { @v: inner; v: @v; }\n.b { v: @v; }");
        assert_eq!(out, ".a {\n  v: inner;\n}\n.b {\n  v: outer;\n}");
    }

    #[test]
    fn dimension_operations_and_units() {
        assert_eq!(css(".x { y: (1 + 2 * 3); }"), ".x {\n  y: 7;\n}");
        assert_eq!(css(".x { y: (13 + 1cm); }"), ".x {\n  y: 14cm;\n}");
        // px / px keeps the backup unit (plan §2.18 worked example).
        assert_eq!(css(".x { y: (1px / 1px); }"), ".x {\n  y: 1px;\n}");
    }

    #[test]
    fn parens_division_gate() {
        // Bare `/` does not divide under parens-division; in parens it does.
        assert_eq!(css(".x { y: 10px/2; }"), ".x {\n  y: 10px/2;\n}");
        assert_eq!(css(".x { y: (10px / 2); }"), ".x {\n  y: 5px;\n}");
    }

    #[test]
    fn color_math_per_channel() {
        assert_eq!(css(".x { c: (#110000 + #001100); }"), ".x {\n  c: #111100;\n}");
        assert_eq!(css(".x { c: (rgb(200, 200, 200) / 2); }"), ".x {\n  c: #646464;\n}");
    }

    #[test]
    fn nesting_and_parent_selector() {
        let out = css(".a {\n  color: red;\n  .b { color: blue; }\n  &:hover { color: green; }\n}");
        assert_eq!(
            out,
            ".a {\n  color: red;\n}\n.a .b {\n  color: blue;\n}\n.a:hover {\n  color: green;\n}"
        );
    }

    #[test]
    fn interpolation_in_selector_and_value() {
        let out = css("@n: 5_x;\n.icon-@{n} { content: \"@{n}\"; }");
        assert_eq!(out, ".icon-5_x {\n  content: \"5_x\";\n}");
    }

    #[test]
    fn variable_variable() {
        assert_eq!(
            css("@var: hello;\n@name: var;\n.x { y: @@name; }"),
            ".x {\n  y: hello;\n}"
        );
    }

    #[test]
    fn parametric_mixin_with_default_and_named_args() {
        // Space-before-parens definition, default value, named argument (§2.5).
        let out = css(".m (@a: 1px, @b: 2px) { p: @a @b; }\n.x { .m(@b: 9px); }");
        assert_eq!(out, ".x {\n  p: 1px 9px;\n}");
    }

    #[test]
    fn pattern_matching_emits_all_matches() {
        // Overloaded definitions; a literal-pattern param + a catch-all (§2.5).
        let out = css(
            ".m(@x) { one: @x; }\n.m(left) { side: left; }\n.y { .m(left); }",
        );
        assert_eq!(out, ".y {\n  one: left;\n  side: left;\n}");
    }

    #[test]
    fn mixin_guard_selects_definition() {
        let out = css(
            ".m(@x) when (@x > 5) { big: @x; }\n.m(@x) when (@x <= 5) { small: @x; }\n\
             .a { .m(9); }\n.b { .m(2); }",
        );
        assert_eq!(out, ".a {\n  big: 9;\n}\n.b {\n  small: 2;\n}");
    }

    #[test]
    fn mixin_closure_captures_definition_scope() {
        // The mixin resolves `@var` in its definition scope, not the caller's.
        let out = css(
            ".scope { @var: 99px; .m() { w: @var; } }\n.x { @var: 0px; .scope > .m(); }",
        );
        assert_eq!(out, ".x {\n  w: 99px;\n}");
    }

    #[test]
    fn mixin_scope_injection_returns_variables() {
        // A called mixin injects its top-level variable into the caller (§2.5).
        let out = css(".m() { @c: red; }\n.x { color: @c; .m(); }");
        assert_eq!(out, ".x {\n  color: red;\n}");
    }

    #[test]
    fn mixin_closure_captures_bound_params() {
        // An inner mixin injected by a parametric outer mixin must freeze the
        // outer's bound param (closure over params, §4.3).
        let out = css(".m(@x) { .inner() { val: @x; } }\n.a { .m(red); .inner(); }");
        assert_eq!(out, ".a {\n  val: red;\n}");
        // Two injections both emit, each with its own frozen binding (last-wins is
        // NOT less.js's behavior — both closures are in scope).
        let out2 = css(".m(@x) { .inner() { val: @x; } }\n.a { .m(red); .m(green); .inner(); }");
        assert_eq!(out2, ".a {\n  val: red;\n  val: green;\n}");
    }

    #[test]
    fn mixin_arguments_includes_variadic_tail() {
        // `@arguments` is the full flattened list, incl. variadic-captured args.
        let out = css(".m(@a, @rest...) { a: @a; r: @rest; args: @arguments; }\n.z { .m(1, 2, 3); }");
        assert_eq!(out, ".z {\n  a: 1;\n  r: 2 3;\n  args: 1 2 3;\n}");
    }

    #[test]
    fn guard_nested_not_and_parens() {
        // Deeply nested `not(…)` + parens evaluate as a recursive boolean grammar.
        let out = css(
            ".t(@v) when ((((@v)))) { a: 1; }\n\
             .t(@v) when not(((not(@v)))) { b: 2; }\n\
             .x { .t(true); }",
        );
        assert_eq!(out, ".x {\n  a: 1;\n  b: 2;\n}");
    }

    #[test]
    fn guarded_namespace_path() {
        // A false guard on a traversed namespace excludes the inner mixin, and a
        // parametric namespace is only entered with zero args (§2.6).
        let out = css(
            "@g: 1;\n\
             #ns when (@g > 0) { .m() { ok: yes; } }\n\
             #ns when (@g < 0) { .m() { no: guard; } }\n\
             #ns(@x) { .m() { no: arity; } }\n\
             .x { #ns > .m(); }",
        );
        assert_eq!(out, ".x {\n  ok: yes;\n}");
    }

    #[test]
    fn mixin_important_forces_important() {
        let out = css(".m() { a: 1; }\n.x { .m() !important; }");
        assert_eq!(out, ".x {\n  a: 1 !important;\n}");
    }

    #[test]
    fn css_guard_and_bare_ampersand_fold() {
        let out = css("@c: 3;\n.x { w: 1; & when (@c = 3) { h: 2; } }");
        assert_eq!(out, ".x {\n  w: 1;\n  h: 2;\n}");
    }

    // ------------------------------------------------------------------
    // Phase 4A: detached rulesets (§2.11)
    // ------------------------------------------------------------------

    #[test]
    fn dr_call_uses_definition_scope_precedence() {
        // The DR value captures the frames at its DECLARATION site; those
        // frames take precedence over the caller's on call (verified 4.6.7).
        assert_eq!(
            css("@a: outer;\n@dr: { v: @a; };\n.x { @a: inner; @dr(); }"),
            ".x {\n  v: outer;\n}"
        );
        // …but names absent from the captured scope fall back to the caller.
        assert_eq!(
            css("@dr: { v: @b; };\n.x { @b: caller; @dr(); }"),
            ".x {\n  v: caller;\n}"
        );
    }

    #[test]
    fn dr_call_drops_variables_but_unlocks_mixins() {
        // A variable-call splices NO variables into the caller (§2.11)…
        assert!(errs("@dr: { @v: leaked; a: 1; };\n.x { @dr(); b: @v; }")
            .contains("variable @v is undefined"));
        // …but mixin definitions inside the DR become callable.
        assert_eq!(
            css("@my: { .mk() { got: yes; } };\n@my();\n.x { .mk(); }"),
            ".x {\n  got: yes;\n}"
        );
        // Call before assignment / unknown DR → the variable-undefined error.
        assert!(errs(".y { @nope(); }").contains("variable @nope is undefined"));
    }

    #[test]
    fn dr_as_mixin_argument_and_default() {
        // A DR literal argument evaluates in the CALLER's scope, not the
        // mixin's (the detached-rulesets fixture core).
        assert_eq!(
            css("@a: 1px;\n.wrap(@r) { @a: bad; .s { @r(); } }\n.x { .wrap({ one: @a; }); }"),
            ".x .s {\n  one: 1px;\n}"
        );
        // DR parameter defaults (`@b: {d: w;}`) work, incl. semicolon form.
        assert_eq!(
            css(".def(@a: {}; @b: {d: w;};) { @a(); @b(); }\n.u { .def({x: y;}); }"),
            ".u {\n  x: y;\n  d: w;\n}"
        );
    }

    #[test]
    fn dr_media_bubbles_at_call_site() {
        // @media inside a DR bubbles when called, wrapping the caller (§2.11).
        assert_eq!(
            css("@dr: { @media (tv) { b: c; } };\n.host { @dr(); }"),
            "@media (tv) {\n  .host {\n    b: c;\n  }\n}"
        );
    }

    // ------------------------------------------------------------------
    // Phase 4A: maps / lookups (§2.12)
    // ------------------------------------------------------------------

    #[test]
    fn map_lookups_all_forms() {
        // Ruleset-as-map, unnamed [], mixin-as-map, namespace variable +
        // property + parameterized lookup.
        let out = css(
            "@sizes: { mobile: 320px; tablet: 768px; };\n\
             .m() { sm: 10px; @last: varlast; }\n\
             #ns { @c: nsvar; prim: nsprop; .mx(@v) { r: (@v + 1); } }\n\
             .x { a: @sizes[tablet]; b: @sizes[]; c: .m()[]; d: #ns[@c]; e: #ns.mx(4)[r]; f: #ns[prim]; }",
        );
        assert_eq!(
            out,
            "#ns {\n  prim: nsprop;\n}\n\
             .x {\n  a: 768px;\n  b: 768px;\n  c: varlast;\n  d: nsvar;\n  e: 5;\n  f: nsprop;\n}"
        );
    }

    #[test]
    fn map_dynamic_keys_and_nested() {
        // `@@lookup` dynamic variable keys chain through nested DR maps.
        assert_eq!(
            css("@config: { @options: { primary: blue; } };\n@lookup: options;\n\
                 .x { color: @config[@@lookup][primary]; }"),
            ".x {\n  color: blue;\n}"
        );
        // `$@var` dynamic property key.
        assert_eq!(
            css("@pn: prim;\n#ns { prim: val; }\n.x { v: #ns[$@pn]; }"),
            "#ns {\n  prim: val;\n}\n.x {\n  v: val;\n}"
        );
        // Unresolved keys use less.js's messages.
        assert!(errs("@m: { a: 1; };\n.x { v: @m[missing]; }")
            .contains("property \"missing\" not found"));
        assert!(errs("@m: { a: 1; };\n.x { v: @m[@nope]; }")
            .contains("variable @nope not found"));
    }

    #[test]
    fn property_accessor_reads_last_and_propagates_important() {
        // Forward reference: the LAST declaration wins, even declared later.
        assert_eq!(
            css(".b { color: red; .c { x: $color; } color: blue; }"),
            ".b {\n  color: red;\n  color: blue;\n}\n.b .c {\n  x: blue;\n}"
        );
        // The parseValue quirk: a read `!important` declaration re-renders
        // without the space, and the reader gains ` !important` (§2.12).
        assert_eq!(
            css(".t { color: red !important; background: $color; }"),
            ".t {\n  color: red!important;\n  background: red !important;\n}"
        );
        // `${prop}` interpolation reads properties.
        assert_eq!(
            css(".p { prop: a; content: \"${prop}\"; }"),
            ".p {\n  prop: a;\n  content: \"a\";\n}"
        );
    }

    // ------------------------------------------------------------------
    // Phase 4A: at-rule bubbling & ordering (§2.13)
    // ------------------------------------------------------------------

    #[test]
    fn media_nested_merges_with_and() {
        assert_eq!(
            css("@media screen { @media (max-width: 768px) { .n { d: b; } } }"),
            "@media screen and (max-width: 768px) {\n  .n {\n    d: b;\n  }\n}"
        );
        // Comma lists cross-multiply, first list varying fastest.
        assert_eq!(
            css("@media (m1), (m2) { .t { @media (m3), (m4) { v: 6; } } }"),
            "@media (m1) and (m3), (m2) and (m3), (m1) and (m4), (m2) and (m4) {\n  .t {\n    v: 6;\n  }\n}"
        );
        // Feature normalization: colon spacing + variables resolve.
        assert_eq!(
            css("@w: 42;\n@media all and (orientation:portrait) and (min-width: @w) { a { b: c; } }"),
            "@media all and (orientation: portrait) and (min-width: 42) {\n  a {\n    b: c;\n  }\n}"
        );
    }

    #[test]
    fn media_in_mixin_keeps_source_order() {
        // A mixin/DR body evaluates with a fresh media context and re-merges at
        // the call position — sibling media stay in source order (§2.13).
        assert_eq!(
            css("@dr: { @media (tv) { .x { a: b; } } };\n\
                 @media (w) {\n  @media (print) { .p { a: b; } }\n  @dr();\n}"),
            "@media (w) and (print) {\n  .p {\n    a: b;\n  }\n}\n\
             @media (w) and (tv) {\n  .x {\n    a: b;\n  }\n}"
        );
    }

    #[test]
    fn supports_bubbles_wrapped_and_stays_inside_media() {
        // @supports wraps bare decls in the parent selector and bubbles only to
        // the nearest at-rule boundary (§2.13).
        assert_eq!(
            css(".top { @supports (d: g) { .in & { p: v; } } }"),
            "@supports (d: g) {\n  .in .top {\n    p: v;\n  }\n}"
        );
        assert_eq!(
            css("@media print { html { i: v; @supports (u: t) { s: first; } } }"),
            "@media print {\n  html {\n    i: v;\n  }\n  @supports (u: t) {\n    html {\n      s: first;\n    }\n  }\n}"
        );
    }

    #[test]
    fn starting_style_stays_nested_and_unknown_bubbles_bare() {
        // @starting-style with a declaration body renders INSIDE the rule.
        assert_eq!(
            css(".u { o: 1; @starting-style { o: 0; } }"),
            ".u {\n  o: 1;\n  @starting-style {\n    o: 0;\n  }\n}"
        );
        // An unknown at-rule bubbles out WITHOUT the selector wrap (isRooted).
        assert_eq!(
            css(".p { @unknown-at (x) { u: 7; } }"),
            "@unknown-at (x) {\n  u: 7;\n}"
        );
    }

    #[test]
    fn container_merges_and_keeps_empty_shell() {
        assert_eq!(
            css("@container card (inline-size > 30em) { @container style(--r: true) { .c { g: 1; } } }"),
            "@container card (inline-size > 30em) {\n}\n\
             @container card (inline-size > 30em) and style(--r: true) {\n  .c {\n    g: 1;\n  }\n}"
        );
    }

    #[test]
    fn charset_hoists_and_dedups() {
        assert_eq!(
            css(".a { x: y; }\n@charset \"UTF-8\";\n@charset \"ISO-8859-1\";"),
            "@charset \"UTF-8\";\n.a {\n  x: y;\n}"
        );
    }

    // ------------------------------------------------------------------
    // Eval-side runaway guards (plan §2.5) — each input would recurse
    // unboundedly without its guard (the OOM-incident class). The depth cap
    // must error cleanly, never exhaust memory; frame growth per level must
    // stay LINEAR (`dedup_frames` — a recursive call's def-scope suffix +
    // caller stack would otherwise double the frame list per level and OOM
    // long before the cap).
    // ------------------------------------------------------------------

    #[test]
    fn guard_runaway_parametric_mixin_errors_cleanly() {
        // less.js dies on the JS call stack here (Syntax, message=undefined);
        // we cap eval depth instead of recursing forever.
        let e = errs(".loop(@n){.loop(@n);}\na { .loop(1); }");
        assert!(e.contains("recursion limit"), "got: {e}");
    }

    #[test]
    fn guard_runaway_detached_ruleset_errors_cleanly() {
        // less.js: "Maximum call stack size exceeded".
        let e = errs("@dr: { @dr(); }\n.a { @dr(); }");
        assert!(e.contains("recursion limit"), "got: {e}");
    }

    #[test]
    fn guard_mutually_recursive_mixins_error_cleanly() {
        let e = errs(".a(){.b();}\n.b(){.a();}\nx { .a(); }");
        assert!(e.contains("recursion limit"), "got: {e}");
    }

    #[test]
    fn guard_ruleset_as_mixin_on_stack_is_skipped_like_less_js() {
        // A plain ruleset calling itself: less.js SKIPS the on-stack candidate
        // (isRecursive continue), so the inner call finds no definition.
        let e = errs(".loop { .loop; }\na { .loop; }");
        assert!(e.contains("No matching definition"), "got: {e}");
    }

    #[test]
    fn guard_deep_but_terminating_recursion_is_untouched() {
        // Legitimate deep recursion far above fixture depths must NOT trip the
        // cap (less.js itself survives ~1000 levels).
        let out = css(
            ".loop(@n) when (@n > 0) {.loop(@n - 1);}\n.loop(0){x:done;}\na { .loop(50); }",
        );
        assert_eq!(out, "a {\n  x: done;\n}");
    }

    #[test]
    fn guard_max_eval_depth_is_configurable() {
        let opts = LessOptions {
            max_eval_depth: Some(8),
            ..LessOptions::default()
        };
        let e = crate::compile(
            ".loop(@n) when (@n > 0) {.loop(@n - 1);}\n.loop(0){x:done;}\na { .loop(50); }",
            &opts,
            &NoopResolver,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("recursion limit"), "got: {e}");
    }
}

/// less.js `functionCaller.call`'s argument normalization: drop `Comment` args,
/// drop comments inside `Expression` args, and re-collapse a now-single-item
/// Expression to its item (plan §2.7).
fn filter_call_args(args: &[Node]) -> Vec<Node> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        match a {
            Node::Comment { .. } => {}
            Node::Expression(items) => {
                let kept: Vec<Node> = items
                    .iter()
                    .filter(|i| !matches!(i, Node::Comment { .. }))
                    .cloned()
                    .collect();
                out.push(match kept.len() {
                    1 => kept.into_iter().next().unwrap(),
                    _ => Node::Expression(kept),
                });
            }
            other => out.push(other.clone()),
        }
    }
    out
}

/// less.js `ToCSSVisitor._mergeRules` (plan §2.10): declarations carrying a
/// merge flag group by property name — `+` starts a new comma group, `+_`
/// space-appends to the current one; later contributors collapse into the
/// first's position. Unflagged declarations (even same-named) never join.
fn merge_rules(decls: &[Node]) -> Vec<Node> {
    if !decls
        .iter()
        .any(|d| matches!(d, Node::Declaration(dd) if dd.merge.is_some()))
    {
        return decls.to_vec();
    }
    let mut out: Vec<Node> = Vec::new();
    // name → (index in `out`, comma groups of space parts, important).
    let mut groups: Vec<(String, usize, Vec<Vec<Node>>, String)> = Vec::new();
    for d in decls {
        match d {
            Node::Declaration(dd) if dd.merge.is_some() => {
                let existing = groups.iter_mut().find(|(n, ..)| *n == dd.name);
                match existing {
                    Some((_, _, comma, important)) => {
                        if dd.merge == Some(crate::ast::MergeKind::Comma) {
                            comma.push(Vec::new());
                        }
                        comma.last_mut().unwrap().push((*dd.value).clone());
                        if important.is_empty() {
                            *important = dd.important.clone();
                        }
                    }
                    None => {
                        groups.push((
                            dd.name.clone(),
                            out.len(),
                            vec![vec![(*dd.value).clone()]],
                            dd.important.clone(),
                        ));
                        out.push(d.clone());
                    }
                }
            }
            other => out.push(other.clone()),
        }
    }
    for (_, idx, comma, important) in groups {
        let value = Node::Value(
            comma
                .into_iter()
                .map(|space| {
                    if space.len() == 1 {
                        space.into_iter().next().unwrap()
                    } else {
                        Node::Expression(space)
                    }
                })
                .collect(),
        );
        if let Node::Declaration(dd) = &mut out[idx] {
            dd.value = Box::new(value);
            dd.important = important;
        }
    }
    out
}

/// less.js to-css-visitor `_removeDuplicateRules` (Phase 3 review C22/F1):
/// walking BACKWARDS through a ruleset's rules, an earlier declaration whose
/// name AND generated CSS both match a later one is dropped (the LAST wins its
/// position; `!important` variants differ in CSS so both stay). Non-declaration
/// rules (comments, at-rules) neither participate nor break the name cache.
fn remove_duplicate_decls(decls: Vec<Node>, np: u8) -> Vec<Node> {
    use std::collections::HashMap;
    let mut cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut keep = vec![true; decls.len()];
    for (i, d) in decls.iter().enumerate().rev() {
        let Node::Declaration(decl) = d else { continue };
        let css = format!(
            "{}: {}{};",
            decl.name,
            render_value(&decl.value, np),
            decl.important
        );
        let seen = cache.entry(decl.name.clone()).or_default();
        if seen.contains(&css) {
            keep[i] = false;
        } else {
            seen.push(css);
        }
    }
    let mut keep_iter = keep.into_iter();
    decls
        .into_iter()
        .filter(|_| keep_iter.next().unwrap_or(true))
        .collect()
}

fn render_decls(decls: &[Node], dind: &str, np: u8) -> String {
    let decls = merge_rules(decls);
    let decls = remove_duplicate_decls(decls, np);
    let mut lines = Vec::new();
    for d in &decls {
        match d {
            Node::Declaration(decl) => {
                let val = render_value(&decl.value, np);
                lines.push(format!("{dind}{}: {}{};", decl.name, val, decl.important));
            }
            Node::Comment { line: false, text, .. } => {
                lines.push(format!("{dind}{text}"));
            }
            // An `(inline)` import payload spliced into the block (§2.9):
            // verbatim, first line indented, own trailing newline kept.
            Node::Anonymous(text) => {
                lines.push(format!("{dind}{text}"));
            }
            Node::AtRule(a) => match &a.block {
                // An inline no-block directive (`@apply …;`) inside a ruleset.
                AtRuleBlock::None => {
                    let prelude = match &a.prelude {
                        Some(p) => format!(" {}", render_value(p, np)),
                        None => String::new(),
                    };
                    lines.push(format!("{dind}{}{prelude};", a.name));
                }
                // A simpleBlock at-rule nested inside the rule (§2.13).
                AtRuleBlock::Rules(_) => lines.push(render_nested_at(d, dind, np)),
            },
            _ => {}
        }
    }
    lines.join("\n")
}

/// Render a `simpleBlock` at-rule (`@starting-style { decls }`) at `ind`.
fn render_nested_at(node: &Node, ind: &str, np: u8) -> String {
    let Node::AtRule(a) = node else {
        return String::new();
    };
    let prelude = match &a.prelude {
        Some(p) => format!(" {}", render_value(p, np)),
        None => String::new(),
    };
    let AtRuleBlock::Rules(rules) = &a.block else {
        return format!("{ind}{}{prelude};", a.name);
    };
    let dind = format!("{ind}  ");
    let body = render_decls(rules, &dind, np);
    format!("{ind}{}{prelude} {{\n{body}\n{ind}}}", a.name)
}
