//! Reads `vendor/composer/installed.json` to locate packages directly, instead of walking
//! the entire `vendor/` tree (which on a real install is ~38k directories to find ~500
//! modules). Each package entry carries its install path and the `autoload.files`
//! (registration.php paths) that pinpoint module roots — including packages that bundle
//! several modules under `src/`.
//!
//! Parsing uses a typed `Deserialize` with only the three fields we need, so serde walks
//! the 1.5MB document once and skips everything else without building a generic value tree.
//! Fields are `Cow<str>` borrowed from the input buffer (zero-copy in the common case,
//! allocating only for the rare escaped string).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

pub(crate) struct ComposerPackage {
    /// Absolute package root directory.
    pub root: PathBuf,
    /// `autoload.files` entries (relative to `root`), typically `registration.php` paths.
    pub autoload_files: Vec<String>,
    /// `autoload.psr-4`: namespace prefix (e.g. `Magento\Catalog\`) -> absolute source dirs.
    pub psr4: Vec<(String, Vec<PathBuf>)>,
}

#[derive(Deserialize)]
struct InstalledFile<'a> {
    #[serde(borrow)]
    packages: Vec<PackageEntry<'a>>,
}

#[derive(Deserialize)]
struct PackageEntry<'a> {
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(rename = "install-path", default, borrow)]
    install_path: Option<Cow<'a, str>>,
    #[serde(default)]
    autoload: AutoloadEntry<'a>,
}

#[derive(Deserialize, Default)]
struct AutoloadEntry<'a> {
    #[serde(default, borrow)]
    files: Vec<Cow<'a, str>>,
    /// Each value is a single path or a list of paths.
    #[serde(default, rename = "psr-4")]
    psr4: HashMap<String, StringOrVec>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

/// Parse `<vendor>/composer/installed.json`. `vendor` is the absolute `vendor/` directory.
pub(crate) fn installed_packages(vendor: &Path) -> Result<Vec<ComposerPackage>, String> {
    let composer_dir = vendor.join("composer");
    let path = composer_dir.join("installed.json");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;

    // Composer 2 wraps packages in `{ "packages": [...] }`; Composer 1 was a bare array.
    let entries: Vec<PackageEntry> = match serde_json::from_str::<InstalledFile>(&text) {
        Ok(f) => f.packages,
        Err(_) => serde_json::from_str::<Vec<PackageEntry>>(&text).map_err(|e| e.to_string())?,
    };

    let mut out = Vec::with_capacity(entries.len());
    for p in entries {
        let root = match p.install_path {
            // install-path is relative to vendor/composer/.
            Some(ip) => normalize(&composer_dir.join(ip.as_ref())),
            // Fallback to the conventional vendor/<name> location.
            None => match p.name {
                Some(n) => vendor.join(n.as_ref()),
                None => continue,
            },
        };
        let autoload_files = p.autoload.files.iter().map(|f| f.as_ref().to_owned()).collect();
        let psr4 = p
            .autoload
            .psr4
            .into_iter()
            .map(|(prefix, v)| {
                let rels = match v {
                    StringOrVec::One(s) => vec![s],
                    StringOrVec::Many(m) => m,
                };
                let dirs = rels.into_iter().map(|r| normalize(&root.join(r))).collect();
                (prefix, dirs)
            })
            .collect();
        out.push(ComposerPackage { root, autoload_files, psr4 });
    }
    Ok(out)
}

/// Lexically resolve `.`/`..` so paths read cleanly (e.g. `vendor/composer/../magento/x`
/// becomes `vendor/magento/x`). Does not touch the filesystem.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
