//! `magecommand static verify` — diff a deployed static tree against a real
//! `setup:static-content:deploy` reference, the oracle harness the DI half has
//! had since M0 (`magecommand di verify`) and this half never did.
//!
//! ## Why this exists
//!
//! The unit and fixture suites are green whether or not the deploy is faithful:
//! they exercise pure functions, not a whole package. Every parity defect found
//! so far — `sri-hashes.json` written per package instead of per area, language
//! packs missing from the js dictionary, PHP float printing, `GLOB_NOSORT`
//! registration order, a Hyvä plugin dropping `tailwind/` — was invisible to
//! them and visible only in a full-tree diff against a real deploy. This turns
//! that diff into a command, so the parity is reproducible and can gate CI.
//!
//! ## Shape
//!
//! Deliberately the same as `di verify`: compare two trees that already exist,
//! rather than running a deploy. The caller deploys with `--out <DIR>` and
//! points `--reference` at the real `pub/static`. That keeps verification
//! independent of a live store, and lets a captured reference tree be archived
//! and re-used.
//!
//! ## Buckets
//!
//! Files are grouped into the deploy's own units — `<area>/<Vendor>/<theme>/
//! <locale>` packages, plus the AREA-level artifacts (`sri-hashes.json`) and
//! the run-level `deployed_version.txt` — because "12 files differ" is not
//! actionable while "every file under one theme differs" names the bug.
//!
//! `deployed_version.txt` holds a per-run timestamp, so it is never a
//! difference worth reporting and is excluded from the comparison entirely.
//!
//! A changed `.css` is re-checked with [`super::cssdiff`]: one that differs only
//! in non-semantic formatting is reported as `equivalent`, not `changed`. That
//! mirrors `di verify`'s `reordered` bucket — a real difference in bytes, but
//! not one that changes behavior, so it does not make the report unclean unless
//! `--strict` says byte-exact.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Written once per run with a timestamp value, so it can never match and says
/// nothing about fidelity.
const RUN_SCOPED: &str = "deployed_version.txt";

/// Which deploy unit a file belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bucket {
    /// A theme package: `<area>/<Vendor>/<theme>/<locale>`.
    Package(String),
    /// An area-level artifact (`<area>/sri-hashes.json`).
    Area(String),
    /// Anything outside both shapes — reported rather than silently pooled.
    Other,
}

impl Bucket {
    /// Classify a static-root-relative path.
    fn of(rel: &str) -> Bucket {
        let seg: Vec<&str> = rel.split('/').collect();
        // `<area>/<Vendor>/<theme>/<locale>/…` — a package needs a file under it.
        if seg.len() > 4 {
            return Bucket::Package(seg[..4].join("/"));
        }
        // `<area>/<file>` — an area-level artifact.
        if seg.len() == 2 {
            return Bucket::Area(seg[0].to_string());
        }
        Bucket::Other
    }

    /// The label used in reports.
    pub fn label(&self) -> String {
        match self {
            Bucket::Package(p) => p.clone(),
            Bucket::Area(a) => format!("{a} (area artifacts)"),
            Bucket::Other => "(unclassified)".to_string(),
        }
    }
}

/// One bucket's tally.
#[derive(Debug, Default, Clone)]
pub struct BucketReport {
    /// In the reference, absent from ours.
    pub missing: Vec<String>,
    /// In ours, absent from the reference — invented files.
    pub extra: Vec<String>,
    /// Present in both, bytes differ, and the difference is real.
    pub changed: Vec<String>,
    /// Present in both, bytes differ, but a semantic CSS diff finds nothing —
    /// formatting only. Clean unless `strict`.
    pub equivalent: Vec<String>,
    /// Present in both, byte-identical.
    pub identical: usize,
}

impl BucketReport {
    /// Files the reference has in this bucket.
    pub fn reference_total(&self) -> usize {
        self.identical + self.changed.len() + self.equivalent.len() + self.missing.len()
    }

    fn is_clean(&self, strict: bool) -> bool {
        self.missing.is_empty()
            && self.extra.is_empty()
            && self.changed.is_empty()
            && (!strict || self.equivalent.is_empty())
    }
}

/// The whole comparison, bucket by bucket in path order.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub buckets: Vec<(Bucket, BucketReport)>,
    /// Packages the reference has that the output does not contain at all —
    /// i.e. simply not part of this deploy. Reported, never counted as
    /// missing files, unless the caller asked for full coverage.
    pub not_deployed: Vec<Bucket>,
    /// Byte-exact required (`equivalent` counts as a difference).
    pub strict: bool,
}

impl VerifyReport {
    /// Sum across buckets.
    pub fn totals(&self) -> BucketReport {
        let mut t = BucketReport::default();
        for (_, b) in &self.buckets {
            t.missing.extend(b.missing.iter().cloned());
            t.extra.extend(b.extra.iter().cloned());
            t.changed.extend(b.changed.iter().cloned());
            t.equivalent.extend(b.equivalent.iter().cloned());
            t.identical += b.identical;
        }
        t
    }

    /// Does our tree reproduce the reference?
    pub fn is_clean(&self) -> bool {
        self.buckets.iter().all(|(_, b)| b.is_clean(self.strict))
    }
}

/// Errors the comparison itself can raise (a missing tree, unreadable dir).
pub type VerifyError = super::less::LessDeployError;

fn err(message: String) -> VerifyError {
    VerifyError { entry: None, module: None, file: None, message }
}

/// Every file under `root`, as static-root-relative path → absolute path.
/// `BTreeMap` keeps the report deterministic regardless of readdir order.
fn collect(root: &Path) -> Result<BTreeMap<String, PathBuf>, VerifyError> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let rd = std::fs::read_dir(&dir)
            .map_err(|e| err(format!("read dir {}: {e}", dir.display())))?;
        for entry in rd {
            let entry = entry.map_err(|e| err(format!("read dir {}: {e}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let ty = entry
                .file_type()
                .map_err(|e| err(format!("stat {}: {e}", entry.path().display())))?;
            if ty.is_dir() {
                stack.push((entry.path(), rel));
            } else if rel != RUN_SCOPED {
                out.insert(rel, entry.path());
            }
        }
    }
    Ok(out)
}

/// Are two files byte-identical? Compares length first, then contents.
fn same_bytes(a: &Path, b: &Path) -> Result<bool, VerifyError> {
    let (ma, mb) = (
        std::fs::metadata(a).map_err(|e| err(format!("stat {}: {e}", a.display())))?,
        std::fs::metadata(b).map_err(|e| err(format!("stat {}: {e}", b.display())))?,
    );
    if ma.len() != mb.len() {
        return Ok(false);
    }
    let (ca, cb) = (
        std::fs::read(a).map_err(|e| err(format!("read {}: {e}", a.display())))?,
        std::fs::read(b).map_err(|e| err(format!("read {}: {e}", b.display())))?,
    );
    Ok(ca == cb)
}

/// Does a changed `.css` differ only in non-semantic formatting? Non-UTF-8 or
/// non-css files are never equivalent.
fn css_equivalent(rel: &str, a: &Path, b: &Path) -> bool {
    if !rel.ends_with(".css") {
        return false;
    }
    let (Ok(x), Ok(y)) = (std::fs::read(a), std::fs::read(b)) else {
        return false;
    };
    let (Ok(x), Ok(y)) = (String::from_utf8(x), String::from_utf8(y)) else {
        return false;
    };
    super::cssdiff::diff(&x, &y).is_clean()
}

/// Compare our deployed tree against a real-deploy reference.
///
/// Both paths are static ROOTS (the `pub/static` equivalent), so the relative
/// paths carry `<area>/<Vendor>/<theme>/<locale>/…` and bucket naturally.
/// `require_full_coverage` decides what a package present only in the
/// reference means. A partial deploy (one theme, one locale) is the normal
/// case — verifying it should compare what it produced, not drown the report
/// in thousands of files belonging to packages the caller never asked for. So
/// by default such packages are listed as `not_deployed` and skipped; with
/// full coverage they count as missing, which is what verifying a COMPLETE
/// deploy means.
pub fn verify(
    reference: &Path,
    output: &Path,
    strict: bool,
    require_full_coverage: bool,
) -> Result<VerifyReport, VerifyError> {
    if !reference.is_dir() {
        return Err(err(format!("reference is not a directory: {}", reference.display())));
    }
    if !output.is_dir() {
        return Err(err(format!("output is not a directory: {}", output.display())));
    }
    let want = collect(reference)?;
    let got = collect(output)?;

    // Which packages does the output actually contain?
    let produced: BTreeSet<Bucket> = got.keys().map(|r| Bucket::of(r)).collect();

    let mut buckets: BTreeMap<Bucket, BucketReport> = BTreeMap::new();
    let mut not_deployed: BTreeSet<Bucket> = BTreeSet::new();
    for (rel, a) in &want {
        let bucket = Bucket::of(rel);
        if !require_full_coverage && !produced.contains(&bucket) {
            not_deployed.insert(bucket);
            continue;
        }
        let b_entry = buckets.entry(bucket).or_default();
        match got.get(rel) {
            None => b_entry.missing.push(rel.clone()),
            Some(b) => {
                if same_bytes(a, b)? {
                    b_entry.identical += 1;
                } else if css_equivalent(rel, a, b) {
                    b_entry.equivalent.push(rel.clone());
                } else {
                    b_entry.changed.push(rel.clone());
                }
            }
        }
    }
    let known: BTreeSet<&String> = want.keys().collect();
    for rel in got.keys() {
        if !known.contains(rel) {
            buckets.entry(Bucket::of(rel)).or_default().extra.push(rel.clone());
        }
    }

    Ok(VerifyReport {
        buckets: buckets.into_iter().collect(),
        not_deployed: not_deployed.into_iter().collect(),
        strict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// Paths bucket into the deploy's own units, so a report names the theme
    /// rather than listing loose files.
    #[test]
    fn paths_bucket_by_package_and_area() {
        assert_eq!(
            Bucket::of("frontend/Magento/blank/nl_NL/css/styles-m.css"),
            Bucket::Package("frontend/Magento/blank/nl_NL".into())
        );
        assert_eq!(
            Bucket::of("frontend/Magento/blank/nl_NL/Magento_Ui/js/x.js"),
            Bucket::Package("frontend/Magento/blank/nl_NL".into())
        );
        assert_eq!(Bucket::of("frontend/sri-hashes.json"), Bucket::Area("frontend".into()));
        assert_eq!(Bucket::of("stray.txt"), Bucket::Other);
    }

    /// An identical pair of trees is clean, and `deployed_version.txt` is
    /// ignored on both sides — its value is a per-run timestamp.
    #[test]
    fn identical_trees_are_clean_and_ignore_the_run_version() {
        let td = tempfile::tempdir().unwrap();
        let (a, b) = (td.path().join("ref"), td.path().join("out"));
        for root in [&a, &b] {
            w(root, "frontend/Magento/blank/nl_NL/css/x.css", ".a{color:red}");
            w(root, "frontend/sri-hashes.json", "{}");
        }
        w(&a, RUN_SCOPED, "1784729141");
        w(&b, RUN_SCOPED, "9999999999");

        let r = verify(&a, &b, false, false).unwrap();
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(r.totals().identical, 2);
    }

    /// Missing, extra and changed files land in their own package's bucket.
    #[test]
    fn differences_are_reported_under_their_package() {
        let td = tempfile::tempdir().unwrap();
        let (a, b) = (td.path().join("ref"), td.path().join("out"));
        w(&a, "frontend/Magento/blank/nl_NL/css/x.css", ".a{color:red}");
        w(&a, "frontend/Magento/blank/nl_NL/js/gone.js", "//");
        w(&b, "frontend/Magento/blank/nl_NL/css/x.css", ".a{color:blue}");
        w(&b, "frontend/Magento/blank/nl_NL/js/new.js", "//");

        let r = verify(&a, &b, false, false).unwrap();
        assert!(!r.is_clean());
        let (bucket, rep) = &r.buckets[0];
        assert_eq!(bucket.label(), "frontend/Magento/blank/nl_NL");
        assert_eq!(rep.changed, vec!["frontend/Magento/blank/nl_NL/css/x.css"]);
        assert_eq!(rep.missing, vec!["frontend/Magento/blank/nl_NL/js/gone.js"]);
        assert_eq!(rep.extra, vec!["frontend/Magento/blank/nl_NL/js/new.js"]);
    }

    /// A css file differing only in formatting is `equivalent` — clean by
    /// default, a difference under `--strict`.
    #[test]
    fn formatting_only_css_is_equivalent_until_strict() {
        let td = tempfile::tempdir().unwrap();
        let (a, b) = (td.path().join("ref"), td.path().join("out"));
        w(&a, "frontend/Magento/blank/nl_NL/css/x.css", ".a {\n  color: #FF0000;\n}\n");
        w(&b, "frontend/Magento/blank/nl_NL/css/x.css", ".a{color:#ff0000}");

        let lenient = verify(&a, &b, false, false).unwrap();
        assert!(lenient.is_clean(), "{lenient:?}");
        assert_eq!(lenient.totals().equivalent.len(), 1);
        assert!(lenient.totals().changed.is_empty());

        let strict = verify(&a, &b, true, false).unwrap();
        assert!(!strict.is_clean(), "byte-exact mode must flag it");
    }

    /// A non-css file that differs is always `changed` — the semantic escape
    /// hatch is css-only, so a js or image difference is never explained away.
    #[test]
    fn non_css_differences_are_never_equivalent() {
        let td = tempfile::tempdir().unwrap();
        let (a, b) = (td.path().join("ref"), td.path().join("out"));
        w(&a, "frontend/Magento/blank/nl_NL/js/x.js", "var a = 1;");
        w(&b, "frontend/Magento/blank/nl_NL/js/x.js", "var a=1;");
        let r = verify(&a, &b, false, false).unwrap();
        assert_eq!(r.totals().changed.len(), 1);
        assert!(r.totals().equivalent.is_empty());
    }

    /// Verifying a PARTIAL deploy against a whole `pub/static` compares what
    /// was deployed and lists the rest as not-deployed — otherwise a one-theme
    /// check drowns in files belonging to themes nobody asked for. `--all`
    /// turns those back into missing, which is what verifying a COMPLETE
    /// deploy means.
    #[test]
    fn undeployed_packages_are_skipped_unless_full_coverage() {
        let td = tempfile::tempdir().unwrap();
        let (a, b) = (td.path().join("ref"), td.path().join("out"));
        w(&a, "frontend/Magento/blank/nl_NL/css/x.css", ".a{}");
        w(&a, "frontend/Magento/luma/nl_NL/css/y.css", ".b{}");
        w(&a, "frontend/Magento/luma/de_DE/css/y.css", ".b{}");
        w(&b, "frontend/Magento/blank/nl_NL/css/x.css", ".a{}");

        let partial = verify(&a, &b, false, false).unwrap();
        assert!(partial.is_clean(), "{partial:?}");
        assert_eq!(partial.totals().identical, 1);
        assert_eq!(partial.not_deployed.len(), 2, "both luma packages");
        assert!(partial.totals().missing.is_empty());

        let full = verify(&a, &b, false, true).unwrap();
        assert!(!full.is_clean(), "--all must demand every package");
        assert_eq!(full.totals().missing.len(), 2);
        assert!(full.not_deployed.is_empty());
    }

    /// A missing tree is an error, not an empty clean report — otherwise a
    /// typo'd path would read as "parity achieved".
    #[test]
    fn a_missing_tree_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let a = td.path().join("ref");
        w(&a, "frontend/Magento/blank/nl_NL/css/x.css", ".a{}");
        assert!(verify(&a, &td.path().join("nope"), false, false).is_err());
        assert!(verify(&td.path().join("nope"), &a, false, false).is_err());
    }
}
