//! A byte-faithful port of the `laminas/laminas-code` generators as Magento
//! drives them (`Magento\Framework\Code\Generator\ClassGenerator` +
//! `EntityAbstract::_fixCodeStyle`). Proxies and interceptors are both built
//! by handing method/parameter/property *definitions* to the ClassGenerator;
//! this module renders those definitions to the exact same bytes.
//!
//! Only the subset the code generators use is implemented: no traits in the
//! generated output, no constants, no readonly classes, single inheritance +
//! a flat interface list, docblocks with a short description and simple tags.

const INDENT: &str = "    ";
const LF: char = '\n';

/// A PHP value as a generated default renders it (Laminas `ValueGenerator`).
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Bool(bool),
    Null,
    Int(i64),
    /// Rendered verbatim (Laminas casts the float to string).
    Float(f64),
    Str(String),
    /// `(key, value)` — `key` absent means a sequential list entry.
    Array(Vec<(Option<ArrKey>, Val)>),
    /// A verbatim PHP expression, rendered as-is (no quoting). Used to
    /// preserve a parameter default the const-evaluator can't fold — e.g. a
    /// `\Class::CONST` whose defining class isn't in the parsed universe.
    /// Dropping such a default would make an inherited-optional parameter
    /// required and fatal at load time, so we keep a fully-qualified,
    /// always-valid reference instead.
    Raw(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrKey {
    Int(i64),
    Str(String),
}

impl Val {
    /// `ValueGenerator::generate` in single-line mode (what `ParameterGenerator`
    /// uses for defaults). Arrays are short `[…]`.
    pub fn render(&self) -> String {
        match self {
            Val::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
            Val::Null => "null".to_owned(),
            Val::Int(n) => n.to_string(),
            Val::Float(f) => render_float(*f),
            Val::Str(s) => escape_single_quoted(s),
            Val::Raw(s) => s.clone(),
            Val::Array(items) => {
                let mut noindex = 0i64;
                let parts: Vec<String> = items
                    .iter()
                    .map(|(key, value)| {
                        let v = value.render();
                        match key {
                            None => {
                                noindex += 1;
                                v
                            }
                            Some(ArrKey::Int(n)) => {
                                if *n == noindex {
                                    noindex += 1;
                                    v
                                } else {
                                    noindex = (n + 1).max(noindex);
                                    format!("{n} => {v}")
                                }
                            }
                            Some(ArrKey::Str(s)) => {
                                format!("{} => {v}", escape_single_quoted(s))
                            }
                        }
                    })
                    .collect();
                format!("[{}]", parts.join(", "))
            }
        }
    }
}

/// `ValueGenerator::escape` = `addcslashes($s, "\\'")` wrapped in single
/// quotes: only backslash and single-quote are escaped.
fn escape_single_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\\' || ch == '\'' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// PHP's `(string)$float`. Integers-as-floats print without a decimal point.
fn render_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        let s = format!("{f}");
        s
    }
}

// ---- type rendering (Laminas TypeGenerator) --------------------------------

/// `TypeGenerator::fromTypeString(type).generate()` — the FQCN-prefixing,
/// nullable-marker, union/intersection-sorting type renderer. Input is the
/// Magento-level type string (reflection `getName()`s joined by `|`/`&`, a
/// single class optionally already `\`-prefixed, an optional leading `?`).
pub fn render_type(type_str: &str) -> String {
    // trimNullable: a single leading `?`.
    let (nullable, trimmed) = match type_str.strip_prefix('?') {
        Some(rest) => (true, rest),
        None => (false, type_str),
    };
    if trimmed.contains('&') && !trimmed.contains('|') {
        // Pure intersection — members are class types, order preserved.
        let members: Vec<String> = trimmed
            .split('&')
            .map(|m| atomic_fqcn(m.trim_matches(['(', ')'])))
            .collect();
        let body = members.join("&");
        return if nullable { format!("?{body}") } else { body };
    }
    if trimmed.contains('|') {
        let mut members: Vec<AtomicSort> = trimmed
            .split('|')
            .map(|m| {
                let m = m.trim_matches(['(', ')']);
                if m.contains('&') {
                    // An intersection nested in a union sorts before atomics.
                    let inner: Vec<String> =
                        m.split('&').map(|x| atomic_fqcn(x.trim())).collect();
                    AtomicSort { sort_index: -1, name: inner.join("&"), sort_key: inner.join("&") }
                } else {
                    atomic_sort(m)
                }
            })
            .collect();
        members.sort_by(|a, b| {
            (a.sort_index, &a.sort_key).cmp(&(b.sort_index, &b.sort_key))
        });
        let body = members
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join("|");
        return if nullable { format!("?{body}") } else { body };
    }
    let atomic = atomic_fqcn(trimmed);
    if nullable {
        format!("?{atomic}")
    } else {
        atomic
    }
}

struct AtomicSort {
    sort_index: i32,
    name: String,
    sort_key: String,
}

/// Laminas `AtomicType` built-in precedence (class types sort_index 0).
fn builtin_precedence(lower: &str) -> Option<i32> {
    Some(match lower {
        "bool" => 1,
        "int" => 2,
        "float" => 3,
        "string" => 4,
        "array" => 5,
        "callable" => 6,
        "iterable" => 7,
        "object" => 8,
        "static" => 9,
        "mixed" => 10,
        "void" => 11,
        "false" => 12,
        "true" => 13,
        "null" => 14,
        "never" => 15,
        _ => return None,
    })
}

fn atomic_sort(name: &str) -> AtomicSort {
    let trimmed = name.strip_prefix('\\').unwrap_or(name);
    let lower = trimmed.to_ascii_lowercase();
    match builtin_precedence(&lower) {
        Some(p) => AtomicSort { sort_index: p, name: lower.clone(), sort_key: lower },
        None => AtomicSort {
            sort_index: 0,
            name: format!("\\{trimmed}"),
            // toString() (the union sort key) is the bare name, no `\`.
            sort_key: trimmed.to_owned(),
        },
    }
}

/// `AtomicType::fullyQualifiedName` — built-ins verbatim, class types with a
/// single leading backslash.
fn atomic_fqcn(name: &str) -> String {
    let trimmed = name.strip_prefix('\\').unwrap_or(name);
    let lower = trimmed.to_ascii_lowercase();
    if builtin_precedence(&lower).is_some() {
        lower
    } else {
        format!("\\{trimmed}")
    }
}

// ---- parameters ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    /// Magento-level type string (pre-Laminas), `None` = untyped.
    pub type_str: Option<String>,
    pub by_ref: bool,
    pub variadic: bool,
    /// `None` = no default (required parameter).
    pub default: Option<Val>,
}

impl Param {
    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(ty) = &self.type_str {
            out.push_str(&render_type(ty));
            out.push(' ');
        }
        if self.by_ref {
            out.push('&');
        }
        if self.variadic {
            out.push_str("... ");
        }
        out.push('$');
        out.push_str(&self.name);
        if let Some(default) = &self.default {
            out.push_str(" = ");
            out.push_str(&default.render());
        }
        out
    }
}

// ---- doc blocks ------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DocBlock {
    pub short: Option<String>,
    pub long: Option<String>,
    /// `(tag name, description)` → `@name description`.
    pub tags: Vec<(String, String)>,
}

impl DocBlock {
    pub fn is_empty(&self) -> bool {
        self.short.is_none() && self.long.is_none() && self.tags.is_empty()
    }

    /// `DocBlockGenerator::generate` at the given indentation (word-wrap off).
    fn render(&self, indent: &str) -> String {
        let mut content = String::new();
        if let Some(s) = &self.short {
            content.push_str(s);
            content.push(LF);
            content.push(LF);
        }
        if let Some(l) = &self.long {
            content.push_str(l);
            content.push(LF);
            content.push(LF);
        }
        for (name, desc) in &self.tags {
            content.push('@');
            content.push_str(name);
            if !desc.is_empty() {
                content.push(' ');
                content.push_str(desc);
            }
            content.push(LF);
        }
        let content = content.trim();
        let mut out = String::new();
        out.push_str(indent);
        out.push_str("/**");
        out.push(LF);
        for line in content.split(LF) {
            out.push_str(indent);
            out.push_str(" *");
            if !line.is_empty() {
                out.push(' ');
                out.push_str(line);
            }
            out.push(LF);
        }
        out.push_str(indent);
        out.push_str(" */");
        out.push(LF);
        out
    }
}

// ---- methods ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Protected,
    Private,
}

impl Visibility {
    fn keyword(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Method {
    pub name: String,
    pub visibility: Visibility,
    pub is_static: bool,
    pub is_final: bool,
    pub is_abstract: bool,
    pub returns_ref: bool,
    pub params: Vec<Param>,
    /// Magento-level return type string, `None` = none declared.
    pub return_type: Option<String>,
    /// Raw method body (no outer indentation); `None` for an interface method.
    pub body: Option<String>,
    pub doc: DocBlock,
}

impl Method {
    fn render(&self) -> String {
        let mut out = String::new();
        if !self.doc.is_empty() {
            out.push_str(&self.doc.render(INDENT));
        }
        out.push_str(INDENT);
        if self.is_abstract {
            out.push_str("abstract ");
        } else if self.is_final {
            out.push_str("final ");
        }
        out.push_str(self.visibility.keyword());
        if self.is_static {
            out.push_str(" static");
        }
        out.push_str(" function ");
        if self.returns_ref {
            out.push_str("& ");
        }
        out.push_str(&self.name);
        out.push('(');
        let params: Vec<String> = self.params.iter().map(Param::render).collect();
        out.push_str(&params.join(", "));
        out.push(')');
        if let Some(rt) = &self.return_type {
            out.push_str(": ");
            out.push_str(&render_type(rt));
        }
        if self.is_abstract || self.body.is_none() {
            out.push(';');
            return out;
        }
        out.push(LF);
        out.push_str(INDENT);
        out.push('{');
        out.push(LF);
        let body = self.body.as_deref().unwrap_or("").trim();
        if !body.is_empty() {
            out.push_str(&indent_body(body));
            out.push(LF);
        }
        out.push_str(INDENT);
        out.push('}');
        out.push(LF);
        out
    }
}

/// `MethodGenerator`'s body indentation: 8 spaces on every line EXCEPT a line
/// that is a bare `word;` (the `(?![a-zA-Z0-9_-]+;)` negative lookahead) — a
/// Laminas quirk faithfully reproduced. Empty lines stay empty.
fn indent_body(body: &str) -> String {
    let pad = "        "; // INDENT + INDENT
    let mut out = String::new();
    let mut first = true;
    for line in body.split(LF) {
        if !first {
            out.push(LF);
        }
        first = false;
        if line.is_empty() || is_bare_label(line) {
            out.push_str(line);
        } else {
            out.push_str(pad);
            out.push_str(line);
        }
    }
    out
}

/// True when the line starts with `[a-zA-Z0-9_-]+;` (the lookahead's target).
fn is_bare_label(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
            i += 1;
        } else {
            break;
        }
    }
    i > 0 && bytes.get(i) == Some(&b';')
}

// ---- properties ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Property {
    pub name: String,
    pub visibility: Visibility,
    /// `None` renders `... = null;` (the generators' only usage).
    pub default: Option<Val>,
    pub doc: DocBlock,
}

impl Property {
    fn render(&self) -> String {
        let mut out = String::new();
        if !self.doc.is_empty() {
            out.push_str(&self.doc.render(INDENT));
        }
        out.push_str(INDENT);
        out.push_str(self.visibility.keyword());
        out.push_str(" $");
        out.push_str(&self.name);
        out.push_str(" = ");
        match &self.default {
            Some(v) => out.push_str(&v.render()),
            None => out.push_str("null"),
        }
        out.push(';');
        out
    }
}

// ---- class -----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Class {
    /// Namespace (no trailing `;`), `None` = global namespace.
    pub namespace: Option<String>,
    /// Short name (the class lives in `namespace`).
    pub name: String,
    pub is_interface: bool,
    /// FQCN of the extended class (a class), rendered short-or-complete.
    pub extends: Option<String>,
    /// Extended interfaces (for an interface) OR implemented interfaces.
    pub implements: Vec<String>,
    /// `use \Trait;` lines, rendered first inside the body.
    pub traits: Vec<String>,
    pub doc: DocBlock,
    pub properties: Vec<Property>,
    pub methods: Vec<Method>,
}

impl Class {
    /// The full generated file, `<?php` header included and `_fixCodeStyle`
    /// applied — byte-identical to a Magento code-generator's output.
    pub fn render(&self) -> String {
        let mut out = String::from("<?php\n");
        out.push_str(&self.render_body());
        fix_code_style(&out)
    }

    fn render_body(&self) -> String {
        let mut out = String::new();
        if let Some(ns) = &self.namespace {
            out.push_str("namespace ");
            out.push_str(ns);
            out.push_str(";\n\n");
        }
        if !self.doc.is_empty() {
            out.push_str(&self.doc.render(""));
        }
        out.push_str(if self.is_interface { "interface " } else { "class " });
        out.push_str(&self.name);
        if let Some(extends) = &self.extends {
            out.push_str(" extends ");
            out.push_str(&self.short_or_complete(extends));
        }
        if !self.implements.is_empty() {
            let kw = if self.is_interface { " extends " } else { " implements " };
            out.push_str(kw);
            let rendered: Vec<String> =
                self.implements.iter().map(|i| self.short_or_complete(i)).collect();
            out.push_str(&rendered.join(", "));
        }
        out.push_str("\n{\n");

        let mut sections: Vec<String> = Vec::new();
        if !self.traits.is_empty() {
            sections.push(
                self.traits
                    .iter()
                    .map(|t| format!("{INDENT}use {};", self.short_or_complete(t)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        if !self.properties.is_empty() {
            sections.push(
                self.properties
                    .iter()
                    .map(Property::render)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            );
        }
        if !self.methods.is_empty() {
            sections.push(
                self.methods.iter().map(Method::render).collect::<Vec<_>>().join("\n"),
            );
        }
        let contents = sections.join("\n\n");
        let contents = contents.trim_end_matches('\n');
        out.push_str(contents);
        if !contents.is_empty() {
            out.push(LF);
        }
        out.push('}');
        out.push(LF);
        out
    }

    /// `ClassGenerator::generateShortOrCompleteClassname` with no use imports:
    /// a class in the current namespace renders short, else `\`-prefixed FQCN.
    fn short_or_complete(&self, fqcn: &str) -> String {
        let trimmed = fqcn.trim_start_matches('\\');
        let (ns, short) = match trimmed.rfind('\\') {
            Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        let current = self.namespace.as_deref().unwrap_or("");
        if ns == current {
            short.to_owned()
        } else {
            format!("\\{trimmed}")
        }
    }
}

/// `EntityAbstract::_fixCodeStyle`: normalize legacy array syntax spacing and
/// collapse blank lines just inside `{ … }`.
fn fix_code_style(code: &str) -> String {
    let code = code.replace(" array (", " array(");
    // `{\n{2,}` → `{\n`
    let code = collapse_after_brace(&code);
    // `\n{2,}}` → `\n}`
    collapse_before_brace(&code)
}

fn collapse_after_brace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        out.push(bytes[i] as char);
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'\n') {
            // Emit one newline, skip the rest of the run.
            out.push('\n');
            i += 2;
            while bytes.get(i) == Some(&b'\n') {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

fn collapse_before_brace(s: &str) -> String {
    // Replace any run of >=2 newlines immediately followed by `}` with `\n}`.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut j = i;
            while bytes.get(j) == Some(&b'\n') {
                j += 1;
            }
            if j - i >= 2 && bytes.get(j) == Some(&b'}') {
                out.push('\n');
                i = j;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_rendering() {
        assert_eq!(render_type("array"), "array");
        assert_eq!(render_type("void"), "void");
        assert_eq!(render_type("static"), "static");
        assert_eq!(render_type("string"), "string");
        assert_eq!(render_type("\\Magento\\Foo\\Bar"), "\\Magento\\Foo\\Bar");
        assert_eq!(render_type("Magento\\Foo\\Bar"), "\\Magento\\Foo\\Bar");
        assert_eq!(render_type("?string"), "?string");
        assert_eq!(render_type("?\\Magento\\Foo"), "?\\Magento\\Foo");
        // union sorts by (builtin precedence, name); class(0) before builtins.
        assert_eq!(render_type("int|false"), "int|false");
        assert_eq!(render_type("false|int"), "int|false");
        assert_eq!(render_type("string|int"), "int|string");
        assert_eq!(render_type("Magento\\Foo|null"), "\\Magento\\Foo|null");
    }

    #[test]
    fn value_rendering() {
        assert_eq!(Val::Bool(true).render(), "true");
        assert_eq!(Val::Null.render(), "null");
        assert_eq!(Val::Int(5).render(), "5");
        assert_eq!(Val::Str("x".into()).render(), "'x'");
        assert_eq!(Val::Str("a\\b'c".into()).render(), "'a\\\\b\\'c'");
        assert_eq!(Val::Array(vec![]).render(), "[]");
        assert_eq!(
            Val::Array(vec![(None, Val::Int(1)), (None, Val::Int(2))]).render(),
            "[1, 2]"
        );
        assert_eq!(
            Val::Array(vec![(Some(ArrKey::Str("k".into())), Val::Int(1))]).render(),
            "['k' => 1]"
        );
    }

    #[test]
    fn body_indentation() {
        assert_eq!(
            indent_body("return $this->x();"),
            "        return $this->x();"
        );
        assert_eq!(
            indent_body("if ($a) {\n    $b = 1;\n}\nreturn $b;"),
            "        if ($a) {\n            $b = 1;\n        }\n        return $b;"
        );
    }

    #[test]
    fn factory_like_method() {
        let m = Method {
            name: "process".into(),
            visibility: Visibility::Public,
            is_static: false,
            is_final: false,
            is_abstract: false,
            returns_ref: false,
            params: vec![
                Param {
                    name: "request".into(),
                    type_str: Some("Magento\\Framework\\App\\RequestInterface".into()),
                    by_ref: false,
                    variadic: false,
                    default: None,
                },
                Param {
                    name: "pathInfo".into(),
                    type_str: None,
                    by_ref: false,
                    variadic: false,
                    default: None,
                },
            ],
            return_type: None,
            body: Some("return $this->_getSubject()->process($request, $pathInfo);".into()),
            doc: DocBlock { short: Some("{@inheritdoc}".into()), ..Default::default() },
        };
        let out = m.render();
        assert!(out.contains(
            "    public function process(\\Magento\\Framework\\App\\RequestInterface $request, $pathInfo)\n    {\n        return $this->_getSubject()->process($request, $pathInfo);\n    }\n"
        ));
        assert!(out.starts_with("    /**\n     * {@inheritdoc}\n     */\n"));
    }
}
