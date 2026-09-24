//! Reads `vendor/composer/installed.json` to locate packages directly, instead of walking
//! the entire `vendor/` tree (which on a real install is ~38k directories to find ~500
//! modules). Each package entry carries its install path and the `autoload.files`
//! (registration.php paths) that pinpoint module roots — including packages that bundle
//! several modules under `src/`.
//!
//! Parsing uses a typed `Deserialize` with only the fields we need, so serde walks the
//! 1.5MB document once and skips everything else without building a generic value tree.
//! Fields are `Cow<str>` borrowed from the input buffer (zero-copy in the common case,
//! allocating only for the rare escaped string).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer};

/// A JSON object, or an empty list standing in for one. PHP's `json_encode`
/// writes an empty array as `[]`, so an installer that serializes a PHP array
/// (bougie does) emits `"psr-0": []` where Composer would omit the key. Composer
/// decodes both the same way; a strict map type rejects the whole file instead.
fn map_or_empty_list<'de, D, V>(deserializer: D) -> Result<HashMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MapOrList<V> {
        Map(HashMap<String, V>),
        List(Vec<serde::de::IgnoredAny>),
    }
    match MapOrList::deserialize(deserializer)? {
        MapOrList::Map(map) => Ok(map),
        MapOrList::List(list) if list.is_empty() => Ok(HashMap::new()),
        MapOrList::List(_) => Err(serde::de::Error::custom(
            "expected an object (or an empty list), found a non-empty list",
        )),
    }
}

pub(crate) struct ComposerPackage {
    /// Package name (`vendor/name`), when the entry has one.
    pub name: Option<String>,
    /// Package version, e.g. `2.4.7-p3`.
    pub version: Option<String>,
    /// Absolute package root directory.
    pub root: PathBuf,
    /// `autoload.files` entries (relative to `root`), typically `registration.php` paths.
    pub autoload_files: Vec<String>,
    /// `autoload.psr-4`: namespace prefix (e.g. `Magento\Catalog\`) -> absolute source dirs.
    pub psr4: Vec<(String, Vec<PathBuf>)>,
    /// `autoload.psr-0` (e.g. `Cm\RedisSession\` -> `src/`); the prefix is *not* stripped
    /// from the path.
    pub psr0: Vec<(String, Vec<PathBuf>)>,
    /// `require` package names (version constraints dropped), sorted.
    pub require: Vec<String>,
    /// Composer package `type` (`magento2-module`, `magento2-library`, …).
    pub package_type: Option<String>,
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
    #[serde(default, borrow)]
    version: Option<Cow<'a, str>>,
    #[serde(rename = "install-path", default, borrow)]
    install_path: Option<Cow<'a, str>>,
    #[serde(default)]
    autoload: AutoloadEntry<'a>,
    /// Only the keys (required package names) matter; constraints are skipped unparsed.
    #[serde(default, deserialize_with = "map_or_empty_list")]
    require: HashMap<String, serde::de::IgnoredAny>,
    #[serde(rename = "type", default, borrow)]
    package_type: Option<Cow<'a, str>>,
}

#[derive(Deserialize, Default)]
struct AutoloadEntry<'a> {
    #[serde(default, borrow)]
    files: Vec<Cow<'a, str>>,
    /// Each value is a single path or a list of paths.
    #[serde(default, rename = "psr-4", deserialize_with = "map_or_empty_list")]
    psr4: HashMap<String, StringOrVec>,
    #[serde(default, rename = "psr-0", deserialize_with = "map_or_empty_list")]
    psr0: HashMap<String, StringOrVec>,
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
    // Report the error of the shape the document actually has: a Composer 2 file
    // that fails to parse would otherwise surface as the retry's "expected a
    // sequence at line 1 column 0", which names nothing that is wrong with it.
    let entries: Vec<PackageEntry> = match serde_json::from_str::<InstalledFile>(&text) {
        Ok(f) => f.packages,
        Err(v2) if text.trim_start().starts_with('{') => return Err(v2.to_string()),
        Err(_) => serde_json::from_str::<Vec<PackageEntry>>(&text).map_err(|e| e.to_string())?,
    };

    let mut out = Vec::with_capacity(entries.len());
    for p in entries {
        let name: Option<String> = p.name.as_ref().map(|n| n.as_ref().to_owned());
        let version: Option<String> = p.version.as_ref().map(|v| v.as_ref().to_owned());
        let root = match p.install_path {
            // install-path is relative to vendor/composer/.
            Some(ip) => normalize(&composer_dir.join(ip.as_ref())),
            // Fallback to the conventional vendor/<name> location.
            None => match &name {
                Some(n) => vendor.join(n),
                None => continue,
            },
        };
        let mut require: Vec<String> = p.require.into_keys().collect();
        require.sort();
        let autoload_files = p.autoload.files.iter().map(|f| f.as_ref().to_owned()).collect();
        let psr4 = prefix_map(p.autoload.psr4, &root);
        let psr0 = prefix_map(p.autoload.psr0, &root);
        let package_type = p.package_type.as_ref().map(|t| t.as_ref().to_owned());
        out.push(ComposerPackage {
            name,
            version,
            root,
            autoload_files,
            psr4,
            psr0,
            require,
            package_type,
        });
    }
    Ok(out)
}

/// `namespace prefix -> source dirs`, the shape of a PSR-4/PSR-0 autoload map.
pub(crate) type PrefixMap = Vec<(String, Vec<PathBuf>)>;

fn prefix_map(map: HashMap<String, StringOrVec>, root: &Path) -> PrefixMap {
    map.into_iter()
        .map(|(prefix, v)| {
            let rels = match v {
                StringOrVec::One(s) => vec![s],
                StringOrVec::Many(m) => m,
            };
            let dirs = rels.into_iter().map(|r| normalize(&root.join(r))).collect();
            (prefix, dirs)
        })
        .collect()
}

/// The ROOT project's own `composer.json` autoload maps — not in `installed.json`, but
/// load-bearing: `Magento\Setup\` (setup/src/…) and, on git checkouts,
/// `Magento\Framework\`/`Magento\` live here. Returns `(psr-4, psr-0)`; empty when there
/// is no readable root composer.json.
pub(crate) fn root_autoload(root: &Path) -> (PrefixMap, PrefixMap) {
    #[derive(Deserialize, Default)]
    struct RootComposer<'a> {
        #[serde(default, borrow)]
        autoload: AutoloadEntry<'a>,
    }
    let Ok(text) = std::fs::read_to_string(root.join("composer.json")) else {
        return (Vec::new(), Vec::new());
    };
    let Ok(rc) = serde_json::from_str::<RootComposer>(&text) else {
        return (Vec::new(), Vec::new());
    };
    (prefix_map(rc.autoload.psr4, root), prefix_map(rc.autoload.psr0, root))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn vendor_with(installed_json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("composer")).expect("composer dir");
        std::fs::write(dir.path().join("composer/installed.json"), installed_json)
            .expect("installed.json");
        dir
    }

    /// bougie writes PHP's empty array as `[]` for empty autoload maps. One such
    /// entry used to fail the whole document, dropping every package (and with
    /// them the library paths and path-repository modules) to a vendor scan.
    #[test]
    fn empty_list_stands_in_for_an_empty_map() {
        let vendor = vendor_with(
            r#"{"packages": [
                {"name": "mage-os/framework", "type": "magento2-library",
                 "install-path": "../mage-os/framework",
                 "autoload": {"files": ["registration.php"], "psr-4": {"Magento\\Framework\\": ""}, "psr-0": []},
                 "require": []},
                {"name": "brick/math", "install-path": "../brick/math",
                 "autoload": {"psr-4": [], "psr-0": []}}
            ], "dev": false}"#,
        );
        let packages = installed_packages(vendor.path()).expect("parses");
        assert_eq!(packages.len(), 2);
        let framework = &packages[0];
        assert_eq!(framework.package_type.as_deref(), Some("magento2-library"));
        assert_eq!(framework.autoload_files, vec!["registration.php".to_owned()]);
        assert_eq!(framework.psr4.len(), 1);
        assert!(framework.psr0.is_empty() && framework.require.is_empty());
        assert!(packages[1].psr4.is_empty() && packages[1].psr0.is_empty());
    }

    #[test]
    fn a_broken_composer_2_file_reports_its_own_error() {
        let vendor = vendor_with(r#"{"packages": [{"name": "a/b", "autoload": {"psr-4": ["src/"]}}]}"#);
        let error = installed_packages(vendor.path()).err().expect("rejects a non-empty list");
        assert!(error.contains("non-empty list"), "{error}");
        assert!(!error.contains("expected a sequence"), "{error}");
    }
}
