//! `interception.php` — Magento's `Interception\Config\Config::initialize`
//! ported: for every type the compile touches, whether an interceptor
//! applies (plugins declared on it, its real type, or any ancestor).
//! The compiled cache writer sorts keys, so only set + values matter.

use std::collections::{BTreeMap, HashMap, HashSet};

use magequery_core::{DiExport, DiScope, Magento};

use crate::areaconfig::AREA_CODES;
use crate::definitions::Definitions;
use crate::phpexport::{PhpKey, PhpValue};

pub fn interception_map(magento: &Magento, defs: &Definitions) -> BTreeMap<String, bool> {
    build_map(magento, defs, true)
}

/// The developer-mode runtime variant: the `interception` cache entry that
/// `Config::initializeUncompiled` saves. Runtime definitions list no classes
/// (`Definition\Runtime::getClasses()` is `[]`), so only types the merged
/// config names are resolved — step 3 below is skipped.
pub fn runtime_interception_map(defs: &Definitions, scopes: &[DiScope]) -> BTreeMap<String, bool> {
    // getOriginalInstanceType consults the object manager's config: primary +
    // global virtual types, later winning (chased to the fixpoint).
    let vtypes: HashMap<&str, &str> = scopes
        .iter()
        .filter(|s| s.scope == "primary" || s.scope == "global")
        .flat_map(|s| s.virtual_types.iter().map(|(n, b)| (n.trim_start_matches('\\'), b.trim_start_matches('\\'))))
        .collect();
    let mut resolver = Resolver { defs, vtypes, map: Default::default() };
    // 1. Every type declaring plugins in any scope (`!empty($typeConfig['plugins'])`
    //    after array_replace_recursive over the scopes).
    for scope in scopes {
        for (name, plugins) in &scope.types {
            if *plugins {
                resolver.map.insert(name.trim_start_matches('\\').to_owned(), true);
            }
        }
    }
    // 2. `hasPlugins` for every key of the merged config.
    for scope in scopes {
        for (name, _) in &scope.types {
            resolver.has_plugins(name);
        }
    }
    resolver.map.into_iter().collect()
}

/// The class universe a developer-mode request resolves interception over,
/// and the scopes it reads: only what the walk can reach is loaded — the
/// config's types, the source chains of the generatable ones, and their
/// ancestors — resolved like the autoloader. At runtime `Relations\Runtime`
/// asks the autoloader, which also finds classes outside the compile scan (a
/// virtual type of an enabled module based on a class of a DISABLED one).
pub fn runtime_definitions(magento: &Magento, root: &std::path::Path) -> Result<(Definitions, Vec<DiScope>), String> {
    runtime_definitions_generating(magento, root, Vec::new()).map(|(defs, scopes, _)| (defs, scopes))
}

/// The definitions, the scopes, and the generated files as `(path under
/// generated/code, bytes)`.
pub type RuntimeGenerated = (Definitions, Vec<DiScope>, Vec<(String, String)>);

/// [`runtime_definitions`], also generating `extra` (generated class names a
/// request will ask for — what `generated/code` held before it was emptied),
/// and returning every file generated on the way, as `(path under
/// generated/code, bytes)`: what PHP's autoloader would write for those names.
/// Names no generator here reproduces exactly are left out.
pub fn runtime_definitions_generating(
    magento: &Magento,
    root: &std::path::Path,
    extra: Vec<String>,
) -> Result<RuntimeGenerated, String> {
    let prof = std::env::var_os("MAGECOMMAND_PROFILE").is_some();
    let t = std::time::Instant::now();
    let scopes = magento.di_scopes().map_err(|e| e.to_string())?;
    let interceptors: Vec<String> = extra.iter().filter(|n| n.ends_with("\\Interceptor")).cloned().collect();
    let mut names: Vec<String> = extra;
    for scope in &scopes {
        names.extend(scope.types.iter().map(|(n, _)| n.trim_start_matches('\\').to_owned()));
        names.extend(scope.virtual_types.iter().map(|(_, b)| b.trim_start_matches('\\').to_owned()));
    }
    names.sort_unstable();
    names.dedup();
    let sources: Vec<String> = names
        .iter()
        .filter_map(|n| {
            let mut chain = Vec::new();
            let mut current = n.clone();
            while let Some((_, source)) = crate::codegen::classify(&current) {
                chain.push(source.clone());
                current = source;
            }
            (!chain.is_empty()).then_some(chain)
        })
        .flatten()
        .collect();
    if prof { eprintln!("  · scopes+names  {:?}", t.elapsed()); }
    let mut defs = Definitions::empty();
    defs.load_closure(magento, root, names.iter().cloned().chain(sources));
    if prof { eprintln!("  · closure       {:?}", t.elapsed()); }
    // `Relations\Runtime::has` is `class_exists`, which runs the generation
    // autoloader: with `generated/` emptied, every generatable name the
    // config mentions (a `Foo\Proxy` argument, a virtual type based on one)
    // materializes — and a proxy extends its subject, so it inherits the
    // subject's plugins. A generated name a global virtual type already
    // takes is never generated (GENERATION_SKIP).
    let vtypes: std::collections::HashSet<String> = scopes
        .iter()
        .filter(|s| s.scope == "primary" || s.scope == "global")
        .flat_map(|s| s.virtual_types.iter().map(|(n, _)| n.trim_start_matches('\\').to_owned()))
        .collect();
    // A default naming another class's constant renders as its value.
    defs.load_constant_classes(magento, root);
    let code = crate::codegen::generate_on_demand_closed(magento, &mut defs, root, &vtypes, names.iter().map(String::as_str));
    if prof { eprintln!("  · on-demand gen  {:?} ({} files)", t.elapsed(), code.files.len()); }
    let mut files = code.files;
    files.extend(compiled_interceptors_for(magento, root, &mut defs, &interceptors));
    if prof { eprintln!("  · interceptors  {:?} ({} files)", t.elapsed(), files.len()); }
    Ok((defs, scopes, files))
}

/// The `\Interceptor`s among `names`, when the store generates them with
/// creatuity/magento2-interceptors (Mage-OS 4 bundles it and prefers its
/// `CompiledInterceptor` for the runtime generator): its plugins-baked-in format,
/// rendered by [`crate::compiled_interceptor`]. A store on Magento's classic
/// generator gets none; PHP generates those when first asked. Subjects are taken
/// in `names` order, which is the order PHP generated them in (see
/// `AreasPluginList::new`).
fn compiled_interceptors_for(
    magento: &Magento,
    root: &std::path::Path,
    defs: &mut Definitions,
    names: &[String],
) -> Vec<(String, String)> {
    use crate::compiled_interceptor::{compiled_interceptors, AreasPluginList};
    let creatuity = magento.modules().iter().any(|m| m.enabled && m.name.to_string() == "Creatuity_Interception");
    if !creatuity {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let subjects: Vec<String> = names
        .iter()
        .filter_map(|n| n.strip_suffix("\\Interceptor"))
        .filter(|s| seen.insert(s.to_owned()) && defs.get(&format!("{s}\\Interceptor")).is_none())
        .map(str::to_owned)
        .collect();
    if subjects.is_empty() {
        return Vec::new();
    }
    defs.load_closure(magento, root, subjects.iter().cloned());
    loop {
        let need = AreasPluginList::required_classes(magento, defs, &subjects);
        if defs.load_closure(magento, root, need) == 0 {
            break;
        }
    }
    defs.load_constant_classes(magento, root);
    let plugins = AreasPluginList::new(magento, defs, &subjects);
    compiled_interceptors(defs, &plugins, &subjects)
        .into_iter()
        .filter_map(|(subject, bytes)| {
            let bytes = bytes.ok()?;
            Some((format!("{}/Interceptor.php", subject.replace(['\\', '_'], "/")), bytes))
        })
        .collect()
}

fn build_map(magento: &Magento, defs: &Definitions, scan_all: bool) -> BTreeMap<String, bool> {
    let exports: Vec<&DiExport> = AREA_CODES
        .iter()
        .map(|(area, _)| magento.di_export_ref(*area))
        .collect();

    // getOriginalInstanceType consults the object manager's config — the
    // GLOBAL virtual-type map (chased to the fixpoint).
    let global_vtypes: HashMap<&str, &str> = exports[0]
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str(), v.base.as_str()))
        .collect();

    let mut resolver = Resolver {
        defs,
        vtypes: global_vtypes,
        map: Default::default(),
    };

    // 1. Every type carrying plugin declarations in ANY scope (disabled
    //    entries count — the check is `!empty($typeConfig['plugins'])`).
    for export in &exports {
        for plugin in &export.plugins {
            resolver.map.insert(plugin.target.as_str().to_owned(), true);
        }
    }
    // 2. Every type the merged config mentions (type/virtualType nodes:
    //    arguments, shared, virtual types, plugin targets).
    let mut config_types: HashSet<&str> = HashSet::new();
    for export in &exports {
        config_types.extend(export.arguments.iter().map(|a| a.type_name.as_str()));
        config_types.extend(export.virtual_types.iter().map(|v| v.name.as_str()));
        config_types.extend(export.shared.iter().map(|s| s.type_name.as_str()));
        config_types.extend(export.plugins.iter().map(|p| p.target.as_str()));
    }
    for name in config_types {
        resolver.has_plugins(name);
    }
    // 3. Every scanned class (app + lib + generated — NOT setup).
    for class in defs.scanned.iter().filter(|_| scan_all) {
        if !defs.setup_classes.contains(class) {
            resolver.has_plugins(class);
        }
    }

    resolver.map.into_iter().collect()
}

struct Resolver<'a> {
    defs: &'a Definitions,
    vtypes: HashMap<&'a str, &'a str>,
    map: rustc_hash::FxHashMap<String, bool>,
}

impl Resolver<'_> {
    /// `Config::_inheritInterception`, memoized into the map (every type
    /// touched — ancestors and interfaces included — gets an entry). Only
    /// CLASSES inherit: `Relations\Runtime::getParents` bails when
    /// `class_exists` is false, so interfaces, traits, and unknowns never walk
    /// their ancestry.
    ///
    /// The key is the name AS GIVEN, like `$this->_intercepted[$type]`: a
    /// di.xml `<type name=>` keeps its config spelling, a scanned class its
    /// declared one. PHP resolves the class case-insensitively, so a config
    /// that spells it differently from its declaration (`…\KingDebtor` in
    /// di.xml, `class Kingdebtor` in source) yields TWO keys with one verdict
    /// — the compiled map has both. The declared case is only for finding the
    /// class, and for its parents, which reflection reports declared.
    fn has_plugins(&mut self, type_name: &str) -> bool {
        let key = type_name.trim_start_matches('\\');
        if let Some(&v) = self.map.get(key) {
            return v;
        }
        let key = key.to_owned();
        // Guard against cycles before recursing.
        self.map.insert(key.clone(), false);

        let real = self.resolve_vtype(&key);
        let result = if real != key {
            self.has_plugins(&real)
        } else {
            let declared = self.defs.canonical_case(&key).unwrap_or(&key).to_owned();
            // Classes whose last segment is exactly `Interceptor` never
            // inherit interception (`_serviceClassTypes`).
            let last = key.rsplit('\\').next().unwrap_or(&key);
            if last == "Interceptor" {
                false
            } else if let Some(record) = self.defs.get(&declared) {
                if record.meta.kind == magecommand_php::ClassKind::Class {
                    let relations = self.defs.relations_of(&declared);
                    let mut any = false;
                    for relation in relations.iter() {
                        let relation = self
                            .defs
                            .canonical_case(relation)
                            .map(str::to_owned)
                            .unwrap_or_else(|| relation.clone());
                        if self.has_plugins(&relation) {
                            any = true;
                            break;
                        }
                    }
                    any
                } else {
                    false
                }
            } else if let Some(relations) = internal_relations(&declared) {
                // Internal PHP classes: reflection still reports their
                // parents + interfaces, which all get (false) entries.
                let mut any = false;
                for relation in relations {
                    if self.has_plugins(relation) {
                        any = true;
                        break;
                    }
                }
                any
            } else {
                false
            }
        };
        if result {
            self.map.insert(key, true);
        }
        result
    }

    fn resolve_vtype(&self, name: &str) -> String {
        if !self.vtypes.contains_key(name) {
            return name.to_owned();
        }
        let mut current = name;
        let mut seen = HashSet::new();
        while let Some(next) = self.vtypes.get(current) {
            if !seen.insert(*next) {
                break;
            }
            current = next;
        }
        current.to_owned()
    }
}

/// `[parent, own interfaces]` of internal PHP classes as reflection reports
/// them (PHP 8.5) — the relation names all receive map entries. Extended as
/// the oracle demands; only names actually REACHED materialize.
pub(crate) fn internal_relations(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "DOMDocument" => &["DOMNode", "DOMParentNode"],
        "FilterIterator" => &["IteratorIterator"],
        "IteratorIterator" => &["OuterIterator", "Traversable", "Iterator"],
        "RecursiveFilterIterator" => &["FilterIterator", "RecursiveIterator"],
        "SplFileObject" => &[
            "SplFileInfo",
            "RecursiveIterator",
            "Traversable",
            "Iterator",
            "SeekableIterator",
        ],
        "SplTempFileObject" => &["SplFileObject"],
        "SplFileInfo" => &["Stringable"],
        "ArrayIterator" => &[
            "SeekableIterator",
            "Traversable",
            "Iterator",
            "ArrayAccess",
            "Serializable",
            "Countable",
        ],
        "ArrayObject" => &[
            "IteratorAggregate",
            "Traversable",
            "ArrayAccess",
            "Serializable",
            "Countable",
        ],
        "SessionHandler" => &["SessionHandlerInterface", "SessionIdInterface"],
        "SimpleXMLElement" => &["Stringable", "Countable", "RecursiveIterator", "Traversable", "Iterator"],
        "Exception" => &["Throwable", "Stringable"],
        "RuntimeException" | "LogicException" | "ErrorException" | "JsonException" => {
            &["Exception"]
        }
        "InvalidArgumentException" | "DomainException" | "LengthException"
        | "BadFunctionCallException" => &["LogicException"],
        "BadMethodCallException" => &["BadFunctionCallException"],
        "OutOfRangeException" => &["LogicException"],
        "OutOfBoundsException" | "RangeException" | "OverflowException"
        | "UnderflowException" | "UnexpectedValueException" => &["RuntimeException"],
        "DateTime" | "DateTimeImmutable" => &["DateTimeInterface"],
        _ => return None,
    })
}

/// Render the map as `interception.php` content.
pub fn render(map: &BTreeMap<String, bool>) -> String {
    let value = PhpValue::Array(
        map.iter()
            .map(|(k, v)| (PhpKey::str(k.clone()), PhpValue::Bool(*v)))
            .collect(),
    );
    crate::phpexport::to_php_file(&value)
}
