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

/// Custom-registered area codes: the keys of `Magento\Framework\App\AreaList`'s
/// `areas` argument (Magento's `AreaList::getCodes()`) minus the seven fixed
/// areas magecommand already emits. Each yields an extra `<code>.php` and a
/// `…|<code>|…|plugin-list.php`, exactly as the compile's Area operation loops
/// `array_merge([GLOBAL], $areaList->getCodes())`. Sorted for determinism.
pub fn custom_area_codes(magento: &Magento) -> Vec<String> {
    use magequery_core::ArgValue;
    const FIXED: [&str; 7] = [
        "global",
        "frontend",
        "adminhtml",
        "crontab",
        "webapi_rest",
        "webapi_soap",
        "graphql",
    ];
    let export = magento.di_export_ref(Area::Global);
    let mut codes: Vec<String> = Vec::new();
    for decl in &export.arguments {
        if decl.type_name.as_str() == "Magento\\Framework\\App\\AreaList" && decl.arg == "areas" {
            if let ArgValue::Array(items) = &decl.value {
                for item in items {
                    if !FIXED.contains(&item.key.as_str()) && !codes.contains(&item.key) {
                        codes.push(item.key.clone());
                    }
                }
            }
        }
    }
    codes.sort();
    codes
}

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
    let export = magento.di_export_ref(area);
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
/// Follow `preferences` from `name` to its concrete (fixpoint, cycle-guarded);
/// a name with no preference resolves to itself.
fn resolve_preference(preferences: &BTreeMap<String, String>, name: &str) -> String {
    let mut cur = name.to_owned();
    let mut seen = std::collections::HashSet::new();
    while let Some(next) = preferences.get(&cur) {
        if next == &cur || !seen.insert(cur.clone()) {
            break;
        }
        cur = next.clone();
    }
    cur
}

pub fn build_area_file(
    magento: &Magento,
    defs: &Definitions,
    area: Area,
    root: &std::path::Path,
    fused: bool,
) -> AreaFile {
    build_area_file_from_export(magento, defs, magento.di_export_ref(area), root, fused)
}

/// Like [`build_area_file`] but driven by a caller-supplied [`DiExport`] — used
/// for custom-registered areas, whose merged config comes from
/// [`Magento::di_export_custom_area`] rather than the fixed `Area` enum. The
/// area itself plays no further role here (it only selected the export), so the
/// reader/chain is identical.
pub fn build_area_file_from_export(
    magento: &Magento,
    defs: &Definitions,
    export: &magequery_core::DiExport,
    root: &std::path::Path,
    fused: bool,
) -> AreaFile {
    let overrides = crate::arguments::setup_overrides(magento, root);
    let ctx = ArgsCtx::new(defs, &defs.scanned, export, overrides, Some(magento), Some(root));

    // Reader.
    let mut arguments = build_arguments(&ctx, magento);
    let sections = area_sections_reader(defs, export);
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

    // Fused mode (`di compile --fused`): creatuity's CompiledInterceptorSubstitution
    // prepends the two constructor deps every fused interceptor gains — the
    // ObjectManager and the scope service — to each `\Interceptor`'s arguments,
    // each resolved through `preferences` (ObjectManagerInterface has none → itself;
    // Config\ScopeInterface → Config\Scope). Only interceptors that already have an
    // arguments row get them (a constructor-less subject has no row and stays that
    // way — its ctor deps autowire), matching the reference exactly.
    if fused {
        let om = resolve_preference(&preferences, "Magento\\Framework\\ObjectManagerInterface");
        let scope = resolve_preference(&preferences, "Magento\\Framework\\Config\\ScopeInterface");
        let inst = |fqcn: &str| {
            PhpValue::Array(vec![(PhpKey::Str("_i_".to_owned()), PhpValue::Str(fqcn.to_owned()))])
        };
        for interceptor in interceptors.values() {
            if let Some(PhpValue::Array(entries)) = arguments.get_mut(interceptor) {
                let mut prepend = vec![
                    (PhpKey::Str("____om".to_owned()), inst(&om)),
                    (PhpKey::Str("____scope".to_owned()), inst(&scope)),
                ];
                prepend.append(entries);
                *entries = prepend;
            }
        }
    }

    // NonLazyTypes: candidates = chain-time arguments keys, then
    // instanceTypes values, then preferences values — each in Magento's
    // insertion order (reconstructed below), deduped first-wins, filtered to
    // the lazy-INeligible.
    let non_lazy = non_lazy_types(
        defs,
        export,
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

/// One compiled area: its metadata-file stem (`global`, `frontend`, …), the
/// full [`AreaFile`], and its already-rendered `<code>.php` body.
pub struct CompiledArea {
    pub code: String,
    pub file: AreaFile,
    pub rendered: String,
}

/// Every compiled area — the seven fixed areas in generation order, then
/// custom-registered areas (sorted).
///
/// This is the compile's single most expensive computation (each area runs the
/// full Reader + modification chain), and it was previously done *twice*: once
/// to write the `<code>.php` metadata and again for codegen's incidental
/// `class_exists` sweep. Build it once here and share both consumers. The areas
/// are independent, so they build — and render — in parallel; `collect` into an
/// indexed `Vec` preserves the fixed-then-custom order the sweep and metadata
/// write rely on.
pub fn build_all_area_files(
    magento: &Magento,
    defs: &Definitions,
    root: &std::path::Path,
    fused: bool,
) -> Vec<CompiledArea> {
    use rayon::prelude::*;
    let custom = custom_area_codes(magento);
    let mut files: Vec<CompiledArea> = AREA_CODES
        .par_iter()
        .map(|(area, code)| {
            let file = build_area_file(magento, defs, *area, root, fused);
            let rendered = file.render();
            CompiledArea { code: (*code).to_owned(), file, rendered }
        })
        .collect();
    let custom_files: Vec<CompiledArea> = custom
        .par_iter()
        .map(|code| {
            let export = magento.di_export_custom_area(code);
            let file = build_area_file_from_export(magento, defs, &export, root, fused);
            let rendered = file.render();
            CompiledArea { code: code.clone(), file, rendered }
        })
        .collect();
    files.extend(custom_files);
    files
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
    let push = |candidates: &mut Vec<String>, seen: &mut HashSet<String>, name: &str| {
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
        // Serialize the four sections straight from their borrowed storage —
        // `area_file_to_php` writes each argument value in place, avoiding a deep
        // clone (and drop) of the whole argument tree that a `PhpValue::Array`
        // wrapper + `to_php_file` would incur (~40 ms/area on a real install).
        crate::phpexport::area_file_to_php(
            &self.arguments,
            &self.preferences,
            &self.instance_types,
            &self.non_lazy,
        )
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
