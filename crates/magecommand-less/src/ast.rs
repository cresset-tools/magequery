//! The parsed AST (plan §9.2).
//!
//! One `Node` enum; self-evaluating leaves (`Dimension`, `Color`, `Quoted`,
//! `Keyword`, `Anonymous`) double as runtime values. The tree mirrors the
//! less.js `tree/` node set (plan §1). This step (STEP 3) lands the **complete
//! parse-side node set** — every construct the recursive-descent parser can
//! produce — while evaluation (variable/mixin/operation resolution, `&` join,
//! `:extend`, `@import` inlining) is the next step. Serialization (`genCSS`) for
//! the plain-CSS path lives in [`crate::css`].
//!
//! `Node` is `Send + Sync` so an `Arc<Node>` can be shared, unevaluated, across
//! every compile job (plan §9.6). Every node carries a [`Span`] resolved to
//! line/col lazily via [`crate::lex::LineMap`].

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

/// How a merged declaration combines with same-named siblings (plan §2.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKind {
    /// `prop+:` — comma-merge.
    Comma,
    /// `prop+_:` — space-merge.
    Space,
}

/// One selector: a run of [`Element`]s plus an optional `when` guard (plan §2.2,
/// §2.6). The guard is parsed and retained for the eval step; the plain-CSS
/// serializer ignores it.
#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    /// The elements (each carries its leading combinator).
    pub elements: Vec<Element>,
    /// A CSS/mixin guard condition (`when (...)`), unevaluated.
    pub guard: Option<Box<Node>>,
    /// Source span.
    pub span: Span,
}

/// A single selector element: a combinator plus a value (plan §2.2). The value
/// is kept as source text for this step (structured `&`/interpolation handling
/// lands with the eval join pass); a bare `&` is flagged for that pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    /// Leading combinator: `""` (start), `" "` (descendant), `>`, `+`, `~`, `|`,
    /// `^`, `^^`, or a slashed combinator `/deep/`.
    pub combinator: String,
    /// The element text (`.class`, `#id`, `tag`, `*`, `[attr=v]`, `:hover`,
    /// `(sub)`, `@{var}`, `&`, …).
    pub value: String,
    /// Source span.
    pub span: Span,
}

/// A ruleset: selectors + a body of rules (plan §2.2, §2.5). A ruleset with a
/// parenthesized selector (`.m()`) is a callable, output-suppressed mixin.
#[derive(Debug, Clone, PartialEq)]
pub struct Ruleset {
    /// The selectors this body applies to.
    pub selectors: Vec<Selector>,
    /// The body rules.
    pub rules: Vec<Node>,
    /// Source span.
    pub span: Span,
}

/// The block attached to an at-rule (plan §2.13).
#[derive(Debug, Clone, PartialEq)]
pub enum AtRuleBlock {
    /// No block — the at-rule is `;`-terminated (`@charset`, `@import`,
    /// `@namespace`).
    None,
    /// A braced body of rules (`@media`, `@supports`, `@font-face`, `@page`, …).
    Rules(Vec<Node>),
}

/// A generic at-rule (plan §2.13): `@name <prelude> { … }` or `@name <prelude>;`.
/// `@media`/`@supports`/`@container`/`@layer`/`@font-face`/`@page`/`@keyframes`
/// all use this shape; bubbling/merging semantics are an eval concern.
#[derive(Debug, Clone, PartialEq)]
pub struct AtRule {
    /// Including the leading `@` (`@media`, `@font-face`).
    pub name: String,
    /// The prelude value (media query, font-face is `None`), unevaluated.
    pub prelude: Option<Box<Node>>,
    /// The block, if any.
    pub block: AtRuleBlock,
    /// Source span.
    pub span: Span,
}

/// A property declaration (plan §2.1, §2.10, §2.15).
#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    /// The property name (may carry `@{…}` interpolation pieces as source text).
    pub name: String,
    /// The value expression.
    pub value: Box<Node>,
    /// `!important` (already normalized to ` !important`, or empty).
    pub important: String,
    /// Merge flag, if `prop+:`/`prop+_:` (plan §2.10).
    pub merge: Option<MergeKind>,
    /// A custom property (`--foo`) whose value is raw/anonymous (plan §2.16).
    pub custom: bool,
    /// Source span.
    pub span: Span,
}

/// A parametric-mixin / detached-ruleset parameter (plan §2.5).
#[derive(Debug, Clone, PartialEq)]
pub struct MixinParam {
    /// Parameter name (`@x`), or `None` for a positional literal/pattern param.
    pub name: Option<String>,
    /// Default value / literal pattern, unevaluated.
    pub default: Option<Box<Node>>,
    /// `...` variadic marker.
    pub variadic: bool,
}

/// A parametric mixin definition (plan §2.5). Output-suppressed; callable.
#[derive(Debug, Clone, PartialEq)]
pub struct MixinDefinition {
    /// The mixin name (`.mixin` / `#ns`).
    pub name: String,
    /// Formal parameters.
    pub params: Vec<MixinParam>,
    /// A guard condition (`when (...)`), unevaluated.
    pub guard: Option<Box<Node>>,
    /// The body rules.
    pub rules: Vec<Node>,
    /// Source span.
    pub span: Span,
}

/// A mixin call (plan §2.5): `.mixin(args);`, `#ns > .m()`, `.m() !important`.
#[derive(Debug, Clone, PartialEq)]
pub struct MixinCall {
    /// The lookup path elements (`#ns`, `>`, `.m`) as source text.
    pub path: Vec<Element>,
    /// Positional/named arguments, unevaluated.
    pub args: Vec<MixinArg>,
    /// `!important` forcing (plan §2.15).
    pub important: bool,
    /// Source span.
    pub span: Span,
}

/// One mixin-call argument (plan §2.5): positional, or `@name: value` named.
#[derive(Debug, Clone, PartialEq)]
pub struct MixinArg {
    /// The argument name, when written `@name: value`.
    pub name: Option<String>,
    /// The argument value.
    pub value: Box<Node>,
}

/// An `@import` resolved by the pre-eval import pass (plan §2.9 stage 1, never
/// parsed): the fetched + parsed payload attached at the import's source
/// position. Stage 2 (eval) splices/emits it position-preservingly.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportResolved {
    /// The imported LESS file's parsed rules (empty for `skip`/`(inline)`).
    pub rules: Vec<Node>,
    /// The verbatim payload of an `(inline)` import.
    pub inline: Option<String>,
    /// The resolver's canonical path — the once-dedup key at eval.
    pub full_path: String,
    /// Hard skip decided at fetch: a duplicate/cyclic once-import, or a missing
    /// `(optional)` file.
    pub skip: bool,
    /// `(multiple)` — exempt from once-dedup.
    pub multiple: bool,
    /// `(reference)` — rules are visibility-blocked (§2.8) until referenced.
    pub reference: bool,
    /// Media features to wrap the payload in at eval (`@import "x" screen`).
    pub features: Option<Box<Node>>,
    /// The imported file's directory (base for its own relative imports/urls).
    pub current_directory: String,
    /// The `rootpath` in effect for urls generated from this file (§2.18).
    pub rootpath: String,
    /// The original import statement's span.
    pub span: Span,
}

/// The AST / runtime-value node (plan §9.2). `#[non_exhaustive]` so the set can
/// grow (the eval step adds a few runtime-only kinds) without a major bump.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Node {
    /// The parse root — the top-level rules (`root=true, firstRoot=true`).
    Root(Vec<Node>),

    // --- structural nodes ---
    /// A selector body.
    Ruleset(Ruleset),
    /// A property declaration.
    Declaration(Declaration),
    /// A generic at-rule.
    AtRule(AtRule),
    /// A variable declaration `@x: value;` (produces no CSS; plan §2.1). A
    /// trailing `!important` propagates up to any declaration that reads it
    /// (`importantScope`, plan §2.1); stored normalized (` !important` or empty).
    VariableDecl { name: String, value: Box<Node>, important: String, span: Span },
    /// A detached ruleset value `@dr: { … };` / an inline `{ … }` (plan §2.11).
    DetachedRuleset { rules: Vec<Node>, span: Span },
    /// A mixin definition.
    MixinDefinition(MixinDefinition),
    /// A mixin call.
    MixinCall(MixinCall),
    /// An eval-only closure produced by scope-injection (never parsed): a mixin
    /// definition frozen with the evaluation frames live at its injection site,
    /// so a later call resolves the enclosing mixin's *bound parameters* (closure
    /// over params, plan §4.3). `scope` indexes the evaluator's captured-frame
    /// side table (the frames themselves can't live in `Node`, which stays
    /// `Send + Sync` for `Arc`-sharing across compile jobs).
    Closure { inner: Box<Node>, scope: u64 },
    /// An `@import` (plan §2.9). `options`/`features` retained as source text.
    Import { path: Box<Node>, options: Vec<String>, features: Option<Box<Node>>, span: Span },
    /// An `@import` resolved by the pre-eval import pass (eval-only, plan §2.9).
    ImportResolved(Box<ImportResolved>),
    /// A comment. `line` = a `//` comment (stripped from output; plan §2.3).
    Comment { text: String, line: bool, span: Span },
    /// The `//@magento_import` directive (only in `magento_mode`; plan §7.1).
    MagentoImport { path: Box<Node>, reference: bool, span: Span },

    // --- value nodes (self-evaluating leaves double as runtime values) ---
    /// A comma-separated value list (plan value model). genCSS joins with `, `.
    Value(Vec<Node>),
    /// A space-separated expression. genCSS joins with ` `.
    Expression(Vec<Node>),
    /// Raw passthrough text (an anonymous value or unparsed run; plan value model).
    Anonymous(String),
    /// A number with a (possibly compound) unit (plan §2.18/§9.3). The rich
    /// [`crate::value::Dimension`] doubles as the runtime value: it carries the
    /// open-multiset [`crate::unit::Unit`] + `backupUnit` that operations need.
    Dimension(crate::value::Dimension),
    /// A color value (plan §2.18/§H3). The rich [`crate::color::Color`] keeps the
    /// original literal for round-trip and RGBA channels for per-channel math.
    Color(crate::color::Color),
    /// A quoted string (`escaped` = the `~"…"` form; plan §2.14).
    Quoted { escaped: bool, quote: char, value: String },
    /// A CSS keyword / bare identifier (incl. named colors, emitted verbatim).
    Keyword(String),
    /// A function call `name(args)` (plan §2.7). Unknown names pass through.
    Call { name: String, args: Vec<Node> },
    /// A `url(...)` value (plan §2.9/§2.18).
    Url(Box<Node>),
    /// A parenthesized value (plan §2.4). `in_op` mirrors less.js `parensInOp`:
    /// set when the paren is an operand of an operation (or a `-` negation) — the
    /// only case genCSS keeps literal parens for a non-folded result (§2.4/calc).
    Paren { inner: Box<Node>, in_op: bool },
    /// An IE-filter style `key=value` function argument (less.js `Assignment`,
    /// plan §2.17): `alpha(opacity=20)`, `progid:…(startColorstr="#333333")`.
    Assignment { key: String, value: Box<Node> },
    /// A binary operation `left op right` (plan §2.4). Emitted literally when
    /// math is off for the operator; `spaced` records source whitespace around
    /// the operator (less.js `Operation.isSpaced`), which drives genCSS spacing.
    Operation { op: String, left: Box<Node>, right: Box<Node>, spaced: bool },
    /// A unary negation `-value` (plan §2.4).
    Negative(Box<Node>),
    /// A variable reference `@name` (plan §2.1). Resolved at use time (eval step).
    Variable { name: String, span: Span },
    /// A `@{name}` string-interpolation reference (plan §2.1). Kept as its source
    /// spelling until the eval step substitutes it.
    Interpolation { name: String, span: Span },
    /// A `@@name` variable-variable (plan §2.1).
    VariableVariable { name: String, span: Span },
    /// A `$prop` property-as-variable accessor (plan §2.1).
    PropertyAccessor { name: String, span: Span },
    /// A detached-ruleset call `@dr();` (statement) / `@dr()` (value) — less.js
    /// `VariableCall` (plan §2.11/§2.12). `name` has no leading `@`.
    VariableCall { name: String, span: Span },
    /// A map/namespace lookup chain `target[key][key…]` — less.js
    /// `NamespaceValue` (plan §2.12). `target` is a [`Node::VariableCall`] or
    /// [`Node::MixinCall`]; each key keeps its source spelling (`""` for the
    /// unnamed `[]`, `@var`, `@@dyn`, `$prop`, `$@dyn`, or a bare property name).
    Lookup { target: Box<Node>, keys: Vec<String>, span: Span },
}

impl Node {
    /// Whether this node contributes output in the plain-CSS serializer.
    ///
    /// Variable/mixin definitions and `//` comments produce nothing; a ruleset or
    /// braced at-rule is visible only if its body has a visible rule (empty blocks
    /// are pruned, mirroring less.js's `ToCSSVisitor`, plan §4/§2.13).
    pub fn is_output_visible(&self) -> bool {
        match self {
            Node::Comment { line, .. } => !line,
            Node::VariableDecl { .. }
            | Node::MixinDefinition(_)
            | Node::Closure { .. }
            | Node::DetachedRuleset { .. } => false,
            // A bare mixin call yields nothing until evaluated; the plain-CSS
            // path never contains one, so treat it as invisible for pruning.
            Node::MixinCall(_) | Node::VariableCall { .. } => false,
            Node::Ruleset(r) => r.rules.iter().any(Node::is_output_visible),
            Node::AtRule(a) => match &a.block {
                AtRuleBlock::None => true,
                AtRuleBlock::Rules(rules) => rules.iter().any(Node::is_output_visible),
            },
            Node::Anonymous(s) => !s.is_empty(),
            Node::ImportResolved(ir) => {
                !ir.skip && (ir.inline.is_some() || ir.rules.iter().any(Node::is_output_visible))
            }
            _ => true,
        }
    }
}
