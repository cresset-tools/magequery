//! A [`DoctorCheck`] as CI reads it: GitHub Actions workflow commands
//! (`::error file=…,line=…::…`), which annotate the pull request lines that
//! cause a finding; a GitLab Code Quality report, which a merge request shows
//! once a job uploads it as `artifacts:reports:codequality`; and a SARIF 2.1.0
//! log, which code scanning and most other CI systems import.
//!
//! Paths are written relative to `base`, normally the working directory: CI
//! runs from the repository root, which need not be the Magento root.

use std::path::{Path, PathBuf};

use super::{ChainNode, DoctorCheck, DoctorLint, StaleEntry};
use crate::error::Severity;

/// The rule id of a stale baseline entry, which has no lint of its own.
const BASELINE_RULE: &str = "baseline";

/// One finding or stale baseline entry, as both formats write it.
struct Annotation<'a> {
    level: Level,
    /// The lint code, or [`BASELINE_RULE`].
    rule: &'a str,
    lint: Option<DoctorLint>,
    message: String,
    file: Option<String>,
    line: u32,
}

#[derive(Clone, Copy)]
enum Level {
    Error,
    Warning,
    Notice,
}

impl From<Severity> for Level {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Error => Level::Error,
            Severity::Warning => Level::Warning,
            Severity::Info => Level::Notice,
        }
    }
}

impl DoctorCheck {
    /// One GitHub Actions workflow command per finding and per stale baseline
    /// entry, each on its own line.
    pub fn github_annotations(&self, base: &Path) -> String {
        let mut out = String::new();
        for a in self.annotations(base) {
            let command = match a.level {
                Level::Error => "error",
                Level::Warning => "warning",
                Level::Notice => "notice",
            };
            let mut props = Vec::new();
            if let Some(file) = &a.file {
                props.push(format!("file={}", escape_property(file)));
                if a.line > 0 {
                    props.push(format!("line={}", a.line));
                }
            }
            let title = match a.lint {
                Some(lint) => format!("{} {}", a.rule, lint_name(lint)),
                None => a.rule.to_owned(),
            };
            props.push(format!("title={}", escape_property(&title)));
            out.push_str(&format!("::{command} {}::{}\n", props.join(","), escape_data(&a.message)));
        }
        out
    }

    /// The findings and stale baseline entries as a GitLab Code Quality
    /// report. GitLab needs a path for every issue, so one without a file is
    /// placed on the root's `app/etc/config.php`, where the module set most
    /// such findings are about is declared.
    pub fn gitlab_code_quality(&self, base: &Path) -> serde_json::Value {
        use sha1::{Digest, Sha1};
        let fallback = relative(&self.root.join("app/etc/config.php"), &canonical(base));
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let issues: Vec<serde_json::Value> = self
            .annotations(base)
            .into_iter()
            .map(|a| {
                let path = a.file.clone().unwrap_or_else(|| fallback.clone());
                // Not the line: an edit above a finding must not make it new.
                // Repeats of one finding are told apart by their order.
                let key = format!("{}\0{path}\0{}", a.rule, a.message);
                let n = seen.entry(key.clone()).or_default();
                *n += 1;
                let fingerprint = Sha1::digest(format!("{key}\0{n}").as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let severity = match a.level {
                    Level::Error => "major",
                    Level::Warning => "minor",
                    Level::Notice => "info",
                };
                serde_json::json!({
                    "description": a.message,
                    "check_name": a.rule,
                    "fingerprint": fingerprint,
                    "severity": severity,
                    "location": { "path": path, "lines": { "begin": a.line.max(1) } },
                })
            })
            .collect();
        serde_json::Value::Array(issues)
    }

    /// The findings and stale baseline entries as a SARIF 2.1.0 log, with a
    /// rule for every code a result uses.
    pub fn sarif(&self, base: &Path) -> serde_json::Value {
        use serde_json::json;
        let annotations = self.annotations(base);
        let mut rules: Vec<serde_json::Value> = Vec::new();
        let mut rule_ids: Vec<&str> = Vec::new();
        for a in &annotations {
            if rule_ids.contains(&a.rule) {
                continue;
            }
            rule_ids.push(a.rule);
            rules.push(match a.lint {
                Some(lint) => json!({
                    "id": a.rule,
                    "name": lint_name(lint),
                    "shortDescription": { "text": lint.summary() },
                    "fullDescription": { "text": lint.explanation() },
                    "help": { "text": format!("magequery doctor --explain {}", a.rule) },
                }),
                None => json!({
                    "id": BASELINE_RULE,
                    "name": "stale-baseline-entry",
                    "shortDescription": {
                        "text": "a baseline entry accepts more findings than occur, so it would hide one that came back"
                    },
                    "help": { "text": "magequery doctor --prune-baseline removes the stale entries" },
                }),
            });
        }
        let results: Vec<serde_json::Value> = annotations
            .iter()
            .map(|a| {
                let level = match a.level {
                    Level::Error => "error",
                    Level::Warning => "warning",
                    Level::Notice => "note",
                };
                let mut result = json!({
                    "ruleId": a.rule,
                    "ruleIndex": rule_ids.iter().position(|r| *r == a.rule),
                    "level": level,
                    "message": { "text": a.message },
                });
                if let Some(file) = &a.file {
                    let mut location = json!({ "artifactLocation": { "uri": file } });
                    if a.line > 0 {
                        location["region"] = json!({ "startLine": a.line });
                    }
                    result["locations"] = json!([{ "physicalLocation": location }]);
                }
                result
            })
            .collect();
        json!({
            "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {
                    "driver": {
                        "name": "magequery",
                        "version": env!("CARGO_PKG_VERSION"),
                        "informationUri": "https://github.com/cresset-tools/magequery",
                        "rules": rules,
                    }
                },
                "results": results,
            }]
        })
    }

    fn annotations(&self, base: &Path) -> Vec<Annotation<'_>> {
        let base = canonical(base);
        let mut out: Vec<Annotation<'_>> = self
            .report
            .findings
            .iter()
            .map(|f| {
                let mut message = f.message.clone();
                chain_text(&f.chain, 1, &base, &mut message);
                Annotation {
                    level: f.severity.into(),
                    rule: f.code,
                    lint: Some(f.lint),
                    message,
                    file: f.source.as_ref().map(|s| relative(&s.file, &base)),
                    line: f.source.as_ref().map_or(0, |s| s.line),
                }
            })
            .collect();
        let baseline_file = self.baseline.as_ref().map(|(_, path)| relative(path, &base));
        out.extend(self.stale().iter().map(|s| Annotation {
            level: Level::Error,
            rule: BASELINE_RULE,
            lint: None,
            message: stale_message(s),
            file: baseline_file.clone(),
            line: s.line,
        }));
        out
    }
}

fn stale_message(s: &StaleEntry) -> String {
    let e = &s.entry;
    let file = e.file.as_deref().map(|f| format!(" in {f}")).unwrap_or_default();
    let found = match s.matched {
        0 => "no longer occurs".to_owned(),
        n => format!("is accepted {} times but occurs {n}", e.count),
    };
    format!("{} \"{}\"{file} {found}", e.code, e.message)
}

/// The chain under a finding, one indented line per step, so the annotation
/// says why the finding has its severity.
fn chain_text(chain: &[ChainNode], depth: usize, base: &Path, out: &mut String) {
    for node in chain {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
        out.push_str(&node.message);
        if let Some(source) = &node.source {
            out.push_str(&format!(" ({}:{})", relative(&source.file, base), source.line));
        }
        chain_text(&node.children, depth + 1, base, out);
    }
}

/// The lint's kebab-case name, as `--deny` takes it.
fn lint_name(lint: DoctorLint) -> String {
    match serde_json::to_value(lint) {
        Ok(serde_json::Value::String(name)) => name,
        _ => lint.code().to_owned(),
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// `path` relative to `base` (canonical), `/`-separated; absolute when it lies
/// outside it.
fn relative(path: &Path, base: &Path) -> String {
    let path = canonical(path);
    let shown = path.strip_prefix(base).unwrap_or(&path);
    shown.to_string_lossy().replace('\\', "/")
}

/// A workflow command's message: `%`, CR and LF escaped, as GitHub's toolkit
/// does (`escapeData`).
fn escape_data(s: &str) -> String {
    s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

/// A workflow command's property value: also `:` and `,`, which would end it
/// (`escapeProperty`).
fn escape_property(s: &str) -> String {
    escape_data(s).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_escapes_what_would_end_the_command() {
        assert_eq!(escape_data("100% done\r\nnext"), "100%25 done%0D%0Anext");
        // `::` inside the message is fine: only the first one separates.
        assert_eq!(escape_data("a::b"), "a::b");
    }

    #[test]
    fn properties_also_escape_their_separators() {
        assert_eq!(escape_property("C:\\shop,x"), "C%3A\\shop%2Cx");
        assert_eq!(escape_property("50%\n"), "50%25%0A");
    }

    #[test]
    fn a_path_outside_the_base_stays_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("a/b.xml");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "").unwrap();
        assert_eq!(relative(&inside, &canonical(dir.path())), "a/b.xml");
        let other = tempfile::tempdir().unwrap();
        assert!(Path::new(&relative(&inside, &canonical(other.path()))).is_absolute());
    }
}
