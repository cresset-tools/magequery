//! Document symbols (the outline/breadcrumb tree for config XML) and workspace symbol
//! search. The outline is a *generic* nested scan — any element with an identifying
//! attribute becomes a symbol — which fits Magento's config dialects uniformly without
//! per-file-type parsers.

use std::path::Path;

use lsp_types::{DocumentSymbol, Location, SymbolInformation, SymbolKind};
use magequery_core::{Area, ClassName, Magento};

use crate::textpos::LineIndex;

/// Attributes that identify an element well enough to be an outline entry.
const ID_ATTRS: [&str; 6] = ["name", "id", "for", "code", "handle", "instance"];

pub(crate) fn document_symbols(
    magento: &Magento,
    path: &Path,
) -> Option<Vec<DocumentSymbol>> {
    if path.extension()?.to_str() != Some("xml") {
        return None;
    }
    let text = magento.read_source(path).ok()?;
    let index = LineIndex::new(&text);

    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().check_end_names = false;
    let mut roots: Vec<DocumentSymbol> = Vec::new();
    // One frame per open element: `Some(symbol)` when it's an outline entry (children
    // collect into it), `None` when it's structural only (children pass through).
    let mut stack: Vec<Option<DocumentSymbol>> = Vec::new();

    let attach = |stack: &mut Vec<Option<DocumentSymbol>>,
                  roots: &mut Vec<DocumentSymbol>,
                  symbol: DocumentSymbol| {
        match stack.iter_mut().rev().find_map(|frame| frame.as_mut()) {
            Some(parent) => parent.children.get_or_insert_with(Vec::new).push(symbol),
            None => roots.push(symbol),
        }
    };

    loop {
        let before = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) => {
                stack.push(make_symbol(&e, &text, &index, before));
            }
            Ok(quick_xml::events::Event::Empty(e)) => {
                if let Some(symbol) = make_symbol(&e, &text, &index, before) {
                    attach(&mut stack, &mut roots, symbol);
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                if let Some(frame) = stack.pop() {
                    if let Some(mut symbol) = frame {
                        let end = index.position(reader.buffer_position() as usize);
                        symbol.range.end = end;
                        attach(&mut stack, &mut roots, symbol);
                    }
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    (!roots.is_empty()).then_some(roots)
}

fn make_symbol(
    e: &quick_xml::events::BytesStart<'_>,
    text: &str,
    index: &LineIndex,
    start_offset: usize,
) -> Option<DocumentSymbol> {
    let element = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut ident = None;
    for attr in e.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        if ID_ATTRS.contains(&key.as_str()) {
            ident = Some(String::from_utf8_lossy(&attr.value).into_owned());
            break;
        }
    }
    let ident = ident?;
    let kind = match element.as_str() {
        "preference" => SymbolKind::INTERFACE,
        "type" | "virtualType" | "block" => SymbolKind::CLASS,
        "plugin" => SymbolKind::METHOD,
        "event" => SymbolKind::EVENT,
        "observer" | "job" => SymbolKind::FUNCTION,
        "argument" | "item" | "field" => SymbolKind::FIELD,
        "table" | "container" => SymbolKind::STRUCT,
        "column" => SymbolKind::PROPERTY,
        _ => SymbolKind::OBJECT,
    };
    let position = index.position(start_offset.min(text.len()));
    #[allow(deprecated)]
    Some(DocumentSymbol {
        name: format!("{element} {ident}"),
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: lsp_types::Range::new(position, position),
        selection_range: lsp_types::Range::new(position, position),
        children: None,
    })
}

/// Editor-wide symbol search over the indexes: classes (from the cached catalog),
/// events, config paths, ACL ids, modules, tables. Ranked prefix-first, capped.
pub(crate) fn workspace_symbols(
    magento: &Magento,
    class_catalog: &[ClassName],
    query: &str,
) -> Vec<SymbolInformation> {
    let query = query.to_lowercase();
    if query.len() < 2 {
        return Vec::new();
    }
    let matches = |name: &str| -> Option<u8> {
        let lower = name.to_lowercase();
        if lower.starts_with(&query) {
            Some(0)
        } else if lower.rsplit(['\\', '/']).next().is_some_and(|s| s.starts_with(&query)) {
            Some(1)
        } else if lower.contains(&query) {
            Some(2)
        } else {
            None
        }
    };
    let location = |source: &magequery_core::Source| -> Option<Location> {
        let file = if source.file.is_absolute() {
            source.file.clone()
        } else {
            magento.root().join(&source.file)
        };
        let line = source.line.saturating_sub(1);
        Some(Location {
            uri: lsp_types::Url::from_file_path(file).ok()?,
            range: lsp_types::Range::new(
                lsp_types::Position::new(line, 0),
                lsp_types::Position::new(line, 0),
            ),
        })
    };

    let mut scored: Vec<(u8, SymbolInformation)> = Vec::new();
    #[allow(deprecated)]
    let mut push = |rank: u8, name: String, kind: SymbolKind, loc: Option<Location>| {
        if let Some(location) = loc {
            scored.push((
                rank,
                SymbolInformation {
                    name,
                    kind,
                    tags: None,
                    deprecated: None,
                    location,
                    container_name: None,
                },
            ));
        }
    };

    for class in class_catalog {
        if let Some(rank) = matches(class.as_str()) {
            let loc = magento.class_file(class).and_then(|file| {
                Some(Location {
                    uri: lsp_types::Url::from_file_path(file).ok()?,
                    range: lsp_types::Range::default(),
                })
            });
            push(rank, class.as_str().to_string(), SymbolKind::CLASS, loc);
        }
    }
    for area in Area::ALL {
        for (event, _) in magento.events(area) {
            if let Some(rank) = matches(event.as_str()) {
                let loc = magento
                    .observers(&event, area)
                    .first()
                    .and_then(|obs| location(&obs.source));
                push(rank, event.as_str().to_string(), SymbolKind::EVENT, loc);
            }
        }
    }
    for field in magento.system_config(None) {
        if let Some(rank) = matches(&field.path) {
            push(rank, field.path.clone(), SymbolKind::PROPERTY, location(&field.source));
        }
    }
    for resource in magento.acl(None) {
        if let Some(rank) = matches(&resource.id) {
            push(rank, resource.id.clone(), SymbolKind::KEY, location(&resource.source));
        }
    }
    for module in magento.modules() {
        if let Some(rank) = matches(module.name.as_str()) {
            let file = module.path.join("etc/module.xml");
            let loc = lsp_types::Url::from_file_path(file).ok().map(|uri| Location {
                uri,
                range: lsp_types::Range::default(),
            });
            push(rank, module.name.as_str().to_string(), SymbolKind::MODULE, loc);
        }
    }
    for table in magento.schema(None) {
        if let Some(rank) = matches(&table.name) {
            push(rank, table.name.clone(), SymbolKind::STRUCT, location(&table.source));
        }
    }

    scored.sort_by(|a, b| (a.0, &a.1.name).cmp(&(b.0, &b.1.name)));
    let mut seen = std::collections::HashSet::new();
    scored
        .into_iter()
        .filter(|(_, s)| seen.insert((s.name.clone(), format!("{:?}", s.kind))))
        .take(100)
        .map(|(_, s)| s)
        .collect()
}
