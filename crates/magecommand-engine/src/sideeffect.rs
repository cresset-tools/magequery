//! Mage-OS 4.0's `Compiler\ConstructorSideEffectScanner` and the eligibility
//! rules `Chain\LazyTypes` layers on it: whether a class may be built as a
//! PHP 8.4 lazy ghost, whose constructor may never run.
//!
//! A port, rule for rule, of the PHP at Mage-OS 4.0.0
//! (`setup/src/Magento/Setup/Module/Di/Compiler/ConstructorSideEffectScanner.php`
//! and `Config/Chain/LazyTypes.php`), with reflection answered from
//! [`Definitions`] and `token_get_all` from [`magecommand_php::token`]. It fails
//! closed like the original: anything not recognised makes a class eager.
//!
//! In short: a constructor is side-effect-free when its signature has no `new`
//! and no property hook, and its body holds only `$this->prop = <pure>;`,
//! `$this->prop ??= <pure>;` and `parent::__construct(<pure>, …);` (after which
//! the parent's constructor is checked the same way). Constructors declared in
//! `Magento\` or `MageOS\` are trusted unscanned, unless one calls a method the
//! class overrides outside those namespaces, or delegates to an untrusted
//! constructor. See the PHP for the full grammar of `<pure>`.
//!
//! Where this can differ from PHP: a hook on a promoted constructor property
//! is not modelled.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use magecommand_php::token::{self, Tok, Token};
use magecommand_php::{ClassMeta, MethodMeta, ParamMeta, Visibility};

use crate::definitions::Definitions;

const TRUSTED_NAMESPACES: [&str; 2] = ["Magento\\", "MageOS\\"];
const OBJECT_MANAGER_CLASS: &str = "Magento\\Framework\\App\\ObjectManager";
const NON_LAZY: &str = "Magento\\Framework\\ObjectManager\\Attribute\\NonLazy";
const SCALAR_TYPES: [&str; 7] = ["int", "float", "string", "bool", "false", "true", "null"];


/// A constructor as reflection reports it: `declaring` is the class
/// `getDeclaringClass()` names (the USING class for a trait's constructor),
/// `file` the file that holds its source.
#[derive(Clone)]
struct Ctor<'d> {
    declaring: &'d str,
    file: &'d Path,
    offset: usize,
    params: &'d [ParamMeta],
}

/// What an untrusted constructor touches.
#[derive(Clone, Default)]
struct Parsed {
    writes: BTreeSet<String>,
    reads: BTreeSet<String>,
    calls_parent: bool,
}

/// The 4.0 lazy-eligibility check, with its caches. One per area build.
pub struct Scanner<'d> {
    defs: &'d Definitions,
    trusted: &'d [&'d str],
    sources: HashMap<PathBuf, Option<Arc<[u8]>>>,
    imports: HashMap<PathBuf, Arc<HashMap<String, String>>>,
    constructor_cache: HashMap<String, Option<Parsed>>,
    trusted_call_cache: HashMap<String, Option<Vec<String>>>,
    trusted_parent_call_cache: HashMap<String, bool>,
    result_cache: HashMap<String, bool>,
}

impl<'d> Scanner<'d> {
    pub fn new(defs: &'d Definitions) -> Self {
        Scanner {
            defs,
            trusted: &TRUSTED_NAMESPACES,
            sources: HashMap::new(),
            imports: HashMap::new(),
            constructor_cache: HashMap::new(),
            trusted_call_cache: HashMap::new(),
            trusted_parent_call_cache: HashMap::new(),
            result_cache: HashMap::new(),
        }
    }

    /// Trust constructors in `namespaces` instead of `Magento\` / `MageOS\`.
    /// For differential testing: trusting none scans every constructor.
    pub fn trusting(mut self, namespaces: &'d [&'d str]) -> Self {
        self.trusted = namespaces;
        self
    }

    /// Whether a class name is in a trusted namespace.
    pub fn is_trusted(&self, class: &str) -> bool {
        let class = class.trim_start_matches('\\');
        self.trusted.iter().any(|ns| class.starts_with(ns))
    }

    // ---- LazyTypes::isLazyEligible, the 4.0 additions ----------------------

    /// 4.0's rules on top of the 3.3+ ones (`is_lazy_eligible`, which the
    /// caller has already applied): no `#[NonLazy]` through an interface,
    /// ancestor or trait, and a constructor that is trusted or proven free of
    /// side effects.
    pub fn is_lazy_eligible(&mut self, name: &str) -> bool {
        let Some(meta) = self.meta(name) else { return false };
        let class = meta.fqcn.as_str();
        if self.is_excluded(class) {
            return false;
        }
        let owner = match self.constructor(class) {
            Ok(Some(c)) => c.declaring,
            Ok(None) => class,
            Err(()) => return false,
        };
        if self.is_trusted(class) && self.is_trusted(owner) && !self.chains_to_untrusted_constructor(class) {
            return true;
        }
        self.is_side_effect_free(class)
    }

    /// An internal ancestor, or `#[NonLazy]` on the class, an ancestor, an
    /// implemented interface or a used trait.
    fn is_excluded(&self, class: &str) -> bool {
        let marked = |name: &str| {
            self.meta(name).is_some_and(|m| m.attributes.iter().any(|a| a == NON_LAZY))
        };
        if self.defs.all_interfaces(class).iter().any(|i| marked(i)) {
            return true;
        }
        let mut current = Some(class.to_owned());
        let mut hops = 0;
        while let Some(name) = current {
            hops += 1;
            let Some(meta) = self.meta(&name) else { return true }; // internal (or missing)
            if hops > 64 || marked(&name) || self.uses_non_lazy_trait(meta, 0) {
                return true;
            }
            current = meta.extends.first().cloned();
        }
        false
    }

    fn uses_non_lazy_trait(&self, meta: &ClassMeta, depth: usize) -> bool {
        depth <= 64
            && meta.traits.iter().any(|t| match self.meta(t) {
                Some(tm) => {
                    tm.attributes.iter().any(|a| a == NON_LAZY) || self.uses_non_lazy_trait(tm, depth + 1)
                }
                None => false,
            })
    }

    // ---- ConstructorSideEffectScanner -------------------------------------

    /// Whether constructing the class has no side effects, so skipping its
    /// constructor is safe.
    pub fn is_side_effect_free(&mut self, class: &str) -> bool {
        if let Some(&r) = self.result_cache.get(class) {
            return r;
        }
        let r = self.evaluate(class).unwrap_or(false);
        self.result_cache.insert(class.to_owned(), r);
        r
    }

    /// Walk the constructor chain that runs when `class` is instantiated.
    /// `Err` is the PHP's caught `Throwable`.
    fn evaluate(&mut self, class: &str) -> Result<bool, ()> {
        if let Some(d) = self.method_declarer(class, "__destruct")
            && !self.is_trusted(d)
        {
            return Ok(false);
        }
        let mut writes = BTreeSet::new();
        let mut reads = BTreeSet::new();
        let mut current = class.to_owned();
        while let Some(ctor) = self.constructor(&current)? {
            let declaring = ctor.declaring;
            if self.is_trusted(declaring) {
                let calls = self.trusted_calls(&ctor)?;
                if !self.calls_only_trusted_methods(class, calls.as_deref()) {
                    return Ok(false);
                }
                match self.untrusted_ancestor_constructor(declaring)? {
                    // A trusted constructor may still delegate upward into one
                    // that is not; scan that one rather than assume.
                    Some(untrusted) => {
                        current = untrusted.declaring.to_owned();
                        continue;
                    }
                    None => break,
                }
            }
            let Some(parsed) = self.parsed_constructor(&ctor)? else { return Ok(false) };
            writes.extend(parsed.writes);
            reads.extend(parsed.reads);
            if !parsed.calls_parent {
                break;
            }
            let Some(parent) = self.parent(declaring) else { return Ok(false) };
            if self.constructor(parent)?.is_none() {
                return Ok(false);
            }
            current = parent.to_owned();
        }
        Ok(self.is_plain_property_access(class, &writes, &reads))
    }

    /// `$this` property access in the constructors cannot run user code.
    fn is_plain_property_access(&self, class: &str, writes: &BTreeSet<String>, reads: &BTreeSet<String>) -> bool {
        if (!writes.is_empty() && self.method_declarer(class, "__set").is_some())
            || (!reads.is_empty() && self.method_declarer(class, "__get").is_some())
        {
            return false;
        }
        writes.iter().chain(reads).all(|name| !self.property(class, name).is_some_and(|p| p.hooked))
    }

    /// Every method a trusted constructor calls on `$this` is declared in a
    /// trusted namespace (`None`: the calls could not be read).
    fn calls_only_trusted_methods(&self, class: &str, methods: Option<&[String]>) -> bool {
        let Some(methods) = methods else { return false };
        methods.iter().all(|method| {
            let candidates: &[&str] = if self.method_declarer(class, method).is_some() {
                &[method.as_str()]
            } else {
                &["__call", "__callStatic"]
            };
            candidates
                .iter()
                .all(|c| self.method_declarer(class, c).is_none_or(|d| self.is_trusted(d)))
        })
    }

    /// The `$this->method()` and `static::method()` calls of a trusted
    /// constructor and its trusted parents; `None` when unknown.
    fn trusted_calls(&mut self, ctor: &Ctor<'d>) -> Result<Option<Vec<String>>, ()> {
        let key = ctor.declaring;
        if let Some(cached) = self.trusted_call_cache.get(key) {
            return Ok(cached.clone());
        }
        self.trusted_call_cache.insert(key.to_owned(), None);

        let src = self.source(ctor.file)?;
        let tokens = constructor_tokens(&src, ctor.offset)?;
        let mut methods = BTreeSet::new();
        let mut calls_parent = false;
        let n = tokens.len();
        let mut i = 0;
        while i + 3 < n {
            let (id, text) = (tokens[i].kind, tokens[i].text);
            let next = tokens[i + 1].kind;
            if id == Tok::Variable
                && text == "$this"
                && matches!(next, Tok::ObjectOperator | Tok::NullsafeObjectOperator)
            {
                if tokens[i + 2].kind != Tok::Str {
                    return Ok(None);
                }
                if tokens[i + 3].kind == Tok::Char(b'(') {
                    methods.insert(tokens[i + 2].text.to_ascii_lowercase());
                }
            } else if next == Tok::DoubleColon && tokens[i + 3].kind == Tok::Char(b'(') {
                let name = tokens[i + 2].text.to_ascii_lowercase();
                let word = !name.is_empty() && name.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric());
                if id == Tok::Static && !word {
                    return Ok(None);
                }
                if id == Tok::Static {
                    methods.insert(name);
                } else if id == Tok::Str && text.eq_ignore_ascii_case("parent") && name == "__construct" {
                    calls_parent = true;
                }
            }
            i += 1;
        }
        self.trusted_parent_call_cache.insert(key.to_owned(), calls_parent);

        let parent = if calls_parent { self.parent(key) } else { None };
        if let Some(parent) = parent
            && let Some(parent_ctor) = self.constructor(parent)?
            && self.is_trusted(parent_ctor.declaring)
        {
            match self.trusted_calls(&parent_ctor)? {
                None => return Ok(None),
                Some(m) => methods.extend(m),
            }
        }
        let methods: Vec<String> = methods.into_iter().collect();
        self.trusted_call_cache.insert(key.to_owned(), Some(methods.clone()));
        Ok(Some(methods))
    }

    /// Whether the constructor chain of `class` reaches a constructor outside
    /// the trusted namespaces.
    fn chains_to_untrusted_constructor(&mut self, class: &str) -> bool {
        // In the PHP this runs outside the scan's try/catch, so a throw would
        // abort the compile; here it counts as untrusted, which then scans.
        let Ok(ctor) = self.constructor(class) else { return true };
        let Some(ctor) = ctor else { return false };
        let declaring = ctor.declaring;
        if !self.is_trusted(declaring) {
            return true;
        }
        // Without an untrusted ancestor that declares a constructor there is
        // nothing for parent::__construct() to reach.
        if !self.has_untrusted_ancestor_constructor(declaring) {
            return false;
        }
        match self.trusted_calls(&ctor) {
            Ok(Some(_)) => {}
            _ => return true,
        }
        !matches!(self.untrusted_ancestor_constructor(declaring), Ok(None))
    }

    /// The first constructor above a trusted one that parent::__construct()
    /// reaches and is not trusted.
    fn untrusted_ancestor_constructor(&self, declaring: &'d str) -> Result<Option<Ctor<'d>>, ()> {
        let mut owner = declaring;
        while self.trusted_parent_call_cache.get(owner).copied().unwrap_or(false) {
            let Some(parent) = self.parent(owner) else { return Ok(None) };
            let Some(parent_ctor) = self.constructor(parent)? else { return Ok(None) };
            owner = parent_ctor.declaring;
            if !self.is_trusted(owner) {
                return Ok(Some(parent_ctor));
            }
        }
        Ok(None)
    }

    /// Whether any ancestor outside the trusted namespaces declares a
    /// constructor of its own.
    fn has_untrusted_ancestor_constructor(&self, declaring: &str) -> bool {
        let mut parent = self.parent(declaring);
        let mut hops = 0;
        while let Some(p) = parent {
            hops += 1;
            if hops > 64 {
                return true;
            }
            if !self.is_trusted(p) && matches!(self.constructor(p), Ok(Some(c)) if c.declaring == p) {
                return true;
            }
            parent = self.parent(p);
        }
        false
    }

    /// Parse an untrusted constructor once.
    fn parsed_constructor(&mut self, ctor: &Ctor<'d>) -> Result<Option<Parsed>, ()> {
        let key = ctor.declaring;
        if let Some(cached) = self.constructor_cache.get(key) {
            return Ok(cached.clone());
        }
        self.constructor_cache.insert(key.to_owned(), None);
        let src = self.source(ctor.file)?;
        let tokens = constructor_tokens(&src, ctor.offset)?;
        let imports = self.imports(ctor.file, &src);
        let mut access = Parsed::default();
        for p in ctor.params {
            if p.promoted.is_some() {
                access.writes.insert(p.name.clone());
            }
        }
        let mut parser = CtorParser {
            scanner: self,
            tokens: &tokens,
            pos: 0,
            imports: &imports,
            scope: ctor.declaring,
            params: ctor.params,
            access,
        };
        let mut calls_parent = false;
        let parsed = parser.parse_constructor(&mut calls_parent).then(|| {
            let mut access = std::mem::take(&mut parser.access);
            access.calls_parent = calls_parent;
            access
        });
        self.constructor_cache.insert(key.to_owned(), parsed.clone());
        Ok(parsed)
    }

    // ---- reflection over Definitions ---------------------------------------

    fn meta(&self, name: &str) -> Option<&'d ClassMeta> {
        self.defs.get(name).map(|r| &r.meta)
    }

    /// `getParentClass()`, by declared name.
    fn parent(&self, class: &str) -> Option<&'d str> {
        let parent = self.meta(class)?.extends.first()?;
        self.meta(parent).map(|m| m.fqcn.as_str())
    }

    /// `getConstructor()`: the class's own (its body, else a used trait's),
    /// else the parent's. `Err` when the chain leaves the parsed set.
    fn constructor(&self, class: &str) -> Result<Option<Ctor<'d>>, ()> {
        let mut current = self.defs.get(class).ok_or(())?;
        for _ in 0..64 {
            let meta = &current.meta;
            if let Some(m) = meta.methods.iter().find(|m| m.name.eq_ignore_ascii_case("__construct")) {
                return Ok(Some(Ctor {
                    declaring: &meta.fqcn,
                    file: &current.file,
                    offset: m.offset,
                    params: &m.params,
                }));
            }
            if let Some((file, m)) = self.trait_method(&meta.traits, "__construct") {
                return Ok(Some(Ctor { declaring: &meta.fqcn, file, offset: m.offset, params: &m.params }));
            }
            match meta.extends.first() {
                Some(parent) => current = self.defs.get(parent).ok_or(())?,
                None => return Ok(None),
            }
        }
        Err(())
    }

    /// A method declared by one of `traits` (or the traits they use).
    fn trait_method(&self, traits: &[String], name: &str) -> Option<(&'d Path, &'d MethodMeta)> {
        let mut stack: Vec<&str> = traits.iter().map(String::as_str).collect();
        let mut seen = 0;
        while let Some(t) = stack.pop() {
            seen += 1;
            if seen > 256 {
                return None;
            }
            let Some(rec) = self.defs.get(t) else { continue };
            if let Some(m) = rec.meta.methods.iter().find(|m| m.name.eq_ignore_ascii_case(name)) {
                return Some((&rec.file, m));
            }
            stack.extend(rec.meta.traits.iter().map(String::as_str));
        }
        None
    }

    /// `hasMethod($name) ? getMethod($name)->getDeclaringClass()->getName()`:
    /// the class whose body or used trait declares it, nearest first.
    fn method_declarer(&self, class: &str, name: &str) -> Option<&'d str> {
        let mut current = self.meta(class);
        let mut hops = 0;
        while let Some(meta) = current {
            hops += 1;
            if hops > 64 {
                return None;
            }
            let own = meta.methods.iter().any(|m| m.name.eq_ignore_ascii_case(name))
                || meta.trait_aliases.iter().any(|a| a.alias.as_deref().is_some_and(|al| al.eq_ignore_ascii_case(name)))
                || self.trait_method(&meta.traits, name).is_some();
            if own {
                return Some(&meta.fqcn);
            }
            current = meta.extends.first().and_then(|p| self.meta(p));
        }
        None
    }

    /// `hasProperty($name) ? getProperty($name)`: declared (or promoted)
    /// here, in a used trait, or in an ancestor — where an ancestor's private
    /// property is not the class's.
    fn property(&self, class: &str, name: &str) -> Option<Prop> {
        let mut current = self.meta(class);
        let mut first = true;
        let mut hops = 0;
        while let Some(meta) = current {
            hops += 1;
            if hops > 64 {
                return None;
            }
            if let Some((prop, visibility)) = self.own_property(meta, name) {
                return (first || visibility != Visibility::Private).then_some(prop);
            }
            first = false;
            current = meta.extends.first().and_then(|p| self.meta(p));
        }
        None
    }

    fn own_property(&self, meta: &ClassMeta, name: &str) -> Option<(Prop, Visibility)> {
        let mut stack: Vec<&ClassMeta> = vec![meta];
        let mut seen = 0;
        while let Some(m) = stack.pop() {
            seen += 1;
            if seen > 256 {
                return None;
            }
            if let Some(p) = m.properties.iter().find(|p| p.name == name) {
                return Some((Prop { is_static: p.is_static, hooked: p.hooked }, p.visibility));
            }
            let promoted = m
                .methods
                .iter()
                .filter(|f| f.name.eq_ignore_ascii_case("__construct"))
                .flat_map(|f| f.params.iter())
                .find(|p| p.name == name && p.promoted.is_some());
            if let Some(p) = promoted {
                return Some((Prop { is_static: false, hooked: false }, p.promoted.unwrap_or(Visibility::Public)));
            }
            stack.extend(m.traits.iter().filter_map(|t| self.meta(t)));
        }
        None
    }

    fn source(&mut self, file: &Path) -> Result<Arc<[u8]>, ()> {
        self.sources
            .entry(file.to_owned())
            .or_insert_with(|| std::fs::read(file).ok().map(Arc::from))
            .clone()
            .ok_or(())
    }

    /// Simple `use A\B;` and `use A\B as C;` imports before the file's first
    /// class-like, lowercase alias => FQCN — the PHP's `collectImports`.
    fn imports(&mut self, file: &Path, src: &[u8]) -> Arc<HashMap<String, String>> {
        if let Some(i) = self.imports.get(file) {
            return i.clone();
        }
        let mut imports = HashMap::new();
        let start = memfind(src, b"<?php").map_or(0, |i| i + 5);
        let tokens: Vec<Token> = token::tokens(src, start)
            .take_while(|t| !matches!(t.kind, Tok::Class | Tok::Trait | Tok::Interface | Tok::Enum))
            .collect();
        for (i, t) in tokens.iter().enumerate() {
            if t.kind != Tok::Use {
                continue;
            }
            let Some(name) = tokens.get(i + 1) else { continue };
            if !matches!(name.kind, Tok::Str | Tok::NameQualified | Tok::NameFullyQualified) {
                continue;
            }
            let fqcn = name.text.trim_start_matches('\\').to_owned();
            match (tokens.get(i + 2), tokens.get(i + 3)) {
                (Some(n), Some(alias)) if n.kind == Tok::As && alias.kind == Tok::Str => {
                    imports.insert(alias.text.to_ascii_lowercase(), fqcn);
                }
                (Some(n), _) if n.kind == Tok::Char(b';') => {
                    let short = fqcn.rsplit('\\').next().unwrap_or(&fqcn).to_ascii_lowercase();
                    imports.insert(short, fqcn);
                }
                _ => {}
            }
        }
        let imports = Arc::new(imports);
        self.imports.insert(file.to_owned(), imports.clone());
        imports
    }
}

#[derive(Clone, Copy)]
struct Prop {
    is_static: bool,
    hooked: bool,
}

fn memfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The tokens of `function __construct(…) {…}`, from the method's offset
/// (its modifiers, `public` / `final` / `private(set)`, are skipped). Braces
/// inside the signature (hooks on promoted parameters) do not open the body.
fn constructor_tokens(src: &[u8], offset: usize) -> Result<Vec<Token<'_>>, ()> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut parens = 0isize;
    let mut modifiers = 0;
    for t in token::tokens(src, offset) {
        if out.is_empty() && t.kind != Tok::Function {
            modifiers += 1;
            if modifiers > 8 {
                return Err(());
            }
            continue;
        }
        if out.len() == 1 && !(t.kind == Tok::Str && t.text.eq_ignore_ascii_case("__construct")) {
            return Err(());
        }
        out.push(t);
        match t.kind {
            Tok::Char(b'(') => parens += 1,
            Tok::Char(b')') => parens -= 1,
            _ if parens > 0 => {}
            Tok::Char(b'{') | Tok::CurlyOpen | Tok::DollarOpenCurlyBraces => depth += 1,
            Tok::Char(b'}') => {
                depth = depth.checked_sub(1).ok_or(())?;
                if depth == 0 {
                    return Ok(out);
                }
            }
            _ => {}
        }
    }
    Err(())
}

/// The recursive descent over one untrusted constructor.
struct CtorParser<'s, 'd, 't> {
    scanner: &'s Scanner<'d>,
    tokens: &'t [Token<'t>],
    pos: usize,
    imports: &'t HashMap<String, String>,
    /// The class declaring the constructor.
    scope: &'d str,
    params: &'d [ParamMeta],
    access: Parsed,
}

impl<'d> CtorParser<'_, 'd, '_> {
    fn peek_at(&self, offset: usize) -> Option<Tok> {
        self.tokens.get(self.pos + offset).map(|t| t.kind)
    }

    fn peek(&self) -> Option<Tok> {
        self.peek_at(0)
    }

    fn is_text(&self, offset: usize, text: &str) -> bool {
        self.tokens
            .get(self.pos + offset)
            .is_some_and(|t| t.kind == Tok::Str && t.text.eq_ignore_ascii_case(text))
    }

    fn accept(&mut self, kind: Tok) -> bool {
        if self.peek() == Some(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn accept_char(&mut self, c: u8) -> bool {
        self.accept(Tok::Char(c))
    }

    /// `function __construct(<params>) { <statements> }`
    fn parse_constructor(&mut self, calls_parent: &mut bool) -> bool {
        self.pos = 2;
        if !self.accept_char(b'(') {
            return false;
        }
        // Signature: anything but `new` (a default that runs on instantiation)
        // and `{` (a hook on a promoted parameter).
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                None | Some(Tok::New | Tok::Char(b'{')) => return false,
                Some(Tok::Char(b'(')) => depth += 1,
                Some(Tok::Char(b')')) => depth -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        if !self.accept_char(b'{') {
            return false;
        }
        while !self.accept_char(b'}') {
            if self.accept_char(b';') {
                continue;
            }
            if self.parse_parent_constructor_call() {
                *calls_parent = true;
                continue;
            }
            if !self.parse_property_assignment() {
                return false;
            }
        }
        self.pos == self.tokens.len()
    }

    /// `parent::__construct(<pure>, …);`
    fn parse_parent_constructor_call(&mut self) -> bool {
        if !self.is_text(0, "parent") || self.peek_at(1) != Some(Tok::DoubleColon) || !self.is_text(2, "__construct") {
            return false;
        }
        self.pos += 3;
        self.accept_char(b'(') && self.parse_arguments(b')') && self.accept_char(b';')
    }

    /// `$this->prop = <pure>;` or `$this->prop ??= <pure>;`
    fn parse_property_assignment(&mut self) -> bool {
        if !self.is_this_property_fetch() {
            return false;
        }
        let property = self.tokens[self.pos + 2].text.to_owned();
        if !self.is_plain_property(&property) {
            return false;
        }
        self.pos += 3;
        self.access.writes.insert(property.clone());
        if self.accept(Tok::CoalesceEqual) {
            self.access.reads.insert(property);
        } else if !self.accept_char(b'=') {
            return false;
        }
        self.parse_expression() && self.accept_char(b';')
    }

    fn parse_expression(&mut self) -> bool {
        if !self.parse_operand() {
            return false;
        }
        loop {
            if self.accept(Tok::Coalesce) {
                if !self.parse_object_manager_fallback() && !self.parse_operand() {
                    return false;
                }
            } else if self.accept_char(b'?') {
                // `?:` or `? <expr> :`
                if !self.accept_char(b':') && !(self.parse_expression() && self.accept_char(b':')) {
                    return false;
                }
                if !self.parse_operand() {
                    return false;
                }
            } else {
                return true;
            }
        }
    }

    fn parse_operand(&mut self) -> bool {
        match self.peek() {
            Some(Tok::Cast) => {
                self.pos += 1;
                self.parse_cast_operand()
            }
            Some(Tok::Char(b'-' | b'+')) => {
                self.pos += 1;
                self.accept(Tok::Number)
            }
            Some(Tok::Number | Tok::Literal) => {
                self.pos += 1;
                true
            }
            Some(Tok::Char(b'(')) => {
                self.pos += 1;
                self.parse_expression() && self.accept_char(b')')
            }
            Some(Tok::Char(b'[')) => {
                self.pos += 1;
                self.parse_array_elements(b']')
            }
            Some(Tok::Array) => {
                self.pos += 1;
                self.accept_char(b'(') && self.parse_array_elements(b')')
            }
            Some(Tok::Variable) => self.parse_variable(),
            Some(k) if is_name(k) => self.parse_constant(),
            _ => false,
        }
    }

    /// A literal, a constant, or a parameter declared with scalar types only.
    fn parse_cast_operand(&mut self) -> bool {
        match self.peek() {
            Some(Tok::Char(b'-' | b'+') | Tok::Number | Tok::Literal) => self.parse_operand(),
            Some(k) if is_name(k) => self.parse_constant(),
            Some(Tok::Variable) => {
                let param = self.parameter();
                self.pos += 1;
                param.is_some_and(is_scalar_parameter) && !self.is_fetch()
            }
            _ => false,
        }
    }

    /// A parameter, `$this`, `$this->prop`, or `$param[<pure>]` on an array.
    fn parse_variable(&mut self) -> bool {
        if self.tokens[self.pos].text == "$this" {
            if self.is_this_property_fetch() {
                let property = self.tokens[self.pos + 2].text.to_owned();
                if !self.is_plain_property(&property) {
                    return false;
                }
                self.access.reads.insert(property);
                self.pos += 2;
            }
            self.pos += 1;
            return !self.is_fetch();
        }
        let Some(param) = self.parameter() else { return false };
        self.pos += 1;
        if self.peek() == Some(Tok::Char(b'[')) {
            if !is_array_parameter(param) {
                return false;
            }
            self.pos += 1;
            if !self.parse_expression() || !self.accept_char(b']') {
                return false;
            }
        }
        !self.is_fetch()
    }

    /// `...$param` where `$param` is an array.
    fn parse_spread(&mut self) -> bool {
        let param = if self.peek() == Some(Tok::Variable) { self.parameter() } else { None };
        if !param.is_some_and(is_array_parameter) {
            return false;
        }
        self.pos += 1;
        !self.is_fetch()
    }

    /// `FOO`, `true`, `X::FOO`, `X::class`; no calls.
    fn parse_constant(&mut self) -> bool {
        self.pos += 1;
        if self.peek() == Some(Tok::Char(b'(')) {
            return false;
        }
        if !self.accept(Tok::DoubleColon) {
            return true;
        }
        match self.peek() {
            Some(Tok::Class) => {
                self.pos += 1;
                true
            }
            Some(Tok::Str) if self.peek_at(1) != Some(Tok::Char(b'(')) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    /// `[key =>] [...]<pure>` elements up to `close`.
    fn parse_array_elements(&mut self, close: u8) -> bool {
        while !self.accept_char(close) {
            if self.accept(Tok::Ellipsis) {
                if !self.parse_spread() {
                    return false;
                }
            } else if !self.parse_expression() || (self.accept(Tok::DoubleArrow) && !self.parse_expression()) {
                return false;
            }
            if !self.accept_char(b',') && self.peek() != Some(Tok::Char(close)) {
                return false;
            }
        }
        true
    }

    /// `[name:] [...]<pure>` call arguments up to `close`.
    fn parse_arguments(&mut self, close: u8) -> bool {
        while !self.accept_char(close) {
            if self.peek() == Some(Tok::Str) && self.peek_at(1) == Some(Tok::Char(b':')) {
                self.pos += 2;
            }
            let ok = if self.accept(Tok::Ellipsis) { self.parse_spread() } else { self.parse_expression() };
            if !ok {
                return false;
            }
            if !self.accept_char(b',') && self.peek() != Some(Tok::Char(close)) {
                return false;
            }
        }
        true
    }

    /// `\Magento\Framework\App\ObjectManager::getInstance()->get|create(<pure>, …)`,
    /// only as the right-hand side of `??`.
    fn parse_object_manager_fallback(&mut self) -> bool {
        let Some(name) = self.tokens.get(self.pos) else { return false };
        if !matches!(name.kind, Tok::Str | Tok::NameQualified | Tok::NameFullyQualified)
            || self.resolve_class_name(name) != OBJECT_MANAGER_CLASS
            || self.peek_at(1) != Some(Tok::DoubleColon)
            || !self.is_text(2, "getInstance")
            || self.peek_at(3) != Some(Tok::Char(b'('))
            || self.peek_at(4) != Some(Tok::Char(b')'))
            || self.peek_at(5) != Some(Tok::ObjectOperator)
            || !(self.is_text(6, "get") || self.is_text(6, "create"))
            || self.peek_at(7) != Some(Tok::Char(b'('))
        {
            return false;
        }
        self.pos += 8;
        self.parse_arguments(b')')
    }

    /// A class name against the file's use imports only (not its namespace).
    fn resolve_class_name(&self, token: &Token) -> String {
        if token.kind == Tok::NameFullyQualified {
            return token.text.trim_start_matches('\\').to_owned();
        }
        let (first, rest) = match token.text.split_once('\\') {
            Some((f, r)) => (f, Some(r)),
            None => (token.text, None),
        };
        match self.imports.get(&first.to_ascii_lowercase()) {
            Some(fqcn) => match rest {
                Some(r) => format!("{fqcn}\\{r}"),
                None => fqcn.clone(),
            },
            None => String::new(),
        }
    }

    /// Declared on the constructor's class, non-static, without hooks.
    fn is_plain_property(&self, name: &str) -> bool {
        self.scanner.property(self.scope, name).is_some_and(|p| !p.is_static && !p.hooked)
    }

    fn parameter(&self) -> Option<&'d ParamMeta> {
        let name = self.tokens[self.pos].text.strip_prefix('$')?;
        self.params.iter().find(|p| p.name == name)
    }

    /// The token at the cursor would continue the preceding variable into a
    /// call or fetch.
    fn is_fetch(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Tok::Char(b'(' | b'[' | b'{')
                    | Tok::ObjectOperator
                    | Tok::NullsafeObjectOperator
                    | Tok::DoubleColon
            )
        )
    }

    /// `$this->prop` (not `?->`).
    fn is_this_property_fetch(&self) -> bool {
        self.peek() == Some(Tok::Variable)
            && self.tokens[self.pos].text == "$this"
            && self.peek_at(1) == Some(Tok::ObjectOperator)
            && self.peek_at(2) == Some(Tok::Str)
    }
}

fn is_name(kind: Tok) -> bool {
    matches!(kind, Tok::Str | Tok::NameQualified | Tok::NameFullyQualified | Tok::NameRelative | Tok::Static)
}

/// Reflection's view of a declared type: `T|null` is the named type `T`.
fn reflected_members(param: &ParamMeta) -> Option<Vec<String>> {
    let ty = crate::reflect::normalize_nullable(param.ty.as_deref()?);
    if let Some(single) = ty.strip_prefix('?') {
        return Some(vec![single.to_ascii_lowercase()]);
    }
    if ty.contains('&') || ty.contains('(') {
        return None;
    }
    Some(ty.split('|').map(|m| m.trim().to_ascii_lowercase()).collect())
}

/// `array`, `?array`, or variadic.
fn is_array_parameter(param: &ParamMeta) -> bool {
    param.variadic || reflected_members(param).is_some_and(|m| m == ["array"])
}

/// Declared with scalar types only (`int`, `?string`, `int|string`, …).
fn is_scalar_parameter(param: &ParamMeta) -> bool {
    !param.variadic
        && reflected_members(param)
            .is_some_and(|m| m.iter().all(|t| SCALAR_TYPES.contains(&t.as_str())))
}
