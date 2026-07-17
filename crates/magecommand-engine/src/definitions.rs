//! The scanned class universe — Magento's DefinitionsCollection: every PHP
//! declaration under the compile paths (enabled modules, libraries, setup,
//! generated code), with Magento's exclude rules, parsed once in parallel.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use magecommand_php::{ClassKind, ClassMeta, ParamMeta};
use magequery_core::Magento;
use rayon::prelude::*;

/// Which compile path a file came from — decides the exclude rule, mirroring
/// DiCompileCommand's per-kind regexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    /// `<module>/Test/**` and `<module>/tests/**` excluded.
    Module,
    /// `<lib>/([\w]+/)?Test|tests/**` excluded (one optional level).
    Library,
    /// any `<setup>/**/Test/**` excluded.
    Setup,
    /// nothing excluded.
    Generated,
}

#[derive(Clone)]
pub struct ClassRecord {
    pub meta: ClassMeta,
    pub file: PathBuf,
}

/// fqcn → declaration. Later scan paths override earlier ones on collision
/// (Magento's Collection::addCollection is array_merge). `scanned` is the
/// compile-path membership — the Reader's collection; `extend_hierarchy`
/// adds ancestor closure classes to `classes` WITHOUT admitting them here.
///
/// `Clone` so a long-running `watch` server can hold the pristine post-scan
/// index and clone it per recompute (each recompute mutates its copy via
/// `extend_hierarchy`), reusing the expensive PHP scan across edits.
#[derive(Clone)]
pub struct Definitions {
    pub classes: HashMap<String, ClassRecord>,
    pub scanned: HashSet<String>,
    /// Scanned classes that came from the setup path — the interception
    /// cache's class list covers app + lib + generated only.
    pub setup_classes: HashSet<String>,
    /// Every declaration the scan walk itself found (any kind) — the file
    /// universe PhpScanner tokenizes; `extend_hierarchy`'s closure additions
    /// never enter here.
    pub from_scan: HashSet<String>,
    /// Scan declarations that came from the generated-code path. They are
    /// compile ARTIFACTS, not source — the codegen exists-gate (Magento's
    /// `class_exists` on an empty generated dir) must not count them.
    pub generated_classes: HashSet<String>,
    /// lowercase fqcn -> declared fqcn (PHP names are case-insensitive).
    canonical: HashMap<String, String>,
}

impl Definitions {
    /// Test-only: build a `Definitions` from `(fqcn, record)` pairs, populating
    /// the case-insensitive `canonical` map. The other index sets stay empty.
    #[cfg(test)]
    pub(crate) fn from_records(
        records: impl IntoIterator<Item = (String, ClassRecord)>,
    ) -> Definitions {
        let classes: HashMap<String, ClassRecord> = records.into_iter().collect();
        let canonical =
            classes.keys().map(|k| (k.to_ascii_lowercase(), k.clone())).collect();
        Definitions {
            classes,
            scanned: HashSet::new(),
            setup_classes: HashSet::new(),
            from_scan: HashSet::new(),
            generated_classes: HashSet::new(),
            canonical,
        }
    }

    /// Scan the compile paths of `magento`, with `generated_code` supplied by
    /// the caller (the live `generated/code`, or an archived `_code` when
    /// reproducing an existing compile).
    pub fn scan(magento: &Magento, root: &Path, generated_code: &Path) -> Definitions {
        let mut roots: Vec<(PathBuf, PathKind)> = Vec::new();
        for module in magento.modules() {
            if module.enabled {
                roots.push((module.path.clone(), PathKind::Module));
            }
        }
        for lib in magento.library_paths() {
            roots.push((lib.clone(), PathKind::Library));
        }
        // DirectoryList::SETUP is 'setup/src', not 'setup/' — the tests tree
        // next to it never enters Magento's scan.
        let setup = root.join("setup/src");
        if setup.is_dir() {
            roots.push((setup, PathKind::Setup));
        }
        if generated_code.is_dir() {
            roots.push((generated_code.to_path_buf(), PathKind::Generated));
        }

        let mut files: Vec<(PathBuf, PathKind)> = Vec::new();
        for (base, kind) in &roots {
            let mut batch = Vec::new();
            collect_included(base, base, *kind, &mut batch);
            files.extend(batch.into_iter().map(|f| (f, *kind)));
        }

        let parsed: Vec<(PathBuf, PathKind, ClassMeta)> = files
            .par_iter()
            .filter_map(|(path, kind)| {
                let src = fs::read(path).ok()?;
                let meta = magecommand_php::parse_file(&src);
                Some(
                    meta.declarations
                        .into_iter()
                        .map(|d| (path.clone(), *kind, d))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect();

        let mut classes = HashMap::with_capacity(parsed.len());
        let mut setup_classes = HashSet::new();
        let mut from_scan = HashSet::new();
        let mut generated_classes = HashSet::new();
        let mut scanned = HashSet::new();
        let mut file_claimed: HashSet<PathBuf> = HashSet::new();
        for (file, kind, meta) in parsed {
            // Magento's FileClassScanner returns ONE name per file: the FIRST
            // unconditional T_CLASS/T_TRAIT at file scope ("it only searches
            // for the first match"), skipping interfaces/enums. A second
            // class in the same file (proforto's Yotpo\Reviews\Model\Config,
            // declared after a constants-holder class) never enters the
            // compile universe — no argument row, no interceptor seed — on
            // 2.4.8 AND 2.4.9 alike (source-verified on both; archive-
            // verified on proforto). `classes` still records every
            // declaration for hierarchy walks and const lookups.
            if matches!(meta.kind, ClassKind::Class | ClassKind::Trait)
                && !meta.conditional
                && file_claimed.insert(file.clone())
            {
                scanned.insert(meta.fqcn.clone());
            }
            if kind == PathKind::Setup {
                setup_classes.insert(meta.fqcn.clone());
            } else {
                // A later non-setup path re-declaring the name un-marks it.
                setup_classes.remove(&meta.fqcn);
            }
            if kind == PathKind::Generated {
                generated_classes.insert(meta.fqcn.clone());
            } else {
                generated_classes.remove(&meta.fqcn);
            }
            // A conditionally-declared class (`if (class_exists(…)) { class … }`)
            // is reflectable via autoload but is never a T_CLASS token at file
            // scope, so Magento's ClassesScanner never puts it in `$definedClasses`.
            // Keep it in `classes` (for plugin reflection) but out of the
            // interceptor/factory/proxy candidate set.
            if !meta.conditional {
                from_scan.insert(meta.fqcn.clone());
            }
            classes.insert(meta.fqcn.clone(), ClassRecord { meta, file });
        }
        let canonical = classes
            .keys()
            .map(|k| (k.to_ascii_lowercase(), k.clone()))
            .collect();
        Definitions { classes, scanned, setup_classes, from_scan, generated_classes, canonical }
    }

    /// The DECLARED spelling of a case-insensitively known class name —
    /// reflection reports declaration case, not the spelling at a use site.
    pub fn canonical_case(&self, name: &str) -> Option<&str> {
        self.canonical.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    pub fn contains(&self, fqcn: &str) -> bool {
        self.get(fqcn).is_some()
    }

    /// Case-insensitive like PHP: an exact hit wins, else the declared-case
    /// record (a use site may spell a namespace differently than the
    /// declaration — Gt\Dom vs GT\Dom).
    pub fn get(&self, fqcn: &str) -> Option<&ClassRecord> {
        if let Some(r) = self.classes.get(fqcn) {
            return Some(r);
        }
        let canonical = self.canonical.get(&fqcn.to_ascii_lowercase())?;
        self.classes.get(canonical)
    }

    /// Magento's `Type::isConcrete` = `!isInterface && !isAbstract` on the
    /// reflection — which counts TRAITS and ENUMS as concrete (traits report
    /// `isAbstract() === false`). The framework's `Interception\Interceptor`
    /// TRAIT therefore enters the compiled arguments universe and even the
    /// interceptor-substitution list; byte parity means reproducing that
    /// quirk, not fixing it.
    pub fn is_concrete(&self, fqcn: &str) -> bool {
        self.get(fqcn).is_some_and(|r| {
            r.meta.kind != ClassKind::Interface && !r.meta.is_abstract
        })
    }

    /// The constructor `fqcn` is built with (reflection semantics: its own
    /// `__construct`, else the nearest ancestor's — including internal PHP
    /// classes, via the stub table), together with the defining context —
    /// defaults with `self::`/use-map names evaluate in the definer's file.
    /// `Ok(None)` = genuinely constructor-less. `Err(name)` = the chain left
    /// the known set at `name` with no stub.
    pub fn constructor_of(&self, fqcn: &str) -> Result<Option<CtorInfo<'_>>, String> {
        let mut current = fqcn.to_owned();
        let mut hops = 0usize;
        loop {
            hops += 1;
            if hops > 64 {
                return Err(format!("inheritance cycle at {current}"));
            }
            let Some(record) = self.get(&current) else {
                if let Some(params) = internal_ctor(&current) {
                    return Ok(if params.is_empty() {
                        None
                    } else {
                        Some(CtorInfo { definer_fqcn: "", definer_uses: &[], params })
                    });
                }
                return Err(current);
            };
            if let Some(ctor) = record
                .meta
                .methods
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case("__construct"))
            {
                return Ok(Some(CtorInfo {
                    definer_fqcn: &record.meta.fqcn,
                    definer_uses: &record.meta.uses,
                    params: &ctor.params,
                }));
            }
            // A trait-provided `__construct` is flattened into the using class
            // and OUTRANKS an inherited one, so it must be found before the
            // parent walk. (PHP: current-class body > trait > inherited.)
            if let Some(info) = self.trait_ctor(&record.meta.traits) {
                return Ok(Some(info));
            }
            match record.meta.extends.first() {
                Some(parent) => current = parent.clone(),
                None => return Ok(None),
            }
        }
    }

    /// Pull the ancestor closure of every scanned class (plus `extra` names)
    /// into the set, parsing out-of-scan files via the install's autoload
    /// maps — classes extending non-scanned vendor libraries (Guzzle,
    /// Laminas, …) still have real inherited constructors under reflection.
    /// Returns the names that could not be resolved anywhere.
    pub fn extend_hierarchy(
        &mut self,
        magento: &Magento,
        root: &Path,
        extra: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        // Classmap-autoloaded packages (colinmollenhour/*, phpunit, …) have
        // no PSR prefix; composer's dumped classmap is the truth for those.
        let classmap: HashMap<String, PathBuf> = {
            let path = root.join("vendor/composer/autoload_classmap.php");
            let text = fs::read_to_string(&path).unwrap_or_default();
            let vendor = root.join("vendor");
            let base = root.to_path_buf();
            text.lines()
                .filter_map(|line| {
                    let line = line.trim();
                    let (name, rest) = line.strip_prefix('\'')?.split_once("' => ")?;
                    let (dir, rel) = if let Some(r) = rest.strip_prefix("$vendorDir . '") {
                        (&vendor, r)
                    } else if let Some(r) = rest.strip_prefix("$baseDir . '") {
                        (&base, r)
                    } else {
                        return None;
                    };
                    let rel = rel.strip_suffix("',")?;
                    Some((
                        name.replace("\\\\", "\\"),
                        dir.join(rel.trim_start_matches('/')),
                    ))
                })
                .collect()
        };
        let mut queue: Vec<String> = self
            .classes
            .values()
            .flat_map(|r| {
                let supers = r.meta.extends.iter().chain(r.meta.implements.iter()).cloned();
                // Constructor param class-hints too: reflection reports their
                // DECLARED case, so the hinted files must be parsed even when
                // no scanned class inherits from them.
                let hints = r
                    .meta
                    .methods
                    .iter()
                    .filter(|m| m.name.eq_ignore_ascii_case("__construct"))
                    .flat_map(|m| m.params.iter())
                    .filter_map(|param| {
                        let ty = param.ty.as_deref()?;
                        let ty = ty.strip_prefix('?').unwrap_or(ty);
                        if ty.contains('|') || ty.contains('&') || ty.contains('(') || !ty.contains('\\') {
                            return None;
                        }
                        Some(ty.to_owned())
                    })
                    .collect::<Vec<_>>();
                supers.chain(hints)
            })
            .chain(extra)
            .collect();
        let mut unresolved = Vec::new();
        let mut attempted: HashSet<String> = HashSet::new();
        while let Some(name) = queue.pop() {
            if self.get(&name).is_some() || !attempted.insert(name.clone()) {
                continue;
            }
            let class_name = magequery_core::ClassName::new(name.clone());
            let resolved = magento
                .class_file(&class_name)
                .or_else(|| classmap.get(&name).cloned());
            let Some(file) = resolved else {
                // Global-namespace names are PHP built-ins (stubbed or not);
                // only namespaced unresolvables are interesting.
                if internal_ctor(&name).is_none() && name.contains('\\') {
                    unresolved.push(name);
                }
                continue;
            };
            let Ok(src) = fs::read(&file) else {
                unresolved.push(name);
                continue;
            };
            for decl in magecommand_php::parse_file(&src).declarations {
                queue.extend(decl.extends.iter().cloned());
                queue.extend(decl.implements.iter().cloned());
                self.canonical
                    .insert(decl.fqcn.to_ascii_lowercase(), decl.fqcn.clone());
                self.classes
                    .insert(decl.fqcn.clone(), ClassRecord { meta: decl, file: file.clone() });
            }
        }
        unresolved.sort();
        unresolved
    }

    /// All interfaces of `fqcn`, transitive, in PHP's `class_implements`
    /// order (verified empirically): the class's DECLARED interfaces in
    /// order, then each declared interface's ancestor table — where a
    /// table lists an interface's parents BEFORE the interface itself —
    /// then the parent class's interfaces.
    pub fn all_interfaces(&self, fqcn: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let Some(record) = self.get(fqcn) else {
            return out;
        };
        if record.meta.kind == ClassKind::Interface {
            for parent in record.meta.extends.iter().rev() {
                self.iface_table(parent, &mut out, &mut seen, 0);
                if seen.insert(parent.clone()) {
                    out.push(parent.clone());
                }
            }
            return out;
        }
        let mut current = Some(record);
        let mut hops = 0;
        while let Some(rec) = current {
            hops += 1;
            if hops > 64 {
                break;
            }
            for iface in &rec.meta.implements {
                if seen.insert(iface.clone()) {
                    out.push(iface.clone());
                }
            }
            for iface in &rec.meta.implements {
                self.iface_table(iface, &mut out, &mut seen, 0);
            }
            // PHP auto-implements Stringable on any class DECLARING
            // __toString — the engine appends it to that class's table.
            if rec
                .meta
                .methods
                .iter()
                .any(|m| m.name.eq_ignore_ascii_case("__toString"))
                && seen.insert("Stringable".to_owned())
            {
                out.push("Stringable".to_owned());
            }
            current = rec
                .meta
                .extends
                .first()
                .and_then(|parent| self.get(parent));
        }
        out
    }

    /// The ancestor table of one interface: for each extended interface,
    /// ITS table first, then the interface itself. Internal PHP interfaces
    /// come from the stub hierarchy (Iterator extends Traversable, …).
    fn iface_table(&self, iface: &str, out: &mut Vec<String>, seen: &mut HashSet<String>, depth: usize) {
        if depth > 64 {
            return;
        }
        let parents: Vec<String> = match self.get(iface) {
            Some(record) => record.meta.extends.clone(),
            None => internal_iface_parents(iface)
                .map(|p| p.iter().map(|s| (*s).to_owned()).collect())
                .unwrap_or_default(),
        };
        // The engine linearizes an interface's extends list in REVERSE
        // declaration order (verified empirically: StorageInterface extends
        // UrlFinderInterface, UrlPersistInterface -> [Persist, Finder]).
        for parent in parents.iter().rev() {
            self.iface_table(parent, out, seen, depth + 1);
            if seen.insert(parent.clone()) {
                out.push(parent.clone());
            }
        }
    }

    /// Magento's `ClassReader::getParents` (the Relations source): the parent
    /// class plus the class's OWN interfaces (all_interfaces minus the
    /// parent's). `None` entries are already dropped.
    pub fn relations_of(&self, fqcn: &str) -> Vec<String> {
        let Some(record) = self.get(fqcn) else {
            return Vec::new();
        };
        let parent = record
            .meta
            .extends
            .first()
            .filter(|_| record.meta.kind != ClassKind::Interface)
            .cloned();
        let own_ifaces: Vec<String> = match &parent {
            Some(p) => {
                let parent_set: HashSet<String> = self.all_interfaces(p).into_iter().collect();
                self.all_interfaces(fqcn)
                    .into_iter()
                    .filter(|i| !parent_set.contains(i))
                    .collect()
            }
            None => self.all_interfaces(fqcn),
        };
        let mut out = Vec::new();
        if let Some(p) = parent {
            out.push(p);
        }
        out.extend(own_ifaces);
        out
    }
}

impl Definitions {
    /// Flattened PUBLIC method names of a class as `get_class_methods` sees
    /// them: own + trait-imported (aliases included, `as protected` hides)
    /// + inherited, nearest definition wins, declared case preserved.
    pub fn public_methods(&self, fqcn: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut current = self.get(fqcn);
        let mut hops = 0;
        while let Some(record) = current {
            hops += 1;
            if hops > 64 {
                break;
            }
            let meta = &record.meta;
            for m in &meta.methods {
                if m.visibility == magecommand_php::Visibility::Public
                    && seen.insert(m.name.to_ascii_lowercase())
                {
                    out.push(m.name.clone());
                }
            }
            // Trait aliases first (they may rename/hide), then trait methods.
            for alias in &meta.trait_aliases {
                let public = alias
                    .visibility
                    .map(|v| v == magecommand_php::Visibility::Public)
                    .unwrap_or(true);
                if let Some(new_name) = &alias.alias {
                    if public && seen.insert(new_name.to_ascii_lowercase()) {
                        out.push(new_name.clone());
                    }
                } else if !public {
                    // visibility-only change hides the original name
                    seen.insert(alias.method.to_ascii_lowercase());
                }
            }
            let mut trait_stack: Vec<String> = meta.traits.clone();
            let mut texpanded: HashSet<String> = HashSet::new();
            while let Some(t) = trait_stack.pop() {
                if !texpanded.insert(t.clone()) {
                    continue;
                }
                if let Some(tr) = self.get(&t) {
                    for m in &tr.meta.methods {
                        if m.visibility == magecommand_php::Visibility::Public
                            && seen.insert(m.name.to_ascii_lowercase())
                        {
                            out.push(m.name.clone());
                        }
                    }
                    trait_stack.extend(tr.meta.traits.iter().cloned());
                }
            }
            current = meta
                .extends
                .first()
                .filter(|_| meta.kind != ClassKind::Interface)
                .and_then(|p| self.get(p));
        }
        out
    }
}

impl Definitions {
    /// Find a `__construct` provided by any of `traits`, transitively (a trait
    /// may `use` another). The returned [`CtorInfo`] cites the **trait** as
    /// definer, so its parameter type hints resolve against the trait file's
    /// namespace + `use` imports — PHP compiles a trait's signatures in the
    /// trait's own context, not the using class's. Outer traits are checked
    /// before the traits they use (a trait's own method beats an imported one);
    /// a genuine multi-trait `__construct` clash is a PHP fatal error, so any
    /// deterministic pick is fine.
    fn trait_ctor(&self, traits: &[String]) -> Option<CtorInfo<'_>> {
        let mut stack: Vec<String> = traits.to_vec();
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(t) = stack.pop() {
            if !seen.insert(t.clone()) {
                continue;
            }
            let Some(rec) = self.get(&t) else { continue };
            if let Some(ctor) =
                rec.meta.methods.iter().find(|m| m.name.eq_ignore_ascii_case("__construct"))
            {
                return Some(CtorInfo {
                    definer_fqcn: &rec.meta.fqcn,
                    definer_uses: &rec.meta.uses,
                    params: &ctor.params,
                });
            }
            stack.extend(rec.meta.traits.iter().cloned());
        }
        None
    }
}

/// A constructor found by walking the inheritance chain.
pub struct CtorInfo<'a> {
    /// FQCN of the class whose file declares the `__construct` (name
    /// resolution context for default expressions); empty for internal PHP
    /// constructors (their defaults are plain literals).
    pub definer_fqcn: &'a str,
    pub definer_uses: &'a [(String, String)],
    pub params: &'a [ParamMeta],
}

/// `extends` lists of internal PHP interfaces (the engine's linearization
/// puts an interface's parents before it in class_implements tables).
fn internal_iface_parents(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "Iterator" | "IteratorAggregate" => &["Traversable"],
        "SeekableIterator" | "RecursiveIterator" | "OuterIterator" => &["Iterator"],
        "Throwable" => &["Stringable"],
        "BackedEnum" => &["UnitEnum"],
        "Traversable" | "ArrayAccess" | "Countable" | "Serializable" | "Stringable"
        | "JsonSerializable" | "SessionHandlerInterface" | "SessionIdInterface"
        | "DOMParentNode" | "DOMChildNode" | "UnitEnum" | "DateTimeInterface" => &[],
        _ => return None,
    })
}

/// Constructor signatures of internal PHP classes, as reflection reports
/// them — inherited by userland subclasses that declare none of their own.
/// Empty slice = constructor-less (or a constructor Magento never resolves
/// arguments against). Extended as the compiled oracle demands.
fn internal_ctor(fqcn: &str) -> Option<&'static [ParamMeta]> {
    use std::sync::OnceLock;
    static EXCEPTION_CTOR: OnceLock<Vec<ParamMeta>> = OnceLock::new();
    let exception_ctor = || {
        EXCEPTION_CTOR
            .get_or_init(|| {
                vec![
                    ParamMeta::synthetic("message", Some("string"), Some("''")),
                    ParamMeta::synthetic("code", Some("int"), Some("0")),
                    ParamMeta::synthetic("previous", Some("?Throwable"), Some("null")),
                ]
            })
            .as_slice()
    };
    static FILTER_ITERATOR_CTOR: OnceLock<Vec<ParamMeta>> = OnceLock::new();
    let filter_iterator_ctor = || {
        FILTER_ITERATOR_CTOR
            .get_or_init(|| vec![ParamMeta::synthetic("iterator", Some("Iterator"), None)])
            .as_slice()
    };
    static ARRAY_ITERATOR_CTOR: OnceLock<Vec<ParamMeta>> = OnceLock::new();
    let array_iterator_ctor = || {
        ARRAY_ITERATOR_CTOR
            .get_or_init(|| {
                vec![
                    ParamMeta::synthetic("array", Some("array|object"), Some("[]")),
                    ParamMeta::synthetic("flags", Some("int"), Some("0")),
                ]
            })
            .as_slice()
    };
    static ARRAY_OBJECT_CTOR: OnceLock<Vec<ParamMeta>> = OnceLock::new();
    let array_object_ctor = || {
        ARRAY_OBJECT_CTOR
            .get_or_init(|| {
                vec![
                    ParamMeta::synthetic("array", Some("array|object"), Some("[]")),
                    ParamMeta::synthetic("flags", Some("int"), Some("0")),
                    ParamMeta::synthetic("iteratorClass", Some("string"), Some("'ArrayIterator'")),
                ]
            })
            .as_slice()
    };
    static SIMPLEXML_CTOR: OnceLock<Vec<ParamMeta>> = OnceLock::new();
    let simplexml_ctor = || {
        SIMPLEXML_CTOR
            .get_or_init(|| {
                vec![
                    ParamMeta::synthetic("data", Some("string"), None),
                    ParamMeta::synthetic("options", Some("int"), Some("0")),
                    ParamMeta::synthetic("dataIsURL", Some("bool"), Some("false")),
                    ParamMeta::synthetic("namespaceOrPrefix", Some("string"), Some("''")),
                    ParamMeta::synthetic("isPrefix", Some("bool"), Some("false")),
                ]
            })
            .as_slice()
    };
    static SPL_FILE_OBJECT_CTOR: OnceLock<Vec<ParamMeta>> = OnceLock::new();
    let spl_file_object_ctor = || {
        SPL_FILE_OBJECT_CTOR
            .get_or_init(|| {
                vec![
                    ParamMeta::synthetic("filename", Some("string"), None),
                    ParamMeta::synthetic("mode", Some("string"), Some("'r'")),
                    ParamMeta::synthetic("useIncludePath", Some("bool"), Some("false")),
                    ParamMeta::synthetic("context", None, Some("null")),
                ]
            })
            .as_slice()
    };
    static SPL_FILE_INFO_CTOR: OnceLock<Vec<ParamMeta>> = OnceLock::new();
    let spl_file_info_ctor = || {
        SPL_FILE_INFO_CTOR
            .get_or_init(|| vec![ParamMeta::synthetic("filename", Some("string"), None)])
            .as_slice()
    };
    static EMPTY: [ParamMeta; 0] = [];
    Some(match fqcn {
        "SplFileObject" | "SplTempFileObject" => spl_file_object_ctor(),
        "SplFileInfo" => spl_file_info_ctor(),
        "FilterIterator" | "RecursiveFilterIterator" => filter_iterator_ctor(),
        "SimpleXMLElement" | "SimpleXMLIterator" => simplexml_ctor(),
        "ArrayIterator" | "RecursiveArrayIterator" => array_iterator_ctor(),
        "ArrayObject" => array_object_ctor(),
        // The \Exception family shares Exception::__construct.
        "Exception" | "RuntimeException" | "LogicException" | "InvalidArgumentException"
        | "DomainException" | "OutOfBoundsException" | "OutOfRangeException"
        | "LengthException" | "RangeException" | "UnexpectedValueException"
        | "BadMethodCallException" | "BadFunctionCallException" | "OverflowException"
        | "UnderflowException" | "TypeError" | "ValueError" | "Error"
        | "JsonException" => exception_ctor(),
        "stdClass" | "Directory" | "SessionHandler" | "php_user_filter" | "XMLReader"
        | "XMLWriter" | "Locale" | "SplObjectStorage" | "SplQueue" | "SplStack"
        | "SplPriorityQueue" | "SplDoublyLinkedList" | "SplMinHeap" | "SplMaxHeap"
        | "DateTime" | "DateTimeImmutable"
        | "DateTimeZone" | "DateInterval" | "IntlDateFormatter" | "NumberFormatter"
        | "Collator" | "ReflectionClass" | "SoapClient" | "SoapServer"
        | "DOMDocument" | "PDO"
        | "IteratorIterator" | "RecursiveIteratorIterator" | "ErrorException"
        | "Magento\\Framework\\Interception\\Interceptor" => &EMPTY,
        _ => return None,
    })
}


fn collect_included(base: &Path, dir: &Path, kind: PathKind, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if excluded(base, &path, kind) {
            continue;
        }
        if ft.is_dir() {
            collect_included(base, &path, kind, out);
        } else if path.extension().is_some_and(|e| e == "php")
            && !path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            out.push(path);
        }
    }
}

/// Every file the compile reads as an INPUT — for content-addressed change
/// detection (the incremental short-circuit + the CI cache key). The `.php` the
/// scan parses (constructor/hierarchy/plugin headers) PLUS the `.xml` that
/// drives DI (di.xml, module.xml, extension_attributes.xml, …), under the SAME
/// roots and the SAME exclusion rules as [`Definitions::scan`] so the set
/// matches exactly what the compile consumes — plus the deployment/composer
/// files (config.php, env.php, installed.json, composer.json/lock) and the
/// primary `app/etc` di. Deliberately NOT `generated/code` (that is output).
///
/// Soundness over speed: over-covering (e.g. a module's `events.xml`, which DI
/// compile ignores) only causes an unnecessary recompile; UNDER-covering would
/// serve a stale result. Returned sorted + deduped for a stable digest.
pub fn compile_input_files(magento: &Magento, root: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<(PathBuf, PathKind)> = Vec::new();
    for module in magento.modules() {
        if module.enabled {
            roots.push((module.path.clone(), PathKind::Module));
        }
    }
    for lib in magento.library_paths() {
        roots.push((lib.clone(), PathKind::Library));
    }
    let setup = root.join("setup/src");
    if setup.is_dir() {
        roots.push((setup, PathKind::Setup));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    for (base, kind) in &roots {
        collect_input_files(base, base, *kind, &mut files);
    }
    // Deployment + composer inputs; app/etc holds the primary di glob
    // (`{*di.xml,*/*di.xml}`) + config.php, which have no module base to
    // exclude against — collect them directly.
    let app_etc = root.join("app/etc");
    collect_input_files(&app_etc, &app_etc, PathKind::Module, &mut files);
    for rel in [
        "app/etc/config.php",
        "app/etc/env.php",
        "vendor/composer/installed.json",
        "composer.json",
        "composer.lock",
    ] {
        let p = root.join(rel);
        if p.is_file() {
            files.push(p);
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Like [`collect_included`] but keeps `.php` AND `.xml` — the DI config is XML.
fn collect_input_files(base: &Path, dir: &Path, kind: PathKind, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if excluded(base, &path, kind) {
            continue;
        }
        if ft.is_dir() {
            collect_input_files(base, &path, kind, out);
        } else if path
            .extension()
            .is_some_and(|e| e == "php" || e == "xml")
            && !path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            out.push(path);
        }
    }
}

/// Mirror DiCompileCommand's exclude regexes. They are PREFIX matches with
/// no trailing slash: `#^<module>/Test#` also kills `TestFramework/…` and a
/// stray `Testify.php` — reproduced faithfully, for files and dirs alike.
fn excluded(base: &Path, path: &Path, kind: PathKind) -> bool {
    let Ok(rel) = path.strip_prefix(base) else {
        return false;
    };
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let hit = |s: &str| s.starts_with("Test") || s.starts_with("tests");
    match kind {
        PathKind::Module => comps.first().is_some_and(|c| hit(c)),
        // `#^<lib>/([\w]+/)?Test|tests#` — depth 1 or 2.
        PathKind::Library => {
            comps.first().is_some_and(|c| hit(c))
                || (comps.len() >= 2 && hit(comps[1]))
        }
        // `#^<setup>(/[\w]+)*/Test#` — capital-T Test at any depth.
        PathKind::Setup => comps.iter().any(|c| c.starts_with("Test")),
        PathKind::Generated => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one PHP declaration into a [`ClassRecord`] (no source file).
    fn record(src: &str) -> (String, ClassRecord) {
        let meta = magecommand_php::parse_file(src.as_bytes())
            .declarations
            .into_iter()
            .next()
            .expect("one declaration");
        (meta.fqcn.clone(), ClassRecord { meta, file: PathBuf::new() })
    }

    fn defs(records: impl IntoIterator<Item = (String, ClassRecord)>) -> Definitions {
        Definitions {
            classes: records.into_iter().collect(),
            scanned: HashSet::new(),
            setup_classes: HashSet::new(),
            from_scan: HashSet::new(),
            generated_classes: HashSet::new(),
            canonical: HashMap::new(),
        }
    }

    #[test]
    fn trait_constructor_outranks_inherited() {
        // C has no ctor of its own; it `use`s a trait that provides one, and
        // extends a parent that also has one. PHP: trait beats inherited.
        let defs = defs([
            record("<?php trait T { public function __construct($b, $c) {} }"),
            record("<?php class P { public function __construct($a) {} }"),
            record("<?php class C extends P { use T; }"),
        ]);
        let ctor = defs.constructor_of("C").unwrap().expect("a constructor");
        let names: Vec<&str> = ctor.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["b", "c"], "trait ctor forwarded, not the parent's");
        assert_eq!(ctor.definer_fqcn, "T", "definer is the trait (its use-context)");
    }

    #[test]
    fn own_constructor_beats_trait() {
        // An explicit body __construct wins over the trait's.
        let defs = defs([
            record("<?php trait T { public function __construct($b) {} }"),
            record("<?php class C { use T; public function __construct($own) {} }"),
        ]);
        let ctor = defs.constructor_of("C").unwrap().expect("a constructor");
        let names: Vec<&str> = ctor.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["own"]);
        assert_eq!(ctor.definer_fqcn, "C");
    }

    #[test]
    fn public_methods_reflection_order_own_traits_nested_parent() {
        // Mirrors the Acme_TraitOrder oracle fixture, verified byte-exact
        // against a real `setup:di:compile` interceptor. Reflection lists a
        // class's own public methods (source order), then each `use`d trait in
        // DECLARATION order — its own methods, then its nested traits, pre-order
        // — then the parent chain. A trait/own method shadowing a parent's keeps
        // the earlier (nearer) position. The earlier stack/`pop` walk reversed
        // sibling traits, so this would have emitted `secondA` before `firstA`.
        let defs = defs([
            record("<?php trait NestedT { public function nestedX() {} }"),
            record(
                "<?php trait FirstT { use NestedT; public function firstA() {} public function firstB() {} }",
            ),
            record("<?php trait SecondT { public function secondA() {} }"),
            record("<?php class P { public function baseAlpha() {} public function baseShared() {} }"),
            record(
                "<?php class C extends P { use FirstT, SecondT; public function ownOne() {} public function baseShared() {} }",
            ),
        ]);
        let names: Vec<String> =
            crate::reflect::public_methods(&defs, "C").into_iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            ["ownOne", "baseShared", "firstA", "firstB", "nestedX", "secondA", "baseAlpha"]
        );
    }

    #[test]
    fn trait_constructor_found_transitively() {
        // A trait that `use`s another trait which provides the ctor.
        let defs = defs([
            record("<?php trait Inner { public function __construct($x) {} }"),
            record("<?php trait Outer { use Inner; }"),
            record("<?php class C { use Outer; }"),
        ]);
        let ctor = defs.constructor_of("C").unwrap().expect("a constructor");
        let names: Vec<&str> = ctor.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["x"]);
        assert_eq!(ctor.definer_fqcn, "Inner");
    }
}
