//! The declaration parser: namespace + imports, type headers, and member
//! signatures, with every name resolved to an FQCN through the file's use
//! map. Bodies are skipped by the scanner; unknown constructs become
//! [`ParseIssue`]s, never guesses.

use std::collections::HashMap;

use crate::model::*;
use crate::scan::Cursor;

/// Parse one PHP source file. Never panics; never reads anything but `src`.
///
/// A name declared more than once (both branches of a `class_exists(…)`
/// guard) keeps its FIRST declaration. Callers that can answer "is this
/// class loadable" use [`parse_file_with`] to keep the branch PHP would run.
pub fn parse_file(src: &[u8]) -> FileMeta {
    parse(src, None)
}

/// [`parse_file`], choosing between same-name declarations by evaluating
/// their [`ClassGuard`]s with `loadable`.
///
/// A `class_exists(…)` guard declares one name in both branches, and PHP
/// defines whichever branch runs. Per name, the first declaration whose guard
/// holds is kept (an unguarded or unreadable-condition declaration always
/// holds); when none holds, the first is kept.
pub fn parse_file_with(src: &[u8], loadable: impl Fn(&str) -> bool) -> FileMeta {
    parse(src, Some(&loadable))
}

fn parse(src: &[u8], loadable: Option<&dyn Fn(&str) -> bool>) -> FileMeta {
    let mut p = Parser {
        cur: Cursor::new(src),
        ns: String::new(),
        uses: HashMap::new(),
        guards: Vec::new(),
        out: FileMeta::default(),
    };
    // Everything before the first open tag is HTML.
    p.cur.skip_html_until_open_tag();
    p.top_level();
    let holds = |d: &ClassMeta| match (&d.guard, loadable) {
        (Some(g), Some(loadable)) => loadable(&g.class) == g.requires,
        _ => true,
    };
    let mut chosen: HashMap<String, usize> = HashMap::new();
    for (i, d) in p.out.declarations.iter().enumerate() {
        match chosen.get(&d.fqcn) {
            None => {
                chosen.insert(d.fqcn.clone(), i);
            }
            Some(&j) if !holds(&p.out.declarations[j]) && holds(d) => {
                chosen.insert(d.fqcn.clone(), i);
            }
            Some(_) => {}
        }
    }
    let mut i = 0;
    p.out.declarations.retain(|d| {
        let keep = chosen.get(&d.fqcn) == Some(&i);
        i += 1;
        keep
    });
    p.out
}

fn invert(g: ClassGuard) -> ClassGuard {
    ClassGuard { requires: !g.requires, ..g }
}

const BUILTIN_TYPES: [&str; 17] = [
    "array", "callable", "bool", "float", "int", "string", "iterable", "object", "mixed",
    "never", "void", "null", "false", "true", "self", "parent", "static",
];

struct Parser<'a> {
    cur: Cursor<'a>,
    ns: String,
    /// lowercased alias -> FQCN (PHP alias lookup is case-insensitive).
    uses: HashMap<String, String>,
    /// The guards of the enclosing `if`/`else` branches, innermost last
    /// (`None` = a branch whose condition we can't read).
    guards: Vec<Option<ClassGuard>>,
    out: FileMeta,
}

impl<'a> Parser<'a> {
    fn issue(&mut self, offset: usize, message: impl Into<String>) {
        self.out.issues.push(ParseIssue {
            offset,
            message: message.into(),
        });
    }

    // ---- top level -----------------------------------------------------

    fn top_level(&mut self) {
        self.scan_declarations(false);
    }

    /// Scan declarations at file scope (`nested = false`) or inside a `{ … }`
    /// block (`nested = true`, returns after consuming the matching `}`).
    fn scan_declarations(&mut self, nested: bool) {
        let mut is_abstract = false;
        let mut is_final = false;
        let mut is_readonly = false;
        let mut pending_attrs: Vec<String> = Vec::new();
        // The guard of the braced `if` block that just closed, for its `else`.
        let mut last_if: Option<ClassGuard> = None;
        loop {
            self.cur.skip_insignificant();
            let Some(b) = self.cur.peek() else { return };
            match b {
                b'#' if self.cur.peek_at(1) == Some(b'[') => {
                    let attrs = self.parse_attribute_names();
                    pending_attrs.extend(attrs);
                }
                b'}' => {
                    self.cur.bump();
                    if nested {
                        return; // the block this scan was opened for just closed
                    }
                }
                b';' => self.cur.bump(),
                b'?' if self.cur.peek_at(1) == Some(b'>') => {
                    self.cur.pos += 2;
                    self.cur.skip_html_until_open_tag();
                }
                _ if self.cur.at_ident_start() => {
                    let start = self.cur.pos;
                    let word = self.cur.read_ident().unwrap_or("").to_ascii_lowercase();
                    match word.as_str() {
                        "namespace" => self.parse_namespace(),
                        "use" => self.parse_use_import(),
                        "abstract" => is_abstract = true,
                        "final" => is_final = true,
                        "readonly" => {
                            // `readonly` is also a valid function name; only a
                            // modifier when a declaration keyword follows.
                            self.cur.skip_insignificant();
                            if self.cur.starts_with_ci(b"class")
                                || self.cur.starts_with_ci(b"final")
                                || self.cur.starts_with_ci(b"abstract")
                            {
                                is_readonly = true;
                            } else {
                                self.cur.pos = start;
                                self.cur.skip_statement();
                            }
                        }
                        "class" | "interface" | "trait" | "enum" => {
                            let kind = match word.as_str() {
                                "class" => ClassKind::Class,
                                "interface" => ClassKind::Interface,
                                "trait" => ClassKind::Trait,
                                _ => ClassKind::Enum,
                            };
                            let attrs = std::mem::take(&mut pending_attrs);
                            self.parse_declaration(
                                kind, start, is_abstract, is_final, is_readonly, attrs, nested,
                            );
                            is_abstract = false;
                            is_final = false;
                            is_readonly = false;
                        }
                        "function" => {
                            pending_attrs.clear();
                            self.skip_function();
                        }
                        // A class can be declared inside an `if`/`else` guard —
                        // the `class_exists(…)` compat-shim pattern (Hyva
                        // modules, PHP polyfills). Descend into the block so the
                        // conditional class enters the universe; `parse_file`
                        // keeps the FIRST declaration of each name, mirroring the
                        // `if`-branch a store with the guarded dependency takes.
                        //
                        // ALTERNATIVE syntax (`if (…): … else: … endif;`) has no
                        // braces, so its guarded class sits at brace depth 0 —
                        // exactly where Magento's brace-tracking FileClassScanner
                        // DOES record it (unlike a braced `if { class }`, at depth
                        // 1, which it skips). So we eat the `:` and let the class
                        // parse inline as a normal top-level declaration.
                        //
                        // Each branch records its `class_exists(…)` guard, so
                        // `parse_file_with` can pick the branch PHP would run.
                        // An `elseif`, and anything after one, is unguarded:
                        // its branch also depends on the earlier conditions.
                        "if" | "elseif" => {
                            pending_attrs.clear();
                            let guard = self.paren_condition_guard();
                            let guard = if word == "if" { guard } else { None };
                            if self.eat_alt_syntax_colon() {
                                if word == "elseif" {
                                    self.guards.pop();
                                }
                                self.guards.push(guard);
                            } else {
                                last_if = guard.clone();
                                self.descend_if_braced(guard);
                            }
                        }
                        "else" => {
                            // `else { … }` descends; `else: …` (alt syntax) eats
                            // the `:` and continues inline; `else if …` leaves the
                            // `if` for the next iteration's handler.
                            pending_attrs.clear();
                            if self.eat_alt_syntax_colon() {
                                let inverted = self.guards.pop().flatten().map(invert);
                                self.guards.push(inverted);
                            } else {
                                let inverted = last_if.take().map(invert);
                                self.descend_if_braced(inverted);
                            }
                        }
                        "endif" => {
                            pending_attrs.clear();
                            self.guards.pop();
                            self.cur.skip_statement();
                        }
                        _ => {
                            // declare(...), const, expressions — top-level code
                            // we don't model.
                            pending_attrs.clear();
                            self.cur.pos = start;
                            self.cur.skip_statement();
                        }
                    }
                }
                _ => self.cur.skip_statement(),
            }
        }
    }

    /// Skip a `( … )` control-structure condition if one is next, returning
    /// its guard when the whole condition is `[!]class_exists(X::class)` (or
    /// `interface_exists`/`trait_exists`/`enum_exists`, or a string argument).
    fn paren_condition_guard(&mut self) -> Option<ClassGuard> {
        self.cur.skip_insignificant();
        if self.cur.peek() != Some(b'(') {
            return None;
        }
        self.cur.bump();
        let start = self.cur.pos;
        let guard = self.read_exists_call();
        self.cur.pos = start;
        self.cur.skip_parens_body(1);
        guard
    }

    /// Read `[!]class_exists(X::class[, …])` followed by the condition's
    /// closing `)`. Leaves the cursor anywhere; the caller rewinds.
    fn read_exists_call(&mut self) -> Option<ClassGuard> {
        self.cur.skip_insignificant();
        let mut requires = true;
        while self.cur.peek() == Some(b'!') {
            self.cur.bump();
            requires = !requires;
            self.cur.skip_insignificant();
        }
        let func = self.read_qualified_name()?;
        let func = func.rsplit('\\').next().unwrap_or(&func).to_ascii_lowercase();
        if !matches!(
            func.as_str(),
            "class_exists" | "interface_exists" | "trait_exists" | "enum_exists"
        ) {
            return None;
        }
        self.cur.skip_insignificant();
        if self.cur.peek() != Some(b'(') {
            return None;
        }
        self.cur.bump();
        self.cur.skip_insignificant();
        let class = match self.cur.peek()? {
            q @ (b'\'' | b'"') => {
                let open = self.cur.pos;
                if q == b'\'' {
                    self.cur.skip_single_quoted();
                } else {
                    self.cur.skip_double_quoted(q);
                }
                let raw = std::str::from_utf8(&self.cur.src[open + 1..self.cur.pos - 1]).ok()?;
                if raw.contains('$') {
                    return None;
                }
                raw.replace("\\\\", "\\").trim_start_matches('\\').to_owned()
            }
            _ => {
                let name = self.read_qualified_name()?;
                self.cur.skip_insignificant();
                if !self.cur.starts_with(b"::") {
                    return None;
                }
                self.cur.pos += 2;
                if !self.cur.eat_keyword("class") {
                    return None;
                }
                self.resolve(&name)
            }
        };
        // The optional autoload flag, then the call's and the condition's `)`.
        self.cur.skip_parens_body(1);
        self.cur.skip_insignificant();
        (self.cur.peek() == Some(b')')).then_some(ClassGuard { class, requires })
    }

    /// Consume a `:` that opens an alternative-syntax block (`if (…): … endif;`),
    /// returning whether one was found. The following statements then parse
    /// inline (no brace boundary), so a guarded class at brace depth 0 is
    /// recorded — matching Magento's FileClassScanner. `endif`/`endforeach`/…
    /// are plain identifiers the main loop skips as statements.
    fn eat_alt_syntax_colon(&mut self) -> bool {
        self.cur.skip_insignificant();
        if self.cur.peek() == Some(b':') {
            self.cur.bump();
            true
        } else {
            false
        }
    }

    /// If a `{` follows, descend into the block scanning for declarations
    /// (consuming through its matching `}`). A braceless body is left for the
    /// main loop — a braceless `if (x) class C {}` still registers `C`.
    fn descend_if_braced(&mut self, guard: Option<ClassGuard>) {
        self.cur.skip_insignificant();
        if self.cur.peek() == Some(b'{') {
            self.cur.bump();
            self.guards.push(guard);
            self.scan_declarations(true);
            self.guards.pop();
        }
    }

    /// Parse a `#[Attr, Other(args)]` group, returning resolved names.
    fn parse_attribute_names(&mut self) -> Vec<String> {
        self.cur.pos += 2; // "#["
        let mut names = Vec::new();
        loop {
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b']') => {
                    self.cur.bump();
                    return names;
                }
                Some(b',') => {
                    self.cur.bump();
                }
                None => return names,
                _ => {
                    let Some(name) = self.read_qualified_name() else {
                        // Not a name where one belongs: fall back to a plain
                        // balanced skip of the rest of the group.
                        let mut depth = 1usize;
                        while let Some(b) = self.cur.peek() {
                            match b {
                                b'[' => depth += 1,
                                b']' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        self.cur.bump();
                                        return names;
                                    }
                                }
                                _ => {}
                            }
                            self.cur.bump();
                        }
                        return names;
                    };
                    names.push(self.resolve(&name));
                    self.cur.skip_insignificant();
                    if self.cur.peek() == Some(b'(') {
                        self.cur.bump();
                        self.cur.skip_parens_body(1);
                    }
                }
            }
        }
    }

    fn parse_namespace(&mut self) {
        self.cur.skip_insignificant();
        let mut ns = String::new();
        loop {
            match self.cur.read_ident() {
                Some(seg) => ns.push_str(seg),
                None => break,
            }
            if self.cur.peek() == Some(b'\\') {
                self.cur.bump();
                ns.push('\\');
            } else {
                break;
            }
        }
        self.ns = ns;
        self.cur.skip_insignificant();
        if matches!(self.cur.peek(), Some(b';' | b'{')) {
            self.cur.bump();
        }
    }

    fn parse_use_import(&mut self) {
        loop {
            self.cur.skip_insignificant();
            // `use function f;` / `use const C;` — consumed but not mapped
            // (class resolution never sees them).
            let mut is_class_import = true;
            if self.cur.eat_keyword("function") || self.cur.eat_keyword("const") {
                is_class_import = false;
            }
            self.cur.skip_insignificant();
            if self.cur.peek() == Some(b'\\') {
                self.cur.bump(); // leading backslash on an import is redundant
            }
            let mut name = String::new();
            let mut grouped = false;
            loop {
                self.cur.skip_insignificant();
                if self.cur.peek() == Some(b'{') {
                    // group use: `use A\B\{C, D as E};`
                    self.cur.bump();
                    grouped = true;
                    self.parse_group_use(&name, is_class_import);
                    break;
                }
                match self.cur.read_ident() {
                    Some(seg) => name.push_str(seg),
                    None => break,
                }
                if self.cur.peek() == Some(b'\\') {
                    self.cur.bump();
                    name.push('\\');
                } else {
                    break;
                }
            }
            if !grouped {
                self.cur.skip_insignificant();
                let alias = if self.cur.eat_keyword("as") {
                    self.cur.skip_insignificant();
                    self.cur.read_ident().map(str::to_owned)
                } else {
                    None
                };
                if is_class_import && !name.is_empty() {
                    let alias = alias
                        .unwrap_or_else(|| name.rsplit('\\').next().unwrap_or(&name).to_owned());
                    self.uses.insert(alias.to_ascii_lowercase(), name.clone());
                }
            }
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b',') => {
                    self.cur.bump();
                    continue;
                }
                Some(b';') => {
                    self.cur.bump();
                    return;
                }
                _ => return,
            }
        }
    }

    fn parse_group_use(&mut self, prefix: &str, default_class_import: bool) {
        loop {
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b'}') => {
                    self.cur.bump();
                    return;
                }
                Some(b',') => {
                    self.cur.bump();
                }
                None => return,
                _ => {
                    let mut is_class_import = default_class_import;
                    if self.cur.eat_keyword("function") || self.cur.eat_keyword("const") {
                        is_class_import = false;
                    }
                    self.cur.skip_insignificant();
                    let mut rel = String::new();
                    loop {
                        match self.cur.read_ident() {
                            Some(seg) => rel.push_str(seg),
                            None => break,
                        }
                        if self.cur.peek() == Some(b'\\') {
                            self.cur.bump();
                            rel.push('\\');
                        } else {
                            break;
                        }
                    }
                    self.cur.skip_insignificant();
                    let alias = if self.cur.eat_keyword("as") {
                        self.cur.skip_insignificant();
                        self.cur.read_ident().map(str::to_owned)
                    } else {
                        None
                    };
                    if is_class_import && !rel.is_empty() {
                        let full = format!("{prefix}{rel}");
                        let alias = alias
                            .unwrap_or_else(|| rel.rsplit('\\').next().unwrap_or(&rel).to_owned());
                        self.uses.insert(alias.to_ascii_lowercase(), full);
                    }
                }
            }
        }
    }

    /// Top-level `function f(...) {...}` — skipped wholesale.
    fn skip_function(&mut self) {
        loop {
            self.cur.skip_insignificant();
            match self.cur.peek() {
                None => return,
                Some(b'(') => {
                    self.cur.bump();
                    self.cur.skip_parens_body(1);
                }
                Some(b'{') => {
                    self.cur.bump();
                    self.cur.skip_code_until_close_brace();
                    return;
                }
                Some(b';') => {
                    self.cur.bump();
                    return;
                }
                _ => self.cur.bump(),
            }
        }
    }

    // ---- declarations ----------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn parse_declaration(
        &mut self,
        kind: ClassKind,
        offset: usize,
        is_abstract: bool,
        is_final: bool,
        is_readonly: bool,
        attributes: Vec<String>,
        conditional: bool,
    ) {
        self.cur.skip_insignificant();
        let Some(name) = self.cur.read_ident() else {
            self.issue(self.cur.pos, "expected a name after class/interface/trait/enum");
            self.cur.skip_statement();
            return;
        };
        let fqcn = if self.ns.is_empty() {
            name.to_owned()
        } else {
            format!("{}\\{}", self.ns, name)
        };
        let mut class = ClassMeta {
            kind,
            fqcn,
            is_abstract,
            is_final,
            is_readonly,
            extends: Vec::new(),
            implements: Vec::new(),
            traits: Vec::new(),
            trait_insteadof: Vec::new(),
            trait_aliases: Vec::new(),
            constants: Vec::new(),
            cases: Vec::new(),
            enum_backing: None,
            methods: Vec::new(),
            properties: Vec::new(),
            attributes,
            offset,
            uses: self.uses.iter().map(|(a, f)| (a.clone(), f.clone())).collect(),
            conditional,
            guard: self.guards.last().cloned().flatten(),
        };

        self.cur.skip_insignificant();
        if kind == ClassKind::Enum && self.cur.peek() == Some(b':') {
            self.cur.bump();
            self.cur.skip_insignificant();
            class.enum_backing = self.cur.read_ident().map(|s| s.to_ascii_lowercase());
            self.cur.skip_insignificant();
        }
        if self.cur.eat_keyword("extends") {
            class.extends = self.parse_name_list();
            self.cur.skip_insignificant();
        }
        if self.cur.eat_keyword("implements") {
            class.implements = self.parse_name_list();
            self.cur.skip_insignificant();
        }
        if self.cur.peek() == Some(b'{') {
            self.cur.bump();
            self.parse_members(&mut class);
        } else {
            self.issue(self.cur.pos, format!("expected '{{' after {} header", class.fqcn));
            self.cur.skip_statement();
        }
        self.out.declarations.push(class);
    }

    /// `A, B\C, \D` — resolved.
    fn parse_name_list(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        loop {
            self.cur.skip_insignificant();
            match self.read_qualified_name() {
                Some(n) => names.push(self.resolve(&n)),
                None => break,
            }
            self.cur.skip_insignificant();
            if self.cur.peek() == Some(b',') {
                self.cur.bump();
            } else {
                break;
            }
        }
        names
    }

    /// Raw qualified name as written: `\A\B`, `A\B`, `namespace\B`, `A`.
    fn read_qualified_name(&mut self) -> Option<String> {
        let mut name = String::new();
        if self.cur.peek() == Some(b'\\') {
            self.cur.bump();
            name.push('\\');
        }
        if !self.cur.at_ident_start() {
            return if name.is_empty() { None } else { Some(name) };
        }
        loop {
            match self.cur.read_ident() {
                Some(seg) => name.push_str(seg),
                None => break,
            }
            if self.cur.peek() == Some(b'\\') && matches!(self.cur.peek_at(1), Some(b) if b == b'_' || b.is_ascii_alphabetic() || b >= 0x80)
            {
                self.cur.bump();
                name.push('\\');
            } else {
                break;
            }
        }
        Some(name)
    }

    /// PHP name resolution: absolute, alias, `namespace\`, else current
    /// namespace. Returns an FQCN without a leading backslash.
    fn resolve(&self, name: &str) -> String {
        if let Some(rest) = name.strip_prefix('\\') {
            return rest.to_owned();
        }
        let (first, rest) = match name.split_once('\\') {
            Some((f, r)) => (f, Some(r)),
            None => (name, None),
        };
        if first.eq_ignore_ascii_case("namespace") {
            return match rest {
                Some(r) if !self.ns.is_empty() => format!("{}\\{}", self.ns, r),
                Some(r) => r.to_owned(),
                None => self.ns.clone(),
            };
        }
        if let Some(mapped) = self.uses.get(&first.to_ascii_lowercase()) {
            return match rest {
                Some(r) => format!("{mapped}\\{r}"),
                None => mapped.clone(),
            };
        }
        if self.ns.is_empty() {
            name.to_owned()
        } else {
            format!("{}\\{}", self.ns, name)
        }
    }

    // ---- class members -----------------------------------------------------

    fn parse_members(&mut self, class: &mut ClassMeta) {
        loop {
            self.cur.skip_insignificant();
            let Some(b) = self.cur.peek() else { return };
            match b {
                b'}' => {
                    self.cur.bump();
                    return;
                }
                b';' => self.cur.bump(),
                b'#' if self.cur.peek_at(1) == Some(b'[') => self.cur.skip_attribute(),
                _ => {
                    let start = self.cur.pos;
                    let mut visibility = None;
                    let mut is_static = false;
                    let mut is_abstract = false;
                    let mut is_final = false;
                    let mut is_readonly = false;
                    // Modifier run.
                    loop {
                        self.cur.skip_insignificant();
                        if self.cur.eat_keyword("public") {
                            visibility.get_or_insert(Visibility::Public);
                            self.eat_set_suffix();
                        } else if self.cur.eat_keyword("protected") {
                            visibility.get_or_insert(Visibility::Protected);
                            self.eat_set_suffix();
                        } else if self.cur.eat_keyword("private") {
                            visibility.get_or_insert(Visibility::Private);
                            self.eat_set_suffix();
                        } else if self.cur.eat_keyword("static") {
                            is_static = true;
                        } else if self.cur.eat_keyword("abstract") {
                            is_abstract = true;
                        } else if self.cur.eat_keyword("final") {
                            is_final = true;
                        } else if self.cur.eat_keyword("readonly") {
                            is_readonly = true;
                        } else if self.cur.eat_keyword("var") {
                            visibility.get_or_insert(Visibility::Public);
                        } else {
                            break;
                        }
                    }
                    let visibility = visibility.unwrap_or(Visibility::Public);

                    self.cur.skip_insignificant();
                    if self.cur.eat_keyword("function") {
                        let offset = start;
                        self.parse_method(class, visibility, is_static, is_abstract, is_final, offset);
                    } else if self.cur.eat_keyword("const") {
                        self.parse_const(class, visibility);
                    } else if self.cur.eat_keyword("use") {
                        self.parse_trait_use(class);
                    } else if class.kind == ClassKind::Enum && self.cur.eat_keyword("case") {
                        self.parse_enum_case(class);
                    } else if self.at_property_start() {
                        let readonly = is_readonly || class.is_readonly;
                        self.parse_property(class, visibility, is_static, readonly);
                    } else {
                        self.issue(self.cur.pos, "unexpected token in class body");
                        // Bounded recovery: skip to the next plausible member
                        // start so one junk run costs one issue, not the rest
                        // of the class.
                        while let Some(b) = self.cur.peek() {
                            if matches!(b, b';' | b'}' | b'$' | b'#') || self.cur.at_ident_start()
                            {
                                break;
                            }
                            self.cur.bump();
                        }
                        if self.cur.pos == start {
                            // Guarantee progress on degenerate input.
                            self.cur.bump();
                        }
                    }
                }
            }
        }
    }

    /// Asymmetric visibility (8.4): `private(set)` etc. after a visibility
    /// keyword. Consumed and ignored — set-visibility never affects DI.
    fn eat_set_suffix(&mut self) {
        if self.cur.peek() == Some(b'(') && self.cur.starts_with_ci(b"(set)") {
            self.cur.pos += 5;
        }
    }

    fn at_property_start(&mut self) -> bool {
        matches!(self.cur.peek(), Some(b'$' | b'?' | b'(' | b'\\')) || self.cur.at_ident_start()
    }

    /// `[type] $name [= expr] [, $more…] (; | { hooks })`. Properties matter
    /// to DI only through `new X()` parameter defaults, whose `var_export`
    /// lists the object's initialized properties. Property hooks (8.4) look
    /// like a body and are skipped like one; the property is only flagged
    /// [`PropertyMeta::hooked`].
    fn parse_property(
        &mut self,
        class: &mut ClassMeta,
        visibility: Visibility,
        is_static: bool,
        is_readonly: bool,
    ) {
        // Optional type expression.
        let ty = if self.cur.peek() != Some(b'$') {
            let ty = self.parse_type_expr();
            self.cur.skip_insignificant();
            ty
        } else {
            None
        };
        loop {
            if self.cur.peek() != Some(b'$') {
                self.issue(self.cur.pos, "expected '$' in property declaration");
                let _ = self.cur.capture_expr_until(b";{");
                break;
            }
            self.cur.bump();
            let name = self.cur.read_ident().unwrap_or("").to_owned();
            self.cur.skip_insignificant();
            let default = if self.cur.peek() == Some(b'=') {
                self.cur.bump();
                self.cur.skip_insignificant();
                // A hook body may follow the default directly; capture stops
                // on a depth-0 `{` before it could open a nesting level.
                Some(self.cur.capture_expr_until(b",;{"))
            } else {
                None
            };
            class.properties.push(PropertyMeta {
                name,
                visibility,
                is_static,
                is_readonly,
                ty: ty.clone(),
                default,
                hooked: false,
            });
            self.cur.skip_insignificant();
            if self.cur.peek() == Some(b',') {
                self.cur.bump();
                self.cur.skip_insignificant();
                continue;
            }
            break;
        }
        match self.cur.peek() {
            Some(b';') => self.cur.bump(),
            Some(b'{') => {
                self.cur.bump();
                self.cur.skip_code_until_close_brace(); // 8.4 hooks
                // Hooks are only legal on a single-property declaration.
                if let Some(p) = class.properties.last_mut() {
                    p.hooked = true;
                }
            }
            _ => {}
        }
    }

    fn parse_const(&mut self, class: &mut ClassMeta, visibility: Visibility) {
        // Typed constants (8.3): `const TYPE NAME = …`. Disambiguate by
        // looking for `=` after the first identifier.
        self.cur.skip_insignificant();
        let mut ty = None;
        let checkpoint = self.cur.pos;
        let first = self.read_qualified_name();
        self.cur.skip_insignificant();
        let name = if self.cur.peek() == Some(b'=') && first.is_some() {
            first.unwrap()
        } else {
            // The first token was a type; re-parse it properly.
            self.cur.pos = checkpoint;
            ty = self.parse_type_expr();
            self.cur.skip_insignificant();
            match self.cur.read_ident() {
                Some(n) => n.to_owned(),
                None => {
                    self.issue(self.cur.pos, "expected constant name");
                    self.cur.skip_statement();
                    return;
                }
            }
        };
        let mut name = name;
        loop {
            self.cur.skip_insignificant();
            if self.cur.peek() == Some(b'=') {
                self.cur.bump();
                self.cur.skip_insignificant();
                let value = self.cur.capture_expr_until(&[b',', b';']);
                class.constants.push(ConstMeta {
                    name: name.clone(),
                    visibility,
                    ty: ty.clone(),
                    value,
                });
            }
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b',') => {
                    self.cur.bump();
                    self.cur.skip_insignificant();
                    match self.cur.read_ident() {
                        Some(n) => name = n.to_owned(),
                        None => break,
                    }
                }
                _ => break,
            }
        }
        if self.cur.peek() == Some(b';') {
            self.cur.bump();
        }
    }

    fn parse_enum_case(&mut self, class: &mut ClassMeta) {
        self.cur.skip_insignificant();
        if let Some(name) = self.cur.read_ident() {
            class.cases.push(name.to_owned());
        }
        self.cur.skip_insignificant();
        if self.cur.peek() == Some(b'=') {
            self.cur.bump();
            let _ = self.cur.capture_expr_until(&[b';']);
        }
        if self.cur.peek() == Some(b';') {
            self.cur.bump();
        }
    }

    fn parse_trait_use(&mut self, class: &mut ClassMeta) {
        loop {
            self.cur.skip_insignificant();
            match self.read_qualified_name() {
                Some(n) => class.traits.push(self.resolve(&n)),
                None => break,
            }
            self.cur.skip_insignificant();
            if self.cur.peek() == Some(b',') {
                self.cur.bump();
            } else {
                break;
            }
        }
        self.cur.skip_insignificant();
        match self.cur.peek() {
            Some(b';') => {
                self.cur.bump();
            }
            Some(b'{') => {
                self.cur.bump();
                self.parse_trait_adaptations(class);
            }
            _ => {}
        }
    }

    fn parse_trait_adaptations(&mut self, class: &mut ClassMeta) {
        loop {
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b'}') => {
                    self.cur.bump();
                    return;
                }
                Some(b';') => {
                    self.cur.bump();
                }
                None => return,
                _ => {
                    let Some(first) = self.read_qualified_name() else {
                        self.issue(self.cur.pos, "unexpected token in trait adaptation");
                        self.cur.skip_statement();
                        continue;
                    };
                    let (trait_fqcn, method) = if self.cur.starts_with(b"::") {
                        self.cur.pos += 2;
                        let m = self.cur.read_ident().unwrap_or("").to_owned();
                        (Some(self.resolve(&first)), m)
                    } else {
                        (None, first)
                    };
                    self.cur.skip_insignificant();
                    if self.cur.eat_keyword("insteadof") {
                        let excluded = self.parse_name_list();
                        class.trait_insteadof.push(TraitInsteadof {
                            trait_fqcn: trait_fqcn.unwrap_or_default(),
                            method,
                            excluded,
                        });
                    } else if self.cur.eat_keyword("as") {
                        self.cur.skip_insignificant();
                        let mut visibility = None;
                        if self.cur.eat_keyword("public") {
                            visibility = Some(Visibility::Public);
                        } else if self.cur.eat_keyword("protected") {
                            visibility = Some(Visibility::Protected);
                        } else if self.cur.eat_keyword("private") {
                            visibility = Some(Visibility::Private);
                        }
                        self.cur.skip_insignificant();
                        let alias = self.cur.read_ident().map(str::to_owned);
                        class.trait_aliases.push(TraitAlias {
                            trait_fqcn,
                            method,
                            alias,
                            visibility,
                        });
                    }
                }
            }
        }
    }

    // ---- methods -------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn parse_method(
        &mut self,
        class: &mut ClassMeta,
        visibility: Visibility,
        is_static: bool,
        is_abstract: bool,
        is_final: bool,
        offset: usize,
    ) {
        self.cur.skip_insignificant();
        let returns_ref = if self.cur.peek() == Some(b'&') {
            self.cur.bump();
            self.cur.skip_insignificant();
            true
        } else {
            false
        };
        let Some(name) = self.cur.read_ident() else {
            self.issue(self.cur.pos, "expected method name");
            self.cur.skip_statement();
            return;
        };
        let name = name.to_owned();
        self.cur.skip_insignificant();
        let params = if self.cur.peek() == Some(b'(') {
            self.cur.bump();
            self.parse_params()
        } else {
            self.issue(self.cur.pos, format!("expected '(' after method {name}"));
            Vec::new()
        };
        self.cur.skip_insignificant();
        let return_type = if self.cur.peek() == Some(b':') {
            self.cur.bump();
            self.cur.skip_insignificant();
            self.parse_type_expr()
        } else {
            None
        };
        self.cur.skip_insignificant();
        match self.cur.peek() {
            Some(b'{') => {
                self.cur.bump();
                self.cur.skip_code_until_close_brace();
            }
            Some(b';') => {
                self.cur.bump();
            }
            _ => self.issue(self.cur.pos, format!("expected body or ';' after method {name}")),
        }
        class.methods.push(MethodMeta {
            name,
            visibility,
            is_static,
            is_abstract,
            is_final,
            returns_ref,
            params,
            return_type,
            offset,
        });
    }

    fn parse_params(&mut self) -> Vec<ParamMeta> {
        let mut params = Vec::new();
        loop {
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b')') => {
                    self.cur.bump();
                    return params;
                }
                Some(b',') => {
                    self.cur.bump();
                }
                None => return params,
                _ => {
                    while self.cur.peek() == Some(b'#') && self.cur.peek_at(1) == Some(b'[') {
                        self.cur.skip_attribute();
                        self.cur.skip_insignificant();
                    }
                    // Promotion modifiers.
                    let mut promoted = None;
                    let mut readonly = false;
                    loop {
                        self.cur.skip_insignificant();
                        if self.cur.eat_keyword("public") {
                            promoted.get_or_insert(Visibility::Public);
                            self.eat_set_suffix();
                        } else if self.cur.eat_keyword("protected") {
                            promoted.get_or_insert(Visibility::Protected);
                            self.eat_set_suffix();
                        } else if self.cur.eat_keyword("private") {
                            promoted.get_or_insert(Visibility::Private);
                            self.eat_set_suffix();
                        } else if self.cur.eat_keyword("readonly") {
                            readonly = true;
                        } else {
                            break;
                        }
                    }
                    // A bare `readonly` param modifier still promotes — to an
                    // implicitly public property (PHP 8.1).
                    if readonly && promoted.is_none() {
                        promoted = Some(Visibility::Public);
                    }
                    self.cur.skip_insignificant();
                    let ty = if matches!(self.cur.peek(), Some(b'$' | b'&' | b'.')) {
                        None
                    } else {
                        self.parse_type_expr()
                    };
                    self.cur.skip_insignificant();
                    let by_ref = if self.cur.peek() == Some(b'&') {
                        self.cur.bump();
                        self.cur.skip_insignificant();
                        true
                    } else {
                        false
                    };
                    let variadic = if self.cur.starts_with(b"...") {
                        self.cur.pos += 3;
                        self.cur.skip_insignificant();
                        true
                    } else {
                        false
                    };
                    if self.cur.peek() != Some(b'$') {
                        self.issue(self.cur.pos, "expected '$' in parameter");
                        // Recover to the next comma or the closing paren.
                        let _ = self.cur.capture_expr_until(&[b',', b')']);
                        continue;
                    }
                    self.cur.bump();
                    let name = self.cur.read_ident().unwrap_or("").to_owned();
                    self.cur.skip_insignificant();
                    let default = if self.cur.peek() == Some(b'=') {
                        self.cur.bump();
                        self.cur.skip_insignificant();
                        Some(self.cur.capture_expr_until(&[b',', b')']))
                    } else {
                        None
                    };
                    params.push(ParamMeta {
                        name,
                        ty,
                        default,
                        by_ref,
                        variadic,
                        promoted,
                        readonly,
                    });
                }
            }
        }
    }

    // ---- types -----------------------------------------------------------------

    /// `?A`, `A|B`, `A&B`, `(A&B)|C`, `namespace\X`, `\Fq\Cn` — resolved,
    /// structure preserved as text. Returns `None` when not at a type.
    fn parse_type_expr(&mut self) -> Option<String> {
        let first = self.parse_type_atom()?;
        let mut out = first;
        loop {
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b'|') => {
                    self.cur.bump();
                    self.cur.skip_insignificant();
                    match self.parse_type_atom() {
                        Some(a) => {
                            out.push('|');
                            out.push_str(&a);
                        }
                        None => break,
                    }
                }
                Some(b'&') => {
                    // Intersection type only when a type follows; `Foo &$x`
                    // is a by-ref parameter.
                    let save = self.cur.pos;
                    self.cur.bump();
                    self.cur.skip_insignificant();
                    if matches!(self.cur.peek(), Some(b'$' | b'.')) {
                        self.cur.pos = save;
                        break;
                    }
                    match self.parse_type_atom() {
                        Some(a) => {
                            out.push('&');
                            out.push_str(&a);
                        }
                        None => {
                            self.cur.pos = save;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        Some(out)
    }

    fn parse_type_atom(&mut self) -> Option<String> {
        self.cur.skip_insignificant();
        match self.cur.peek() {
            Some(b'?') => {
                self.cur.bump();
                let inner = self.parse_type_atom()?;
                Some(format!("?{inner}"))
            }
            Some(b'(') => {
                self.cur.bump();
                let inner = self.parse_type_expr()?;
                self.cur.skip_insignificant();
                if self.cur.peek() == Some(b')') {
                    self.cur.bump();
                }
                Some(format!("({inner})"))
            }
            _ => {
                let name = self.read_qualified_name()?;
                if !name.contains('\\') {
                    let lower = name.to_ascii_lowercase();
                    if BUILTIN_TYPES.contains(&lower.as_str()) {
                        return Some(lower);
                    }
                }
                Some(self.resolve(&name))
            }
        }
    }
}
