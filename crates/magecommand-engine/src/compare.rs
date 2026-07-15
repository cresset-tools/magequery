//! Archive comparison: a generated output tree vs the archived ground truth.
//!
//! Byte-level by design for M0 — "changed" means "not byte-identical". The
//! normalized comparable-metadata diffing (ordering-insensitive) arrives with
//! the metadata emitters in M2, layered on top of this, never replacing it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Outcome of comparing an output tree against an archive tree.
///
/// Paths are `/`-separated, relative to their tree root, sorted.
#[derive(Debug, Default, serde::Serialize)]
#[non_exhaustive]
pub struct CompareReport {
    /// In the archive but absent from the output: not yet generated.
    pub missing: Vec<String>,
    /// In the output but absent from the archive: invented files.
    pub extra: Vec<String>,
    /// Present in both, content differs.
    pub changed: Vec<String>,
    /// Present in both, not byte-identical but identical apart from the order of
    /// an interceptor's method blocks — behaviorally the same file. Only ever
    /// populated in lenient (non-`strict_ordering`) mode; treated as clean.
    pub reordered: Vec<String>,
    /// Present in both, byte-identical.
    pub identical: usize,
}

impl CompareReport {
    /// True when the output reproduces the archive exactly. Method-order-only
    /// differences ([`Self::reordered`]) are behaviorally identical and do not
    /// make the report unclean; under `strict_ordering` they land in
    /// [`Self::changed`] instead and this returns false.
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.changed.is_empty()
    }

    /// Total number of files in the archive (the denominator for progress).
    pub fn archive_total(&self) -> usize {
        self.missing.len() + self.changed.len() + self.reordered.len() + self.identical
    }
}

/// Compare `output` against the ground-truth `archive`.
///
/// The archive must exist ([`Error::ArchiveMissing`] otherwise). A
/// nonexistent output directory is treated as empty — the honest state of a
/// codebase where nothing has been generated yet, and the M0 acceptance case.
///
/// When `strict_ordering` is false, files that differ only in the order of an
/// interceptor's method blocks are reported as [`CompareReport::reordered`]
/// (behaviorally identical) rather than `changed`. Method order in a PHP class
/// is semantically irrelevant, and PHP's `getMethods()` reflection order — which
/// the interceptor generator follows — differs across PHP versions (8.4 vs 8.5)
/// for trait-using classes. Pass `strict_ordering = true` to require exact bytes.
pub fn compare_dirs(archive: &Path, output: &Path, strict_ordering: bool) -> Result<CompareReport> {
    if !archive.is_dir() {
        return Err(Error::ArchiveMissing(archive.to_path_buf()));
    }
    let archived = collect_files(archive)?;
    let produced = if output.is_dir() {
        collect_files(output)?
    } else {
        BTreeMap::new()
    };

    let mut report = CompareReport::default();
    for (rel, a_path) in &archived {
        match produced.get(rel) {
            None => report.missing.push(rel.clone()),
            Some(o_path) => {
                if same_content(a_path, o_path)? {
                    report.identical += 1;
                } else if !strict_ordering && same_modulo_ordering(a_path, o_path)? {
                    report.reordered.push(rel.clone());
                } else {
                    report.changed.push(rel.clone());
                }
            }
        }
    }
    for rel in produced.keys() {
        if !archived.contains_key(rel) {
            report.extra.push(rel.clone());
        }
    }
    Ok(report)
}

/// Recursively collect files under `root` as relative-path → absolute-path.
/// BTreeMap keeps the report deterministic.
fn collect_files(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))? {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if file_type.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("entry is under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(rel, path);
            }
        }
    }
    Ok(files)
}

fn same_content(a: &Path, b: &Path) -> Result<bool> {
    let len_a = fs::metadata(a).map_err(|e| Error::io(a, e))?.len();
    let len_b = fs::metadata(b).map_err(|e| Error::io(b, e))?.len();
    if len_a != len_b {
        return Ok(false);
    }
    let bytes_a = fs::read(a).map_err(|e| Error::io(a, e))?;
    let bytes_b = fs::read(b).map_err(|e| Error::io(b, e))?;
    Ok(bytes_a == bytes_b)
}

/// True when two files are byte-identical after canonicalizing the order of a
/// generated interceptor's method blocks — i.e. they differ only in method
/// sequence and are behaviorally the same PHP class.
fn same_modulo_ordering(a: &Path, b: &Path) -> Result<bool> {
    let bytes_a = fs::read(a).map_err(|e| Error::io(a, e))?;
    let bytes_b = fs::read(b).map_err(|e| Error::io(b, e))?;
    Ok(match (canonical_method_order(&bytes_a), canonical_method_order(&bytes_b)) {
        (Some(ca), Some(cb)) => ca == cb,
        // A file that is not a recognizable interceptor never matches modulo
        // ordering — genuine content changes are never masked.
        _ => false,
    })
}

/// Canonicalize a generated `Interceptor.php` by sorting its method blocks into
/// a stable order. Returns `None` for anything that is not an interceptor-shaped
/// class (so the caller falls back to strict byte comparison).
///
/// A Magento interceptor has a rigid layout: a header ending in the trait `use`,
/// then blank-line-separated method blocks (`__construct` first), then a lone
/// closing `}`. Reordering those blocks is a semantic no-op, so two files with
/// the same header, footer and *multiset* of blocks are equivalent.
fn canonical_method_order(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    // Only interceptors: the marker is unambiguous and present in every one.
    if !text.contains("implements \\Magento\\Framework\\Interception\\InterceptorInterface") {
        return None;
    }
    let trailing_nl = text.ends_with('\n');
    // Strip the class-closing "}" (and its trailing newline) to isolate the body.
    let body = text.strip_suffix('\n').unwrap_or(text).strip_suffix("\n}")?;
    // Blocks are blank-line separated; generated method bodies contain no blank
    // lines, so this split is exact for the interceptor template.
    let mut blocks: Vec<&str> = body.split("\n\n").collect();
    // Need a header plus at least two method blocks for order to be meaningful.
    if blocks.len() < 3 {
        return None;
    }
    let header = blocks.remove(0);
    blocks.sort_unstable();
    let mut out = String::with_capacity(text.len());
    out.push_str(header);
    for block in &blocks {
        out.push_str("\n\n");
        out.push_str(block);
    }
    out.push_str("\n}");
    if trailing_nl {
        out.push('\n');
    }
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn classifies_missing_extra_changed_identical() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        write(archive.path(), "a.php", "same");
        write(archive.path(), "sub/b.php", "old");
        write(archive.path(), "only-archived.php", "x");
        write(output.path(), "a.php", "same");
        write(output.path(), "sub/b.php", "new");
        write(output.path(), "invented.php", "y");

        let report = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(report.identical, 1);
        assert_eq!(report.changed, vec!["sub/b.php"]);
        assert_eq!(report.missing, vec!["only-archived.php"]);
        assert_eq!(report.extra, vec!["invented.php"]);
        assert!(!report.is_clean());
        assert_eq!(report.archive_total(), 3);
    }

    #[test]
    fn nonexistent_output_is_all_missing() {
        let archive = tempfile::tempdir().unwrap();
        write(archive.path(), "a.php", "x");
        write(archive.path(), "b/c.php", "y");

        let report =
            compare_dirs(archive.path(), Path::new("/nonexistent/magecommand-test"), false)
                .unwrap();
        assert_eq!(report.missing.len(), 2);
        assert_eq!(report.identical, 0);
        assert!(report.extra.is_empty());
    }

    #[test]
    fn identical_trees_are_clean() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        write(archive.path(), "m/global.php", "<?php return [];");
        write(output.path(), "m/global.php", "<?php return [];");

        let report = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.identical, 1);
    }

    #[test]
    fn missing_archive_errors() {
        let output = tempfile::tempdir().unwrap();
        let err =
            compare_dirs(Path::new("/nonexistent/magecommand-archive"), output.path(), false);
        assert!(matches!(err, Err(Error::ArchiveMissing(_))));
    }

    #[test]
    fn same_length_different_content_is_changed() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        write(archive.path(), "a.php", "aaaa");
        write(output.path(), "a.php", "aaab");

        let report = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(report.changed, vec!["a.php"]);
    }

    /// A minimal interceptor with `methods` emitted in the given order.
    fn interceptor(methods: &[&str]) -> String {
        let mut s = String::from(
            "<?php\nnamespace Foo;\n\n/**\n * Interceptor class for @see \\Foo\\Bar\n */\n\
             class Interceptor extends \\Foo\\Bar implements \
             \\Magento\\Framework\\Interception\\InterceptorInterface\n{\n\
             \x20   use \\Magento\\Framework\\Interception\\Interceptor;\n\n\
             \x20   public function __construct()\n    {\n        $this->___init();\n    }",
        );
        for m in methods {
            s.push_str(&format!(
                "\n\n    /**\n     * {{@inheritdoc}}\n     */\n    public function {m}()\n    {{\n\
                 \x20       $pluginInfo = $this->pluginList->getNext($this->subjectType, '{m}');\n\
                 \x20       return $pluginInfo ? $this->___callPlugins('{m}', func_get_args(), \
                 $pluginInfo) : parent::{m}();\n    }}"
            ));
        }
        s.push_str("\n}\n");
        s
    }

    #[test]
    fn method_reorder_is_reordered_not_changed_when_lenient() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        write(archive.path(), "I/Interceptor.php", &interceptor(&["alpha", "beta"]));
        write(output.path(), "I/Interceptor.php", &interceptor(&["beta", "alpha"]));

        let lenient = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(lenient.reordered, vec!["I/Interceptor.php"]);
        assert!(lenient.changed.is_empty());
        assert!(lenient.is_clean());
        assert_eq!(lenient.archive_total(), 1);

        let strict = compare_dirs(archive.path(), output.path(), true).unwrap();
        assert_eq!(strict.changed, vec!["I/Interceptor.php"]);
        assert!(strict.reordered.is_empty());
        assert!(!strict.is_clean());
    }

    #[test]
    fn different_method_body_is_changed_even_when_lenient() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        // Same method names, but the output drops one method → not a reorder.
        write(archive.path(), "I/Interceptor.php", &interceptor(&["alpha", "beta"]));
        write(output.path(), "I/Interceptor.php", &interceptor(&["alpha", "gamma"]));

        let report = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(report.changed, vec!["I/Interceptor.php"]);
        assert!(report.reordered.is_empty());
    }

    #[test]
    fn non_interceptor_reorder_is_still_changed() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        // A plain PHP file with the same blocks in a different order must NOT be
        // treated as a benign reorder — only interceptors get that treatment.
        write(archive.path(), "x.php", "<?php\n\nA;\n\nB;\n");
        write(output.path(), "x.php", "<?php\n\nB;\n\nA;\n");

        let report = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(report.changed, vec!["x.php"]);
        assert!(report.reordered.is_empty());
    }
}
