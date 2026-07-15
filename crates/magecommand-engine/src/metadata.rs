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

/// What an incremental write actually did, for the CLI to report.
#[derive(Debug, Default, Clone, Copy)]
pub struct IncrementalStats {
    pub written: usize,
    pub skipped: usize,
    pub deleted: usize,
}

/// Reconcile `generated/code` against the previous compile's manifest: write
/// only the files whose content changed (or are new), delete the ones that
/// disappeared, and leave byte-identical files untouched. Returns the stats and
/// the fresh manifest (hashes of the CURRENT output) for the caller to persist.
///
/// This replaces `clear + write_code_files` on the `--incremental` path — the
/// tree is NOT cleared first, so unchanged files (the vast majority of a
/// re-compile) are never touched, and on APFS the ~1.8s write phase collapses to
/// the changed subset. `prev = None` writes everything (a cold incremental
/// build, e.g. first run or an invalidated manifest).
///
/// Byte-exactness: the in-memory `files` are the single source of truth; we only
/// skip writing bytes whose blake3 already matches the manifest, so the result
/// is identical to a full compile. The manifest is trusted as the on-disk record
/// (the caller guards that `generated/code` actually exists before trusting it).
pub fn write_code_files_incremental(
    root: &Path,
    files: &[(String, String)],
    prev: Option<&crate::manifest::Manifest>,
    bp: &str,
) -> Result<(IncrementalStats, crate::manifest::Manifest)> {
    use rayon::prelude::*;
    use std::collections::{BTreeMap, HashSet};
    use crate::manifest::Manifest;

    let base = root.join("generated/code");

    // Hash the whole new output (parallel) → the fresh manifest map.
    let new_files: BTreeMap<String, String> = files
        .par_iter()
        .map(|(rel, content)| (rel.clone(), Manifest::hash(content.as_bytes())))
        .collect();

    // Writes = new path, or a hash that differs from the manifest.
    let to_write: Vec<&(String, String)> = files
        .iter()
        .filter(|(rel, _)| match prev.and_then(|m| m.files.get(rel)) {
            Some(prev_hash) => prev_hash != &new_files[rel],
            None => true,
        })
        .collect();

    // Deletes = in the old manifest but not in the new output.
    let new_paths: HashSet<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
    let to_delete: Vec<&String> = prev
        .map(|m| m.files.keys().filter(|p| !new_paths.contains(p.as_str())).collect())
        .unwrap_or_default();

    // Pre-create the parent dirs of the files we WILL write (dedup), then write
    // them directly in parallel — same as the full path, minus the untouched
    // majority.
    fs::create_dir_all(&base).map_err(|e| Error::io(&base, e))?;
    let dirs: Vec<PathBuf> = to_write
        .iter()
        .filter_map(|(rel, _)| Path::new(rel).parent())
        .filter(|p| !p.as_os_str().is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|p| base.join(p))
        .collect();
    dirs.par_iter()
        .try_for_each(|dir| fs::create_dir_all(dir).map_err(|e| Error::io(dir, e)))?;
    to_write.par_iter().try_for_each(|(rel, content)| -> Result<()> {
        let target = base.join(rel);
        fs::write(&target, content).map_err(|e| Error::io(&target, e))
    })?;

    // Remove files that no longer belong (a missing one is fine — already gone).
    for rel in &to_delete {
        let target = base.join(rel);
        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&target, e)),
        }
    }

    let stats = IncrementalStats {
        written: to_write.len(),
        skipped: files.len() - to_write.len(),
        deleted: to_delete.len(),
    };
    Ok((stats, Manifest::from_hashes(bp, new_files)))
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

    #[test]
    fn incremental_write_skips_unchanged_writes_changed_and_deletes_removed() {
        let root = tempfile::tempdir().unwrap();
        let bp = "/srv/shop";
        let base = root.path().join("generated/code");
        let read = |rel: &str| fs::read_to_string(base.join(rel)).unwrap();

        // Cold build (no manifest): everything is written, nothing skipped.
        let v1 = vec![
            ("A/Keep.php".to_owned(), "<?php // keep".to_owned()),
            ("A/Change.php".to_owned(), "<?php // v1".to_owned()),
            ("B/Gone.php".to_owned(), "<?php // gone".to_owned()),
        ];
        let (s, m1) = write_code_files_incremental(root.path(), &v1, None, bp).unwrap();
        assert_eq!((s.written, s.skipped, s.deleted), (3, 0, 0));
        assert_eq!(read("A/Change.php"), "<?php // v1");

        // Second run: Keep unchanged, Change edited, Gone removed, New added.
        let v2 = vec![
            ("A/Keep.php".to_owned(), "<?php // keep".to_owned()),
            ("A/Change.php".to_owned(), "<?php // v2".to_owned()),
            ("C/New.php".to_owned(), "<?php // new".to_owned()),
        ];
        let (s, m2) = write_code_files_incremental(root.path(), &v2, Some(&m1), bp).unwrap();
        assert_eq!((s.written, s.skipped, s.deleted), (2, 1, 1));
        assert_eq!(read("A/Change.php"), "<?php // v2");
        assert_eq!(read("C/New.php"), "<?php // new");
        assert!(!base.join("B/Gone.php").exists(), "removed file deleted");
        // The fresh manifest reflects the new set exactly.
        let keys: Vec<_> = m2.files.keys().cloned().collect();
        assert_eq!(keys, ["A/Change.php", "A/Keep.php", "C/New.php"]);

        // Re-running with the up-to-date manifest is a pure no-op.
        let (s, _) = write_code_files_incremental(root.path(), &v2, Some(&m2), bp).unwrap();
        assert_eq!((s.written, s.skipped, s.deleted), (0, 3, 0));
    }
}
