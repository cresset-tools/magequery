//! A focused PHP parser that extracts just a class file's header: its namespace, `use`
//! imports, and the `extends`/`implements` names — resolved to fully-qualified names. This
//! is enough to walk the type hierarchy (for plugin-on-ancestor resolution). We never
//! execute PHP; we tokenize far enough to read the first type declaration, then stop.

use std::collections::HashMap;

use crate::ids::ClassName;

pub(crate) struct PhpClass {
    /// The file's own FQCN (kept for sanity/future use; resolution keys on the request).
    #[allow(dead_code)]
    pub fqcn: ClassName,
    /// Parent class(es) — one for a class, possibly several for an interface.
    pub extends: Vec<ClassName>,
    pub implements: Vec<ClassName>,
    pub is_interface: bool,
    pub is_abstract: bool,
}

enum Token {
    Ident(String),
    Punct(char),
    /// A single- or double-quoted string literal (content only, escapes collapsed).
    Str(String),
}

/// Parse the header of a PHP class/interface/trait/enum file. Returns `None` if no type
/// declaration is found.
pub(crate) fn parse_header(src: &str) -> Option<PhpClass> {
    let tokens = tokenize(src);
    let mut namespace = String::new();
    let mut uses: HashMap<String, String> = HashMap::new();
    let mut depth: i32 = 0;
    let mut is_abstract = false;
    let mut i = 0;

    while i < tokens.len() {
        match &tokens[i] {
            Token::Ident(kw) if depth == 0 && kw == "namespace" => {
                i += 1;
                if let Some(Token::Ident(ns)) = tokens.get(i) {
                    namespace = ns.trim_matches('\\').to_string();
                }
                i = skip_to_semi_or_brace(&tokens, i);
            }
            Token::Ident(kw) if depth == 0 && kw == "use" => {
                i = parse_use(&tokens, i + 1, &mut uses);
            }
            Token::Ident(kw) if depth == 0 && kw == "abstract" => {
                is_abstract = true;
                i += 1;
            }
            Token::Ident(kw)
                if depth == 0
                    && matches!(kw.as_str(), "class" | "interface" | "trait" | "enum") =>
            {
                let is_interface = kw == "interface";
                return parse_type_header(&tokens, i + 1, &namespace, &uses, is_interface, is_abstract);
            }
            Token::Punct('{') => {
                depth += 1;
                i += 1;
            }
            Token::Punct('}') => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

fn parse_type_header(
    tokens: &[Token],
    mut i: usize,
    namespace: &str,
    uses: &HashMap<String, String>,
    is_interface: bool,
    is_abstract: bool,
) -> Option<PhpClass> {
    let name = match tokens.get(i) {
        Some(Token::Ident(n)) => n.clone(),
        _ => return None,
    };
    i += 1;

    let mut extends = Vec::new();
    let mut implements = Vec::new();
    let mut mode = 0u8; // 1 = extends, 2 = implements

    while i < tokens.len() {
        match &tokens[i] {
            Token::Punct('{') => break,
            Token::Ident(k) if k == "extends" => mode = 1,
            Token::Ident(k) if k == "implements" => mode = 2,
            Token::Ident(n) => {
                let resolved = resolve_name(n, namespace, uses);
                match mode {
                    1 => extends.push(resolved),
                    2 => implements.push(resolved),
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }

    let fqcn = if namespace.is_empty() {
        ClassName::new(name)
    } else {
        ClassName::new(format!("{namespace}\\{name}"))
    };
    Some(PhpClass { fqcn, extends, implements, is_interface, is_abstract })
}

/// Parse a `use` statement starting at `i` (after the `use` keyword). Records imports into
/// `uses` (alias -> FQCN) and returns the index after the terminating `;`.
fn parse_use(tokens: &[Token], mut i: usize, uses: &mut HashMap<String, String>) -> usize {
    // `use function ...` / `use const ...` import symbols, not classes — skip them.
    if let Some(Token::Ident(k)) = tokens.get(i) {
        if k == "function" || k == "const" {
            return skip_to_semi(tokens, i);
        }
    }

    let prefix = match tokens.get(i) {
        Some(Token::Ident(n)) => n.clone(),
        _ => return skip_to_semi(tokens, i),
    };
    i += 1;

    if matches!(tokens.get(i), Some(Token::Punct('{'))) {
        // Group use: `use Prefix\{ A, B as C };`
        i += 1;
        loop {
            while matches!(tokens.get(i), Some(Token::Punct(','))) {
                i += 1;
            }
            if matches!(tokens.get(i), Some(Token::Punct('}'))) {
                i += 1;
                break;
            }
            match tokens.get(i) {
                Some(Token::Ident(sub)) => {
                    i += 1;
                    let full = format!("{prefix}{sub}");
                    let mut alias = last_segment(&full);
                    if matches!(tokens.get(i), Some(Token::Ident(k)) if k == "as") {
                        i += 1;
                        if let Some(Token::Ident(a)) = tokens.get(i) {
                            alias = a.clone();
                            i += 1;
                        }
                    }
                    uses.insert(alias, full.trim_matches('\\').to_string());
                }
                None => break,
                _ => i += 1,
            }
        }
        skip_to_semi(tokens, i)
    } else {
        // Simple use: `use A\B\C [as Alias];`
        let mut alias = last_segment(&prefix);
        if matches!(tokens.get(i), Some(Token::Ident(k)) if k == "as") {
            i += 1;
            if let Some(Token::Ident(a)) = tokens.get(i) {
                alias = a.clone();
                i += 1;
            }
        }
        uses.insert(alias, prefix.trim_matches('\\').to_string());
        skip_to_semi(tokens, i)
    }
}

/// Resolve a name as written in a class header to a fully-qualified class name, following
/// PHP rules: leading `\` is absolute; a matching `use` alias on the first segment expands;
/// otherwise it's relative to the current namespace.
fn resolve_name(raw: &str, namespace: &str, uses: &HashMap<String, String>) -> ClassName {
    if let Some(abs) = raw.strip_prefix('\\') {
        return ClassName::new(abs);
    }
    let first = raw.split('\\').next().unwrap_or(raw);
    if let Some(fqcn) = uses.get(first) {
        let rest = &raw[first.len()..];
        return ClassName::new(format!("{fqcn}{rest}"));
    }
    if namespace.is_empty() {
        ClassName::new(raw)
    } else {
        ClassName::new(format!("{namespace}\\{raw}"))
    }
}

/// A `before*`/`around*`/`after*` plugin method found in a class file.
pub(crate) struct PluginMethodRaw {
    /// `"before"`, `"around"`, or `"after"`.
    pub prefix: &'static str,
    /// Full method name, e.g. `beforeSave`.
    pub name: String,
}

/// Scan a plugin class file for its interception methods: public methods named
/// `before*`/`around*`/`after*` (with an uppercase letter after the prefix). Private and
/// protected methods are skipped — Magento only intercepts public ones.
pub(crate) fn plugin_methods(src: &str) -> Vec<PluginMethodRaw> {
    let tokens = tokenize(src);
    let mut out = Vec::new();
    for i in 0..tokens.len() {
        let Token::Ident(kw) = &tokens[i] else { continue };
        if kw != "function" {
            continue;
        }
        let Some(Token::Ident(name)) = tokens.get(i + 1) else { continue };
        if !is_public_method(&tokens, i) {
            continue;
        }
        for prefix in ["before", "around", "after"] {
            if let Some(rest) = name.strip_prefix(prefix) {
                if rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    out.push(PluginMethodRaw { prefix, name: name.clone() });
                    break;
                }
            }
        }
    }
    out
}

/// A value written as a string literal, a class-constant reference (`self::COMMAND_NAME`),
/// or a property reference (`$this->commandName`) — the ways commands state their
/// name/description.
pub(crate) enum StrOrConst {
    Str(String),
    /// The constant's name; resolved against this file's `consts` or an ancestor's.
    Const(String),
    /// The property's name; resolved against this file's `props` or an ancestor's.
    Prop(String),
}

/// What a console-command class declares about itself, found by a flat token scan:
/// `setName(…)`/`setDescription(…)` calls, the Symfony `$defaultName`/`$defaultDescription`
/// static properties, a name passed to `parent::__construct(…)`, plus every string class
/// constant and `$var = '…'` assignment (for resolving `self::X`/`$this->x` references —
/// the caller may also consult ancestors).
#[derive(Default)]
pub(crate) struct CommandInfo {
    pub name: Option<StrOrConst>,
    pub description: Option<StrOrConst>,
    pub consts: HashMap<String, String>,
    pub props: HashMap<String, String>,
}

/// Scan a command class file. Heuristic (no execution, no data flow): a name built from
/// concatenation or a computed variable stays `None`.
pub(crate) fn command_info(src: &str) -> CommandInfo {
    let tokens = tokenize(src);
    let mut info = CommandInfo::default();
    let mut set_name = None;
    let mut ctor_name = None;

    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            // `const NAME = 'literal';`
            Token::Ident(k) if k == "const" => {
                if let (Some(Token::Ident(name)), Some(Token::Punct('=')), Some(Token::Str(v))) =
                    (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
                {
                    info.consts.insert(name.clone(), v.clone());
                    i += 4;
                    continue;
                }
            }
            // `$name = '…'` — a property default (`private $commandName = 'a:b';`, the
            // Symfony `$defaultName`) or a plain assignment; first one wins per name.
            Token::Punct('$') => {
                if let (Some(Token::Ident(prop)), Some(Token::Punct('=')), Some(Token::Str(v))) =
                    (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
                {
                    info.props.entry(prop.clone()).or_insert_with(|| v.clone());
                }
            }
            // `setName(<value>)` / `setDescription(<value>)`, where the value may be
            // wrapped in the `__('…')` translation helper.
            Token::Ident(k) if k == "setName" || k == "setDescription" => {
                if matches!(tokens.get(i + 1), Some(Token::Punct('('))) {
                    let mut v = str_or_const(&tokens, i + 2);
                    if v.is_none()
                        && matches!(tokens.get(i + 2), Some(Token::Ident(f)) if f == "__")
                        && matches!(tokens.get(i + 3), Some(Token::Punct('(')))
                    {
                        v = str_or_const(&tokens, i + 4);
                    }
                    if let Some(v) = v {
                        if k == "setName" {
                            set_name.get_or_insert(v);
                        } else {
                            info.description = Some(v);
                        }
                    }
                }
            }
            // `parent::__construct(<str|self::CONST>)` — Symfony's Command constructor
            // takes the name as its first argument.
            Token::Ident(k) if k == "parent" => {
                if let (
                    Some(Token::Punct(':')),
                    Some(Token::Punct(':')),
                    Some(Token::Ident(m)),
                    Some(Token::Punct('(')),
                ) = (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3), tokens.get(i + 4))
                {
                    if m == "__construct" {
                        if let Some(v) = str_or_const(&tokens, i + 5) {
                            ctor_name.get_or_insert(v);
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    let default_name = info.props.get("defaultName").cloned().map(StrOrConst::Str);
    info.name = set_name.or(default_name).or(ctor_name);
    if info.description.is_none() {
        info.description = info.props.get("defaultDescription").cloned().map(StrOrConst::Str);
    }
    info
}

/// Parse a string literal, a `self::CONST`/`static::CONST` reference, or a `$this->prop`
/// reference at `i` — but only when it's the whole argument (followed by `)` or `,`), so a
/// concatenation like `$this->prefix . 'x'` is never mistaken for its first operand.
fn str_or_const(tokens: &[Token], i: usize) -> Option<StrOrConst> {
    let ends_arg = |j: usize| matches!(tokens.get(j), Some(Token::Punct(')' | ',')));
    match tokens.get(i)? {
        Token::Str(s) if ends_arg(i + 1) => Some(StrOrConst::Str(s.clone())),
        Token::Ident(recv) if recv == "self" || recv == "static" => {
            if let (Some(Token::Punct(':')), Some(Token::Punct(':')), Some(Token::Ident(c))) =
                (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
            {
                if ends_arg(i + 4) {
                    return Some(StrOrConst::Const(c.clone()));
                }
            }
            None
        }
        Token::Punct('$') => {
            if let (
                Some(Token::Ident(this)),
                Some(Token::Punct('-')),
                Some(Token::Punct('>')),
                Some(Token::Ident(p)),
            ) = (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3), tokens.get(i + 4))
            {
                if this == "this" && ends_arg(i + 5) {
                    return Some(StrOrConst::Prop(p.clone()));
                }
            }
            None
        }
        _ => None,
    }
}

// ---------- EAV setup-call scan (`$eavSetup->addAttribute(entity, code, [...])`) ----------

/// A value in an `addAttribute` properties array — enough to render PHP-style.
pub(crate) enum EavValue {
    Str(String),
    Num(String),
    Bool(bool),
    Null,
    Class(ClassName),
    /// Anything non-literal (nested array, variable, concat) — display text only.
    Other(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EavCallKind {
    Add,
    Update,
    Remove,
}

pub(crate) struct RawEavCall {
    pub kind: EavCallKind,
    /// Resolved entity-type code (`catalog_product`) when recognizable, else the raw
    /// expression as written (`Foo::ENTITY`).
    pub entity: String,
    pub entity_known: bool,
    pub code: String,
    pub props: Vec<(String, EavValue)>,
    /// Byte offset of the method-name token (for line lookup).
    pub offset: usize,
}

/// Entity-type codes that exist on a stock install (for recognizing literal entity args).
const KNOWN_ENTITIES: [&str; 8] = [
    "catalog_product",
    "catalog_category",
    "customer",
    "customer_address",
    "order",
    "invoice",
    "creditmemo",
    "shipment",
];

/// `Class::CONST` idioms that name an entity type.
fn entity_const(class: &str, konst: &str) -> Option<&'static str> {
    match (class, konst) {
        ("Magento\\Catalog\\Model\\Product", "ENTITY") => Some("catalog_product"),
        ("Magento\\Catalog\\Model\\Category", "ENTITY") => Some("catalog_category"),
        ("Magento\\Customer\\Model\\Customer", "ENTITY") => Some("customer"),
        ("Magento\\Catalog\\Api\\Data\\ProductAttributeInterface", "ENTITY_TYPE_CODE") => {
            Some("catalog_product")
        }
        ("Magento\\Catalog\\Api\\Data\\CategoryAttributeInterface", "ENTITY_TYPE_CODE") => {
            Some("catalog_category")
        }
        ("Magento\\Customer\\Api\\CustomerMetadataInterface", "ENTITY_TYPE_CUSTOMER") => {
            Some("customer")
        }
        ("Magento\\Customer\\Api\\AddressMetadataInterface", "ENTITY_TYPE_ADDRESS") => {
            Some("customer_address")
        }
        _ => None,
    }
}

/// Scan a setup-script file for `addAttribute`/`updateAttribute`/`removeAttribute` calls
/// with literal entity + attribute-code arguments. Guards against unrelated methods of
/// the same name (SimpleXML's `addAttribute(name, value)`): an add must pass an array as
/// its third argument, and update/remove must name a recognizable entity. Method
/// *definitions* (EavSetup itself) are skipped.
pub(crate) fn eav_setup_calls(src: &str) -> Vec<RawEavCall> {
    let (tokens, offsets) = tokenize_at(src);
    let mut namespace = String::new();
    let mut uses: HashMap<String, String> = HashMap::new();
    let mut depth: i32 = 0;
    let mut out = Vec::new();

    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Ident(kw) if depth == 0 && kw == "namespace" => {
                if let Some(Token::Ident(ns)) = tokens.get(i + 1) {
                    namespace = ns.trim_matches('\\').to_string();
                }
                i = skip_to_semi_or_brace(&tokens, i);
                continue;
            }
            Token::Ident(kw) if depth == 0 && kw == "use" => {
                i = parse_use(&tokens, i + 1, &mut uses);
                continue;
            }
            Token::Punct('{') => depth += 1,
            Token::Punct('}') => depth -= 1,
            Token::Ident(m)
                if matches!(m.as_str(), "addAttribute" | "updateAttribute" | "removeAttribute") =>
            {
                let is_definition =
                    i > 0 && matches!(&tokens[i - 1], Token::Ident(k) if k == "function");
                if !is_definition && matches!(tokens.get(i + 1), Some(Token::Punct('('))) {
                    let kind = match m.as_str() {
                        "addAttribute" => EavCallKind::Add,
                        "updateAttribute" => EavCallKind::Update,
                        _ => EavCallKind::Remove,
                    };
                    if let Some(call) =
                        parse_eav_call(&tokens, i + 2, offsets[i], kind, &namespace, &uses)
                    {
                        out.push(call);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Parse one call's arguments, starting just after the opening `(`.
fn parse_eav_call(
    tokens: &[Token],
    mut i: usize,
    offset: usize,
    kind: EavCallKind,
    namespace: &str,
    uses: &HashMap<String, String>,
) -> Option<RawEavCall> {
    // Arg 1: the entity — a string literal or a `Class::CONST` reference.
    let (entity, entity_known, was_literal) = match tokens.get(i)? {
        Token::Str(s) => (s.clone(), KNOWN_ENTITIES.contains(&s.as_str()), true),
        Token::Ident(name) => {
            let (Some(Token::Punct(':')), Some(Token::Punct(':')), Some(Token::Ident(c))) =
                (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
            else {
                return None;
            };
            let resolved = resolve_name(name, namespace, uses);
            i += 3;
            match entity_const(resolved.as_str(), c) {
                Some(code) => (code.to_string(), true, false),
                None => (format!("{}::{c}", last_segment(name)), false, false),
            }
        }
        _ => return None,
    };
    i += 1;
    // An unrecognized literal entity on update/remove is almost certainly a different
    // method (SimpleXML etc.) — adds are additionally gated by the array arg below.
    if kind != EavCallKind::Add && was_literal && !entity_known {
        return None;
    }

    // Arg 2: the attribute code — must be a literal (a variable can't be indexed).
    if !matches!(tokens.get(i), Some(Token::Punct(','))) {
        return None;
    }
    let Some(Token::Str(code)) = tokens.get(i + 1) else { return None };
    let code = code.clone();
    i += 2;

    let mut props = Vec::new();
    match kind {
        EavCallKind::Remove => {}
        EavCallKind::Add => {
            // The properties array is required — it's what distinguishes
            // `EavSetup::addAttribute` from same-named methods.
            if !matches!(tokens.get(i), Some(Token::Punct(','))) {
                return None;
            }
            props = parse_eav_array(tokens, i + 1, namespace, uses)?;
        }
        EavCallKind::Update => {
            // (entity, code, [props]) or (entity, code, 'field', value).
            if matches!(tokens.get(i), Some(Token::Punct(','))) {
                if let Some(p) = parse_eav_array(tokens, i + 1, namespace, uses) {
                    props = p;
                } else if let Some(Token::Str(field)) = tokens.get(i + 1) {
                    if matches!(tokens.get(i + 2), Some(Token::Punct(','))) {
                        let (value, _) = parse_eav_value(tokens, i + 3, namespace, uses);
                        props.push((field.clone(), value));
                    }
                }
            }
        }
    }

    Some(RawEavCall { kind, entity, entity_known, code, props, offset })
}

/// Parse a `[...]` / `array(...)` literal at `i` into scalar key => value pairs. Nested
/// arrays and non-literal values are kept as `Other`. Returns `None` if `i` isn't an array.
fn parse_eav_array(
    tokens: &[Token],
    mut i: usize,
    namespace: &str,
    uses: &HashMap<String, String>,
) -> Option<Vec<(String, EavValue)>> {
    let close = match tokens.get(i)? {
        Token::Punct('[') => ']',
        Token::Ident(a) if a == "array" && matches!(tokens.get(i + 1), Some(Token::Punct('('))) => {
            i += 1;
            ')'
        }
        _ => return None,
    };
    i += 1;

    let mut props = Vec::new();
    loop {
        while matches!(tokens.get(i), Some(Token::Punct(','))) {
            i += 1;
        }
        match tokens.get(i) {
            None => break,
            Some(Token::Punct(c)) if *c == close => break,
            _ => {}
        }
        // A scalar `'key' => value` pair; anything else is skipped to the next element.
        if let (Some(Token::Str(key)), Some(Token::Punct('=')), Some(Token::Punct('>'))) =
            (tokens.get(i), tokens.get(i + 1), tokens.get(i + 2))
        {
            let (value, next) = parse_eav_value(tokens, i + 3, namespace, uses);
            props.push((key.clone(), value));
            i = next;
        }
        i = skip_element(tokens, i, close);
    }
    Some(props)
}

/// Parse one array value at `i`. Returns the value and the index just past it; the caller
/// still skips to the element boundary (so a concatenation's tail is consumed there, and
/// the captured first operand is downgraded by the boundary check below).
fn parse_eav_value(
    tokens: &[Token],
    i: usize,
    namespace: &str,
    uses: &HashMap<String, String>,
) -> (EavValue, usize) {
    let ends_element = |j: usize| {
        matches!(tokens.get(j), Some(Token::Punct(',' | ']' | ')'))) || tokens.get(j).is_none()
    };
    match tokens.get(i) {
        Some(Token::Str(s)) if ends_element(i + 1) => (EavValue::Str(s.clone()), i + 1),
        Some(Token::Ident(k)) if (k == "true" || k == "false") && ends_element(i + 1) => {
            (EavValue::Bool(k == "true"), i + 1)
        }
        Some(Token::Ident(k)) if k == "null" && ends_element(i + 1) => (EavValue::Null, i + 1),
        Some(Token::Ident(n)) if n.bytes().all(|b| b.is_ascii_digit()) && ends_element(i + 1) => {
            (EavValue::Num(n.clone()), i + 1)
        }
        Some(Token::Punct('-')) => match tokens.get(i + 1) {
            Some(Token::Ident(n)) if n.bytes().all(|b| b.is_ascii_digit()) && ends_element(i + 2) => {
                (EavValue::Num(format!("-{n}")), i + 2)
            }
            _ => (EavValue::Other("…".to_string()), i + 1),
        },
        Some(Token::Ident(n)) => {
            if let (Some(Token::Punct(':')), Some(Token::Punct(':')), Some(Token::Ident(c))) =
                (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
            {
                if ends_element(i + 4) {
                    let v = if c == "class" {
                        EavValue::Class(resolve_name(n, namespace, uses))
                    } else {
                        EavValue::Other(format!("{}::{c}", last_segment(n)))
                    };
                    return (v, i + 4);
                }
            }
            (EavValue::Other("…".to_string()), i + 1)
        }
        // Return `i` unadvanced so skip_element counts the opening bracket itself.
        Some(Token::Punct('[')) => (EavValue::Other("[…]".to_string()), i),
        _ => (EavValue::Other("…".to_string()), i + 1),
    }
}

/// Skip to the next `,` (or the array's closing bracket) at nesting depth 0.
fn skip_element(tokens: &[Token], mut i: usize, close: char) -> usize {
    let mut depth = 0i32;
    while let Some(t) = tokens.get(i) {
        match t {
            Token::Punct('[' | '(') => depth += 1,
            Token::Punct(']' | ')') if depth > 0 => depth -= 1,
            Token::Punct(c) if depth == 0 && (*c == ',' || *c == close) => return i,
            _ => {}
        }
        i += 1;
    }
    i
}

/// Look back over modifier keywords before a `function` token: a method is public unless
/// explicitly `private`/`protected`.
fn is_public_method(tokens: &[Token], func_idx: usize) -> bool {
    for back in 1..=3 {
        let Some(idx) = func_idx.checked_sub(back) else { break };
        match &tokens[idx] {
            Token::Ident(m) if m == "private" || m == "protected" => return false,
            Token::Ident(m) if matches!(m.as_str(), "public" | "final" | "static" | "abstract") => {
                continue
            }
            _ => break,
        }
    }
    true
}

/// `Save` -> `save`, `GetList` -> `getList`.
pub(crate) fn lcfirst(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn last_segment(name: &str) -> String {
    name.trim_matches('\\').rsplit('\\').next().unwrap_or(name).to_string()
}

fn skip_to_semi(tokens: &[Token], mut i: usize) -> usize {
    while i < tokens.len() && !matches!(tokens[i], Token::Punct(';')) {
        i += 1;
    }
    i + 1
}

fn skip_to_semi_or_brace(tokens: &[Token], mut i: usize) -> usize {
    while i < tokens.len() && !matches!(tokens[i], Token::Punct(';') | Token::Punct('{')) {
        i += 1;
    }
    i
}

fn tokenize(s: &str) -> Vec<Token> {
    tokenize_at(s).0
}

/// Tokenize, additionally returning each token's starting byte offset (for line lookup).
fn tokenize_at(s: &str) -> (Vec<Token>, Vec<usize>) {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            // Line comments: `//` and `#` (but `#[` is a PHP 8 attribute, not a comment).
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' if b.get(i + 1) != Some(&b'[') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            // Block comment.
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            // Strings — captured (command names/descriptions live in literals). Escapes are
            // collapsed to the escaped byte, which is exact for `\'`/`\"`/`\\` (all that
            // matters in the identifiers we read).
            b'\'' | b'"' => {
                let q = c;
                offsets.push(i);
                i += 1;
                let mut bytes = Vec::new();
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    bytes.push(b[i]);
                    i += 1;
                }
                i += 1;
                out.push(Token::Str(String::from_utf8_lossy(&bytes).into_owned()));
            }
            _ if is_ident(c) => {
                let start = i;
                while i < b.len() && is_ident(b[i]) {
                    i += 1;
                }
                offsets.push(start);
                out.push(Token::Ident(s[start..i].to_string()));
            }
            _ => {
                offsets.push(i);
                out.push(Token::Punct(c as char));
                i += 1;
            }
        }
    }
    (out, offsets)
}

fn is_ident(c: u8) -> bool {
    c == b'_' || c == b'\\' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod command_tests {
    use super::{command_info, StrOrConst};

    fn as_str(v: &Option<StrOrConst>) -> Option<&str> {
        match v {
            Some(StrOrConst::Str(s)) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn literal_set_name_and_description() {
        let src = r#"<?php
class CronCommand extends Command
{
    protected function configure()
    {
        $this->setName('cron:run')
            ->setDescription('Runs jobs by schedule')
            ->setDefinition($options);
    }
}"#;
        let info = command_info(src);
        assert_eq!(as_str(&info.name), Some("cron:run"));
        assert_eq!(as_str(&info.description), Some("Runs jobs by schedule"));
    }

    #[test]
    fn const_reference_resolves_via_consts() {
        let src = r#"<?php
class QueryLogEnableCommand extends Command
{
    public const COMMAND_NAME = 'dev:query-log:enable';
    protected function configure()
    {
        $this->setName(self::COMMAND_NAME);
    }
}"#;
        let info = command_info(src);
        let Some(StrOrConst::Const(c)) = &info.name else { panic!("expected const ref") };
        assert_eq!(c, "COMMAND_NAME");
        assert_eq!(info.consts.get("COMMAND_NAME").map(String::as_str), Some("dev:query-log:enable"));
    }

    #[test]
    fn default_name_and_ctor_fallbacks() {
        let sym = r#"<?php
class A extends Command
{
    protected static $defaultName = 'app:sym';
    protected static $defaultDescription = 'Symfony style';
}"#;
        let info = command_info(sym);
        assert_eq!(as_str(&info.name), Some("app:sym"));
        assert_eq!(as_str(&info.description), Some("Symfony style"));

        let ctor = r#"<?php
class B extends Command
{
    public function __construct()
    {
        parent::__construct('app:ctor');
    }
}"#;
        assert_eq!(as_str(&command_info(ctor).name), Some("app:ctor"));

        // setName wins over both.
        let both = r#"<?php
class C extends Command
{
    protected static $defaultName = 'app:old';
    protected function configure()
    {
        $this->setName('app:new');
    }
}"#;
        assert_eq!(as_str(&command_info(both).name), Some("app:new"));
    }

    #[test]
    fn dynamic_name_stays_none() {
        let src = r#"<?php
class D extends Command
{
    protected function configure()
    {
        $this->setName($this->prefix . 'thing');
    }
}"#;
        assert!(command_info(src).name.is_none());
    }
}

#[cfg(test)]
mod eav_tests {
    use super::{eav_setup_calls, EavCallKind, EavValue};

    #[test]
    fn add_update_remove_and_guards() {
        let src = r#"<?php
namespace Hyva\CmsTailwindJit\Setup\Patch\Data;

use Magento\Catalog\Api\Data\ProductAttributeInterface;
use Magento\Eav\Model\Entity\Attribute\Source\Boolean;

class AddIsJitEnabledProductAttribute implements DataPatchInterface
{
    public function apply()
    {
        $this->eavSetup->addAttribute(
            ProductAttributeInterface::ENTITY_TYPE_CODE,
            'is_tailwindcss_jit_enabled',
            [
                'type'         => 'int',
                'label'        => 'Generate Hyvä Styles',
                'required'     => 0,
                'user_defined' => true,
                'source'       => Boolean::class,
                'sort_order'   => 3,
                'option'       => ['values' => ['a', 'b']],
                'default'      => null,
            ]
        );
        $this->eavSetup->updateAttribute('catalog_product', 'color', 'is_searchable', 1);
        $this->eavSetup->removeAttribute(\Magento\Catalog\Model\Category::ENTITY, 'old_attr');
        // SimpleXML lookalikes — no array third arg / unknown literal entity:
        $xml->addAttribute('name', 'value');
        $xml->addAttribute('xsi:type', 'varchar', 'xsi');
        $other->updateAttribute('name', 'thing', 'x', 1);
    }

    public function addAttribute($entityTypeId, $code, array $attr) { return $this; }
}"#;
        let calls = eav_setup_calls(src);
        assert_eq!(calls.len(), 3);

        let add = &calls[0];
        assert!(matches!(add.kind, EavCallKind::Add));
        assert_eq!((add.entity.as_str(), add.entity_known), ("catalog_product", true));
        assert_eq!(add.code, "is_tailwindcss_jit_enabled");
        let prop = |k: &str| add.props.iter().find(|(key, _)| key == k).map(|(_, v)| v);
        assert!(matches!(prop("type"), Some(EavValue::Str(s)) if s == "int"));
        assert!(matches!(prop("required"), Some(EavValue::Num(n)) if n == "0"));
        assert!(matches!(prop("user_defined"), Some(EavValue::Bool(true))));
        assert!(matches!(prop("source"), Some(EavValue::Class(c))
            if c.as_str() == "Magento\\Eav\\Model\\Entity\\Attribute\\Source\\Boolean"));
        assert!(matches!(prop("option"), Some(EavValue::Other(_)))); // nested array survives
        assert!(matches!(prop("default"), Some(EavValue::Null)));
        assert_eq!(add.props.len(), 8);

        let upd = &calls[1];
        assert!(matches!(upd.kind, EavCallKind::Update));
        assert_eq!((upd.entity.as_str(), upd.code.as_str()), ("catalog_product", "color"));
        assert!(matches!(&upd.props[..], [(k, EavValue::Num(n))] if k == "is_searchable" && n == "1"));

        let rem = &calls[2];
        assert!(matches!(rem.kind, EavCallKind::Remove));
        assert_eq!((rem.entity.as_str(), rem.code.as_str()), ("catalog_category", "old_attr"));
    }
}
