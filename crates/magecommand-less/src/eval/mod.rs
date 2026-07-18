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

use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{AtRuleBlock, Declaration, Element, MixinParam, Node, Selector};
use crate::color::Color;
use crate::css::{render_value, Css, Warning};
use crate::error::{ErrorKind, LessError};
use crate::functions;
use crate::options::{LessOptions, MathMode};
use crate::resolver::ImportResolver;
use crate::value::Dimension;

/// A scope frame: the (unevaluated) rule list of an entered ruleset. Variable and
/// mixin lookup scan it directly (last-declaration-wins), so lazy eval + forward
/// references fall out (plan §4.1).
type Frame = Rc<Vec<Node>>;

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
    warnings: Vec<Warning>,
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
        warnings: Vec::new(),
    };

    // globalVars / modifyVars are prepended/appended rulesets (plan §2.0). Their
    // implementation is deferred; the default harness passes none.
    let mut outs: Vec<Out> = Vec::new();
    ctx.push_frame(Rc::new(rules.clone()));
    let (_own, children) = ctx.process_body(&rules, None)?;
    outs.extend(children);
    ctx.pop_frame();

    // Output ordering (plan §2.13/§C): `@charset`, then `@import`, then
    // `@namespace`, hoisted to the top of the stylesheet (stable within each).
    outs.sort_by_key(|o| match o {
        Out::At { header, .. } if header.starts_with("@charset") => 0,
        Out::At { header, .. } if header.starts_with("@import") => 1,
        Out::At { header, .. } if header.starts_with("@namespace") => 2,
        _ => 3,
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
    fn eval_rules(
        &mut self,
        rules: &[Node],
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<(), LessError> {
        for rule in rules {
            match rule {
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
                Node::MixinCall(call) => {
                    // Expand the mixin's rules into this body in place, evaluated
                    // with the bound parameters in scope.
                    self.eval_mixin_call(call, self_paths, own, children)?;
                }
                Node::Ruleset(rs) => {
                    self.eval_nested_ruleset(&rs.selectors, &rs.rules, self_paths, children)?;
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

        self.push_frame(Rc::new(rules.to_vec()));
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
                self.push_frame(Rc::new(rules.to_vec()));
                let (own, children) = self.process_body(rules, inner_parent)?;
                self.pop_frame();

                // Declarations first (like less.js Ruleset.genCSS), then nested.
                let mut body_outs: Vec<Out> = Vec::new();
                if has_visible(&own) {
                    body_outs.push(Out::Decls(own));
                }
                body_outs.extend(children);

                // An at-rule with an empty block emits nothing (plan §2.13).
                if body_outs.is_empty() {
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
        // Resolve interpolation in the property name.
        let name = if d.name.contains("@{") || d.name.contains("${") {
            self.interpolate(&d.name)?
        } else {
            d.name.clone()
        };

        if d.custom {
            // Custom properties: NOT evaluated as LESS, but @{} interpolation runs
            // inside the raw value (plan §2.16).
            let raw = match d.value.as_ref() {
                Node::Anonymous(s) => self.interpolate(s)?,
                other => render_value(other, self.opts.num_precision),
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
        let value = self.eval_value(&d.value)?;
        let popped = self.important_scope.pop().flatten();
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
            Node::Paren(inner) => self.eval_paren(inner),
            Node::Call { name, args } => self.eval_call(name, args),
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
        let result = self.eval_value(&val);
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

    fn eval_paren(&mut self, inner: &Node) -> Result<Node, LessError> {
        self.parens += 1;
        let v = self.eval_value(inner);
        self.parens -= 1;
        let v = v?;
        // A math result collapses the paren; a deferred/non-final value keeps it
        // so it emits literally (e.g. inside calc()).
        match v {
            Node::Dimension(_) | Node::Color(_) => Ok(v),
            other => Ok(Node::Paren(Box::new(other))),
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

        // calc() — evaluate variables inside, but suppress math (plan §2.4).
        if lname == "calc" {
            let prev = self.math_on;
            let prev_calc = self.in_calc;
            self.math_on = false;
            self.in_calc = true;
            let mut evaled = Vec::with_capacity(args.len());
            for a in args {
                evaled.push(self.eval_value(a)?);
            }
            self.math_on = prev;
            self.in_calc = prev_calc;
            return Ok(Node::Call {
                name: name.to_string(),
                args: evaled,
            });
        }

        // Evaluate arguments first (the common path).
        let mut evaled = Vec::with_capacity(args.len());
        for a in args {
            evaled.push(self.eval_value(a)?);
        }

        if let Some(result) = functions::dispatch(&lname, &evaled, self.opts.num_precision)? {
            return Ok(result);
        }

        // Unknown / passthrough CSS function: re-emit name(evaluated-args) (§2.7).
        Ok(Node::Call {
            name: name.to_string(),
            args: evaled,
        })
    }

    // ------------------------------------------------------------------
    // Mixins (basic — no guards/patterns/overloading beyond simple filtering)
    // ------------------------------------------------------------------

    fn eval_mixin_call(
        &mut self,
        call: &crate::ast::MixinCall,
        self_paths: Option<&[String]>,
        own: &mut Vec<Node>,
        children: &mut Vec<Out>,
    ) -> Result<(), LessError> {
        if self.mixin_depth > MAX_MIXIN_DEPTH {
            return Err(self.err(ErrorKind::Runtime, "mixin recursion limit exceeded"));
        }
        let path = mixin_path_names(&call.path);
        if path.is_empty() {
            return Ok(());
        }

        // Search frames innermost-first for a matching definition.
        let frames = self.frames.clone();
        for frame in &frames {
            if let Some((params, guard, rules, def_frame)) = find_mixin(frame, &path) {
                let param_frame = self.bind_params(&params, &call.args)?;
                self.mixin_depth += 1;
                // Push param frame (then optional definition frame) atop the
                // caller's stack: params resolve first, then the caller scope —
                // the Magento theming lever (§4.3).
                self.push_frame(Rc::new(param_frame));
                if let Some(df) = &def_frame {
                    self.push_frame(df.clone());
                }
                let guard_ok = match &guard {
                    Some(g) => self.eval_guard(g)?,
                    None => true,
                };
                if guard_ok {
                    // The mixin's own body forms a frame for its local vars/mixins.
                    self.push_frame(Rc::new(rules.clone()));
                    let res = self.eval_rules(&rules, self_paths, own, children);
                    self.pop_frame();
                    if def_frame.is_some() {
                        self.pop_frame();
                    }
                    self.pop_frame();
                    self.mixin_depth -= 1;
                    res?;
                    return Ok(());
                }
                if def_frame.is_some() {
                    self.pop_frame();
                }
                self.pop_frame();
                self.mixin_depth -= 1;
            }
        }
        Err(self.err(
            ErrorKind::Runtime,
            format!(
                "No matching definition was found for `{}(...)`",
                path.join(" ")
            ),
        ))
    }

    fn bind_params(
        &mut self,
        params: &[MixinParam],
        args: &[crate::ast::MixinArg],
    ) -> Result<Vec<Node>, LessError> {
        let mut frame: Vec<Node> = Vec::new();
        // Positional binding + defaults + named. Minimal (no variadic/pattern).
        let mut pos = 0usize;
        // First, apply named args.
        let mut named: Vec<(&str, &Node)> = Vec::new();
        let mut positional: Vec<&Node> = Vec::new();
        for a in args {
            match &a.name {
                Some(n) => named.push((n.trim_start_matches('@'), a.value.as_ref())),
                None => positional.push(a.value.as_ref()),
            }
        }
        for p in params {
            let Some(pname) = &p.name else {
                pos += 1;
                continue;
            };
            let key = pname.trim_start_matches('@').to_string();
            let value_node: Node = if let Some((_, v)) = named.iter().find(|(n, _)| *n == key) {
                self.reparse_arg(v)?
            } else if pos < positional.len() {
                let v = positional[pos];
                pos += 1;
                self.reparse_arg(v)?
            } else if let Some(def) = &p.default {
                self.reparse_arg(def)?
            } else {
                Node::Anonymous(String::new())
            };
            frame.push(Node::VariableDecl {
                name: key,
                value: Box::new(value_node),
                important: String::new(),
                span: Default::default(),
            });
        }
        Ok(frame)
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
    // Guards
    // ------------------------------------------------------------------

    fn eval_guard(&mut self, guard: &Node) -> Result<bool, LessError> {
        // Guards are retained as raw Anonymous text by the structural parser.
        let raw = match guard {
            Node::Anonymous(s) => s.clone(),
            other => render_value(other, self.opts.num_precision),
        };
        self.eval_guard_str(&raw)
    }

    /// Evaluate a `when (...)` guard string (minimal: `and`, comma-OR, simple
    /// comparisons + `true`). Enough for the milestone gate; the full guard
    /// algebra + `default()` is Phase 2.
    fn eval_guard_str(&mut self, raw: &str) -> Result<bool, LessError> {
        let s = raw.trim();
        // Split top-level commas = OR.
        for clause in split_top(s, ',') {
            if self.eval_guard_and(&clause)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn eval_guard_and(&mut self, clause: &str) -> Result<bool, LessError> {
        let mut all = true;
        for part in split_word(clause, "and") {
            if !self.eval_guard_atom(part.trim())? {
                all = false;
                break;
            }
        }
        Ok(all)
    }

    fn eval_guard_atom(&mut self, atom: &str) -> Result<bool, LessError> {
        let a = atom.trim().trim_start_matches('(').trim_end_matches(')').trim();
        if a.eq_ignore_ascii_case("true") {
            return Ok(true);
        }
        // comparison ops
        for (sym, _) in [("<=", 0), (">=", 0), ("=<", 0), ("=", 0), ("<", 0), (">", 0)] {
            if let Some(idx) = a.find(sym) {
                // avoid matching `<` inside `<=`
                if (sym == "<" || sym == ">") && a[idx..].starts_with(&format!("{sym}=")) {
                    continue;
                }
                let (l, r) = a.split_at(idx);
                let r = &r[sym.len()..];
                return self.compare_guard(l.trim(), sym, r.trim());
            }
        }
        // bare value: true iff it evaluates to keyword `true`.
        let v = self.reparse_and_eval(a)?;
        Ok(value_to_plain_string(&v) == "true")
    }

    fn reparse_and_eval(&mut self, src: &str) -> Result<Node, LessError> {
        if let Ok(v) = crate::parser::parse_value_fragment(src, self.opts) {
            self.eval_value(&v)
        } else {
            Ok(Node::Anonymous(src.to_string()))
        }
    }

    fn compare_guard(&mut self, l: &str, op: &str, r: &str) -> Result<bool, LessError> {
        let lv = self.reparse_and_eval(l)?;
        let rv = self.reparse_and_eval(r)?;
        let cmp = compare_values(&lv, &rv);
        Ok(match op {
            "=" => cmp == Some(0),
            "<" => cmp == Some(-1),
            ">" => cmp == Some(1),
            "<=" | "=<" => matches!(cmp, Some(-1) | Some(0)),
            ">=" => matches!(cmp, Some(1) | Some(0)),
            _ => false,
        })
    }

    // ------------------------------------------------------------------
    // Selectors & interpolation
    // ------------------------------------------------------------------

    fn render_selector(&mut self, sel: &Selector) -> Result<String, LessError> {
        let mut s = String::new();
        for el in &sel.elements {
            s.push_str(&combinator_css(&el.combinator));
            if el.value.contains("@{") || el.value.contains("$}") {
                s.push_str(&self.interpolate(&el.value)?);
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
    fn interpolate(&mut self, input: &str) -> Result<String, LessError> {
        let mut s = input.to_string();
        for _ in 0..32 {
            let Some(start) = find_interp(&s) else { break };
            let after = &s[start + 2..];
            let Some(end_rel) = after.find('}') else { break };
            let name = after[..end_rel].trim().to_string();
            let val = self.eval_variable(&name, Default::default())?;
            let replacement = value_to_plain_string(&val);
            let end = start + 2 + end_rel + 1;
            s.replace_range(start..end, &replacement);
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
    for r in frame.iter() {
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

/// Find a mixin definition matching `path` in a frame (recursing into namespaces).
/// Returns `(params, guard, body_rules, optional-def-frame)`.
#[allow(clippy::type_complexity)]
fn find_mixin(
    frame: &Frame,
    path: &[String],
) -> Option<(Vec<MixinParam>, Option<Node>, Vec<Node>, Option<Frame>)> {
    find_mixin_in(frame.as_ref(), path)
}

fn find_mixin_in(
    rules: &[Node],
    path: &[String],
) -> Option<(Vec<MixinParam>, Option<Node>, Vec<Node>, Option<Frame>)> {
    let head = &path[0];
    for r in rules {
        match r {
            Node::MixinDefinition(def) if &def.name == head => {
                if path.len() == 1 {
                    return Some((
                        def.params.clone(),
                        def.guard.as_deref().cloned(),
                        def.rules.clone(),
                        None,
                    ));
                }
                if let Some(found) = find_mixin_in(&def.rules, &path[1..]) {
                    return Some(found);
                }
            }
            Node::Ruleset(rs) => {
                // A ruleset usable as a mixin: a single simple class/id selector.
                if let Some(sel_name) = simple_selector_name(rs) {
                    if &sel_name == head {
                        if path.len() == 1 {
                            return Some((Vec::new(), None, rs.rules.clone(), None));
                        }
                        if let Some(found) = find_mixin_in(&rs.rules, &path[1..]) {
                            return Some(found);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// The callable name of a ruleset used as a mixin (`.foo`/`#bar`), if it is one.
fn simple_selector_name(rs: &crate::ast::Ruleset) -> Option<String> {
    if rs.selectors.len() != 1 {
        return None;
    }
    let sel = &rs.selectors[0];
    if sel.elements.len() != 1 {
        return None;
    }
    let v = &sel.elements[0].value;
    if v.starts_with('.') || v.starts_with('#') {
        Some(v.clone())
    } else {
        None
    }
}

/// Build the mixin-call lookup path (`#ns`, `.m`) from its path elements.
fn mixin_path_names(path: &[Element]) -> Vec<String> {
    path.iter()
        .map(|e| e.value.clone())
        .filter(|v| !v.is_empty())
        .collect()
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
/// equality) — no surrounding quotes for quoted strings (plan §2.1).
fn value_to_plain_string(node: &Node) -> String {
    match node {
        Node::Quoted { value, .. } => value.clone(),
        Node::Keyword(k) => k.clone(),
        Node::Anonymous(s) => s.clone(),
        other => render_value(other, 8),
    }
}

/// Compare two evaluated values (guard comparisons): dimensions numerically
/// (unit-unified), else string equality (plan §2.6). Returns -1/0/1 or None.
fn compare_values(a: &Node, b: &Node) -> Option<i32> {
    if let (Node::Dimension(da), Node::Dimension(db)) = (a, b) {
        let (ua, ub) = if da.unit.is_empty() || db.unit.is_empty() {
            (da.clone(), db.clone())
        } else {
            (da.unify(), db.unify())
        };
        if !ua.unit.is_empty() && !ub.unit.is_empty() && ua.unit.to_unit_string() != ub.unit.to_unit_string() {
            return None;
        }
        return Some(match ua.value.partial_cmp(&ub.value) {
            Some(std::cmp::Ordering::Less) => -1,
            Some(std::cmp::Ordering::Greater) => 1,
            _ => 0,
        });
    }
    let sa = value_to_plain_string(a);
    let sb = value_to_plain_string(b);
    if sa == sb {
        Some(0)
    } else {
        None
    }
}

// ----------------------------------------------------------------------
// Selector joining (JoinSelector, string-level)
// ----------------------------------------------------------------------

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

    #[test]
    fn lazy_last_wins_and_forward_reference() {
        // Forward reference (@var reads @a defined later) + last-declaration-wins.
        assert_eq!(css("@var: @a;\n@a: 1;\n@a: 2;\n.x { y: @var; }"), ".x {\n  y: 2;\n}");
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
}

fn render_decls(decls: &[Node], dind: &str, np: u8) -> String {
    let mut lines = Vec::new();
    for d in decls {
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
