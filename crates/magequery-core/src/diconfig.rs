//! The `global::DiConfig` cache entry without PHP: what
//! `App\ObjectManager\ConfigLoader::load('global')` saves on a cold cache, the
//! merged global `di.xml` as `ObjectManager\Config\Mapper\Dom` converts it,
//! PHP-serialized.
//!
//! Producing it byte for byte means porting the whole read:
//!
//! 1. `Config\Dom` merging every enabled module's `etc/di.xml` in load order with
//!    `ObjectManager\Config\Reader\Dom`'s id attributes, `xsi:type` as the type
//!    attribute;
//! 2. `Mapper\Dom::convert`, each argument through `Mapper\ArgumentParser` (the
//!    `Config\Converter\Dom\Flat` converter) and the interpreters
//!    `ObjectManagerFactory::createArgumentInterpreter` composes, array items
//!    ordered by `ObjectManager\Helper\SortItems`;
//! 3. `serialize`.
//!
//! Constants (`xsi:type="const"`, `init_parameter`) are PHP values this crate
//! cannot evaluate, so the caller supplies them. Anything this cannot reproduce
//! exactly is an `Err`: the caller leaves the entry for PHP to build.

use crate::configdom::{merge_document, parse_dom, Element, MergeConfig, Node};
use crate::phparray::PhpValue;
use crate::{Error, Magento, Result};

/// `ObjectManager\Config\Reader\Dom::$_idAttributes`.
fn id_attribute(path: &[&str]) -> Option<&'static str> {
    match path {
        ["config", "preference"] => Some("for"),
        ["config", "type" | "virtualType"] => Some("name"),
        ["config", "type" | "virtualType", "plugin"] => Some("name"),
        ["config", "type" | "virtualType", "arguments", "argument"] => Some("name"),
        ["config", "type" | "virtualType", "arguments", "argument", items @ ..]
            if !items.is_empty() && items.iter().all(|i| *i == "item") =>
        {
            Some("name")
        }
        _ => None,
    }
}

const MERGE: MergeConfig = MergeConfig { id_attribute, type_attribute: Some("xsi:type") };

type R<T> = std::result::Result<T, String>;

impl Magento {
    /// The `GLOBAL__DICONFIG` payload: `serialize()` of the merged global
    /// `di.xml` as the object manager's config loader reads it. `constant`
    /// evaluates a PHP constant name (`Class::NAME`) to its value.
    pub fn global_di_config(&self, constant: &dyn Fn(&str) -> Option<PhpValue>) -> Result<String> {
        use rayon::prelude::*;
        let mut modules: Vec<_> = self.index.modules.iter().filter(|m| m.enabled).collect();
        modules.sort_by_key(|m| m.load_order);
        // Parse in parallel; merge in load order.
        let parsed: Vec<(std::path::PathBuf, std::result::Result<Element, String>)> = modules
            .par_iter()
            .filter_map(|m| {
                let file = m.path.join("etc/di.xml");
                let text = self.index.vfs.read_to_string(&file).ok()?;
                Some((file, parse_dom(&text)))
            })
            .collect();
        let mut merged: Option<Element> = None;
        for (file, dom) in parsed {
            let dom = dom.map_err(|detail| Error::Parse { file: file.clone(), detail })?;
            match merged.as_mut() {
                None => merged = Some(dom),
                Some(base) => merge_document(&MERGE, base, dom).map_err(|detail| Error::Parse { file, detail })?,
            }
        }
        let err = |detail: String| Error::Parse { file: "etc/di.xml".into(), detail };
        let config = match merged {
            Some(root) => convert(&root, constant).map_err(err)?,
            None => PhpValue::Array(Vec::new()),
        };
        let mut out = String::new();
        serialize(&config, &mut out).map_err(err)?;
        Ok(out)
    }
}

// ---------- PHP arrays ----------

/// A PHP array key as an assignment coerces it: a canonical decimal integer
/// string becomes an int key.
fn php_key(s: &str) -> PhpValue {
    let canonical = s == "0"
        || (!s.is_empty()
            && !s.starts_with('0')
            && !s.starts_with("-0")
            && s != "-"
            && s.strip_prefix('-').unwrap_or(s).bytes().all(|b| b.is_ascii_digit()));
    match canonical.then(|| s.parse::<i64>().ok()).flatten() {
        Some(i) => PhpValue::Int(i),
        None => PhpValue::Str(s.to_owned()),
    }
}

/// `$array[$key] = $value`: in place when the key exists, appended otherwise.
fn set(items: &mut Vec<(PhpValue, PhpValue)>, key: PhpValue, value: PhpValue) {
    match items.iter_mut().find(|(k, _)| *k == key) {
        Some((_, v)) => *v = value,
        None => items.push((key, value)),
    }
}

fn get<'a>(items: &'a [(PhpValue, PhpValue)], key: &str) -> Option<&'a PhpValue> {
    items.iter().find(|(k, _)| matches!(k, PhpValue::Str(s) if s == key)).map(|(_, v)| v)
}

fn remove(items: &mut Vec<(PhpValue, PhpValue)>, key: &str) -> Option<PhpValue> {
    let i = items.iter().position(|(k, _)| matches!(k, PhpValue::Str(s) if s == key))?;
    Some(items.remove(i).1)
}

/// PHP's `trim()` character set.
fn php_trim(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

/// `(int)$string`: leading whitespace, an optional sign, then digits.
fn php_int(s: &str) -> i64 {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C']);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
    let n: i64 = digits[..end].parse().unwrap_or(0);
    if neg { -n } else { n }
}

/// `is_numeric()` for a string (PHP 8: leading and trailing whitespace allowed).
fn php_is_numeric(s: &str) -> bool {
    let t = s.trim_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C']);
    let t = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = (!int.is_empty() || !frac.is_empty()) && digits(int) && digits(frac);
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exponent_ok
}

/// `BooleanUtils::toBoolean` for an XML string.
fn to_boolean(v: &PhpValue) -> R<bool> {
    match v {
        PhpValue::Str(s) if s == "true" || s == "1" => Ok(true),
        PhpValue::Str(s) if s == "false" || s == "0" => Ok(false),
        PhpValue::Bool(b) => Ok(*b),
        other => Err(format!("boolean value expected, got {other:?}")),
    }
}

// ---------- Mapper\Dom ----------

fn attr<'a>(el: &'a Element, name: &str, what: &str) -> R<&'a str> {
    el.attr(name).ok_or_else(|| format!("<{}> without {name} ({what})", el.name))
}

fn convert(root: &Element, constant: &dyn Fn(&str) -> Option<PhpValue>) -> R<PhpValue> {
    let mut output: Vec<(PhpValue, PhpValue)> = Vec::new();
    for node in root.elements() {
        match node.name.as_str() {
            "preference" => {
                let key = PhpValue::Str("preferences".into());
                let at = match output.iter().position(|(k, _)| *k == key) {
                    Some(i) => i,
                    None => {
                        output.push((key, PhpValue::Array(Vec::new())));
                        output.len() - 1
                    }
                };
                let PhpValue::Array(prefs) = &mut output[at].1 else {
                    return Err("preferences is not an array".into());
                };
                set(prefs, php_key(attr(node, "for", "preference")?), PhpValue::Str(attr(node, "type", "preference")?.into()));
            }
            "type" | "virtualType" => {
                let mut data: Vec<(PhpValue, PhpValue)> = Vec::new();
                if let Some(shared) = node.attr("shared") {
                    data.push(("shared".into(), PhpValue::Bool(to_boolean(&PhpValue::Str(shared.into()))?)));
                }
                if node.name == "virtualType"
                    && let Some(ty) = node.attr("type")
                {
                    data.push(("type".into(), PhpValue::Str(ty.into())));
                }
                let mut arguments: Vec<(PhpValue, PhpValue)> = Vec::new();
                let mut plugins: Vec<(PhpValue, PhpValue)> = Vec::new();
                for child in node.elements() {
                    match child.name.as_str() {
                        "arguments" => {
                            for argument in child.elements() {
                                let name = attr(argument, "name", "argument")?;
                                let PhpValue::Array(parsed) = flat(argument, "argument")? else {
                                    return Err(format!("argument {name} parsed to a string"));
                                };
                                set(&mut arguments, php_key(name), evaluate(parsed, constant)?);
                            }
                        }
                        "plugin" => {
                            let mut plugin: Vec<(PhpValue, PhpValue)> =
                                vec![("sortOrder".into(), PhpValue::Int(child.attr("sortOrder").map_or(0, php_int)))];
                            if let Some(disabled) = child.attr("disabled") {
                                plugin.push(("disabled".into(), PhpValue::Bool(to_boolean(&PhpValue::Str(disabled.into()))?)));
                            }
                            if let Some(ty) = child.attr("type") {
                                plugin.push(("instance".into(), PhpValue::Str(ty.into())));
                            }
                            set(&mut plugins, php_key(attr(child, "name", "plugin")?), PhpValue::Array(plugin));
                        }
                        other => return Err(format!("unknown node {other}")),
                    }
                }
                data.push(("arguments".into(), PhpValue::Array(arguments)));
                if !plugins.is_empty() {
                    data.push(("plugins".into(), PhpValue::Array(plugins)));
                }
                set(&mut output, php_key(attr(node, "name", &node.name)?), PhpValue::Array(data));
            }
            other => return Err(format!("unknown node {other}")),
        }
    }
    Ok(PhpValue::Array(output))
}

impl From<&str> for PhpValue {
    fn from(s: &str) -> PhpValue {
        PhpValue::Str(s.to_owned())
    }
}

/// `Config\Converter\Dom\Flat::convert` with `ArgumentParser`'s one array
/// node, `argument(/item)+` keyed by `name`.
fn flat(source: &Element, base: &str) -> R<PhpValue> {
    let mut value: Vec<(PhpValue, PhpValue)> = Vec::new();
    let mut text: Option<String> = None;
    for node in &source.children {
        match node {
            Node::Element(el) => {
                let path = format!("{base}/{}", el.name);
                let is_array = path.starts_with("argument/") && path["argument".len()..].split('/').skip(1).all(|p| p == "item");
                let key = PhpValue::Str(el.name.clone());
                if !is_array && value.iter().any(|(k, _)| *k == key) {
                    return Err(format!("node path '{path}' is not unique, but it has not been marked as array"));
                }
                let data = flat(el, &path)?;
                if is_array {
                    let item_key = match &data {
                        PhpValue::Array(items) => match get(items, "name") {
                            Some(PhpValue::Str(n)) => php_key(n),
                            _ => return Err(format!("{path}: array is expected to contain value for key 'name'")),
                        },
                        _ => return Err(format!("{path}: array is expected to contain value for key 'name'")),
                    };
                    let at = match value.iter().position(|(k, _)| *k == key) {
                        Some(i) => i,
                        None => {
                            value.push((key, PhpValue::Array(Vec::new())));
                            value.len() - 1
                        }
                    };
                    match &mut value[at].1 {
                        PhpValue::Array(list) => set(list, item_key, data),
                        _ => return Err(format!("{path}: not an array")),
                    }
                } else {
                    set(&mut value, key, data);
                }
            }
            Node::Cdata(t) => {
                text = Some(t.clone());
                break;
            }
            Node::Text(t) if !php_trim(t).is_empty() => {
                text = Some(t.clone());
                break;
            }
            _ => {}
        }
    }
    let mut result: Vec<(PhpValue, PhpValue)> =
        source.attrs.iter().map(|(k, v)| (PhpValue::Str(k.clone()), PhpValue::Str(v.clone()))).collect();
    Ok(match text {
        None => {
            // array_merge: the element names are string keys, set in place.
            for (k, v) in value {
                set(&mut result, k, v);
            }
            if result.is_empty() { PhpValue::Str(String::new()) } else { PhpValue::Array(result) }
        }
        Some(t) if result.is_empty() => PhpValue::Str(php_trim(&t).to_owned()),
        Some(t) => {
            set(&mut result, "value".into(), PhpValue::Str(php_trim(&t).to_owned()));
            PhpValue::Array(result)
        }
    })
}

/// The `Composite` interpreter over `xsi:type`.
fn evaluate(mut data: Vec<(PhpValue, PhpValue)>, constant: &dyn Fn(&str) -> Option<PhpValue>) -> R<PhpValue> {
    let Some(PhpValue::Str(ty)) = remove(&mut data, "xsi:type") else {
        return Err("value for key \"xsi:type\" is missing in the argument data".into());
    };
    let value = get(&data, "value");
    Ok(match ty.as_str() {
        "boolean" => PhpValue::Bool(to_boolean(value.ok_or("boolean value is missing")?)?),
        "string" => match value {
            Some(PhpValue::Str(s)) => PhpValue::Str(s.clone()),
            Some(_) => return Err("string value is expected".into()),
            None => PhpValue::Str(String::new()),
        },
        "number" => match value {
            Some(PhpValue::Str(s)) if php_is_numeric(s) => PhpValue::Str(s.clone()),
            _ => return Err("numeric value is expected".into()),
        },
        "null" => PhpValue::Null,
        "object" => {
            let instance = value.cloned().ok_or("object without a class")?;
            let mut out = vec![("instance".into(), instance)];
            if let Some(order) = get(&data, "sortOrder") {
                out.push(("sortOrder".into(), order.clone()));
            }
            if let Some(shared) = get(&data, "shared") {
                out.push(("shared".into(), PhpValue::Bool(to_boolean(shared)?)));
            }
            PhpValue::Array(out)
        }
        "const" => const_value(value, constant)?,
        "init_parameter" => PhpValue::Array(vec![("argument".into(), const_value(value, constant)?)]),
        "array" => {
            let items = match get(&data, "item") {
                None => Vec::new(),
                Some(PhpValue::Array(items)) => items.clone(),
                Some(_) => return Err("array items are expected".into()),
            };
            let mut out = Vec::new();
            for (key, item) in sort_items(items) {
                let PhpValue::Array(item) = item else {
                    return Err("array item is not an array".into());
                };
                out.push((key, evaluate(item, constant)?));
            }
            PhpValue::Array(out)
        }
        other => return Err(format!("argument interpreter named '{other}' has not been defined")),
    })
}

fn const_value(value: Option<&PhpValue>, constant: &dyn Fn(&str) -> Option<PhpValue>) -> R<PhpValue> {
    let Some(PhpValue::Str(name)) = value else {
        return Err("constant name is expected".into());
    };
    constant(name).ok_or_else(|| format!("constant {name} could not be evaluated"))
}

// ---------- ObjectManager\Helper\SortItems ----------

fn sort_order_of(item: &PhpValue) -> Option<&PhpValue> {
    match item {
        PhpValue::Array(items) => get(items, "sortOrder").filter(|v| !matches!(v, PhpValue::Null)),
        _ => None,
    }
}

/// `(int)` of an `isset` sortOrder, 0 otherwise.
fn sort_value(item: &PhpValue) -> i64 {
    match sort_order_of(item) {
        Some(PhpValue::Str(s)) => php_int(s),
        Some(PhpValue::Int(i)) => *i,
        Some(PhpValue::Bool(b)) => i64::from(*b),
        Some(PhpValue::Float(f)) => *f as i64,
        _ => 0,
    }
}

fn sort_items(items: Vec<(PhpValue, PhpValue)>) -> Vec<(PhpValue, PhpValue)> {
    let defined = items.iter().any(|(_, v)| {
        sort_order_of(v).is_some()
            || matches!(v, PhpValue::Array(inner) if inner.iter().any(|(_, d)| sort_order_of(d).is_some()))
    });
    if !defined {
        return items;
    }
    // isMultiSortOrder: the flag the LAST inner value of the last array item sets.
    let mut multi = false;
    for (_, v) in &items {
        if let PhpValue::Array(inner) = v {
            for (_, d) in inner {
                multi = sort_order_of(d).is_some();
            }
        }
    }
    if !multi {
        // uksort is stable in PHP 8; equal sort orders keep their order.
        let mut items = items;
        items.sort_by_key(|(_, v)| sort_value(v));
        return items;
    }
    let mut flat: Vec<(PhpValue, PhpValue, PhpValue)> = Vec::new();
    for (parent, v) in items {
        if let PhpValue::Array(inner) = v {
            for (k, d) in inner {
                flat.push((parent.clone(), k, d));
            }
        }
    }
    flat.sort_by_key(|(_, _, d)| sort_value(d));
    let mut out: Vec<(PhpValue, PhpValue)> = Vec::new();
    for (parent, k, d) in flat {
        let at = match out.iter().position(|(p, _)| *p == parent) {
            Some(i) => i,
            None => {
                out.push((parent, PhpValue::Array(Vec::new())));
                out.len() - 1
            }
        };
        if let PhpValue::Array(list) = &mut out[at].1 {
            set(list, k, d);
        }
    }
    out
}

// ---------- serialize() ----------

fn serialize(v: &PhpValue, out: &mut String) -> R<()> {
    match v {
        PhpValue::Null => out.push_str("N;"),
        PhpValue::Bool(b) => out.push_str(if *b { "b:1;" } else { "b:0;" }),
        PhpValue::Int(i) => out.push_str(&format!("i:{i};")),
        PhpValue::Float(f) => {
            // serialize_precision -1: the shortest round-trip form, which Rust's
            // Display also prints, except where PHP switches to exponents.
            if !f.is_finite() || (f.abs() >= 1e15 && f.fract() == 0.0) || (*f != 0.0 && f.abs() < 1e-4) {
                return Err(format!("float {f} needs PHP's own formatting"));
            }
            out.push_str(&format!("d:{f};"));
        }
        PhpValue::Str(s) => out.push_str(&format!("s:{}:\"{s}\";", s.len())),
        PhpValue::Array(items) => {
            out.push_str(&format!("a:{}:{{", items.len()));
            for (k, v) in items {
                match k {
                    PhpValue::Int(_) | PhpValue::Str(_) => serialize(k, out)?,
                    other => return Err(format!("array key {other:?}")),
                }
                serialize(v, out)?;
            }
            out.push('}');
        }
        PhpValue::Const(c) => return Err(format!("unevaluated constant {c}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_coerce_like_php() {
        assert_eq!(php_key("0"), PhpValue::Int(0));
        assert_eq!(php_key("12"), PhpValue::Int(12));
        assert_eq!(php_key("-3"), PhpValue::Int(-3));
        assert_eq!(php_key("012"), PhpValue::Str("012".into()));
        assert_eq!(php_key("-0"), PhpValue::Str("-0".into()));
        assert_eq!(php_key("1.5"), PhpValue::Str("1.5".into()));
        assert_eq!(php_key("a"), PhpValue::Str("a".into()));
    }

    #[test]
    fn numeric_strings() {
        for s in ["1", " 1", "1 ", "-1.5", ".5", "5.", "1e3", "+2"] {
            assert!(php_is_numeric(s), "{s}");
        }
        for s in ["", ".", "e3", "1e", "abc", "1a", "--1"] {
            assert!(!php_is_numeric(s), "{s}");
        }
    }
}
