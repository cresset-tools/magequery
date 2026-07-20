//! `.min.css` / `.min.js` production — the minification building block of
//! `static deploy`.
//!
//! ## Deliberately NOT byte-parity
//!
//! This is the one place in the static pipeline where we do not chase
//! Magento's exact bytes. Magento minifies with `tubalmartin/cssmin` v4.1.1
//! (a YUI Compressor port) and `JShrink` (whitespace/comment stripping only,
//! no renaming) — both unmaintained, and byte-reproducing them buys nothing:
//! a `.min` artifact's only contract is *semantic equivalence* with its
//! source. The locked tooling is **lightningcss** for CSS and the **oxc**
//! suite for JS (versions pinned exactly in `Cargo.toml`; both ecosystems
//! churn, so bumps must be deliberate and re-run the gates below).
//!
//! ## The gates are semantic, and they bite
//!
//! - **CSS**: for the compiled blank + luma `styles-m`, (1) the corpus must
//!   parse with **zero recovery warnings** (so `error_recovery` never
//!   silently dropped anything from OUR corpus); (2) the minified output
//!   must reparse **strictly** and re-minify to **byte-identical** output
//!   (fixpoint — any print-step loss or instability breaks it); (3) the
//!   existing order-preserving semantic differ ([`super::cssdiff`]), run
//!   over the expanded re-prints of the input's and the minified output's
//!   parses (both sides in lightningcss's canonical spelling, so every
//!   finding is genuinely introduced by the minified *print*), must report
//!   **zero structural findings** — no rule or declaration may appear,
//!   vanish or move — and only value respellings from a pinned allowlist of
//!   spec-equal substitutions the minify printer applies (today:
//!   `font-weight: normal→400`/`bold→700`, the translate-family matrix
//!   collapses like `translate3d(0, 0, 0)`→`translate(0)`, the legacy
//!   `-webkit-gradient` position keywords, and `calc(-A + B)`→`calc(B -
//!   A)`; anything new after a version bump fails the gate until reviewed).
//!
//!   Why round-trip rather than a raw input-vs-minified diff: lightningcss
//!   is a *typed* parser/serializer, and value canonicalization is inherent
//!   to that model with **no setting to disable it** — verified empirically
//!   on this corpus: colors respell to their shortest form (`#ff0000` →
//!   `red`, `transparent` → `#00000000`), font-family quoting drops
//!   (`'Open Sans'` → `Open Sans`), zero lengths drop units (`0rem` → `0`),
//!   string escapes are cooked (`'\2014 \00A0'` → the raw characters),
//!   `list-style: none none` collapses, and — the one genuine fidelity loss
//!   — numbers pass through `f32` (`1.42857143` → `1.42857`). A raw diff of
//!   blank's styles-m therefore reports ~2000 findings, almost all
//!   semantics-preserving respellings the pinned differ rightly refuses to
//!   normalize (its normalization table is pinned to the less.php oracle,
//!   and must not be loosened for this gate). Structural minification is
//!   still fully gated: [`StyleSheet::minify`] — the transform/optimization
//!   pass (rule merging, shorthand collapsing, downleveling) — is **never
//!   called** (we want minification, not optimization), and the round-trip
//!   catches anything the minified *print* drops, truncates, reorders or
//!   mis-escapes. The raw finding count is reported alongside as data.
//! - **JS**: every `.js` file of the deployed blank/luma package view must
//!   minify without error, the output must **reparse** cleanly, and every
//!   `define(`/`require(` dependency-array string literal must survive
//!   verbatim (the env-gated sweep below).
//!
//! ## Conservative settings, and why
//!
//! CSS ([`minify_css`]):
//! - `targets` empty — no vendor-prefix adding/stripping, no downleveling:
//!   the input is the LESS pipeline's output, already written for the
//!   browsers Magento supports, and we must not assume newer ones;
//! - `error_recovery: true` — real Magento CSS carries hacky/legacy
//!   constructs (star hacks, malformed third-party rules); a minifier that
//!   errors on one weird rule would break the whole deploy. **Recovery is
//!   lossy**: an invalid declaration empties its whole declaration block
//!   (probed: `.a { color: red; *zoom: 1 }` → `.a{}`) — which is why every
//!   recovery is surfaced as a warning on [`MinifiedCss::warnings`], and the
//!   gate demands zero of them on the blank/luma corpus (which carries no
//!   star/underscore hacks; its IE-legacy `filter: alpha(…)`/quoted progid
//!   values parse fine). The caller decides how loud a warning is;
//! - leading `/*! … */` license comments are re-emitted by lightningcss;
//!   other comments are dropped (cssmin behaves the same way).
//!
//! JS ([`minify_js`]):
//! - the corpus is old-school AMD/ES5 (plus pre-minified ES5 libs), loaded
//!   as classic scripts. The compressor's `target` is pinned to **ES5**
//!   (`Engine::Es → 5`, below the parseable `es2015` floor of the string
//!   API, hence built by hand), so no newer-than-input syntax is injected
//!   (`??`, `?.`, `||=`, template literals, optional catch binding — the
//!   features oxc's peephole passes would otherwise use);
//! - parsed as [`SourceType::unambiguous`]: classic script unless a file
//!   carries ESM syntax. Script top-level bindings are globals and are
//!   never mangled or removed (oxc's own rule for script sources);
//! - `require`/`exports`/`module` are **reserved** from mangling: RequireJS
//!   CJS-sugar (`define(function (require) …)`) extracts dependencies by
//!   regex-scanning `factory.toString()` for literal `require("…")` calls —
//!   renaming that parameter would silently erase dependencies;
//! - `drop_debugger: false` — strictly semantics-preserving (JShrink keeps
//!   `debugger;` too);
//! - `/*!` / `@license` / `@preserve` comments are kept inline
//!   ([`LegalComment::Inline`]), matching JShrink, which preserves `/*!`
//!   blocks; all other comments are dropped;
//! - a parse error is a per-file [`MinifyError`] carrying the file name —
//!   never a panic, and never "minified" output from a broken AST. The
//!   caller (the future `static deploy`) fails loudly, §7.5-style;
//! - **the output fixup pass** ([`fixup_output`]): two oxc transforms have
//!   no disabling knob and violate the corpus constraints, so [`minify_js`]
//!   reparses its own output and reverses exactly them, span-precisely:
//!   1. *detemplate* — the minify codegen prints every string literal with
//!      whichever quote is shortest *including the backtick* (a tie prefers
//!      it), i.e. as an ES2015 template literal, which both injects
//!      post-ES5 syntax and breaks RequireJS's CJS-sugar dependency scanner
//!      (its regex only recognizes `require("…")`/`require('…')`). Every
//!      untagged, substitution-free template literal is rewritten back into
//!      a plain-quoted string (`"` or `'`, whichever escapes less — SVG
//!      icon packs are double-quote-heavy). Templates the *input* legitimately
//!      contained (with substitutions, or tagged) are left alone — the rule
//!      is "no newer-than-input syntax", not "no modern syntax";
//!   2. *dep-array re-expansion* — the compressor rewrites any all-string
//!      array of ~26+ elements into `"a.b.…".split(".")`
//!      (`substitute_array_expression`, unconditional). Runtime-equivalent,
//!      but it erases the literal dependency array of a `define(`/
//!      `require(` call (jquery/compat.js's 36 deps, really), breaking
//!      every textual dep scanner. Split-calls of that shape appearing as a
//!      DIRECT ARGUMENT of a `define(`/`require(` call are re-expanded to
//!      the literal array (computed exactly as the runtime would); the
//!      substitution is kept everywhere else, where it is pure size win.
//!
//! [`StyleSheet::minify`]: lightningcss::stylesheet::StyleSheet::minify
//! [`SourceType::unambiguous`]: oxc_span::SourceType::unambiguous

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Expression, TaggedTemplateExpression, TemplateLiteral};
use oxc_ast_visit::Visit;
use oxc_codegen::{Codegen, CodegenOptions, CommentOptions, LegalComment};
use oxc_compat::{Engine, EngineTargets, Version};
use oxc_minifier::{
    CompressOptions, MangleOptions, Minifier, MinifierOptions,
};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// A minification failure, attributed to the input file when known.
#[derive(Debug)]
pub struct MinifyError {
    /// The file being minified (as given by the caller), when known.
    pub file: Option<String>,
    /// What went wrong (the underlying parser/printer rendering).
    pub message: String,
}

impl fmt::Display for MinifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{file}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for MinifyError {}

/// Minified CSS plus the parser's recovery warnings (one per rule/declaration
/// lightningcss dropped under `error_recovery` — surfaced, never swallowed:
/// a dropped rule is a semantic change the caller must get to see).
#[derive(Debug)]
pub struct MinifiedCss {
    /// The minified stylesheet.
    pub css: String,
    /// Human-readable recovery warnings, empty on a clean parse.
    pub warnings: Vec<String>,
}

/// Minify CSS to its `.min.css` form: parse + re-print with whitespace
/// removed. **Serialization-only** — `StyleSheet::minify` (lightningcss's
/// structural optimizer) is deliberately not called, and `targets` stays
/// empty, so rules are neither merged, downleveled, nor re-prefixed; see the
/// module docs for why the semantic gate depends on this.
pub fn minify_css(source: &str) -> Result<MinifiedCss, MinifyError> {
    let err = |message: String| MinifyError { file: None, message };
    let minify_print = |sheet: &StyleSheet| {
        sheet
            .to_css(PrinterOptions {
                minify: true,
                ..PrinterOptions::default()
            })
            .map(|out| out.code)
            .map_err(|e| err(e.to_string()))
    };

    let warnings = Arc::new(RwLock::new(Vec::new()));
    let options = ParserOptions {
        error_recovery: true,
        warnings: Some(Arc::clone(&warnings)),
        ..ParserOptions::default()
    };
    let sheet = StyleSheet::parse(source, options).map_err(|e| err(e.to_string()))?;
    let mut css = minify_print(&sheet)?;

    // Iterate to the printer's fixpoint: lightningcss's minify-mode value
    // substitutions are not one-step stable (probed on the corpus:
    // `translate3d(0, 0, 0)` prints as `translate(0,0)`, which only the
    // NEXT round prints as `translate(0)`). The artifact must be its own
    // fixpoint — re-minifying a `.min.css` must be a no-op — so reparse
    // (strictly: our own output must parse without recovery) and re-print
    // until stable. Two rounds settle real corpora; the cap turns an
    // upstream oscillation into a loud error instead of an unstable file.
    for round in 0.. {
        let next = {
            let sheet = StyleSheet::parse(&css, ParserOptions::default())
                .map_err(|e| err(format!("minified output failed to reparse (internal): {e}")))?;
            minify_print(&sheet)?
        };
        if next == css {
            break;
        }
        if round >= 3 {
            return Err(err("minified output did not stabilize (internal)".to_string()));
        }
        css = next;
    }

    let warnings = warnings
        .read()
        .map(|w| w.iter().map(|e| e.to_string()).collect())
        .unwrap_or_default();
    Ok(MinifiedCss { css, warnings })
}

/// Minify JavaScript to its `.min.js` form: oxc parse → compress + mangle →
/// codegen. `filename` attributes errors (and nothing else — the content is
/// `source`). Settings are the conservative set in the module docs: ES5
/// output floor, script-mode top levels untouched, `require`/`exports`/
/// `module` reserved, legal comments kept.
pub fn minify_js(source: &str, filename: &str) -> Result<String, MinifyError> {
    let err = |message: String| MinifyError {
        file: Some(filename.to_string()),
        message,
    };

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::unambiguous()).parse();
    // Error severity fails (a recovered-but-broken AST must never be
    // "minified"); warning-severity diagnostics are tolerated — legacy code
    // trips them routinely.
    if parsed.panicked || parsed.diagnostics.has_errors() {
        let first = parsed
            .diagnostics
            .errors()
            .next()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "parser panicked".to_string());
        return Err(err(format!("parse error: {first}")));
    }
    let mut program = parsed.program;

    // ES5 floor: `Engine::Es → 5` marks every ES2015+ feature unsupported, so
    // the peephole passes can't inject newer-than-input syntax. (The string
    // API bottoms out at `es2015`; the map is public, so build it directly.)
    let mut target = EngineTargets::default();
    target.insert(Engine::Es, Version(5, 0, 0));

    let mangle = MangleOptions {
        reserved: ["require", "exports", "module"]
            .into_iter()
            .map(Into::into)
            .collect(),
        ..MangleOptions::default()
    };
    let compress = CompressOptions {
        target,
        drop_debugger: false,
        ..CompressOptions::smallest()
    };
    let minified = Minifier::new(MinifierOptions {
        mangle: Some(mangle),
        compress: Some(compress),
    })
    .minify(&allocator, &mut program);

    let out = Codegen::new()
        .with_options(CodegenOptions {
            minify: true,
            comments: CommentOptions {
                normal: false,
                jsdoc: false,
                annotation: false,
                legal: LegalComment::Inline,
            },
            ..CodegenOptions::default()
        })
        .with_scoping(minified.scoping)
        .build(&program);
    fixup_output(out.code, filename)
}

/// Reverse the two undisableable oxc output shapes (module docs): rewrite
/// every untagged, substitution-free template literal into a plain-quoted
/// string, and re-expand `"a.b".split(".")` dependency arrays inside
/// `define(`/`require(` calls. Span-precise: the output is reparsed and only
/// the exact offending spans are spliced, so nothing else moves.
fn fixup_output(code: String, filename: &str) -> Result<String, MinifyError> {
    // Fast path: neither shape present, nothing to reparse.
    if !code.contains('`') && !code.contains(".split(") {
        return Ok(code);
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &code, SourceType::unambiguous()).parse();
    if parsed.panicked || parsed.diagnostics.has_errors() {
        // Internal invariant: our own codegen output must parse.
        return Err(MinifyError {
            file: Some(filename.to_string()),
            message: "minified output failed to reparse (internal)".to_string(),
        });
    }

    struct Collect {
        spans: Vec<(u32, u32, String)>,
    }
    impl<'a> Visit<'a> for Collect {
        fn visit_template_literal(&mut self, it: &TemplateLiteral<'a>) {
            if it.expressions.is_empty() && it.quasis.len() == 1 {
                let quasi = &it.quasis[0];
                // `lone_surrogates` encodes the cooked value with \u{FFFD}
                // escapes — leave those (vanishingly rare) alone. And never
                // promote a would-be directive: converting a leading
                // `` `use strict` `` expression into a string literal WOULD
                // turn it into one, changing semantics.
                if !quasi.lone_surrogates {
                    if let Some(cooked) = &quasi.value.cooked {
                        if !cooked.starts_with("use ") {
                            self.spans.push((it.span.start, it.span.end, {
                                let mut s = String::new();
                                plain_quoted(cooked, &mut s);
                                s
                            }));
                            return;
                        }
                    }
                }
            }
            oxc_ast_visit::walk::walk_template_literal(self, it);
        }
        fn visit_tagged_template_expression(&mut self, it: &TaggedTemplateExpression<'a>) {
            // The quasi belongs to the tag (its identity and raw parts are
            // observable) — visit only the tag and the interpolations.
            self.visit_expression(&it.tag);
            for e in &it.quasi.expressions {
                self.visit_expression(e);
            }
        }
        fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
            if let Expression::Identifier(id) = &it.callee {
                if id.name == "define" || id.name == "require" {
                    for arg in &it.arguments {
                        if let Some((span, rebuilt)) =
                            arg.as_expression().and_then(split_call_to_array)
                        {
                            self.spans.push((span.0, span.1, rebuilt));
                        }
                    }
                }
            }
            oxc_ast_visit::walk::walk_call_expression(self, it);
        }
    }
    let mut collect = Collect { spans: Vec::new() };
    collect.visit_program(&parsed.program);
    if collect.spans.is_empty() {
        return Ok(code);
    }
    // Outermost-first on ties; a span nested inside an already-spliced one
    // (the split-call's own joined string printed as a template) is skipped.
    collect
        .spans
        .sort_unstable_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));

    let mut out = String::with_capacity(code.len());
    let mut pos = 0usize;
    for (start, end, text) in &collect.spans {
        let (s, e) = (*start as usize, *end as usize);
        if s < pos {
            continue; // nested inside a previous replacement
        }
        out.push_str(&code[pos..s]);
        out.push_str(text);
        pos = e;
    }
    out.push_str(&code[pos..]);
    Ok(out)
}

/// Match `"a.b.c".split(".")` (string or no-substitution template on either
/// side) and rebuild the literal array it evaluates to — exactly as the
/// runtime would, so the replacement is value-identical. Returns the span of
/// the whole call plus the rebuilt `["a","b","c"]` text.
fn split_call_to_array(expr: &Expression<'_>) -> Option<((u32, u32), String)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return None;
    };
    if member.property.name != "split" || member.optional || call.optional {
        return None;
    }
    let joined = plain_string_value(&member.object)?;
    if call.arguments.len() != 1 {
        return None;
    }
    let sep = plain_string_value(call.arguments[0].as_expression()?)?;
    if sep.is_empty() {
        return None;
    }
    let mut text = String::with_capacity(joined.len() * 2);
    text.push('[');
    for (i, part) in joined.split(sep).enumerate() {
        if i > 0 {
            text.push(',');
        }
        plain_quoted(part, &mut text);
    }
    text.push(']');
    Some(((call.span.start, call.span.end), text))
}

/// The value of a plain string expression: a `StringLiteral` or an untagged,
/// substitution-free `TemplateLiteral` (how the minify codegen prints
/// strings). `None` for anything else, including lone-surrogate encodings.
fn plain_string_value<'a>(expr: &'a Expression<'_>) -> Option<&'a str> {
    match expr {
        Expression::StringLiteral(s) if !s.lone_surrogates => Some(s.value.as_str()),
        Expression::TemplateLiteral(t)
            if t.expressions.is_empty() && t.quasis.len() == 1 && !t.quasis[0].lone_surrogates =>
        {
            t.quasis[0].value.cooked.as_ref().map(|c| c.as_str())
        }
        _ => None,
    }
}

/// Append `value` as a plain-quoted JS string literal, picking whichever of
/// `"`/`'` needs fewer escapes (ties go to `"` — SVG-in-JS icon packs are
/// double-quote-heavy, so a fixed `"` would grow them). Escapes the quote,
/// `\`, line terminators (incl. U+2028/U+2029 for pre-ES2019 parsers), NUL,
/// and `</` (script-tag safety, as oxc's own string printer does).
fn plain_quoted(value: &str, out: &mut String) {
    let doubles = value.matches('"').count();
    let quote = if value.matches('\'').count() < doubles { '\'' } else { '"' };
    out.push(quote);
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            '\0' => out.push_str("\\u0000"),
            '<' if chars.peek() == Some(&'/') => out.push_str("<\\"),
            _ => out.push(c),
        }
    }
    out.push(quote);
}

/// Insert the `.min` sign into an asset path — `foo.css` → `foo.min.css`,
/// mirroring `Magento\Framework\View\Asset\Minification::addMinifiedSign`
/// minus its config gates (enabled/exclude belong to the deploy, not the
/// naming rule): an already-`.min.*` name is returned unchanged
/// (`isMinifiedFilename`: the five characters ending at the LAST dot are
/// `.min.`), only the final extension is displaced (`jquery.ui.js` →
/// `jquery.ui.min.js`), and a name with no (or an empty) extension passes
/// through — upstream those never reach the sign because no content type
/// enables them.
pub fn min_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let name = s.rsplit('/').next().unwrap_or(&s);
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => return path.to_path_buf(),
    };
    // isMinifiedFilename over the whole string, as upstream does: the last
    // dot is the basename's (it has one), so the window can straddle into
    // the stem but never into a directory.
    let last_dot = s.rfind('.').expect("basename has an extension");
    if last_dot >= 4 && &s[last_dot - 4..=last_dot] == ".min." {
        return path.to_path_buf();
    }
    PathBuf::from(format!("{}min.{ext}", &s[..s.len() - ext.len()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- min_path ----------------------------------------------------------

    #[test]
    fn min_path_inserts_the_sign() {
        for (given, want) in [
            ("foo.css", "foo.min.css"),
            ("foo.js", "foo.min.js"),
            ("jquery.ui.js", "jquery.ui.min.js"),
            ("css/styles-m.css", "css/styles-m.min.css"),
            ("mage/menu.js", "mage/menu.min.js"),
            // `min` in the stem without the dotted sign still gets one.
            ("foomin.js", "foomin.min.js"),
            ("admin.js", "admin.min.js"),
        ] {
            assert_eq!(min_path(Path::new(given)), PathBuf::from(want), "{given}");
        }
    }

    #[test]
    fn min_path_keeps_already_minified_names() {
        for given in [
            "foo.min.js",
            "foo.min.css",
            "requirejs/require.min.js",
            "jquery.ui.min.js",
            ".min.js",
        ] {
            assert_eq!(min_path(Path::new(given)), PathBuf::from(given), "{given}");
        }
    }

    #[test]
    fn min_path_passes_extensionless_names_through() {
        // Upstream never signs these (no content type enables an empty
        // extension); the naming rule alone must not mangle them.
        for given in ["Makefile", "path/to/LICENSE", "trailing.", "v1.2/noext"] {
            assert_eq!(min_path(Path::new(given)), PathBuf::from(given), "{given}");
        }
    }

    // -- minify_css --------------------------------------------------------

    #[test]
    fn css_minifies_without_restructuring() {
        let css = "/* dropped */\n.a { color: #ff0000; margin: 0 auto; }\n\
                   .a { color: blue; }\n@media (min-width: 768px) { .b { top: .5em; } }\n";
        let out = minify_css(css).expect("minify");
        assert!(out.warnings.is_empty(), "clean parse: {:?}", out.warnings);
        // Whitespace and comments gone…
        assert!(!out.css.contains("dropped"));
        assert!(!out.css.contains('\n') || out.css.trim_end().lines().count() == 1);
        // …but the duplicate .a rules are NOT merged (serialization only —
        // the optimizer would collapse them and trip the semantic differ).
        assert_eq!(out.css.matches(".a{").count(), 2, "no rule merging: {}", out.css);
        assert!(out.css.contains("@media"), "{}", out.css);
    }

    #[test]
    fn css_error_recovery_surfaces_warnings() {
        // The classic IE star hack: an invalid declaration. Locks the probed
        // recovery behavior — the sheet comes through (Ok), the surrounding
        // rules survive, but the affected rule's WHOLE declaration block is
        // emptied, and a warning reports it: lossy recovery must never be
        // silent. (The blank/luma corpus carries no such hacks — the gate
        // asserts zero warnings there.)
        let css = ".ok { color: red; }\n.hacked { color: red; *zoom: 1; }\n.also-ok { top: 0; }\n";
        let out = minify_css(css).expect("recovers");
        assert!(!out.warnings.is_empty(), "expected a recovery warning");
        assert!(out.css.contains(".ok{color:red}"), "{}", out.css);
        assert!(out.css.contains(".also-ok{top:0}"), "{}", out.css);
        assert!(out.css.contains(".hacked{}"), "lossy-recovery shape changed: {}", out.css);
    }

    #[test]
    fn css_keeps_leading_license_comments() {
        let css = "/*! Copyright, license terms */\n.a { color: red; }\n";
        let out = minify_css(css).expect("minify");
        assert!(out.css.contains("license terms"), "{}", out.css);
    }

    #[test]
    fn css_ie_legacy_values_survive() {
        // The IE-legacy values the compiled blank/luma corpus DOES carry:
        // `filter: alpha(...)` and an underscore hack. These must parse and
        // survive minification (no warnings, no drops).
        let css = ".o { filter: alpha(opacity=50); }\n.u { _height: 1px; height: 2px; }\n";
        let out = minify_css(css).expect("minify");
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        assert!(out.css.contains("alpha(opacity=50)"), "{}", out.css);
        assert!(out.css.contains("_height:1px"), "{}", out.css);
        assert!(out.css.contains("height:2px"), "{}", out.css);
    }

    // -- minify_js ---------------------------------------------------------

    /// A representative AMD module: deps array + factory.
    #[test]
    fn js_amd_module_survives() {
        let src = r#"
define([
    'jquery',
    'Magento_Customer/js/customer-data',
    'mage/translate'
], function ($, customerData, $t) {
    'use strict';
    var privateHelper = function (value) {
        return value + 1;
    };
    return function (config) {
        return privateHelper(config.start) + $t('done');
    };
});
"#;
        let out = minify_js(src, "amd.js").expect("minify");
        // The call structure and every dependency string literal survive.
        assert!(out.starts_with("define("), "{out}");
        for dep in ["jquery", "Magento_Customer/js/customer-data", "mage/translate"] {
            assert!(
                out.contains(&format!("\"{dep}\"")) || out.contains(&format!("'{dep}'")),
                "dep `{dep}` must survive verbatim: {out}"
            );
        }
        // The output reparses cleanly.
        reparse(&out).expect("output reparses");
        assert!(out.len() < src.len());
    }

    /// A jQuery-widget-style file (the other dominant Magento shape).
    #[test]
    fn js_jquery_widget_survives() {
        let src = r#"
define(['jquery', 'jquery-ui-modules/widget'], function ($) {
    'use strict';
    $.widget('mage.example', {
        options: { delay: 300, template: '<div class="x"></div>' },
        _create: function () {
            var self = this;
            this.element.on('click', function (event) {
                event.preventDefault();
                self._toggle();
            });
        },
        _toggle: function () {
            this.element.toggleClass(this.options.openClass);
        }
    });
    return $.mage.example;
});
"#;
        let out = minify_js(src, "widget.js").expect("minify");
        assert!(out.contains("\"mage.example\"") || out.contains("'mage.example'"), "{out}");
        assert!(out.contains("_create"), "widget method names are properties: {out}");
        reparse(&out).expect("output reparses");
    }

    /// `/*!` license banners are retained (JShrink parity — the decision:
    /// keep legal comments inline, drop everything else).
    #[test]
    fn js_license_comment_is_kept() {
        let src = "/*!\n * jQuery JavaScript Library v3.7.1\n * (c) OpenJS Foundation\n */\n(function () { var x = 1; return x; })();\n";
        let out = minify_js(src, "lib.js").expect("minify");
        assert!(out.contains("jQuery JavaScript Library"), "{out}");
        // An ordinary comment is not.
        let src2 = "/* internal note */\nvar a = 1;\n";
        let out2 = minify_js(src2, "note.js").expect("minify");
        assert!(!out2.contains("internal note"), "{out2}");
    }

    /// `'use strict'` directives must survive (semantics: strict mode).
    #[test]
    fn js_use_strict_directive_survives() {
        let src = "'use strict';\nvar greeting = function (name) { return 'hi ' + name; };\ngreeting('x');\n";
        let out = minify_js(src, "strict.js").expect("minify");
        assert!(
            out.starts_with("\"use strict\"") || out.starts_with("'use strict'"),
            "directive must stay first: {out}"
        );
        reparse(&out).expect("output reparses");
    }

    /// A dependency array long enough to trigger oxc's unconditional
    /// `"a.b".split(".")` substitution must come back out as the literal
    /// array (the fixup pass re-expands it inside define/require).
    #[test]
    fn js_long_dep_array_survives_the_split_substitution() {
        let deps: Vec<String> = (0..30).map(|i| format!("'mage/dep-{i}'")).collect();
        let src = format!(
            "define([{}], function () {{ return 1; }});\n\
             var other = ['x0','x1','x2','x3','x4','x5','x6','x7','x8','x9',\
             'y0','y1','y2','y3','y4','y5','y6','y7','y8','y9',\
             'z0','z1','z2','z3','z4','z5','z6','z7','z8','z9'];\n\
             window.other = other;\n",
            deps.join(", ")
        );
        let out = minify_js(&src, "long.js").expect("minify");
        for i in 0..30 {
            let dep = format!("mage/dep-{i}");
            assert!(
                out.contains(&format!("\"{dep}\"")) || out.contains(&format!("'{dep}'")),
                "dep `{dep}` must survive verbatim: {out}"
            );
        }
        assert!(!out.contains("].split") && !out.contains(")).split"), "{out}");
        // Outside define/require the substitution is kept (pure size win).
        assert!(out.contains(".split("), "non-AMD array should stay substituted: {out}");
        reparse(&out).expect("output reparses");
    }

    /// The ES5 floor bites: patterns oxc would otherwise rewrite with newer
    /// syntax stay ES5. (Verified against the default target, which emits
    /// `??` here — this test fails if the floor is dropped.)
    #[test]
    fn js_output_stays_es5() {
        let src = "define([], function () {\n\
            return function (a, b) {\n\
                var v = a !== null && a !== void 0 ? a : b;\n\
                try { v(); } catch (unusedErr) { v = b; }\n\
                return v;\n\
            };\n\
        });\n";
        let out = minify_js(src, "es5.js").expect("minify");
        assert!(!out.contains("??"), "nullish coalescing injected: {out}");
        assert!(!out.contains("=>"), "arrow injected: {out}");
        assert!(!out.contains("catch{"), "optional catch binding injected: {out}");
        assert!(!out.contains('`'), "template literal injected: {out}");
        reparse(&out).expect("output reparses");
    }

    /// `debugger;` is semantics we keep (JShrink parity).
    #[test]
    fn js_debugger_is_kept() {
        let out = minify_js("function f() { debugger; return 1; } f();", "d.js").expect("minify");
        assert!(out.contains("debugger"), "{out}");
    }

    /// A broken file is a per-file error carrying the filename — no panic,
    /// no output.
    #[test]
    fn js_parse_error_is_attributed() {
        let err = minify_js("define(['a'], function ( { return; });", "broken/file.js")
            .expect_err("must fail");
        assert_eq!(err.file.as_deref(), Some("broken/file.js"));
        assert!(err.message.contains("parse error"), "{}", err.message);
        assert!(err.to_string().starts_with("broken/file.js: "), "{err}");
    }

    /// RequireJS CJS sugar: the `require` parameter must keep its name (the
    /// loader regex-scans `factory.toString()` for literal `require("…")`).
    #[test]
    fn js_cjs_sugar_require_param_not_mangled() {
        let src = "define(function (require, exports, module) {\n\
            'use strict';\n\
            var mainDependency = require('mage/apply/main');\n\
            module.exports = function () { return mainDependency; };\n\
        });\n";
        let out = minify_js(src, "cjs.js").expect("minify");
        assert!(
            out.contains("require(\"mage/apply/main\")") || out.contains("require('mage/apply/main')"),
            "literal require(...) call must survive: {out}"
        );
        reparse(&out).expect("output reparses");
    }

    /// Parse `code` as the corpus is parsed; Err = first parser diagnostic.
    pub(super) fn reparse(code: &str) -> Result<(), String> {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, code, SourceType::unambiguous()).parse();
        if ret.panicked || ret.diagnostics.has_errors() {
            return Err(ret
                .diagnostics
                .errors()
                .next()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "parser panicked".to_string()));
        }
        Ok(())
    }
}

/// The environment-gated semantic gates: need the reference checkout
/// (`/home/jelle/mg-install-310`, overridable with `MAGECOMMAND_CORPUS`) and
/// SKIP — not fail — without it, like the `tests/` byte gates.
#[cfg(test)]
mod gates {
    use std::path::PathBuf;

    use super::super::{bundle, cssdiff, less};
    use super::*;

    fn reference_root() -> Option<PathBuf> {
        let root = PathBuf::from(
            std::env::var("MAGECOMMAND_CORPUS")
                .unwrap_or_else(|_| "/home/jelle/mg-install-310".to_owned()),
        );
        root.is_dir().then_some(root)
    }

    /// CSS Gate: compile styles-m for blank + luma with the real LESS
    /// machinery, minify it, and demand: (1) zero recovery warnings on the
    /// corpus (nothing was dropped), (2) the minified output reparses
    /// STRICTLY and re-minifies to the identical bytes (fixpoint), and
    /// (3) the semantic differ, run over the expanded re-prints of both
    /// parses (both sides in lightningcss's canonical spelling), reports
    /// **zero structural findings** and only value respellings from the
    /// pinned [`allowed_minify_substitution`] list. The raw input↔minified
    /// diff (dominated by lightningcss's undisableable value
    /// canonicalization — see the module docs) is reported as data, not
    /// gated.
    #[test]
    fn css_minify_round_trips_exactly_on_blank_and_luma() {
        let Some(root) = reference_root() else {
            println!("SKIP: reference install not present");
            return;
        };
        let magento = magequery_core::Magento::open(&root).expect("open reference root");
        for theme in ["Magento/blank", "Magento/luma"] {
            let orch = less::LessOrchestrator::from_magento(&magento, "frontend", theme)
                .expect("theme chain");
            let compiled = orch
                .compile_entry("styles-m", &less::LessDeployOptions::default())
                .expect("compile styles-m");
            let min = minify_css(&compiled.css).expect("minify_css");
            assert!(
                min.warnings.is_empty(),
                "{theme}: error_recovery dropped something from OUR corpus: {:?}",
                min.warnings
            );

            // (2) the minified output reparses STRICTLY (no error_recovery)
            // and is a byte-level FIXPOINT: minifying it again changes
            // nothing (any print-step loss or instability would break this).
            let min2 = minify_css(&min.css).expect("re-minify");
            assert!(min2.warnings.is_empty(), "{theme}: {:?}", min2.warnings);
            lightningcss::stylesheet::StyleSheet::parse(
                &min.css,
                lightningcss::stylesheet::ParserOptions::default(),
            )
            .expect("minified output parses strictly");
            assert_eq!(min2.css, min.css, "{theme}: minify is not idempotent");

            // (3) the differ over the expanded re-prints of both parses:
            // selectors and values are then in lightningcss's canonical
            // spelling on BOTH sides, so every finding is a real difference
            // introduced by the minified PRINT. Zero structural findings
            // allowed (no rule/declaration may appear, vanish or move);
            // value findings must sit in the pinned allowlist of known
            // spec-equal substitutions the minify printer applies.
            let expanded_in = expanded_print(&compiled.css, "input");
            let expanded_min = expanded_print(&min.css, "minified");
            let d = cssdiff::diff(&expanded_in, &expanded_min);
            let mut structural = 0usize;
            let mut off_list = 0usize;
            for f in &d.findings {
                match f {
                    cssdiff::Finding::DeclChanged { prop, expected, actual, .. }
                        if allowed_minify_substitution(prop, expected, actual) => {}
                    cssdiff::Finding::DeclChanged { .. } => {
                        off_list += 1;
                        println!("{theme}: off-allowlist: {f}");
                    }
                    _ => {
                        structural += 1;
                        println!("{theme}: STRUCTURAL: {f}");
                    }
                }
            }
            assert_eq!(structural, 0, "{theme}: structural finding(s) — content lost or moved");
            assert_eq!(off_list, 0, "{theme}: value substitution outside the pinned allowlist");

            // Reported, not gated: the raw delta (value canonicalization).
            let raw = cssdiff::diff(&compiled.css, &min.css);
            println!(
                "{theme}: styles-m {} bytes -> {} bytes · {} rules · idempotent · {} allowlisted substitution(s) · raw canonicalization delta: {} finding(s)",
                compiled.css.len(),
                min.css.len(),
                d.rules_actual,
                d.findings.len(),
                raw.findings.len()
            );
        }
    }

    /// Expanded (non-minified) print of a strict parse — the canonical form
    /// both gate sides are compared in.
    fn expanded_print(css: &str, what: &str) -> String {
        lightningcss::stylesheet::StyleSheet::parse(
            css,
            lightningcss::stylesheet::ParserOptions::default(),
        )
        .unwrap_or_else(|e| panic!("{what} does not parse strictly: {e}"))
        .to_css(lightningcss::printer::PrinterOptions::default())
        .expect("expanded print")
        .code
    }

    /// The pinned allowlist of value respellings lightningcss's MINIFY
    /// printer applies (spec-equal transformations; verified on the
    /// corpus). Anything new appearing after a version bump fails the gate
    /// until reviewed and pinned here.
    fn allowed_minify_substitution(prop: &str, expected: &str, actual: &str) -> bool {
        // CSS 2.1 §15.6: font-weight normal == 400, bold == 700.
        if matches!(
            (prop, expected, actual),
            ("font-weight", "normal", "400") | ("font-weight", "bold", "700")
        ) {
            return true;
        }
        // Transforms producing the equal matrix collapse to the shortest
        // form (css-transforms-1/-2: translate3d(x, y, 0) == translate(x,
        // y); translate(x, 0) == translate(x) == translateX(x);
        // translate(0, y) == translateY(y)). NOTE the known real-world
        // caveat: translate3d is the classic force-GPU-layer hint, which
        // the 2D form may not trigger — spec-equal, engine heuristics
        // differ. Accepted (documented) for .min.css.
        if prop == "transform" || prop.ends_with("-transform") {
            return matches!(
                (expected, actual),
                ("translate3d(0, 0, 0)", "translate(0)")
                    | ("translate(0, 0)", "translate(0)")
                    | ("translateX(0)", "translate(0)")
                    | ("translateX(100%)", "translate(100%)")
                    | ("translateX(-100%)", "translate(-100%)")
                    | ("translate(0, -50%)", "translateY(-50%)")
                    | ("translate(0, 50%)", "translateY(50%)")
            );
        }
        // The legacy `-webkit-gradient()` position keywords print as
        // coordinates (`left top` == `0 0`, `right bottom` == `100% 100%`
        // per the old webkit syntax); everything else must be identical.
        if prop == "background" || prop.ends_with("-image") {
            if expected.starts_with("-webkit-gradient(")
                && expected
                    .replacen("linear, left top, right bottom,", "linear, 0 0, 100% 100%,", 1)
                    == actual
            {
                return true;
            }
        }
        // calc() term reordering: `calc(-A + B)` == `calc(B - A)`.
        if let (Some(e), Some(a)) = (
            expected.strip_prefix("calc(-").and_then(|s| s.strip_suffix(")")),
            actual.strip_prefix("calc(").and_then(|s| s.strip_suffix(")")),
        ) {
            if let (Some((ea, eb)), Some((ab, aa))) = (e.split_once(" + "), a.split_once(" - ")) {
                return ea == aa && eb == ab;
            }
        }
        false
    }

    /// JS Gate: minify EVERY `.js` of the deployed luma package view; each
    /// must minify, reparse, and keep its `define(`/`require(` dependency
    /// array string literals verbatim.
    #[test]
    fn js_minify_sweeps_the_deployed_luma_view() {
        let Some(root) = reference_root() else {
            println!("SKIP: reference install not present");
            return;
        };
        let magento = magequery_core::Magento::open(&root).expect("open reference root");
        let themes = magento.themes();
        let chain = less::theme_chain("frontend", "Magento/luma", &themes).expect("chain");
        let modules: Vec<less::ModuleRef> = magento
            .modules()
            .iter()
            .filter(|m| m.enabled)
            .map(|m| less::ModuleRef {
                name: m.name.to_string(),
                dir: m.path.clone(),
            })
            .collect();
        let tree = bundle::deployed_tree(&root, "frontend", &chain, &modules, "en_US");
        let js: Vec<(&String, &PathBuf)> =
            tree.iter().filter(|(p, _)| p.ends_with(".js")).collect();
        assert!(js.len() > 500, "expected a real corpus, got {} files", js.len());

        let mut in_total = 0usize;
        let mut out_total = 0usize;
        let mut deps_checked = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for (rel, path) in &js {
            let src = std::fs::read_to_string(path).expect("read source");
            let out = match minify_js(&src, rel) {
                Ok(o) => o,
                Err(e) => {
                    failures.push(format!("minify: {e}"));
                    continue;
                }
            };
            if let Err(e) = super::tests::reparse(&out) {
                failures.push(format!("reparse: {rel}: {e}"));
                continue;
            }
            for dep in dep_array_literals(&src) {
                deps_checked += 1;
                if !out.contains(&format!("\"{dep}\"")) && !out.contains(&format!("'{dep}'")) {
                    failures.push(format!("dep lost: {rel}: `{dep}`"));
                }
            }
            in_total += src.len();
            out_total += out.len();
        }
        for f in failures.iter().take(25) {
            println!("FAIL {f}");
        }
        println!(
            "swept {} js file(s): {} dep-array literal(s) preserved, {} -> {} bytes ({:.1}%)",
            js.len(),
            deps_checked,
            in_total,
            out_total,
            100.0 * out_total as f64 / in_total as f64
        );

        // Calibration (not a gate): our minified size vs the shipped
        // pre-minified `.min.js` siblings, for the first few pairs.
        let mut calibrated = 0;
        for (rel, path) in &js {
            if calibrated == 5 {
                break;
            }
            let Some(stem) = rel.strip_suffix(".js") else { continue };
            if stem.ends_with(".min") {
                continue;
            }
            let Some(min_sibling) = tree.get(&format!("{stem}.min.js")) else {
                continue;
            };
            let src = std::fs::read_to_string(path).expect("read source");
            let Ok(ours) = minify_js(&src, rel) else { continue };
            let shipped = std::fs::metadata(min_sibling).map(|m| m.len()).unwrap_or(0);
            println!(
                "calibration: {rel}: source {} B, ours {} B, shipped .min {} B",
                src.len(),
                ours.len(),
                shipped
            );
            calibrated += 1;
        }

        assert!(failures.is_empty(), "{} sweep failure(s)", failures.len());
    }

    /// The `define(`/`require(` dependency-array string literals of `src`:
    /// every `'…'`/`"…"` literal inside the bracketed first-or-second
    /// argument. A light scan, not a parse — good enough to assert survival.
    fn dep_array_literals(src: &str) -> Vec<String> {
        let mut deps = Vec::new();
        let bytes = src.as_bytes();
        for callee in ["define(", "require("] {
            let mut from = 0;
            while let Some(at) = src[from..].find(callee) {
                let call = from + at + callee.len();
                from = call;
                // Word boundary on the left (`urlBuilder.require(` etc. is
                // fine too — it just won't have an array — but skip obvious
                // member calls to keep the scan honest).
                let start = from - callee.len();
                if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_' || bytes[start - 1] == b'.' || bytes[start - 1] == b'$') {
                    continue;
                }
                let mut i = call;
                // Optional module-name string + comma before the array.
                i = skip_ws(src, i);
                if i < src.len() && (bytes[i] == b'\'' || bytes[i] == b'"') {
                    let Some(end) = scan_string(src, i) else { continue };
                    i = skip_ws(src, end);
                    if i < src.len() && bytes[i] == b',' {
                        i = skip_ws(src, i + 1);
                    }
                }
                if i >= src.len() || bytes[i] != b'[' {
                    continue;
                }
                i += 1;
                while i < src.len() && bytes[i] != b']' {
                    match bytes[i] {
                        b'\'' | b'"' => {
                            let Some(end) = scan_string(src, i) else { break };
                            let lit = &src[i + 1..end - 1];
                            // Escapes are not un-escaped by this scan; skip
                            // the rare literal that has any.
                            if !lit.contains('\\') {
                                deps.push(lit.to_string());
                            }
                            i = end;
                        }
                        _ => i += 1,
                    }
                }
            }
        }
        deps
    }

    fn skip_ws(src: &str, mut i: usize) -> usize {
        let bytes = src.as_bytes();
        while i < src.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        i
    }

    /// `i` at the opening quote; returns the index just past the closing one.
    fn scan_string(src: &str, i: usize) -> Option<usize> {
        let bytes = src.as_bytes();
        let quote = bytes[i];
        let mut j = i + 1;
        while j < src.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b if b == quote => return Some(j + 1),
                _ => j += 1,
            }
        }
        None
    }
}
