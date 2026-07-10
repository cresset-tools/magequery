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
