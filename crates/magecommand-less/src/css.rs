//! Output CSS tree + genCSS (plan §4.7, §9.4).
//!
//! Our own `genCSS` owns **both** the diffable expanded `.css` and the
//! byte-exact `compress` output (the latter gated by the less.js
//! `compress`/`compression` fixtures, §C4) — neither is delegated to
//! lightningcss, which is confined to `.min.css` (plan §9.4).
//!
//! This step (STEP 3) implements the **expanded, plain-CSS serializer**: it walks
//! the parsed [`crate::ast::Node`] tree and reproduces less.js's `genCSS` spacing
//! exactly (ruleset/at-rule indentation §4.7, selector/combinator spacing,
//! declaration `prop: value;`, value expression/list joins). LESS-feature
//! *evaluation* (variables, mixins, operations, `&` join, `:extend`, `@import`
//! inlining) is the next step; the serializer emits the tree as-parsed, which is
//! correct for plain CSS and is the basis the evaluator will feed.

use crate::ast::{AtRuleBlock, Node};
use crate::error::LessError;

/// A non-fatal problem collected during compile (plan §4.6): `extend '…' has no
/// matches`, optional-import miss, ignored `@plugin`/`@dumpLineNumbers`,
/// deprecations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// The message.
    pub message: String,
    /// Source file, if known.
    pub filename: Option<String>,
    /// 1-based line, if known.
    pub line: Option<usize>,
}

/// The compile result (plan §9.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Css {
    /// The rendered CSS. (§9.5 field name is `code`.)
    pub code: String,
    /// Absolute paths of every file imported during compilation.
    pub imports: Vec<String>,
    /// Source map JSON, when `sourceMap` is enabled (OUT for now — §8).
    pub source_map: Option<String>,
    /// Non-fatal diagnostics.
    pub warnings: Vec<Warning>,
}

impl Css {
    /// A result carrying just rendered code (no imports/warnings).
    pub fn from_code(code: impl Into<String>) -> Self {
        Css {
            code: code.into(),
            ..Css::default()
        }
    }
}

/// genCSS context (plan §4.7): the current indentation depth and compress flag.
#[derive(Debug, Clone, Default)]
pub struct GenContext {
    /// Whether to emit the compressed serializer (gated by the `compress`
    /// fixtures, §C4): comma lists join with `,`, dimensions drop the leading
    /// zero (and the unit of a zero length), computed colors hex-shorten.
    pub compress: bool,
    /// Current nesting depth (`tabLevel` in less.js).
    pub tab_level: usize,
    /// less.js `numPrecision` (default 8) for dimension `fround`.
    pub num_precision: u8,
    /// less.php flavor (`php_zero_units`, §C4): keep the unit on a zero
    /// length under compress — less.php's strict `0.0 === 0` never strips.
    pub keep_zero_units: bool,
}

/// Render a parsed [`Node::Root`] to expanded CSS (the `firstRoot`/`root=true`
/// entry point of less.js's `Ruleset.genCSS`, plan §4.7).
pub fn render_root(root: &Node) -> String {
    let mut out = String::new();
    let mut ctx = GenContext {
        compress: false,
        tab_level: 0,
        num_precision: 8,
        keep_zero_units: false,
    };
    if let Node::Root(rules) = root {
        gen_root_rules(rules, &mut ctx, &mut out);
    } else {
        gen(root, &mut ctx, &mut out);
    }
    out
}

/// Render a single (already-evaluated) value node to its CSS string — the
/// evaluator's flat serializer reuses this so value spacing/number formatting
/// lives in one place (plan §4.7).
pub(crate) fn render_value(node: &Node, num_precision: u8) -> String {
    render_value_cz(node, num_precision, false, false)
}

/// [`render_value`] with explicit compress + less.php zero-unit flavor flags
/// (§C4) — the evaluator's FINAL-output sites use this; internal comparisons
/// (mixin pattern matching, declaration dedup) stay on the expanded form so
/// identities never shift. `keep_zero_units` (`php_zero_units`) disables the
/// compress "zero length drops its unit" strip.
pub(crate) fn render_value_cz(
    node: &Node,
    num_precision: u8,
    compress: bool,
    keep_zero_units: bool,
) -> String {
    let mut ctx = GenContext {
        compress,
        tab_level: 0,
        num_precision,
        keep_zero_units,
    };
    let mut out = String::new();
    gen(node, &mut ctx, &mut out);
    out
}

/// The `root=true, firstRoot=true` rule list (no selector/braces; §4.7).
fn gen_root_rules(rules: &[Node], ctx: &mut GenContext, out: &mut String) {
    let visible: Vec<&Node> = rules.iter().filter(|r| r.is_output_visible()).collect();
    for (i, rule) in visible.iter().enumerate() {
        if i > 0 {
            out.push('\n'); // tabRuleStr == "" at root
        }
        gen(rule, ctx, out);
    }
    if !visible.is_empty() {
        out.push('\n'); // firstRoot trailing newline
    }
}

/// Serialize one node (dispatch mirroring each `tree/*.js` `genCSS`).
fn gen(node: &Node, ctx: &mut GenContext, out: &mut String) {
    match node {
        Node::Root(rules) => gen_root_rules(rules, ctx, out),
        // Transparent for serialization — the file tag only matters at eval.
        Node::WithFile { inner, .. } => gen(inner, ctx, out),
        // Inline JS never renders — eval always errors on it first (§C-jserr).
        Node::JavaScript { .. } => {}
        Node::Ruleset(r) => gen_ruleset(&r.selectors, &r.rules, ctx, out),
        Node::Declaration(d) => {
            out.push_str(&d.name);
            out.push_str(if ctx.compress { ":" } else { ": " });
            gen(&d.value, ctx, out);
            out.push_str(&d.important);
            out.push(';');
        }
        Node::AtRule(a) => {
            out.push_str(&a.name);
            if let Some(p) = &a.prelude {
                out.push(' ');
                gen(p, ctx, out);
            }
            match &a.block {
                AtRuleBlock::None => out.push(';'),
                AtRuleBlock::Rules(rules) => gen_at_block(rules, ctx, out),
            }
        }
        Node::Import {
            path, features, ..
        } => {
            out.push_str("@import ");
            gen(path, ctx, out);
            if let Some(f) = features {
                out.push(' ');
                gen(f, ctx, out);
            }
            out.push(';');
        }
        Node::Comment { text, line, .. } => {
            if !line {
                out.push_str(text);
            }
        }

        // --- value nodes ---
        Node::Value(items) => {
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    // less.js `Value.genCSS`: the comma list is the ONE value
                    // join that compresses (`Call` args keep `, ` even then).
                    out.push_str(if ctx.compress { "," } else { ", " });
                }
                gen(it, ctx, out);
            }
        }
        Node::Expression(items) => {
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    // less.js skips the space before an Anonymous ",".
                    let next_is_comma = matches!(it, Node::Anonymous(s) if s == ",");
                    if !next_is_comma {
                        out.push(' ');
                    }
                }
                gen(it, ctx, out);
            }
        }
        Node::Anonymous(s) => out.push_str(s),
        Node::Dimension(d) => {
            // less.js `Dimension.genCSS` compress branch (§2.18), decided on
            // the FROUNDED value (which `format_number` already applied): a
            // zero length drops its unit; `0 < v < 1` drops the leading zero.
            // less.php never takes the zero branch (`php_zero_units`): its
            // strict `$value === 0` compares a floatval'd 0.0 to int 0.
            let s = format_number(d.value, ctx.num_precision);
            let zero_length =
                ctx.compress && !ctx.keep_zero_units && s == "0" && d.unit.is_length();
            if ctx.compress && s.starts_with("0.") {
                out.push_str(&s[1..]);
            } else {
                out.push_str(&s);
            }
            if !zero_length {
                d.unit.gen_css(false, out);
            }
        }
        Node::Color(c) => out.push_str(&c.to_css_c(ctx.num_precision, ctx.compress)),
        Node::Quoted {
            escaped,
            quote,
            value,
        } => {
            if *escaped {
                out.push_str(value);
            } else {
                out.push(*quote);
                out.push_str(value);
                out.push(*quote);
            }
        }
        Node::Keyword(k) => out.push_str(k),
        Node::Call { name, args, .. } => {
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                gen(a, ctx, out);
            }
            out.push(')');
        }
        Node::Url(inner) => {
            out.push_str("url(");
            gen(inner, ctx, out);
            out.push(')');
        }
        Node::Paren { inner, .. } => {
            out.push('(');
            gen(inner, ctx, out);
            out.push(')');
        }
        Node::Assignment { key, value } => {
            out.push_str(key);
            out.push('=');
            gen(value, ctx, out);
        }
        Node::Operation {
            op,
            left,
            right,
            spaced,
        } => {
            gen(left, ctx, out);
            if *spaced {
                out.push(' ');
            }
            out.push_str(op);
            if *spaced {
                out.push(' ');
            }
            gen(right, ctx, out);
        }
        Node::Negative(inner) => {
            out.push('-');
            gen(inner, ctx, out);
        }
        Node::Variable { name, .. } => {
            out.push('@');
            out.push_str(name);
        }
        Node::VariableVariable { name, .. } => {
            out.push_str("@@");
            out.push_str(name);
        }
        Node::Interpolation { name, .. } => {
            out.push_str("@{");
            out.push_str(name);
            out.push('}');
        }
        Node::PropertyAccessor { name, .. } => {
            out.push('$');
            out.push_str(name);
        }
        Node::VariableCall { name, .. } => {
            out.push('@');
            out.push_str(name);
            out.push_str("()");
        }
        Node::Lookup { target, keys, .. } => {
            gen(target, ctx, out);
            for k in keys {
                out.push('[');
                out.push_str(k);
                out.push(']');
            }
        }

        // Non-output nodes (variable/mixin definitions, detached rulesets, bare
        // mixin calls, the magento directive) emit nothing in the plain-CSS path.
        Node::VariableDecl { .. }
        | Node::DetachedRuleset { .. }
        | Node::MixinDefinition(_)
        | Node::Closure { .. }
        | Node::MixinCall(_)
        | Node::ImportResolved(_)
        | Node::ExtendRule(_)
        | Node::MagentoImport { .. }
        | Node::FileEnter(_)
        | Node::FileExit => {}
    }
}

/// Strip the silent comments out of a DECLARATION VALUE tree (§C4): less.js's
/// toCSSVisitor removes them before genCSS, so under compress the list joins
/// skip them entirely — `grey, /* blue */ orange` compresses to `grey,orange`.
/// Bang comments stay. Applied only where the visitor would reach (declaration
/// values at final render) — an interpolated property NAME was flattened to a
/// string at eval time, so its comments survive, exactly like less.js.
pub(crate) fn strip_value_comments(node: &Node) -> Node {
    fn keep(n: &Node) -> bool {
        !matches!(n, Node::Comment { line, text, .. }
                  if *line || text.as_bytes().get(2) != Some(&b'!'))
    }
    fn walk(n: &Node) -> Node {
        match n {
            Node::Value(items) => {
                Node::Value(items.iter().filter(|i| keep(i)).map(walk).collect())
            }
            Node::Expression(items) => {
                Node::Expression(items.iter().filter(|i| keep(i)).map(walk).collect())
            }
            Node::Paren { inner, in_op } => Node::Paren {
                inner: Box::new(walk(inner)),
                in_op: *in_op,
            },
            Node::Call { name, args, span } => Node::Call {
                name: name.clone(),
                args: args.iter().filter(|a| keep(a)).map(walk).collect(),
                span: *span,
            },
            other => other.clone(),
        }
    }
    walk(node)
}

/// A nested ruleset (`root=false`; §4.7). `selectors` render as paths joined by
/// `,\n<tabSet>`, then the braced body indented by `<tabRule>`.
fn gen_ruleset(
    selectors: &[crate::ast::Selector],
    rules: &[Node],
    ctx: &mut GenContext,
    out: &mut String,
) {
    ctx.tab_level += 1;
    let tab_rule = "  ".repeat(ctx.tab_level);
    let tab_set = "  ".repeat(ctx.tab_level.saturating_sub(1));

    for (i, sel) in selectors.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
            out.push_str(&tab_set);
        }
        gen_selector(sel, out);
    }
    out.push_str(" {\n");
    out.push_str(&tab_rule);

    let visible: Vec<&Node> = rules.iter().filter(|r| r.is_output_visible()).collect();
    for (i, rule) in visible.iter().enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(&tab_rule);
        }
        gen(rule, ctx, out);
    }

    out.push('\n');
    out.push_str(&tab_set);
    out.push('}');
    ctx.tab_level -= 1;
}

/// An at-rule braced body (`AtRule.outputRuleset`, §4.7). Non-compress spacing:
/// `tabSet = "\n" + 2·(tabLevel-1) spaces`, `tabRule = tabSet + "  "`.
fn gen_at_block(rules: &[Node], ctx: &mut GenContext, out: &mut String) {
    ctx.tab_level += 1;
    let tab_set = format!("\n{}", "  ".repeat(ctx.tab_level.saturating_sub(1)));
    let tab_rule = format!("{tab_set}  ");

    let visible: Vec<&Node> = rules.iter().filter(|r| r.is_output_visible()).collect();
    if visible.is_empty() {
        out.push_str(&format!(" {{{tab_set}}}"));
    } else {
        out.push_str(&format!(" {{{tab_rule}"));
        for (i, rule) in visible.iter().enumerate() {
            if i > 0 {
                out.push_str(&tab_rule);
            }
            gen(rule, ctx, out);
        }
        out.push_str(&format!("{tab_set}}}"));
    }
    ctx.tab_level -= 1;
}

/// Serialize a selector: firstSelector=true (no leading descendant space), each
/// element = combinator + value (less.js `Selector`/`Element`/`Combinator`).
fn gen_selector(sel: &crate::ast::Selector, out: &mut String) {
    for el in &sel.elements {
        out.push_str(&combinator_css(&el.combinator));
        out.push_str(&el.value);
    }
}

/// Combinator spacing (less.js `Combinator.genCSS`): `>`/`+`/`~`/`^`/`^^` get a
/// space on each side in expanded output; ``/` `/`|` do not (§4.7).
fn combinator_css(c: &str) -> String {
    let no_space = matches!(c, "" | " " | "|");
    if no_space {
        c.to_string()
    } else {
        format!(" {c} ")
    }
}

use crate::value::format_number;

/// Serialize a CSS identifier with proper escaping, via cssparser (plan §9:
/// cssparser is used only for CSS-side serialization helpers, never to tokenize
/// LESS). A thin wrapper so the rest of genCSS stays cssparser-agnostic.
#[allow(dead_code)]
pub(crate) fn serialize_ident(name: &str) -> Result<String, LessError> {
    let mut out = String::with_capacity(name.len());
    cssparser::serialize_identifier(name, &mut out).map_err(|_| {
        LessError::new(
            crate::error::ErrorKind::Runtime,
            "identifier serialization failed",
        )
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_ident_escapes_leading_digit() {
        // cssparser escapes a leading digit so the identifier round-trips.
        assert_eq!(serialize_ident("1a").unwrap(), "\\31 a");
        assert_eq!(serialize_ident("foo-bar").unwrap(), "foo-bar");
    }

    #[test]
    fn number_formatting_matches_less_js() {
        assert_eq!(format_number(2.0, 8), "2");
        assert_eq!(format_number(0.4, 8), "0.4");
        assert_eq!(format_number(-1.0, 8), "-1");
        assert_eq!(format_number(3.5, 8), "3.5");
        assert_eq!(format_number(-0.0000000001, 8), "0"); // frounds to 0
        assert_eq!(format_number(400.0, 8), "400");
    }

    #[test]
    fn combinator_spacing() {
        assert_eq!(combinator_css(""), "");
        assert_eq!(combinator_css(" "), " ");
        assert_eq!(combinator_css(">"), " > ");
        assert_eq!(combinator_css("+"), " + ");
    }
}
