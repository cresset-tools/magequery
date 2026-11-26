//! A doctor run that decides pass or fail: `magequery doctor`'s reporting flags
//! as a value, for a caller that gates on the result (magebuild's `[doctor]`).

use std::path::PathBuf;

use super::{DoctorBaseline, DoctorLint, DoctorReport, ModuleSource, StaleEntry};

/// Which baseline a check reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BaselineChoice {
    /// `magequery-baseline.json` at the Magento root, when it exists.
    #[default]
    Default,
    /// This file, which must exist.
    File(PathBuf),
    /// None: every finding counts.
    Ignore,
}

/// How a check reads doctor's findings: `magequery doctor`'s `--source`,
/// `--deny`, `--in`, baseline and `--fail-on-warnings`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorPolicy {
    /// Only findings declared in this part of the project.
    pub source: Option<ModuleSource>,
    /// Lints whose findings count as errors.
    pub deny: Vec<DoctorLint>,
    /// Only findings located in one of these ([`DoctorReport::retain_in`]).
    pub in_scopes: Vec<String>,
    pub baseline: BaselineChoice,
    /// Warnings fail the check too, not only errors.
    pub fail_on_warnings: bool,
}

/// The result of [`Magento::doctor_check`](crate::Magento::doctor_check).
#[derive(Debug, Clone)]
pub struct DoctorCheck {
    /// The findings that count, the baseline's and the scope's already applied.
    pub report: DoctorReport,
    /// The baseline read and the file it came from, when one was.
    pub baseline: Option<(DoctorBaseline, PathBuf)>,
    fail_on_warnings: bool,
    /// The Magento root, which places findings without a file of their own.
    pub(super) root: PathBuf,
}

impl DoctorCheck {
    /// Baseline entries that accept more findings than occur. Each is an error:
    /// it would hide its finding if that came back. Empty in a narrowed run,
    /// which does not look at the rest of the project.
    pub fn stale(&self) -> &[StaleEntry] {
        self.report.baseline.as_ref().map_or(&[], |b| &b.unmatched)
    }

    /// Errors, stale baseline entries included.
    pub fn errors(&self) -> usize {
        self.report.errors() + self.stale().len()
    }

    /// Whether the check fails: on any error, and on a warning when the policy
    /// says so. Infos never fail it.
    pub fn failed(&self) -> bool {
        self.errors() > 0 || (self.fail_on_warnings && self.report.warnings() > 0)
    }
}

impl crate::Magento {
    /// Run doctor and read its findings by `policy`, as `magequery doctor` does
    /// (short of writing or pruning a baseline). An error is a baseline that
    /// cannot be read, or a named one that does not exist.
    pub fn doctor_check(&self, policy: &DoctorPolicy) -> Result<DoctorCheck, String> {
        let root = self.root();
        let mut report = self.doctor(policy.source);
        report.deny(&policy.deny);
        report.retain_in(root, &policy.in_scopes);
        let path = match &policy.baseline {
            BaselineChoice::Default => Some(root.join(DoctorBaseline::FILE)),
            BaselineChoice::File(path) => Some(path.clone()),
            BaselineChoice::Ignore => None,
        };
        let mut baseline = None;
        if let Some(path) = path {
            match DoctorBaseline::read(&path)? {
                Some(read) => {
                    report.apply_baseline(&read, root);
                    baseline = Some((read, path));
                }
                // Only the default file is optional.
                None if policy.baseline != BaselineChoice::Default => {
                    return Err(format!("baseline {} does not exist", path.display()));
                }
                None => {}
            }
        }
        // In a narrowed run, accepted findings outside the scope just weren't looked at.
        let scoped = policy.source.is_some() || !policy.in_scopes.is_empty();
        if let Some(outcome) = report.baseline.as_mut().filter(|_| scoped) {
            outcome.unmatched.clear();
        }
        Ok(DoctorCheck { report, baseline, fail_on_warnings: policy.fail_on_warnings, root: root.to_path_buf() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Magento;
    use std::path::Path;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// A store whose one finding is a warning: module Acme_Loose is on disk
    /// but not in config.php (E1002).
    fn store() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "app/etc/config.php", "<?php\nreturn ['modules' => []];\n");
        write(
            dir.path(),
            "app/code/Acme/Loose/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Loose\"/></config>\n",
        );
        write(
            dir.path(),
            "app/code/Acme/Loose/registration.php",
            "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\
             \\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, 'Acme_Loose', __DIR__);\n",
        );
        dir
    }

    fn check(dir: &tempfile::TempDir, policy: DoctorPolicy) -> DoctorCheck {
        Magento::open(dir.path()).unwrap().doctor_check(&policy).unwrap()
    }

    #[test]
    fn a_warning_fails_only_when_asked() {
        let dir = store();
        let default = check(&dir, DoctorPolicy::default());
        assert_eq!(default.report.warnings(), 1);
        assert!(!default.failed());
        assert!(check(&dir, DoctorPolicy { fail_on_warnings: true, ..Default::default() }).failed());
    }

    #[test]
    fn a_denied_lint_fails() {
        let dir = store();
        let denied = check(&dir, DoctorPolicy { deny: vec![DoctorLint::ModuleNotRegistered], ..Default::default() });
        assert_eq!(denied.report.errors(), 1);
        assert!(denied.failed());
    }

    #[test]
    fn findings_outside_the_scope_do_not_count() {
        let dir = store();
        let elsewhere = DoctorPolicy { in_scopes: vec!["Other".into()], fail_on_warnings: true, ..Default::default() };
        assert!(!check(&dir, elsewhere).failed());
        let here = DoctorPolicy { in_scopes: vec!["Acme".into()], fail_on_warnings: true, ..Default::default() };
        assert!(check(&dir, here).failed());
    }

    #[test]
    fn the_default_baseline_accepts_its_findings() {
        let dir = store();
        let report = Magento::open(dir.path()).unwrap().doctor(None);
        write(dir.path(), DoctorBaseline::FILE, &DoctorBaseline::from_report(&report, dir.path()).to_json());
        let strict = DoctorPolicy { fail_on_warnings: true, ..Default::default() };
        let accepted = check(&dir, strict);
        assert_eq!(accepted.report.warnings(), 0, "the baseline hides it");
        assert!(!accepted.failed());
        let ignored = check(&dir, DoctorPolicy { baseline: BaselineChoice::Ignore, fail_on_warnings: true, ..Default::default() });
        assert!(ignored.failed());
    }

    #[test]
    fn a_stale_baseline_entry_fails() {
        let dir = store();
        let report = Magento::open(dir.path()).unwrap().doctor(None);
        write(dir.path(), DoctorBaseline::FILE, &DoctorBaseline::from_report(&report, dir.path()).to_json());
        // Fix the finding: register the module.
        write(dir.path(), "app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Loose' => 1]];\n");
        let stale = check(&dir, DoctorPolicy::default());
        assert_eq!(stale.stale().len(), 1);
        assert!(stale.failed(), "a stale entry would hide the finding if it came back");
    }

    #[test]
    fn a_named_baseline_must_exist() {
        let dir = store();
        let missing = DoctorPolicy { baseline: BaselineChoice::File(dir.path().join("nope.json")), ..Default::default() };
        let err = Magento::open(dir.path()).unwrap().doctor_check(&missing).unwrap_err();
        assert!(err.contains("nope.json"), "{err}");
    }
}
