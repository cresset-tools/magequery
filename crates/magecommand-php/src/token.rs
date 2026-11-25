//! A token stream shaped like PHP's `token_get_all`, with whitespace and
//! comments dropped — for the few places that must read a method BODY rather
//! than skip it (Mage-OS 4.0's constructor side-effect scan).
//!
//! It distinguishes only the token kinds those readers branch on; every other
//! keyword and operator is [`Tok::Other`], which a fail-closed reader rejects.
//! Interpolating strings, backtick strings and heredocs yield their inner
//! tokens as PHP does — `"a {$this->m()}"` is `"`, a text run,
//! [`Tok::CurlyOpen`], `$this`, `->`, `m`, `(`, `)`, `}`, `"` — so a reader
//! looking for calls sees the ones written inside strings. `?>` and the HTML
//! after it are not modelled.

use std::collections::VecDeque;

use crate::scan::Cursor;

/// The token kinds a body reader branches on. Names follow PHP's `T_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    /// `T_VARIABLE`: `$name`.
    Variable,
    /// `T_STRING`: an unqualified name that is not a keyword (`foo`, `true`,
    /// `parent`, `self`), and any name after `->` / `?->`.
    Str,
    /// `T_NAME_QUALIFIED`: `A\B`.
    NameQualified,
    /// `T_NAME_FULLY_QUALIFIED`: `\A\B`.
    NameFullyQualified,
    /// `T_NAME_RELATIVE`: `namespace\A`.
    NameRelative,
    /// `T_LNUMBER` / `T_DNUMBER`.
    Number,
    /// `T_CONSTANT_ENCAPSED_STRING` and the magic constants PHP lexes as
    /// their own tokens (`__LINE__`, `__CLASS__`, …; not `__PROPERTY__`).
    Literal,
    /// `(int)`, `(integer)`, `(float)`, `(double)`, `(string)`, `(binary)`,
    /// `(array)`, `(bool)`, `(boolean)`. `(object)` and `(unset)` are other.
    Cast,
    /// `->`
    ObjectOperator,
    /// `?->`
    NullsafeObjectOperator,
    /// `::`
    DoubleColon,
    /// `??`
    Coalesce,
    /// `??=`
    CoalesceEqual,
    /// `=>`
    DoubleArrow,
    /// `...`
    Ellipsis,
    Function,
    New,
    Array,
    Static,
    Class,
    Use,
    As,
    Trait,
    Interface,
    Enum,
    /// A single-character token: `(`, `)`, `[`, `]`, `{`, `}`, `;`, `,`, `=`,
    /// `?`, `:`, `-`, `+`, `&`, `!`, …
    Char(u8),
    /// `T_CURLY_OPEN`: the `{` of `{$…}` inside a string.
    CurlyOpen,
    /// `T_DOLLAR_OPEN_CURLY_BRACES`: `${` inside a string.
    DollarOpenCurlyBraces,
    /// `T_ENCAPSED_AND_WHITESPACE`: literal text inside an interpolating
    /// string, backtick string or heredoc (all of a nowdoc's body).
    EncapsedAndWhitespace,
    /// `T_START_HEREDOC`: `<<<LABEL` and its newline.
    StartHeredoc,
    /// `T_END_HEREDOC`: the closing label.
    EndHeredoc,
    /// Anything else: another keyword, a multi-character operator, `#[`.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: Tok,
    pub text: &'a str,
    /// Byte offset of the token in the source.
    pub offset: usize,
}

/// Reserved words PHP lexes as their own token, other than the ones [`Tok`]
/// names. Lowercase. (`true`/`false`/`null`/`self`/`parent` and type names
/// are plain names.)
const OTHER_KEYWORDS: &[&str] = &[
    "abstract", "and", "break", "callable", "case", "catch", "clone", "const", "continue",
    "declare", "default", "die", "do", "echo", "else", "elseif", "empty", "enddeclare", "endfor",
    "endforeach", "endif", "endswitch", "endwhile", "eval", "exit", "extends", "final", "finally",
    "fn", "for", "foreach", "global", "goto", "if", "implements", "include", "include_once",
    "instanceof", "insteadof", "isset", "list", "match", "namespace", "or", "print", "private",
    "protected", "public", "readonly", "require", "require_once", "return", "switch", "throw",
    "try", "unset", "var", "while", "xor", "yield", "__halt_compiler", "__property__",
];

const MAGIC_CONSTANTS: &[&str] = &[
    "__line__", "__file__", "__dir__", "__class__", "__trait__", "__method__", "__function__",
    "__namespace__",
];

/// Multi-character operators, longest first within a shared prefix.
const OPERATORS: &[(&str, Tok)] = &[
    ("??=", Tok::CoalesceEqual),
    ("?->", Tok::NullsafeObjectOperator),
    ("??", Tok::Coalesce),
    ("->", Tok::ObjectOperator),
    ("::", Tok::DoubleColon),
    ("=>", Tok::DoubleArrow),
    ("...", Tok::Ellipsis),
    ("<<=", Tok::Other),
    (">>=", Tok::Other),
    ("**=", Tok::Other),
    ("===", Tok::Other),
    ("!==", Tok::Other),
    ("<=>", Tok::Other),
    ("==", Tok::Other),
    ("!=", Tok::Other),
    ("<>", Tok::Other),
    ("<=", Tok::Other),
    (">=", Tok::Other),
    ("<<", Tok::Other),
    (">>", Tok::Other),
    ("++", Tok::Other),
    ("--", Tok::Other),
    ("+=", Tok::Other),
    ("-=", Tok::Other),
    ("*=", Tok::Other),
    ("/=", Tok::Other),
    (".=", Tok::Other),
    ("%=", Tok::Other),
    ("&=", Tok::Other),
    ("|=", Tok::Other),
    ("^=", Tok::Other),
    ("**", Tok::Other),
    ("&&", Tok::Other),
    ("||", Tok::Other),
    ("#[", Tok::Other),
    ("?>", Tok::Other),
];

/// Tokens of `src` from byte `start`, lazily. Start inside PHP code.
pub fn tokens(src: &[u8], start: usize) -> Tokens<'_> {
    let mut cur = Cursor::new(src);
    cur.pos = start.min(src.len());
    Tokens { cur, after_arrow: false, frames: Vec::new(), pending: VecDeque::new() }
}

pub struct Tokens<'a> {
    cur: Cursor<'a>,
    /// The previous token was `->` / `?->`: a following name is `T_STRING`
    /// whatever it spells.
    after_arrow: bool,
    /// Strings and `{$…}` islands open at the cursor, innermost last.
    frames: Vec<Frame>,
    /// Tokens already lexed past the cursor (simple interpolation's
    /// `[key]` / `->prop`, a nowdoc's body and end).
    pending: VecDeque<Token<'a>>,
}

enum Frame {
    /// Inside a string body, which `close` ends.
    Str(Close),
    /// Code inside `{$…}` or `${…}`; `depth` counts the `{` opened within.
    Island { depth: usize },
}

enum Close {
    Quote(u8),
    Heredoc(Vec<u8>),
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic() || b >= 0x80
}

fn is_ident(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80
}

impl<'a> Tokens<'a> {
    fn text(&self, start: usize) -> &'a str {
        std::str::from_utf8(&self.cur.src[start..self.cur.pos]).unwrap_or("")
    }

    /// At an identifier start: a name, possibly qualified.
    fn name(&mut self, start: usize) -> Tok {
        self.cur.read_ident();
        let first_end = self.cur.pos;
        let mut qualified = false;
        while self.cur.peek() == Some(b'\\') && self.cur.peek_at(1).is_some_and(is_ident_start) {
            self.cur.bump();
            self.cur.read_ident();
            qualified = true;
        }
        let first = std::str::from_utf8(&self.cur.src[start..first_end]).unwrap_or("");
        if qualified {
            return if first.eq_ignore_ascii_case("namespace") {
                Tok::NameRelative
            } else {
                Tok::NameQualified
            };
        }
        if self.after_arrow {
            return Tok::Str;
        }
        let lower = first.to_ascii_lowercase();
        match lower.as_str() {
            "function" => Tok::Function,
            "new" => Tok::New,
            "array" => Tok::Array,
            "static" => Tok::Static,
            "class" => Tok::Class,
            "use" => Tok::Use,
            "as" => Tok::As,
            "trait" => Tok::Trait,
            "interface" => Tok::Interface,
            // Contextual: a keyword only before a declared name.
            "enum" => {
                let mut at = self.cur.pos;
                let src = self.cur.src;
                let mut gap = false;
                while src.get(at).is_some_and(|b| b.is_ascii_whitespace()) {
                    at += 1;
                    gap = true;
                }
                let next_is_name = src.get(at).copied().is_some_and(is_ident_start);
                let rest = &src[at..];
                let clause = |w: &[u8]| {
                    rest.len() >= w.len()
                        && rest[..w.len()].eq_ignore_ascii_case(w)
                        && !rest.get(w.len()).copied().is_some_and(is_ident)
                };
                if gap && next_is_name && !clause(b"extends") && !clause(b"implements") {
                    Tok::Enum
                } else {
                    Tok::Str
                }
            }
            // PHP 8.4 asymmetric visibility: `private(set)` is one token.
            "public" | "protected" | "private" => {
                let rest = &self.cur.src[self.cur.pos..];
                if rest.len() >= 5 && rest[..5].eq_ignore_ascii_case(b"(set)") {
                    self.cur.pos += 5;
                }
                Tok::Other
            }
            "yield" => {
                // `yield from` is one token.
                let src = self.cur.src;
                let mut at = self.cur.pos;
                while src.get(at).is_some_and(|b| b.is_ascii_whitespace()) {
                    at += 1;
                }
                let from = src.get(at..at + 4).is_some_and(|w| w.eq_ignore_ascii_case(b"from"));
                if at > self.cur.pos && from && !src.get(at + 4).copied().is_some_and(is_ident) {
                    self.cur.pos = at + 4;
                }
                Tok::Other
            }
            l if MAGIC_CONSTANTS.contains(&l) => Tok::Literal,
            l if OTHER_KEYWORDS.contains(&l) => Tok::Other,
            _ => Tok::Str,
        }
    }

    fn number(&mut self) -> Tok {
        let src = self.cur.src;
        let radix = matches!(self.cur.peek(), Some(b'0'))
            && matches!(self.cur.peek_at(1), Some(b'x' | b'X' | b'b' | b'B' | b'o' | b'O'));
        if radix {
            self.cur.pos += 2;
            while self.cur.peek().is_some_and(|b| b.is_ascii_hexdigit() || b == b'_') {
                self.cur.bump();
            }
            return Tok::Number;
        }
        let digits = |c: &mut Cursor| {
            while c.peek().is_some_and(|b| b.is_ascii_digit() || b == b'_') {
                c.bump();
            }
        };
        digits(&mut self.cur);
        if self.cur.peek() == Some(b'.') && self.cur.peek_at(1) != Some(b'.') {
            self.cur.bump();
            digits(&mut self.cur);
        }
        if matches!(self.cur.peek(), Some(b'e' | b'E')) {
            let sign = usize::from(matches!(self.cur.peek_at(1), Some(b'+' | b'-')));
            if src.get(self.cur.pos + 1 + sign).is_some_and(u8::is_ascii_digit) {
                self.cur.pos += 1 + sign;
                digits(&mut self.cur);
            }
        }
        Tok::Number
    }

    /// At `"`: one constant string, or the opening `"` of an interpolating one.
    fn double_quoted(&mut self) -> Tok {
        if self.interpolates() {
            self.cur.bump();
            self.frames.push(Frame::Str(Close::Quote(b'"')));
            Tok::Char(b'"')
        } else {
            self.cur.skip_double_quoted(b'"');
            Tok::Literal
        }
    }

    /// At `"`: whether the string interpolates (else it is one constant
    /// string token).
    fn interpolates(&self) -> bool {
        let mut probe = Cursor::new(self.cur.src);
        probe.pos = self.cur.pos;
        probe.skip_double_quoted(b'"');
        let body = &self.cur.src[self.cur.pos + 1..probe.pos.saturating_sub(1).max(self.cur.pos + 1)];
        let mut i = 0;
        while i < body.len() {
            match body[i] {
                b'\\' => i += 2,
                b'$' if body.get(i + 1).is_some_and(|&b| is_ident_start(b) || b == b'{') => return true,
                b'{' if body.get(i + 1) == Some(&b'$') => return true,
                _ => i += 1,
            }
        }
        false
    }

    /// At a line start in a heredoc body: the closing label (indentation
    /// allowed), not followed by a name byte. Returns its end.
    fn closing_label_end(&self, at: usize, label: &[u8]) -> Option<usize> {
        let src = self.cur.src;
        if at != 0 && src.get(at - 1) != Some(&b'\n') {
            return None;
        }
        let mut i = at;
        while matches!(src.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        let end = i + label.len();
        (src.get(i..end) == Some(label) && !src.get(end).copied().is_some_and(is_ident)).then_some(end)
    }

    /// At `<<<`: the heredoc/nowdoc header. A heredoc opens a string frame;
    /// a nowdoc's body and end are lexed at once into `pending`.
    fn heredoc_start(&mut self, start: usize) -> Tok {
        let src = self.cur.src;
        let mut at = start + 3;
        while matches!(src.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        let quote = src.get(at).copied().filter(|&q| q == b'\'' || q == b'"');
        if quote.is_some() {
            at += 1;
        }
        let label_start = at;
        while src.get(at).copied().is_some_and(is_ident) {
            at += 1;
        }
        let label = src[label_start..at].to_vec();
        if let Some(q) = quote {
            if src.get(at) != Some(&q) {
                self.cur.pos = start + 3;
                return Tok::Other;
            }
            at += 1;
        }
        if src.get(at) == Some(&b'\r') {
            at += 1;
        }
        if label.is_empty() || src.get(at) != Some(&b'\n') {
            self.cur.pos = start + 3;
            return Tok::Other;
        }
        self.cur.pos = at + 1;
        if quote != Some(b'\'') {
            self.frames.push(Frame::Str(Close::Heredoc(label)));
            return Tok::StartHeredoc;
        }
        // Nowdoc: no interpolation; the body runs to the closing line.
        let body = self.cur.pos;
        let mut line = body;
        let close = loop {
            if let Some(end) = self.closing_label_end(line, &label) {
                break Some((line, end));
            }
            match src[line..].iter().position(|&b| b == b'\n') {
                Some(i) => line += i + 1,
                None => break None,
            }
        };
        let (body_end, end) = close.unwrap_or((src.len(), src.len()));
        let text = |a: usize, b: usize| std::str::from_utf8(&src[a..b]).unwrap_or("");
        self.pending.push_back(Token { kind: Tok::StartHeredoc, text: text(start, body), offset: start });
        if body_end > body {
            self.pending.push_back(Token { kind: Tok::EncapsedAndWhitespace, text: text(body, body_end), offset: body });
        }
        if close.is_some() {
            self.pending.push_back(Token { kind: Tok::EndHeredoc, text: text(body_end, end), offset: body_end });
        }
        self.cur.pos = end;
        Tok::StartHeredoc
    }

    /// The next token inside a string body.
    fn string_part(&mut self) -> Option<Token<'a>> {
        let src = self.cur.src;
        let start = self.cur.pos;
        let b = self.cur.peek()?;
        let token = |kind, end: usize| Token {
            kind,
            text: std::str::from_utf8(&src[start..end]).unwrap_or(""),
            offset: start,
        };
        let (quote, label) = match self.frames.last() {
            Some(Frame::Str(Close::Quote(q))) => (Some(*q), None),
            Some(Frame::Str(Close::Heredoc(l))) => (None, Some(l.clone())),
            _ => return None,
        };
        if quote == Some(b) {
            self.cur.bump();
            self.frames.pop();
            return Some(token(Tok::Char(b), self.cur.pos));
        }
        if let Some(end) = label.as_deref().and_then(|l| self.closing_label_end(start, l)) {
            self.cur.pos = end;
            self.frames.pop();
            return Some(token(Tok::EndHeredoc, end));
        }
        match (b, self.cur.peek_at(1)) {
            (b'$', Some(n)) if is_ident_start(n) => {
                self.cur.bump();
                self.cur.read_ident();
                let var = token(Tok::Variable, self.cur.pos);
                self.simple_interpolation_tail();
                return Some(var);
            }
            (b'{', Some(b'$')) => {
                self.cur.bump();
                self.frames.push(Frame::Island { depth: 0 });
                return Some(token(Tok::CurlyOpen, self.cur.pos));
            }
            (b'$', Some(b'{')) => {
                self.cur.pos += 2;
                self.frames.push(Frame::Island { depth: 0 });
                return Some(token(Tok::DollarOpenCurlyBraces, self.cur.pos));
            }
            _ => {}
        }
        // A text run, up to the next interpolation or the end.
        let mut at = start;
        while let Some(&c) = src.get(at) {
            let next = src.get(at + 1).copied();
            if Some(c) == quote
                || (c == b'$' && next.is_some_and(|n| is_ident_start(n) || n == b'{'))
                || (c == b'{' && next == Some(b'$'))
            {
                break;
            }
            if c == b'\\' {
                at = (at + 2).min(src.len());
                continue;
            }
            at += 1;
            if c == b'\n' && label.as_deref().is_some_and(|l| self.closing_label_end(at, l).is_some()) {
                break;
            }
        }
        self.cur.pos = at;
        Some(token(Tok::EncapsedAndWhitespace, at))
    }

    /// After `$var` in a string: `[key]` or `->prop` / `?->prop`, as PHP's
    /// simple interpolation reads them, into `pending`.
    fn simple_interpolation_tail(&mut self) {
        let src = self.cur.src;
        let push = |this: &mut Self, kind, from: usize| {
            this.pending.push_back(Token {
                kind,
                text: std::str::from_utf8(&src[from..this.cur.pos]).unwrap_or(""),
                offset: from,
            });
        };
        let at = self.cur.pos;
        if self.cur.peek() == Some(b'[') {
            self.cur.bump();
            push(self, Tok::Char(b'['), at);
            let key = self.cur.pos;
            match self.cur.peek() {
                Some(b'-') => {
                    self.cur.bump();
                    push(self, Tok::Char(b'-'), key);
                    let num = self.cur.pos;
                    while self.cur.peek().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
                        self.cur.bump();
                    }
                    push(self, Tok::Number, num);
                }
                Some(b) if b.is_ascii_digit() => {
                    while self.cur.peek().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
                        self.cur.bump();
                    }
                    push(self, Tok::Number, key);
                }
                Some(b'$') if self.cur.peek_at(1).is_some_and(is_ident_start) => {
                    self.cur.bump();
                    self.cur.read_ident();
                    push(self, Tok::Variable, key);
                }
                Some(b) if is_ident_start(b) => {
                    self.cur.read_ident();
                    push(self, Tok::Str, key);
                }
                _ => {}
            }
            if self.cur.peek() == Some(b']') {
                let close = self.cur.pos;
                self.cur.bump();
                push(self, Tok::Char(b']'), close);
            }
        } else {
            let (op, len) = if self.cur.starts_with(b"->") {
                (Tok::ObjectOperator, 2)
            } else if self.cur.starts_with(b"?->") {
                (Tok::NullsafeObjectOperator, 3)
            } else {
                return;
            };
            if !self.cur.peek_at(len).is_some_and(is_ident_start) {
                return;
            }
            self.cur.pos += len;
            push(self, op, at);
            let name = self.cur.pos;
            self.cur.read_ident();
            push(self, Tok::Str, name);
        }
    }

    /// At `(`: a cast, when only spaces/tabs surround a cast type.
    fn cast(&mut self) -> Option<Tok> {
        let src = self.cur.src;
        let mut at = self.cur.pos + 1;
        while matches!(src.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        let word_start = at;
        while src.get(at).is_some_and(|b| b.is_ascii_alphabetic()) {
            at += 1;
        }
        let word = src[word_start..at].to_ascii_lowercase();
        while matches!(src.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        if src.get(at) != Some(&b')') {
            return None;
        }
        let kind = match word.as_slice() {
            b"int" | b"integer" | b"float" | b"double" | b"string" | b"binary" | b"array"
            | b"bool" | b"boolean" => Tok::Cast,
            b"object" | b"unset" => Tok::Other,
            _ => return None,
        };
        self.cur.pos = at + 1;
        Some(kind)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.cur.peek() {
                Some(b) if b.is_ascii_whitespace() => self.cur.bump(),
                Some(b'/') if self.cur.peek_at(1) == Some(b'/') => self.line_comment(),
                Some(b'/') if self.cur.peek_at(1) == Some(b'*') => self.cur.skip_block_comment(),
                Some(b'#') if self.cur.peek_at(1) != Some(b'[') => self.line_comment(),
                _ => return,
            }
        }
    }

    /// A line comment ends at a newline or before `?>`.
    fn line_comment(&mut self) {
        while let Some(b) = self.cur.peek() {
            if b == b'\n' || (b == b'?' && self.cur.peek_at(1) == Some(b'>')) {
                return;
            }
            self.cur.bump();
        }
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        if let Some(t) = self.pending.pop_front() {
            self.after_arrow = false;
            return Some(t);
        }
        if matches!(self.frames.last(), Some(Frame::Str(_))) {
            self.after_arrow = false;
            return self.string_part();
        }
        self.skip_trivia();
        let start = self.cur.pos;
        let b = self.cur.peek()?;
        let kind = match b {
            b'$' if self.cur.peek_at(1).is_some_and(is_ident_start) => {
                self.cur.bump();
                self.cur.read_ident();
                Tok::Variable
            }
            b'\\' if self.cur.peek_at(1).is_some_and(is_ident_start) => {
                self.cur.bump();
                self.name(start);
                Tok::NameFullyQualified
            }
            b if is_ident_start(b) => {
                // `b'…'` / `b"…"` binary string prefixes.
                if matches!(b, b'b' | b'B') && matches!(self.cur.peek_at(1), Some(b'\'' | b'"')) {
                    self.cur.bump();
                    if self.cur.peek() == Some(b'\'') {
                        self.cur.skip_single_quoted();
                        Tok::Literal
                    } else {
                        self.double_quoted()
                    }
                } else {
                    self.name(start)
                }
            }
            b'0'..=b'9' => self.number(),
            b'.' if self.cur.peek_at(1).is_some_and(|b| b.is_ascii_digit()) => self.number(),
            b'\'' => {
                self.cur.skip_single_quoted();
                Tok::Literal
            }
            b'"' => self.double_quoted(),
            b'`' => {
                self.cur.bump();
                self.frames.push(Frame::Str(Close::Quote(b'`')));
                Tok::Char(b'`')
            }
            b'<' if self.cur.starts_with(b"<<<") => {
                let kind = self.heredoc_start(start);
                // A nowdoc queued its own header, body and end.
                if self.pending.front().is_some_and(|t| t.offset == start) {
                    return self.pending.pop_front();
                }
                kind
            }
            // `T_NS_SEPARATOR` (`Foo\{A, B}`).
            b'\\' => {
                self.cur.bump();
                Tok::Other
            }
            b'{' => {
                if let Some(Frame::Island { depth }) = self.frames.last_mut() {
                    *depth += 1;
                }
                self.cur.bump();
                Tok::Char(b'{')
            }
            b'}' => {
                match self.frames.last_mut() {
                    Some(Frame::Island { depth: 0 }) => {
                        self.frames.pop();
                    }
                    Some(Frame::Island { depth }) => *depth -= 1,
                    _ => {}
                }
                self.cur.bump();
                Tok::Char(b'}')
            }
            b'(' => match self.cast() {
                Some(kind) => kind,
                None => {
                    self.cur.bump();
                    Tok::Char(b'(')
                }
            },
            _ => match OPERATORS.iter().find(|(op, _)| self.cur.starts_with(op.as_bytes())) {
                Some((op, kind)) => {
                    self.cur.pos += op.len();
                    *kind
                }
                None => {
                    self.cur.bump();
                    Tok::Char(b)
                }
            },
        };
        self.after_arrow = matches!(kind, Tok::ObjectOperator | Tok::NullsafeObjectOperator);
        Some(Token { kind, text: self.text(start), offset: start })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(Tok, &str)> {
        tokens(src.as_bytes(), 0).map(|t| (t.kind, t.text)).collect()
    }

    #[test]
    fn a_constructor() {
        let src = "function __construct(Foo $a, array $b = [], ?int $c = null) {\n    \
                   $this->a = $a ?? \\Magento\\Framework\\App\\ObjectManager::getInstance()->get(Foo::class);\n    \
                   parent::__construct(...$b); // done\n}";
        let got = kinds(src);
        use Tok::*;
        assert_eq!(
            &got[..8],
            &[
                (Function, "function"),
                (Str, "__construct"),
                (Char(b'('), "("),
                (Str, "Foo"),
                (Variable, "$a"),
                (Char(b','), ","),
                (Array, "array"),
                (Variable, "$b"),
            ]
        );
        assert!(got.contains(&(NameFullyQualified, "\\Magento\\Framework\\App\\ObjectManager")));
        assert!(got.contains(&(Class, "class")));
        assert!(got.contains(&(Coalesce, "??")));
        assert!(got.contains(&(Ellipsis, "...")));
        assert_eq!(got.last(), Some(&(Char(b'}'), "}")));
    }

    #[test]
    fn names_after_an_arrow_are_plain() {
        use Tok::*;
        assert_eq!(
            kinds("$this->new = $x?->class;"),
            [
                (Variable, "$this"),
                (ObjectOperator, "->"),
                (Str, "new"),
                (Char(b'='), "="),
                (Variable, "$x"),
                (NullsafeObjectOperator, "?->"),
                (Str, "class"),
                (Char(b';'), ";"),
            ]
        );
        // After `::` a keyword stays a keyword.
        assert_eq!(kinds("X::class")[2], (Class, "class"));
    }

    #[test]
    fn literals_casts_and_others() {
        use Tok::*;
        let got = kinds("'a' \"b\\$\" \"c $d\" 1_000 0x1F .5 1e3 (int) ( string ) (object) __CLASS__ __PROPERTY__ exit ??= == <<<EOT\nx\nEOT;");
        let kinds: Vec<Tok> = got.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            [
                Literal, Literal, Char(b'"'), EncapsedAndWhitespace, Variable, Char(b'"'), Number,
                Number, Number, Number, Cast, Cast, Other, Literal, Other, Other, CoalesceEqual, Other,
                StartHeredoc, EncapsedAndWhitespace, EndHeredoc, Char(b';'),
            ]
        );
    }

    #[test]
    fn interpolation_yields_its_inner_tokens() {
        use Tok::*;
        assert_eq!(
            kinds(r#""a {$this->m()} $x->y $z[0] $w[k] ${v}" . `c $d`;"#),
            [
                (Char(b'"'), "\""),
                (EncapsedAndWhitespace, "a "),
                (CurlyOpen, "{"),
                (Variable, "$this"),
                (ObjectOperator, "->"),
                (Str, "m"),
                (Char(b'('), "("),
                (Char(b')'), ")"),
                (Char(b'}'), "}"),
                (EncapsedAndWhitespace, " "),
                (Variable, "$x"),
                (ObjectOperator, "->"),
                (Str, "y"),
                (EncapsedAndWhitespace, " "),
                (Variable, "$z"),
                (Char(b'['), "["),
                (Number, "0"),
                (Char(b']'), "]"),
                (EncapsedAndWhitespace, " "),
                (Variable, "$w"),
                (Char(b'['), "["),
                (Str, "k"),
                (Char(b']'), "]"),
                (EncapsedAndWhitespace, " "),
                (DollarOpenCurlyBraces, "${"),
                (Str, "v"),
                (Char(b'}'), "}"),
                (Char(b'"'), "\""),
                (Char(b'.'), "."),
                (Char(b'`'), "`"),
                (EncapsedAndWhitespace, "c "),
                (Variable, "$d"),
                (Char(b'`'), "`"),
                (Char(b';'), ";"),
            ]
        );
        // A nested string and braces inside an island.
        assert_eq!(
            kinds(r#""{$a["k"]} {$b->c([1])}" ;"#).iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            [
                Char(b'"'), CurlyOpen, Variable, Char(b'['), Literal, Char(b']'), Char(b'}'),
                EncapsedAndWhitespace, CurlyOpen, Variable, ObjectOperator, Str, Char(b'('),
                Char(b'['), Number, Char(b']'), Char(b')'), Char(b'}'), Char(b'"'), Char(b';'),
            ]
        );
        // A heredoc interpolates; a nowdoc does not.
        assert_eq!(
            kinds("<<<A\n  x {$this->m()}\n  A;\n<<<'B'\n{$y}\nB;").iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            [
                StartHeredoc, EncapsedAndWhitespace, CurlyOpen, Variable, ObjectOperator, Str,
                Char(b'('), Char(b')'), Char(b'}'), EncapsedAndWhitespace, EndHeredoc, Char(b';'),
                StartHeredoc, EncapsedAndWhitespace, EndHeredoc, Char(b';'),
            ]
        );
    }

    #[test]
    fn names() {
        use Tok::*;
        assert_eq!(
            kinds("A\\B \\C namespace\\D true parent static enum Foo"),
            [
                (NameQualified, "A\\B"),
                (NameFullyQualified, "\\C"),
                (NameRelative, "namespace\\D"),
                (Str, "true"),
                (Str, "parent"),
                (Static, "static"),
                (Enum, "enum"),
                (Str, "Foo"),
            ]
        );
    }
}
