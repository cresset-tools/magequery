//! `config status`: a native `bin/magento app:config:status`.
//!
//! Magento's `ChangeDetector` takes each section an importer is registered for
//! (`ImporterPool`'s `importers` argument in di.xml: `scopes`, `themes` and
//! `system` in core), reads that section of the deployment config, and compares
//! `sha1(json_encode($section))` with the hash `app:config:import` stored in the
//! `flag` table (`flag_code = 'config_hash'`). Any difference means an import is
//! needed (exit 2).
//!
//! The deployment config is `config.php` with `env.php` laid over it by
//! `array_replace_recursive`, so reproducing the hash means reproducing PHP's
//! array semantics (integer-like string keys become ints, later duplicate keys
//! win in the first one's place) and `json_encode`'s output byte for byte.
//! Anything that can't be reproduced exactly (a constant reference, an unusual
//! float, `MAGENTO_DC__OVERRIDE`) is an [`Error::Unsupported`], so a caller can
//! fall back to PHP rather than guess.

use std::fmt::Write as _;

use crate::ids::{Area, ClassName};
use crate::model::ArgValue;
use crate::phparray::PhpValue;
use crate::{deploy, Error, Magento, Result};

const IMPORTER_POOL: &str = "Magento\\Deploy\\Model\\DeploymentConfig\\ImporterPool";
/// `DeploymentConfig::OVERRIDE_KEY`: a JSON object laid over the whole config.
const OVERRIDE_ENV: &str = "MAGENTO_DC__OVERRIDE";
#[cfg(any(feature = "db", test))]
/// `Hash::CONFIG_KEY`.
pub(crate) const HASH_FLAG: &str = "config_hash";

/// The result of a config status check.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConfigStatus {
    /// One per importer section, in di.xml merge order.
    pub sections: Vec<ConfigSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConfigSection {
    pub name: String,
    /// The hash the last import stored, if any.
    pub stored: Option<String>,
    /// The hash of the section as the config files have it now. `None` when the
    /// section is absent, or empty with nothing stored: Magento skips both.
    pub current: Option<String>,
    pub changed: bool,
}

impl ConfigStatus {
    /// `app:config:status` would exit 2.
    pub fn import_required(&self) -> bool {
        self.sections.iter().any(|s| s.changed)
    }
}

impl Magento {
    /// The sections `app:config:import` imports: the keys of `ImporterPool`'s
    /// merged `importers` argument.
    pub fn config_import_sections(&self) -> Vec<String> {
        let pool = ClassName::new(IMPORTER_POOL);
        let args = self.args_of(&pool, Area::Global, &mut std::collections::HashSet::new());
        match args.get("importers") {
            Some((ArgValue::Array(items), _)) => items.iter().map(|i| i.key.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Each import section's current hash, as `Hash\Generator` computes it from
    /// the config files. `None` for a section that is absent (Magento skips it).
    pub fn config_section_hashes(&self) -> Result<Vec<(String, Option<Section>)>> {
        if std::env::var_os(OVERRIDE_ENV).is_some_and(|v| !v.is_empty()) {
            return Err(Error::Unsupported(format!(
                "{OVERRIDE_ENV} is set; its JSON override is not modelled"
            )));
        }
        let merged = self.deployment_config()?;
        let mut out = Vec::new();
        for name in self.config_import_sections() {
            let section = match merged.get(&Key::Str(name.clone())) {
                None | Some(Php::Null) => None,
                Some(v) => Some(Section {
                    empty: v.is_empty(),
                    hash: sha1_hex(json_encode(v)?.as_bytes()),
                }),
            };
            out.push((name, section));
        }
        Ok(out)
    }

    /// `DeploymentConfig\Reader::load()`: config.php, then env.php over it with
    /// `array_replace_recursive`. A missing file is skipped, as Magento skips it.
    fn deployment_config(&self) -> Result<Php> {
        let root = self.root();
        let vfs = &self.index.vfs;
        let mut merged = Php::Arr(Vec::new());
        for file in [deploy::read_config_php(root, vfs), deploy::read_env(root, vfs)] {
            let value = match file {
                Ok(v) => v,
                Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                    continue
                }
                Err(e) => return Err(e),
            };
            let value = Php::from_value(&value)?;
            if !matches!(value, Php::Arr(_)) {
                return Err(Error::Unsupported("a deployment config file does not return an array".into()));
            }
            merged = replace_recursive(merged, value);
        }
        Ok(merged)
    }
}

#[cfg(feature = "db")]
impl Magento {
    /// Whether `app:config:import` has work to do, by the comparison
    /// `bin/magento app:config:status` makes, without booting Magento.
    /// [`Error::Unsupported`] when the config can't be hashed exactly (fall back
    /// to PHP); [`Error::Db`] when the stored hashes can't be read, where
    /// `bin/magento` would fail too.
    pub fn config_status(&self) -> Result<ConfigStatus> {
        let current = self.config_section_hashes()?;
        let cfg = self.db_config()?;
        let conn = crate::default_connection(&cfg)?.clone();
        let mut db = crate::db::status::open(&conn).map_err(Error::Db)?;
        let table = format!("{}flag", cfg.table_prefix);
        let data: Option<Option<Vec<u8>>> = db
            .exec_first(format!("SELECT flag_data FROM `{table}` WHERE flag_code = ?"), (HASH_FLAG,))
            .map_err(Error::Db)?;
        let data = data.flatten().map(|b| String::from_utf8_lossy(&b).into_owned());
        Ok(compare(current, &parse_stored_hashes(data.as_deref())?))
    }
}

/// One section's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// PHP `empty()` of the section.
    pub empty: bool,
    pub hash: String,
}

#[cfg(any(feature = "db", test))]
/// `ChangeDetector::hasChanges`, per section.
pub(crate) fn compare(
    current: Vec<(String, Option<Section>)>,
    stored: &std::collections::HashMap<String, String>,
) -> ConfigStatus {
    let sections = current
        .into_iter()
        .map(|(name, section)| {
            let saved = stored.get(&name).cloned();
            let (current, changed) = match section {
                None => (None, false),
                // `empty($config) && !$savedHash ? null : generate($config)`; a saved
                // hash of "" or "0" is falsy too.
                Some(s) if s.empty && saved.as_deref().is_none_or(|h| h.is_empty() || h == "0") => {
                    (None, saved.is_some())
                }
                Some(s) => {
                    let changed = saved.as_deref() != Some(s.hash.as_str());
                    (Some(s.hash), changed)
                }
            };
            ConfigSection { name, stored: saved, current, changed }
        })
        .collect();
    ConfigStatus { sections }
}

#[cfg(any(feature = "db", test))]
/// `Hash::get()`: the `config_hash` flag's data, JSON with a PHP `unserialize`
/// fallback, as section → hash. A missing row or empty data is no hashes.
pub(crate) fn parse_stored_hashes(flag_data: Option<&str>) -> Result<std::collections::HashMap<String, String>> {
    let mut out = std::collections::HashMap::new();
    let Some(data) = flag_data else { return Ok(out) };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
        if let serde_json::Value::Object(map) = v {
            for (k, v) in map {
                if let serde_json::Value::String(h) = v {
                    out.insert(k, h);
                }
            }
        }
        return Ok(out);
    }
    parse_serialized_string_map(data)
        .ok_or_else(|| Error::Unsupported("the config_hash flag holds data that is neither JSON nor a serialized string map".into()))
}

#[cfg(any(feature = "db", test))]
/// `unserialize()` of `a:N:{s:L:"key";s:L:"value";…}`, the only shape a
/// pre-JSON Magento wrote for this flag.
fn parse_serialized_string_map(s: &str) -> Option<std::collections::HashMap<String, String>> {
    fn string(b: &[u8], i: &mut usize) -> Option<String> {
        let rest = b.get(*i..)?.strip_prefix(b"s:")?;
        let colon = rest.iter().position(|&c| c == b':')?;
        let len: usize = std::str::from_utf8(&rest[..colon]).ok()?.parse().ok()?;
        let start = *i + 2 + colon + 1;
        if b.get(start) != Some(&b'"') {
            return None;
        }
        let body = b.get(start + 1..start + 1 + len)?;
        if b.get(start + 1 + len..start + 3 + len)? != b"\";" {
            return None;
        }
        *i = start + 3 + len;
        String::from_utf8(body.to_vec()).ok()
    }
    let b = s.trim().as_bytes();
    let rest = b.strip_prefix(b"a:")?;
    let colon = rest.iter().position(|&c| c == b':')?;
    let n: usize = std::str::from_utf8(&rest[..colon]).ok()?.parse().ok()?;
    let mut i = 2 + colon + 1;
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut out = std::collections::HashMap::new();
    for _ in 0..n {
        let k = string(b, &mut i)?;
        let v = string(b, &mut i)?;
        out.insert(k, v);
    }
    (b.get(i..)? == b"}").then_some(out)
}

/// A PHP array key after PHP's key normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Key {
    Int(i64),
    Str(String),
}

/// A PHP value with PHP's array semantics applied.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Php {
    Arr(Vec<(Key, Php)>),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl Php {
    pub(crate) fn from_value(v: &PhpValue) -> Result<Php> {
        Ok(match v {
            PhpValue::Array(items) => {
                let mut out: Vec<(Key, Php)> = Vec::with_capacity(items.len());
                for (k, v) in items {
                    let key = Key::from_value(k)?;
                    let val = Php::from_value(v)?;
                    // A repeated key overwrites the value and keeps the first position.
                    match out.iter_mut().find(|(existing, _)| *existing == key) {
                        Some(slot) => slot.1 = val,
                        None => out.push((key, val)),
                    }
                }
                Php::Arr(out)
            }
            PhpValue::Str(s) => Php::Str(s.clone()),
            PhpValue::Int(i) => Php::Int(*i),
            PhpValue::Float(f) => Php::Float(*f),
            PhpValue::Bool(b) => Php::Bool(*b),
            PhpValue::Null => Php::Null,
            PhpValue::Const(c) => {
                return Err(Error::Unsupported(format!("the deployment config uses the constant `{c}`")))
            }
        })
    }

    pub(crate) fn get(&self, key: &Key) -> Option<&Php> {
        match self {
            Php::Arr(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// PHP `empty()`.
    fn is_empty(&self) -> bool {
        match self {
            Php::Arr(items) => items.is_empty(),
            Php::Str(s) => s.is_empty() || s == "0",
            Php::Int(i) => *i == 0,
            Php::Float(f) => *f == 0.0,
            Php::Bool(b) => !b,
            Php::Null => true,
        }
    }
}

impl Key {
    /// PHP's array key cast: a decimal-integer string, a bool or a float becomes
    /// an int; null becomes "".
    fn from_value(v: &PhpValue) -> Result<Key> {
        Ok(match v {
            PhpValue::Int(i) => Key::Int(*i),
            PhpValue::Str(s) => match crate::phparray::integer_key(s) {
                Some(i) => Key::Int(i),
                None => Key::Str(s.clone()),
            },
            PhpValue::Bool(b) => Key::Int(i64::from(*b)),
            PhpValue::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 => Key::Int(*f as i64),
            PhpValue::Null => Key::Str(String::new()),
            other => return Err(Error::Unsupported(format!("array key {other:?}"))),
        })
    }
}

/// PHP `array_replace_recursive($base, $replacement)`.
fn replace_recursive(base: Php, replacement: Php) -> Php {
    let (Php::Arr(mut base), Php::Arr(replacement)) = (base, replacement) else {
        unreachable!("replace_recursive takes arrays")
    };
    for (key, value) in replacement {
        match base.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => {
                let old = std::mem::replace(&mut slot.1, Php::Null);
                slot.1 = match (old, value) {
                    (old @ Php::Arr(_), new @ Php::Arr(_)) => replace_recursive(old, new),
                    (_, new) => new,
                };
            }
            None => base.push((key, value)),
        }
    }
    Php::Arr(base)
}

/// PHP `json_encode($value)` with no flags.
pub(crate) fn json_encode(v: &Php) -> Result<String> {
    let mut out = String::new();
    encode(v, &mut out)?;
    Ok(out)
}

fn encode(v: &Php, out: &mut String) -> Result<()> {
    match v {
        Php::Null => out.push_str("null"),
        Php::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Php::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Php::Float(f) => out.push_str(&encode_float(*f)?),
        Php::Str(s) => encode_str(s, out),
        Php::Arr(items) => {
            // A list (keys 0..n in order) is a JSON array, anything else an object.
            let is_list = items.iter().enumerate().all(|(i, (k, _))| *k == Key::Int(i as i64));
            if is_list {
                out.push('[');
                for (i, (_, v)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    encode(v, out)?;
                }
                out.push(']');
            } else {
                out.push('{');
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    match k {
                        Key::Int(n) => encode_str(&n.to_string(), out),
                        Key::Str(s) => encode_str(s, out),
                    }
                    out.push(':');
                    encode(v, out)?;
                }
                out.push('}');
            }
        }
    }
    Ok(())
}

/// `json_encode` of a float under the default `serialize_precision = -1`: the
/// shortest round-trip digits, and no `.0` on integral values (that takes
/// `JSON_PRESERVE_ZERO_FRACTION`). Only the plain decimal range is reproduced;
/// PHP switches to exponent form outside it.
fn encode_float(f: f64) -> Result<String> {
    if !f.is_finite() {
        return Err(Error::Unsupported("the deployment config holds INF or NAN".into()));
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e15).contains(&a) {
        return Err(Error::Unsupported(format!("the float {f} would be written in exponent form")));
    }
    // Rust's `Display` is shortest round-trip too, never uses an exponent, and
    // writes integral values without a fraction.
    Ok(format!("{f}"))
}

/// A JSON string as PHP escapes it by default: `/` escaped, everything outside
/// ASCII as `\uXXXX` (UTF-16, lowercase hex).
fn encode_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub(crate) fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phparray::parse;

    fn php(src: &str) -> Php {
        Php::from_value(&parse(src).unwrap()).unwrap()
    }

    #[test]
    fn json_matches_php_escaping_and_list_detection() {
        // Each expected string is PHP's own json_encode of the same literal.
        let cases = [
            (r#"<?php return [];"#, "[]"),
            (r#"<?php return ['a', 'b'];"#, r#"["a","b"]"#),
            (r#"<?php return [1 => 'a'];"#, r#"{"1":"a"}"#),
            (r#"<?php return ['0' => 'a', '1' => 'b'];"#, r#"["a","b"]"#),
            (r#"<?php return ['01' => 'a'];"#, r#"{"01":"a"}"#),
            (r#"<?php return ['k' => 'a/b', 'é' => "x\ty"];"#, r#"{"k":"a\/b","\u00e9":"x\ty"}"#),
            (r#"<?php return ['e' => '😀'];"#, r#"{"e":"\ud83d\ude00"}"#),
            (r#"<?php return ['t' => true, 'n' => null, 'i' => -3, 'f' => 1.0, 'g' => 0.5];"#,
             r#"{"t":true,"n":null,"i":-3,"f":1,"g":0.5}"#),
            (r#"<?php return [5 => 'a', 'b'];"#, r#"{"5":"a","6":"b"}"#),
            (r#"<?php return ['x' => 0.1, 'y' => 123456.789, 'd' => "\x01"];"#, r#"{"x":0.1,"y":123456.789,"d":"\u0001"}"#),
            (r#"<?php return ['a' => 1, 'b' => 2, 'a' => 3];"#, r#"{"a":3,"b":2}"#),
        ];
        for (src, want) in cases {
            assert_eq!(json_encode(&php(src)).unwrap(), want, "{src}");
        }
    }

    #[test]
    fn env_php_replaces_config_php_recursively() {
        let config = php(r#"<?php return ['system' => ['default' => ['a' => '1', 'b' => ['x' => 1]]], 'scopes' => ['w' => 1]];"#);
        let env = php(r#"<?php return ['system' => ['default' => ['b' => ['y' => 2], 'c' => '3']]];"#);
        let merged = replace_recursive(replace_recursive(Php::Arr(vec![]), config), env);
        assert_eq!(
            json_encode(merged.get(&Key::Str("system".into())).unwrap()).unwrap(),
            r#"{"default":{"a":"1","b":{"x":1,"y":2},"c":"3"}}"#
        );
    }

    #[test]
    fn constants_are_unsupported_not_guessed() {
        let v = parse(r#"<?php return ['a' => \Foo::BAR];"#).unwrap();
        assert!(matches!(Php::from_value(&v), Err(Error::Unsupported(_))));
    }

    #[test]
    fn compare_follows_change_detector() {
        let hash = |s: &str| Some(Section { empty: false, hash: s.into() });
        let stored: std::collections::HashMap<_, _> =
            [("scopes".to_string(), "aaa".to_string()), ("system".to_string(), "bbb".to_string())].into();
        let status = compare(
            vec![
                ("scopes".into(), hash("aaa")),
                ("themes".into(), None),
                ("system".into(), hash("ccc")),
            ],
            &stored,
        );
        assert_eq!(status.sections.iter().map(|s| s.changed).collect::<Vec<_>>(), [false, false, true]);
        assert!(status.import_required());

        // An empty section with no stored hash is skipped; with one, it is compared.
        let empty = || Some(Section { empty: true, hash: sha1_hex(b"[]") });
        assert!(!compare(vec![("themes".into(), empty())], &Default::default()).import_required());
        let stored: std::collections::HashMap<_, _> = [("themes".to_string(), "old".to_string())].into();
        assert!(compare(vec![("themes".into(), empty())], &stored).import_required());
    }

    #[test]
    fn stored_hashes_read_json_and_serialized() {
        let j = parse_stored_hashes(Some(r#"{"scopes":"abc","system":"def"}"#)).unwrap();
        assert_eq!(j["system"], "def");
        let s = parse_stored_hashes(Some(r#"a:1:{s:6:"scopes";s:3:"abc";}"#)).unwrap();
        assert_eq!(s["scopes"], "abc");
        assert!(parse_stored_hashes(None).unwrap().is_empty());
    }
}
