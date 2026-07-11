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
    /// Present in both, byte-identical.
    pub identical: usize,
}

impl CompareReport {
    /// True when the output reproduces the archive exactly.
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.changed.is_empty()
    }

    /// Total number of files in the archive (the denominator for progress).
    pub fn archive_total(&self) -> usize {
        self.missing.len() + self.changed.len() + self.identical
    }
}

/// Compare `output` against the ground-truth `archive`.
///
/// The archive must exist ([`Error::ArchiveMissing`] otherwise). A
/// nonexistent output directory is treated as empty — the honest state of a
/// codebase where nothing has been generated yet, and the M0 acceptance case.
pub fn compare_dirs(archive: &Path, output: &Path) -> Result<CompareReport> {
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

        let report = compare_dirs(archive.path(), output.path()).unwrap();
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
            compare_dirs(archive.path(), Path::new("/nonexistent/magecommand-test")).unwrap();
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

        let report = compare_dirs(archive.path(), output.path()).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.identical, 1);
    }

    #[test]
    fn missing_archive_errors() {
        let output = tempfile::tempdir().unwrap();
        let err = compare_dirs(Path::new("/nonexistent/magecommand-archive"), output.path());
        assert!(matches!(err, Err(Error::ArchiveMissing(_))));
    }

    #[test]
    fn same_length_different_content_is_changed() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        write(archive.path(), "a.php", "aaaa");
        write(output.path(), "a.php", "aaab");

        let report = compare_dirs(archive.path(), output.path()).unwrap();
        assert_eq!(report.changed, vec!["a.php"]);
    }
}
