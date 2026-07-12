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

pub struct ClassRecord {
    pub meta: ClassMeta,
    pub file: PathBuf,
}

/// fqcn → declaration. Later scan paths override earlier ones on collision
/// (Magento's Collection::addCollection is array_merge). `scanned` is the
/// compile-path membership — the Reader's collection; `extend_hierarchy`
/// adds ancestor closure classes to `classes` WITHOUT admitting them here.
pub struct Definitions {
    pub classes: HashMap<String, ClassRecord>,
    pub scanned: HashSet<String>,
    /// lowercase fqcn -> declared fqcn (PHP names are case-insensitive).
    canonical: HashMap<String, String>,
}

impl Definitions {
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

        let mut files: Vec<PathBuf> = Vec::new();
        for (base, kind) in &roots {
            collect_included(base, base, *kind, &mut files);
        }

        let parsed: Vec<(PathBuf, ClassMeta)> = files
            .par_iter()
            .filter_map(|path| {
                let src = fs::read(path).ok()?;
                let meta = magecommand_php::parse_file(&src);
                Some(
                    meta.declarations
                        .into_iter()
                        .map(|d| (path.clone(), d))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect();

        let mut classes = HashMap::with_capacity(parsed.len());
        for (file, meta) in parsed {
            classes.insert(meta.fqcn.clone(), ClassRecord { meta, file });
        }
        // Magento's FileClassScanner recognizes class/interface/trait tokens
        // but NOT `enum` — enums never enter the compile universe (they stay
        // in `classes` for constant lookups and hierarchy walks).
        let scanned = classes
            .iter()
            .filter(|(_, r)| r.meta.kind != ClassKind::Enum)
            .map(|(k, _)| k.clone())
            .collect();
        let canonical = classes
            .keys()
            .map(|k| (k.to_ascii_lowercase(), k.clone()))
            .collect();
        Definitions { classes, scanned, canonical }
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
        extra: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
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
            let Some(file) = magento.class_file(&class_name) else {
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

    /// All interfaces of `fqcn`, transitive (PHP's `class_implements`):
    /// direct implements + interface extends, then the parent chain's, in
    /// discovery order, deduped.
    pub fn all_interfaces(&self, fqcn: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut current = Some(fqcn.to_owned());
        let mut hops = 0;
        while let Some(name) = current {
            hops += 1;
            if hops > 64 {
                break;
            }
            let Some(record) = self.get(&name) else { break };
            let mut stack: Vec<String> = if record.meta.kind == ClassKind::Interface {
                record.meta.extends.clone()
            } else {
                record.meta.implements.clone()
            };
            stack.reverse();
            while let Some(iface) = stack.pop() {
                if seen.insert(iface.clone()) {
                    out.push(iface.clone());
                    if let Some(r) = self.get(&iface) {
                        for parent in r.meta.extends.iter().rev() {
                            stack.push(parent.clone());
                        }
                    }
                }
            }
            current = record.meta.extends.first().cloned().filter(|_| {
                record.meta.kind != ClassKind::Interface
            });
        }
        out
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

/// A constructor found by walking the inheritance chain.
pub struct CtorInfo<'a> {
    /// FQCN of the class whose file declares the `__construct` (name
    /// resolution context for default expressions); empty for internal PHP
    /// constructors (their defaults are plain literals).
    pub definer_fqcn: &'a str,
    pub definer_uses: &'a [(String, String)],
    pub params: &'a [ParamMeta],
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
