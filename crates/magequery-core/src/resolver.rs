//! Class → source-file resolution via PSR-4 autoload maps. This is the first slice of the
//! class resolver: enough to answer "does this class actually exist?" (so `preference` can
//! report a genuine not-found instead of echoing a typo back). The PHP-header parse that
//! yields the inheritance chain — needed for plugin-on-ancestor resolution — comes later.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::composer::ComposerPackage;
use crate::ids::ClassName;
use crate::model::{InterceptKind, Module, ModuleSource, PluginMethod};
use crate::php::{self, PhpClass};

pub(crate) struct ClassResolver {
    /// `(namespace prefix ending in '\', source dirs)`, sorted longest-prefix-first so PSR-4
    /// longest-match wins.
    prefixes: Vec<(String, Vec<PathBuf>)>,
    /// Lazily parsed PHP headers, keyed by class name (`None` = file missing/unparseable).
    headers: Mutex<HashMap<ClassName, Option<Arc<PhpClass>>>>,
}

impl ClassResolver {
    pub fn build(packages: &[ComposerPackage], modules: &[Module]) -> Self {
        let mut prefixes: Vec<(String, Vec<PathBuf>)> = Vec::new();

        // Vendor PSR-4 from composer metadata.
        for pkg in packages {
            for (prefix, dirs) in &pkg.psr4 {
                prefixes.push((prefix.clone(), dirs.clone()));
            }
        }

        // app/code is not composer-managed; synthesize the Magento convention
        // `Vendor_Module` -> namespace `Vendor\Module\` rooted at the module dir.
        for m in modules.iter().filter(|m| m.source == ModuleSource::App) {
            let ns = format!("{}\\", m.name.as_str().replace('_', "\\"));
            prefixes.push((ns, vec![m.path.clone()]));
        }

        prefixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        Self { prefixes, headers: Mutex::new(HashMap::new()) }
    }

    /// The on-disk file a class maps to, if any PSR-4 prefix resolves it to an existing
    /// `.php`. Scans matching prefixes longest-first and returns the first file that exists.
    pub fn file_for(&self, class: &ClassName) -> Option<PathBuf> {
        let name = class.as_str();
        for (prefix, dirs) in &self.prefixes {
            if let Some(rest) = name.strip_prefix(prefix.as_str()) {
                let rel = format!("{}.php", rest.replace('\\', "/"));
                for dir in dirs {
                    let candidate = dir.join(&rel);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }

    /// Whether the class resolves to an existing source file.
    pub fn exists(&self, class: &ClassName) -> bool {
        self.file_for(class).is_some()
    }

    /// The parsed header for `class`, reading + parsing on first request and caching.
    fn header(&self, class: &ClassName) -> Option<Arc<PhpClass>> {
        if let Some(cached) = self.headers.lock().unwrap().get(class) {
            return cached.clone();
        }
        let parsed = self
            .file_for(class)
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|src| php::parse_header(&src))
            .map(Arc::new);
        self.headers.lock().unwrap().insert(class.clone(), parsed.clone());
        parsed
    }

    /// All transitive ancestors of `class`: parent classes plus implemented interfaces
    /// (and interfaces those extend), nearest-first. Excludes `class` itself. Types whose
    /// source we can't find (e.g. PHP built-ins) simply end a branch.
    ///
    /// This is what makes plugin resolution correct: a plugin declared on an interface or
    /// parent applies to every implementation/subclass.
    pub fn ancestors(&self, class: &ClassName) -> Vec<ClassName> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        seen.insert(class.clone());
        let mut queue = VecDeque::new();
        queue.push_back(class.clone());

        while let Some(cur) = queue.pop_front() {
            if let Some(h) = self.header(&cur) {
                for parent in h.extends.iter().chain(h.implements.iter()) {
                    if seen.insert(parent.clone()) {
                        result.push(parent.clone());
                        queue.push_back(parent.clone());
                    }
                }
            }
        }
        result
    }

    /// The interception methods a plugin class defines (`before*`/`around*`/`after*`), each
    /// with the target method it intercepts. Empty if the file can't be read.
    pub fn plugin_methods(&self, plugin: &ClassName) -> Vec<PluginMethod> {
        let Some(src) = self.file_for(plugin).and_then(|f| std::fs::read_to_string(f).ok()) else {
            return Vec::new();
        };
        php::plugin_methods(&src)
            .into_iter()
            .map(|m| {
                let kind = match m.prefix {
                    "before" => InterceptKind::Before,
                    "around" => InterceptKind::Around,
                    _ => InterceptKind::After,
                };
                let target = php::lcfirst(&m.name[m.prefix.len()..]);
                PluginMethod { kind, target, plugin_method: m.name }
            })
            .collect()
    }
}
