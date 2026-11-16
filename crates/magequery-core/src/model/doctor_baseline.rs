//! A `doctor` baseline (phpstan's `--generate-baseline`): the findings a project has
//! accepted for now, so CI fails only on new ones while the old ones are cleaned up.

use std::collections::BTreeMap;
use std::path::Path;

use super::{DoctorFinding, DoctorReport};

/// The accepted findings, read from and written to [`DoctorBaseline::FILE`] at the
/// Magento root. A finding is matched by its code, its file (root-relative, without the
/// line, so edits elsewhere in the file don't break the match) and its message; `count`
/// is how many such findings are accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DoctorBaseline {
    pub version: u32,
    pub ignore: Vec<BaselineEntry>,
    /// The 1-based line each `ignore` entry starts on, when parsed from a file.
    #[serde(skip)]
    lines: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct BaselineEntry {
    pub code: String,
    /// Root-relative, `/`-separated; `None` for findings without a file (module drift).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub message: String,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub count: usize,
}

fn one() -> usize {
    1
}
fn is_one(n: &usize) -> bool {
    *n == 1
}

/// What applying a baseline did, for the report's summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BaselineOutcome {
    /// Findings the baseline hid.
    pub suppressed: usize,
    /// Entries that accept more findings than occur (fixed, or changed): a stale entry
    /// would hide the finding if it came back, so it's an error until pruned. Empty when
    /// the report was narrowed to part of the project, which leaves the rest unchecked.
    pub unmatched: Vec<StaleEntry>,
}

/// A baseline entry that accepts more findings than the run found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StaleEntry {
    /// The entry as written (its `count` is what it accepts).
    pub entry: BaselineEntry,
    /// Its position in `ignore`.
    pub index: usize,
    /// The line it starts on in the baseline file; 0 when unknown.
    pub line: u32,
    /// How many findings it matched: fewer than `entry.count`.
    pub matched: usize,
}

type Key = (String, Option<String>, String);

impl DoctorBaseline {
    /// The file `doctor` reads by default, at the Magento root.
    pub const FILE: &'static str = "magequery-baseline.json";
    const VERSION: u32 = 1;

    /// Every finding of `report`, accepted.
    pub fn from_report(report: &DoctorReport, root: &Path) -> Self {
        let mut counts: BTreeMap<Key, usize> = BTreeMap::new();
        for f in &report.findings {
            *counts.entry(key(f, root)).or_default() += 1;
        }
        Self {
            version: Self::VERSION,
            ignore: counts
                .into_iter()
                .map(|((code, file, message), count)| BaselineEntry {
                    code,
                    file,
                    message,
                    count,
                })
                .collect(),
            lines: Vec::new(),
        }
    }

    pub fn parse(json: &str) -> Result<Self, String> {
        let mut baseline: Self = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if baseline.version != Self::VERSION {
            return Err(format!("unsupported baseline version {}", baseline.version));
        }
        baseline.lines = entry_lines(json);
        Ok(baseline)
    }

    /// Drop exactly what `stale` says no longer occurs: an entry that matched nothing is
    /// removed, one that matched some keeps a `count` of what it matched. Every other
    /// entry stays as it is.
    pub fn prune(&mut self, stale: &[StaleEntry]) {
        for s in stale {
            if let Some(e) = self.ignore.get_mut(s.index) {
                e.count = s.matched;
            }
        }
        self.ignore.retain(|e| e.count > 0);
        self.lines.clear();
    }

    /// Pretty JSON, sorted by code, file and message, so regenerating gives small diffs.
    pub fn to_json(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("a baseline always serializes");
        out.push('\n');
        out
    }

    /// Read `path`; `Ok(None)` when it doesn't exist.
    pub fn read(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(path) {
            Ok(json) => Self::parse(&json)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

impl DoctorReport {
    /// Drop the findings `baseline` accepts, each entry hiding at most `count` findings,
    /// and record what it did in [`DoctorReport::baseline`].
    pub fn apply_baseline(&mut self, baseline: &DoctorBaseline, root: &Path) {
        // Entries are matched one by one, in file order, so a stale one is reported as
        // itself even when another entry has the same key.
        let mut by_key: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
        for (i, e) in baseline.ignore.iter().enumerate() {
            by_key
                .entry((e.code.clone(), e.file.clone(), e.message.clone()))
                .or_default()
                .push(i);
        }
        let mut matched = vec![0; baseline.ignore.len()];
        let before = self.findings.len();
        self.findings.retain(|f| {
            let open = by_key.get(&key(f, root)).and_then(|entries| {
                entries.iter().find(|&&i| matched[i] < baseline.ignore[i].count)
            });
            match open {
                Some(&i) => {
                    matched[i] += 1;
                    false
                }
                None => true,
            }
        });
        self.baseline = Some(BaselineOutcome {
            suppressed: before - self.findings.len(),
            unmatched: baseline
                .ignore
                .iter()
                .enumerate()
                .filter(|&(i, e)| matched[i] < e.count)
                .map(|(i, e)| StaleEntry {
                    entry: e.clone(),
                    index: i,
                    line: baseline.lines.get(i).copied().unwrap_or(0),
                    matched: matched[i],
                })
                .collect(),
        });
    }
}

/// The line each element of the top-level `ignore` array starts on: a scan for the `{`
/// that opens an object at depth 3 (document, array, entry), skipping strings.
fn entry_lines(json: &str) -> Vec<u32> {
    let (mut lines, mut line, mut depth) = (Vec::new(), 1, 0);
    let (mut in_string, mut escaped) = (false, false);
    for c in json.chars() {
        match c {
            '\n' => line += 1,
            _ if in_string => match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            },
            '"' => in_string = true,
            '{' | '[' => {
                depth += 1;
                if c == '{' && depth == 3 {
                    lines.push(line);
                }
            }
            '}' | ']' => depth -= 1,
            _ => {}
        }
    }
    lines
}

fn key(f: &DoctorFinding, root: &Path) -> Key {
    let file = f.source.as_ref().map(|s| {
        let file = s.file.strip_prefix(root).unwrap_or(&s.file);
        let file = file.strip_prefix(".").unwrap_or(file);
        file.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    });
    (f.code.to_string(), file, f.message.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Severity;
    use crate::model::DoctorLint;
    use crate::{Area, ModuleName, Source};

    fn finding(lint: DoctorLint, file: Option<&str>, line: u32, message: &str) -> DoctorFinding {
        DoctorFinding {
            lint,
            code: lint.code(),
            severity: Severity::Error,
            message: message.into(),
            source: file.map(|f| Source {
                module: ModuleName::new("Acme_Widget"),
                file: Path::new("/shop").join(f),
                line,
                area: Area::Global,
            }),
            subject: None,
            chain: Vec::new(),
        }
    }

    // Matching ignores lines and counts repeats; an entry that accepts more than it
    // matched comes back as stale with its line in the file, and pruning removes exactly
    // that.
    #[test]
    fn baseline_hides_accepted_findings_and_prunes_stale_ones() {
        let root = Path::new("/shop");
        let di = "app/code/Acme/Widget/etc/di.xml";
        let report = |findings| DoctorReport {
            findings,
            baseline: None,
        };
        let old = report(vec![
            finding(DoctorLint::PluginClassMissing, Some(di), 10, "plugin gone"),
            finding(DoctorLint::PluginClassMissing, Some(di), 20, "plugin gone"),
            finding(DoctorLint::ModuleNotRegistered, None, 0, "module drift"),
        ]);
        let json = DoctorBaseline::from_report(&old, root).to_json();
        assert!(json.contains(r#""file": "app/code/Acme/Widget/etc/di.xml""#));
        let baseline = DoctorBaseline::parse(&json).unwrap();
        assert_eq!(baseline.ignore, DoctorBaseline::from_report(&old, root).ignore);
        assert_eq!(baseline.lines, [4, 8]);

        // One duplicate fixed, the other moved down a few lines, and a new finding.
        let mut new = report(vec![
            finding(DoctorLint::PluginClassMissing, Some(di), 25, "plugin gone"),
            finding(DoctorLint::ModuleNotRegistered, None, 0, "module drift"),
            finding(DoctorLint::PluginClassMissing, Some(di), 30, "another plugin gone"),
        ]);
        new.apply_baseline(&baseline, root);
        let messages: Vec<_> = new.findings.iter().map(|f| f.message.as_str()).collect();
        assert_eq!(messages, ["another plugin gone"]);
        let outcome = new.baseline.unwrap();
        assert_eq!(outcome.suppressed, 2);
        let [stale] = &outcome.unmatched[..] else { panic!("{:?}", outcome.unmatched) };
        assert_eq!((stale.entry.code.as_str(), stale.entry.count), ("E2005", 2));
        assert_eq!((stale.index, stale.line, stale.matched), (1, 8, 1));

        // Pruning lowers that entry to what it matched and leaves the other alone…
        let mut pruned = baseline.clone();
        pruned.prune(&outcome.unmatched);
        let counts: Vec<_> = pruned.ignore.iter().map(|e| (e.code.as_str(), e.count)).collect();
        assert_eq!(counts, [("E1002", 1), ("E2005", 1)]);
        // …and removes an entry that matched nothing.
        let mut pruned = baseline.clone();
        pruned.prune(&[StaleEntry { matched: 0, ..stale.clone() }]);
        let codes: Vec<_> = pruned.ignore.iter().map(|e| e.code.as_str()).collect();
        assert_eq!(codes, ["E1002"]);

        assert!(DoctorBaseline::parse(r#"{"version": 2, "ignore": []}"#).is_err());
    }
}
