//! Per-area compiled-config sections, mirroring
//! `Setup\Module\Di\Compiler\Config\Reader` + the modification chain.
//!
//! Built so far: `preferences` and `instanceTypes` (post-chain: interceptor
//! substitution applied, interceptor preference entries merged, ksorted).
//! `arguments`/`nonLazyTypes` follow with the ArgumentsResolver port.

use std::collections::{BTreeMap, HashMap, HashSet};

use magequery_core::{Area, Magento};

use crate::arguments::{build_arguments, ArgsCtx};
use crate::definitions::Definitions;
use crate::phpexport::{PhpKey, PhpValue};

/// The seven compiled areas, in Magento's generation order (global first),
/// with the code string used as the metadata file stem.
pub const AREA_CODES: [(Area, &str); 7] = [
    (Area::Global, "global"),
    (Area::Frontend, "frontend"),
    (Area::Adminhtml, "adminhtml"),
    (Area::Crontab, "crontab"),
    (Area::WebapiRest, "webapi_rest"),
    (Area::WebapiSoap, "webapi_soap"),
    (Area::Graphql, "graphql"),
];

pub struct AreaSections {
    /// `instance => preference`, interceptor-substituted, sorted.
    pub preferences: Vec<(String, String)>,
    /// `virtualType => base`, interceptor-substituted, sorted.
    pub instance_types: Vec<(String, String)>,
}

/// `\Interceptor`-suffixed concrete classes in the scanned universe,
/// keyed by the class they intercept — InterceptorSubstitution's list.
pub fn interceptors_map(defs: &Definitions) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for name in defs.classes.keys() {
        if let Some(original) = name.strip_suffix("\\Interceptor") {
            if defs.is_concrete(name) {
                map.insert(original.to_owned(), name.clone());
            }
        }
    }
    map
}

pub fn area_sections(magento: &Magento, defs: &Definitions, area: Area) -> AreaSections {
    let export = magento.di_export(area);
    let pref_map: HashMap<&str, &str> = export
        .preferences
        .iter()
        .map(|p| (p.for_type.as_str(), p.prefer.as_str()))
        .collect();

    // Magento's Config::getPreference: chase to the fixpoint.
    let resolve = |name: &str| -> String {
        let mut current = name;
        let mut seen = HashSet::new();
        while let Some(next) = pref_map.get(current) {
            if !seen.insert(*next) {
                break;
            }
            current = next;
        }
        current.to_owned()
    };

    let interceptors = interceptors_map(defs);

    // Reader: instance names = scanned classes ∪ preference keys
    // (fillThirdPartyInterfaces); emit name => resolved when different.
    let mut prefs: BTreeMap<String, String> = BTreeMap::new();
    let names = defs
        .classes
        .keys()
        .map(String::as_str)
        .chain(pref_map.keys().copied());
    for name in names {
        let resolved = resolve(name);
        if resolved != name {
            prefs.insert(name.to_owned(), resolved);
        }
    }

    // instanceTypes: vtype => the REAL class it instantiates. Magento's
    // Config::getInstanceType chases vtype-on-vtype chains to the fixpoint,
    // not one hop.
    let vtype_map: HashMap<&str, &str> = export
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str(), v.base.as_str()))
        .collect();
    let resolve_vtype = |name: &str| -> String {
        let mut current = name;
        let mut seen = HashSet::new();
        while let Some(next) = vtype_map.get(current) {
            if !seen.insert(*next) {
                break;
            }
            current = next;
        }
        current.to_owned()
    };
    let mut instance_types: BTreeMap<String, String> = export
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str().to_owned(), resolve_vtype(v.base.as_str())))
        .collect();

    // InterceptorSubstitution: rewrite values, then merge the interceptor
    // map UNDER existing preference entries (array_merge: later wins).
    for value in prefs.values_mut() {
        if let Some(interceptor) = interceptors.get(value) {
            *value = interceptor.clone();
        }
    }
    let mut merged = interceptors.clone();
    merged.extend(prefs);
    for value in instance_types.values_mut() {
        if let Some(interceptor) = interceptors.get(value) {
            *value = interceptor.clone();
        }
    }

    AreaSections {
        preferences: merged.into_iter().collect(),
        instance_types: instance_types.into_iter().collect(),
    }
}

/// A complete area file (chain applied), ready to serialize. `nonLazyTypes`
/// pending.
pub struct AreaFile {
    pub arguments: BTreeMap<String, PhpValue>,
    pub preferences: Vec<(String, String)>,
    pub instance_types: Vec<(String, String)>,
    pub findings: Vec<String>,
}

/// Reader + modification chain for one area: arguments, preferences,
/// instanceTypes — in Magento's exact order of operations.
pub fn build_area_file(
    magento: &Magento,
    defs: &Definitions,
    area: Area,
    root: &std::path::Path,
) -> AreaFile {
    let export = magento.di_export(area);
    let overrides = crate::arguments::setup_overrides(magento, root);
    let ctx = ArgsCtx::new(defs, &defs.scanned, &export, overrides);

    // Reader.
    let mut arguments = build_arguments(&ctx, magento);
    let sections = area_sections_reader(defs, &export);
    let mut preferences = sections.0;
    let mut instance_types = sections.1;

    // Chain: BackslashTrim is an invariant here (ClassName strips leading
    // backslashes at construction). PreferencesResolving pass 1:
    resolve_preferences_in_args(&mut arguments, &preferences);

    // InterceptorSubstitution.
    let interceptors: BTreeMap<String, String> = arguments
        .keys()
        .filter_map(|k| {
            k.strip_suffix("\\Interceptor")
                .map(|orig| (orig.to_owned(), k.clone()))
        })
        .collect();
    for interceptor in interceptors.values() {
        arguments.remove(interceptor);
    }
    for (original, interceptor) in &interceptors {
        // PHP-ism reproduced faithfully: the rename is guarded by isset(),
        // which is FALSE for a NULL row — a constructor-less intercepted
        // class keeps its own key (its Interceptor row is still dropped).
        if arguments.get(original).is_some_and(|v| *v != PhpValue::Null) {
            let value = arguments.remove(original).unwrap();
            arguments.insert(interceptor.clone(), value);
        }
    }
    for value in preferences.values_mut() {
        if let Some(i) = interceptors.get(value) {
            *value = i.clone();
        }
    }
    let mut merged = interceptors.clone();
    merged.extend(std::mem::take(&mut preferences));
    preferences = merged;
    for value in instance_types.values_mut() {
        if let Some(i) = interceptors.get(value) {
            *value = i.clone();
        }
    }

    // PreferencesResolving pass 2, now through interceptor-augmented prefs.
    resolve_preferences_in_args(&mut arguments, &preferences);

    AreaFile {
        arguments,
        preferences: preferences.into_iter().collect(),
        instance_types: instance_types.into_iter().collect(),
        findings: ctx.take_findings(),
    }
}

/// Reader-stage preferences (fixpoint) and instanceTypes (vtype chains
/// resolved), BEFORE interceptor substitution.
fn area_sections_reader(
    defs: &Definitions,
    export: &magequery_core::DiExport,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let pref_map: HashMap<&str, &str> = export
        .preferences
        .iter()
        .map(|p| (p.for_type.as_str(), p.prefer.as_str()))
        .collect();
    let resolve = |name: &str| -> String {
        let mut current = name;
        let mut seen = HashSet::new();
        while let Some(next) = pref_map.get(current) {
            if !seen.insert(*next) {
                break;
            }
            current = next;
        }
        current.to_owned()
    };
    let mut prefs: BTreeMap<String, String> = BTreeMap::new();
    let names = defs
        .scanned
        .iter()
        .map(String::as_str)
        .chain(pref_map.keys().copied());
    for name in names {
        let resolved = resolve(name);
        if resolved != name {
            prefs.insert(name.to_owned(), resolved);
        }
    }
    let vtype_map: HashMap<&str, &str> = export
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str(), v.base.as_str()))
        .collect();
    let resolve_vtype = |name: &str| -> String {
        let mut current = name;
        let mut seen = HashSet::new();
        while let Some(next) = vtype_map.get(current) {
            if !seen.insert(*next) {
                break;
            }
            current = next;
        }
        current.to_owned()
    };
    let instance_types: BTreeMap<String, String> = export
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str().to_owned(), resolve_vtype(v.base.as_str())))
        .collect();
    (prefs, instance_types)
}

/// Chain\PreferencesResolving: every `_i_`/`_ins_` value in the arguments
/// tree is chased through the preferences map to its end.
fn resolve_preferences_in_args(
    arguments: &mut BTreeMap<String, PhpValue>,
    preferences: &BTreeMap<String, String>,
) {
    fn chase(value: &str, prefs: &BTreeMap<String, String>) -> String {
        let mut current = value;
        let mut seen = HashSet::new();
        while let Some(next) = prefs.get(current) {
            if !seen.insert(next.as_str()) {
                break;
            }
            current = next;
        }
        current.to_owned()
    }
    fn walk(value: &mut PhpValue, prefs: &BTreeMap<String, String>) {
        if let PhpValue::Array(entries) = value {
            for (key, v) in entries.iter_mut() {
                let is_instance_key =
                    matches!(key, PhpKey::Str(s) if s == "_i_" || s == "_ins_");
                if is_instance_key {
                    if let PhpValue::Str(s) = v {
                        *s = chase(s, prefs);
                    }
                } else {
                    walk(v, prefs);
                }
            }
        }
    }
    for value in arguments.values_mut() {
        walk(value, preferences);
    }
}

/// Render one `'key' => array(...)` section exactly as it appears inside the
/// area file (indent 2), for section-level parity checks during bring-up.
pub fn render_section(key: &str, entries: &[(String, String)]) -> String {
    let section = PhpValue::Array(
        entries
            .iter()
            .map(|(k, v)| (PhpKey::str(k.clone()), PhpValue::str(v.clone())))
            .collect(),
    );
    let wrapper = PhpValue::Array(vec![(PhpKey::str(key), section)]);
    let full = crate::phpexport::to_php_file(&wrapper);
    let body = full
        .strip_prefix("<?php return array (\n")
        .and_then(|s| s.strip_suffix("\n);"))
        .expect("wrapper shape is fixed");
    format!("{body}\n")
}
