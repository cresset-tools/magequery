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
