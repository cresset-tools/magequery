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

/// Write every `generated/code` file in parallel. The files are independent
/// and [`write_code_file`]'s per-file atomic temp+rename plus the idempotent
/// `create_dir_all` (concurrent calls for a shared parent dir are safe) make
/// concurrent writes correct. Returns the number written, or the first I/O
/// error encountered.
pub fn write_code_files(root: &Path, files: &[(String, String)], force: bool) -> Result<usize> {
    use rayon::prelude::*;
    files
        .par_iter()
        .try_for_each(|(rel, content)| write_code_file(root, rel, content, force).map(|_| ()))?;
    Ok(files.len())
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
}
