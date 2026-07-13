//! `textDocument/rename` + `prepareRename`.
//!
//! Scope: the pure-Magento string identifiers Intelephense/Phpactor can't see and that are
//! a *literal string* wherever they appear — ACL resource ids, event names, and layout
//! block/container names. Each is renamed across every occurrence our indexes (plus a PHP
//! string-literal sweep) can reach, with column-precise spans.
//!
//! Deliberately **not** renameable:
//! - **Classes** — a class rename belongs to the PHP language server (the file, the `class`
//!   declaration, every `use`/type reference); ours would touch only the XML half and leave
//!   PHP inconsistent. Left to Intelephense/Phpactor on purpose (the "not a PHP language
//!   server" line).
//! - **Config paths** — expressed as *nested XML elements* in config.xml/system.xml, not a
//!   renameable string, so a text rewrite can't move them coherently.
//! - **Templates / layout handles** — bound to a `.phtml`/handle *file*; renaming the
//!   reference string without moving the file breaks it (a filesystem concern, not a text
//!   one).
//!
//! Correctness comes from re-classifying every candidate occurrence with [`entity_at`]:
//! only a span that classifies to the *same* entity is rewritten, so a longer identifier
//! that merely has ours as a prefix (`Foo::sales` inside `Foo::sales_view`) or a snake_case
//! string that isn't an event (no `dispatch` on the line) is left untouched. The core grep
//! ([`Magento::files_containing`]) is only a coarse file pre-filter; this module owns the
//! precision.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{Position, PrepareRenameResponse, TextEdit, Url, WorkspaceEdit};
use magequery_core::Magento;

use crate::entity::{entity_at, Entity};
use crate::textpos::LineIndex;

/// The identifier string if `entity` is one we rename; `None` otherwise. Every arm here is
/// a literal string with no structural, filesystem, or PHP-declaration coupling.
fn renameable(entity: &Entity) -> Option<String> {
    match entity {
        Entity::Acl(id) => Some(id.clone()),
        Entity::Event(event) => Some(event.as_str().to_string()),
        Entity::BlockName(name) => Some(name.clone()),
        _ => None,
    }
}

/// Validate the cursor sits on a renameable identifier, returning its range + the current
/// text as the placeholder (the client pre-fills the rename box with it). `None` — a null
/// response — is how the client learns rename isn't available here (a class, a config path,
/// anything unclassified).
pub(crate) fn prepare_rename(
    magento: &Magento,
    path: &Path,
    position: Position,
) -> Option<PrepareRenameResponse> {
    let text = magento.read_source(path).ok()?;
    let index = LineIndex::new(&text);
    let offset = index.offset(position)?;
    let found = entity_at(&entity_file_name(path)?, &text, offset)?;
    renameable(&found.entity)?;
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: index.range(found.span.clone()),
        placeholder: text.get(found.span)?.to_string(),
    })
}

/// The workspace edit renaming the identifier under the cursor to `new_name` — every
/// occurrence our indexes and the PHP grep reach, each rewritten over its exact span.
/// `None` when the cursor isn't on a renameable entity, the new name is empty/unchanged, or
/// nothing references it.
pub(crate) fn rename(
    magento: &Magento,
    path: &Path,
    position: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let new_name = new_name.trim();
    let text = magento.read_source(path).ok()?;
    let offset = LineIndex::new(&text).offset(position)?;
    let found = entity_at(&entity_file_name(path)?, &text, offset)?;
    let old = renameable(&found.entity)?;
    if new_name.is_empty() || new_name == old {
        return None;
    }

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for file in candidate_files(magento, path, &found.entity, &old) {
        let file_text = if file == path {
            text.clone()
        } else {
            match magento.read_source(&file) {
                Ok(text) => text,
                Err(_) => continue,
            }
        };
        let Some(file_name) = entity_file_name(&file) else { continue };
        let edits = occurrences(&file_name, &file_text, &found.entity, &old, new_name);
        if edits.is_empty() {
            continue;
        }
        if let Ok(uri) = Url::from_file_path(&file) {
            changes.entry(uri).or_default().extend(edits);
        }
    }
    (!changes.is_empty()).then(|| WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

/// The files a rename might touch. ACL ids and event names live in literal strings across
/// XML *and* PHP, so the core grep is the source of truth; block names live only in the
/// (already-parsed) layout ops, whose sources point at the contributing files. The current
/// file is always included so at minimum the occurrence under the cursor is covered.
fn candidate_files(magento: &Magento, path: &Path, entity: &Entity, old: &str) -> Vec<PathBuf> {
    let mut files = vec![path.to_path_buf()];
    match entity {
        Entity::Acl(_) | Entity::Event(_) => files.extend(magento.files_containing(old)),
        Entity::BlockName(name) => {
            let area = crate::layout::area_of_file(magento, path);
            for (_, op) in crate::layout::ops_where(magento, area, |op| {
                op.name == *name || op.parent.as_deref() == Some(name.as_str())
            }) {
                files.push(absolute(magento, op.source.file.clone()));
            }
        }
        _ => {}
    }
    files.sort();
    files.dedup();
    files
}

/// Every whole-token occurrence of `old` in `text` that classifies to `target`, as an edit
/// replacing it with `new`. The [`entity_at`] re-classification is the precision: a
/// substring of a longer identifier, or a same-looking string in the wrong context, fails
/// the equality check and is skipped. Pure (no `Magento`), so it unit-tests directly.
fn occurrences(
    file_name: &str,
    text: &str,
    target: &Entity,
    old: &str,
    new: &str,
) -> Vec<TextEdit> {
    let index = LineIndex::new(text);
    let mut edits = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut from = 0;
    while let Some(relative) = text[from..].find(old) {
        let start = from + relative;
        from = start + old.len();
        // Classify at the middle of the match: only spans that ARE this exact entity get
        // rewritten. A prefix of a longer id, or a snake_case string that isn't an event,
        // classifies to something else (or nothing) and is dropped.
        let found = match entity_at(file_name, text, start + old.len() / 2) {
            Some(found) if found.entity == *target => found,
            _ => continue,
        };
        // For these string entities the classified span equals the identifier verbatim (no
        // leading `\` or `::method` trimming, unlike classes). Guard so a surprise never
        // rewrites the wrong bytes.
        if text.get(found.span.clone()) != Some(old) || !seen.insert(found.span.start) {
            continue;
        }
        edits.push(TextEdit { range: index.range(found.span), new_text: new.to_string() });
    }
    edits
}

/// The file name [`entity_at`] classifies by, treating `.phtml` as PHP (it is) so a
/// `dispatch()`/`isAllowed()` in a template participates in the sweep.
fn entity_file_name(file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?;
    Some(if name.ends_with(".phtml") { "template.php".to_string() } else { name.to_string() })
}

fn absolute(magento: &Magento, file: PathBuf) -> PathBuf {
    if file.is_absolute() {
        file
    } else {
        magento.root().join(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use magequery_core::EventName;

    fn texts(edits: &[TextEdit]) -> Vec<String> {
        edits.iter().map(|e| e.new_text.clone()).collect()
    }

    #[test]
    fn acl_across_xml_and_php() {
        let target = Entity::Acl("Acme_Widget::manage".to_string());

        // acl.xml declaration + a webapi `ref` reference; the longer `manage_advanced` id
        // (ours as a prefix) must be left alone.
        let xml = r#"<resource id="Acme_Widget::manage" title="Manage"/>
<resource ref="Acme_Widget::manage"/>
<resource id="Acme_Widget::manage_advanced"/>"#;
        let edits = occurrences("acl.xml", xml, &target, "Acme_Widget::manage", "Acme_Widget::admin");
        assert_eq!(edits.len(), 2, "id + ref, not the longer prefix: {edits:?}");
        assert_eq!(edits[0].range.start.line, 0);
        assert_eq!(edits[1].range.start.line, 1);
        assert!(texts(&edits).iter().all(|t| t == "Acme_Widget::admin"));

        // PHP: an `isAllowed` guard.
        let php = "<?php\nif ($this->auth->isAllowed('Acme_Widget::manage')) { echo 1; }\n";
        let edits = occurrences("Block.php", php, &target, "Acme_Widget::manage", "Acme_Widget::admin");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range.start.line, 1);
    }

    #[test]
    fn event_only_at_dispatch() {
        let target = Entity::Event(EventName::new("acme_thing_saved"));

        let xml = r#"<event name="acme_thing_saved"><observer name="x" instance="Y"/></event>"#;
        let edits = occurrences("events.xml", xml, &target, "acme_thing_saved", "acme_thing_updated");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "acme_thing_updated");

        // A dispatch renames; a same-named array key (no `dispatch` on the line) does not.
        let php = "<?php\n$m->dispatch('acme_thing_saved', $data);\n$row = ['acme_thing_saved' => 1];\n";
        let edits = occurrences("Model.php", php, &target, "acme_thing_saved", "acme_thing_updated");
        assert_eq!(edits.len(), 1, "dispatch yes, array-key no: {edits:?}");
        assert_eq!(edits[0].range.start.line, 1);
    }

    #[test]
    fn block_name_across_layout() {
        let target = Entity::BlockName("acme.chip".to_string());
        let xml = r#"<referenceContainer name="content">
    <block name="acme.chip" class="Acme\Widget\Block\Chip"/>
    <move element="acme.chip" destination="content"/>
</referenceContainer>"#;
        let edits = occurrences("catalog_product_view.xml", xml, &target, "acme.chip", "acme.badge");
        assert_eq!(edits.len(), 2, "block name + move element: {edits:?}");
        assert!(texts(&edits).iter().all(|t| t == "acme.badge"));
    }

    #[test]
    fn non_renameable_entities_return_none() {
        assert!(renameable(&Entity::Class(magequery_core::ClassName::new("Foo\\Bar"))).is_none());
        assert!(renameable(&Entity::ConfigPath("a/b/c".to_string())).is_none());
        assert!(renameable(&Entity::Module(magequery_core::ModuleName::new("Foo_Bar"))).is_none());
    }
}
