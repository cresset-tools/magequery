//! Quick fixes for doctor diagnostics. Every fix is a pure workspace edit derived from
//! the diagnostic's structured `data.subject` (never from the message text): did-you-mean
//! replacements against the class/ACL catalogs, config.php module removal, and
//! registration boilerplate for unregistered commands/observers/plugins.

use std::path::{Path, PathBuf};

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CreateFile, DocumentChangeOperation,
    DocumentChanges, Position, Range, ResourceOp, TextDocumentEdit, TextEdit, Url,
    WorkspaceEdit,
};
use magequery_core::Magento;

use crate::textpos::LineIndex;

const MISSING_CLASS_LINTS: [&str; 10] = [
    "preference-target-missing",
    "virtual-type-base-missing",
    "plugin-class-missing",
    "di-argument-class-missing",
    "observer-class-missing",
    "cron-instance-missing",
    "webapi-service-missing",
    "command-class-missing",
    "mq-handler-missing",
    "graphql-resolver-missing",
];

pub(crate) fn code_actions(
    magento: &Magento,
    class_catalog: &[magequery_core::ClassName],
    path: &Path,
    diagnostics: &[lsp_types::Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    for diagnostic in diagnostics {
        if diagnostic.source.as_deref() != Some("magequery") {
            continue;
        }
        let Some(lsp_types::NumberOrString::String(lint)) = &diagnostic.code else { continue };
        let subject = diagnostic
            .data
            .as_ref()
            .and_then(|d| d.get("subject"))
            .and_then(|s| s.as_str());
        let Some(subject) = subject else { continue };

        if MISSING_CLASS_LINTS.contains(&lint.as_str()) {
            let mut names: Vec<&str> = class_catalog.iter().map(|c| c.as_str()).collect();
            let virtual_types = magento.virtual_type_names();
            names.extend(virtual_types.iter().map(|v| v.as_str()));
            for candidate in did_you_mean(subject, &names) {
                if let Some(action) =
                    replace_subject(magento, path, diagnostic, subject, &candidate)
                {
                    actions.push(action);
                }
            }
        } else if lint == "acl-resource-unknown" {
            let acl_ids: Vec<String> =
                magento.acl(None).into_iter().map(|resource| resource.id).collect();
            let names: Vec<&str> = acl_ids.iter().map(String::as_str).collect();
            for candidate in did_you_mean(subject, &names) {
                if let Some(action) =
                    replace_subject(magento, path, diagnostic, subject, &candidate)
                {
                    actions.push(action);
                }
            }
        } else if lint == "module-missing-on-disk" {
            actions.push(remove_line_action(
                path,
                diagnostic,
                format!("Remove {subject} from config.php"),
            ));
        } else if lint == "command-unregistered" {
            if let Some(action) = register_action(
                magento,
                path,
                diagnostic,
                subject,
                "etc/di.xml",
                DI_XML_HEADER,
                &format!(
                    "    <type name=\"Magento\\Framework\\Console\\CommandListInterface\">\n        <arguments>\n            <argument name=\"commands\" xsi:type=\"array\">\n                <item name=\"{item}\" xsi:type=\"object\">{subject}</item>\n            </argument>\n        </arguments>\n    </type>\n",
                    item = snake_case(subject),
                ),
                format!("Register `{}` on CommandListInterface in di.xml", short(subject)),
            ) {
                actions.push(action);
            }
        } else if lint == "observer-unregistered" {
            if let Some(action) = register_action(
                magento,
                path,
                diagnostic,
                subject,
                "etc/events.xml",
                EVENTS_XML_HEADER,
                &format!(
                    "    <event name=\"EVENT_NAME_TODO\">\n        <observer name=\"{item}\" instance=\"{subject}\"/>\n    </event>\n",
                    item = snake_case(subject),
                ),
                format!("Register `{}` in events.xml (event name TODO)", short(subject)),
            ) {
                actions.push(action);
            }
        } else if lint == "plugin-unregistered" {
            if let Some(action) = register_action(
                magento,
                path,
                diagnostic,
                subject,
                "etc/di.xml",
                DI_XML_HEADER,
                &format!(
                    "    <type name=\"TARGET_CLASS_TODO\">\n        <plugin name=\"{item}\" type=\"{subject}\"/>\n    </type>\n",
                    item = snake_case(subject),
                ),
                format!("Register `{}` as a plugin in di.xml (target TODO)", short(subject)),
            ) {
                actions.push(action);
            }
        }
    }
    actions
}

const DI_XML_HEADER: &str = "<?xml version=\"1.0\"?>\n<config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:noNamespaceSchemaLocation=\"urn:magento:framework:ObjectManager/etc/config.xsd\">\n";
const EVENTS_XML_HEADER: &str = "<?xml version=\"1.0\"?>\n<config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:noNamespaceSchemaLocation=\"urn:magento:framework:Event/etc/events.xsd\">\n";

/// Replace the subject text on the diagnostic's line with a candidate.
fn replace_subject(
    magento: &Magento,
    path: &Path,
    diagnostic: &lsp_types::Diagnostic,
    subject: &str,
    candidate: &str,
) -> Option<CodeActionOrCommand> {
    let text = magento.read_source(path).ok()?;
    let index = LineIndex::new(&text);
    let line_start = index.offset(Position::new(diagnostic.range.start.line, 0))?;
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |i| line_start + i);
    let column = text[line_start..line_end].find(subject)?;
    let span = line_start + column..line_start + column + subject.len();
    let edit = TextEdit { range: index.range(span), new_text: candidate.to_string() };
    Some(quick_fix(
        format!("Replace with `{candidate}`"),
        diagnostic,
        WorkspaceEdit {
            changes: Some(
                [(Url::from_file_path(path).ok()?, vec![edit])].into_iter().collect(),
            ),
            ..Default::default()
        },
    ))
}

/// Delete the diagnostic's whole line (config.php module entries).
fn remove_line_action(
    path: &Path,
    diagnostic: &lsp_types::Diagnostic,
    title: String,
) -> CodeActionOrCommand {
    let line = diagnostic.range.start.line;
    let edit = TextEdit {
        range: Range::new(Position::new(line, 0), Position::new(line + 1, 0)),
        new_text: String::new(),
    };
    let changes = Url::from_file_path(path)
        .ok()
        .map(|uri| [(uri, vec![edit])].into_iter().collect());
    quick_fix(title, diagnostic, WorkspaceEdit { changes, ..Default::default() })
}

/// Insert a registration block into the owning module's config file, creating the file
/// when the module doesn't have it yet. Offered for vendor modules too — the diagnostic
/// lives there, and a local edit is legitimate while debugging — but composer will wipe
/// it; the user sees the target path in the edit preview.
#[allow(clippy::too_many_arguments)]
fn register_action(
    magento: &Magento,
    path: &Path,
    diagnostic: &lsp_types::Diagnostic,
    _subject: &str,
    file_rel: &str,
    new_file_header: &str,
    block: &str,
    title: String,
) -> Option<CodeActionOrCommand> {
    let module = magento
        .modules()
        .iter()
        .filter(|module| path.starts_with(&module.path))
        .max_by_key(|module| module.path.as_os_str().len())?;
    let target: PathBuf = module.path.join(file_rel);
    let uri = Url::from_file_path(&target).ok()?;

    let edit = match magento.read_source(&target) {
        Ok(text) => {
            let insert_at = text.rfind("</config>")?;
            let index = LineIndex::new(&text);
            let position = index.position(insert_at);
            WorkspaceEdit {
                changes: Some(
                    [(
                        uri,
                        vec![TextEdit {
                            range: Range::new(position, position),
                            new_text: block.to_string(),
                        }],
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }
        }
        Err(_) => WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri.clone(),
                    options: None,
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                        uri,
                        version: None,
                    },
                    edits: vec![lsp_types::OneOf::Left(TextEdit {
                        range: Range::default(),
                        new_text: format!("{new_file_header}{block}</config>\n"),
                    })],
                }),
            ])),
            ..Default::default()
        },
    };
    Some(quick_fix(title, diagnostic, edit))
}

fn quick_fix(
    title: String,
    diagnostic: &lsp_types::Diagnostic,
    edit: WorkspaceEdit,
) -> CodeActionOrCommand {
    CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(edit),
        ..Default::default()
    })
}

fn short(class: &str) -> &str {
    class.rsplit('\\').next().unwrap_or(class)
}

fn snake_case(class: &str) -> String {
    let mut out = String::new();
    for (i, ch) in short(class).chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The closest catalog entries to `subject` by bounded Levenshtein distance — the
/// did-you-mean candidates. Length-prefiltered; up to three, nearest first.
fn did_you_mean(subject: &str, names: &[&str]) -> Vec<String> {
    let max_distance = 4usize.min(2 + subject.len() / 16);
    let mut scored: Vec<(usize, &str)> = names
        .iter()
        .filter(|name| {
            name.len().abs_diff(subject.len()) <= max_distance && **name != subject
        })
        .filter_map(|name| {
            levenshtein_capped(subject, name, max_distance).map(|d| (d, *name))
        })
        .collect();
    scored.sort_by_key(|(distance, name)| (*distance, name.to_string()));
    scored.into_iter().take(3).map(|(_, name)| name.to_string()).collect()
}

/// Levenshtein distance, abandoning early once it exceeds `cap`.
fn levenshtein_capped(a: &str, b: &str, cap: usize) -> Option<usize> {
    let a: Vec<u8> = a.bytes().collect();
    let b: Vec<u8> = b.bytes().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        let mut row_min = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let value = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            row_min = row_min.min(value);
            current.push(value);
        }
        if row_min > cap {
            return None;
        }
        previous = current;
    }
    (previous[b.len()] <= cap).then_some(previous[b.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn did_you_mean_ranks_by_distance() {
        let names = ["Acme\\Widget\\Model\\Thing", "Acme\\Widget\\Model\\Mising", "Other"];
        let got = did_you_mean("Acme\\Widget\\Model\\Missing", &names);
        assert_eq!(got, vec!["Acme\\Widget\\Model\\Mising".to_string()]);
    }

    #[test]
    fn snake_cases_short_names() {
        assert_eq!(snake_case("Acme\\Widget\\Console\\HelloWorld"), "hello_world");
    }
}
