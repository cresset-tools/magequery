//! The language features: definition, hover, references, code lens. Each is a thin
//! dispatch — extract the entity under the cursor ([`crate::entity`]), run the matching
//! core query, shape the answer as LSP types.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};

use lsp_types::{
    CodeLens, GotoDefinitionResponse, Hover, HoverContents, Location, MarkupContent,
    MarkupKind, Position, Url,
};
use magequery_core::model::ModuleSource;
use magequery_core::{Area, ClassName, ConfigSourceKind, EventName, Magento, Source};

use crate::entity::{completion_context, entity_at, CompletionKind, Entity, EntityAt};
use crate::textpos::LineIndex;

// ---- completion --------------------------------------------------------------------

/// Cap on returned items; `is_incomplete` makes the client re-query as typing narrows.
const COMPLETION_LIMIT: usize = 200;

/// Completions for the value under the cursor. `class_catalog` is the (expensive,
/// caller-cached) class enumeration; every other candidate set enumerates cheaply from
/// the handle's indexes per request.
pub(crate) fn completions(
    magento: &Magento,
    class_catalog: &[ClassName],
    path: &Path,
    position: Position,
) -> Option<lsp_types::CompletionList> {
    let text = magento.read_source(path).ok()?;
    let index = LineIndex::new(&text);
    let offset = index.offset(position)?;
    let ctx = completion_context(path.file_name()?.to_str()?, &text, offset)?;
    let range = index.range(ctx.span.clone());

    use lsp_types::CompletionItemKind as K;
    let mut candidates: Vec<(String, K, Option<&'static str>)> = Vec::new();
    match ctx.kind {
        CompletionKind::Class => {
            candidates.extend(
                class_catalog
                    .iter()
                    .map(|class| (class.as_str().to_string(), K::CLASS, None)),
            );
            candidates.extend(
                magento
                    .virtual_type_names()
                    .into_iter()
                    .map(|name| (name.as_str().to_string(), K::REFERENCE, Some("virtual type"))),
            );
        }
        CompletionKind::Event => {
            let mut names = std::collections::BTreeSet::new();
            for area in Area::ALL {
                for (event, _) in magento.events(area) {
                    names.insert(event.as_str().to_string());
                }
            }
            candidates.extend(names.into_iter().map(|name| (name, K::EVENT, None)));
        }
        CompletionKind::ConfigPath => {
            let mut paths = std::collections::BTreeSet::new();
            for field in magento.system_config(None) {
                paths.insert(field.path);
            }
            candidates.extend(paths.into_iter().map(|path| (path, K::VALUE, None)));
        }
        CompletionKind::Acl => {
            candidates.extend(
                magento
                    .acl(None)
                    .into_iter()
                    .map(|resource| (resource.id, K::CONSTANT, None)),
            );
        }
        CompletionKind::Module => {
            candidates.extend(
                magento
                    .modules()
                    .iter()
                    .map(|module| (module.name.as_str().to_string(), K::MODULE, None)),
            );
        }
        CompletionKind::Table => {
            candidates.extend(
                magento
                    .schema(None)
                    .into_iter()
                    .map(|table| (table.name, K::STRUCT, None)),
            );
        }
        CompletionKind::Template => {
            let area = crate::layout::area_of_file(magento, path);
            candidates.extend(
                magento
                    .templates(area, None)
                    .into_iter()
                    .map(|t| (t.reference, K::FILE, None)),
            );
        }
        CompletionKind::LayoutHandle => {
            let area = crate::layout::area_of_file(magento, path);
            candidates.extend(
                magento
                    .layout_handles(area)
                    .into_iter()
                    .map(|(handle, _)| (handle, K::REFERENCE, None)),
            );
        }
        CompletionKind::Column(table) => {
            if let Some(table) = magento.table(&table) {
                candidates.extend(
                    table
                        .columns
                        .into_iter()
                        .map(|column| (column.name, K::PROPERTY, None)),
                );
            }
        }
        CompletionKind::BlockName => {
            let area = crate::layout::area_of_file(magento, path);
            let mut names = std::collections::BTreeSet::new();
            for (_, op) in crate::layout::ops_where(magento, area, crate::layout::is_declaration) {
                names.insert(op.name);
            }
            candidates.extend(names.into_iter().map(|n| (n, K::FIELD, None)));
        }
    }

    // Rank: whole-value prefix, then short-name prefix, then substring; all
    // case-insensitive. An empty prefix offers everything (capped).
    let typed = ctx.typed.to_lowercase();
    let mut scored: Vec<(u8, String, K, Option<&'static str>)> = candidates
        .into_iter()
        .filter_map(|(label, kind, detail)| {
            let lower = label.to_lowercase();
            let rank = if typed.is_empty() {
                3
            } else if lower.starts_with(&typed) {
                // Prefer completions that continue at a segment boundary: typing
                // `Magento\Quote` should list `Magento\Quote\…` before
                // `Magento\QuoteGraphQl\…` (whose `G` would byte-sort first).
                let boundary = lower.len() == typed.len()
                    || typed.ends_with('\\')
                    || lower.as_bytes().get(typed.len()) == Some(&b'\\');
                if boundary {
                    0
                } else {
                    1
                }
            } else if lower.rsplit('\\').next().is_some_and(|short| short.starts_with(&typed)) {
                2
            } else if lower.contains(&typed) {
                3
            } else {
                return None;
            };
            Some((rank, label, kind, detail))
        })
        .collect();
    scored.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let total = scored.len();

    let items: Vec<lsp_types::CompletionItem> = scored
        .into_iter()
        .take(COMPLETION_LIMIT)
        .enumerate()
        .map(|(rank, (_, label, kind, detail))| lsp_types::CompletionItem {
            label: label.clone(),
            kind: Some(kind),
            detail: detail.map(str::to_string),
            filter_text: Some(label.clone()),
            sort_text: Some(format!("{rank:04}")),
            text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                range,
                new_text: label,
            })),
            ..Default::default()
        })
        .collect();
    Some(lsp_types::CompletionList {
        is_incomplete: total > COMPLETION_LIMIT,
        items,
    })
}

/// Source read + entity extraction shared by every position-based handler. Reads go
/// through the handle, so unsaved buffer contents (the overlay) are what get analyzed.
fn entity_under_cursor(
    magento: &Magento,
    path: &Path,
    position: Position,
) -> Option<(String, EntityAt)> {
    let text = magento.read_source(path).ok()?;
    let offset = LineIndex::new(&text).offset(position)?;
    let found = entity_at(path.file_name()?.to_str()?, &text, offset)?;
    Some((text, found))
}

// ---- definition --------------------------------------------------------------------

pub(crate) fn definition(
    magento: &Magento,
    path: &Path,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let (_, found) = entity_under_cursor(magento, path, position)?;
    let mut locations = match found.entity {
        Entity::Class(class) => class_locations(magento, &class),
        Entity::Event(event) => observer_locations(magento, &event),
        Entity::ConfigPath(config_path) => config_locations(magento, &config_path),
        Entity::Acl(id) => magento
            .acl_resource(&id)
            .map(|resource| source_location(magento, &resource.source))
            .into_iter()
            .collect(),
        Entity::Module(name) => magento
            .modules()
            .iter()
            .find(|module| module.name == name)
            .map(|module| file_location(module.path.join("etc/module.xml"), None))
            .into_iter()
            .collect(),
        Entity::PluginMethod(method) => {
            // Forward jump to the intercepted implementation; a Magento model's own
            // beforeSave/afterSave is interception-*shaped* but not a plugin — fall
            // back to the reverse lookup (who intercepts it) for those.
            let forward = plugin_method_locations(magento, path, &method);
            if forward.is_empty() {
                interceptor_locations(magento, path, &method)
            } else {
                forward
            }
        }
        Entity::Method(method) => interceptor_locations(magento, path, &method),
        Entity::Template(reference) => {
            let area = crate::layout::area_of_file(magento, path);
            let reference = crate::layout::normalize_ref(magento, path, &reference);
            crate::layout::resolve_template(magento, &reference, area)
                .into_iter()
                .map(|(_, file)| file_location(file, None))
                .collect()
        }
        Entity::LayoutHandle(handle) => {
            let area = crate::layout::area_of_file(magento, path);
            magento
                .layout(&handle, area)
                .map(|view| {
                    view.contributions
                        .into_iter()
                        .map(|c| file_location(c.file, None))
                        .collect()
                })
                .unwrap_or_default()
        }
        Entity::BlockName(name) => {
            let area = crate::layout::area_of_file(magento, path);
            crate::layout::ops_where(magento, area, |op| {
                crate::layout::is_declaration(op) && op.name == name
            })
            .into_iter()
            .map(|(_, op)| source_location(magento, &op.source))
            .collect()
        }
        Entity::Table(name) => magento
            .table(&name)
            .map(|table| source_location(magento, &table.source))
            .into_iter()
            .collect(),
        Entity::Route(needle) => route_locations(magento, path, &needle),
    };
    match locations.len() {
        0 => None,
        1 => Some(GotoDefinitionResponse::Scalar(locations.pop()?)),
        _ => Some(GotoDefinitionResponse::Array(locations)),
    }
}

/// The class's file — and, when a preference redirects it, the concrete class Magento
/// actually instantiates (the answer you'd miss reading the file alone).
fn class_locations(magento: &Magento, class: &ClassName) -> Vec<Location> {
    let mut out = Vec::new();
    if let Some(file) = magento.class_file(class) {
        out.push(decl_location(magento, file, class));
    }
    if let Ok(preference) = magento.preference(class, Area::Global) {
        if preference.concrete != *class {
            if let Some(file) = magento.class_file(&preference.concrete) {
                out.push(decl_location(magento, file, &preference.concrete));
            }
        }
    }
    out
}

/// Point into the file at `class Foo` / `interface Foo` rather than line 0.
fn decl_location(magento: &Magento, file: PathBuf, class: &ClassName) -> Location {
    let short = class.as_str().rsplit('\\').next().unwrap_or(class.as_str());
    let span = magento
        .read_source(&file)
        .ok()
        .and_then(|text| find_decl_span(&text, short).map(|span| (text, span)));
    match span {
        Some((text, span)) => file_location_at(&file, &text, span),
        None => file_location(file, None),
    }
}

/// The byte span of `name` in the file's `class|interface|trait|enum <name>` header.
pub(crate) fn find_decl_span(text: &str, name: &str) -> Option<Range<usize>> {
    let mut search_from = 0;
    while let Some(relative) = text[search_from..].find(name) {
        let start = search_from + relative;
        search_from = start + 1;
        let before = text[..start].trim_end();
        let keyword = ["class", "interface", "trait", "enum"]
            .iter()
            .any(|kw| before.ends_with(kw));
        let boundary = text[start + name.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        if keyword && boundary {
            return Some(start..start + name.len());
        }
    }
    None
}

/// `aroundSave` → `save`: strip the interception prefix, lowercase the first letter.
fn intercepted_method(plugin_method: &str) -> Option<String> {
    for prefix in ["before", "around", "after"] {
        if let Some(rest) = plugin_method.strip_prefix(prefix) {
            let mut chars = rest.chars();
            if let Some(first) = chars.next().filter(|c| c.is_ascii_uppercase()) {
                return Some(format!("{}{}", first.to_ascii_lowercase(), chars.as_str()));
            }
        }
    }
    None
}

/// The jump from a plugin's `aroundSave` to the `save()` it intercepts: every type this
/// class is declared as a plugin on, preference-resolved to the concrete, then the
/// hierarchy walked nearest-first for the file that actually defines the method.
fn plugin_method_locations(magento: &Magento, path: &Path, method: &str) -> Vec<Location> {
    let Some(plugin_class) = class_of_file(magento, path) else {
        return Vec::new();
    };
    let Some(target_method) = intercepted_method(method) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for target in magento.plugin_targets(&plugin_class) {
        let concrete = magento
            .preference(&target.declared_on, Area::Global)
            .map(|preference| preference.concrete)
            .unwrap_or_else(|_| target.declared_on.clone());
        let chain = std::iter::once(concrete.clone()).chain(magento.ancestors(&concrete));
        for class in chain {
            let Some(file) = magento.class_file(&class) else { continue };
            let Ok(text) = magento.read_source(&file) else { continue };
            if let Some(span) = find_method_span(&text, &target_method) {
                out.push(file_location_at(&file, &text, span));
                break; // nearest definition in the hierarchy wins
            }
        }
    }
    dedup_locations(out)
}

/// The byte span of `name` in a `function <name>(` declaration. PHP method names are
/// case-insensitive, so the search is too (ASCII — method names always are).
fn find_method_span(text: &str, name: &str) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let needle = name.as_bytes();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            let end = i + needle.len();
            let prefix = text[..i].trim_end();
            let after_function = prefix.len() >= 8
                && prefix.as_bytes()[prefix.len() - 8..].eq_ignore_ascii_case(b"function")
                && !prefix.as_bytes().get(prefix.len().wrapping_sub(9)).is_some_and(|b| {
                    b.is_ascii_alphanumeric() || *b == b'_'
                });
            let boundary = !bytes
                .get(end)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
            let called = text[end..].trim_start().starts_with('(');
            if after_function && boundary && called {
                return Some(i..end);
            }
        }
        i += 1;
    }
    None
}

/// A plugin method intercepting the method under the cursor — the reverse direction.
struct Interceptor {
    kind: magequery_core::InterceptKind,
    plugin_method: String,
    plugin_name: String,
    class: ClassName,
    disabled: bool,
    /// The di.xml `<plugin>` declaration.
    source: Source,
}

/// Plugins whose `before*/around*/after*` methods target `method` on the class this
/// file declares. `plugins_all_areas` already resolves the preference and collects
/// declarations on ancestors/interfaces, so interface-declared plugins show up on the
/// concrete class's methods.
fn interceptors_of(magento: &Magento, path: &Path, method: &str) -> Vec<Interceptor> {
    let Some(class) = class_of_file(magento, path) else {
        return Vec::new();
    };
    let plugins = magento.plugins_all_areas(&class).unwrap_or_default();
    method_interceptors(&plugins, method)
}

/// The subset of `plugins` intercepting `method` (PHP method names are
/// case-insensitive). Split out so code lens can fetch the plugin set once per file.
fn method_interceptors(plugins: &[magequery_core::Plugin], method: &str) -> Vec<Interceptor> {
    let mut out = Vec::new();
    for plugin in plugins {
        for plugin_method in &plugin.methods {
            if plugin_method.target.eq_ignore_ascii_case(method) {
                out.push(Interceptor {
                    kind: plugin_method.kind,
                    plugin_method: plugin_method.plugin_method.clone(),
                    plugin_name: plugin.name.clone(),
                    class: plugin.class.clone(),
                    disabled: plugin.disabled,
                    source: plugin.source.clone(),
                });
            }
        }
    }
    out
}

/// Jump targets for the reverse direction: the plugin method's own declaration, falling
/// back to the di.xml `<plugin>` line when the plugin file can't be read.
fn interceptor_locations(magento: &Magento, path: &Path, method: &str) -> Vec<Location> {
    interceptor_sites(magento, interceptors_of(magento, path, method))
}

fn interceptor_sites(magento: &Magento, interceptors: Vec<Interceptor>) -> Vec<Location> {
    let locations = interceptors
        .into_iter()
        .map(|interceptor| {
            magento
                .class_file(&interceptor.class)
                .and_then(|file| {
                    let text = magento.read_source(&file).ok()?;
                    let span = find_method_span(&text, &interceptor.plugin_method)?;
                    Some(file_location_at(&file, &text, span))
                })
                .unwrap_or_else(|| source_location(magento, &interceptor.source))
        })
        .collect();
    dedup_locations(locations)
}

/// Hover for a method that plugins intercept: who fires around it, in execution order.
fn method_hover(magento: &Magento, path: &Path, method: &str) -> Option<String> {
    let interceptors = interceptors_of(magento, path, method);
    if interceptors.is_empty() {
        return None;
    }
    let mut md = format!(
        "**`{method}()`** — intercepted by {} plugin method(s)\n",
        interceptors.len()
    );
    for interceptor in interceptors.iter().take(12) {
        let _ = write!(
            md,
            "\n- {} `{}` — `{}` (plugin `{}`{})",
            interceptor.kind,
            interceptor.plugin_method,
            interceptor.class,
            interceptor.plugin_name,
            if interceptor.disabled { ", disabled" } else { "" },
        );
    }
    Some(md)
}

/// Where a template resolves (module file + theme overrides) and how many layout ops
/// reference it.
fn template_hover(magento: &Magento, path: &Path, reference: &str) -> Option<String> {
    let area = crate::layout::area_of_file(magento, path);
    let reference = &crate::layout::normalize_ref(magento, path, reference);
    let resolved = crate::layout::resolve_template(magento, reference, area);
    let uses = magento
        .template(reference, area)
        .map(|t| t.usages.len())
        .unwrap_or(0);
    let mut md = format!("**template `{reference}`**");
    if resolved.is_empty() {
        let _ = write!(md, "

no file found (module missing, or path typo)");
    } else {
        for (provider, file) in &resolved {
            let _ = write!(md, "
- {provider}: `{}`", file.display());
        }
        if resolved.len() > 1 {
            let _ = write!(
                md,
                "

which override applies depends on the active theme"
            );
        }
    }
    if uses > 0 {
        let _ = write!(md, "

referenced by {uses} layout op(s)");
    }
    Some(md)
}

fn handle_hover(magento: &Magento, path: &Path, handle: &str) -> Option<String> {
    let area = crate::layout::area_of_file(magento, path);
    let view = magento.layout(handle, area)?;
    let mut md = format!(
        "**layout handle `{handle}`** — {} contributing file(s)",
        view.contributions.len()
    );
    if !view.includes.is_empty() {
        let _ = write!(md, "

includes: {}", view.includes.join(", "));
    }
    if !view.included_by.is_empty() {
        let _ = write!(md, "

included by: {}", view.included_by.join(", "));
    }
    Some(md)
}

fn block_hover(magento: &Magento, path: &Path, name: &str) -> Option<String> {
    let area = crate::layout::area_of_file(magento, path);
    let declarations = crate::layout::ops_where(magento, area, |op| {
        crate::layout::is_declaration(op) && op.name == name
    });
    let references = crate::layout::ops_where(magento, area, |op| {
        !crate::layout::is_declaration(op)
            && (op.name == name || op.parent.as_deref() == Some(name))
    })
    .len();
    let (handle, op) = declarations.first()?;
    let mut md = format!("**block `{name}`** — declared in handle `{handle}`");
    if let Some(class) = &op.class {
        let _ = write!(md, "

class `{class}`");
    }
    if let Some(template) = &op.template {
        let _ = write!(md, "

template `{template}`");
    }
    let _ = write!(md, "

{} ({})", op.source.module, op.source.location());
    if references > 0 {
        let _ = write!(md, "

referenced/modified by {references} layout op(s)");
    }
    Some(md)
}

/// routes.xml lives under `etc/<area>/`; the file path decides which router set to ask.
fn route_area(path: &Path) -> Area {
    if path.to_string_lossy().contains("/etc/adminhtml/") {
        Area::Adminhtml
    } else {
        Area::Frontend
    }
}

fn route_locations(magento: &Magento, path: &Path, needle: &str) -> Vec<Location> {
    magento
        .routes(route_area(path))
        .into_iter()
        .filter(|route| route.id == needle || route.front_name == needle)
        .map(|route| source_location(magento, &route.source))
        .collect()
}

fn route_hover(magento: &Magento, path: &Path, needle: &str) -> Option<String> {
    let route = magento
        .routes(route_area(path))
        .into_iter()
        .find(|route| route.id == needle || route.front_name == needle)?;
    let mut md = format!(
        "**route `{}`** — router `{}`, frontName `{}`",
        route.id, route.router, route.front_name
    );
    let modules = route
        .modules
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if !modules.is_empty() {
        let _ = write!(md, "

handled by: {modules}");
    }
    Some(md)
}

fn table_hover(magento: &Magento, name: &str) -> Option<String> {
    let table = magento.table(name)?;
    let mut md = format!(
        "**table `{}`** — {} column(s) · {}",
        table.name,
        table.columns.len(),
        table.source.module,
    );
    let mut foreign: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| c.source.module != table.source.module)
        .map(|c| c.source.module.as_str())
        .collect();
    foreign.sort_unstable();
    foreign.dedup();
    if !foreign.is_empty() {
        let _ = write!(md, "

columns also added by: {}", foreign.join(", "));
    }
    Some(md)
}

/// events.xml declarations registering `class` as an observer, across all areas.
fn observer_registrations(magento: &Magento, class: &ClassName) -> Vec<(String, Location)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for area in Area::ALL {
        for (event, _) in magento.events(area) {
            for observer in magento.observers(&event, area) {
                if observer.instance == *class
                    && seen.insert((observer.source.file.clone(), observer.source.line))
                {
                    out.push((
                        event.as_str().to_string(),
                        source_location(magento, &observer.source),
                    ));
                }
            }
        }
    }
    out
}

fn observer_locations(magento: &Magento, event: &EventName) -> Vec<Location> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for area in Area::ALL {
        for observer in magento.observers(event, area) {
            if seen.insert((observer.source.file.clone(), observer.source.line)) {
                out.push(source_location(magento, &observer.source));
            }
        }
    }
    out
}

/// Where a config path lives: its system.xml field declaration plus every static source
/// that sets a value for it.
fn config_locations(magento: &Magento, config_path: &str) -> Vec<Location> {
    let mut out = Vec::new();
    for field in magento.system_config(Some(config_path)) {
        if field.path == config_path {
            out.push(source_location(magento, &field.source));
        }
    }
    if let Ok(set) = magento.config(false) {
        for value in set.scopes_for(config_path) {
            if let Some(file) = &value.file {
                out.push(file_location(
                    absolute(magento, file.clone()),
                    (value.line > 0).then(|| value.line - 1),
                ));
            }
        }
    }
    out
}

// ---- hover -------------------------------------------------------------------------

pub(crate) fn hover(magento: &Magento, path: &Path, position: Position) -> Option<Hover> {
    let (text, found) = entity_under_cursor(magento, path, position)?;
    let range = LineIndex::new(&text).range(found.span.clone());
    let markdown = match &found.entity {
        Entity::Class(class) => class_hover(magento, class),
        Entity::Event(event) => event_hover(magento, event),
        Entity::ConfigPath(config_path) => config_hover(magento, config_path),
        Entity::Acl(id) => acl_hover(magento, id),
        Entity::Module(name) => module_hover(magento, name),
        Entity::PluginMethod(method) => plugin_method_hover(magento, path, method)
            .or_else(|| method_hover(magento, path, method)),
        Entity::Method(method) => method_hover(magento, path, method),
        Entity::Template(reference) => template_hover(magento, path, reference),
        Entity::LayoutHandle(handle) => handle_hover(magento, path, handle),
        Entity::BlockName(name) => block_hover(magento, path, name),
        Entity::Table(name) => table_hover(magento, name),
        Entity::Route(needle) => route_hover(magento, path, needle),
    }?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(range),
    })
}

/// A compressed `whatis`: identity, DI resolution, and what the config wires it into.
fn class_hover(magento: &Magento, class: &ClassName) -> Option<String> {
    let w = magento.whatis(class).ok()?;
    let mut md = String::new();

    let kind = if w.is_virtual_type {
        "virtual type".to_string()
    } else {
        w.kind.clone().unwrap_or_else(|| "class".to_string())
    };
    let _ = write!(md, "**`{}`** — {kind}", w.class);
    if let Some(module) = &w.module {
        let _ = write!(md, " · {module}");
    }
    if let Some(package) = &w.package {
        let _ = write!(md, " · {package}");
        if let Some(version) = &w.package_version {
            let _ = write!(md, " {version}");
        }
    }

    if let Some(resolves_to) = &w.resolves_to {
        let _ = write!(md, "\n\npreference resolves it to `{resolves_to}`");
    }
    if let Some(instantiates) = &w.instantiates {
        let _ = write!(md, "\n\ninstantiates `{instantiates}`");
    }

    let mut di = Vec::new();
    if w.plugin_count > 0 {
        di.push(format!("{} plugin(s)", w.plugin_count));
    }
    if w.argument_count > 0 {
        di.push(format!("{} configured argument(s)", w.argument_count));
    }
    let wired = w.uses.preferred_for.len() + w.uses.virtual_types.len() + w.uses.injections.len();
    if wired > 0 {
        di.push(format!("wired into {wired} di.xml place(s)"));
    }
    if !di.is_empty() {
        let _ = write!(md, "\n\n{}", di.join(" · "));
    }

    let mut roles = Vec::new();
    for observer in w.observes.iter().take(3) {
        roles.push(format!("observes `{}`", observer.event));
    }
    for job in w.cron_jobs.iter().take(2) {
        roles.push(format!("cron job `{}`", job.name));
    }
    for route in w.webapi.iter().take(2) {
        roles.push(format!("serves `{} {}`", route.method, route.url));
    }
    if let Some(command) = &w.command {
        if let Some(name) = &command.name {
            roles.push(format!("`bin/magento {name}`"));
        }
    }
    for class_ref in w.graphql.iter().take(2).chain(w.mq.iter().take(2)) {
        roles.push(class_ref.role.clone());
    }
    for action in w.action_urls.iter().take(2) {
        roles.push(format!("controller for `{}`", action.url));
    }
    if !roles.is_empty() {
        let _ = write!(md, "\n\n{}", roles.join("  \n"));
    }
    Some(md)
}

fn event_hover(magento: &Magento, event: &EventName) -> Option<String> {
    let mut seen = HashSet::new();
    let mut observers = Vec::new();
    for area in Area::ALL {
        for observer in magento.observers(event, area) {
            if seen.insert((observer.source.file.clone(), observer.source.line)) {
                observers.push(observer);
            }
        }
    }
    if observers.is_empty() {
        return Some(format!("**event `{event}`** — no observers registered"));
    }
    let mut md = format!("**event `{event}`** — {} observer(s)\n", observers.len());
    for observer in observers.iter().take(12) {
        let _ = write!(
            md,
            "\n- `{}` → `{}` ({}){}",
            observer.name,
            observer.instance,
            observer.source.module,
            if observer.disabled { " — disabled" } else { "" },
        );
    }
    Some(md)
}

fn config_hover(magento: &Magento, config_path: &str) -> Option<String> {
    let field = magento
        .system_config(Some(config_path))
        .into_iter()
        .find(|field| field.path == config_path);
    let values = magento
        .config(false)
        .map(|set| set.scopes_for(config_path))
        .unwrap_or_default();
    if field.is_none() && values.is_empty() {
        return None;
    }

    let mut md = format!("**config `{config_path}`**");
    if let Some(field) = &field {
        let breadcrumb = [field.tab.as_deref(), Some(field.section.as_str()), Some(field.group.as_str()), Some(field.label.as_str())]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" → ");
        let _ = write!(md, "\n\n{breadcrumb}");
        if !field.scopes.is_empty() {
            let _ = write!(md, " · [{}]", field.scopes.join(", "));
        }
    }
    if !values.is_empty() {
        md.push('\n');
        for value in values.iter().take(10) {
            let source = match value.source {
                ConfigSourceKind::ConfigXml => "config.xml",
                ConfigSourceKind::ConfigPhp => "config.php",
                ConfigSourceKind::EnvPhp => "env.php",
                ConfigSourceKind::EnvVar => "env var",
                ConfigSourceKind::Database => "db",
            };
            let _ = write!(md, "\n- `{}` = `{}` ({source})", value.scope, value.value);
        }
    } else {
        let _ = write!(md, "\n\nno static value set (database values need the CLI: `magequery config {config_path} --db`)");
    }
    Some(md)
}

fn acl_hover(magento: &Magento, id: &str) -> Option<String> {
    let resource = magento.acl_resource(id)?;
    let breadcrumb = magento
        .acl_ancestors(id)
        .iter()
        .map(|ancestor| ancestor.title.clone())
        .filter(|title| !title.is_empty())
        .collect::<Vec<_>>()
        .join(" → ");
    let mut md = format!("**ACL `{id}`**");
    if !resource.title.is_empty() {
        let _ = write!(md, " — {}", resource.title);
    }
    if !breadcrumb.is_empty() {
        let _ = write!(md, "\n\n{breadcrumb}");
    }
    if !resource.children.is_empty() {
        let _ = write!(md, "\n\ngrants {} sub-resource(s)", resource.children.len());
    }
    Some(md)
}

/// What this interception method fires on: each target type, the concrete it resolves
/// to, and the plugin declaration behind it. `None` when no di.xml declares the class
/// as a plugin — the caller falls back to the reverse (who-intercepts-me) hover, which
/// covers a model's own interception-shaped `beforeSave`.
fn plugin_method_hover(magento: &Magento, path: &Path, method: &str) -> Option<String> {
    let plugin_class = class_of_file(magento, path)?;
    let target_method = intercepted_method(method)?;
    let targets = magento.plugin_targets(&plugin_class);
    if targets.is_empty() {
        return None;
    }
    let mut md = format!("**`{method}`** — intercepts `{target_method}()` on\n");
    for target in targets.iter().take(8) {
        let _ = write!(md, "\n- `{}`", target.declared_on);
        if let Ok(preference) = magento.preference(&target.declared_on, Area::Global) {
            if preference.concrete != target.declared_on {
                let _ = write!(md, " → `{}`", preference.concrete);
            }
        }
        let _ = write!(md, " (plugin `{}`", target.plugin_name);
        if target.disabled {
            let _ = write!(md, ", disabled");
        }
        let _ = write!(md, ")");
    }
    Some(md)
}

fn module_hover(magento: &Magento, name: &magequery_core::ModuleName) -> Option<String> {
    let module = magento.modules().iter().find(|module| module.name == *name)?;
    let source = match module.source {
        ModuleSource::App => "app/code",
        ModuleSource::Vendor => "vendor",
        _ => "setup",
    };
    let mut md = format!(
        "**module {}** — {source}, {}",
        module.name,
        if module.enabled { "enabled" } else { "disabled" },
    );
    if !module.sequence.is_empty() {
        let sequence = module
            .sequence
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(md, "\n\nloads after: {sequence}");
    }
    Some(md)
}

// ---- references --------------------------------------------------------------------

pub(crate) fn references(
    magento: &Magento,
    path: &Path,
    position: Position,
) -> Option<Vec<Location>> {
    let (_, found) = entity_under_cursor(magento, path, position)?;
    let locations = match found.entity {
        Entity::Class(class) => class_references(magento, &class),
        Entity::Event(event) => observer_locations(magento, &event),
        Entity::ConfigPath(config_path) => config_locations(magento, &config_path),
        Entity::Acl(id) => acl_references(magento, &id),
        Entity::Module(name) => match magento.deps(&name) {
            Ok(deps) => deps
                .depended_on_by
                .iter()
                .map(|edge| source_location(magento, &edge.source))
                .collect(),
            Err(_) => Vec::new(),
        },
        // The di.xml `<plugin>` declarations wiring this method's class in — or, when
        // there are none, the plugin methods intercepting a method of this name.
        Entity::PluginMethod(method) => {
            let declarations: Vec<Location> = match class_of_file(magento, path) {
                Some(plugin_class) => magento
                    .plugin_targets(&plugin_class)
                    .iter()
                    .map(|target| source_location(magento, &target.source))
                    .collect(),
                None => Vec::new(),
            };
            if declarations.is_empty() {
                interceptor_locations(magento, path, &method)
            } else {
                declarations
            }
        }
        // Everything wired around this method: the intercepting plugin methods plus
        // their di.xml declarations.
        Entity::Method(method) => {
            let mut locations = interceptor_locations(magento, path, &method);
            for interceptor in interceptors_of(magento, path, &method) {
                locations.push(source_location(magento, &interceptor.source));
            }
            locations
        }
        Entity::Template(reference) => {
            let area = crate::layout::area_of_file(magento, path);
            let reference = crate::layout::normalize_ref(magento, path, &reference);
            magento
                .template(&reference, area)
                .map(|t| {
                    t.usages
                        .iter()
                        .map(|u| source_location(magento, &u.source))
                        .collect()
                })
                .unwrap_or_default()
        }
        Entity::LayoutHandle(handle) => {
            let area = crate::layout::area_of_file(magento, path);
            crate::layout::ops_where(magento, area, |op| {
                op.kind == magequery_core::LayoutOpKind::Update && op.name == handle
            })
            .into_iter()
            .map(|(_, op)| source_location(magento, &op.source))
            .collect()
        }
        Entity::BlockName(name) => {
            let area = crate::layout::area_of_file(magento, path);
            crate::layout::ops_where(magento, area, |op| {
                op.name == name || op.parent.as_deref() == Some(name.as_str())
            })
            .into_iter()
            .map(|(_, op)| source_location(magento, &op.source))
            .collect()
        }
        Entity::Route(needle) => route_locations(magento, path, &needle),
        // Every module contributing columns/constraints to the table.
        Entity::Table(name) => magento
            .table(&name)
            .map(|table| {
                let mut sites = vec![source_location(magento, &table.source)];
                sites.extend(table.columns.iter().map(|c| source_location(magento, &c.source)));
                sites
            })
            .unwrap_or_default(),
    };
    let deduped = dedup_locations(locations);
    (!deduped.is_empty()).then_some(deduped)
}

/// Every configuration reference to a class: reverse DI (preferences to it, virtual
/// types on it, constructor injections) plus the whatis sweep (observer registrations,
/// cron jobs, webapi routes, console command, GraphQL resolvers, queue handlers) and
/// plugins declared directly on it.
pub(crate) fn class_references(magento: &Magento, class: &ClassName) -> Vec<Location> {
    let mut out = Vec::new();
    if let Ok(uses) = magento.uses(class, None) {
        for use_ref in uses.preferred_for.iter().chain(uses.virtual_types.iter()) {
            out.push(source_location(magento, &use_ref.source));
        }
        for injection in &uses.injections {
            out.push(source_location(magento, &injection.source));
        }
    }
    if let Ok(w) = magento.whatis(class) {
        for observer in &w.observes {
            out.push(source_location(magento, &observer.source));
        }
        for job in &w.cron_jobs {
            out.push(source_location(magento, &job.source));
        }
        for route in &w.webapi {
            out.push(source_location(magento, &route.source));
        }
        if let Some(command) = &w.command {
            out.push(source_location(magento, &command.source));
        }
        for class_ref in w.graphql.iter().chain(w.mq.iter()) {
            out.push(source_location(magento, &class_ref.source));
        }
    }
    if let Ok(plugins) = magento.plugins_all_areas(class) {
        for plugin in plugins {
            if plugin.declared_on == *class {
                out.push(source_location(magento, &plugin.source));
            }
        }
    }
    out
}

fn acl_references(magento: &Magento, id: &str) -> Vec<Location> {
    let mut out = Vec::new();
    for route in magento.webapi(None) {
        if route.resources.iter().any(|resource| resource == id) {
            out.push(source_location(magento, &route.source));
        }
    }
    for item in magento.menu(None) {
        if item.resource.as_deref() == Some(id) {
            out.push(source_location(magento, &item.source));
        }
    }
    out
}

fn dedup_locations(locations: Vec<Location>) -> Vec<Location> {
    let mut seen = HashSet::new();
    locations
        .into_iter()
        .filter(|location| {
            seen.insert((location.uri.to_string(), location.range.start.line))
        })
        .collect()
}

// ---- code lens ---------------------------------------------------------------------

/// On a PHP class declaration: how the DI config sees this class. Lenses carry a
/// `magequery.showReferences` command; the VS Code extension maps it onto the editor's
/// peek view (clients without the command show inert text, which is still the fact).
pub(crate) fn code_lens(magento: &Magento, path: &Path) -> Option<Vec<CodeLens>> {
    if path.extension()?.to_str() == Some("phtml") {
        return phtml_lenses(magento, path);
    }
    if path.extension()?.to_str() != Some("php") {
        return None;
    }
    let class = class_of_file(magento, path)?;
    let text = magento.read_source(path).ok()?;
    let index = LineIndex::new(&text);
    let short = class.as_str().rsplit('\\').next().unwrap_or(class.as_str());
    let range = index.range(find_decl_span(&text, short)?);

    // Fetched once per file: the plugins firing on this class (feeds the class lens and
    // every intercepted-method lens) and the types this class is a plugin on.
    let plugins = magento.plugins_all_areas(&class).unwrap_or_default();
    let plugin_targets = magento.plugin_targets(&class);

    let mut lenses = Vec::new();
    let active: Vec<_> = plugins.iter().filter(|plugin| !plugin.disabled).collect();
    if !active.is_empty() {
        let inherited = active
            .iter()
            .filter(|plugin| plugin.declared_on != class)
            .count();
        let title = if inherited > 0 {
            format!("{} plugin(s), {inherited} via ancestors", active.len())
        } else {
            format!("{} plugin(s)", active.len())
        };
        let locations: Vec<Location> = active
            .iter()
            .map(|plugin| source_location(magento, &plugin.source))
            .collect();
        lenses.push(lens(path, range, title, locations));
    }
    let references = dedup_locations(class_references(magento, &class));
    if !references.is_empty() {
        lenses.push(lens(
            path,
            range,
            format!("wired in {} config place(s)", references.len()),
            references,
        ));
    }

    // Per-method lenses. In a plugin class: what each interception method fires on. In
    // any class with plugins: who intercepts each targeted method.
    for (name, span) in method_decl_spans(&text) {
        let method_range = index.range(span);
        if !plugin_targets.is_empty() && intercepted_method(&name).is_some() {
            let locations = plugin_method_locations(magento, path, &name);
            if locations.is_empty() {
                continue;
            }
            let target = intercepted_method(&name).expect("checked above");
            let title = if let [only] = plugin_targets.as_slice() {
                let concrete = magento
                    .preference(&only.declared_on, Area::Global)
                    .map(|preference| preference.concrete)
                    .unwrap_or_else(|_| only.declared_on.clone());
                let short = concrete.as_str().rsplit('\\').next().unwrap_or(concrete.as_str());
                format!("intercepts {short}::{target}()")
            } else {
                format!("intercepts {target}() on {} types", plugin_targets.len())
            };
            lenses.push(lens(path, method_range, title, locations));
            continue;
        }
        if name == "execute" {
            let registrations = observer_registrations(magento, &class);
            if !registrations.is_empty() {
                let title = match registrations.as_slice() {
                    [(event, _)] => format!("→ {event}"),
                    many => format!("observes {} event(s)", many.len()),
                };
                let locations = registrations.into_iter().map(|(_, l)| l).collect();
                lenses.push(lens(path, method_range, title, locations));
            }
        }
        let interceptors = method_interceptors(&plugins, &name);
        if !interceptors.is_empty() {
            let title = format!("intercepted by {} plugin method(s)", interceptors.len());
            let locations = interceptor_sites(magento, interceptors);
            lenses.push(lens(path, method_range, title, locations));
        }
    }
    (!lenses.is_empty()).then_some(lenses)
}

/// Every named `function <name>(` declaration in the file, in order. Closures (no name)
/// and arrow functions never match; a mention inside a comment is the accepted noise.
fn method_decl_spans(text: &str) -> Vec<(String, Range<usize>)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(relative) = text[from..].find("function") {
        let keyword = from + relative;
        from = keyword + "function".len();
        let bounded = keyword == 0
            || !(bytes[keyword - 1].is_ascii_alphanumeric() || bytes[keyword - 1] == b'_');
        if !bounded {
            continue;
        }
        let mut i = from;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'&' {
            i += 1; // by-ref return
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        let name_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i == name_start {
            continue; // anonymous function
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if bytes.get(j) == Some(&b'(') {
            out.push((text[name_start..i].to_string(), name_start..i));
        }
    }
    out
}

/// The lens facts as inline annotations, for editors that render inlay hints but not
/// code lenses (Zed): `« N plugins »` at the end of each intercepted method's signature
/// line, `→ Save::execute()` on a plugin's interception methods. The tooltip carries
/// the hover-style breakdown; the label links to the first location on the other side.
pub(crate) fn inlay_hints(
    magento: &Magento,
    path: &Path,
    range: lsp_types::Range,
) -> Option<Vec<lsp_types::InlayHint>> {
    if path.extension()?.to_str() != Some("php") {
        return None;
    }
    let class = class_of_file(magento, path)?;
    let text = magento.read_source(path).ok()?;
    let index = LineIndex::new(&text);
    let plugins = magento.plugins_all_areas(&class).unwrap_or_default();
    let plugin_targets = magento.plugin_targets(&class);
    if plugins.is_empty() && plugin_targets.is_empty() {
        return None;
    }

    let mut hints = Vec::new();
    for (name, span) in method_decl_spans(&text) {
        // Anchor at the end of the signature's first line.
        let eol = text[span.end..]
            .find('\n')
            .map(|i| span.end + i)
            .unwrap_or(text.len());
        let position = index.position(eol);
        if position.line < range.start.line || position.line > range.end.line {
            continue;
        }

        if !plugin_targets.is_empty() && intercepted_method(&name).is_some() {
            let locations = plugin_method_locations(magento, path, &name);
            let Some(first) = locations.first() else { continue };
            let target = intercepted_method(&name).expect("checked above");
            let label = if let [only] = plugin_targets.as_slice() {
                let concrete = magento
                    .preference(&only.declared_on, Area::Global)
                    .map(|preference| preference.concrete)
                    .unwrap_or_else(|_| only.declared_on.clone());
                let short = concrete.as_str().rsplit('\\').next().unwrap_or(concrete.as_str());
                format!("→ {short}::{target}()")
            } else {
                format!("→ {target}() on {} types", plugin_targets.len())
            };
            hints.push(hint(
                position,
                label,
                plugin_method_hover(magento, path, &name),
                Some(first.clone()),
            ));
            continue;
        }

        let interceptors = method_interceptors(&plugins, &name);
        if !interceptors.is_empty() {
            let label = format!("« {} plugin(s) »", interceptors.len());
            let tooltip = method_hover(magento, path, &name);
            let first = interceptor_sites(magento, interceptors).into_iter().next();
            hints.push(hint(position, label, tooltip, first));
        }
    }
    (!hints.is_empty()).then_some(hints)
}

fn hint(
    position: Position,
    label: String,
    tooltip: Option<String>,
    location: Option<Location>,
) -> lsp_types::InlayHint {
    lsp_types::InlayHint {
        position,
        label: lsp_types::InlayHintLabel::LabelParts(vec![lsp_types::InlayHintLabelPart {
            value: label,
            tooltip: tooltip.map(|markdown| {
                lsp_types::InlayHintLabelPartTooltip::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: markdown,
                })
            }),
            location,
            command: None,
        }]),
        kind: None,
        text_edits: None,
        tooltip: None,
        padding_left: Some(true),
        padding_right: Some(false),
        data: None,
    }
}

/// Lenses at the top of a `.phtml` file: the override chain and layout usage.
fn phtml_lenses(magento: &Magento, path: &Path) -> Option<Vec<CodeLens>> {
    let (reference, area, theme) = crate::layout::template_ref_of_file(magento, path)?;
    let top = lsp_types::Range::new(Position::new(0, 0), Position::new(0, 0));
    let mut lenses = Vec::new();

    let resolved = crate::layout::resolve_template(magento, &reference, area);
    match &theme {
        Some(_) => {
            // A theme override: link back to the module's original.
            let originals: Vec<Location> = resolved
                .iter()
                .filter(|(provider, _)| provider.starts_with("module"))
                .map(|(_, file)| file_location(file.clone(), None))
                .collect();
            if !originals.is_empty() {
                lenses.push(lens(path, top, format!("overrides {reference}"), originals));
            }
        }
        None => {
            let overrides: Vec<Location> = resolved
                .iter()
                .filter(|(provider, _)| provider.starts_with("theme"))
                .map(|(_, file)| file_location(file.clone(), None))
                .collect();
            if !overrides.is_empty() {
                lenses.push(lens(
                    path,
                    top,
                    format!("overridden in {} theme(s)", overrides.len()),
                    overrides,
                ));
            }
        }
    }

    let uses: Vec<Location> = magento
        .template(&reference, area)
        .map(|t| {
            t.usages
                .iter()
                .map(|u| source_location(magento, &u.source))
                .collect()
        })
        .unwrap_or_default();
    if !uses.is_empty() {
        lenses.push(lens(
            path,
            top,
            format!("used in {} layout op(s)", uses.len()),
            dedup_locations(uses),
        ));
    }
    (!lenses.is_empty()).then_some(lenses)
}

fn lens(path: &Path, range: lsp_types::Range, title: String, locations: Vec<Location>) -> CodeLens {
    let arguments = Url::from_file_path(path).ok().map(|uri| {
        vec![
            serde_json::json!(uri),
            serde_json::json!(range.start),
            serde_json::json!(locations),
        ]
    });
    CodeLens {
        range,
        command: Some(lsp_types::Command {
            title,
            command: "magequery.showReferences".to_string(),
            arguments,
        }),
        data: None,
    }
}

/// The class a PHP file declares, by the `Vendor_Module` → `Vendor\Module\` convention —
/// verified by resolving the candidate back through PSR-4 to the same file, so a module
/// whose namespace diverges from its name is skipped, never misreported (doctor's rule).
pub(crate) fn class_of_file(magento: &Magento, path: &Path) -> Option<ClassName> {
    let module = magento
        .modules()
        .iter()
        .filter(|module| path.starts_with(&module.path))
        .max_by_key(|module| module.path.as_os_str().len())?;
    let relative = path.strip_prefix(&module.path).ok()?.with_extension("");
    let mut name = module.name.as_str().replace('_', "\\");
    for component in relative.components() {
        name.push('\\');
        name.push_str(component.as_os_str().to_str()?);
    }
    let class = ClassName::new(name);
    let resolved = magento.class_file(&class)?;
    (resolved == path || resolved.canonicalize().ok() == path.canonicalize().ok())
        .then_some(class)
}

// ---- location plumbing ---------------------------------------------------------------

/// A `Source` (file + 1-based line) as an LSP location, root-joined when relative.
fn source_location(magento: &Magento, source: &Source) -> Location {
    file_location(
        absolute(magento, source.file.clone()),
        (source.line > 0).then(|| source.line - 1),
    )
}

fn absolute(magento: &Magento, file: PathBuf) -> PathBuf {
    if file.is_absolute() {
        file
    } else {
        magento.root().join(file)
    }
}

fn file_location(file: PathBuf, line: Option<u32>) -> Location {
    let position = Position::new(line.unwrap_or(0), 0);
    Location {
        uri: Url::from_file_path(&file)
            .unwrap_or_else(|_| Url::parse("file:///").expect("static uri")),
        range: lsp_types::Range::new(position, position),
    }
}

fn file_location_at(file: &Path, text: &str, span: Range<usize>) -> Location {
    Location {
        uri: Url::from_file_path(file)
            .unwrap_or_else(|_| Url::parse("file:///").expect("static uri")),
        range: LineIndex::new(text).range(span),
    }
}
