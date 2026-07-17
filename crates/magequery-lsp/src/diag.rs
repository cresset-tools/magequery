//! Core [`Diagnostic`]s and doctor findings, regrouped per file as LSP diagnostics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{DiagnosticSeverity, NumberOrString, Position, Range};
use magequery_core::{Magento, Severity, Source};

/// Everything the index knows to be wrong, keyed by the file to publish it on: doctor
/// findings (dangling references, cycles, drift — each tagged with its kebab-case lint id
/// as the diagnostic code) plus per-file parse diagnostics. Findings without a source
/// file (e.g. module-set drift) have nowhere to render and are skipped — the CLI's
/// `doctor` remains the complete view.
pub(crate) fn diagnostics_by_file(
    magento: &Magento,
) -> HashMap<PathBuf, Vec<lsp_types::Diagnostic>> {
    let mut by_file: HashMap<PathBuf, Vec<lsp_types::Diagnostic>> = HashMap::new();

    // Doctor first: it builds every lazy index, so the diagnostics() sweep below also
    // sees each file's parse problems.
    for finding in magento.doctor(None).findings {
        let Some(source) = finding.source else { continue };
        // DoctorLint serializes kebab-case ("preference-target-missing") — reuse that as
        // the LSP diagnostic code rather than maintaining a parallel name table.
        let code = serde_json::to_value(finding.lint)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        push(
            &mut by_file,
            magento.root(),
            source,
            finding.severity,
            finding.message,
            code,
            finding.subject,
        );
    }
    for diagnostic in magento.diagnostics() {
        let Some(source) = diagnostic.source else { continue };
        push(
            &mut by_file,
            magento.root(),
            source,
            diagnostic.severity,
            diagnostic.message,
            None,
            None,
        );
    }
    by_file
}

fn push(
    by_file: &mut HashMap<PathBuf, Vec<lsp_types::Diagnostic>>,
    root: &Path,
    source: Source,
    severity: Severity,
    message: String,
    code: Option<String>,
    subject: Option<String>,
) {
    // Source lines are 1-based with 0 = "known file, unknown line"; either way the range
    // is the whole line (core has no column provenance), which clients clamp for us.
    let line = source.line.saturating_sub(1);
    let diagnostic = lsp_types::Diagnostic {
        range: Range::new(Position::new(line, 0), Position::new(line, u32::MAX)),
        severity: Some(match severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            _ => DiagnosticSeverity::WARNING,
        }),
        code: code.map(NumberOrString::String),
        source: Some("magequery".to_string()),
        message,
        // Structured facts for quick fixes (round-tripped into codeAction requests).
        data: subject.map(|subject| serde_json::json!({ "subject": subject })),
        ..Default::default()
    };
    let file = if source.file.is_absolute() {
        source.file
    } else {
        root.join(&source.file)
    };
    by_file.entry(file).or_default().push(diagnostic);
}
