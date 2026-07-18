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

/// The evaluator context (plan §4.1/§4.2): innermost-first frame stack, math
/// state, the parens stack for `isMathOn`, and the `importantScope` stack.
pub struct Ctx<'a> {
    frames: Vec<Frame>,
    opts: &'a LessOptions,
    #[allow(dead_code)]
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

const MAX_MIXIN_DEPTH: usize = 128;

/// One flattened output node (post JoinSelector). genCSS emits these at top level.
enum Out {
    Rule {
        selectors: Vec<String>,
        decls: Vec<Node>,
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
}

enum AtBody {
    None,
    Rules(Vec<Out>),
}

/// Evaluate a parsed AST to [`Css`] (plan §9.5).
pub fn eval(
    root: &Arc<Node>,
    opts: &LessOptions,
    resolver: &dyn ImportResolver,
) -> Result<Css, LessError> {
    let rules = match root.as_ref() {
        Node::Root(r) => r.clone(),
        // A passthrough anonymous root (scaffold callers) — emit verbatim.
        Node::Anonymous(text) => return Ok(Css::from_code(text.clone())),
        other => vec![other.clone()],
    };

    let mut ctx = Ctx {
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
        warnings: Vec::new(),
    };

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

    // Output ordering (plan §2.13/§C): `@charset` then `@import` hoist to the
    // top of the stylesheet (stable within each). `@namespace` does NOT hoist —
    // less.js leaves it in source position (verified against 4.6.7).
    outs.sort_by_key(|o| match o {
        Out::At { header, .. } if header.starts_with("@charset") => 0,
        Out::At { header, .. } if header.starts_with("@import") => 1,
        _ => 2,
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
    fn push_frame(&mut self, f: Frame) {
        self.frames.insert(0, f);
    }
    fn pop_frame(&mut self) {
        self.frames.remove(0);
    }

    fn err(&self, kind: ErrorKind, msg: impl Into<String>) -> LessError {
        LessError::new(kind, msg)
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
        // Pass 1: expand mixin calls, inject scope, memoize output per position.
        let mut expansions: Vec<Option<(Vec<Node>, Vec<Out>)>> = Vec::with_capacity(rules.len());
        for rule in rules {
            if let Node::MixinCall(call) = rule {
                let mut ex_own = Vec::new();
                let mut ex_children = Vec::new();
                let injected =
                    self.expand_mixin_call(call, self_paths, &mut ex_own, &mut ex_children)?;
                if let Some(frame) = self.frames.first().cloned() {
                    for node in injected {
                        let keep = match &node {
                            Node::VariableDecl { name, .. } => !frame_has_var(&frame, name),
                            _ => true,
                        };
                        if keep {
                            frame.borrow_mut().push(node);
                        }
                    }
                }
                expansions.push(Some((ex_own, ex_children)));
            } else {
                expansions.push(None);
            }
        }

        // Pass 2: source-order output.
        for (idx, rule) in rules.iter().enumerate() {
            match rule {
                Node::MixinCall(_) => {
                    if let Some((ex_own, ex_children)) = expansions[idx].take() {
                        own.extend(ex_own);
                        children.extend(ex_children);
                    }
                }
                Node::VariableDecl { .. }
                | Node::MixinDefinition(_)
                | Node::Comment { line: true, .. }
                | Node::MagentoImport { .. } => {
                    // No direct output (declarations register in the frame; the
                    // magento directive is a later phase).
                }
                Node::Import {
                    path, features, ..
                } => {
                    // `@import` resolution/inlining is Phase 4. Re-emit CSS/`url()`
                    // imports literally (correct output); `.less` imports would be
                    // inlined, so drop them here rather than emit a bogus at-rule.
                    let is_css = matches!(path.as_ref(), Node::Url(_))
                        || matches!(path.as_ref(), Node::Quoted { value, .. } if value.ends_with(".css"))
                        || matches!(path.as_ref(), Node::Anonymous(s) if s.ends_with(".css"));
                    if is_css {
                        let ps = render_value(&self.eval_value(path)?, self.opts.num_precision);
                        let mut header = format!("@import {ps}");
                        if let Some(f) = features {
                            let fs = self.eval_prelude(f)?;
                            if !fs.is_empty() {
                                header.push(' ');
                                header.push_str(&fs);
                            }
                        }
                        children.push(Out::At {
                            header,
                            body: AtBody::None,
                        });
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
                    } else if let Some(out) = self.eval_at_rule(a, self_paths)? {
                        children.push(out);
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
        }
        Ok(())
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
        for sel in selectors {
            if let Some(g) = &sel.guard {
                if !self.eval_guard(g)? {
                    return Ok(()); // guard failed → ruleset emits nothing
                }
            }
            own_sel.push(self.render_selector(sel)?);
        }
        let joined = join_selectors(parent_paths, &own_sel);

        self.push_frame(frame_of(rules.to_vec()));
        let (decls, children) = self.process_body(rules, Some(&joined))?;
        self.pop_frame();

        if has_visible(&decls) {
            out.push(Out::Rule {
                selectors: joined,
                decls,
            });
        }
        out.extend(children);
        Ok(())
    }

    fn eval_at_rule(
        &mut self,
        a: &crate::ast::AtRule,
        parent_paths: Option<&[String]>,
    ) -> Result<Option<Out>, LessError> {
        let prelude = match &a.prelude {
            Some(p) => Some(self.eval_prelude(p)?),
            None => None,
        };
        let header = match &prelude {
            Some(p) if !p.is_empty() => format!("{} {}", a.name, p),
            _ => a.name.clone(),
        };
        match &a.block {
            AtRuleBlock::None => Ok(Some(Out::At {
                header,
                body: AtBody::None,
            })),
            AtRuleBlock::Rules(rules) => {
                // A container at-rule (@media/@supports/…) carries the parent
                // selector into its body; a declaration at-rule (@font-face/@page/
                // @keyframes) starts a fresh scope for its declarations.
                let is_container = matches!(
                    a.name.as_str(),
                    "@media" | "@supports" | "@document" | "@-moz-document" | "@layer"
                        | "@container"
                );
                let inner_parent = if is_container { parent_paths } else { None };
                self.push_frame(frame_of(rules.to_vec()));
                let (own, children) = self.process_body(rules, inner_parent)?;
                self.pop_frame();

                // Declarations first (like less.js Ruleset.genCSS), then nested.
                let mut body_outs: Vec<Out> = Vec::new();
                if has_visible(&own) {
                    body_outs.push(Out::Decls(own));
                }
                body_outs.extend(children);

                // An at-rule with an empty block emits nothing (plan §2.13).
                // A bubbling container (@media/…) is empty even when only
                // comments remain; a declaration at-rule (@keyframes) keeps a
                // comment-only body (verified against less.js 4.6.7).
                if body_outs.is_empty()
                    || (is_container
                        && body_outs.iter().all(|o| matches!(o, Out::Comment(_))))
                {
                    return Ok(None);
                }
                Ok(Some(Out::At {
                    header,
                    body: AtBody::Rules(body_outs),
                }))
            }
        }
    }

    // ------------------------------------------------------------------
    // Declarations
    // ------------------------------------------------------------------

    fn eval_declaration(&mut self, d: &Declaration) -> Result<Node, LessError> {
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
                Node::Anonymous(s) => self.interpolate(s)?,
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
        if matches!(value, Node::DetachedRuleset { .. }) {
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
                let v = self.eval_value(inner)?;
                Ok(Node::Url(Box::new(v)))
            }
            Node::Quoted {
                escaped,
                quote,
                value,
            } => {
                // Interpolation runs inside quoted strings (plan §2.14).
                let v = if value.contains("@{") || value.contains("$}") || value.contains("@") {
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
            Node::PropertyAccessor { name, .. } => {
                // $prop — property-as-variable. Minimal: unresolved → keep literal.
                Ok(Node::Anonymous(format!("${name}")))
            }
            // Self-evaluating leaves.
            Node::Dimension(_)
            | Node::Color(_)
            | Node::Keyword(_)
            | Node::Anonymous(_)
            | Node::DetachedRuleset { .. } => Ok(node.clone()),
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
            self.eval_binary('*', &minus_one, inner, true)
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
        let opc = op.chars().next().unwrap_or('+');
        let opc = if op == "./" { '/' } else { opc };
        self.eval_binary(opc, left, right, spaced)
    }

    /// The math core (less.js `Operation.eval`): eval operands, and if math is on
    /// for the op, coerce and operate; else return a deferred `Operation` (whose
    /// source spacing is preserved for literal emission).
    fn eval_binary(
        &mut self,
        op: char,
        left: &Node,
        right: &Node,
        spaced: bool,
    ) -> Result<Node, LessError> {
        let a = self.eval_value(left)?;
        let b = self.eval_value(right)?;
        if !self.is_math_on(op) {
            return Ok(Node::Operation {
                op: op.to_string(),
                left: Box::new(a),
                right: Box::new(b),
                spaced,
            });
        }
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
            self.push_frame(frame_of(body.clone()));
            let r = self.eval_rules(&body, self_paths, own, children);
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
        if self.mixin_depth > MAX_MIXIN_DEPTH {
            return Err(self.err(ErrorKind::Runtime, "mixin recursion limit exceeded"));
        }
        let path = mixin_names(&call.path);
        if path.is_empty() {
            return Ok(Vec::new());
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
            let mut found = find_candidates(&frames[k].borrow(), &path, &def_scope, &self.closures, &[]);
            // Drop ruleset candidates already on the eval stack (recursion guard;
            // MixinDefinitions are exempt — their `ruleset_span` is `None`).
            found.retain(|c| match c.ruleset_span {
                Some(span) => !self.active_rulesets.contains(&span),
                None => true,
            });
            if found.is_empty() {
                continue;
            }
            is_one_found = true;
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

        // Emit every candidate whose group is defNone or the chosen default.
        let mut injected: Vec<Node> = Vec::new();
        for (idx, cand) in chosen.iter().enumerate() {
            let g = groups[idx];
            if g == 0 || g == default_result {
                let inj = self.emit_candidate(cand, &args, call.important, self_paths, own, children)?;
                injected.extend(inj);
            }
        }
        Ok(injected)
    }

    /// Evaluate the call's arguments to `(name?, value)` pairs (§2.5). A trailing
    /// `...` on a positional argument (`.m(@list...)`) **spreads** the list value
    /// into individual arguments (less.js `arg.expand`).
    fn eval_call_args(&mut self, args: &[MixinArg]) -> Result<Vec<EvArg>, LessError> {
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            let name = a.name.as_ref().map(|n| n.trim_start_matches('@').to_string());
            // Detect the spread form on an unnamed argument.
            if name.is_none() {
                if let Node::Anonymous(s) = a.value.as_ref() {
                    if let Some(inner) = s.trim().strip_suffix("...") {
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
        let saved = std::mem::replace(&mut self.frames, new_frames);
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
                Node::Ruleset(_) => out.push(r.clone()),
                _ => {}
            }
        }
        out
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
        let saved = std::mem::replace(&mut self.frames, frames);
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
        // Bare value: true iff it evaluates to the keyword `true`.
        let v = self.reparse_and_eval(inner)?;
        Ok(matches!(&v, Node::Keyword(k) if k == "true"))
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
            if el.value.contains("@{") || el.value.contains("$}") {
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

    fn eval_prelude(&mut self, node: &Node) -> Result<String, LessError> {
        match node {
            Node::Anonymous(s) => {
                let s = if s.contains("@{") || s.contains("$}") {
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
    fn resolve_prelude_vars(&mut self, s: &str) -> Result<String, LessError> {
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
            while let Some(start) = find_interp(rest) {
                let after = &rest[start + 2..];
                // A match needs a `}` with only `[\w-]` name chars before it.
                let end_rel = after.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'));
                match end_rel {
                    Some(e) if e > 0 && after[e..].starts_with('}') => {
                        let name = &after[..e];
                        let val = self.eval_variable(name, Default::default())?;
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
                        // Not a simple name — emit `@{` literally and move on.
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
        if let Node::VariableDecl {
            name: n,
            value,
            important,
            ..
        } = r
        {
            if n == name {
                result = Some(((**value).clone(), !important.is_empty()));
            }
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
fn find_candidates(
    rules: &[Node],
    path: &[String],
    def_scope: &[Frame],
    closures: &[Vec<Frame>],
    path_guards: &[Node],
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for r in rules {
        match r {
            // A scope-injected closure: resolve against the frames frozen at
            // injection (the enclosing mixin's bound params), not the caller's.
            Node::Closure { inner, scope } => {
                if let Node::MixinDefinition(def) = inner.as_ref() {
                    let captured = &closures[*scope as usize];
                    let names = extract_names_dropamp(&def.name);
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
                            out.extend(find_candidates(&def.rules, &path[m..], &inner_scope, closures, &child));
                        }
                    }
                }
            }
            Node::MixinDefinition(def) => {
                let names = extract_names_dropamp(&def.name);
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
                        out.extend(find_candidates(&def.rules, &path[m..], &inner_scope, closures, &child));
                    }
                }
            }
            Node::Ruleset(rs) => {
                for sel in &rs.selectors {
                    let names = mixin_names(&sel.elements);
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
                            out.extend(find_candidates(&rs.rules, &path[m..], &inner_scope, closures, &child));
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
        Out::At { body, .. } => {
            if let AtBody::Rules(inner) = body {
                for o in inner.iter_mut() {
                    make_important_out(o);
                }
            }
        }
        Out::Comment(_) => {}
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

/// Combine parent paths with a ruleset's own selectors, resolving `&` (§2.2/§4).
fn join_selectors(parent: Option<&[String]>, own: &[String]) -> Vec<String> {
    let parents: Vec<String> = match parent {
        Some(p) if !p.is_empty() => p.to_vec(),
        _ => vec![String::new()],
    };
    let mut out = Vec::new();
    for os in own {
        if os.contains('&') {
            for pp in &parents {
                out.push(os.replace('&', pp).trim().to_string());
            }
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

/// Find the byte index of the next `@{` (interpolation start) in a string.
fn find_interp(s: &str) -> Option<usize> {
    s.find("@{")
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
        Out::Decls(decls) => {
            if !has_visible(decls) {
                return None;
            }
            Some(render_decls(decls, &ind, np))
        }
        Out::Rule { selectors, decls } => {
            if !has_visible(decls) {
                return None;
            }
            let header = selectors.join(&format!(",\n{ind}"));
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
        },
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
            Node::AtRule(a) => {
                // An inline no-block directive (`@apply …;`) inside a ruleset.
                let prelude = match &a.prelude {
                    Some(p) => format!(" {}", render_value(p, np)),
                    None => String::new(),
                };
                lines.push(format!("{dind}{}{prelude};", a.name));
            }
            _ => {}
        }
    }
    lines.join("\n")
}
