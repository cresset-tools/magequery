//! Compiled-metadata emitters (`generated/metadata/*.php`), accepted
//! byte-for-byte against a `setup:di:compile` archive.
//!
//! First emitter: `app_action_list.php` — every class declared under any
//! module's `Controller/` tree, keyed by lowercased FQCN. Classes come from
//! actually parsing each file (magecommand-php), not from the path
//! convention, so namespace-diverging modules are handled.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use magequery_core::Magento;

use crate::phpexport::{PhpKey, PhpValue};
use crate::{Error, Result};

/// Build `app_action_list.php` exactly as Magento's
/// `Module\Dir\Reader::getActionFiles` + `AppActionListGenerator` do: for
/// every ENABLED module in module-list order, every leaf file under
/// `Controller/` becomes `<Module\Namespace>\Controller\<relative path minus
/// 4 chars>` — a pure PATH convention, never the parsed declaration (a file
/// whose namespace diverges from its module still gets the path-derived
/// name; byte parity means reproducing that faithfully). Keys are the
/// lowercased names, `ksort`ed; a later module's collision overwrites.
pub fn app_action_list(magento: &Magento) -> PhpValue {
    let mut actions: BTreeMap<String, String> = BTreeMap::new();
    for module in magento.modules() {
        if !module.enabled {
            continue;
        }
        let dir = module.path.join("Controller");
        if !dir.is_dir() {
            continue;
        }
        let namespace = module.name.as_str().replace('_', "\\");
        let mut files = Vec::new();
        collect_leaf_files(&dir, &mut files);
        for file in files {
            let rel = file
                .strip_prefix(&dir)
                .expect("file is under its Controller dir")
                .to_string_lossy()
                .replace('/', "\\");
            // Magento strips exactly 4 trailing chars (".php"), blindly.
            let stem = &rel[..rel.len().saturating_sub(4)];
            let action = format!("{namespace}\\Controller\\{stem}");
            actions.insert(action.to_ascii_lowercase(), action);
        }
    }
    PhpValue::Array(
        actions
            .into_iter()
            .map(|(k, v)| (PhpKey::Str(k), PhpValue::Str(v)))
            .collect(),
    )
}

/// Write one metadata file atomically (temp + rename). `force` allows
/// overwriting an existing file; without it an existing file is an error —
/// the caller decides what may be clobbered.
pub fn write_metadata_file(
    root: &Path,
    name: &str,
    content: &str,
    force: bool,
) -> Result<PathBuf> {
    let dir = root.join("generated/metadata");
    fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    let target = dir.join(name);
    if target.exists() && !force {
        return Err(Error::WouldOverwrite(target));
    }
    let tmp = dir.join(format!(".{name}.tmp"));
    fs::write(&tmp, content).map_err(|e| Error::io(&tmp, e))?;
    fs::rename(&tmp, &target).map_err(|e| Error::io(&target, e))?;
    Ok(target)
}

/// Write one `generated/code` file (`rel_path` is `Foo/Bar/Proxy.php`),
/// creating the intermediate directories. Same atomic temp+rename +
/// clobber-guard as [`write_metadata_file`].
pub fn write_code_file(
    root: &Path,
    rel_path: &str,
    content: &str,
    force: bool,
) -> Result<PathBuf> {
    let target = root.join("generated/code").join(rel_path);
    let dir = target
        .parent()
        .ok_or_else(|| Error::io(&target, std::io::Error::other("no parent dir")))?;
    fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    if target.exists() && !force {
        return Err(Error::WouldOverwrite(target));
    }
    let file_name = target.file_name().expect("target has a file name").to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.tmp"));
    fs::write(&tmp, content).map_err(|e| Error::io(&tmp, e))?;
    fs::rename(&tmp, &target).map_err(|e| Error::io(&target, e))?;
    Ok(target)
}

/// Write every `generated/code` file in parallel — the compile's bulk output
/// (~10k files on a large store). Returns the number written, or the first I/O
/// error.
///
/// Two deliberate departures from the atomic per-file [`write_code_file`]:
///
/// 1. **Direct write, no temp+rename.** `compile` clears `generated/code`
///    first (mirroring `setup:di:compile`), so the tree is already torn down
///    for the whole write phase — the per-file rename guards nothing at the
///    directory level, and nothing reads the tree mid-compile. A direct
///    `fs::write` is one syscall instead of open-tmp+write+rename, and matches
///    Magento's own `file_put_contents`. (A crash mid-compile leaves a partial
///    tree either way; you re-run compile.)
/// 2. **Each unique parent dir is created ONCE**, up front, instead of a
///    `create_dir_all` per file re-walking shared parent chains 10k+ times.
///
/// On macOS/APFS — where the old path measured ~165µs/file — this is the
/// dominant write-phase cost. `force=false` keeps a best-effort clobber guard;
/// the compile always passes `force=true`.
pub fn write_code_files(root: &Path, files: &[(String, String)], force: bool) -> Result<usize> {
    use rayon::prelude::*;
    use std::collections::HashSet;

    let base = root.join("generated/code");
    fs::create_dir_all(&base).map_err(|e| Error::io(&base, e))?;

    // Pre-create every distinct parent dir once (dedup). `create_dir_all` is
    // recursive, so creating each leaf dir covers its ancestors; concurrent
    // calls racing on a shared ancestor are safe (AlreadyExists is ignored).
    let dirs: Vec<PathBuf> = files
        .iter()
        .filter_map(|(rel, _)| Path::new(rel).parent())
        .filter(|p| !p.as_os_str().is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|p| base.join(p))
        .collect();
    dirs.par_iter()
        .try_for_each(|dir| fs::create_dir_all(dir).map_err(|e| Error::io(dir, e)))?;

    files.par_iter().try_for_each(|(rel, content)| -> Result<()> {
        let target = base.join(rel);
        if !force && target.exists() {
            return Err(Error::WouldOverwrite(target));
        }
        fs::write(&target, content).map_err(|e| Error::io(&target, e))
    })?;
    Ok(files.len())
}

/// Write every `generated/<rel>` file in `files` (keys relative to `generated/`,
/// e.g. `metadata/global.php` or `code/Magento/…/Interceptor.php`) — the whole
/// output of [`crate::build::compute_outputs`]. Parent dirs are pre-created once
/// (deduped), then the files are written in parallel. The compile's bulk write.
pub fn write_generated(root: &Path, files: &[(String, String)]) -> Result<usize> {
    use rayon::prelude::*;
    use std::collections::HashSet;

    let base = root.join("generated");
    let dirs: Vec<PathBuf> = files
        .iter()
        .filter_map(|(rel, _)| Path::new(rel).parent())
        .filter(|p| !p.as_os_str().is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|p| base.join(p))
        .collect();
    dirs.par_iter()
        .try_for_each(|dir| fs::create_dir_all(dir).map_err(|e| Error::io(dir, e)))?;

    files.par_iter().try_for_each(|(rel, content)| -> Result<()> {
        let target = base.join(rel);
        fs::write(&target, content).map_err(|e| Error::io(&target, e))
    })?;
    Ok(files.len())
}

/// Groups of output paths (dirs or files, relative to `generated/`) that
/// differ only in letter case. Harmless on a case-sensitive filesystem (both
/// spellings coexist as separate entries, exactly like on a Linux deploy) —
/// but on a case-INSENSITIVE one (macOS default) they collapse into a single
/// physical entry whose case the first writer decides, leaving the other
/// spelling's files at a path that no longer matches their declared
/// namespace. PSR-4 then can't autoload them on a case-sensitive host: the
/// classic "works on the Mac, 500s on the Linux deploy" trap. Real
/// `setup:di:compile` on macOS produces the very same collapse (verified on
/// proforto: 5 identical mismatches in its `_code` archive), so this is a
/// build-host hazard to WARN about, not an output difference to fix.
///
/// Each group lists the distinct spellings of one case-folded path; only the
/// SHORTEST colliding prefix is reported (children of a colliding dir also
/// differ textually, but the parent is the root cause). Sources of truth are
/// the INTENDED output paths, so the check is pure string work — no
/// filesystem access — and identical on every platform.
pub fn case_collisions(files: &[(String, String)]) -> Vec<Vec<String>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut spellings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (rel, _) in files {
        let mut prefix = String::new();
        for segment in rel.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            spellings.entry(prefix.to_lowercase()).or_default().insert(prefix.clone());
        }
    }
    let colliding: BTreeSet<&String> =
        spellings.iter().filter(|(_, s)| s.len() > 1).map(|(k, _)| k).collect();
    spellings
        .iter()
        .filter(|(key, s)| {
            s.len() > 1
                && !key
                    .rmatch_indices('/')
                    .any(|(i, _)| colliding.contains(&key[..i].to_string()))
        })
        .map(|(_, s)| s.iter().cloned().collect())
        .collect()
}

/// Whether the filesystem holding `generated/` treats the case-flipped
/// spelling of an existing entry as the same entry. Probed with a real
/// written path (`sample`, relative to `generated/`) so the answer reflects
/// the actual volume the output landed on, not an assumption from the OS.
pub fn output_fs_is_case_insensitive(root: &Path, sample: &str) -> bool {
    let flipped: String = sample
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect();
    flipped != sample && root.join("generated").join(&flipped).exists()
}

/// What a delta write did, for the `watch` server to report.
#[derive(Debug, Default, Clone, Copy)]
pub struct DeltaStats {
    pub written: usize,
    pub deleted: usize,
    pub unchanged: usize,
}

/// Write only the files that changed between `prev` (the last compute's output,
/// held in memory) and `new` (this compute's output), and delete files that
/// disappeared — the `watch` server's write. Because both trees are in memory,
/// the diff is a map compare (no disk reads, no hashing) and only the handful of
/// genuinely-changed files touch the filesystem — the whole point on APFS, where
/// touching all ~10k files is the wall. Keys are relative to `generated/`.
///
/// Returns the stats and the `new` map (so the caller can make it the next
/// `prev`). Parent dirs for written files are created as needed.
pub fn write_generated_delta(
    root: &Path,
    new: &[(String, String)],
    prev: &std::collections::HashMap<String, String>,
) -> Result<DeltaStats> {
    let base = root.join("generated");
    let mut stats = DeltaStats::default();
    let new_keys: std::collections::HashSet<&str> =
        new.iter().map(|(k, _)| k.as_str()).collect();

    for (rel, content) in new {
        if prev.get(rel).map(String::as_str) == Some(content.as_str()) {
            stats.unchanged += 1;
            continue;
        }
        let target = base.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::write(&target, content).map_err(|e| Error::io(&target, e))?;
        stats.written += 1;
    }

    // Files present last time but not now = removed outputs (e.g. a class that
    // lost its last plugin no longer needs an Interceptor).
    for rel in prev.keys() {
        if !new_keys.contains(rel.as_str()) {
            let target = base.join(rel);
            match fs::remove_file(&target) {
                Ok(()) => stats.deleted += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(&target, e)),
            }
        }
    }
    Ok(stats)
}

/// Remove a compile output directory (`generated/code` or
/// `generated/metadata`) so a fresh compile starts clean, exactly as
/// `setup:di:compile` wipes `generated/code` before running. A missing dir is
/// not an error. NEVER pass an archive dir (`_code`/`_metadata`).
///
/// `generated/code` holds ~4100 files under `Vendor/Module/…`; a single
/// `remove_dir_all` unlinks them serially (~140ms). Instead, remove each
/// `Vendor/Module` subtree in parallel (hundreds of independent subtrees =
/// good fan-out), then drop the emptied top dir. Falls back to a plain
/// `remove_dir_all` for a shallow tree.
///
/// (A background/deferred variant — rename aside + unlink on a worker thread —
/// was tried and reverted: on APFS the compile already saturates the single FS
/// metadata lock, so a background delete only contends with the foreground
/// scan/write instead of overlapping it. Deferral helps only where the FS lets
/// independent subtrees delete concurrently; there it saved ~40ms, not worth the
/// machinery. The real macOS lever is FEWER files, i.e. incremental/CAS.)
pub fn clear_generated_dir(root: &Path, subdir: &str) -> Result<()> {
    use rayon::prelude::*;
    debug_assert!(
        !subdir.starts_with('_'),
        "refusing to clear an archive dir: {subdir}"
    );
    let dir = root.join("generated").join(subdir);

    // Gather depth-2 targets: each `Vendor/Module` dir, plus any stray file or
    // non-dir entry directly under `Vendor/`. Missing dir ⇒ nothing to do.
    let vendors = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(&dir, e)),
    };
    let mut targets: Vec<PathBuf> = Vec::new();
    for vendor in vendors.flatten() {
        let vpath = vendor.path();
        if vendor.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            match fs::read_dir(&vpath) {
                Ok(modules) => targets.extend(modules.flatten().map(|m| m.path())),
                Err(_) => targets.push(vpath),
            }
        } else {
            targets.push(vpath);
        }
    }

    targets.par_iter().try_for_each(|p| -> Result<()> {
        let r = if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) };
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(p, e)),
        }
    })?;

    // Drop the now-emptied Vendor dirs + the top dir in one final sweep.
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(&dir, e)),
    }
}

fn collect_leaf_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_leaf_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_code_file_creates_nested_dirs_and_guards_clobber() {
        let root = tempfile::tempdir().unwrap();
        let rel = "Magento/Catalog/Model/ProductFactory.php";
        let path = write_code_file(root.path(), rel, "<?php\n", false).unwrap();
        assert_eq!(path, root.path().join("generated/code").join(rel));
        assert_eq!(fs::read_to_string(&path).unwrap(), "<?php\n");
        // No temp file left behind.
        assert!(!path.with_file_name(".ProductFactory.php.tmp").exists());
        // Second write without force is refused; with force it overwrites.
        assert!(write_code_file(root.path(), rel, "<?php // v2\n", false).is_err());
        write_code_file(root.path(), rel, "<?php // v2\n", true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "<?php // v2\n");
    }

    /// The proforto shape: sibling dirs differing only in case (a class named
    /// `FulfilStaff` next to a `Fulfilstaff\Grid` namespace), reported once at
    /// the SHORTEST colliding prefix — the deeper `…/Grid` paths textually
    /// differ too, but only because their parent does. File-level collisions
    /// (two classes differing only in case) are caught the same way; unrelated
    /// same-case paths never report.
    #[test]
    fn case_collisions_report_shortest_prefix_only() {
        let f = |rel: &str| (rel.to_owned(), String::new());
        let files = vec![
            f("code/Magestore/Report/FulfilStaff/Interceptor.php"),
            f("code/Magestore/Report/Fulfilstaff/Grid/Interceptor.php"),
            f("code/Magestore/Report/Fulfilstaff/Grid/Extra/Interceptor.php"),
            f("code/Acme/Widget/AFactory.php"),
            f("code/Acme/Widget/aFactory.php"),
            f("metadata/global.php"),
            f("metadata/frontend.php"),
        ];
        let groups = case_collisions(&files);
        assert_eq!(
            groups,
            vec![
                vec![
                    "code/Acme/Widget/AFactory.php".to_owned(),
                    "code/Acme/Widget/aFactory.php".to_owned(),
                ],
                vec![
                    "code/Magestore/Report/FulfilStaff".to_owned(),
                    "code/Magestore/Report/Fulfilstaff".to_owned(),
                ],
            ],
            "one group per shortest colliding prefix, children absorbed"
        );
        assert!(case_collisions(&[f("code/A/B.php"), f("code/A/C.php")]).is_empty());
    }

    #[test]
    fn clear_generated_dir_removes_tree_and_tolerates_absence() {
        let root = tempfile::tempdir().unwrap();
        write_code_file(root.path(), "A/B/CFactory.php", "x", false).unwrap();
        assert!(root.path().join("generated/code").exists());
        clear_generated_dir(root.path(), "code").unwrap();
        assert!(!root.path().join("generated/code").exists());
        // A missing dir is fine (idempotent).
        clear_generated_dir(root.path(), "code").unwrap();
    }
}
