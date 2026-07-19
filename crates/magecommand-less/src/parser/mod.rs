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
use crate::lex::{normalize_source, Cursor};
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
    // A `{ … }` fragment is a detached-ruleset literal (a mixin argument or
    // parameter default — plan §2.11).
    if parser.cur.cur() == Some(b'{') {
        let start = parser.here();
        let rules = parser.parse_block()?;
        return Ok(Node::DetachedRuleset {
            rules,
            span: parser.span(start),
        });
    }
    let value = parser.parse_value()?;
    Ok(value)
}

/// The recursive-descent parser state.
struct Parser<'a> {
    cur: Cursor<'a>,
    file: FileInfo,
    magento_mode: bool,
    /// Nodes a statement parser wants emitted AFTER the statement itself (an
    /// `@import`'s media-feature comments become root-level siblings — the
    /// less.js commentStore flush).
    pending: Vec<Node>,
    /// less.js `parserInput`'s furthest-failure bookkeeping (minimal form):
    /// a `restore("Expected ')'")`-style CANDIDATE recorded by a soft failure.
    /// If the enclosing statement ultimately fails, the candidate becomes the
    /// reported error (message + position); a successful statement clears it.
    furthest: usize,
    furthest_msg: Option<String>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, file: FileInfo, opts: &LessOptions) -> Self {
        Parser {
            cur: Cursor::new(src),
            file,
            magento_mode: opts.magento_mode,
            pending: Vec::new(),
            furthest: 0,
            furthest_msg: None,
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
        self.err_kind_at(ErrorKind::Parse, msg, self.cur.i)
    }

    /// A located parse-time error with an explicit kind — less.js's parser
    /// raises both `Parse` (the default) and `Syntax` (an explicit
    /// `error(msg, 'Syntax')`) types.
    fn err_kind(&self, kind: ErrorKind, msg: impl Into<String>) -> LessError {
        self.err_kind_at(kind, msg, self.cur.i)
    }

    /// A located parse-time error at an explicit byte index.
    fn err_kind_at(&self, kind: ErrorKind, msg: impl Into<String>, index: usize) -> LessError {
        LessError::at(kind, msg, self.file.filename.clone(), index).located(self.cur.src())
    }

    /// less.js `$parseUntil` pairing over a raw capture: every closer must
    /// match the innermost opener (`Expected ']'` at the offender) and every
    /// quote must close on its own line (`Expected '"'` at the opener).
    /// Comments are skipped. `base` = the capture's byte offset in the source.
    fn validate_brackets(&self, base: usize, text: &str) -> Result<(), LessError> {
        let bytes = text.as_bytes();
        let mut stack: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 1,
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                        i += 1;
                    }
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'"' | b'\'' => {
                    let q = bytes[i];
                    let qpos = i;
                    i += 1;
                    loop {
                        match bytes.get(i) {
                            Some(b'\\') => i += 2,
                            Some(&c) if c == q => break,
                            Some(&b'\n') | None => {
                                return Err(self.err_kind_at(
                                    ErrorKind::Parse,
                                    format!("Expected '{}'", q as char),
                                    base + qpos,
                                ));
                            }
                            Some(_) => i += 1,
                        }
                    }
                }
                b'{' | b'(' | b'[' => stack.push(bytes[i]),
                b'}' | b')' | b']' => {
                    let expected = match stack.last() {
                        Some(b'{') => b'}',
                        Some(b'(') => b')',
                        Some(b'[') => b']',
                        _ => bytes[i],
                    };
                    if expected != bytes[i] {
                        return Err(self.err_kind_at(
                            ErrorKind::Parse,
                            format!("Expected '{}'", expected as char),
                            base + i,
                        ));
                    }
                    stack.pop();
                }
                _ => {}
            }
            i += 1;
        }
        Ok(())
    }

    /// less.js's mixin-args `Cannot mix ; and , as delimiter types` check
    /// (parser.js:1097/1147), replayed over the raw argument text. Two error
    /// sites, both Syntax, positions matching the real parser:
    /// - a NAMED arg (`@x:`) while accumulating under `;` separation — at the
    ///   value after the `:` (+ whitespace);
    /// - a `;` separator closing a group whose comma-run contained a named arg
    ///   — right after the `;` (+ whitespace).
    fn validate_arg_separators(&self, base: usize, raw: &str) -> Result<(), LessError> {
        let bytes = raw.as_bytes();
        let len = bytes.len();
        let mut i = 0usize;
        let mut is_semi = false;
        let mut expr_count = 0usize;
        let mut expr_named = false;
        let skip_ws = |i: &mut usize| {
            while *i < len && bytes[*i].is_ascii_whitespace() {
                *i += 1;
            }
        };
        let skip_value = |i: &mut usize| {
            // consume to a TOP-LEVEL `,`/`;`
            let mut depth = 0i32;
            while *i < len {
                match bytes[*i] {
                    b',' | b';' if depth == 0 => break,
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth -= 1,
                    b'"' | b'\'' => {
                        let q = bytes[*i];
                        *i += 1;
                        while *i < len && bytes[*i] != q {
                            if bytes[*i] == b'\\' {
                                *i += 1;
                            }
                            *i += 1;
                        }
                    }
                    _ => {}
                }
                *i += 1;
            }
        };
        while i < len {
            skip_ws(&mut i);
            if i >= len {
                break;
            }
            // A named argument head: `@name :` / `$name :` at top level.
            let mut named = false;
            if bytes[i] == b'@' || bytes[i] == b'$' {
                let mut j = i + 1;
                while j < len
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-' || bytes[j] == b'_')
                {
                    j += 1;
                }
                let mut k = j;
                while k < len && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if j > i + 1 && k < len && bytes[k] == b':' {
                    named = true;
                    i = k + 1;
                    skip_ws(&mut i);
                    if expr_count > 0 {
                        if is_semi {
                            return Err(self.err_kind_at(
                                ErrorKind::Syntax,
                                "Cannot mix ; and , as delimiter types",
                                base + i,
                            ));
                        }
                        expr_named = true;
                    }
                }
            }
            let _ = named;
            skip_value(&mut i);
            expr_count += 1;
            if i < len && bytes[i] == b',' {
                i += 1;
                continue;
            }
            let has_sep = i < len && bytes[i] == b';';
            if has_sep {
                i += 1;
                skip_ws(&mut i);
            }
            if has_sep || is_semi {
                if expr_named {
                    return Err(self.err_kind_at(
                        ErrorKind::Syntax,
                        "Cannot mix ; and , as delimiter types",
                        base + i,
                    ));
                }
                is_semi = true;
                expr_count = 0;
                expr_named = false;
            }
            if !has_sep && (i >= len || bytes[i] != b',') {
                break;
            }
        }
        Ok(())
    }

    /// Scan a backtick-delimited inline-JS expression (verbatim body).
    fn parse_javascript(&mut self, escaped: bool, start: usize) -> Node {
        self.cur.bump(); // `
        let es = self.here();
        while let Some(b) = self.cur.cur() {
            if b == b'`' {
                break;
            }
            self.cur.bump();
        }
        let expr = self.cur.src()[es..self.here()].to_string();
        self.cur.eat(b'`');
        Node::JavaScript {
            expr,
            escaped,
            span: self.span(start),
        }
    }

    /// A SOFT failure marker (message-less): a statement alternative that
    /// sees it backtracks and tries the next parse; if every alternative
    /// fails, [`Self::unrecognised`] surfaces the recorded furthest candidate.
    fn soft_fail(&mut self, msg: &str, index: usize) -> LessError {
        if index >= self.furthest {
            self.furthest = index;
            self.furthest_msg = Some(msg.to_string());
        }
        LessError::new(ErrorKind::Parse, "")
    }

    /// A message-less furthest candidate — less.js `restore()` without a
    /// message: a strictly further failure advances `furthest` and CLEARS any
    /// recorded message, so end-of-parse reports plain `Unrecognised input`
    /// at that position (review F6).
    fn soft_fail_pos(&mut self, index: usize) -> LessError {
        if index > self.furthest {
            self.furthest = index;
            self.furthest_msg = None;
        }
        LessError::new(ErrorKind::Parse, "")
    }

    /// less.js's end-of-parse error (`parser.js` `end()` handling): the
    /// message is `Unrecognised input` plus a hint derived from the character
    /// at the failure index — `}` / `)` suggest a missing opener, reaching EOF
    /// suggests something missing.
    fn unrecognised(&self, index: usize) -> LessError {
        // A recorded furthest candidate (a soft `restore("Expected ')'")`)
        // wins over the generic message — less.js `endInfo
        // .furthestPossibleErrorMessage`.
        if let Some(msg) = &self.furthest_msg {
            return self.err_kind_at(ErrorKind::Parse, msg.clone(), self.furthest);
        }
        // less.js anchors at the FURTHEST position any backtracked
        // alternative reached (`if (i < furthest) i = furthest`), and the
        // `Possibly missing something` suffix fires whenever furthest reached
        // the LAST char or beyond (`furthest >= input.length - 1`), not only
        // at EOF (review F6).
        let index = index.max(self.furthest);
        let src = self.cur.src();
        let mut msg = String::from("Unrecognised input");
        match src.as_bytes().get(index) {
            Some(b'}') => msg.push_str(". Possibly missing opening '{'"),
            Some(b')') => msg.push_str(". Possibly missing opening '('"),
            _ if index + 1 >= src.len() => msg.push_str(". Possibly missing something"),
            _ => {}
        }
        self.err_kind_at(ErrorKind::Parse, msg, index)
    }

    fn expect_eof(&mut self) -> Result<(), LessError> {
        self.cur.skip_trivia();
        if self.cur.eof() {
            Ok(())
        } else {
            Err(self.unrecognised(self.cur.i))
        }
    }

    // -----------------------------------------------------------------------
    // Primary — the rule list at root or inside a `{ … }` block
    // -----------------------------------------------------------------------

    /// Parse a rule list. `root` selects top-level vs a braced block (which stops
    /// at `}`).
    fn parse_primary(&mut self, _root: bool) -> Result<Vec<Node>, LessError> {
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
            if self.cur.cur() == Some(b'}') {
                // In a block the caller consumes it; at root `expect_eof`
                // raises less.js's `Possibly missing opening '{'` error.
                break;
            }
            // Stray semicolons between rules.
            if self.cur.eat(b';') {
                continue;
            }
            let node = match self.parse_statement() {
                Ok(n) => n,
                // A SOFT failure reaching statement level surfaces as the
                // recorded candidate (or generic unrecognised input).
                Err(e) if e.message.is_empty() => return Err(self.unrecognised(self.cur.i)),
                Err(e) => return Err(e),
            };
            // The statement parsed — any soft furthest candidate recorded by a
            // failed alternative inside it is moot (less.js's furthest only
            // matters when the whole parse fails, and a later failure always
            // sits past it; resetting keeps our per-statement machinery from
            // resurrecting stale positions).
            self.furthest_msg = None;
            self.furthest = 0;
            rules.push(node);
            rules.append(&mut self.pending);
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
            // Unclosed block: less.js's primary just stops and the end check
            // reports the furthest position (EOF → `Possibly missing something`).
            return Err(self.unrecognised(self.cur.i));
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
            if let Some(node) = self.try_variable_call_statement()? {
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
        // Colon GLUED to the name = definitely a variable declaration; with
        // whitespace between (`@page :left`) it may be an at-rule + pseudo,
        // so the permissive fallback below must not fire.
        let glued_colon = self.cur.cur() == Some(b':');
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
        // Structured value first; a variable declaration whose value the
        // entity chain rejects (`@list: #selector, .bar;`) falls back to the
        // PERMISSIVE raw capture, like less.js (permissive-parse).
        let vsave = self.here();
        let value = match self.parse_value() {
            Ok(v) => v,
            Err(_) => {
                self.cur.i = vsave;
                self.parse_custom_property_raw()?
            }
        };
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
            // Not a clean structured value — retry with the PERMISSIVE raw
            // capture (brace-balanced), which owns arrow-function-ish values
            // (`@this: () => { …; };`, permissive-parse) — before backing off
            // to at-rule handling. Only for a glued `@name:` head — a spaced
            // colon (`@page :left`) is an at-rule + pseudo-class.
            if !glued_colon {
                self.cur.i = save;
                return Ok(None);
            }
            self.cur.i = vsave;
            let value = self.parse_custom_property_raw()?;
            self.cur.skip_whitespace();
            let important = self.parse_important();
            self.cur.skip_trivia();
            if self.cur.cur() == Some(b';') || self.cur.cur() == Some(b'}') || self.cur.eof() {
                self.cur.eat(b';');
                return Ok(Some(Node::VariableDecl {
                    name,
                    value: Box::new(value),
                    important,
                    span: self.span(start),
                }));
            }
            self.cur.i = save;
            Ok(None)
        }
    }

    /// A detached-ruleset call statement `@dr();` (less.js `variableCall`,
    /// plan §2.11). The `(` must follow the name with NO whitespace (so `@media
    /// (…)` stays an at-rule), and the parens must be empty.
    fn try_variable_call_statement(&mut self) -> Result<Option<Node>, LessError> {
        let save = self.here();
        debug_assert_eq!(self.cur.cur(), Some(b'@'));
        self.cur.bump(); // @
        let name = self.cur.scan_ident().to_string();
        if name.is_empty() || self.cur.cur() != Some(b'(') {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.bump();
        self.cur.skip_whitespace();
        if !self.cur.eat(b')') {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.skip_trivia();
        if !matches!(self.cur.cur(), None | Some(b';') | Some(b'}')) {
            // `@dr() !important;` and friends: less.js has no parser that
            // accepts a variable call followed by anything else — the statement
            // fails as unrecognised input (verified: ParseError).
            return Err(self.err("Unrecognised input"));
        }
        self.cur.eat(b';');
        Ok(Some(Node::VariableCall {
            name,
            span: self.span(save),
        }))
    }

    /// Parse a chain of `[key]` rule lookups (less.js `ruleLookups`, plan §2.12):
    /// each key matches `[@$]{0,2}[\w-]*` (empty = the unnamed lookup). Returns
    /// `None` when the cursor isn't at a valid lookup chain.
    fn try_rule_lookups(&mut self) -> Option<Vec<String>> {
        let mut keys = Vec::new();
        while self.cur.cur() == Some(b'[') {
            let save = self.here();
            self.cur.bump();
            // less.js's parserInput skips whitespace after every matched token,
            // so `[ key ]` / `[\nkey]` resolve like `[key]`.
            self.cur.skip_whitespace();
            let ks = self.here();
            let mut sigils = 0;
            while sigils < 2 && matches!(self.cur.cur(), Some(b'@') | Some(b'$')) {
                self.cur.bump();
                sigils += 1;
            }
            while matches!(self.cur.cur(), Some(b) if b == b'-' || b == b'_'
                || b.is_ascii_alphanumeric())
            {
                self.cur.bump();
            }
            let key = self.cur.src()[ks..self.here()].to_string();
            self.cur.skip_whitespace();
            if !self.cur.eat(b']') {
                self.cur.i = save;
                break;
            }
            keys.push(key);
        }
        if keys.is_empty() {
            None
        } else {
            Some(keys)
        }
    }

    fn parse_at_rule(&mut self) -> Result<Node, LessError> {
        let start = self.here();
        debug_assert_eq!(self.cur.cur(), Some(b'@'));
        self.cur.bump(); // @
        let name = format!("@{}", self.cur.scan_ident());

        // `@@x: …` / a lone `@` — nothing in less.js parses this statement
        // (variable regexes need a word after `@`): Unrecognised input at the
        // statement start (bad-variable-declaration1).
        if name == "@" {
            return Err(self.unrecognised(start));
        }

        // less.js's at-rule matcher is lowercase-only (`@[a-z-]+`-shaped):
        // `@MEDIA`/`@CHARSET` fall through to `variableCall` and die on the
        // missing lookup (verified vs 4.6.7). Its name token was matched via
        // `$re`, which skips trailing whitespace/comments — the error anchors
        // AFTER them (`@CHARSET "x"` cites the quote, review E2).
        if name.bytes().any(|b| b.is_ascii_uppercase()) {
            self.cur.skip_trivia();
            return Err(self.err("Missing '[...]' lookup in variable call"));
        }

        if name == "@import" {
            return self.parse_import(start);
        }

        // Prelude: raw source up to `{` or `;`, structurally re-parsed as a value
        // when non-empty (media queries, `@charset "UTF-8"`, `@namespace svg "…"`).
        // Whitespace only — a comment between the name and the prelude must
        // reach `split_prelude_comments` as a LEADING prelude comment (@media
        // relocates it into the block twice, review C1).
        self.cur.skip_whitespace();
        let prelude_start = self.here();
        self.scan_prelude()?;
        // An un-parenthesized `@{`/`${` stops `scan_prelude` at the interp's
        // `{` — mirroring where less.js's structured prelude parsers halt
        // (`mediaFeature` has no `variableCurly` alternative, `entity()`
        // rejects `@{`). The at-rule kind picks the error (reviews F4/E1):
        // @media/@container → the block-statement Syntax error; identifier
        // at-rules → `expected <name> identifier`; both anchored at the `@`.
        if self.cur.cur() == Some(b'{')
            && matches!(
                self.cur.src().as_bytes().get(self.here().wrapping_sub(1)),
                Some(b'@') | Some(b'$')
            )
            && self.here() > prelude_start
        {
            let interp_pos = self.here() - 1;
            if matches!(name.as_str(), "@media" | "@container") {
                return Err(self.err_kind_at(
                    ErrorKind::Syntax,
                    "media definitions require block statements after any features",
                    interp_pos,
                ));
            }
            if matches!(name.as_str(), "@keyframes" | "@counter-style")
                || name.strip_prefix("@-").is_some_and(|r| {
                    r.split_once('-').is_some_and(|(_, tail)| tail == "keyframes")
                })
            {
                return Err(self.err_kind_at(
                    ErrorKind::Syntax,
                    format!("expected {name} identifier"),
                    interp_pos,
                ));
            }
        }
        let prelude_all = self.cur.src()[prelude_start..self.here()].trim();
        // Block comments in the prelude relocate into the block (less.js's
        // commentStore attaches them to the body's rules — `@-webkit-keyframes
        // hover /* c */{}` renders the comment inside the braces).
        let (prelude_raw, prelude_comments) = split_prelude_comments_mode(
            prelude_all,
            if matches!(name.as_str(), "@media" | "@container") {
                PreludeComments::Media
            } else {
                PreludeComments::AtRule
            },
        );
        let prelude_raw = prelude_raw.trim();
        let prelude = if prelude_raw.is_empty() {
            None
        } else {
            Some(Box::new(Node::Anonymous(prelude_raw.to_string())))
        };

        self.cur.skip_trivia();
        // less.js `prepareAndGetNestableAtRule` (@media/@container): a missing
        // or unclosed block is a Syntax error at the failure position.
        let requires_block = matches!(name.as_str(), "@media" | "@container");
        if requires_block && self.cur.cur() != Some(b'{') {
            return Err(self.err_kind(
                ErrorKind::Syntax,
                "media definitions require block statements after any features",
            ));
        }
        let block = if self.cur.cur() == Some(b'{') {
            let mut rules = match self.parse_block() {
                Ok(r) => r,
                Err(e) if requires_block => {
                    return Err(self.err_kind_at(
                        ErrorKind::Syntax,
                        "media definitions require block statements after any features",
                        e.index.unwrap_or(self.here()),
                    ));
                }
                Err(e) => return Err(e),
            };
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
    fn scan_prelude(&mut self) -> Result<(), LessError> {
        let mut depth = 0i32;
        while let Some(b) = self.cur.cur() {
            match b {
                b'{' | b';' if depth == 0 => break,
                b'}' if depth == 0 => break,
                // A `{` INSIDE parens: the feature parser never closed its
                // paren — less.js's `Missing closing ')'`
                // (parse-error-missing-parens). An interpolation's `{` stalls
                // less.js at the `@`/`$` itself (no `variableCurly`
                // alternative in `mediaFeature`), one char earlier (review E3).
                b'{' => {
                    let at = self.here();
                    let at = match self.cur.src().as_bytes().get(at.wrapping_sub(1)) {
                        Some(b'@') | Some(b'$') if at > 0 => at - 1,
                        _ => at,
                    };
                    return Err(self.err_kind_at(
                        ErrorKind::Parse,
                        "Missing closing ')'",
                        at,
                    ));
                }
                b'u' if self.cur.rest().starts_with("url(") => {
                    // less.js `entities.url()`: the raw run consumes anything
                    // but `()'"` (braces/newlines included), then REQUIRES the
                    // `)` — `expected ')' got ''` at EOF
                    // (at-rules-unmatching-block).
                    self.cur.eat_str("url(");
                    loop {
                        match self.cur.cur() {
                            Some(b'\\') => {
                                self.cur.bump();
                                self.cur.bump();
                            }
                            Some(b'"') | Some(b'\'') => {
                                self.cur.scan_string();
                            }
                            Some(b'(') | Some(b')') | None => break,
                            Some(_) => {
                                self.cur.bump();
                            }
                        }
                    }
                    if !self.cur.eat(b')') {
                        let got = self
                            .cur
                            .cur()
                            .map(|c| (c as char).to_string())
                            .unwrap_or_default();
                        return Err(self.err_kind(
                            ErrorKind::Syntax,
                            format!("expected ')' got '{got}'"),
                        ));
                    }
                }
                b'(' | b'[' => {
                    depth += 1;
                    self.cur.bump();
                }
                // A stray closer at depth 0 ends the prelude unconsumed — the
                // at-rule then fails its block requirement THERE (less.js's
                // mediaFeatures stop at `(extra: bracket))`'s second `)`).
                b')' | b']' if depth == 0 => break,
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
        Ok(())
    }

    fn parse_import(&mut self, start: usize) -> Result<Node, LessError> {
        self.cur.skip_trivia();
        // Options: `@import (reference, once) "…";`
        let mut options = Vec::new();
        if self.cur.cur() == Some(b'(') {
            self.cur.bump();
            // less.js `importOption` is the CLOSED set (parser.js:1865); an
            // unknown word stops the option loop THERE, so `expectChar(')')`
            // fails AT the offending word — `SyntaxError: expected ')' got
            // 'b'` anchored on its first char (review F7), never past the
            // consumed list. `error()` defaults to the Syntax kind.
            loop {
                self.cur.skip_trivia();
                let word_start = self.here();
                while matches!(self.cur.cur(), Some(b) if b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    self.cur.bump();
                }
                let word = self.cur.src()[word_start..self.here()].to_string();
                let known = matches!(
                    word.as_str(),
                    "less" | "css" | "multiple" | "once" | "inline" | "reference" | "optional"
                );
                if !word.is_empty() && known {
                    options.push(word);
                    self.cur.skip_trivia();
                    if self.cur.eat(b',') {
                        continue;
                    }
                    self.cur.skip_trivia();
                } else {
                    self.cur.i = word_start;
                }
                if !self.cur.eat(b')') {
                    let got = self
                        .cur
                        .cur()
                        .map(|b| (b as char).to_string())
                        .unwrap_or_default();
                    return Err(self.err_kind_at(
                        ErrorKind::Syntax,
                        format!("expected ')' got '{got}'"),
                        self.cur.i,
                    ));
                }
                break;
            }
            self.cur.skip_trivia();
        }
        // Path: a url() or a quoted string.
        let path = if self.cur.rest().starts_with("url(") || self.cur.rest().starts_with("url (") {
            self.parse_url()?
        } else if matches!(self.cur.cur(), Some(b'"') | Some(b'\'')) {
            match self.parse_quoted() {
                Some(q) => q,
                // An UNTERMINATED path: less.js's `entities.quoted` rejects
                // it, nulling the path → `SyntaxError: malformed import
                // statement` at the `@import` (review F11).
                None => {
                    return Err(self.err_kind_at(
                        ErrorKind::Syntax,
                        "malformed import statement",
                        start,
                    ));
                }
            }
        } else {
            // less.js accepts only `quoted() || url()` here — a bare WORD is
            // its `malformed import statement` Syntax error, reported with
            // `parserInput.i` reset to the `@import` (parser.js:1827).
            if matches!(self.cur.cur(), Some(b) if b.is_ascii_alphabetic()) {
                return Err(self.err_kind_at(
                    ErrorKind::Syntax,
                    "malformed import statement",
                    start,
                ));
            }
            // Interpolated path — capture up to a media list, `;` or EOF.
            let ps = self.here();
            while let Some(b) = self.cur.cur() {
                if matches!(b, b';' | b'{') || b.is_ascii_whitespace() {
                    break;
                }
                self.cur.bump();
            }
            Node::Anonymous(self.cur.src()[ps..self.here()].to_string())
        };
        // Optional media features up to `;`. Feature-list comments become
        // root-level siblings AFTER the import (less.js commentStore flush).
        self.cur.skip_whitespace();
        let fs = self.here();
        self.scan_prelude()?;
        let feat_all = self.cur.src()[fs..self.here()].trim();
        // EVERY feature-list comment flushes as a root sibling after the
        // import (less.js commentStore; probed: a mid-feature `/*! c */`
        // renders after the import's media block too — review C1i).
        let (feat_clean, feat_comments) = split_prelude_comments_all(feat_all);
        for text in feat_comments {
            self.pending.push(Node::Comment {
                text,
                line: false,
                span: self.span(start),
            });
        }
        let feat_raw = feat_clean.trim();
        let features = if feat_raw.is_empty() {
            None
        } else {
            Some(Box::new(Node::Anonymous(feat_raw.to_string())))
        };
        // less.js: the statement must close with `;` — EOF included
        // (import-no-semi) — reported at the @import (`i = index; error(…)`,
        // parser.js:1820).
        if !self.cur.eat(b';') {
            return Err(self.err_kind_at(
                ErrorKind::Syntax,
                "missing semi-colon or unrecognised media features on import",
                start,
            ));
        }
        Ok(Node::Import {
            path: Box::new(path),
            options,
            features,
            error: None,
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
        // Comments may sit between the name and the `:` (dropped, like
        // less.js's commentStore: `color/* survive */: grey`).
        self.cur.skip_trivia();
        if !self.cur.eat(b':') {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.skip_trivia();

        // A `{` after a PROPERTY's colon: detached rulesets only bind to
        // `@vars` — less.js has no parser for this and dies with the furthest
        // `Unrecognised input` at the `{` (detached-ruleset-6).
        if !custom && self.cur.cur() == Some(b'{') {
            return Err(self.unrecognised(self.here()));
        }
        let value = if custom {
            self.parse_custom_property_value()?
        } else if let Some(v) = self.try_anonymous_value() {
            v
        } else {
            match self.parse_value() {
                Ok(v) => v,
                // A SOFT failure backtracks — the statement may still be a
                // ruleset (glued-pseudo heads like `a:hover when (…)`).
                Err(e) if e.message.is_empty() => {
                    self.cur.i = save;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
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
        if self.cur.src()[start..self.here()] == *"*" {
            // The IE star hack needs a NAME glued to the `*` — a lone `*` is
            // not a property (less.js `ruleProperty` requires `\*?[-\w…]+`;
            // property-asterisk-only-name).
            self.cur.i = start;
            return None;
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
    fn parse_custom_property_value(&mut self) -> Result<Node, LessError> {
        let save = self.here();
        if let Some(v) = self.try_custom_entities() {
            return Ok(v);
        }
        self.cur.i = save;
        self.parse_custom_property_raw()
    }

    /// The `permissiveValue` entity loop: `entity ([,] entity)*` reaching `;`/`}`.
    /// Block comments between entities are KEPT as `Comment` nodes (less.js
    /// preserves them in the value with a joining space — `--value: a/* c */;`
    /// renders `a /* c */`, review F6).
    fn try_custom_entities(&mut self) -> Option<Node> {
        let mut items: Vec<Node> = Vec::new();
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

    /// Raw custom-property capture up to `;`/`}` (balanced). A backslash
    /// escapes the next byte — `\'` inside the value must NOT open a string
    /// (less.js's permissive scanner honors escapes; review F5).
    ///
    /// less.js `$parseUntil` faithfulness (the custom-property-unmatched-block
    /// fixtures): a closer that does not match the innermost opener is
    /// `Expected '<matching closer>'` at the closer's position, and a quote
    /// with no closing mate on the same line is `Expected '<quote>'` at the
    /// OPENING quote (less.js quoted-string regexes never span newlines).
    fn parse_custom_property_raw(&mut self) -> Result<Node, LessError> {
        let start = self.here();
        let mut stack: Vec<u8> = Vec::new();
        while let Some(b) = self.cur.cur() {
            match b {
                b';' | b'}' if stack.is_empty() => break,
                b'\\' if self.cur.peek(1).is_some() => {
                    self.cur.bump();
                    self.cur.bump();
                }
                b'{' | b'(' | b'[' => {
                    stack.push(b);
                    self.cur.bump();
                }
                b'}' | b')' | b']' => {
                    let expected = match stack.last() {
                        Some(b'{') => b'}',
                        Some(b'(') => b')',
                        Some(b'[') => b']',
                        _ => b,
                    };
                    if expected != b {
                        return Err(self.err(format!("Expected '{}'", expected as char)));
                    }
                    stack.pop();
                    self.cur.bump();
                }
                b'"' | b'\'' => {
                    let quote = b;
                    let qpos = self.here();
                    let rest = &self.cur.rest()[1..];
                    let mut close = None;
                    let mut k = 0;
                    let bytes = rest.as_bytes();
                    while k < bytes.len() {
                        match bytes[k] {
                            b'\\' => k += 2,
                            b'\n' => break,
                            c if c == quote => {
                                close = Some(k);
                                break;
                            }
                            _ => k += 1,
                        }
                    }
                    match close {
                        Some(k) => self.cur.i = qpos + 1 + k + 1,
                        None => {
                            return Err(self.err_kind_at(
                                ErrorKind::Parse,
                                format!("Expected '{}'", quote as char),
                                qpos,
                            ))
                        }
                    }
                }
                _ => self.cur.bump(),
            }
        }
        Ok(Node::Anonymous(self.cur.src()[start..self.here()].trim().to_string()))
    }

    // -----------------------------------------------------------------------
    // Selectors → rulesets / mixin definitions / mixin calls
    // -----------------------------------------------------------------------

    fn parse_selector_statement(&mut self) -> Result<Node, LessError> {
        let start = self.here();
        let mut selectors = self.parse_selector_group()?;
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
                // The body extend statement `&:extend(target…);` (plan §2.8):
                // applies to every selector path of the enclosing ruleset.
                if selectors.len() == 1
                    && !selectors[0].extend_list.is_empty()
                    && selectors[0].elements.len() == 1
                    && selectors[0].elements[0].value == "&"
                {
                    self.cur.eat(b';');
                    return Ok(Node::ExtendRule(selectors.remove(0).extend_list));
                }
                // A bare mixin call: `.mixin;` / `.mixin(args);` / `.m() !important;`.
                // less.js `mixin.call` only matches `.`/`#` heads — anything
                // else here is unparseable input, reported at the FURTHEST
                // position reached (the terminator), like the real parser's
                // furthest-failure bookkeeping.
                let is_mixin_head = selectors
                    .first()
                    .and_then(|s| s.elements.first())
                    .map(|e| e.value.starts_with('.') || e.value.starts_with('#'))
                    .unwrap_or(false);
                if !is_mixin_head {
                    return Err(self.unrecognised(self.here()));
                }
                // less.js's args parser rejects mixing `;` and `,` argument
                // delimiters (mixed-mixin-definition-args-1/-2) — validate the
                // raw arg text with the same state machine before accepting.
                if let Some(lp) = self.cur.src()[start..self.here()].find('(') {
                    let inner_start = start + lp + 1;
                    let end = self.cur.src()[inner_start..self.here()]
                        .rfind(')')
                        .map(|r| inner_start + r)
                        .unwrap_or(self.here());
                    let raw = self.cur.src()[inner_start..end].to_string();
                    self.validate_arg_separators(inner_start, &raw)?;
                }
                self.cur.eat(b';');
                Ok(self.as_mixin_call(selectors, important, start))
            }
            _ => Err(self.unrecognised(self.here())),
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
            // less.js `parsers.selectors`: a guard is only allowed on a lone
            // selector — checked after EVERY selector parse (`s.condition &&
            // selectors.length > 1`) and again right after a comma when the
            // guarded selector comes first. Both are Syntax errors at the
            // current (whitespace-skipped) position.
            if selectors.len() > 1 && selectors.last().is_some_and(|s| s.guard.is_some()) {
                return Err(self.err_kind(
                    ErrorKind::Syntax,
                    "Guards are only currently allowed on a single selector.",
                ));
            }
            if self.cur.cur() == Some(b',') {
                self.cur.bump();
                self.cur.skip_trivia();
                if selectors.last().is_some_and(|s| s.guard.is_some()) {
                    return Err(self.err_kind(
                        ErrorKind::Syntax,
                        "Guards are only currently allowed on a single selector.",
                    ));
                }
                continue;
            }
            break;
        }
        Ok(selectors)
    }

    fn parse_selector(&mut self) -> Result<Selector, LessError> {
        let start = self.here();
        let mut elements = Vec::new();
        let mut extend_list: Vec<crate::ast::ExtendTarget> = Vec::new();
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
            // A trailing `:extend(…)` clause — with or without a leading
            // combinator (`.a:extend(.b)` ≡ `.a :extend(.b)`); several may
            // chain (`:extend(.a):extend(.b)`). Plan §2.8. less.js also parses
            // it FIRST in a selector (`extend()` is tried before elements) —
            // a selector that then never gains an element errors below.
            if self.cur.rest().starts_with(":extend(") {
                self.cur.eat_str(":extend(");
                extend_list.extend(self.parse_extend_targets()?);
                first = false;
                continue;
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
                    // less.js has no "expected a selector" — a failed selector
                    // alternative backtracks and the end-of-parse machinery
                    // reports `Unrecognised input` (+ char-derived suffix) at
                    // the furthest position (review F5).
                    return Err(self.unrecognised(self.here()));
                }
                break;
            }
            if !extend_list.is_empty() {
                // less.js: extend must be the last thing in the selector — a
                // Syntax `error(...)` at the current position, which sits
                // AFTER the auto-skipped whitespace (extend-not-at-end).
                self.cur.skip_trivia();
                return Err(self.err_kind(
                    ErrorKind::Syntax,
                    "Extend can only be used at the end of selector",
                ));
            }
            elements.push(Element {
                combinator,
                value,
                span: self.span(elem_start),
            });
            first = false;
        }
        // A selector that is ONLY `:extend(…)` clauses (no elements) is
        // less.js's "cannot be used on its own" Syntax error, raised at the
        // current position (after the clause + whitespace).
        if elements.is_empty() && !extend_list.is_empty() {
            return Err(self.err_kind(
                ErrorKind::Syntax,
                "Extend must be used to extend a selector, it cannot be used on its own",
            ));
        }
        // Optional guard.
        let guard = self.try_parse_guard()?;
        Ok(Selector {
            elements,
            guard,
            extend_list,
            span: self.span(start),
        })
    }

    /// Parse the comma-separated target list of one `:extend(…)` clause — the
    /// opening `(` already consumed; consumes through the closing `)`. Each
    /// target is a selector-element run with an optional trailing `all`.
    fn parse_extend_targets(&mut self) -> Result<Vec<crate::ast::ExtendTarget>, LessError> {
        let mut targets = Vec::new();
        loop {
            self.cur.skip_trivia();
            let mut elements: Vec<Element> = Vec::new();
            let mut all = false;
            let mut first = true;
            loop {
                let ws = self.cur.skip_trivia();
                match self.cur.cur() {
                    None | Some(b')') | Some(b',') => break,
                    _ => {}
                }
                // The `all` option keyword — only just before `)` / `,`.
                // less.js `/^(!?all)(?=\s*(\)|,))/`: `!all` is a synonym.
                let opt_len = if self.cur.rest().starts_with("!all") {
                    4
                } else if self.cur.rest().starts_with("all") {
                    3
                } else {
                    0
                };
                if opt_len > 0
                    && matches!(
                        self.cur.rest()[opt_len..].trim_start().as_bytes().first(),
                        None | Some(b')') | Some(b',')
                    )
                {
                    for _ in 0..opt_len {
                        self.cur.bump();
                    }
                    all = true;
                    continue;
                }
                let combinator = self.scan_combinator(first, if first { false } else { ws });
                self.cur.skip_whitespace();
                let elem_start = self.here();
                let value = self.scan_element_value();
                if value.is_empty() {
                    break;
                }
                elements.push(Element {
                    combinator,
                    value,
                    span: self.span(elem_start),
                });
                first = false;
            }
            if elements.is_empty() {
                // less.js `extend()`: `error('Missing target selector for
                // :extend().')` — Syntax kind (error()'s default), period
                // included (review F14).
                return Err(self.err_kind(
                    ErrorKind::Syntax,
                    "Missing target selector for :extend().",
                ));
            }
            targets.push(crate::ast::ExtendTarget { elements, all });
            self.cur.skip_trivia();
            match self.cur.cur() {
                Some(b',') => {
                    self.cur.bump();
                    continue;
                }
                Some(b')') => {
                    self.cur.bump();
                    break;
                }
                _ => return Err(self.err("expected ')' or ',' in :extend")),
            }
        }
        Ok(targets)
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
                // `:extend(…)` is never part of an element — it ends the
                // compound so the selector parser can claim it (plan §2.8).
                Some(b':') if self.cur.rest().starts_with(":extend(") => break,
                // A `:` OPENING an element without pseudo-name material ends
                // the selector (`* : 1;` — less.js's element() fails at the
                // bare colon and the statement dies `Unrecognised input`
                // there). Mid-element (`color:`) the colon is consumed as
                // before — the glued-declaration head shape.
                Some(b':') if self.here() == start
                    && !matches!(self.cur.peek(1), Some(n) if n == b':' || n == b'-'
                        || n == b'_' || n == b'\\' || n == b'@' || n.is_ascii_alphanumeric()
                        || n >= 0x80) =>
                {
                    break;
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

    /// Consume a balanced `open … close` region (handles nesting, strings, and
    /// comments — an apostrophe inside `// isn't possible` must not start a
    /// string scan that swallows a closing bracket).
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
            if self.cur.at_line_comment() || self.cur.at_block_comment() {
                self.cur.scan_comment();
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
        // less.js `expect(this.conditions, 'expected condition')`: a guard
        // condition must open with `(` or `not` (mixins-guards-cond-expected).
        if self.cur.cur() != Some(b'(')
            && !(self.cur.rest().starts_with("not")
                && matches!(self.cur.peek(3), Some(b) if b.is_ascii_whitespace() || b == b'('))
        {
            return Err(self.err_kind(ErrorKind::Syntax, "expected condition"));
        }
        let gs = self.here();
        // The guard runs up to the block/terminator, balancing parens + strings.
        let mut depth = 0i32;
        while let Some(b) = self.cur.cur() {
            // less.js `conditions()` continues past a comma ONLY when it opens
            // another parenthesized/`not(` condition (`peek(/^,\s*(not\s*)?\(/)`)
            // — otherwise the comma belongs to the SELECTOR GROUP and the
            // guards-on-multiple-selectors error fires there.
            if b == b',' && depth == 0 {
                let rest = &self.cur.rest()[1..];
                let t = rest.trim_start();
                let continues = t.starts_with('(')
                    || (t.starts_with("not") && t[3..].trim_start().starts_with('('));
                if !continues {
                    break;
                }
            }
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
        // consume `url` `(` … `)` — less.js `entities.url`: the content is a
        // quoted string, a bare `@variable` (evaluated! F7-review), or the raw
        // run `/^(?:(?:\\[()'"])|[^()'"])+/` — whitespace INSIDE is kept (only
        // the post-`url(` skip trims leading spaces), and a `(` in the raw run
        // (e.g. `url(fn(x))`) leaves the required `)` unmatched → parse error,
        // exactly like less.js.
        self.cur.eat_str("url");
        self.cur.skip_whitespace();
        if !self.cur.eat(b'(') {
            return Err(self.err("expected '(' after url"));
        }
        self.cur.skip_whitespace();
        // An unterminated quote falls through to the raw run, which stops AT
        // the quote — so `expectChar(')')` then fails there (`expected ')'
        // got '"'`), exactly less.js's cascade.
        let quoted = if matches!(self.cur.cur(), Some(b'"') | Some(b'\'') | Some(b'~')) {
            self.parse_quoted()
        } else {
            None
        };
        let inner = if let Some(q) = quoted {
            q
        } else if self.cur.cur() == Some(b'@') && self.cur.peek(1) != Some(b'{') {
            // entities.variable: `@@?[\w-]+` — resolved at eval time. (The
            // interpolated `@{a}` form stays a raw Anonymous, like less.js.)
            let start = self.here();
            self.cur.bump();
            if self.cur.cur() == Some(b'@') {
                self.cur.bump();
            }
            let nstart = self.here();
            while matches!(self.cur.cur(), Some(b) if b == b'-' || b == b'_'
                || b.is_ascii_alphanumeric())
            {
                self.cur.bump();
            }
            if self.here() == nstart {
                self.cur.i = start;
                Node::Anonymous(String::new())
            } else if self.cur.src()[start..].starts_with("@@") {
                Node::VariableVariable {
                    name: self.cur.src()[start + 2..self.here()].to_string(),
                    span: self.span(start),
                }
            } else {
                Node::Variable {
                    name: self.cur.src()[start + 1..self.here()].to_string(),
                    span: self.span(start),
                }
            }
        } else {
            let s = self.here();
            while let Some(b) = self.cur.cur() {
                match b {
                    b'\\' if matches!(
                        self.cur.peek(1),
                        Some(b'(') | Some(b')') | Some(b'\'') | Some(b'"')
                    ) =>
                    {
                        self.cur.bump();
                        self.cur.bump();
                    }
                    b'(' | b')' | b'\'' | b'"' => break,
                    _ => self.cur.bump(),
                }
            }
            Node::Anonymous(self.cur.src()[s..self.here()].to_string())
        };
        if !matches!(inner, Node::Anonymous(_)) {
            // Token parsers skip trailing whitespace in less.js; the raw run
            // consumed it into the value instead (`url(spaced.png  )` keeps it).
            self.cur.skip_whitespace();
        }
        // less.js `expectChar(')')` → `error(…)` — SYNTAX kind by default,
        // and EOF renders as `got ''` (empty currentChar), review F12.
        match self.cur.cur() {
            Some(b')') => {
                self.cur.bump();
            }
            Some(c) => {
                return Err(self.err_kind(
                    ErrorKind::Syntax,
                    format!("expected ')' got '{}'", c as char),
                ));
            }
            None => return Err(self.err_kind(ErrorKind::Syntax, "expected ')' got ''")),
        }
        Ok(Node::Url(Box::new(inner)))
    }

    /// Parse a quoted string (`"…"`, `'…'`, `~"…"`). `None` (cursor restored)
    /// when the string is UNTERMINATED — less.js's `$quoted` requires the
    /// closing quote before any bare `\r`/`\n` or EOF (a `\` escapes even a
    /// newline), so an open quote fails the alternative instead of swallowing
    /// the rest of the file (review F6/F11).
    fn parse_quoted(&mut self) -> Option<Node> {
        let save = self.here();
        let escaped = self.cur.eat(b'~');
        let bytes = self.cur.src().as_bytes();
        let qpos = self.here();
        let quote_b = match bytes.get(qpos) {
            Some(&b @ (b'"' | b'\'')) => b,
            _ => {
                self.cur.i = save;
                return None;
            }
        };
        let mut i = qpos + 1;
        let terminated = loop {
            match bytes.get(i) {
                None | Some(b'\r') | Some(b'\n') => break false,
                Some(b'\\') => i += 2,
                Some(&b) if b == quote_b => break true,
                Some(_) => i += 1,
            }
        };
        if !terminated {
            self.cur.i = save;
            return None;
        }
        let raw = self.cur.scan_string();
        let quote = raw.chars().next().unwrap_or('"');
        let inner = &raw[1..raw.len().saturating_sub(1)];
        Some(Node::Quoted {
            escaped,
            quote,
            value: inner.to_string(),
        })
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
                // Only whitespace — a block comment after the comma belongs to
                // the NEXT expression and renders (`grey, /* blue */ orange`).
                self.cur.skip_whitespace();
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
            // A bracket run GLUED to the item (`input[type=text]`) joins it
            // verbatim — less.js keeps such attribute-ish tails attached, so
            // value rendering and selector re-parse stay unspaced
            // (parse-interpolation).
            while self.here() > before
                && self.cur.cur() == Some(b'[')
                // Only a keyword-shaped bracket run is absorbed (less.js's
                // keyword regex needs a word char after `[`): `['key']` /
                // `[@{k}]` after a failed lookup stay Unrecognised input.
                && matches!(self.cur.peek(1), Some(b) if b == b'-' || b == b'_'
                    || b == b'\\' || b == b'*' || b == b'=' || b.is_ascii_alphanumeric() || b >= 0x80)
            {
                let bs = self.here();
                self.skip_balanced(b'[', b']');
                let txt = self.cur.src()[bs..self.here()].to_string();
                match items.last_mut() {
                    Some(Node::Keyword(k)) => k.push_str(&txt),
                    Some(Node::Anonymous(a)) => a.push_str(&txt),
                    _ => items.push(Node::Anonymous(txt)),
                }
            }
            if self.here() == before {
                // An arithmetic operator stranded after a quoted string is
                // less.js's hard `Unrecognised input` ParseError (F13:
                // `b: "x" + "y"` — quoted literals are not operands).
                if matches!(self.cur.cur(), Some(b'+') | Some(b'*'))
                    && matches!(
                        items.iter().rev().find(|n| !matches!(n, Node::Comment { .. })),
                        Some(Node::Quoted { escaped: false, .. })
                    )
                {
                    return Err(self.err("Unrecognised input"));
                }
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
            // The legacy forced-division operator `./` (less.js `$str('./')`).
            let is_legacy_div = op == Some(b'.') && self.cur.peek(1) == Some(b'/');
            if op == Some(b'*') || is_div || is_legacy_div {
                // less.js `operand()` accepts dimensions/colors/variables/
                // calls/subs — NOT plain keywords (`small/20px` is entities,
                // not an Operation, so it renders spaced; math-strict css).
                if !operand_like(&left) {
                    self.cur.i = save;
                    break;
                }
                let op_str = if is_legacy_div {
                    self.cur.bump();
                    self.cur.bump();
                    "./".to_string()
                } else {
                    self.cur.bump();
                    (op.unwrap() as char).to_string()
                };
                let ws_after = self.cur.skip_whitespace();
                let before = self.here();
                let right = self.parse_operand()?;
                if self.here() == before || !operand_like(&right) {
                    self.cur.i = save; // no right operand — leave the operator alone
                    break;
                }
                let mut right = right;
                mark_in_op(&mut left);
                mark_in_op(&mut right);
                left = Node::Operation {
                    op: op_str,
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
        let ent_start = self.here();
        match self.cur.cur() {
            Some(b'"') | Some(b'\'') => match self.parse_quoted() {
                Some(n) => Ok(n),
                // Unterminated: the alternative fails, recording the quote as
                // the furthest position — end-of-parse then reports
                // `Unrecognised input` AT the quote (review F6, less.js
                // furthest tracking).
                None => Err(self.soft_fail_pos(self.here())),
            },
            Some(b'~') if matches!(self.cur.peek(1), Some(b'"') | Some(b'\'')) => {
                match self.parse_quoted() {
                    Some(n) => Ok(n),
                    // less.js consumed the `~` before `$quoted` failed, so
                    // furthest sits one past it.
                    None => Err(self.soft_fail_pos(self.here() + 1)),
                }
            }
            // Inline JavaScript backticks (plan §8): parsed far enough to
            // raise the precise disabled-JS error at EVAL (§C-jserr) — the
            // backtick body is captured verbatim, never executed.
            Some(b'`') => Ok(self.parse_javascript(false, ent_start)),
            Some(b'~') if self.cur.peek(1) == Some(b'`') => {
                self.cur.bump();
                Ok(self.parse_javascript(true, ent_start))
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
                Ok(Node::Call { name, args, span: self.span(ent_start) })
            }
            Some(b'(') => {
                // less.js `sub` = `'(' addition ')'` — a SINGLE chain, parsed
                // STRICT-FIRST so an inner paren's failure is the furthest
                // candidate (parens-error-2/3) while a list after the chain
                // fails at the outer stop (parens-error-1: less.js never even
                // reaches the inner paren there).
                let psave = self.here();
                self.cur.bump();
                self.skip_value_trivia();
                let inner = self.parse_addition()?;
                self.skip_value_trivia();
                if self.cur.eat(b')') {
                    return Ok(Node::Paren { inner: Box::new(inner), in_op: false });
                }
                let at = self.here();
                // A declaration-shaped paren (`(min-width: @val)`) is not an
                // expression — keep it VERBATIM like less.js's permissive
                // capture, so the parens and `:` survive re-rendering
                // (namespacing-media).
                self.cur.i = psave;
                let vs = self.here();
                self.skip_balanced(b'(', b')');
                let text = self.cur.src()[vs..self.here()].to_string();
                // The verbatim capture still obeys less.js `$parseUntil`
                // bracket/quote pairing — `({ …: [ })` is `Expected ']'` at
                // the mismatched closer, never a silent capture
                // (custom-property-unmatched-block-1).
                self.validate_brackets(vs, &text)?;
                if paren_text_has_colon(&text) {
                    return Ok(Node::Anonymous(text));
                }
                // SOFT: another statement alternative may still parse this
                // (`a:hover when (2 = true) { }` — the declaration try fails
                // here but the ruleset succeeds, like less.js's restore).
                self.cur.i = at;
                Err(self.soft_fail("Expected ')'", at))
            }
            Some(b'[') => {
                // A bracketed value token (`[line-name]` in grid) — captured
                // verbatim as one entity. less.js's keyword regex
                // `\[?([\w-]|escape)+\]?` needs at least one word char after
                // the bracket, so `['key']` / `[@{k}]` are Unrecognised input
                // (the stray-lookup family, verified 4.6.7).
                if !matches!(self.cur.peek(1), Some(b) if b == b'-' || b == b'_'
                    || b == b'\\' || b.is_ascii_alphanumeric() || b >= 0x80)
                {
                    return Err(self.err("Unrecognised input"));
                }
                let s = self.here();
                self.skip_balanced(b'[', b']');
                Ok(Node::Anonymous(self.cur.src()[s..self.here()].to_string()))
            }
            Some(b'#') => {
                // A namespace/mixin call value (`#ns.mx(4)[result]`, `#ns[key]`)
                // takes precedence when the shape fits (plan §2.12)…
                if let Some(m) = self.try_mixin_call_arg()? {
                    return Ok(m);
                }
                // …else a hex color. An invalid hex literal (`#ggg`, wrong
                // length) is less.js's `Unrecognised input` ParseError (C18).
                let s = self.here();
                self.cur.bump();
                self.cur.scan_ident();
                let text = self.cur.src()[s..self.here()].to_string();
                match crate::color::Color::from_hex(&text) {
                    Some(color) => Ok(Node::Color(color)),
                    // SOFT: the declaration try backtracks and the statement
                    // fails later at its furthest point — less.js reports
                    // `Unrecognised input` at the post-comment `;`, not at the
                    // bad literal (invalid-color-with-comment).
                    None => Err(LessError::new(ErrorKind::Parse, "")),
                }
            }
            Some(b'.') if matches!(self.cur.peek(1), Some(b) if b == b'-' || b == b'_'
                || b.is_ascii_alphabetic()) =>
            {
                // A mixin call in value position (`.mk-map()`, `.m()[key]` —
                // less.js `mixinLookup`, plan §2.12). Backtracks to the stray-
                // punctuation arm when the shape doesn't fit.
                match self.try_mixin_call_arg()? {
                    Some(m) => Ok(m),
                    None => {
                        // The whole `.ident` run is one token (`@classes: .a,
                        // .b` interpolates as class selectors — never `. a`).
                        let s = self.here();
                        self.cur.bump();
                        self.cur.scan_ident();
                        Ok(Node::Anonymous(self.cur.src()[s..self.here()].to_string()))
                    }
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
                // `@name()` / `@name[...]` — a variable (detached-ruleset) call
                // and/or a lookup chain (less.js `variableCall`, plan §2.12).
                // The `(`/`[` must follow the name with NO whitespace.
                if self.cur.cur() == Some(b'(') {
                    let save = self.here();
                    self.cur.bump();
                    self.cur.skip_whitespace();
                    if self.cur.eat(b')') {
                        // A value-position variable call with parens is ALWAYS
                        // rejected — with or without a lookup chain. less.js
                        // `variableCall(parsedName)`: the parsed name is the
                        // bare `@dr` string, so `name[2] !== '()'` holds and
                        // the saved error surfaces (`@dr()`/`@dr()[k]` both
                        // parse-error; only `@dr[k]` is legal in value
                        // position, verified 4.6.7).
                        return Err(self.err("Missing '[...]' lookup in variable call"));
                    }
                    self.cur.i = save;
                } else if self.cur.cur() == Some(b'[') {
                    let lb = self.here();
                    if let Some(keys) = self.try_rule_lookups() {
                        return Ok(Node::Lookup {
                            target: Box::new(Node::VariableCall {
                                name: name.clone(),
                                span: self.span(start),
                            }),
                            keys,
                            // less.js's value-position `variableCall(parsedName)`
                            // captures `i` AFTER the consumed `@name` — the
                            // NamespaceValue (and its not-found errors) anchor
                            // at the `[` (namespacing-2/-4).
                            span: self.span(lb),
                        });
                    }
                }
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
                    // less.js `customFuncCall`: an `alpha(` call first tries the
                    // IE `ieAlpha` grammar (`opacity=<digits>` / `opacity=@var`,
                    // case-normalized to lowercase); on a regex miss it falls
                    // back to normal arguments (plan §2.17, review C14/C15/F9).
                    if ident.eq_ignore_ascii_case("alpha") {
                        if let Some(q) = self.try_ie_alpha()? {
                            return Ok(q);
                        }
                    }
                    let args = self.parse_call_args()?;
                    self.skip_value_trivia();
                    self.cur.eat(b')');
                    return Ok(Node::Call { name: ident, args, span: self.span(ent_start) });
                }
                Ok(Node::Keyword(ident))
            }
            // A bare `%` is a KEYWORD in less.js (`/^%|…/`) whose eval throws
            // `Invalid % without number` (percentage-missing-space).
            Some(b'%') => {
                self.cur.bump();
                Ok(Node::Keyword("%".to_string()))
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
            } else if let Some(m) = self.try_mixin_call_arg()? {
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

    /// A mixin CALL in function-argument position (`each(.set-2(), …)` — the
    /// less.js `arguments()` chain routes these through `entities.mixinLookup`,
    /// review F3-residual): `[.#]name` segments then `(args)`, NOT followed by
    /// `{` (that form is an anonymous mixin, tried earlier). Backtracks on any
    /// non-fit (`.5em` starts with `.` but has no parens).
    fn try_mixin_call_arg(&mut self) -> Result<Option<Node>, LessError> {
        if !matches!(self.cur.cur(), Some(b'.') | Some(b'#')) {
            return Ok(None);
        }
        let start = self.here();
        let mut path: Vec<Element> = Vec::new();
        loop {
            // Between segments less.js's `elements()` allows a `>` child
            // combinator or descendant whitespace (`#ns > .m()[k]`,
            // `#ns .m()[k]` — verified 4.6.7 in value position). Only commit
            // the skip when a real `.`/`#` segment follows.
            let seg_save = self.here();
            let mut combinator = String::new();
            if !path.is_empty() {
                let had_ws = {
                    let before = self.here();
                    self.cur.skip_whitespace();
                    self.here() > before
                };
                if self.cur.cur() == Some(b'>') {
                    self.cur.bump();
                    self.cur.skip_whitespace();
                    combinator = ">".to_string();
                } else if had_ws {
                    combinator = " ".to_string();
                }
            }
            if !matches!(self.cur.cur(), Some(b'.') | Some(b'#')) {
                self.cur.i = seg_save;
                break;
            }
            let seg_start = self.here();
            self.cur.bump();
            let nstart = self.here();
            while matches!(self.cur.cur(), Some(b) if b == b'-' || b == b'_'
                || b.is_ascii_alphanumeric())
            {
                self.cur.bump();
            }
            if self.here() == nstart {
                if path.is_empty() {
                    self.cur.i = start;
                    return Ok(None);
                }
                self.cur.i = seg_save;
                break;
            }
            path.push(Element {
                combinator,
                value: self.cur.src()[seg_start..self.here()].to_string(),
                span: self.span(seg_start),
            });
        }
        if path.is_empty() {
            self.cur.i = start;
            return Ok(None);
        }
        // Optional `(args)` — a lookup-only form (`#ns[key]`) has none.
        let mut args = Vec::new();
        let has_parens = self.cur.cur() == Some(b'(');
        if has_parens {
            let params_start = self.here();
            self.skip_balanced(b'(', b')');
            let args_src =
                self.cur.src()[params_start + 1..self.here().saturating_sub(1)].to_string();
            args = parse_mixin_args(&args_src);
        }
        // Optional `[key]` lookups (plan §2.12). A value-position mixin call
        // needs parens or lookups (less.js `inValue` rule).
        let keys = self.try_rule_lookups();
        if !has_parens && keys.is_none() {
            self.cur.i = start;
            return Ok(None);
        }
        let call = Node::MixinCall(MixinCall {
            path,
            args,
            important: false,
            span: self.span(start),
        });
        Ok(Some(match keys {
            Some(keys) => Node::Lookup {
                target: Box::new(call),
                keys,
                span: self.span(start),
            },
            None => call,
        }))
    }

    /// less.js `parsers.ieAlpha`, tried when an `alpha(` call opens (§2.17).
    /// Cursor sits just after the `(`. A `/^opacity=/i` miss backtracks to a
    /// normal argument parse (`alpha(0.5)`, `alpha(opacity = 87)` — the spaced
    /// form is NOT ieAlpha and later hits the color function's toHSL error);
    /// a hit commits: `<digits>` or `@var` (→ `@{var}` interpolation), then a
    /// REQUIRED `)` — anything else is less.js's propagated parse error
    /// (`alpha(opacity=87.5)` → `expected ')' got '.'`; `alpha(opacity=)` /
    /// `alpha(opacity=@{v})` → `Could not parse alpha`). The result is the
    /// lowercase-normalized escaped `alpha(opacity=…)` literal.
    fn try_ie_alpha(&mut self) -> Result<Option<Node>, LessError> {
        let save = self.here();
        self.skip_value_trivia();
        let at = self.here();
        let rest = &self.cur.src()[at..];
        if rest.len() < 8 || !rest[..8].eq_ignore_ascii_case("opacity=") {
            self.cur.i = save;
            return Ok(None);
        }
        self.cur.i = at + 8;
        self.skip_value_trivia(); // less.js skips trivia after every token match
        let dstart = self.here();
        while matches!(self.cur.cur(), Some(b) if b.is_ascii_digit()) {
            self.cur.bump();
        }
        let value = if self.here() > dstart {
            self.cur.src()[dstart..self.here()].to_string()
        } else if self.cur.cur() == Some(b'@') {
            // `expect(entities.variable)`: `@@?[\w-]+`, then re-emitted as the
            // interpolation form `@{name}` (name = the match minus one `@`).
            let vstart = self.here();
            self.cur.bump();
            if self.cur.cur() == Some(b'@') {
                self.cur.bump();
            }
            let nstart = self.here();
            while matches!(self.cur.cur(), Some(b) if b == b'-' || b == b'_'
                || b.is_ascii_alphanumeric())
            {
                self.cur.bump();
            }
            if self.here() == nstart {
                return Err(self.err("Could not parse alpha"));
            }
            let full = &self.cur.src()[vstart..self.here()];
            format!("@{{{}}}", &full[1..])
        } else {
            return Err(self.err("Could not parse alpha"));
        };
        self.skip_value_trivia();
        // `expectChar(')')` — Syntax kind, `got ''` at EOF (review F12).
        match self.cur.cur() {
            Some(b')') => {
                self.cur.bump();
            }
            Some(c) => {
                return Err(self.err_kind(
                    ErrorKind::Syntax,
                    format!("expected ')' got '{}'", c as char),
                ));
            }
            None => return Err(self.err_kind(ErrorKind::Syntax, "expected ')' got ''")),
        }
        Ok(Some(Node::Quoted {
            escaped: true,
            quote: '"',
            value: format!("alpha(opacity={value})"),
        }))
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
/// Every comment relocates, once, in order — the `@import` feature list
/// (less.js flushes its commentStore as root siblings after the import).
fn split_prelude_comments_all(s: &str) -> (String, Vec<String>) {
    split_prelude_comments_mode(s, PreludeComments::All)
}

/// Which comments of a prelude relocate (probed vs 4.6.7, review C1/C1c).
#[derive(Clone, Copy, PartialEq)]
enum PreludeComments {
    /// Generic at-rule: only a comment directly before a `,` / the end.
    AtRule,
    /// `@media`/`@container` (less.js `mediaFeatures` commentStore quirks):
    /// additionally a LEADING comment lands in the block TWICE (the feature
    /// scan passes it twice) and a comment right after a query `,` once.
    Media,
    /// `@import` features: every comment, once.
    All,
}

fn split_prelude_comments_mode(s: &str, mode: PreludeComments) -> (String, Vec<String>) {
    let mut clean = String::with_capacity(s.len());
    let mut comments = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    // Tracks the last non-ws byte BEFORE the current position (None = start).
    let mut prev_sig: Option<u8> = None;
    while i < b.len() {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                let end = s[i + 2..].find("*/").map(|e| i + 2 + e + 2).unwrap_or(b.len());
                // A comment directly before a `,` (or the prelude's end)
                // relocates into the block (the less.js commentStore
                // behavior). In media preludes, so does one at the START
                // (twice) or right after a `,` (once); a truly mid-query
                // comment embeds in the feature value — which we drop (only
                // renderable inside a header, a case no default fixture
                // exercises).
                let mut j = end;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let text = &s[i..end];
                if mode == PreludeComments::Media && prev_sig.is_none() {
                    comments.push(text.to_string());
                    comments.push(text.to_string());
                } else if j >= b.len() || b[j] == b',' {
                    comments.push(text.to_string());
                } else if mode == PreludeComments::Media && prev_sig == Some(b',') {
                    comments.push(text.to_string());
                } else if mode == PreludeComments::All {
                    comments.push(text.to_string());
                }
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
                prev_sig = Some(q);
            }
            _ => {
                let ch_len = utf8_char_len(b[i]);
                if !b[i].is_ascii_whitespace() {
                    prev_sig = Some(b[i]);
                }
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
/// Whether a parsed value contains a stalled `:` item — the signature of a
/// declaration-shaped parenthesized capture (`(min-width: 480px)`).
/// A `: ` inside balanced paren text (outside quotes) — the declaration-shaped
/// paren fallback's trigger (`(min-width: @val)`).
fn paren_text_has_colon(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
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
            b':' => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Remove `/* … */` and `// …` comments from a raw mixin param/argument list
/// (less.js throws its `commentStore` away between args — the comments/comments
/// `.mixin_def_with_colors(@a: white, // in` case). String-aware; a `//`
/// directly after `:` is a URL protocol, not a comment.
fn strip_param_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                let q = b[i];
                let start = i;
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                out.push_str(&src[start..i]);
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b'/' if i + 1 < b.len()
                && b[i + 1] == b'/'
                && out.trim_end().as_bytes().last() != Some(&b':') =>
            {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Whether a parsed node is a valid less.js `operand()` for `*`/`/` (plan
/// §2.4): dimension, color (hex or NAMED keyword), variable/property accessor,
/// call, parenthesized sub, negation, lookup — a plain keyword (`small`) or
/// raw run is NOT, so `small/20px` stays an entity list.
fn operand_like(n: &Node) -> bool {
    match n {
        Node::Dimension(_)
        | Node::Color(_)
        | Node::Variable { .. }
        | Node::VariableVariable { .. }
        | Node::PropertyAccessor { .. }
        | Node::Call { .. }
        | Node::Paren { .. }
        | Node::Negative(_)
        | Node::Lookup { .. }
        | Node::VariableCall { .. }
        | Node::MixinCall(_)
        | Node::Operation { .. } => true,
        Node::Quoted { escaped, .. } => *escaped,
        Node::Keyword(k) => crate::data::colors::named_color(k).is_some(),
        _ => false,
    }
}

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
        // A `.name( … )` element (parens attached to the name). A `:` in the
        // head is a pseudo-class (`.foo:not(…)`), never a mixin name — less.js
        // mixin names are `[#.][\w-]+` runs (selectors fixture).
        if let Some(p) = v.find('(') {
            if v.ends_with(')') && !v[..p].contains(':') {
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
    let src = &strip_param_comments(src);
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
    let src = &strip_param_comments(src);
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
        assert!(matches!(d0.value.as_ref(), Node::Call { name, args, .. } if name == "rgba" && args.len() == 4));
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
