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

/// Base types that mark a controller action (an implementation of any of these is an
/// endpoint). `ActionInterface` covers the modern `HttpGet/PostActionInterface` (which
/// extend it) and the legacy `Action\Action` (which implements it); the others are belt-and-
/// suspenders for older/admin bases.
fn is_action_base(c: &ClassName) -> bool {
    matches!(
        c.as_str(),
        "Magento\\Framework\\App\\ActionInterface"
            | "Magento\\Framework\\App\\Action\\Action"
            | "Magento\\Backend\\App\\AbstractAction"
    )
}

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

    /// Whether `class` is a concrete controller action: an instantiable class (not an
    /// interface or abstract base) whose ancestors include a Magento action base.
    pub fn is_action(&self, class: &ClassName) -> bool {
        match self.header(class) {
            Some(h) if !h.is_interface && !h.is_abstract => {
                self.ancestors(class).iter().any(is_action_base)
            }
            _ => false,
        }
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

    /// The `bin/magento` name and description a console-command class declares
    /// (`setName`/`$defaultName`/`parent::__construct`, `setDescription`). A `self::CONST`
    /// or `$this->prop` reference is resolved against the file's own constants/property
    /// defaults, then its ancestors' files. `None` when the value is built dynamically.
    pub fn command_info(&self, class: &ClassName) -> (Option<String>, Option<String>) {
        // A `\Proxy` suffix names a generated lazy wrapper (absent on a fresh checkout —
        // it lives in `generated/`); the real command class is the prefix.
        let mut class = class.clone();
        if self.file_for(&class).is_none() {
            if let Some(real) = class.as_str().strip_suffix("\\Proxy") {
                class = ClassName::new(real);
            }
        }
        let Some(src) = self.file_for(&class).and_then(|f| std::fs::read_to_string(f).ok()) else {
            return (None, None);
        };
        let info = php::command_info(&src);
        let resolve = |v: &Option<php::StrOrConst>| {
            let (key, from_props) = match v {
                Some(php::StrOrConst::Str(s)) => return Some(s.clone()),
                Some(php::StrOrConst::Const(c)) => (c, false),
                Some(php::StrOrConst::Prop(p)) => (p, true),
                None => return None,
            };
            let get = |i: &php::CommandInfo| {
                if from_props { i.props.get(key).cloned() } else { i.consts.get(key).cloned() }
            };
            if let Some(s) = get(&info) {
                return Some(s);
            }
            // Inherited: scan ancestor files (nearest-first) for the constant/property.
            self.ancestors(&class).iter().find_map(|a| {
                let src = std::fs::read_to_string(self.file_for(a)?).ok()?;
                get(&php::command_info(&src))
            })
        };
        (resolve(&info.name), resolve(&info.description))
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
