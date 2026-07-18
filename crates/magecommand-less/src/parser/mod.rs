//! Recursive-descent LESS parser (plan §9.1).
//!
//! Hand-written, like less.js's own bespoke parser, driving the [`crate::lex`]
//! [`Cursor`]. This step (STEP 3) produces the **complete parse-side AST** for
//! the core subset: stylesheets; rulesets + nested rulesets; selectors incl.
//! `&`; declarations (`!important`, merge `+`/`+_`, custom properties); at-rules
//! (`@media`, `@import`, `@font-face`, `@page`, …); comments; variable
//! declarations `@x: …;` and references `@x`/`@{x}`; mixin definitions/calls with
//! params; and value expressions (numbers+units, colors, strings, operations,
//! function calls). Evaluation is the next step.
//!
//! The sibling `selector`/`expr`/`entities`/`atrule` modules hold the plan's
//! documented ownership boundaries; the implementation is co-located here for the
//! shared [`Parser`] cursor state.

pub mod atrule;
pub mod entities;
pub mod expr;
pub mod selector;

use std::sync::Arc;

use crate::ast::{
    AtRule, AtRuleBlock, Declaration, Element, MergeKind, MixinArg, MixinCall, MixinDefinition,
    MixinParam, Node, Ruleset, Selector, Span,
};
use crate::error::{ErrorKind, LessError};
use crate::lex::{normalize_source, Cursor, LineMap};
use crate::options::LessOptions;
use crate::resolver::FileInfo;

/// Parse LESS `source` into an immutable AST (plan §9.5). Source encoding is
/// normalized first (BOM strip + CRLF→LF, §H2).
pub fn parse(source: &str, file: FileInfo, opts: &LessOptions) -> Result<Arc<Node>, LessError> {
    let normalized = normalize_source(source);
    let mut parser = Parser::new(normalized.as_ref(), file, opts);
    let rules = parser.parse_primary(true)?;
    parser.expect_eof()?;
    Ok(Arc::new(Node::Root(rules)))
}

/// Parse a bare value fragment (a declaration RHS: comma-separated expressions)
/// from a string — used by the evaluator to (re-)parse mixin arguments, defaults,
/// and guard operands that the structural parser kept as raw text.
pub fn parse_value_fragment(src: &str, opts: &LessOptions) -> Result<Node, LessError> {
    let normalized = normalize_source(src);
    let mut parser = Parser::new(normalized.as_ref(), FileInfo::default(), opts);
    parser.cur.skip_trivia();
    let value = parser.parse_value()?;
    Ok(value)
}

/// The recursive-descent parser state.
struct Parser<'a> {
    cur: Cursor<'a>,
    file: FileInfo,
    magento_mode: bool,
    line_map: LineMap,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, file: FileInfo, opts: &LessOptions) -> Self {
        Parser {
            cur: Cursor::new(src),
            file,
            magento_mode: opts.magento_mode,
            line_map: LineMap::new(src),
        }
    }

    fn here(&self) -> usize {
        self.cur.i
    }

    fn span(&self, start: usize) -> Span {
        Span::new(start, self.here())
    }

    /// Build a located parse error at the current position.
    fn err(&self, msg: impl Into<String>) -> LessError {
        let (line, column) = self.line_map.line_col(self.cur.i);
        LessError {
            kind: ErrorKind::Parse,
            message: msg.into(),
            filename: if self.file.filename.is_empty() {
                None
            } else {
                Some(self.file.filename.clone())
            },
            line: Some(line),
            column: Some(column),
            excerpt: Vec::new(),
        }
    }

    fn expect_eof(&mut self) -> Result<(), LessError> {
        self.cur.skip_trivia();
        if self.cur.eof() {
            Ok(())
        } else {
            Err(self.err(format!(
                "unexpected '{}'",
                self.cur.rest().chars().next().unwrap_or(' ')
            )))
        }
    }

    // -----------------------------------------------------------------------
    // Primary — the rule list at root or inside a `{ … }` block
    // -----------------------------------------------------------------------

    /// Parse a rule list. `root` selects top-level vs a braced block (which stops
    /// at `}`).
    fn parse_primary(&mut self, root: bool) -> Result<Vec<Node>, LessError> {
        let mut rules = Vec::new();
        loop {
            // Whitespace + line comments (dropped) + block comments (kept).
            loop {
                self.cur.skip_whitespace();
                if self.cur.at_line_comment() {
                    let start = self.here();
                    let (text, _) = self.cur.scan_comment();
                    // In magento_mode the `//@magento_import 'source/_x.less';`
                    // line comment is a directive, not a stripped comment (§7.1).
                    if self.magento_mode {
                        if let Some(node) = parse_magento_import(text, self.span(start)) {
                            rules.push(node);
                        }
                    }
                } else if self.cur.at_block_comment() {
                    let start = self.here();
                    let (text, _) = self.cur.scan_comment();
                    rules.push(Node::Comment {
                        text: text.to_string(),
                        line: false,
                        span: self.span(start),
                    });
                } else {
                    break;
                }
            }
            if self.cur.eof() {
                break;
            }
            if !root && self.cur.cur() == Some(b'}') {
                break;
            }
            // Stray semicolons between rules.
            if self.cur.eat(b';') {
                continue;
            }
            let node = self.parse_statement()?;
            rules.push(node);
        }
        Ok(rules)
    }

    /// Parse a braced `{ … }` body, consuming the closing brace.
    fn parse_block(&mut self) -> Result<Vec<Node>, LessError> {
        if !self.cur.eat(b'{') {
            return Err(self.err("expected '{'"));
        }
        let rules = self.parse_primary(false)?;
        self.cur.skip_trivia();
        if !self.cur.eat(b'}') {
            return Err(self.err("expected '}'"));
        }
        Ok(rules)
    }

    // -----------------------------------------------------------------------
    // Statement dispatch
    // -----------------------------------------------------------------------

    fn parse_statement(&mut self) -> Result<Node, LessError> {
        let c = self.cur.cur().unwrap_or(b' ');

        // `@` — variable declaration, detached-ruleset assignment, or at-rule.
        if c == b'@' && self.cur.peek(1) == Some(b'{') {
            // `@{x}: …` interpolated property declaration — treat as declaration.
        } else if c == b'@' {
            if let Some(node) = self.try_variable_decl()? {
                return Ok(node);
            }
            return self.parse_at_rule();
        }

        // Declaration — but never when the line clearly opens a selector
        // (`.`/`#`/`&`/`:`), mirroring less.js `declaration()`.
        if !matches!(c, b'.' | b'#' | b'&' | b':') {
            if let Some(node) = self.try_declaration()? {
                return Ok(node);
            }
        }

        // A bare function-call statement (`e('…');` at any level — less.js
        // `primary` tries `entities.call()` after `ruleset`); only when the call
        // is immediately terminated, so `input:not(.x) { }` stays a selector.
        if let Some(node) = self.try_call_statement()? {
            return Ok(node);
        }

        // Otherwise a ruleset, mixin definition, or mixin call.
        self.parse_selector_statement()
    }

    /// Try `ident(args);` as a statement-level function call (css-escapes'
    /// root-level `e('…');`). Backtracks unless the balanced call is followed by
    /// `;`/`}`/EOF.
    fn try_call_statement(&mut self) -> Result<Option<Node>, LessError> {
        let save = self.here();
        let c = self.cur.cur().unwrap_or(b' ');
        if !(c.is_ascii_alphabetic() || c == b'_' || c == b'%' || c == b'~') {
            return Ok(None);
        }
        // Cheap shape probe first: ident + balanced parens + terminator.
        let ident = self.cur.scan_ident().to_string();
        if ident.is_empty() || self.cur.cur() != Some(b'(') {
            self.cur.i = save;
            return Ok(None);
        }
        self.skip_balanced(b'(', b')');
        self.cur.skip_whitespace();
        // `each(...)` may be followed directly by the next statement (less.js
        // parses any primary-level call; we only need the rule-producing one).
        let terminated = matches!(self.cur.cur(), None | Some(b';') | Some(b'}'))
            || ident.eq_ignore_ascii_case("each");
        self.cur.i = save;
        if !terminated {
            return Ok(None);
        }
        let node = self.parse_entity()?;
        self.cur.skip_trivia();
        self.cur.eat(b';');
        Ok(Some(node))
    }

    // -----------------------------------------------------------------------
    // Variable declarations & at-rules
    // -----------------------------------------------------------------------

    /// `@name: value;` or `@name: { … };` (detached ruleset). Backtracks and
    /// returns `None` if the value doesn't terminate as a declaration (so `@page
    /// :left { … }` falls through to the at-rule parser).
    fn try_variable_decl(&mut self) -> Result<Option<Node>, LessError> {
        let save = self.here();
        let start = self.here();
        debug_assert_eq!(self.cur.cur(), Some(b'@'));
        self.cur.bump(); // @
        let name = self.cur.scan_ident();
        if name.is_empty() {
            self.cur.i = save;
            return Ok(None);
        }
        let name = name.to_string();
        self.cur.skip_whitespace();
        if !self.cur.eat(b':') {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.skip_trivia();
        // Detached ruleset value.
        if self.cur.cur() == Some(b'{') {
            let rules = self.parse_block()?;
            self.cur.skip_trivia();
            self.cur.eat(b';');
            return Ok(Some(Node::VariableDecl {
                name,
                value: Box::new(Node::DetachedRuleset {
                    rules,
                    span: self.span(start),
                }),
                important: String::new(),
                span: self.span(start),
            }));
        }
        let value = self.parse_value()?;
        self.cur.skip_whitespace();
        let important = self.parse_important();
        self.cur.skip_trivia();
        if self.cur.cur() == Some(b';') || self.cur.cur() == Some(b'}') || self.cur.eof() {
            self.cur.eat(b';');
            Ok(Some(Node::VariableDecl {
                name,
                value: Box::new(value),
                important,
                span: self.span(start),
            }))
        } else {
            // Not a clean declaration — back off to at-rule handling.
            self.cur.i = save;
            Ok(None)
        }
    }

    fn parse_at_rule(&mut self) -> Result<Node, LessError> {
        let start = self.here();
        debug_assert_eq!(self.cur.cur(), Some(b'@'));
        self.cur.bump(); // @
        let name = format!("@{}", self.cur.scan_ident());

        if name == "@import" {
            return self.parse_import(start);
        }

        // Prelude: raw source up to `{` or `;`, structurally re-parsed as a value
        // when non-empty (media queries, `@charset "UTF-8"`, `@namespace svg "…"`).
        self.cur.skip_trivia();
        let prelude_start = self.here();
        self.scan_prelude();
        let prelude_all = self.cur.src()[prelude_start..self.here()].trim();
        // Block comments in the prelude relocate into the block (less.js's
        // commentStore attaches them to the body's rules — `@-webkit-keyframes
        // hover /* c */{}` renders the comment inside the braces).
        let (prelude_raw, prelude_comments) = split_prelude_comments(prelude_all);
        let prelude_raw = prelude_raw.trim();
        let prelude = if prelude_raw.is_empty() {
            None
        } else {
            Some(Box::new(Node::Anonymous(prelude_raw.to_string())))
        };

        self.cur.skip_trivia();
        let block = if self.cur.cur() == Some(b'{') {
            let mut rules = self.parse_block()?;
            for (i, text) in prelude_comments.into_iter().enumerate() {
                rules.insert(
                    i,
                    Node::Comment {
                        text,
                        line: false,
                        span: self.span(start),
                    },
                );
            }
            AtRuleBlock::Rules(rules)
        } else {
            self.cur.eat(b';');
            AtRuleBlock::None
        };

        Ok(Node::AtRule(AtRule {
            name,
            prelude,
            block,
            span: self.span(start),
        }))
    }

    /// Advance over an at-rule prelude: everything up to a top-level `{` or `;`,
    /// respecting nested `()`/`[]`, strings, and comments.
    fn scan_prelude(&mut self) {
        let mut depth = 0i32;
        while let Some(b) = self.cur.cur() {
            match b {
                b'{' | b';' if depth == 0 => break,
                b'}' if depth == 0 => break,
                b'(' | b'[' => {
                    depth += 1;
                    self.cur.bump();
                }
                b')' | b']' => {
                    depth -= 1;
                    self.cur.bump();
                }
                b'"' | b'\'' => {
                    self.cur.scan_string();
                }
                // A `//` inside parens (e.g. `url(http://…)`) is part of the
                // value, not a line comment — only scan line comments at depth 0.
                b'/' if self.cur.at_block_comment()
                    || (depth == 0 && self.cur.at_line_comment()) =>
                {
                    self.cur.scan_comment();
                }
                _ => self.cur.bump(),
            }
        }
    }

    fn parse_import(&mut self, start: usize) -> Result<Node, LessError> {
        self.cur.skip_trivia();
        // Options: `@import (reference, once) "…";`
        let mut options = Vec::new();
        if self.cur.cur() == Some(b'(') {
            self.cur.bump();
            let opt_start = self.here();
            while let Some(b) = self.cur.cur() {
                if b == b')' {
                    break;
                }
                self.cur.bump();
            }
            let raw = self.cur.src()[opt_start..self.here()].to_string();
            self.cur.eat(b')');
            options = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            self.cur.skip_trivia();
        }
        // Path: a url() or a quoted string.
        let path = if self.cur.rest().starts_with("url(") || self.cur.rest().starts_with("url (") {
            self.parse_url()?
        } else if matches!(self.cur.cur(), Some(b'"') | Some(b'\'')) {
            self.parse_quoted()
        } else {
            // Interpolated / bare path — capture up to a media list, `;` or EOF.
            let ps = self.here();
            while let Some(b) = self.cur.cur() {
                if matches!(b, b';' | b'{') || b.is_ascii_whitespace() {
                    break;
                }
                self.cur.bump();
            }
            Node::Anonymous(self.cur.src()[ps..self.here()].to_string())
        };
        // Optional media features up to `;`.
        self.cur.skip_whitespace();
        let fs = self.here();
        self.scan_prelude();
        let feat_raw = self.cur.src()[fs..self.here()].trim();
        let features = if feat_raw.is_empty() {
            None
        } else {
            Some(Box::new(Node::Anonymous(feat_raw.to_string())))
        };
        self.cur.eat(b';');
        Ok(Node::Import {
            path: Box::new(path),
            options,
            features,
            span: self.span(start),
        })
    }

    // -----------------------------------------------------------------------
    // Declarations
    // -----------------------------------------------------------------------

    /// Try to parse a property declaration; backtrack and return `None` if the
    /// text isn't a declaration (so it can be reparsed as a ruleset/mixin).
    fn try_declaration(&mut self) -> Result<Option<Node>, LessError> {
        let save = self.here();
        let start = self.here();

        let Some((name, merge, custom)) = self.parse_property_name() else {
            self.cur.i = save;
            return Ok(None);
        };
        self.cur.skip_whitespace();
        if !self.cur.eat(b':') {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.skip_trivia();

        let value = if custom {
            self.parse_custom_property_value()
        } else if let Some(v) = self.try_anonymous_value() {
            v
        } else {
            self.parse_value()?
        };

        self.cur.skip_whitespace();
        let important = self.parse_important();
        self.cur.skip_trivia();

        match self.cur.cur() {
            Some(b';') => {
                self.cur.bump();
            }
            Some(b'}') | None => {}
            _ => {
                // Not a terminating declaration — back off.
                self.cur.i = save;
                return Ok(None);
            }
        }

        Ok(Some(Node::Declaration(Declaration {
            name,
            value: Box::new(value),
            important,
            merge,
            custom,
            span: self.span(start),
        })))
    }

    /// less.js `anonymousValue` — the declaration fast path: a value containing
    /// none of `.#@$+/'"*`(;{}-` up to a `;` is captured VERBATIM (whitespace,
    /// newlines and a trailing `!important` included), which is what preserves
    /// source formatting in simple values (`background: the,\n great,\n wall`).
    /// The `;` is left for the caller's terminator handling.
    fn try_anonymous_value(&mut self) -> Option<Node> {
        let start = self.here();
        loop {
            match self.cur.cur() {
                Some(b';') => break,
                Some(
                    b'.' | b'#' | b'@' | b'$' | b'+' | b'/' | b'\'' | b'"' | b'*' | b'`'
                    | b'(' | b'{' | b'}' | b'-',
                )
                | None => {
                    self.cur.i = start;
                    return None;
                }
                _ => self.cur.bump(),
            }
        }
        Some(Node::Anonymous(
            self.cur.src()[start..self.here()].to_string(),
        ))
    }

    /// Parse a property name (ident, `@{interp}` pieces, `--custom`), plus an
    /// optional merge flag. Returns `(name, merge, is_custom)`.
    fn parse_property_name(&mut self) -> Option<(String, Option<MergeKind>, bool)> {
        let start = self.here();
        let custom = self.cur.rest().starts_with("--");
        loop {
            let b = self.cur.cur();
            match b {
                Some(b'@') if self.cur.peek(1) == Some(b'{') => {
                    // `@{prop}` interpolation piece — capture verbatim.
                    while let Some(c) = self.cur.cur() {
                        self.cur.bump();
                        if c == b'}' {
                            break;
                        }
                    }
                }
                Some(b'$') if self.cur.peek(1) == Some(b'{') => {
                    while let Some(c) = self.cur.cur() {
                        self.cur.bump();
                        if c == b'}' {
                            break;
                        }
                    }
                }
                Some(b) if b == b'-' || b == b'_' || b == b'*' || b.is_ascii_alphanumeric() => {
                    self.cur.bump();
                }
                Some(b) if b >= 0x80 => self.cur.bump(),
                _ => break,
            }
        }
        if self.here() == start {
            return None;
        }
        let name = self.cur.src()[start..self.here()].to_string();
        // Merge flag: `+` (comma) or `+_` (space) before the `:` — whitespace
        // tolerated around it (`prop +  :`), like less.js's token auto-skip.
        self.cur.skip_whitespace();
        let merge = if self.cur.cur() == Some(b'+') {
            if self.cur.peek(1) == Some(b'_') {
                self.cur.bump();
                self.cur.bump();
                Some(MergeKind::Space)
            } else {
                self.cur.bump();
                Some(MergeKind::Comma)
            }
        } else {
            None
        };
        Some((name, merge, custom))
    }

    /// `!important` (or `! important`) → normalized ` !important`; empty otherwise.
    fn parse_important(&mut self) -> String {
        if self.cur.cur() == Some(b'!') {
            let save = self.here();
            self.cur.bump();
            self.cur.skip_whitespace();
            let word = self.cur.scan_ident();
            if word.eq_ignore_ascii_case("important") {
                return " !important".to_string();
            }
            self.cur.i = save;
        }
        String::new()
    }

    /// Custom-property (`--foo`) value (plan §2.16, less.js `permissiveValue`):
    /// a run of plain *entities* (no operations/parens) parses structured — so
    /// `--x: rgba(0, 30, 0, 238);` IS evaluated, matching less.js — while
    /// anything the entity chain can't fully consume falls back to the raw
    /// capture (where `@{}` interpolation still runs in the eval step).
    fn parse_custom_property_value(&mut self) -> Node {
        let save = self.here();
        if let Some(v) = self.try_custom_entities() {
            return v;
        }
        self.cur.i = save;
        self.parse_custom_property_raw()
    }

    /// The `permissiveValue` entity loop: `entity ([,] entity)*` reaching `;`/`}`.
    fn try_custom_entities(&mut self) -> Option<Node> {
        let mut items: Vec<Node> = Vec::new();
        loop {
            self.skip_value_trivia();
            match self.cur.cur() {
                None | Some(b';') | Some(b'}') => break,
                Some(b',') => {
                    // less.js pushes a literal `,` Anonymous between entities.
                    self.cur.bump();
                    items.push(Node::Anonymous(",".to_string()));
                    continue;
                }
                _ => {}
            }
            let before = self.here();
            let e = self.parse_entity().ok()?;
            if self.here() == before {
                return None; // stalled — not entity-parseable
            }
            // The raw-capture fallback owns `@{…}` interpolation and anything
            // the stray-punctuation arm swallowed byte-by-byte.
            match &e {
                Node::Anonymous(s) if s.len() <= 1 => return None,
                // Not in less.js's `entity()` chain — those go the raw path.
                Node::Interpolation { .. } | Node::Paren { .. } => return None,
                _ => {}
            }
            items.push(e);
        }
        if items.is_empty() {
            return Some(Node::Anonymous(String::new()));
        }
        Some(if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Node::Expression(items)
        })
    }

    /// Raw custom-property capture up to `;`/`}` (balanced).
    fn parse_custom_property_raw(&mut self) -> Node {
        let start = self.here();
        let mut depth = 0i32;
        while let Some(b) = self.cur.cur() {
            match b {
                b';' | b'}' if depth == 0 => break,
                b'{' | b'(' | b'[' => {
                    depth += 1;
                    self.cur.bump();
                }
                b'}' | b')' | b']' => {
                    depth -= 1;
                    self.cur.bump();
                }
                b'"' | b'\'' => {
                    self.cur.scan_string();
                }
                _ => self.cur.bump(),
            }
        }
        Node::Anonymous(self.cur.src()[start..self.here()].trim().to_string())
    }

    // -----------------------------------------------------------------------
    // Selectors → rulesets / mixin definitions / mixin calls
    // -----------------------------------------------------------------------

    fn parse_selector_statement(&mut self) -> Result<Node, LessError> {
        let start = self.here();
        let selectors = self.parse_selector_group()?;
        self.cur.skip_trivia();
        // A mixin call may carry a trailing `!important` before its terminator.
        let important = !self.parse_important().is_empty();
        self.cur.skip_trivia();

        match self.cur.cur() {
            Some(b'{') => {
                let rules = self.parse_block()?;
                // A single selector whose sole element is a `.name(params)` head
                // is a parametric mixin definition.
                if let Some(def) = self.as_mixin_definition(&selectors, &rules, start) {
                    return Ok(Node::MixinDefinition(def));
                }
                Ok(Node::Ruleset(Ruleset {
                    selectors,
                    rules,
                    span: self.span(start),
                }))
            }
            Some(b';') | Some(b'}') | None => {
                // A bare mixin call: `.mixin;` / `.mixin(args);` / `.m() !important;`.
                self.cur.eat(b';');
                Ok(self.as_mixin_call(selectors, important, start))
            }
            _ => Err(self.err("expected '{', ';' or '}' after selector")),
        }
    }

    /// Parse a comma-separated selector group, each selector a run of elements
    /// with an optional trailing `when (...)` guard.
    fn parse_selector_group(&mut self) -> Result<Vec<Selector>, LessError> {
        let mut selectors = Vec::new();
        loop {
            let sel = self.parse_selector()?;
            selectors.push(sel);
            self.cur.skip_trivia();
            if self.cur.cur() == Some(b',') {
                self.cur.bump();
                self.cur.skip_trivia();
                continue;
            }
            break;
        }
        Ok(selectors)
    }

    fn parse_selector(&mut self) -> Result<Selector, LessError> {
        let start = self.here();
        let mut elements = Vec::new();
        let mut first = true;
        loop {
            let ws = self.cur.skip_trivia();
            // A `when (...)` guard ends the selector (may follow whitespace).
            if !first
                && self.cur.rest().starts_with("when")
                && matches!(self.cur.peek(4), Some(b) if b.is_ascii_whitespace() || b == b'(')
            {
                break;
            }
            let ws = if first { false } else { ws };
            match self.cur.cur() {
                None => break,
                Some(b'{') | Some(b',') | Some(b';') | Some(b'}') => break,
                _ => {}
            }
            let combinator = self.scan_combinator(first, ws);
            self.cur.skip_whitespace();
            let elem_start = self.here();
            let value = self.scan_element_value();
            if value.is_empty() {
                if first {
                    return Err(self.err("expected a selector"));
                }
                break;
            }
            elements.push(Element {
                combinator,
                value,
                span: self.span(elem_start),
            });
            first = false;
        }
        // Optional guard.
        let guard = self.try_parse_guard()?;
        Ok(Selector {
            elements,
            guard,
            span: self.span(start),
        })
    }

    /// A combinator preceding an element (plan §4.7 / less.js `combinator`).
    fn scan_combinator(&mut self, first: bool, ws_before: bool) -> String {
        // A slashed combinator `/deep/`, `/shadow/` (less.js `Combinator`).
        if self.cur.cur() == Some(b'/') {
            let save = self.here();
            self.cur.bump();
            let word = self.cur.scan_ident().to_string();
            if !word.is_empty() && self.cur.cur() == Some(b'/') {
                self.cur.bump();
                self.cur.skip_whitespace();
                return format!("/{word}/");
            }
            self.cur.i = save;
        }
        match self.cur.cur() {
            Some(c @ (b'>' | b'+' | b'~' | b'|' | b'^')) => {
                self.cur.bump();
                let mut s = (c as char).to_string();
                if c == b'^' && self.cur.cur() == Some(b'^') {
                    self.cur.bump();
                    s.push('^');
                }
                self.cur.skip_whitespace();
                s
            }
            _ => {
                if first {
                    String::new()
                } else if ws_before {
                    " ".to_string()
                } else {
                    String::new()
                }
            }
        }
    }

    /// Scan a single selector element's text (`.class`, `#id`, `tag`, `*`, `&`,
    /// `[attr=v]`, `:pseudo`, `(sub)`, `@{var}`), balancing `[]`/`()`.
    fn scan_element_value(&mut self) -> String {
        let start = self.here();
        // A keyframe-percentage element (`50%`, `5.5%`).
        if self.cur.at_number() {
            let (_n, _u) = self.cur.scan_number();
            return self.cur.src()[start..self.here()].to_string();
        }
        loop {
            match self.cur.cur() {
                Some(b'[') => self.skip_balanced(b'[', b']'),
                Some(b'(') => self.skip_balanced(b'(', b')'),
                Some(b'@') if self.cur.peek(1) == Some(b'{') => {
                    // `@{var}` interpolation piece.
                    self.cur.bump();
                    self.skip_balanced(b'{', b'}');
                }
                Some(b'\\') => {
                    self.cur.bump();
                    self.cur.bump();
                }
                Some(b'.') | Some(b'#') | Some(b':') | Some(b'&') | Some(b'*') | Some(b'%')
                | Some(b'|') => {
                    self.cur.bump();
                }
                Some(b'-') | Some(b'_') => self.cur.bump(),
                Some(b) if b.is_ascii_alphanumeric() => self.cur.bump(),
                Some(b) if b >= 0x80 => self.cur.bump(),
                _ => break,
            }
        }
        self.cur.src()[start..self.here()].to_string()
    }

    /// Consume a balanced `open … close` region (handles nesting + strings).
    fn skip_balanced(&mut self, open: u8, close: u8) {
        if self.cur.cur() != Some(open) {
            return;
        }
        self.cur.bump();
        let mut depth = 1i32;
        while let Some(b) = self.cur.cur() {
            if b == b'"' || b == b'\'' {
                self.cur.scan_string();
                continue;
            }
            if b == open {
                depth += 1;
            } else if b == close {
                depth -= 1;
                self.cur.bump();
                if depth == 0 {
                    break;
                }
                continue;
            }
            self.cur.bump();
        }
    }

    /// Parse an optional `when (guard)` clause, retaining the full boolean
    /// expression as raw text (comma-OR, `and`/`or`, `not`, nested parens — the
    /// evaluator parses it, plan §2.6).
    fn try_parse_guard(&mut self) -> Result<Option<Box<Node>>, LessError> {
        let save = self.here();
        self.cur.skip_trivia();
        if !self.cur.rest().starts_with("when")
            || !matches!(self.cur.peek(4), Some(b) if b.is_ascii_whitespace() || b == b'(')
        {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.eat_str("when");
        self.cur.skip_trivia();
        let gs = self.here();
        // The guard runs up to the block/terminator, balancing parens + strings.
        let mut depth = 0i32;
        while let Some(b) = self.cur.cur() {
            match b {
                b'{' | b';' if depth == 0 => break,
                b'}' if depth == 0 => break,
                b'(' | b'[' => {
                    depth += 1;
                    self.cur.bump();
                }
                b')' | b']' => {
                    depth -= 1;
                    self.cur.bump();
                }
                b'"' | b'\'' => {
                    self.cur.scan_string();
                }
                _ => self.cur.bump(),
            }
        }
        let raw = self.cur.src()[gs..self.here()].trim().to_string();
        Ok(Some(Box::new(Node::Anonymous(raw))))
    }

    /// Recognize a parametric mixin definition from a parsed selector group.
    /// The parameter parens may be attached (`.m(@a)`) or a separate element after
    /// whitespace (`.m (@a)`) — both split to `(name, params)` here.
    fn as_mixin_definition(
        &self,
        selectors: &[Selector],
        rules: &[Node],
        start: usize,
    ) -> Option<MixinDefinition> {
        if selectors.len() != 1 {
            return None;
        }
        let sel = &selectors[0];
        let (path_els, args_src) = split_mixin_parens(&sel.elements);
        let args_src = args_src?;
        // A definition is a single `.name`/`#name` head + the parameter parens.
        if path_els.len() != 1 {
            return None;
        }
        let name = path_els[0].value.clone();
        if !(name.starts_with('.') || name.starts_with('#')) {
            return None;
        }
        let params = parse_mixin_params(&args_src);
        Some(MixinDefinition {
            name,
            params,
            guard: sel.guard.clone(),
            rules: rules.to_vec(),
            span: Span::new(start, sel.span.end),
        })
    }

    /// Build a mixin call from a parsed selector group ending in `;`.
    fn as_mixin_call(&self, selectors: Vec<Selector>, important: bool, start: usize) -> Node {
        let elements = selectors
            .into_iter()
            .next()
            .map(|s| s.elements)
            .unwrap_or_default();
        let (path, args_src) = split_mixin_parens(&elements);
        let args = args_src
            .map(|s| parse_mixin_args(&s))
            .unwrap_or_default();
        Node::MixinCall(MixinCall {
            path,
            args,
            important,
            span: self.span(start),
        })
    }

    // -----------------------------------------------------------------------
    // Values (delegated to expr.rs helpers on `Parser`)
    // -----------------------------------------------------------------------

    fn parse_url(&mut self) -> Result<Node, LessError> {
        // consume `url` `(` … `)`
        self.cur.eat_str("url");
        self.cur.skip_whitespace();
        if !self.cur.eat(b'(') {
            return Err(self.err("expected '(' after url"));
        }
        self.cur.skip_whitespace();
        let inner = if matches!(self.cur.cur(), Some(b'"') | Some(b'\'')) {
            self.parse_quoted()
        } else {
            let s = self.here();
            let mut depth = 0i32;
            while let Some(b) = self.cur.cur() {
                match b {
                    b')' if depth == 0 => break,
                    b'(' => {
                        depth += 1;
                        self.cur.bump();
                    }
                    b')' => {
                        depth -= 1;
                        self.cur.bump();
                    }
                    _ => self.cur.bump(),
                }
            }
            Node::Anonymous(self.cur.src()[s..self.here()].trim().to_string())
        };
        self.cur.skip_whitespace();
        self.cur.eat(b')');
        Ok(Node::Url(Box::new(inner)))
    }

    fn parse_quoted(&mut self) -> Node {
        let escaped = self.cur.eat(b'~');
        let raw = self.cur.scan_string();
        let quote = raw.chars().next().unwrap_or('"');
        let inner = &raw[1..raw.len().saturating_sub(1)];
        Node::Quoted {
            escaped,
            quote,
            value: inner.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Value expressions (expr.rs ownership; co-located for cursor state)
    // -----------------------------------------------------------------------

    /// A declaration/variable value: a comma-separated list of expressions
    /// (plan value model). Stops at `;`, `}`, `)`, `!important`, or EOF.
    fn parse_value(&mut self) -> Result<Node, LessError> {
        let mut exprs = vec![self.parse_expression()?];
        loop {
            self.skip_value_trivia();
            if self.cur.cur() == Some(b',') {
                self.cur.bump();
                self.skip_value_trivia();
                if self.at_value_end() {
                    break;
                }
                exprs.push(self.parse_expression()?);
            } else {
                break;
            }
        }
        Ok(if exprs.len() == 1 {
            exprs.pop().unwrap()
        } else {
            Node::Value(exprs)
        })
    }

    /// A space-separated expression of entities/operations (plan §2.4). Block
    /// comments between items are KEPT as `Comment` nodes (less.js `expression`
    /// pushes them; function callers filter them, plain values render them).
    fn parse_expression(&mut self) -> Result<Node, LessError> {
        let mut items = Vec::new();
        loop {
            self.cur.skip_whitespace();
            if self.cur.at_block_comment() {
                let s = self.here();
                self.cur.scan_comment();
                items.push(Node::Comment {
                    text: self.cur.src()[s..self.here()].to_string(),
                    line: false,
                    span: self.span(s),
                });
                continue;
            }
            if self.cur.at_line_comment() {
                self.cur.scan_comment();
                continue;
            }
            if self.at_value_end() || self.cur.cur() == Some(b',') {
                break;
            }
            let before = self.here();
            items.push(self.parse_addition()?);
            if self.here() == before {
                // Never stall on an unrecognized byte — capture it raw.
                let s = self.here();
                self.cur.bump();
                items.push(Node::Anonymous(self.cur.src()[s..self.here()].to_string()));
            }
        }
        Ok(match items.len() {
            0 => Node::Anonymous(String::new()),
            1 => items.pop().unwrap(),
            _ => Node::Expression(items),
        })
    }

    /// `+`/`-` operations — only when whitespace surrounds the operator (plan
    /// §2.4: `@a - 1` is subtraction; `@a -1` is a two-item list).
    fn parse_addition(&mut self) -> Result<Node, LessError> {
        let mut left = self.parse_multiplication()?;
        loop {
            let save = self.here();
            // less.js `addition`: op is `[-+]` when whitespace *follows* it, OR
            // when there is no whitespace *before* it. Only `<space><op><no-space>`
            // (`@a -1`) is a sign, not an operator (plan §2.4).
            let sp_before = self.cur.skip_whitespace();
            let op = self.cur.cur();
            if !matches!(op, Some(b'+') | Some(b'-')) {
                self.cur.i = save;
                break;
            }
            let opc = op.unwrap();
            self.cur.bump();
            let sp_after = self.cur.skip_whitespace();
            if !(sp_after || !sp_before) {
                self.cur.i = save; // a sign, not an operator
                break;
            }
            let before = self.here();
            let right = self.parse_multiplication()?;
            if self.here() == before {
                self.cur.i = save; // no right operand
                break;
            }
            let mut right = right;
            mark_in_op(&mut left);
            mark_in_op(&mut right);
            left = Node::Operation {
                op: (opc as char).to_string(),
                left: Box::new(left),
                right: Box::new(right),
                spaced: sp_before || sp_after,
            };
        }
        Ok(left)
    }

    /// `*`/`/` operations (plan §2.4). `/` before a comment (`/*`, `//`) is not an
    /// operator.
    fn parse_multiplication(&mut self) -> Result<Node, LessError> {
        let mut left = self.parse_operand()?;
        loop {
            let save = self.here();
            let had_ws = self.cur.skip_whitespace();
            let op = self.cur.cur();
            let is_div = op == Some(b'/') && !matches!(self.cur.peek(1), Some(b'*') | Some(b'/'));
            if op == Some(b'*') || is_div {
                let opc = op.unwrap();
                self.cur.bump();
                let ws_after = self.cur.skip_whitespace();
                let before = self.here();
                let right = self.parse_operand()?;
                if self.here() == before {
                    self.cur.i = save; // no right operand — leave the operator alone
                    break;
                }
                let mut right = right;
                mark_in_op(&mut left);
                mark_in_op(&mut right);
                left = Node::Operation {
                    op: (opc as char).to_string(),
                    left: Box::new(left),
                    right: Box::new(right),
                    spaced: had_ws || ws_after,
                };
            } else {
                self.cur.i = save;
                break;
            }
        }
        Ok(left)
    }

    /// A single operand: an entity, a parenthesized value, or a negation.
    fn parse_operand(&mut self) -> Result<Node, LessError> {
        self.skip_value_trivia();
        if self.cur.cur() == Some(b'-') && matches!(self.cur.peek(1), Some(b'@') | Some(b'(')) {
            self.cur.bump();
            let mut inner = self.parse_entity()?;
            // less.js `operand`: a negated sub is marked `parensInOp`.
            mark_in_op(&mut inner);
            return Ok(Node::Negative(Box::new(inner)));
        }
        self.parse_entity()
    }

    /// A value leaf (plan §2.1/§2.7/§2.18).
    fn parse_entity(&mut self) -> Result<Node, LessError> {
        self.skip_value_trivia();
        match self.cur.cur() {
            Some(b'"') | Some(b'\'') => Ok(self.parse_quoted()),
            Some(b'~') if matches!(self.cur.peek(1), Some(b'"') | Some(b'\'')) => {
                Ok(self.parse_quoted())
            }
            // `%(fmt, …)` and the `~(…)` list escape are calls whose names are
            // punctuation (less.js call-name regex `[\w-]+|%|~|progid:[\w.]+`).
            Some(b'%') | Some(b'~') if self.cur.peek(1) == Some(b'(') => {
                let name = (self.cur.cur().unwrap() as char).to_string();
                self.cur.bump();
                self.cur.bump();
                let args = self.parse_call_args()?;
                self.skip_value_trivia();
                self.cur.eat(b')');
                Ok(Node::Call { name, args })
            }
            Some(b'(') => {
                self.cur.bump();
                let inner = self.parse_value()?;
                self.skip_value_trivia();
                self.cur.eat(b')');
                Ok(Node::Paren { inner: Box::new(inner), in_op: false })
            }
            Some(b'[') => {
                // A bracketed value token (`[line-name]` in grid, an attribute-ish
                // run) — captured verbatim as one entity.
                let s = self.here();
                self.skip_balanced(b'[', b']');
                Ok(Node::Anonymous(self.cur.src()[s..self.here()].to_string()))
            }
            Some(b'#') => {
                // A hex color (or, rarely, an id-ish token) — captured verbatim.
                let s = self.here();
                self.cur.bump();
                self.cur.scan_ident();
                let text = self.cur.src()[s..self.here()].to_string();
                match crate::color::Color::from_hex(&text) {
                    Some(color) => Ok(Node::Color(color)),
                    None => Ok(Node::Anonymous(text)),
                }
            }
            Some(b'@') if self.cur.peek(1) == Some(b'{') => {
                let start = self.here();
                self.cur.bump();
                self.skip_balanced(b'{', b'}');
                // `@{a}_checked` / `@{a}@{b}px` — an interpolation glued to more
                // word chars is ONE token: less.js's permissiveValue captures it
                // as an escaped Quoted whose `@{}`s interpolate on eval.
                if matches!(self.cur.cur(), Some(c) if c == b'-' || c == b'_'
                    || c.is_ascii_alphanumeric() || c >= 0x80)
                    || (self.cur.cur() == Some(b'@') && self.cur.peek(1) == Some(b'{'))
                {
                    return Ok(self.scan_interp_word(start));
                }
                let raw = &self.cur.src()[start..self.here()];
                let name = raw
                    .trim_start_matches("@{")
                    .trim_end_matches('}')
                    .to_string();
                Ok(Node::Interpolation {
                    name,
                    span: self.span(start),
                })
            }
            Some(b'@') if self.cur.peek(1) == Some(b'@') => {
                let start = self.here();
                self.cur.bump();
                self.cur.bump();
                let name = self.cur.scan_ident().to_string();
                Ok(Node::VariableVariable {
                    name,
                    span: self.span(start),
                })
            }
            Some(b'@') => {
                let start = self.here();
                self.cur.bump();
                let name = self.cur.scan_ident().to_string();
                Ok(Node::Variable {
                    name,
                    span: self.span(start),
                })
            }
            Some(b'$') => {
                let start = self.here();
                self.cur.bump();
                let name = self.cur.scan_ident().to_string();
                Ok(Node::PropertyAccessor {
                    name,
                    span: self.span(start),
                })
            }
            Some(b'\\') => {
                // A CSS-escaped identifier token in a value (`#000 \9`,
                // `\5FAE\8F6F`): less.js's keyword regex accepts `\` escapes —
                // 1-6 hex digits (+ one optional trailing space) or any single
                // char — interleaved with ident chars, as ONE keyword.
                let s = self.here();
                loop {
                    match self.cur.cur() {
                        Some(b'\\') => {
                            self.cur.bump();
                            let mut n = 0;
                            while n < 6
                                && matches!(self.cur.cur(), Some(c) if c.is_ascii_hexdigit())
                            {
                                self.cur.bump();
                                n += 1;
                            }
                            if n > 0 {
                                if self.cur.cur() == Some(b' ') {
                                    self.cur.bump();
                                }
                            } else if self.cur.cur().is_some() {
                                self.cur.bump();
                            }
                        }
                        Some(b'-') | Some(b'_') => self.cur.bump(),
                        Some(c) if c.is_ascii_alphanumeric() => self.cur.bump(),
                        Some(c) if c >= 0x80 => self.cur.bump(),
                        _ => break,
                    }
                }
                Ok(Node::Keyword(self.cur.src()[s..self.here()].trim_end().to_string()))
            }
            _ if self.cur.at_number() => {
                let (n, u) = self.cur.scan_number();
                let value: f64 = n.parse().unwrap_or(0.0);
                Ok(Node::Dimension(crate::value::Dimension::with_unit(value, u)))
            }
            Some(b'U') | Some(b'u') if self.cur.peek(1) == Some(b'+') => {
                // A unicode-range descriptor: `U+[0-9A-Fa-f?]+(-[0-9A-Fa-f?]+)?`.
                let s = self.here();
                self.cur.bump(); // U
                self.cur.bump(); // +
                while matches!(self.cur.cur(), Some(b) if b.is_ascii_hexdigit() || b == b'?' || b == b'-') {
                    self.cur.bump();
                }
                Ok(Node::Anonymous(self.cur.src()[s..self.here()].to_string()))
            }
            Some(b) if b.is_ascii_alphabetic() || b == b'-' || b == b'_' || b >= 0x80 => {
                let start = self.here();
                let mut ident = self.cur.scan_ident().to_string();
                if ident.eq_ignore_ascii_case("url") && self.cur.cur() == Some(b'(') {
                    self.cur.i = start;
                    return self.parse_url();
                }
                // IE-filter call names: `progid:[\w.]+(` (less.js call-name
                // regex; plan §2.17). Extends the name past `:` and `.`.
                if ident.eq_ignore_ascii_case("progid") && self.cur.cur() == Some(b':') {
                    let save = self.here();
                    self.cur.bump();
                    while matches!(self.cur.cur(), Some(c) if c == b'.' || c == b'_'
                        || c.is_ascii_alphanumeric())
                    {
                        self.cur.bump();
                    }
                    if self.cur.cur() == Some(b'(') {
                        ident = self.cur.src()[start..self.here()].to_string();
                    } else {
                        self.cur.i = save;
                    }
                }
                if self.cur.cur() == Some(b'(') {
                    // Function call.
                    self.cur.bump();
                    let args = self.parse_call_args()?;
                    self.skip_value_trivia();
                    self.cur.eat(b')');
                    return Ok(Node::Call { name: ident, args });
                }
                Ok(Node::Keyword(ident))
            }
            _ => {
                // A stray punctuation entity (e.g. a lone `/` in `0 0 / 1`).
                let s = self.here();
                self.cur.bump();
                Ok(Node::Anonymous(self.cur.src()[s..self.here()].to_string()))
            }
        }
    }

    /// Parse a function call's arguments, stopping at `)`. Mirrors less.js
    /// `arguments()`: comma-separated normally; when a top-level `;` occurs, the
    /// semicolon groups win and each group's commas form one `Value` argument.
    /// A `{ … }` argument is a detached ruleset; `.( … ) { … }`/`#( … ) { … }`
    /// is an anonymous mixin (the `each()` callback forms).
    fn parse_call_args(&mut self) -> Result<Vec<Node>, LessError> {
        let mut comma_args: Vec<Node> = Vec::new();
        let mut semi_args: Vec<Node> = Vec::new();
        let mut semi = false;
        self.skip_value_trivia();
        if self.cur.cur() == Some(b')') {
            return Ok(comma_args);
        }
        loop {
            let arg = if self.cur.cur() == Some(b'{') {
                let start = self.here();
                let rules = self.parse_block()?;
                Node::DetachedRuleset {
                    rules,
                    span: self.span(start),
                }
            } else if let Some(m) = self.try_anonymous_mixin()? {
                m
            } else if let Some(a) = self.try_assignment()? {
                a
            } else {
                self.parse_expression()?
            };
            comma_args.push(arg);
            self.skip_value_trivia();
            match self.cur.cur() {
                Some(b',') => {
                    self.cur.bump();
                    self.skip_value_trivia();
                }
                Some(b';') => {
                    self.cur.bump();
                    self.skip_value_trivia();
                    semi = true;
                    semi_args.push(group_args(std::mem::take(&mut comma_args)));
                    if self.cur.cur() == Some(b')') {
                        break;
                    }
                }
                _ => break,
            }
        }
        if semi {
            if !comma_args.is_empty() {
                semi_args.push(group_args(comma_args));
            }
            return Ok(semi_args);
        }
        Ok(comma_args)
    }

    /// An anonymous mixin argument `.(@v; @k) { … }` / `#(@v) { … }` (the
    /// `each()` callback with named params).
    fn try_anonymous_mixin(&mut self) -> Result<Option<Node>, LessError> {
        if !matches!(self.cur.cur(), Some(b'.') | Some(b'#')) || self.cur.peek(1) != Some(b'(') {
            return Ok(None);
        }
        let start = self.here();
        self.cur.bump(); // . or #
        let params_start = self.here();
        self.skip_balanced(b'(', b')');
        let params_src = self.cur.src()[params_start + 1..self.here().saturating_sub(1)].to_string();
        self.cur.skip_trivia();
        if self.cur.cur() != Some(b'{') {
            self.cur.i = start;
            return Ok(None);
        }
        let rules = self.parse_block()?;
        Ok(Some(Node::MixinDefinition(crate::ast::MixinDefinition {
            name: String::new(),
            params: parse_mixin_params(&params_src),
            guard: None,
            rules,
            span: self.span(start),
        })))
    }

    /// Continue scanning a word interleaving ident chars and `@{…}` pieces from
    /// `start`; the whole run becomes an escaped Quoted (interpolates on eval,
    /// renders raw — the `@{a}_checked` gluing case).
    fn scan_interp_word(&mut self, start: usize) -> Node {
        loop {
            match self.cur.cur() {
                Some(b'@') if self.cur.peek(1) == Some(b'{') => {
                    self.cur.bump();
                    self.skip_balanced(b'{', b'}');
                }
                Some(c) if c == b'-' || c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 => {
                    self.cur.bump();
                }
                _ => break,
            }
        }
        Node::Quoted {
            escaped: true,
            quote: '\'',
            value: self.cur.src()[start..self.here()].to_string(),
        }
    }

    /// An IE-filter `key=value` argument (less.js `entities.assignment`, §2.17):
    /// `\w+ =` then an entity. Backtracks if the shape doesn't match.
    fn try_assignment(&mut self) -> Result<Option<Node>, LessError> {
        let save = self.here();
        let key = self.cur.scan_ident().to_string();
        if key.is_empty() || key.contains('-') {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.skip_whitespace();
        if !self.cur.eat(b'=') {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.skip_whitespace();
        let value = self.parse_entity()?;
        Ok(Some(Node::Assignment {
            key,
            value: Box::new(value),
        }))
    }

    /// Skip whitespace and block comments within a value (line comments too).
    fn skip_value_trivia(&mut self) {
        loop {
            self.cur.skip_whitespace();
            if self.cur.at_block_comment() || self.cur.at_line_comment() {
                self.cur.scan_comment();
            } else {
                break;
            }
        }
    }

    /// True at a value terminator.
    fn at_value_end(&self) -> bool {
        matches!(
            self.cur.cur(),
            None | Some(b';') | Some(b'}') | Some(b')') | Some(b'!') | Some(b'{')
        )
    }
}

/// Collapse one semicolon-group of call arguments (less.js `arguments()`):
/// a single expression stays itself, several comma pieces form one `Value`.
fn group_args(v: Vec<Node>) -> Node {
    if v.len() == 1 {
        v.into_iter().next().unwrap()
    } else {
        Node::Value(v)
    }
}

/// Split `/* … */` comments out of an at-rule prelude (they relocate into the
/// block; see `parse_at_rule`). Quotes are respected.
fn split_prelude_comments(s: &str) -> (String, Vec<String>) {
    let mut clean = String::with_capacity(s.len());
    let mut comments = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                let end = s[i + 2..].find("*/").map(|e| i + 2 + e + 2).unwrap_or(b.len());
                comments.push(s[i..end].to_string());
                i = end;
            }
            q @ (b'"' | b'\'') => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                clean.push_str(&s[start..i]);
            }
            _ => {
                let ch_len = utf8_char_len(b[i]);
                clean.push_str(&s[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    (clean, comments)
}

/// The byte length of the UTF-8 char whose lead byte is `b`.
fn utf8_char_len(b: u8) -> usize {
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

/// Mark a parenthesized operand as participating in an operation (less.js sets
/// `parensInOp` on both operands in `addition`/`multiplication`; only parens
/// carry the flag for us — see `Node::Paren`).
fn mark_in_op(node: &mut Node) {
    if let Node::Paren { in_op, .. } = node {
        *in_op = true;
    }
}

/// Parse a `//@magento_import [(reference)] 'path';` directive line (§7.1).
fn parse_magento_import(comment: &str, span: Span) -> Option<Node> {
    let body = comment.strip_prefix("//")?.trim_start();
    let rest = body.strip_prefix("@magento_import")?.trim_start();
    let (reference, rest) = match rest.strip_prefix("(reference)") {
        Some(r) => (true, r.trim_start()),
        None => (false, rest),
    };
    // The path is a quoted string.
    let bytes = rest.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let end = rest[1..].find(quote as char)? + 1;
    let path = rest[1..end].to_string();
    Some(Node::MagentoImport {
        path: Box::new(Node::Quoted {
            escaped: false,
            quote: quote as char,
            value: path,
        }),
        reference,
        span,
    })
}

/// Split a selector's elements into `(path_elements, Some(args_src))` when the
/// last element carries the mixin parentheses, else `(elements, None)`. Handles
/// both attached (`.m(@a)` — one element) and detached (`.m (@a)` — a trailing
/// `(@a)` element) parameter parens.
fn split_mixin_parens(elements: &[Element]) -> (Vec<Element>, Option<String>) {
    let mut els: Vec<Element> = elements.to_vec();
    if let Some(last) = els.last_mut() {
        let v = &last.value;
        // A standalone `( … )` element (space before the parens).
        if v.starts_with('(') && v.ends_with(')') && v.len() >= 2 {
            let args = v[1..v.len() - 1].to_string();
            els.pop();
            return (els, Some(args));
        }
        // A `.name( … )` element (parens attached to the name).
        if let Some(p) = v.find('(') {
            if v.ends_with(')') {
                let args = v[p + 1..v.len() - 1].to_string();
                last.value.truncate(p);
                return (els, Some(args));
            }
        }
    }
    (els, None)
}

/// Parse a mixin-definition parameter list source (`@a; @b: 2; @rest...`).
fn parse_mixin_params(src: &str) -> Vec<MixinParam> {
    let parts = split_args(src);
    parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let p = p.trim();
            if let Some(v) = p.strip_suffix("...") {
                let v = v.trim();
                MixinParam {
                    name: if v.is_empty() { None } else { Some(v.to_string()) },
                    default: None,
                    variadic: true,
                }
            } else if let Some(colon) = p.find(':') {
                let (n, d) = p.split_at(colon);
                MixinParam {
                    name: Some(n.trim().to_string()),
                    default: Some(Box::new(Node::Anonymous(d[1..].trim().to_string()))),
                    variadic: false,
                }
            } else if p.starts_with('@') {
                MixinParam {
                    name: Some(p.to_string()),
                    default: None,
                    variadic: false,
                }
            } else {
                // A positional literal/pattern param.
                MixinParam {
                    name: None,
                    default: Some(Box::new(Node::Anonymous(p.to_string()))),
                    variadic: false,
                }
            }
        })
        .collect()
}

/// Parse a mixin-call argument list source into [`MixinArg`]s.
fn parse_mixin_args(src: &str) -> Vec<MixinArg> {
    split_args(src)
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let p = p.trim();
            if p.starts_with('@') {
                if let Some(colon) = p.find(':') {
                    let (n, v) = p.split_at(colon);
                    return MixinArg {
                        name: Some(n.trim().to_string()),
                        value: Box::new(Node::Anonymous(v[1..].trim().to_string())),
                    };
                }
            }
            MixinArg {
                name: None,
                value: Box::new(Node::Anonymous(p.to_string())),
            }
        })
        .collect()
}

/// Split an argument source on `;` (preferred) or `,`, respecting nesting/strings.
/// less.js uses `;` as the separator when any argument contains a comma (§2.5).
fn split_args(src: &str) -> Vec<String> {
    let has_semi = scan_top_level(src, b';');
    let sep = if has_semi { b';' } else { b',' };
    split_on_top_level(src, sep)
}

fn scan_top_level(src: &str, sep: u8) -> bool {
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'"' | b'\'' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b if b == sep && depth == 0 => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(src: &str) -> Vec<Node> {
        let opts = LessOptions::default();
        match parse(src, FileInfo::default(), &opts).unwrap().as_ref() {
            Node::Root(r) => r.clone(),
            _ => panic!("not a root"),
        }
    }

    #[test]
    fn parses_variable_declaration_and_reference() {
        let r = root("@color: #fff;\na { c: @color; }");
        assert!(matches!(&r[0], Node::VariableDecl { name, .. } if name == "color"));
        let Node::Ruleset(rs) = &r[1] else {
            panic!("expected ruleset");
        };
        let Node::Declaration(d) = &rs.rules[0] else {
            panic!("expected declaration");
        };
        assert!(matches!(d.value.as_ref(), Node::Variable { name, .. } if name == "color"));
    }

    #[test]
    fn parses_mixin_definition_with_params() {
        let r = root(".m(@a; @b: 2px) { x: @a; }");
        let Node::MixinDefinition(def) = &r[0] else {
            panic!("expected mixin definition");
        };
        assert_eq!(def.name, ".m");
        assert_eq!(def.params.len(), 2);
        assert_eq!(def.params[0].name.as_deref(), Some("@a"));
        assert!(def.params[1].default.is_some());
    }

    #[test]
    fn parses_mixin_call_with_args_and_important() {
        let r = root(".foo { .m(1, 2) !important; }");
        let Node::Ruleset(rs) = &r[0] else {
            panic!("expected ruleset");
        };
        let Node::MixinCall(call) = &rs.rules[0] else {
            panic!("expected mixin call");
        };
        assert!(call.important);
        assert_eq!(call.args.len(), 2);
    }

    #[test]
    fn parses_operations_with_precedence() {
        // `1 + 2 * 3` → Operation('+', 1, Operation('*', 2, 3)).
        let r = root("a { b: 1 + 2 * 3; }");
        let Node::Ruleset(rs) = &r[0] else {
            panic!()
        };
        let Node::Declaration(d) = &rs.rules[0] else {
            panic!()
        };
        let Node::Operation { op, right, .. } = d.value.as_ref() else {
            panic!("expected operation, got {:?}", d.value);
        };
        assert_eq!(op, "+");
        assert!(matches!(right.as_ref(), Node::Operation { op, .. } if op == "*"));
    }

    #[test]
    fn parses_function_call_and_color() {
        let r = root("a { b: rgba(255, 0, 0, 0.5); c: #abc; }");
        let Node::Ruleset(rs) = &r[0] else {
            panic!()
        };
        let Node::Declaration(d0) = &rs.rules[0] else {
            panic!()
        };
        assert!(matches!(d0.value.as_ref(), Node::Call { name, args } if name == "rgba" && args.len() == 4));
        let Node::Declaration(d1) = &rs.rules[1] else {
            panic!()
        };
        assert!(matches!(d1.value.as_ref(), Node::Color(c) if c.original.as_deref() == Some("#abc")));
    }

    #[test]
    fn parses_nested_ruleset_and_parent_selector() {
        let r = root(".a { &:hover { x: 1; } .b { y: 2; } }");
        let Node::Ruleset(rs) = &r[0] else {
            panic!()
        };
        assert_eq!(rs.rules.len(), 2);
        assert!(matches!(&rs.rules[0], Node::Ruleset(_)));
        let Node::Ruleset(inner) = &rs.rules[0] else {
            panic!()
        };
        assert_eq!(inner.selectors[0].elements[0].value, "&:hover");
    }

    #[test]
    fn parses_at_rule_with_block_and_prelude() {
        let r = root("@media screen and (min-width: 700px) { a { b: c; } }");
        let Node::AtRule(a) = &r[0] else {
            panic!("expected at-rule");
        };
        assert_eq!(a.name, "@media");
        assert!(a.prelude.is_some());
        assert!(matches!(a.block, AtRuleBlock::Rules(_)));
    }

    #[test]
    fn parses_import_with_options() {
        let r = root("@import (reference) \"foo.less\";");
        let Node::Import { options, path, .. } = &r[0] else {
            panic!("expected import");
        };
        assert_eq!(options, &["reference"]);
        assert!(matches!(path.as_ref(), Node::Quoted { value, .. } if value == "foo.less"));
    }

    #[test]
    fn parses_custom_property_raw() {
        let r = root("a { --x: 1 + 2; }");
        let Node::Ruleset(rs) = &r[0] else {
            panic!()
        };
        let Node::Declaration(d) = &rs.rules[0] else {
            panic!()
        };
        assert!(d.custom);
        // NOT evaluated as LESS — kept raw (plan §2.16).
        assert!(matches!(d.value.as_ref(), Node::Anonymous(s) if s == "1 + 2"));
    }

    #[test]
    fn magento_import_directive_only_in_magento_mode() {
        // Default (non-magento) mode: the directive is a stripped line comment.
        let r = root("//@magento_import 'source/_x.less';\na { b: c; }");
        assert_eq!(r.len(), 1);
        assert!(matches!(&r[0], Node::Ruleset(_)));

        // magento_mode: it becomes a directive node (§7.1).
        let opts = LessOptions::magento_production();
        let parsed = parse(
            "//@magento_import (reference) 'source/_x.less';",
            FileInfo::default(),
            &opts,
        )
        .unwrap();
        let Node::Root(rules) = parsed.as_ref() else {
            panic!()
        };
        assert!(
            matches!(&rules[0], Node::MagentoImport { reference, .. } if *reference)
        );
    }
}

fn split_on_top_level(src: &str, sep: u8) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'"' | b'\'' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b if b == sep && depth == 0 => {
                out.push(src[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(src[start..].to_string());
    out
}
