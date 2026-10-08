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
                if !is_generated_artifact(&rel) {
                    continue;
                }
                files.insert(rel, path);
            }
        }
    }
    Ok(files)
}

/// Whether a path under a `generated/` tree is compile OUTPUT, and so belongs in
/// the diff at all.
///
/// Two things live in `generated/` that `setup:di:compile` does not produce, and
/// counting them as differences is noise the reader has to learn to ignore:
/// Magento's own `generated/.htaccess` (shipped by the installer — a compile
/// into a freshly emptied directory simply never recreates it) and magecommand's
/// `.mqcache/` bookkeeping (the incremental-compile manifest). Both showed up as
/// unexplained `extra` entries whenever a whole `generated/` tree was compared
/// rather than its `code/` and `metadata/` halves.
fn is_generated_artifact(rel: &str) -> bool {
    rel != ".htaccess" && !rel.starts_with(".mqcache/") && !rel.contains("/.mqcache/")
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

/// True when two files are byte-identical after canonicalizing the block order of
/// a generated interceptor or proxy — i.e. they differ only in method sequence and
/// are behaviorally the same PHP class.
pub(crate) fn same_modulo_ordering(a: &Path, b: &Path) -> Result<bool> {
    let bytes_a = fs::read(a).map_err(|e| Error::io(a, e))?;
    let bytes_b = fs::read(b).map_err(|e| Error::io(b, e))?;
    // A plugin-list cache is a different shape from a generated class, and its
    // top-level sections are lookup maps rather than ordered sequences.
    let is_plugin_list = a
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("plugin-list.php"));
    if is_plugin_list {
        return Ok(
            match (canonical_map_order(&bytes_a), canonical_map_order(&bytes_b)) {
                (Some(ca), Some(cb)) => ca == cb,
                _ => false,
            },
        );
    }
    Ok(match (canonical_method_order(&bytes_a), canonical_method_order(&bytes_b)) {
        (Some(ca), Some(cb)) => ca == cb,
        // A file that is not a recognizable interceptor or proxy never matches
        // modulo ordering — genuine content changes are never masked.
        _ => false,
    })
}

/// Canonicalize a `var_export`-shaped plugin-list cache by sorting the entries
/// within each top-level section. `None` for anything not of that shape, so
/// the caller falls back to strict bytes.
///
/// The three sections are `_data`, `_inherited` and `_processed`
/// (`PluginList.php:227`), and every read of them is a keyed lookup —
/// `isset($this->_inherited[$type])`, `array_key_exists`, `[$type][$code]`
/// (`PluginList.php:174,191`). None is ever iterated, so PHP array key order
/// carries no meaning and two files that differ only in it behave identically.
///
/// Entries are sorted as whole TEXT BLOCKS, key and value together, so the
/// canonical form depends on the multiset of (key, value) pairs: a changed
/// value, an added key or a removed one all defeat the match and stay
/// `changed`. This is the same trick `canonical_method_order` plays on
/// blank-line-separated method blocks.
pub(crate) fn canonical_map_order(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.starts_with("<?php return array (") {
        return None;
    }
    // An entry opens at exactly four spaces with a quoted or numeric key.
    let opens_entry = |l: &str| {
        l.strip_prefix("    ").is_some_and(|rest| {
            !rest.starts_with(' ')
                && (rest.starts_with('\'') || rest.starts_with(|c: char| c.is_ascii_digit()))
                && rest.contains(" => ")
        })
    };
    // An entry runs until its brackets balance AND it is comma-terminated:
    // `'k' => NULL,` closes on its opening line, while `'k' => ` followed by
    // `    array (` … `    ),` spans several — and that continuation sits at
    // the SAME indent as the opener, so indentation alone cannot delimit it.
    let depth_of = |l: &str| {
        let mut d = 0i32;
        let mut in_str = false;
        let mut prev = '\0';
        for c in l.chars() {
            match c {
                '\'' if prev != '\\' => in_str = !in_str,
                '(' | '[' if !in_str => d += 1,
                ')' | ']' if !in_str => d -= 1,
                _ => {}
            }
            prev = c;
        }
        d
    };
    let mut out: Vec<String> = Vec::new();
    let mut entries: Vec<Vec<String>> = Vec::new();
    let mut open_depth = 0i32;
    let mut in_entry = false;
    let mut saw_section = false;
    let flush = |entries: &mut Vec<Vec<String>>, out: &mut Vec<String>, saw: &mut bool| {
        if !entries.is_empty() {
            entries.sort();
            for e in entries.drain(..) {
                out.extend(e);
            }
            *saw = true;
        }
    };
    for line in text.lines() {
        if in_entry {
            entries.last_mut().expect("entry open").push(line.to_owned());
            open_depth += depth_of(line);
            if open_depth <= 0 && line.trim_end().ends_with(',') {
                in_entry = false;
            }
            continue;
        }
        if opens_entry(line) {
            entries.push(vec![line.to_owned()]);
            open_depth = depth_of(line);
            in_entry = !(open_depth <= 0 && line.trim_end().ends_with(','));
            continue;
        }
        flush(&mut entries, &mut out, &mut saw_section);
        out.push(line.to_owned());
    }
    flush(&mut entries, &mut out, &mut saw_section);
    // Nothing map-shaped in it — fall back to strict bytes rather than
    // declaring two unrelated files equivalent.
    if !saw_section {
        return None;
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    Some(joined.into_bytes())
}

/// Canonicalize a generated interceptor or proxy by sorting its blank-line
/// separated blocks into a stable order. Returns `None` for anything that is not
/// an interceptor- or proxy-shaped class (so the caller falls back to strict byte
/// comparison).
///
/// Both templates share a rigid layout: a header, then blank-line-separated
/// blocks (method bodies for interceptors; property declarations followed by
/// method bodies for proxies), then a lone closing `}`. Generated bodies contain
/// no blank lines, so the split is exact. Sorting the blocks yields a scrambled
/// but deterministic canonical form — two files produce the same bytes only when
/// they share the same header and the same *multiset* of blocks, i.e. they differ
/// solely in the order of methods (a reflection-order artifact) and are
/// behaviorally identical. Any genuine change to the class declaration, a
/// property, or a method body alters a block and defeats the match.
pub(crate) fn canonical_method_order(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    // Markers are unambiguous and present in every generated file of each kind.
    let is_interceptor =
        text.contains("implements \\Magento\\Framework\\Interception\\InterceptorInterface");
    let is_proxy =
        text.contains("implements \\Magento\\Framework\\ObjectManager\\NoninterceptableInterface");
    if !is_interceptor && !is_proxy {
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

    /// A plugin-list cache file whose `_inherited` section lists the same
    /// types in a different ORDER. That section is a lookup map — PluginList
    /// only ever reaches it by `isset`/`array_key_exists`/`[$type]`
    /// (PluginList.php:174,191) and never iterates it — so two such files are
    /// behaviorally identical and `di verify` should say `reordered`, not
    /// `changed`.
    ///
    /// Real case: a class implementing a miscased built-in (`implements
    /// \arrayaccess`) makes Magento append `Traversable` as a sibling of
    /// `Iterator` while we reach it by another route, putting the same key in
    /// a different position.
    fn plugin_list(order: &[&str]) -> String {
        let mut s = String::from("<?php return array (\n  0 => \n  array (\n  ),\n  1 => \n  array (\n");
        for t in order {
            s.push_str(&format!("    '{t}' => NULL,\n"));
        }
        s.push_str("  ),\n  2 => \n  array (\n  ),\n);\n");
        s
    }

    #[test]
    fn plugin_list_key_reorder_is_reordered_not_changed_when_lenient() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        // Real cache files join their scopes with `|`
        // (`frontend|global|primary|plugin-list.php`), which Windows will not
        // put in a filename — writing one here failed CI with
        // ERROR_INVALID_NAME. Detection keys on the `plugin-list.php` suffix,
        // so a `|`-free name exercises exactly the same path and keeps this
        // test running on every platform.
        let name = "metadata/frontend-global-primary-plugin-list.php";
        write(archive.path(), name, &plugin_list(&["ArrayAccess", "Countable", "Iterator", "Traversable"]));
        write(output.path(), name, &plugin_list(&["ArrayAccess", "Countable", "Iterator", "IteratorAggregate", "Traversable"]));

        // Different SET of keys is a genuine change, never masked.
        let differing = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(differing.changed, vec![name], "an added key must stay `changed`");

        // Same set, different order → reordered, and clean.
        write(output.path(), name, &plugin_list(&["ArrayAccess", "Countable", "Traversable", "Iterator"]));
        let lenient = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(lenient.reordered, vec![name]);
        assert!(lenient.changed.is_empty());
        assert!(lenient.is_clean());

        // Strict mode still demands exact bytes.
        let strict = compare_dirs(archive.path(), output.path(), true).unwrap();
        assert_eq!(strict.changed, vec![name]);
        assert!(strict.reordered.is_empty());
        assert!(!strict.is_clean());
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

    /// A minimal proxy with fixed properties and `methods` in the given order.
    fn proxy(methods: &[&str]) -> String {
        let mut s = String::from(
            "<?php\nnamespace Foo;\n\n/**\n * Proxy class for @see \\Foo\\Bar\n */\n\
             class Proxy extends \\Foo\\Bar implements \
             \\Magento\\Framework\\ObjectManager\\NoninterceptableInterface\n{\n\
             \x20   /**\n     * @var string\n     */\n    protected $_instanceName = null;\n\n\
             \x20   /**\n     * @var \\Foo\\Bar\n     */\n    protected $_subject = null;",
        );
        for m in methods {
            s.push_str(&format!(
                "\n\n    /**\n     * {{@inheritdoc}}\n     */\n    public function {m}()\n    {{\n\
                 \x20       return $this->_getSubject()->{m}();\n    }}"
            ));
        }
        s.push_str("\n}\n");
        s
    }

    #[test]
    fn proxy_method_reorder_is_reordered_not_changed_when_lenient() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        write(archive.path(), "P/Proxy.php", &proxy(&["alpha", "beta"]));
        write(output.path(), "P/Proxy.php", &proxy(&["beta", "alpha"]));

        let lenient = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(lenient.reordered, vec!["P/Proxy.php"]);
        assert!(lenient.changed.is_empty());
        assert!(lenient.is_clean());

        let strict = compare_dirs(archive.path(), output.path(), true).unwrap();
        assert_eq!(strict.changed, vec!["P/Proxy.php"]);
        assert!(strict.reordered.is_empty());
    }

    #[test]
    fn proxy_with_changed_body_is_changed_even_when_lenient() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        write(archive.path(), "P/Proxy.php", &proxy(&["alpha", "beta"]));
        write(output.path(), "P/Proxy.php", &proxy(&["alpha", "gamma"]));

        let report = compare_dirs(archive.path(), output.path(), false).unwrap();
        assert_eq!(report.changed, vec!["P/Proxy.php"]);
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

