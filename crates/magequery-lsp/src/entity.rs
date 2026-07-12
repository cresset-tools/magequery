//! Position → entity: what Magento thing sits under the cursor.
//!
//! Core answers **name → facts**; an editor asks **position → facts**. This module is
//! the inversion layer, and it is deliberately pure text: no `Magento` handle, no XML
//! DOM. A line-local scan finds the attribute value / text node / PHP token at the
//! offset, and the classification uses position first (a `type=` attribute is
//! class-valued whatever the value looks like — virtual type names have no backslash),
//! shape second (`Vendor\Thing` → class, `Vendor_Module::x` → ACL id, `a/b/c` → config
//! path). Multi-line attribute values are the accepted blind spot.

use std::ops::Range;

use magequery_core::{ClassName, EventName, ModuleName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entity {
    Class(ClassName),
    Event(EventName),
    ConfigPath(String),
    Acl(String),
    Module(ModuleName),
    /// A `before*/around*/after*` method *declaration* in a plugin class (the method
    /// name, e.g. `aroundSave`). Definition jumps to the intercepted method on the
    /// plugin's target type(s).
    PluginMethod(String),
    /// Any other method *declaration*. Definition/references resolve the plugins that
    /// intercept it (the reverse of [`Entity::PluginMethod`]); nothing when the class
    /// has none, leaving the verb to the PHP language server.
    Method(String),
    /// `Vendor_Module::path/file.phtml` — a template reference in layout XML (or a PHP
    /// string). Distinguished from ACL ids by the `.`/`/` in the path half.
    Template(String),
    /// `<update handle="…">` in layout XML.
    LayoutHandle(String),
    /// A block/container name in layout XML (declaration or reference).
    BlockName(String),
    /// A db_schema table name (`table=`/`referenceTable=` attributes).
    Table(String),
    /// A routes.xml route, by `id=` or `frontName=` on `<route>` (either value matches
    /// against both fields — they're usually equal anyway).
    Route(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EntityAt {
    pub entity: Entity,
    /// Byte span of the token in the file text (the hover highlight).
    pub span: Range<usize>,
}

pub(crate) fn entity_at(file_name: &str, text: &str, offset: usize) -> Option<EntityAt> {
    if offset > text.len() {
        return None;
    }
    if file_name.ends_with(".xml") {
        xml_entity_at(text, offset)
    } else if file_name.ends_with(".php") {
        php_entity_at(text, offset)
    } else if file_name.ends_with(".graphqls") {
        gql_entity_at(text, offset)
    } else {
        None
    }
}

// ---- XML ---------------------------------------------------------------------------

fn xml_entity_at(text: &str, offset: usize) -> Option<EntityAt> {
    if let Some((attr, span)) = attribute_value_at(text, offset) {
        let element = enclosing_tag(text, span.start);
        return classify_xml(element.as_deref(), Some(&attr), text, span);
    }
    let span = text_node_at(text, offset)?;
    let element = enclosing_tag(text, span.start);
    classify_xml(element.as_deref(), None, text, span)
}

/// The quoted attribute value containing `offset` on its line, as (attribute name,
/// value span). Line-local: config XML virtually never wraps an attribute value.
fn attribute_value_at(text: &str, offset: usize) -> Option<(String, Range<usize>)> {
    let (line_start, line) = line_around(text, offset);
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        if quote == b'"' || quote == b'\'' {
            let value_start = i + 1;
            let mut j = value_start;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            if j >= bytes.len() {
                return None; // unterminated on this line
            }
            let span = line_start + value_start..line_start + j;
            if span.contains(&offset) || offset == span.end {
                let name = attribute_name_before(line, i)?;
                return Some((name, span));
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    None
}

/// The `name` of `name="` whose opening quote sits at byte `quote` of `line`.
fn attribute_name_before(line: &str, quote: usize) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = quote;
    // skip back over `=` and any whitespace around it
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'=' {
        return None;
    }
    i -= 1;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    let end = i;
    while i > 0 && is_xml_name_byte(bytes[i - 1]) {
        i -= 1;
    }
    (i < end).then(|| line[i..end].to_string())
}

fn is_xml_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':')
}

/// The trimmed text-node run around `offset` (between `>` and `<`), or `None` when the
/// cursor is inside a tag.
fn text_node_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 {
        match bytes[start - 1] {
            b'>' => break,
            b'<' => return None, // inside a tag
            _ => start -= 1,
        }
    }
    let mut end = offset;
    while end < bytes.len() {
        match bytes[end] {
            b'<' => break,
            b'>' => return None,
            _ => end += 1,
        }
    }
    let run = &text[start..end];
    let trimmed_start = start + (run.len() - run.trim_start().len());
    let trimmed_end = end - (run.len() - run.trim_end().len());
    (trimmed_start < trimmed_end).then_some(trimmed_start..trimmed_end)
}

/// The element whose tag opens nearest before `before`, with the full tag text (so the
/// caller can check things like `xsi:type="object"`). Skips closers/comments/PIs.
fn enclosing_tag(text: &str, before: usize) -> Option<String> {
    let open = text[..before].rfind('<')?;
    let rest = &text[open + 1..];
    if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
        return None;
    }
    let close = text[open..].find('>').map(|i| open + i).unwrap_or(text.len());
    Some(text[open + 1..close].to_string())
}

fn tag_name(tag: &str) -> &str {
    tag.split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
}

fn classify_xml(
    tag: Option<&str>,
    attr: Option<&str>,
    text: &str,
    span: Range<usize>,
) -> Option<EntityAt> {
    let value = &text[span.clone()];
    let element = tag.map(tag_name).unwrap_or("");

    // Position first: attributes whose value is a type name in every config file that
    // uses them (di, events, crontab, webapi, communication, widget, mview, indexer, …).
    // `name` counts only where the *named thing* is a type (di.xml `<type>`,
    // `<virtualType>`). `xsi:type` never matches — the prefix is part of the name.
    // Layout XML first: these attributes are unambiguous wherever they appear.
    if attr == Some("template") {
        return Some(EntityAt {
            entity: Entity::Template(value.to_string()),
            span,
        });
    }
    if attr == Some("handle") && element == "update" {
        return Some(EntityAt {
            entity: Entity::LayoutHandle(value.to_string()),
            span,
        });
    }
    if matches!(attr, Some("id") | Some("frontName")) && element == "route" {
        return Some(EntityAt {
            entity: Entity::Route(value.to_string()),
            span,
        });
    }
    if matches!(attr, Some("referenceTable"))
        || (attr == Some("table") && element == "constraint")
        || (attr == Some("name") && element == "table")
    {
        return Some(EntityAt {
            entity: Entity::Table(value.to_string()),
            span,
        });
    }
    if (attr == Some("name")
        && matches!(element, "block" | "container" | "referenceBlock" | "referenceContainer"))
        || (matches!(attr, Some("element") | Some("destination")) && element == "move")
    {
        return Some(EntityAt {
            entity: Entity::BlockName(value.to_string()),
            span,
        });
    }

    let class_position = match attr {
        Some("type" | "instance" | "class" | "service" | "for" | "handler") => true,
        Some("name") => matches!(element, "type" | "virtualType"),
        None => {
            matches!(element, "source_model" | "backend_model" | "frontend_model")
                || (matches!(element, "argument" | "item")
                    && tag.is_some_and(|t| t.contains("\"object\"") || t.contains("'object'")))
        }
        _ => false,
    };
    if class_position {
        return Some(class_entity(value, span));
    }

    if attr == Some("name") && element == "event" {
        return Some(EntityAt {
            entity: Entity::Event(EventName::new(value)),
            span,
        });
    }
    // webapi `<resource ref=…>`, acl.xml `<resource id=…>`, menu `<add resource=…>`.
    if (attr == Some("ref") || attr == Some("resource") || attr == Some("id"))
        && (element == "resource" || attr == Some("resource"))
        && acl_shaped(value)
    {
        return Some(EntityAt { entity: Entity::Acl(value.to_string()), span });
    }
    if element == "config_path" && attr.is_none() {
        return Some(EntityAt {
            entity: Entity::ConfigPath(value.to_string()),
            span,
        });
    }

    // Shape fallback for everything else.
    classify_shape(value, span)
}

fn class_entity(value: &str, span: Range<usize>) -> EntityAt {
    // `Class::method` handler references: the entity is the class half.
    let class = value.split("::").next().unwrap_or(value);
    EntityAt {
        entity: Entity::Class(ClassName::new(class)),
        span,
    }
}

fn classify_shape(value: &str, span: Range<usize>) -> Option<EntityAt> {
    if value.is_empty() || value.len() > 300 {
        return None;
    }
    if value.contains('\\') {
        return Some(class_entity(value, span));
    }
    if template_shaped(value) {
        return Some(EntityAt {
            entity: Entity::Template(value.to_string()),
            span,
        });
    }
    if acl_shaped(value) {
        return Some(EntityAt { entity: Entity::Acl(value.to_string()), span });
    }
    if module_shaped(value) {
        return Some(EntityAt {
            entity: Entity::Module(ModuleName::new(value)),
            span,
        });
    }
    if config_path_shaped(value) {
        return Some(EntityAt {
            entity: Entity::ConfigPath(value.to_string()),
            span,
        });
    }
    None
}

/// `Vendor_Module::path/to/file.phtml` — the path half has `/` or `.`, which an ACL
/// resource id never does.
fn template_shaped(value: &str) -> bool {
    match value.split_once("::") {
        Some((module, path)) => {
            module_shaped(module)
                && !path.is_empty()
                && (path.contains('/') || path.contains('.'))
                && !path.contains(char::is_whitespace)
        }
        None => false,
    }
}

/// `Vendor_Module::resource_id`.
fn acl_shaped(value: &str) -> bool {
    match value.split_once("::") {
        Some((module, resource)) => {
            module_shaped(module)
                && !resource.is_empty()
                && resource.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// `Vendor_Module` — exactly two CamelCase halves.
fn module_shaped(value: &str) -> bool {
    match value.split_once('_') {
        Some((vendor, module)) => [vendor, module].iter().all(|half| {
            half.starts_with(|c: char| c.is_ascii_uppercase())
                && half.chars().all(|c| c.is_ascii_alphanumeric())
                && !half.is_empty()
        }),
        None => false,
    }
}

/// `section/group/field` — at least three lowercase segments, so two-segment strings
/// (URL paths, `catalog/product` sort keys) don't light up as config.
fn config_path_shaped(value: &str) -> bool {
    let segments: Vec<&str> = value.split('/').collect();
    segments.len() >= 3
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

// ---- PHP ---------------------------------------------------------------------------

fn php_entity_at(text: &str, offset: usize) -> Option<EntityAt> {
    // Inside a string literal: config paths, event names, FQCN strings.
    if let Some(span) = php_string_at(text, offset) {
        let content = &text[span.clone()];
        if content.contains('\\') {
            let name = content.replace("\\\\", "\\");
            let class = name.split("::").next().unwrap_or(&name).to_string();
            return Some(EntityAt {
                entity: Entity::Class(ClassName::new(class)),
                span,
            });
        }
        if config_path_shaped(content) {
            return Some(EntityAt {
                entity: Entity::ConfigPath(content.to_string()),
                span,
            });
        }
        if template_shaped(content) {
            return Some(EntityAt {
                entity: Entity::Template(content.to_string()),
                span,
            });
        }
        if acl_shaped(content) {
            return Some(EntityAt { entity: Entity::Acl(content.to_string()), span });
        }
        // An event name only where it's dispatched — bare snake_case strings are
        // anything (array keys, column names).
        let (line_start, line) = line_around(text, span.start);
        if line[..span.start - line_start].contains("dispatch") {
            return Some(EntityAt {
                entity: Entity::Event(EventName::new(content)),
                span,
            });
        }
        return None;
    }

    let span = expand(text, offset, |c: char| {
        c.is_ascii_alphanumeric() || c == '_' || c == '\\'
    });
    let token = text[span.clone()].trim_matches('\\');
    if token.is_empty() {
        return None;
    }
    if token.contains('\\') {
        let class = token.split("::").next().unwrap_or(token);
        return Some(EntityAt {
            entity: Entity::Class(ClassName::new(class)),
            span,
        });
    }
    // A method being *declared*: interception-shaped names are (probable) plugin
    // methods, everything else is a potential interception target.
    if text[..span.start].trim_end().ends_with("function") {
        return Some(EntityAt {
            entity: if is_intercept_shaped(token) {
                Entity::PluginMethod(token.to_string())
            } else {
                Entity::Method(token.to_string())
            },
            span,
        });
    }
    // A bare identifier: the class this file declares, or a `use`-imported name.
    if let Some(class) = own_class_at(text, &span, token).or_else(|| use_import(text, token)) {
        return Some(EntityAt { entity: Entity::Class(class), span });
    }
    None
}

/// `beforeX`/`aroundX`/`afterX` with an uppercase target — Magento's interception naming.
fn is_intercept_shaped(name: &str) -> bool {
    ["before", "around", "after"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_uppercase())
    })
}

/// The single-quoted or double-quoted string content containing `offset`, line-local.
fn php_string_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let (line_start, line) = line_around(text, offset);
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        if quote == b'"' || quote == b'\'' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != quote {
                if bytes[j] == b'\\' {
                    j += 1; // escape: skip the next byte
                }
                j += 1;
            }
            if j >= bytes.len() {
                return None;
            }
            let span = line_start + start..line_start + j;
            if span.contains(&offset) || offset == span.end {
                return Some(span);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    None
}

/// When the token is the name in this file's own `class|interface|trait Foo` header,
/// the FQCN comes from the file's `namespace` declaration.
fn own_class_at(text: &str, span: &Range<usize>, token: &str) -> Option<ClassName> {
    let before = text[..span.start].trim_end();
    let is_decl = ["class", "interface", "trait", "enum"]
        .iter()
        .any(|kw| before.ends_with(kw));
    if !is_decl {
        return None;
    }
    let namespace = text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("namespace ")?;
        Some(rest.trim_end_matches(';').trim().to_string())
    })?;
    Some(ClassName::new(format!("{namespace}\\{token}")))
}

/// Resolve a bare identifier through the file's `use` imports
/// (`use A\B\C;` / `use A\B\C as D;`).
fn use_import(text: &str, token: &str) -> Option<ClassName> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("use ") else { continue };
        let rest = rest.trim_end().trim_end_matches(';');
        if rest.starts_with("function ") || rest.starts_with("const ") || rest.contains('{') {
            continue;
        }
        let (path, alias) = match rest.split_once(" as ") {
            Some((path, alias)) => (path.trim(), alias.trim()),
            None => (rest, rest.rsplit('\\').next().unwrap_or(rest)),
        };
        if alias == token && path.contains('\\') {
            return Some(ClassName::new(path));
        }
    }
    None
}

// ---- GraphQL SDL -------------------------------------------------------------------

fn gql_entity_at(text: &str, offset: usize) -> Option<EntityAt> {
    // The only Magento entities in .graphqls are the FQCNs inside directive strings
    // (`@resolver(class: "Magento\\CatalogGraphQl\\...")`), written with doubled
    // backslashes per the SDL string escape rules.
    let span = expand(text, offset, |c: char| {
        c.is_ascii_alphanumeric() || c == '_' || c == '\\'
    });
    let token = text[span.clone()].trim_matches('\\');
    if !token.contains('\\') {
        return None;
    }
    Some(EntityAt {
        entity: Entity::Class(ClassName::new(token.replace("\\\\", "\\"))),
        span,
    })
}

// ---- completion --------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletionKind {
    /// FQCNs and virtual type names.
    Class,
    Event,
    ConfigPath,
    Acl,
    Module,
    /// db_schema table names (`referenceTable=`).
    Table,
    /// Template references in layout XML.
    Template,
    /// Layout handles (`<update handle=`).
    LayoutHandle,
    /// Block/container names being *referenced* (referenceBlock/move — not declarations).
    BlockName,
    /// Columns of the same tag's `referenceTable=` (db_schema foreign keys).
    Column(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CompletionCtx {
    pub kind: CompletionKind,
    /// What's typed of the value so far — the filter, and the span the completion
    /// replaces (value start .. cursor).
    pub typed: String,
    pub span: Range<usize>,
}

/// The completion context at `offset`, tolerant of mid-edit states: an unterminated
/// attribute value (`type="Mag`), an auto-paired one (`type="Mag|"`), or a partial
/// text node. Position rules mirror [`entity_at`]'s classification.
pub(crate) fn completion_context(
    file_name: &str,
    text: &str,
    offset: usize,
) -> Option<CompletionCtx> {
    if offset > text.len() {
        return None;
    }
    if file_name.ends_with(".xml") {
        xml_completion_context(text, offset)
    } else if file_name.ends_with(".php") {
        php_completion_context(text, offset)
    } else {
        None
    }
}

fn xml_completion_context(text: &str, offset: usize) -> Option<CompletionCtx> {
    let (line_start, line) = line_around(text, offset);
    let cursor = offset - line_start;
    // Walk the line up to the cursor with quote state: if we end inside a quote, the
    // cursor sits in an attribute value opened at `open`.
    let bytes = line.as_bytes();
    let mut open: Option<usize> = None;
    let mut quote = 0u8;
    for (i, &b) in bytes.iter().enumerate().take_while(|(i, _)| *i < cursor) {
        match open {
            None if b == b'"' || b == b'\'' => {
                open = Some(i);
                quote = b;
            }
            Some(_) if b == quote => open = None,
            _ => {}
        }
    }
    if let Some(open) = open {
        let attr = attribute_name_before(line, open)?;
        let value_start = line_start + open + 1;
        let typed = text[value_start..offset].to_string();
        let tag = enclosing_tag(text, value_start);
        let element = tag.as_deref().map(tag_name).unwrap_or("");
        let kind = match attr.as_str() {
            "type" | "instance" | "class" | "service" | "for" | "handler" => {
                Some(CompletionKind::Class)
            }
            "name" if matches!(element, "type" | "virtualType") => Some(CompletionKind::Class),
            "name" if element == "event" => Some(CompletionKind::Event),
            "name" if element == "module" => Some(CompletionKind::Module),
            "name" if matches!(element, "referenceBlock" | "referenceContainer") => {
                Some(CompletionKind::BlockName)
            }
            "element" | "destination" if element == "move" => Some(CompletionKind::BlockName),
            "template" => Some(CompletionKind::Template),
            "handle" if element == "update" => Some(CompletionKind::LayoutHandle),
            "ref" | "resource" => Some(CompletionKind::Acl),
            "referenceTable" => Some(CompletionKind::Table),
            "table" if element == "constraint" => Some(CompletionKind::Table),
            "referenceColumn" => tag.as_deref().and_then(|t| {
                // The referenced table comes from the same tag.
                let idx = t.find("referenceTable=")? + "referenceTable=".len();
                let quote = *t.as_bytes().get(idx)?;
                let rest = &t[idx + 1..];
                let end = rest.find(quote as char)?;
                Some(CompletionKind::Column(rest[..end].to_string()))
            }),
            _ => None,
        }?;
        return Some(CompletionCtx { kind, typed, span: value_start..offset });
    }

    // Text-node context: between `>` and the cursor with no intervening `<`/`>`.
    let run_start = {
        let mut i = offset;
        loop {
            if i == 0 {
                return None;
            }
            match text.as_bytes()[i - 1] {
                b'>' => break i,
                b'<' => return None,
                _ => i -= 1,
            }
        }
    };
    let run = &text[run_start..offset];
    let typed_start = run_start + (run.len() - run.trim_start().len());
    let typed = text[typed_start..offset].to_string();
    let tag = enclosing_tag(text, run_start)?;
    let element = tag_name(&tag);
    let kind = match element {
        "source_model" | "backend_model" | "frontend_model" => CompletionKind::Class,
        "config_path" => CompletionKind::ConfigPath,
        "resource" => CompletionKind::Acl,
        "argument" | "item"
            if tag.contains("\"object\"") || tag.contains("'object'") =>
        {
            CompletionKind::Class
        }
        _ => return None,
    };
    Some(CompletionCtx { kind, typed, span: typed_start..offset })
}

fn php_completion_context(text: &str, offset: usize) -> Option<CompletionCtx> {
    let (line_start, line) = line_around(text, offset);
    let cursor = offset - line_start;
    let bytes = line.as_bytes();
    let mut open: Option<usize> = None;
    let mut quote = 0u8;
    let mut i = 0;
    while i < cursor.min(bytes.len()) {
        let b = bytes[i];
        match open {
            None if b == b'"' || b == b'\'' => {
                open = Some(i);
                quote = b;
            }
            Some(_) if b == b'\\' => i += 1, // escape inside the string
            Some(_) if b == quote => open = None,
            _ => {}
        }
        i += 1;
    }
    let open = open?;
    let before = &line[..open];
    let kind = if before.contains("dispatch") {
        CompletionKind::Event
    } else if before.contains("getValue") || before.contains("isSetFlag") {
        CompletionKind::ConfigPath
    } else if before.contains("isAllowed") || before.contains("ADMIN_RESOURCE") {
        CompletionKind::Acl
    } else {
        return None;
    };
    let value_start = line_start + open + 1;
    Some(CompletionCtx {
        kind,
        typed: text[value_start..offset].to_string(),
        span: value_start..offset,
    })
}

// ---- shared ------------------------------------------------------------------------

/// The line containing `offset`, as (line start offset, line text without newline).
fn line_around(text: &str, offset: usize) -> (usize, &str) {
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
    (start, &text[start..end])
}

/// Expand around `offset` over bytes satisfying `pred` (ASCII-oriented on purpose —
/// every entity charset here is ASCII).
fn expand(text: &str, offset: usize, pred: impl Fn(char) -> bool) -> Range<usize> {
    let bytes = text.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 && (bytes[start - 1] as char).is_ascii() && pred(bytes[start - 1] as char) {
        start -= 1;
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() && (bytes[end] as char).is_ascii() && pred(bytes[end] as char) {
        end += 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(file: &str, text: &str, needle: &str) -> Option<Entity> {
        // Position the cursor in the middle of the (first) needle occurrence.
        let offset = text.find(needle).unwrap() + needle.len() / 2;
        entity_at(file, text, offset).map(|e| e.entity)
    }

    #[test]
    fn di_xml_positions() {
        let di = r#"<config>
    <preference for="Magento\Quote\Api\CartManagementInterface" type="Magento\Quote\Model\QuoteManagement" />
    <type name="Magento\Framework\App\RouterList">
        <arguments>
            <argument name="routerList" xsi:type="array">
                <item name="blog" xsi:type="object">Magefan\Blog\Controller\Router</item>
            </argument>
        </arguments>
        <plugin name="clean_cache" type="Magento\Framework\App\Cache\CleanCachePlugin" sortOrder="10"/>
    </type>
    <virtualType name="sessionStorage" type="Magento\Framework\Session\Storage"/>
</config>"#;
        assert_eq!(
            at("di.xml", di, "CartManagementInterface"),
            Some(Entity::Class(ClassName::new(
                "Magento\\Quote\\Api\\CartManagementInterface"
            )))
        );
        assert_eq!(
            at("di.xml", di, "RouterList\">"),
            Some(Entity::Class(ClassName::new("Magento\\Framework\\App\\RouterList")))
        );
        // Text node of an object item — a class even without position rules by shape,
        // but here also by the xsi:type="object" position.
        assert_eq!(
            at("di.xml", di, "Magefan\\Blog\\Controller\\Router"),
            Some(Entity::Class(ClassName::new("Magefan\\Blog\\Controller\\Router")))
        );
        // The virtual type NAME is a class entity despite having no backslash.
        assert_eq!(
            at("di.xml", di, "sessionStorage"),
            Some(Entity::Class(ClassName::new("sessionStorage")))
        );
        // The plugin name is a declaration name, not an entity.
        assert_eq!(at("di.xml", di, "clean_cache"), None);
    }

    #[test]
    fn events_webapi_and_system_xml() {
        let events = r#"<event name="checkout_cart_product_add_after">
            <observer name="mine" instance="Acme\Widget\Observer\Recalc" />
        </event>"#;
        assert_eq!(
            at("events.xml", events, "checkout_cart_product_add_after"),
            Some(Entity::Event(EventName::new("checkout_cart_product_add_after")))
        );
        assert_eq!(
            at("events.xml", events, "Acme\\Widget\\Observer\\Recalc"),
            Some(Entity::Class(ClassName::new("Acme\\Widget\\Observer\\Recalc")))
        );

        let webapi = r#"<route url="/V1/products" method="GET">
            <service class="Magento\Catalog\Api\ProductRepositoryInterface" method="getList"/>
            <resources><resource ref="Magento_Catalog::products" /></resources>
        </route>"#;
        assert_eq!(
            at("webapi.xml", webapi, "Magento_Catalog::products"),
            Some(Entity::Acl("Magento_Catalog::products".to_string()))
        );

        let system = r#"<field id="engine" type="select">
            <source_model>Magento\Search\Model\Adminhtml\System\Config\Source\Engine</source_model>
            <config_path>catalog/search/engine</config_path>
        </field>"#;
        assert_eq!(
            at("system.xml", system, "Source\\Engine"),
            Some(Entity::Class(ClassName::new(
                "Magento\\Search\\Model\\Adminhtml\\System\\Config\\Source\\Engine"
            )))
        );
        assert_eq!(
            at("system.xml", system, "catalog/search/engine"),
            Some(Entity::ConfigPath("catalog/search/engine".to_string()))
        );
    }

    #[test]
    fn module_and_sequence() {
        let module = r#"<module name="Acme_Widget">
            <sequence><module name="Magento_Catalog"/></sequence>
        </module>"#;
        assert_eq!(
            at("module.xml", module, "Magento_Catalog"),
            Some(Entity::Module(ModuleName::new("Magento_Catalog")))
        );
    }

    #[test]
    fn php_tokens_and_strings() {
        let php = r#"<?php
namespace Acme\Widget\Observer;

use Magento\Framework\Event\ObserverInterface;
use Magento\Store\Model\ScopeInterface as Scope;

class Recalc implements ObserverInterface
{
    public function execute(\Magento\Framework\Event\Observer $observer)
    {
        $value = $this->config->getValue('web/secure/base_url');
        $this->eventManager->dispatch('acme_recalc_done', []);
    }
}
"#;
        // FQCN token.
        assert_eq!(
            at("Recalc.php", php, "\\Magento\\Framework\\Event\\Observer $"),
            Some(Entity::Class(ClassName::new("Magento\\Framework\\Event\\Observer")))
        );
        // use-imported bare identifier.
        assert_eq!(
            at("Recalc.php", php, "ObserverInterface\n{"),
            Some(Entity::Class(ClassName::new(
                "Magento\\Framework\\Event\\ObserverInterface"
            )))
        );
        // The declaration name resolves through the namespace.
        assert_eq!(
            at("Recalc.php", php, "class Recalc"),
            Some(Entity::Class(ClassName::new("Acme\\Widget\\Observer\\Recalc")))
        );
        // Strings: config path, and an event only because dispatch() precedes it.
        assert_eq!(
            at("Recalc.php", php, "web/secure/base_url"),
            Some(Entity::ConfigPath("web/secure/base_url".to_string()))
        );
        assert_eq!(
            at("Recalc.php", php, "acme_recalc_done"),
            Some(Entity::Event(EventName::new("acme_recalc_done")))
        );
    }

    #[test]
    fn php_plugin_method_declarations() {
        let php = r#"<?php
namespace Acme\Widget\Plugin;

class Tweak
{
    public function aroundSave($subject, callable $proceed)
    {
        return $proceed();
    }

    public function execute($input)
    {
        return $this->aroundSave($input, fn () => null);
    }
}
"#;
        // The declaration is a plugin method…
        assert_eq!(
            at("Tweak.php", php, "aroundSave($subject"),
            Some(Entity::PluginMethod("aroundSave".to_string()))
        );
        // …a *call* to it is not (only `function`-preceded names count)…
        assert_eq!(at("Tweak.php", php, "aroundSave($input"), None);
        // …and a non-interception declaration is a Method (the reverse lookup:
        // which plugins intercept it).
        assert_eq!(
            at("Tweak.php", php, "execute($input"),
            Some(Entity::Method("execute".to_string()))
        );
    }

    #[test]
    fn completion_contexts() {
        let ctx = |file: &str, text: &str, cursor_after: &str| {
            let offset = text.find(cursor_after).unwrap() + cursor_after.len();
            completion_context(file, text, offset)
        };

        // Unterminated attribute value mid-typing.
        let di = r#"<config>
    <preference for="Acme\Widget\Api\ThingInterface" type="Acme\Wid
</config>"#;
        let got = ctx("di.xml", di, "type=\"Acme\\Wid").unwrap();
        assert_eq!(got.kind, CompletionKind::Class);
        assert_eq!(got.typed, "Acme\\Wid");

        // Auto-paired quotes: cursor between them.
        let di = r#"<type name="Mag"></type>"#;
        let got = ctx("di.xml", di, "name=\"Mag").unwrap();
        assert_eq!(got.kind, CompletionKind::Class);
        assert_eq!(got.typed, "Mag");

        // Event name attribute.
        let events = r#"<event name="acme_">"#;
        let got = ctx("events.xml", events, "name=\"acme_").unwrap();
        assert_eq!(got.kind, CompletionKind::Event);
        assert_eq!(got.typed, "acme_");

        // Text node of a source_model.
        let system = r#"<field><source_model>Magento\Se</source_model></field>"#;
        let got = ctx("system.xml", system, "<source_model>Magento\\Se").unwrap();
        assert_eq!(got.kind, CompletionKind::Class);
        assert_eq!(got.typed, "Magento\\Se");

        // config_path text node.
        let system = r#"<field><config_path>catalog/se</config_path></field>"#;
        let got = ctx("system.xml", system, "<config_path>catalog/se").unwrap();
        assert_eq!(got.kind, CompletionKind::ConfigPath);
        assert_eq!(got.typed, "catalog/se");

        // Module in a sequence.
        let module = r#"<sequence><module name="Magento_Ca"/></sequence>"#;
        let got = ctx("module.xml", module, "name=\"Magento_Ca").unwrap();
        assert_eq!(got.kind, CompletionKind::Module);

        // PHP: config path behind getValue, event behind dispatch; a plain string is
        // no context.
        let php = r#"<?php
$v = $this->config->getValue('web/sec');
$this->events->dispatch('acme_th');
$x = 'not_a_context';
"#;
        let got = ctx("X.php", php, "getValue('web/sec").unwrap();
        assert_eq!(got.kind, CompletionKind::ConfigPath);
        assert_eq!(got.typed, "web/sec");
        let got = ctx("X.php", php, "dispatch('acme_th").unwrap();
        assert_eq!(got.kind, CompletionKind::Event);
        assert!(ctx("X.php", php, "'not_a").is_none());

        // referenceColumn completion is scoped by the same tag's referenceTable.
        let schema = r#"<constraint xsi:type="foreign" referenceTable="sales_order" referenceColumn="ent">"#;
        let got = ctx("db_schema.xml", schema, "referenceColumn=\"ent").unwrap();
        assert_eq!(got.kind, CompletionKind::Column("sales_order".to_string()));
        assert_eq!(got.typed, "ent");

        // Outside any value: no context.
        assert!(ctx("di.xml", "<config>  </config>", "<config> ").is_none());
    }

    #[test]
    fn route_entities() {
        let routes = r#"<router id="standard">
    <route id="checkout" frontName="checkout">
        <module name="Magento_Checkout"/>
    </route>
</router>"#;
        assert_eq!(
            at("routes.xml", routes, "\"checkout\" front"),
            Some(Entity::Route("checkout".to_string()))
        );
        assert_eq!(
            at("routes.xml", routes, "Name=\"checkout\""),
            Some(Entity::Route("checkout".to_string()))
        );
        // The router element's own id is not a route.
        assert_eq!(at("routes.xml", routes, "r id=\"standard"), None);
    }

    #[test]
    fn layout_entities() {
        let layout = r#"<page>
    <update handle="catalog_product_view"/>
    <referenceBlock name="product.info">
        <block class="Acme\Widget\Block\Chip" name="acme.chip" template="Acme_Widget::chip/render.phtml"/>
    </referenceBlock>
    <move element="acme.chip" destination="content"/>
</page>"#;
        assert_eq!(
            at("catalog_product_view.xml", layout, "Acme_Widget::chip/render.phtml"),
            Some(Entity::Template("Acme_Widget::chip/render.phtml".to_string()))
        );
        assert_eq!(
            at("catalog_product_view.xml", layout, "catalog_product_view"),
            Some(Entity::LayoutHandle("catalog_product_view".to_string()))
        );
        assert_eq!(
            at("catalog_product_view.xml", layout, "product.info"),
            Some(Entity::BlockName("product.info".to_string()))
        );
        assert_eq!(
            at("catalog_product_view.xml", layout, "element=\"acme.chip"),
            Some(Entity::BlockName("acme.chip".to_string()))
        );
        // The class attribute still classifies as a class inside layout XML.
        assert_eq!(
            at("catalog_product_view.xml", layout, "Acme\\Widget\\Block\\Chip"),
            Some(Entity::Class(ClassName::new("Acme\\Widget\\Block\\Chip")))
        );
        // Template shape wins over ACL shape only when the path half has . or /.
        let php = "<?php $b->getTemplate('Acme_Widget::chip/render.phtml');";
        assert_eq!(
            at("B.php", php, "Acme_Widget::chip/render.phtml"),
            Some(Entity::Template("Acme_Widget::chip/render.phtml".to_string()))
        );
    }

    #[test]
    fn graphqls_resolver() {
        let gql = r#"type Query {
    products(search: String): Products
        @resolver(class: "Magento\\CatalogGraphQl\\Model\\Resolver\\Products")
}"#;
        assert_eq!(
            at("schema.graphqls", gql, "CatalogGraphQl"),
            Some(Entity::Class(ClassName::new(
                "Magento\\CatalogGraphQl\\Model\\Resolver\\Products"
            )))
        );
    }
}
