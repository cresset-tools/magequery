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
use crate::vfs::Vfs;
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
    /// PSR-4: `(namespace prefix ending in '\', source dirs)`, sorted longest-prefix-first
    /// so longest-match wins. The prefix is stripped from the path.
    prefixes: Vec<(String, Vec<PathBuf>)>,
    /// PSR-0: the prefix stays part of the path (`Cm\RedisSession\X` under `src/` is
    /// `src/Cm/RedisSession/X.php`). (Underscore-as-separator in the class name is not
    /// modeled — no Magento-adjacent PSR-0 package needs it.)
    psr0: Vec<(String, Vec<PathBuf>)>,
    /// Lazily parsed PHP headers, keyed by class name (`None` = file missing/unparseable).
    headers: Mutex<HashMap<ClassName, Option<Arc<PhpClass>>>>,
    /// Content reads (headers, plugin methods, command names) honor the buffer overlay.
    vfs: std::sync::Arc<Vfs>,
}

impl ClassResolver {
    pub fn build(
        packages: &[ComposerPackage],
        modules: &[Module],
        root: &std::path::Path,
        vfs: std::sync::Arc<Vfs>,
    ) -> Self {
        let mut prefixes: Vec<(String, Vec<PathBuf>)> = Vec::new();
        let mut psr0: Vec<(String, Vec<PathBuf>)> = Vec::new();

        // Vendor PSR-4/PSR-0 from composer metadata.
        for pkg in packages {
            for (prefix, dirs) in &pkg.psr4 {
                prefixes.push((prefix.clone(), dirs.clone()));
            }
            for (prefix, dirs) in &pkg.psr0 {
                psr0.push((prefix.clone(), dirs.clone()));
            }
        }

        // The root project's own composer.json autoload (not in installed.json): this is
        // where `Magento\Setup\` — and on git checkouts the whole framework — lives.
        let (root_psr4, root_psr0) = crate::composer::root_autoload(root);
        prefixes.extend(root_psr4);
        psr0.extend(root_psr0);

        // app/code is not composer-managed; synthesize the Magento convention
        // `Vendor_Module` -> namespace `Vendor\Module\` rooted at the module dir.
        for m in modules.iter().filter(|m| m.source == ModuleSource::App) {
            let ns = format!("{}\\", m.name.as_str().replace('_', "\\"));
            prefixes.push((ns, vec![m.path.clone()]));
        }

        prefixes.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        psr0.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        Self { prefixes, psr0, headers: Mutex::new(HashMap::new()), vfs }
    }

    /// The on-disk file a class maps to, if any PSR-4/PSR-0 prefix resolves it to an
    /// existing `.php`. Scans matching prefixes longest-first and returns the first file
    /// that exists.
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
        for (prefix, dirs) in &self.psr0 {
            if name.starts_with(prefix.as_str()) {
                let rel = format!("{}.php", name.replace('\\', "/"));
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

    /// Whether any autoload prefix covers this class name at all. When none does, the
    /// class may still exist via a classmap (which installed.json only names as dirs) —
    /// i.e. "not found" is only meaningful for classes this returns `true` for.
    pub fn has_prefix_for(&self, class: &ClassName) -> bool {
        let name = class.as_str();
        self.prefixes.iter().any(|(p, _)| name.starts_with(p.as_str()))
            || self.psr0.iter().any(|(p, _)| name.starts_with(p.as_str()))
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

    /// The parsed header for `class` (crate-internal; `doctor` checks interface/abstract).
    pub(crate) fn header_of(&self, class: &ClassName) -> Option<Arc<PhpClass>> {
        self.header(class)
    }

    /// The parsed header for `class`, reading + parsing on first request and caching.
    fn header(&self, class: &ClassName) -> Option<Arc<PhpClass>> {
        if let Some(cached) = self.headers.lock().unwrap().get(class) {
            return cached.clone();
        }
        let parsed = self
            .file_for(class)
            .and_then(|f| self.vfs.read_to_string(&f).ok())
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
        let Some(src) = self.file_for(&class).and_then(|f| self.vfs.read_to_string(&f).ok()) else {
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
                let src = self.vfs.read_to_string(&self.file_for(a)?).ok()?;
                get(&php::command_info(&src))
            })
        };
        (resolve(&info.name), resolve(&info.description))
    }

    /// The interception methods a plugin class defines (`before*`/`around*`/`after*`), each
    /// with the target method it intercepts. Empty if the file can't be read.
    pub fn plugin_methods(&self, plugin: &ClassName) -> Vec<PluginMethod> {
        let Some(src) = self.file_for(plugin).and_then(|f| self.vfs.read_to_string(&f).ok()) else {
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

    /// Every class name derivable from the autoload maps: each PSR-4/PSR-0 source dir
    /// walked in parallel, paths mapped back to FQCNs. Names are derived from paths, not
    /// parsed headers — cheap, but a file whose class diverges from its path is
    /// misnamed here (the same convention bet the whole resolver makes). Sorted,
    /// deduped. This is the expensive enumeration (`file_for` in reverse, in bulk);
    /// callers cache it — the result only changes when PHP files appear or disappear.
    pub fn class_names(&self) -> Vec<ClassName> {
        use rayon::prelude::*;
        // Skip autoload roots inside generated/ and var/: runtime-written code
        // (interceptors, proxies, factories) is never something a human types into
        // config, and it doesn't exist on a fresh checkout anyway.
        let is_runtime_dir = |dir: &PathBuf| {
            dir.components()
                .any(|c| c.as_os_str() == "generated" || c.as_os_str() == "var")
        };
        let mut jobs: Vec<(&str, &PathBuf, bool)> = Vec::new();
        for (prefix, dirs) in &self.prefixes {
            for dir in dirs.iter().filter(|d| !is_runtime_dir(d)) {
                jobs.push((prefix.as_str(), dir, false));
            }
        }
        for (prefix, dirs) in &self.psr0 {
            for dir in dirs.iter().filter(|d| !is_runtime_dir(d)) {
                jobs.push((prefix.as_str(), dir, true));
            }
        }
        let lists: Vec<Vec<ClassName>> = jobs
            .par_iter()
            .map(|(prefix, dir, psr0)| {
                let mut out = Vec::new();
                collect_php_classes(dir, &mut String::new(), 0, &mut |rel_class| {
                    let name = if *psr0 {
                        // PSR-0 keeps the prefix inside the path; only names under the
                        // registered prefix belong to it.
                        if !rel_class.starts_with(prefix) {
                            return;
                        }
                        rel_class.to_string()
                    } else {
                        format!("{prefix}{rel_class}")
                    };
                    out.push(ClassName::new(name));
                });
                out
            })
            .collect();
        let set: std::collections::BTreeSet<ClassName> = lists.into_iter().flatten().collect();
        set.into_iter().collect()
    }
}

/// Recursive `.php` walk, calling `push` with the `\`-joined relative class name (no
/// extension). Skips test fixtures and non-class files (PSR class files start with an
/// uppercase letter; `registration.php`, `functions.php` and friends don't).
fn collect_php_classes(
    dir: &std::path::Path,
    rel: &mut String,
    depth: usize,
    push: &mut impl FnMut(&str),
) {
    if depth > 12 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();
        if path.is_dir() {
            // `generated`/`var`/`pub` also guard the *descent*: filtering only the
            // autoload roots misses stores whose root composer.json maps a prefix to an
            // ancestor of generated/ — the walk would rediscover every interceptor and
            // proxy from below (seen live on a real store).
            if matches!(
                name,
                "Test" | "Tests" | "_files" | "node_modules" | "generated" | "var" | "pub" | "dev"
            ) || name.starts_with('.')
            {
                continue;
            }
            let len = rel.len();
            rel.push_str(name);
            rel.push('\\');
            collect_php_classes(&path, rel, depth + 1, push);
            rel.truncate(len);
        } else if let Some(stem) = name.strip_suffix(".php") {
            if stem.starts_with(|c: char| c.is_ascii_uppercase()) {
                let len = rel.len();
                rel.push_str(stem);
                push(rel);
                rel.truncate(len);
            }
        }
    }
}
