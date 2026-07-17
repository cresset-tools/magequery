//! `interception.php` — Magento's `Interception\Config\Config::initialize`
//! ported: for every type the compile touches, whether an interceptor
//! applies (plugins declared on it, its real type, or any ancestor).
//! The compiled cache writer sorts keys, so only set + values matter.

use std::collections::{BTreeMap, HashMap, HashSet};

use magequery_core::{DiExport, Magento};

use crate::areaconfig::AREA_CODES;
use crate::definitions::Definitions;
use crate::phpexport::{PhpKey, PhpValue};

pub fn interception_map(magento: &Magento, defs: &Definitions) -> BTreeMap<String, bool> {
    let exports: Vec<DiExport> = AREA_CODES
        .iter()
        .map(|(area, _)| magento.di_export(*area))
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
        map: HashMap::new(),
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
    for class in &defs.scanned {
        if !defs.setup_classes.contains(class) {
            resolver.has_plugins(class);
        }
    }

    resolver.map.into_iter().collect()
}

struct Resolver<'a> {
    defs: &'a Definitions,
    vtypes: HashMap<&'a str, &'a str>,
    map: HashMap<String, bool>,
}

impl Resolver<'_> {
    /// `Config::_inheritInterception`, memoized into the map (every type
    /// touched — ancestors and interfaces included — gets an entry). Keys
    /// use the DECLARED case (reflection's view), and only CLASSES inherit:
    /// `Relations\Runtime::getParents` bails when `class_exists` is false,
    /// so interfaces, traits, and unknowns never walk their ancestry.
    fn has_plugins(&mut self, type_name: &str) -> bool {
        let key = self
            .defs
            .canonical_case(type_name)
            .unwrap_or(type_name)
            .to_owned();
        if let Some(&v) = self.map.get(&key) {
            return v;
        }
        // Guard against cycles before recursing.
        self.map.insert(key.clone(), false);

        let real = self.resolve_vtype(&key);
        let result = if real != key {
            self.has_plugins(&real)
        } else {
            // Classes whose last segment is exactly `Interceptor` never
            // inherit interception (`_serviceClassTypes`).
            let last = key.rsplit('\\').next().unwrap_or(&key);
            if last == "Interceptor" {
                false
            } else if let Some(record) = self.defs.get(&key) {
                if record.meta.kind == magecommand_php::ClassKind::Class {
                    let relations = self.defs.relations_of(&key);
                    let mut any = false;
                    for relation in relations {
                        if self.has_plugins(&relation) {
                            any = true;
                            break;
                        }
                    }
                    any
                } else {
                    false
                }
            } else if let Some(relations) = internal_relations(&key) {
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
