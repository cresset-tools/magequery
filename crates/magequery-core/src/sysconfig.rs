//! System configuration (the "Stores → Configuration" path→value settings), resolved by
//! re-implementing Magento's `ConfigSourceAggregated` merge. The source layers and their
//! order are **not hardcoded** — they're read from the `systemConfigSourceAggregated`
//! virtual type in di.xml (each source's `sortOrder`), so a module that re-orders or drops a
//! source is honored. The recognized sources, in Magento's default order:
//!
//! 1. `ModularConfigSource`  (sortOrder 10)   → module `config.xml` `<default>`
//! 2. `RuntimeConfigSource`  (sortOrder 100)  → `core_config_data` (database, opt-in)
//! 3. `InitialConfigSource`  (sortOrder 1000) → deployment config: `config.php` then
//!    `env.php` `system` node
//!
//! `CONFIG__*` environment variables are applied last (they override the deployment config).
//! Unrecognized custom `ConfigSourceInterface` implementations are skipped.

use std::collections::HashMap;
use std::path::PathBuf;

use rayon::prelude::*;

use crate::vfs::Vfs;
use crate::model::{ConfigSourceKind, ConfigValue, Module};
use crate::parse;
use crate::phparray::PhpValue;

/// A resolved system-config set — query it with [`get`](ConfigSet::get) /
/// [`section`](ConfigSet::section) / [`scopes_for`](ConfigSet::scopes_for). Obtained from
/// [`Magento::config`](crate::Magento::config).
pub struct ConfigSet {
    /// `(scope, path)` -> winning value.
    map: HashMap<(String, String), ConfigValue>,
    /// `store code` -> parent `website code`, from `config.php`'s `scopes` node. Drives the
    /// store → website → default fallback chain.
    store_parent: HashMap<String, String>,
}

/// One recognized system-config source layer. The order in which these are applied is
/// **derived from di.xml** (`systemConfigSourceAggregated` `sortOrder`), not hardcoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SysCfgSource {
    /// `ModularConfigSource` — module `config.xml` `<default>` values.
    Modular,
    /// `RuntimeConfigSource` — `core_config_data` (database).
    Dynamic,
    /// `InitialConfigSource` — the deployment config (`config.php` then `env.php` `system`).
    Initial,
}

/// Magento's default source order when di.xml can't be read (modular → dynamic → initial).
pub(crate) const DEFAULT_SOURCE_ORDER: [SysCfgSource; 3] =
    [SysCfgSource::Modular, SysCfgSource::Dynamic, SysCfgSource::Initial];

impl ConfigSet {
    /// Build the set by applying each recognized source in `order` (ascending `sortOrder`,
    /// later wins), then the `CONFIG__*` env-var overlay. `db_values` are `(scope, path,
    /// value)` rows from `core_config_data` (empty unless the DB source was fetched).
    pub(crate) fn build(
        root: &std::path::Path,
        modules: &[Module],
        vfs: &Vfs,
        env: &PhpValue,
        config_php: &PhpValue,
        db_values: Vec<(String, String, String)>,
        order: &[SysCfgSource],
    ) -> Self {
        let mut map = HashMap::new();
        let mut db_values = Some(db_values);

        for src in order {
            match src {
                SysCfgSource::Modular => apply_modular(&mut map, modules, vfs),
                SysCfgSource::Dynamic => {
                    for (scope, path, value) in db_values.take().unwrap_or_default() {
                        insert(&mut map, scope, path, value, ConfigSourceKind::Database, None, 0);
                    }
                }
                SysCfgSource::Initial => {
                    let cfg_php_file = Some(root.join("app/etc/config.php"));
                    let env_file = Some(root.join("app/etc/env.php"));
                    apply_system(&mut map, config_php.get("system"), ConfigSourceKind::ConfigPhp, cfg_php_file);
                    apply_system(&mut map, env.get("system"), ConfigSourceKind::EnvPhp, env_file);
                }
            }
        }

        // `CONFIG__*` env vars always win (they override the deployment config).
        apply_env_vars(&mut map);

        Self { map, store_parent: scope_parents(config_php) }
    }

    /// Scope fallback chain (most specific first), mirroring Magento's scope resolution: a
    /// store falls back to its parent **website**, then to `default`. The website parentage
    /// comes from `config.php`'s `scopes` node ([`scope_parents`]); a store with no known
    /// parent (or a website scope) falls back straight to `default`.
    fn chain(&self, scope: &str) -> Vec<String> {
        if scope == "default" {
            return vec!["default".to_string()];
        }
        let mut chain = vec![scope.to_string()];
        if let Some(code) = scope.strip_prefix("stores/") {
            if let Some(website) = self.store_parent.get(code) {
                chain.push(format!("websites/{website}"));
            }
        }
        chain.push("default".to_string());
        chain
    }

    /// Resolve `path` at `scope`, falling back through the scope chain.
    pub fn get(&self, scope: &str, path: &str) -> Option<&ConfigValue> {
        self.chain(scope).iter().find_map(|s| self.map.get(&(s.clone(), path.to_string())))
    }

    /// All leaf values under `prefix` (a section/group), resolved for `scope`. Sorted by path.
    pub fn section(&self, scope: &str, prefix: &str) -> Vec<ConfigValue> {
        let chain = self.chain(scope);
        // Distinct paths under the prefix that exist in any scope of the chain.
        let mut paths: Vec<&str> = self
            .map
            .keys()
            .filter(|(s, p)| chain.contains(s) && under(p, prefix))
            .map(|(_, p)| p.as_str())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        paths.into_iter().filter_map(|p| self.get(scope, p).cloned()).collect()
    }

    /// Every scope that sets `path` explicitly (across all sources), sorted by scope.
    pub fn scopes_for(&self, path: &str) -> Vec<ConfigValue> {
        let mut v: Vec<ConfigValue> =
            self.map.iter().filter(|((_, p), _)| p == path).map(|(_, val)| val.clone()).collect();
        v.sort_by(|a, b| a.scope.cmp(&b.scope));
        v
    }
}

/// Build the `store code -> parent website code` map from `config.php`'s `scopes` node.
/// Stores reference their website by numeric `website_id`; we first map `website_id -> code`
/// from `scopes/websites`, then resolve each store's `website_id` through it.
fn scope_parents(config_php: &PhpValue) -> HashMap<String, String> {
    let scopes = config_php.get("scopes");
    let mut id_to_website: HashMap<String, String> = HashMap::new();
    if let Some(websites) = scopes.and_then(|s| s.get("websites")).and_then(PhpValue::as_array) {
        for (code, entry) in websites {
            let code = code.scalar_string().unwrap_or_default();
            if let Some(id) = entry.get("website_id").and_then(PhpValue::scalar_string) {
                id_to_website.insert(id, code);
            }
        }
    }

    let mut store_parent = HashMap::new();
    if let Some(stores) = scopes.and_then(|s| s.get("stores")).and_then(PhpValue::as_array) {
        for (code, entry) in stores {
            let code = code.scalar_string().unwrap_or_default();
            if let Some(website) =
                entry.get("website_id").and_then(PhpValue::scalar_string).and_then(|id| id_to_website.get(&id))
            {
                store_parent.insert(code, website.clone());
            }
        }
    }
    store_parent
}

/// Apply module `config.xml` `<default>` values (parsed in parallel, applied in load order).
fn apply_modular(map: &mut HashMap<(String, String), ConfigValue>, modules: &[Module], vfs: &Vfs) {
    let parsed: Vec<(PathBuf, Vec<(String, String, String, u32)>)> = modules
        .par_iter()
        .filter(|m| m.enabled) // Magento only loads enabled modules' configuration
        .map(|m| {
            let path = m.path.join("etc/config.xml");
            let leaves = vfs.read_to_string(&path)
                .ok()
                .map(|t| parse::config_xml_defaults(&t))
                .unwrap_or_default();
            (path, leaves)
        })
        .filter(|(_, leaves)| !leaves.is_empty())
        .collect();
    for (path, leaves) in parsed {
        for (scope, cpath, value, line) in leaves {
            insert(map, scope, cpath, value, ConfigSourceKind::ConfigXml, Some(path.clone()), line);
        }
    }
}

fn under(path: &str, prefix: &str) -> bool {
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn insert(
    map: &mut HashMap<(String, String), ConfigValue>,
    scope: String,
    path: String,
    value: String,
    source: ConfigSourceKind,
    file: Option<PathBuf>,
    line: u32,
) {
    map.insert(
        (scope.clone(), path.clone()),
        ConfigValue { path, scope, value, source, file, line },
    );
}

/// Flatten a `system` node (`default` / `websites/<code>` / `stores/<code>` → path tree).
fn apply_system(
    map: &mut HashMap<(String, String), ConfigValue>,
    system: Option<&PhpValue>,
    source: ConfigSourceKind,
    file: Option<PathBuf>,
) {
    let Some(items) = system.and_then(PhpValue::as_array) else { return };
    for (scope_type, scope_val) in items {
        let st = scope_type.scalar_string().unwrap_or_default();
        match st.as_str() {
            "default" => flatten(map, "default", "", scope_val, source, &file),
            "websites" | "stores" => {
                if let Some(codes) = scope_val.as_array() {
                    for (code, tree) in codes {
                        let scope = format!("{st}/{}", code.scalar_string().unwrap_or_default());
                        flatten(map, &scope, "", tree, source, &file);
                    }
                }
            }
            _ => {}
        }
    }
}

fn flatten(
    map: &mut HashMap<(String, String), ConfigValue>,
    scope: &str,
    prefix: &str,
    v: &PhpValue,
    source: ConfigSourceKind,
    file: &Option<PathBuf>,
) {
    match v {
        PhpValue::Array(items) => {
            for (k, val) in items {
                let key = k.scalar_string().unwrap_or_default();
                let next = if prefix.is_empty() { key } else { format!("{prefix}/{key}") };
                flatten(map, scope, &next, val, source, file);
            }
        }
        other => {
            if let Some(value) = other.scalar_string() {
                insert(map, scope.to_string(), prefix.to_string(), value, source, file.clone(), 0);
            }
        }
    }
}

/// `CONFIG__<SCOPE>__<PATH>` env vars (path uppercased, `__`-separated).
fn apply_env_vars(map: &mut HashMap<(String, String), ConfigValue>) {
    for (key, value) in std::env::vars() {
        let Some(rest) = key.strip_prefix("CONFIG__") else { continue };
        let parts: Vec<&str> = rest.split("__").collect();
        let (scope, path_parts): (String, &[&str]) = match parts.first().map(|s| *s) {
            Some("DEFAULT") => ("default".to_string(), &parts[1..]),
            Some("WEBSITES") if parts.len() >= 2 => {
                (format!("websites/{}", parts[1].to_lowercase()), &parts[2..])
            }
            Some("STORES") if parts.len() >= 2 => {
                (format!("stores/{}", parts[1].to_lowercase()), &parts[2..])
            }
            _ => continue,
        };
        if path_parts.is_empty() {
            continue;
        }
        let path = path_parts.iter().map(|p| p.to_lowercase()).collect::<Vec<_>>().join("/");
        insert(map, scope, path, value, ConfigSourceKind::EnvVar, None, 0);
    }
}
