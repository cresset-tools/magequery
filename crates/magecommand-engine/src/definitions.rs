//! The scanned class universe — Magento's DefinitionsCollection: every PHP
//! declaration under the compile paths (enabled modules, libraries, setup,
//! generated code), with Magento's exclude rules, parsed once in parallel.

use std::collections::HashMap;
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

/// fqcn → declaration, plus scan order. Later scan paths override earlier
/// ones on collision (Magento's Collection::addCollection is array_merge).
pub struct Definitions {
    pub classes: HashMap<String, ClassRecord>,
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
        let setup = root.join("setup");
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
        Definitions { classes }
    }

    pub fn contains(&self, fqcn: &str) -> bool {
        self.classes.contains_key(fqcn)
    }

    pub fn get(&self, fqcn: &str) -> Option<&ClassRecord> {
        self.classes.get(fqcn)
    }

    /// Magento's `Type::isConcrete` = `!isInterface && !isAbstract` on the
    /// reflection — which counts TRAITS and ENUMS as concrete (traits report
    /// `isAbstract() === false`). The framework's `Interception\Interceptor`
    /// TRAIT therefore enters the compiled arguments universe and even the
    /// interceptor-substitution list; byte parity means reproducing that
    /// quirk, not fixing it.
    pub fn is_concrete(&self, fqcn: &str) -> bool {
        self.classes.get(fqcn).is_some_and(|r| {
            r.meta.kind != ClassKind::Interface && !r.meta.is_abstract
        })
    }

    /// The constructor `fqcn` is built with: its own `__construct`, else the
    /// nearest ancestor's (reflection semantics), else an internal-class stub
    /// (`\Exception` etc.). `Ok(None)` = genuinely constructor-less.
    /// `Err(parent)` = the chain left the scanned set at `parent` with no
    /// stub — a hard finding, not a guess.
    pub fn constructor_of(&self, fqcn: &str) -> Result<Option<&[ParamMeta]>, String> {
        let mut current = fqcn.to_owned();
        let mut hops = 0usize;
        loop {
            hops += 1;
            if hops > 64 {
                return Err(format!("inheritance cycle at {current}"));
            }
            let Some(record) = self.classes.get(&current) else {
                if internal_ctor_is_empty(&current) {
                    return Ok(None);
                }
                return Err(current);
            };
            if let Some(ctor) = record
                .meta
                .methods
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case("__construct"))
            {
                return Ok(Some(&ctor.params));
            }
            match record.meta.extends.first() {
                Some(parent) => current = parent.clone(),
                None => return Ok(None),
            }
        }
    }
}

/// Internal PHP classes whose inherited constructors are irrelevant to DI
/// metadata (Magento never configures them) or genuinely parameterless.
/// The compiled oracle decides if this list ever needs real signatures.
fn internal_ctor_is_empty(fqcn: &str) -> bool {
    matches!(
        fqcn,
        "Exception"
            | "RuntimeException"
            | "LogicException"
            | "InvalidArgumentException"
            | "DomainException"
            | "OutOfBoundsException"
            | "OutOfRangeException"
            | "LengthException"
            | "RangeException"
            | "UnexpectedValueException"
            | "BadMethodCallException"
            | "BadFunctionCallException"
            | "OverflowException"
            | "UnderflowException"
            | "ErrorException"
            | "TypeError"
            | "ValueError"
            | "ArrayObject"
            | "ArrayIterator"
            | "SplObjectStorage"
            | "SplQueue"
            | "SplStack"
            | "SplPriorityQueue"
            | "SplFileInfo"
            | "SplFileObject"
            | "DateTime"
            | "DateTimeImmutable"
            | "DateTimeZone"
            | "DateInterval"
            | "IntlDateFormatter"
            | "NumberFormatter"
            | "Locale"
            | "Collator"
            | "ReflectionClass"
            | "SoapClient"
            | "SoapServer"
            | "SimpleXMLElement"
            | "XMLReader"
            | "XMLWriter"
            | "DOMDocument"
            | "PDO"
            | "stdClass"
            | "Directory"
            | "SessionHandler"
            | "php_user_filter"
            | "FilterIterator"
            | "IteratorIterator"
            | "RecursiveIteratorIterator"
            | "Magento\\Framework\\Interception\\Interceptor" // trait, in libs anyway
    )
}

fn collect_included(base: &Path, dir: &Path, kind: PathKind, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if excluded_dir(base, &path, kind) {
                continue;
            }
            collect_included(base, &path, kind, out);
        } else if path.extension().is_some_and(|e| e == "php") {
            out.push(path);
        }
    }
}

/// Mirror DiCompileCommand's exclude regexes structurally.
fn excluded_dir(base: &Path, dir: &Path, kind: PathKind) -> bool {
    let Ok(rel) = dir.strip_prefix(base) else {
        return false;
    };
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let is_test = |s: &str| s == "Test" || s == "tests";
    match kind {
        PathKind::Module => comps.len() == 1 && is_test(comps[0]),
        // `<lib>/Test`, `<lib>/tests`, `<lib>/<word>/Test`, `<lib>/<word>/tests`
        PathKind::Library => {
            (comps.len() == 1 && is_test(comps[0])) || (comps.len() == 2 && is_test(comps[1]))
        }
        PathKind::Setup => comps.last().is_some_and(|c| *c == "Test"),
        PathKind::Generated => false,
    }
}
