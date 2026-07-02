//! A focused parser for Magento's `schema.graphqls` files: GraphQL SDL type definitions
//! plus the Magento directives that matter (`@resolver`, `@typeResolver`, `@doc`,
//! `@deprecated`, `@cache`). Tolerant by design — an unrecognized construct is skipped,
//! never a parse failure — and it must always make progress (no panics, no hangs).
//! Cross-module merge happens in `breadth::GqlIndex`.

/// One token with the 1-based line it starts on. Commas are whitespace in GraphQL and are
/// dropped by the lexer; `#` starts a line comment.
enum Tok {
    Name(String),
    /// A `"…"` or `"""…"""` string, unescaped.
    Str(String),
    Punct(char),
}

fn lex(src: &str) -> Vec<(Tok, u32)> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut line = 1u32;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' | b',' => i += 1,
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                let start_line = line;
                let mut bytes = Vec::new();
                if b[i..].starts_with(b"\"\"\"") {
                    // Block string: raw until the closing triple quote; `\"""` is the one
                    // escape block strings have.
                    i += 3;
                    while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                        if b[i] == b'\\' && b[i + 1..].starts_with(b"\"\"\"") {
                            bytes.extend_from_slice(b"\"\"\"");
                            i += 4;
                            continue;
                        }
                        if b[i] == b'\n' {
                            line += 1;
                        }
                        bytes.push(b[i]);
                        i += 1;
                    }
                    i += 3;
                    let text = String::from_utf8_lossy(&bytes).trim().to_string();
                    out.push((Tok::Str(text), start_line));
                } else {
                    i += 1;
                    while i < b.len() && b[i] != b'"' {
                        if b[i] == b'\\' && i + 1 < b.len() {
                            i += 1;
                            match b[i] {
                                b'n' => bytes.push(b'\n'),
                                b't' => bytes.push(b'\t'),
                                other => bytes.push(other),
                            }
                        } else {
                            if b[i] == b'\n' {
                                line += 1;
                            }
                            bytes.push(b[i]);
                        }
                        i += 1;
                    }
                    i += 1;
                    out.push((Tok::Str(String::from_utf8_lossy(&bytes).into_owned()), start_line));
                }
            }
            _ if is_name(c) => {
                let start = i;
                while i < b.len() && is_name(b[i]) {
                    i += 1;
                }
                out.push((Tok::Name(src[start..i].to_string()), line));
            }
            _ => {
                out.push((Tok::Punct(c as char), line));
                i += 1;
            }
        }
    }
    out
}

/// Name characters, deliberately loose: identifiers plus the number/float characters, so a
/// numeric literal lexes as one throwaway token.
fn is_name(c: u8) -> bool {
    c == b'_' || c == b'-' || c == b'.' || c.is_ascii_alphanumeric()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawGqlKind {
    Object,
    Interface,
    Input,
    Enum,
    Union,
    Scalar,
}

pub(crate) struct RawDirective {
    pub name: String,
    /// `(name, value)` pairs; string values unescaped, other scalars as their token text.
    pub args: Vec<(String, String)>,
}

pub(crate) struct RawGqlArg {
    pub name: String,
    pub ty: String,
}

pub(crate) struct RawGqlField {
    pub name: String,
    pub args: Vec<RawGqlArg>,
    pub ty: String,
    pub directives: Vec<RawDirective>,
    /// SDL description string, when present (`@doc` usually carries it instead).
    pub description: Option<String>,
    pub line: u32,
}

pub(crate) struct RawGqlType {
    pub kind: RawGqlKind,
    pub name: String,
    pub implements: Vec<String>,
    pub directives: Vec<RawDirective>,
    pub fields: Vec<RawGqlField>,
    /// Enum values.
    pub values: Vec<String>,
    /// Union member types.
    pub members: Vec<String>,
    pub description: Option<String>,
    pub line: u32,
}

/// Parse one `schema.graphqls`. `extend type X` is returned as a plain re-declaration of
/// `X` (the cross-module merge unions it, which is also how Magento's schema reader
/// treats re-declared types). `schema {}` and `directive @x on …` definitions are skipped.
pub(crate) fn schema_graphqls(src: &str) -> Vec<RawGqlType> {
    let toks = lex(src);
    let mut p = Parser { toks, i: 0 };
    let mut out = Vec::new();
    let mut desc: Option<String> = None;

    while let Some(tok) = p.peek() {
        match tok {
            Tok::Str(s) => {
                desc = Some(s.clone());
                p.i += 1;
            }
            Tok::Name(kw) => {
                let kw = kw.clone();
                match kw.as_str() {
                    "extend" => {
                        p.i += 1; // the following `type X {…}` merges like a re-declaration
                    }
                    "type" | "interface" | "input" => {
                        let kind = match kw.as_str() {
                            "interface" => RawGqlKind::Interface,
                            "input" => RawGqlKind::Input,
                            _ => RawGqlKind::Object,
                        };
                        if let Some(t) = p.type_def(kind, desc.take()) {
                            out.push(t);
                        }
                    }
                    "enum" => {
                        if let Some(t) = p.enum_def(desc.take()) {
                            out.push(t);
                        }
                    }
                    "union" => {
                        if let Some(t) = p.union_def(desc.take()) {
                            out.push(t);
                        }
                    }
                    "scalar" => {
                        if let Some(t) = p.scalar_def(desc.take()) {
                            out.push(t);
                        }
                    }
                    "schema" => {
                        p.i += 1;
                        p.skip_directives();
                        p.skip_balanced('{', '}');
                    }
                    "directive" => p.skip_directive_def(),
                    _ => p.i += 1, // unknown keyword: skip a token, keep going
                }
            }
            Tok::Punct(_) => p.i += 1,
        }
    }
    out
}

/// The keywords that can start a new definition — the boundary bare-name lists (the
/// legacy comma `implements` form) must never consume across.
fn is_definition_keyword(name: &str) -> bool {
    matches!(
        name,
        "type" | "interface" | "input" | "enum" | "union" | "scalar" | "extend" | "schema"
            | "directive"
    )
}

struct Parser {
    toks: Vec<(Tok, u32)>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|(t, _)| t)
    }
    fn line(&self) -> u32 {
        self.toks.get(self.i).map(|(_, l)| *l).unwrap_or(0)
    }
    fn name(&mut self) -> Option<String> {
        match self.toks.get(self.i) {
            Some((Tok::Name(n), _)) => {
                self.i += 1;
                Some(n.clone())
            }
            _ => None,
        }
    }
    fn punct(&mut self, c: char) -> bool {
        if matches!(self.toks.get(self.i), Some((Tok::Punct(p), _)) if *p == c) {
            self.i += 1;
            return true;
        }
        false
    }

    /// `type|interface|input Name [implements A & B] @dir… [{ fields }]` — positioned on
    /// the keyword.
    fn type_def(&mut self, kind: RawGqlKind, description: Option<String>) -> Option<RawGqlType> {
        let line = self.line();
        self.i += 1; // keyword
        let name = self.name()?;
        // `implements A & B` — also the legacy comma form `implements A, B` (commas are
        // whitespace to the lexer, so it reads as bare consecutive names). A bare name is
        // accepted only when it isn't a definition keyword, so a bodiless type can't
        // swallow the next definition.
        let mut implements = Vec::new();
        if matches!(self.peek(), Some(Tok::Name(k)) if k == "implements") {
            self.i += 1;
            loop {
                if !self.punct('&') && !matches!(self.peek(), Some(Tok::Name(n)) if !is_definition_keyword(n))
                {
                    break;
                }
                match self.name() {
                    Some(n) => implements.push(n),
                    None => break,
                }
            }
        }
        let directives = self.directives();
        let mut fields = Vec::new();
        if self.punct('{') {
            while !self.punct('}') {
                if self.i >= self.toks.len() {
                    break;
                }
                match self.field() {
                    Some(f) => fields.push(f),
                    None => self.i += 1, // recover: drop one token and retry
                }
            }
        }
        Some(RawGqlType {
            kind,
            name,
            implements,
            directives,
            fields,
            values: Vec::new(),
            members: Vec::new(),
            description,
            line,
        })
    }

    /// `[desc] name [(args)]: Type @dir…`
    fn field(&mut self) -> Option<RawGqlField> {
        let description = match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.i += 1;
                Some(s)
            }
            _ => None,
        };
        let line = self.line();
        let name = self.name()?;
        let mut args = Vec::new();
        if self.punct('(') {
            while !self.punct(')') {
                if self.i >= self.toks.len() {
                    break;
                }
                // `[desc] name: Type [= default] @dir…`
                if matches!(self.peek(), Some(Tok::Str(_))) {
                    self.i += 1;
                }
                let Some(an) = self.name() else {
                    self.i += 1;
                    continue;
                };
                if !self.punct(':') {
                    continue;
                }
                let ty = self.type_ref();
                if self.punct('=') {
                    self.skip_value();
                }
                self.skip_directives();
                args.push(RawGqlArg { name: an, ty });
            }
        }
        if !self.punct(':') {
            return None;
        }
        let ty = self.type_ref();
        let directives = self.directives();
        Some(RawGqlField { name, args, ty, directives, description, line })
    }

    /// A type reference rendered back to text: `[ProductInterface!]!`.
    fn type_ref(&mut self) -> String {
        let mut s = String::new();
        if self.punct('[') {
            s.push('[');
            s.push_str(&self.type_ref());
            if self.punct(']') {
                s.push(']');
            }
        } else if let Some(n) = self.name() {
            s.push_str(&n);
        }
        if self.punct('!') {
            s.push('!');
        }
        s
    }

    /// `@name[(k: v …)]`, repeated.
    fn directives(&mut self) -> Vec<RawDirective> {
        let mut out = Vec::new();
        while self.punct('@') {
            let Some(name) = self.name() else { break };
            let mut args = Vec::new();
            if self.punct('(') {
                while !self.punct(')') {
                    if self.i >= self.toks.len() {
                        break;
                    }
                    let Some(k) = self.name() else {
                        self.i += 1;
                        continue;
                    };
                    if !self.punct(':') {
                        continue;
                    }
                    let v = match self.peek() {
                        Some(Tok::Str(s)) => {
                            let s = s.clone();
                            self.i += 1;
                            s
                        }
                        Some(Tok::Name(n)) => {
                            let n = n.clone();
                            self.i += 1;
                            n
                        }
                        _ => {
                            self.skip_value();
                            "…".to_string()
                        }
                    };
                    args.push((k, v));
                }
            }
            out.push(RawDirective { name, args });
        }
        out
    }

    fn skip_directives(&mut self) {
        self.directives();
    }

    /// `enum Name @dir… { [desc] VALUE @dir… … }`
    fn enum_def(&mut self, description: Option<String>) -> Option<RawGqlType> {
        let line = self.line();
        self.i += 1;
        let name = self.name()?;
        let directives = self.directives();
        let mut values = Vec::new();
        if self.punct('{') {
            while !self.punct('}') {
                if self.i >= self.toks.len() {
                    break;
                }
                match self.peek() {
                    Some(Tok::Str(_)) => self.i += 1,
                    Some(Tok::Name(_)) => {
                        values.push(self.name().unwrap());
                        self.skip_directives();
                    }
                    _ => self.i += 1,
                }
            }
        }
        Some(RawGqlType {
            kind: RawGqlKind::Enum,
            name,
            implements: Vec::new(),
            directives,
            fields: Vec::new(),
            values,
            members: Vec::new(),
            description,
            line,
        })
    }

    /// `union Name @dir… = A | B | C`
    fn union_def(&mut self, description: Option<String>) -> Option<RawGqlType> {
        let line = self.line();
        self.i += 1;
        let name = self.name()?;
        let directives = self.directives();
        // `= A | B | C` — members only behind explicit `|`s, so the next definition's
        // keyword (a bare Name) is never swallowed as a member.
        let mut members = Vec::new();
        if self.punct('=') {
            self.punct('|'); // optional leading `|`
            if let Some(n) = self.name() {
                members.push(n);
                while self.punct('|') {
                    match self.name() {
                        Some(n) => members.push(n),
                        None => break,
                    }
                }
            }
        }
        Some(RawGqlType {
            kind: RawGqlKind::Union,
            name,
            implements: Vec::new(),
            directives,
            fields: Vec::new(),
            values: Vec::new(),
            members,
            description,
            line,
        })
    }

    fn scalar_def(&mut self, description: Option<String>) -> Option<RawGqlType> {
        let line = self.line();
        self.i += 1;
        let name = self.name()?;
        let directives = self.directives();
        Some(RawGqlType {
            kind: RawGqlKind::Scalar,
            name,
            implements: Vec::new(),
            directives,
            fields: Vec::new(),
            values: Vec::new(),
            members: Vec::new(),
            description,
            line,
        })
    }

    /// `directive @name[(args)] [repeatable] on LOC | LOC …` — a definition, not a use.
    fn skip_directive_def(&mut self) {
        self.i += 1; // `directive`
        if self.punct('@') {
            self.name();
        }
        self.skip_balanced('(', ')');
        if matches!(self.peek(), Some(Tok::Name(k)) if k == "repeatable") {
            self.i += 1;
        }
        if matches!(self.peek(), Some(Tok::Name(k)) if k == "on") {
            self.i += 1;
            self.name();
            while self.punct('|') {
                self.name();
            }
        }
    }

    /// Skip one value: a scalar token, or a balanced `[…]`/`{…}`.
    fn skip_value(&mut self) {
        match self.peek() {
            Some(Tok::Punct('[')) => self.skip_balanced('[', ']'),
            Some(Tok::Punct('{')) => self.skip_balanced('{', '}'),
            Some(_) => self.i += 1,
            None => {}
        }
    }

    /// If positioned on `open`, skip to (and past) its matching `close`.
    fn skip_balanced(&mut self, open: char, close: char) {
        if !matches!(self.peek(), Some(Tok::Punct(c)) if *c == open) {
            return;
        }
        let mut depth = 0i32;
        while self.i < self.toks.len() {
            match &self.toks[self.i].0 {
                Tok::Punct(c) if *c == open => depth += 1,
                Tok::Punct(c) if *c == close => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{schema_graphqls, RawGqlKind};

    const SDL: &str = r#"
# Copyright comment
type Query {
    products(
        search: String @doc(description: "search term")
        pageSize: Int = 20
    ): Products @resolver(class: "Magento\\CatalogGraphQl\\Model\\Resolver\\Products") @cache(cacheIdentity: "X") @doc(description: "Search products.")
}

interface ProductInterface @typeResolver(class: "\\Magento\\CatalogGraphQl\\Model\\ProductInterfaceTypeResolverComposite") {
    name: String @doc(description: "Product name.")
}

"""A described type."""
type Simple implements ProductInterface & Other {
    sku: String! @deprecated(reason: "Use uid.")
    items: [LineItem!]!
}

extend type Query {
    extra: String @resolver(class: "A\\B")
}

enum Mode { DEFAULT "doc" CATALOG @deprecated }
union Result = Simple | Other
scalar Money
schema { query: Query }
directive @cache(cacheIdentity: String="" cacheable: Boolean=true) on QUERY
"#;

    #[test]
    fn parses_types_fields_directives() {
        let types = schema_graphqls(SDL);
        let by = |n: &str| types.iter().filter(|t| t.name == n).collect::<Vec<_>>();

        // Query appears twice: the base declaration and the `extend`.
        let queries = by("Query");
        assert_eq!(queries.len(), 2);
        let products = &queries[0].fields[0];
        assert_eq!(products.name, "products");
        assert_eq!(products.ty, "Products");
        assert_eq!(products.args.len(), 2);
        assert_eq!(products.args[1].name, "pageSize"); // default value skipped cleanly
        let resolver = products.directives.iter().find(|d| d.name == "resolver").unwrap();
        // String unescaping: "Magento\\CatalogGraphQl" → single backslashes.
        assert_eq!(resolver.args[0].1, "Magento\\CatalogGraphQl\\Model\\Resolver\\Products");
        assert_eq!(queries[1].fields[0].name, "extra");

        let iface = &by("ProductInterface")[0];
        assert!(matches!(iface.kind, RawGqlKind::Interface));
        let tr = iface.directives.iter().find(|d| d.name == "typeResolver").unwrap();
        assert!(tr.args[0].1.starts_with('\\')); // raw; ClassName::new normalizes later

        let simple = &by("Simple")[0];
        assert_eq!(simple.implements, ["ProductInterface", "Other"]);
        assert_eq!(simple.description.as_deref(), Some("A described type."));
        assert_eq!(simple.fields[0].name, "sku");
        assert_eq!(simple.fields[1].ty, "[LineItem!]!");
        let dep = simple.fields[0].directives.iter().find(|d| d.name == "deprecated").unwrap();
        assert_eq!(dep.args[0].1, "Use uid.");

        assert_eq!(by("Mode")[0].values, ["DEFAULT", "CATALOG"]);
        assert_eq!(by("Result")[0].members, ["Simple", "Other"]);
        assert!(matches!(by("Money")[0].kind, RawGqlKind::Scalar));
        // schema {} and the directive definition produce no types: Query ×2 (base +
        // extend), ProductInterface, Simple, Mode, Result, Money.
        assert_eq!(types.len(), 7);
    }

    #[test]
    fn legacy_comma_implements_keeps_fields() {
        // Pre-June-2018 SDL (still accepted by graphql-php): commas separate interfaces.
        // The commas are whitespace to the lexer; the names must still be collected and,
        // crucially, the `{…}` body must not be lost.
        let sdl = r#"
type Legacy implements A, B, C {
    x: Int
}
type Bodiless implements A
type Next {
    y: Int
}
"#;
        let types = schema_graphqls(sdl);
        let legacy = types.iter().find(|t| t.name == "Legacy").unwrap();
        assert_eq!(legacy.implements, ["A", "B", "C"]);
        assert_eq!(legacy.fields.len(), 1);
        // A bodiless type must not swallow the next definition's keyword.
        let bodiless = types.iter().find(|t| t.name == "Bodiless").unwrap();
        assert_eq!(bodiless.implements, ["A"]);
        let next = types.iter().find(|t| t.name == "Next").unwrap();
        assert_eq!(next.fields.len(), 1);
    }

    #[test]
    fn block_string_escape_and_junk_recovery() {
        let sdl = r#"
"""Contains \""" inside."""
type Documented { x: Int }
%%% ??? !!!
type AfterJunk { y: Int @resolver(class: "A\\B") }
"#;
        let types = schema_graphqls(sdl);
        let doc = types.iter().find(|t| t.name == "Documented").unwrap();
        assert_eq!(doc.description.as_deref(), Some("Contains \"\"\" inside."));
        // Garbage between definitions is skipped; parsing resumes at the next keyword.
        let after = types.iter().find(|t| t.name == "AfterJunk").unwrap();
        assert_eq!(after.fields[0].name, "y");
    }

    #[test]
    fn every_prefix_parses_without_panic() {
        // Truncation fuzz: any prefix of a real-looking document must parse (possibly to
        // less data), never panic or hang — the tolerant-parser guarantee.
        for n in 0..SDL.len() {
            if SDL.is_char_boundary(n) {
                let _ = schema_graphqls(&SDL[..n]);
            }
        }
    }
}
