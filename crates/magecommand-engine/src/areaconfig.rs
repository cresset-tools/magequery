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

/// A complete area file (chain applied), ready to serialize.
pub struct AreaFile {
    pub arguments: BTreeMap<String, PhpValue>,
    pub preferences: Vec<(String, String)>,
    pub instance_types: Vec<(String, String)>,
    /// `class => true`, in Magento's candidate order (NOT sorted — the one
    /// section Area::doOperation never ksorts).
    pub non_lazy: Vec<String>,
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

    // NonLazyTypes: candidates = chain-time arguments keys, then
    // instanceTypes values, then preferences values — each in Magento's
    // insertion order (reconstructed below), deduped first-wins, filtered to
    // the lazy-INeligible.
    let non_lazy = non_lazy_types(
        defs,
        &export,
        &arguments,
        &instance_types,
        &preferences,
        &interceptors,
    );

    AreaFile {
        arguments,
        preferences: preferences.into_iter().collect(),
        instance_types: instance_types.into_iter().collect(),
        non_lazy,
        findings: ctx.take_findings(),
    }
}

/// Chain\NonLazyTypes (PHP >= 8.4, active on the oracle): which candidate
/// classes cannot be lazy-proxied. Order is the candidate insertion order —
/// this section is written pre-ksort and never sorted.
fn non_lazy_types(
    defs: &Definitions,
    export: &magequery_core::DiExport,
    arguments: &BTreeMap<String, PhpValue>,
    instance_types: &BTreeMap<String, String>,
    preferences: &BTreeMap<String, String>,
    interceptors: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut prefs_by_decl: Vec<&magequery_core::PreferenceDecl> = export.preferences.iter().collect();
    prefs_by_decl.sort_by_key(|p| p.decl_order);
    let mut vtypes_by_decl: Vec<&magequery_core::VirtualTypeDecl> =
        export.virtual_types.iter().collect();
    vtypes_by_decl.sort_by_key(|v| v.decl_order);
    let pref_key_set: HashSet<&str> = export
        .preferences
        .iter()
        .map(|p| p.for_type.as_str())
        .collect();
    let vtype_set: HashSet<&str> = export
        .virtual_types
        .iter()
        .map(|v| v.name.as_str())
        .collect();
    let appended_interceptors: HashSet<&str> =
        interceptors.values().map(String::as_str).collect();

    let mut candidates: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |candidates: &mut Vec<String>, seen: &mut HashSet<String>, name: &str| {
        if seen.insert(name.to_owned()) {
            candidates.push(name.to_owned());
        }
    };

    // [A] preference keys with argument rows, in declaration order
    // (fillThirdPartyInterfaces put them at the FRONT of the collection).
    // A preference key that is a VIRTUAL TYPE gets its row from the
    // Reader's vtype loop instead — appended later, block [C].
    for p in &prefs_by_decl {
        let key = p.for_type.as_str();
        if arguments.contains_key(key)
            && !(vtype_set.contains(key) && !defs.scanned.contains(key))
        {
            push(&mut candidates, &mut seen, key);
        }
    }
    // [B] the sorted scanned block (collection was ksort'ed). A virtual
    // type whose name collides with a scanned class keeps the class row's
    // position here (PHP re-assignment preserves the first position).
    for key in arguments.keys() {
        if pref_key_set.contains(key.as_str())
            || appended_interceptors.contains(key.as_str())
            || (vtype_set.contains(key.as_str()) && !defs.scanned.contains(key.as_str()))
        {
            continue;
        }
        push(&mut candidates, &mut seen, key.as_str());
    }
    // [C] virtual-type rows, appended by the Reader in declaration order
    // (the seen-set already holds scanned-name collisions).
    for v in &vtypes_by_decl {
        if arguments.contains_key(v.name.as_str()) {
            push(&mut candidates, &mut seen, v.name.as_str());
        }
    }
    // [D] interceptor rows appended by the substitution's rename.
    for interceptor in interceptors.values() {
        if arguments.contains_key(interceptor.as_str()) {
            push(&mut candidates, &mut seen, interceptor.as_str());
        }
    }
    // instanceTypes VALUES in vtype declaration order.
    for v in &vtypes_by_decl {
        if let Some(value) = instance_types.get(v.name.as_str()) {
            push(&mut candidates, &mut seen, value.as_str());
        }
    }
    // preferences VALUES: the interceptor-map block first (its insertion
    // order), then reader preferences in declaration order.
    for original in interceptors.keys() {
        if let Some(value) = preferences.get(original.as_str()) {
            push(&mut candidates, &mut seen, value.as_str());
        }
    }
    for p in &prefs_by_decl {
        if let Some(value) = preferences.get(p.for_type.as_str()) {
            push(&mut candidates, &mut seen, value.as_str());
        }
    }

    candidates
        .into_iter()
        .filter(|name| !is_lazy_eligible(defs, name))
        .collect()
}

/// Chain\NonLazyTypes::isLazyEligible, statically. Ineligible (=> nonLazy):
/// `\Proxy`-suffixed, unknown to the class universe (class_exists false —
/// virtual types land here), interface/trait/enum/abstract/final/readonly,
/// any internal PHP ancestor, or the #[NonLazy] attribute.
fn is_lazy_eligible(defs: &Definitions, name: &str) -> bool {
    use magecommand_php::ClassKind;
    if name.ends_with("\\Proxy") {
        return false;
    }
    let Some(record) = defs.get(name) else {
        return false;
    };
    let meta = &record.meta;
    if meta.kind != ClassKind::Class || meta.is_abstract || meta.is_final || meta.is_readonly {
        return false;
    }
    if meta
        .attributes
        .iter()
        .any(|a| a == "Magento\\Framework\\ObjectManager\\Attribute\\NonLazy")
    {
        return false;
    }
    // Internal-ancestor walk: an extends chain that leaves the parsed set
    // hit a PHP-internal class (DateTime, ArrayIterator, Exception, ...).
    let mut current = meta;
    let mut hops = 0;
    while let Some(parent) = current.extends.first() {
        hops += 1;
        if hops > 64 {
            return false;
        }
        match defs.get(parent) {
            Some(r) => current = &r.meta,
            None => return false,
        }
    }
    true
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

impl AreaFile {
    /// The complete `<area>.php` content: the four sections in Magento's
    /// write order, var_export-exact.
    pub fn render(&self) -> String {
        let pairs = |entries: &[(String, String)]| {
            PhpValue::Array(
                entries
                    .iter()
                    .map(|(k, v)| (PhpKey::str(k.clone()), PhpValue::str(v.clone())))
                    .collect(),
            )
        };
        let file = PhpValue::Array(vec![
            (
                PhpKey::str("arguments"),
                PhpValue::Array(
                    self.arguments
                        .iter()
                        .map(|(k, v)| (PhpKey::str(k.clone()), v.clone()))
                        .collect(),
                ),
            ),
            (PhpKey::str("preferences"), pairs(&self.preferences)),
            (PhpKey::str("instanceTypes"), pairs(&self.instance_types)),
            (
                PhpKey::str("nonLazyTypes"),
                PhpValue::Array(
                    self.non_lazy
                        .iter()
                        .map(|k| (PhpKey::str(k.clone()), PhpValue::Bool(true)))
                        .collect(),
                ),
            ),
        ]);
        crate::phpexport::to_php_file(&file)
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
