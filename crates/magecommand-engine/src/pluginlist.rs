//! The `<scopes>|plugin-list.php` files — Magento's `PluginListGenerator`
//! ported: per scope set, the `[_data, _inherited, _processed]` triple that
//! runtime interception loads. `_data` = merged plugin declarations,
//! `_inherited` = per-type resolved (ancestors folded, sorted) plugin maps,
//! `_processed` = per-method listener chains keyed
//! `{type}_{method}_{prevAroundOrSelf}`.

use std::collections::{HashMap, HashSet};

use magequery_core::{Area, DiExport, Magento, PluginDecl};

use crate::areaconfig::AREA_CODES;
use crate::definitions::Definitions;
use crate::phpexport::{PhpKey, PhpValue};

const LISTENER_BEFORE: i64 = 1;
const LISTENER_AROUND: i64 = 2;
const LISTENER_AFTER: i64 = 4;

/// One plugin's merged declaration, field order preserved.
#[derive(Clone)]
struct Entry {
    sort_order: i32,
    disabled: Option<bool>,
    instance: Option<String>,
    disabled_before_instance: bool,
}

/// Scope-read rank of a core config layer (0 primary, 1 modules, 2 area):
/// the generator reads global modules FIRST, then primary, then the area.
fn read_rank(layer: u8) -> u8 {
    match layer {
        0 => 1, // primary read second
        1 => 0, // module-global read first
        _ => 2,
    }
}

impl Entry {
    fn from_decl(d: &PluginDecl) -> Entry {
        // Within one scope read the mapper always emits
        // [sortOrder, disabled, instance]; a field arriving from a LATER
        // read appends after the existing ones.
        let disabled_before_instance = match (d.disabled_layer, d.instance_layer) {
            (Some(dl), Some(il)) => read_rank(dl) <= read_rank(il),
            _ => true,
        };
        Entry {
            sort_order: d.sort_order,
            disabled: d.disabled_attr,
            instance: d.class.as_ref().map(|c| {
                if d.class_backslash {
                    format!("\\{}", c.as_str())
                } else {
                    c.as_str().to_owned()
                }
            }),
            disabled_before_instance,
        }
    }

    fn render_trimmed(&self) -> PhpValue {
        let mut clone = self.clone();
        clone.instance = clone.instance.map(|i| i.trim_start_matches('\\').to_owned());
        clone.render()
    }

    fn render(&self) -> PhpValue {
        let mut fields = vec![(
            PhpKey::str("sortOrder"),
            PhpValue::Int(self.sort_order as i64),
        )];
        let disabled = self
            .disabled
            .map(|d| (PhpKey::str("disabled"), PhpValue::Bool(d)));
        let instance = self
            .instance
            .as_ref()
            .map(|i| (PhpKey::str("instance"), PhpValue::str(i.clone())));
        if self.disabled_before_instance {
            fields.extend(disabled);
            fields.extend(instance);
        } else {
            fields.extend(instance);
            fields.extend(disabled);
        }
        PhpValue::Array(fields)
    }
}

/// type -> (plugin name -> Entry), both levels in insertion order.
type PluginData = Vec<(String, Vec<(String, Entry)>)>;

/// The scope-read band a declaration belongs to, mirroring the order the
/// generator loads scopes for a given file: global module files, THEN the
/// primary app/etc files, THEN the area overlay. (getAllScopes starts with
/// 'primary', which shuffles the scheme to [global, primary].)
fn band(d: &PluginDecl) -> u8 {
    match (d.decl_layer, d.decl_load_order) {
        (0, 0) => 1, // primary (load order 0 is reserved for app/etc files)
        (0, _) => 0, // module global
        _ => 2,      // area overlay
    }
}

fn plugin_data_of(export: &DiExport) -> PluginData {
    // Types appear in per-scope-read document order. A backslash-spelled
    // `<type name="\X">` is a DISTINCT DOM node with its own position; the
    // generator ltrims when merging, appending that node's plugins after the
    // plain node's. So order within a type = subgroups by (band, node
    // position of the SPELLING), plugins within a subgroup by declaration.
    let node_pos: HashMap<&str, &magequery_core::TypeNodePosition> = export
        .node_positions
        .iter()
        .map(|n| (n.name.as_str(), n))
        .collect();
    let subgroup_key = |d: &PluginDecl| -> (u8, u32) {
        let b = band(d);
        let spelled = if d.target_backslash {
            format!("\\{}", d.target.as_str())
        } else {
            d.target.as_str().to_owned()
        };
        let slot = node_pos.get(spelled.as_str()).and_then(|n| match b {
            0 => n.modules,
            1 => n.primary,
            _ => n.overlay,
        });
        (b, slot.unwrap_or(u32::MAX))
    };
    let mut per_type: HashMap<&str, Vec<&PluginDecl>> = HashMap::new();
    for decl in &export.plugins {
        per_type.entry(decl.target.as_str()).or_default().push(decl);
    }
    let mut types: Vec<(&str, Vec<&PluginDecl>)> = per_type.into_iter().collect();
    for (_, decls) in types.iter_mut() {
        decls.sort_by_key(|d| {
            let (b, pos) = subgroup_key(d);
            (b, pos, d.decl_load_order, d.decl_line)
        });
    }
    types.sort_by_key(|(_, decls)| {
        decls
            .iter()
            .map(|d| {
                let (b, pos) = subgroup_key(d);
                (b, pos, d.decl_load_order, d.decl_line)
            })
            .min()
            .unwrap_or((u8::MAX, u32::MAX, u32::MAX, u32::MAX))
    });
    types
        .into_iter()
        .map(|(name, decls)| {
            (
                name.to_owned(),
                decls
                    .into_iter()
                    .map(|d| (d.name.clone(), Entry::from_decl(d)))
                    .collect(),
            )
        })
        .collect()
}

pub struct GeneratedPluginLists {
    pub files: Vec<(String, String)>,
    pub findings: Vec<String>,
}

/// For the Interception operation: seed the plugin inheritance from `seeds`
/// (`getInterceptedClasses`) and return, for EVERY class the walk populates
/// with non-null plugins, its plugin instance classes unioned across scopes.
/// This is the keyset of `getPluginsConfig` — which, because `inheritPlugins`
/// recurses through parents/interfaces, includes concrete ANCESTORS that
/// themselves carry plugins even when they weren't seeds (e.g. the lone
/// `Symfony\…\Command`, or a class implementing `NoninterceptableInterface`
/// that a seed subclasses). Mirrors `getPluginsList` + `mergeAreaPlugins`:
/// each area is a CLEAN `[global]`/`[global, area]` merge (NOT the plugin-list
/// accumulation bug), areas reading the fully-merged `di_export(area)` and
/// global `di_export(Global)`. Disabled plugins are kept — the snapshot
/// includes them, so their target methods still count as intercepted.
pub struct ClassPlugins {
    /// Every plugin instance class applying (disabled included) — used for the
    /// intercepted-method union. A class appears here when the inherit walk
    /// gave it a non-null config: its own plugins, OR an ancestor's disabled
    /// plugins inherited via a cache hit (order-dependent — see `inherit`).
    pub instances: Vec<String>,
}

pub fn plugin_instances_across_scopes(
    magento: &Magento,
    defs: &Definitions,
    seeds: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, ClassPlugins> {
    let global_export = magento.di_export(Area::Global);
    let global_vtypes: HashMap<String, String> = global_export
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str().to_owned(), v.base.as_str().to_owned()))
        .collect();

    // Each area's inheritance is an INDEPENDENT walk over the same read-only
    // `defs`/`global_vtypes` — its own `Inherit` state, its own export. The
    // order-dependent disabled-plugin quirk lives WITHIN one area's inherit
    // sequence (vtypes, then sorted seeds), never across areas, so the seven
    // areas run in parallel. Determinism is preserved by merging their
    // contributions in the FIXED area order below: the instances Vec then
    // accumulates exactly as the old sequential loop produced it.
    use rayon::prelude::*;
    let per_area: Vec<Vec<(String, Vec<String>)>> = AREA_CODES
        .par_iter()
        .map(|(area, _)| {
            let export_owned;
            let export = if *area == Area::Global {
                &global_export
            } else {
                export_owned = magento.di_export(*area);
                &export_owned
            };
            let plugin_data = plugin_data_of(export);
            let mut state = Inherit {
                defs,
                global_vtypes: &global_vtypes,
                plugin_data: &plugin_data,
                plugin_index: plugin_data
                    .iter()
                    .enumerate()
                    .map(|(i, (t, _))| (t.as_str(), i))
                    .collect(),
                inherited: Vec::new(),
                inherited_index: HashMap::new(),
                processed: Vec::new(),
                findings: Vec::new(),
            };
            // `_loadScopedData` inherits the scope's typed virtual types FIRST,
            // then the class definitions (seeds). Inheriting a virtual type
            // populates ITS ancestors too — that is how a concrete class
            // carrying plugins (e.g. one implementing NoninterceptableInterface,
            // never a seed) still lands in the config and gets an interceptor.
            for v in &export.virtual_types {
                state.inherit(v.name.as_str());
            }
            // Seeds are a HashSet — iterate in a STABLE order. This matters only
            // for the disabled-plugin-inheritance quirk (a subclass inherits an
            // ancestor's disabled plugin iff the ancestor is computed first); a
            // deterministic order keeps the output reproducible. We approximate
            // Magento's scan order (`$definedClasses`) with the class name,
            // which puts core `Magento\…` ancestors before third-party
            // subclasses — the relative order that decides the common
            // cross-module case.
            let mut sorted_seeds: Vec<&String> = seeds.iter().collect();
            sorted_seeds.sort_unstable();
            for class in sorted_seeds {
                state.inherit(class);
            }
            // This area's contribution: EVERY populated class (getPluginsConfig's
            // keyset), in `inherited` insertion order, each with its deduped
            // instance list. A populated-but-instance-less key is kept (empty
            // Vec) so the merged keyset matches the sequential version exactly.
            state
                .inherited
                .iter()
                .filter_map(|(key, cfg)| {
                    let cfg = cfg.as_ref()?;
                    let mut instances: Vec<String> = Vec::new();
                    for (_, e) in cfg {
                        if let Some(inst) = &e.instance {
                            let inst = inst.trim_start_matches('\\').to_owned();
                            if !instances.contains(&inst) {
                                instances.push(inst);
                            }
                        }
                    }
                    Some((key.clone(), instances))
                })
                .collect()
        })
        .collect();

    let mut out: std::collections::HashMap<String, ClassPlugins> =
        std::collections::HashMap::new();
    for contrib in per_area {
        for (key, instances) in contrib {
            let entry = out
                .entry(key)
                .or_insert_with(|| ClassPlugins { instances: Vec::new() });
            for inst in instances {
                if !entry.instances.contains(&inst) {
                    entry.instances.push(inst);
                }
            }
        }
    }
    out
}

pub fn generate(magento: &Magento, defs: &Definitions) -> GeneratedPluginLists {
    let global_export = magento.di_export(Area::Global);
    let global_vtypes: HashMap<String, String> = global_export
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str().to_owned(), v.base.as_str().to_owned()))
        .collect();

    let mut files = Vec::new();
    let mut findings = Vec::new();
    for (area, _code) in AREA_CODES {
        // Scope processing: getAllScopes() = [primary, global, areas...].
        // The global|primary file is written during the PRIMARY pass with
        // scheme [global, primary]; the frontend file still accumulates on
        // top of that state; but the `$scope === 'global'` snapshot never
        // fires (that write is skipped as already-loaded), so every area
        // AFTER frontend starts from an EMPTY base and contains only its own
        // overlay — a genuine Magento bug the archive faithfully records.
        let export_owned;
        let export = match area {
            Area::Global => &global_export,
            Area::Frontend => {
                export_owned = magento.di_export(area);
                &export_owned
            }
            _ => {
                export_owned = magento.di_export_overlay(area);
                &export_owned
            }
        };
        // Scope set + filename: scheme starts ['primary','global']; the
        // current scope moves/appends to the end; the ID sorts them.
        let mut scopes: Vec<&str> = vec!["primary", "global"];
        let area_code = match area {
            Area::Global => "global",
            Area::Frontend => "frontend",
            Area::Adminhtml => "adminhtml",
            Area::Crontab => "crontab",
            Area::WebapiRest => "webapi_rest",
            Area::WebapiSoap => "webapi_soap",
            Area::Graphql => "graphql",
        };
        if area != Area::Global {
            scopes.push(area_code);
        } else {
            // 'global' moves from position 1 to the end — same set.
        }
        let mut sorted = scopes.clone();
        sorted.sort_unstable();
        let filename = format!("{}|plugin-list.php", sorted.join("|"));

        // Seeds: typed virtual types per newly-read layer, in each layer's
        // NODE order (module-band read first, then primary; area files seed
        // their own overlay only).
        let node_pos: HashMap<&str, &magequery_core::TypeNodePosition> = export
            .node_positions
            .iter()
            .map(|n| (n.name.as_str(), n))
            .collect();
        let mut vtype_seeds: Vec<(u8, u32, &str)> = export
            .virtual_types
            .iter()
            .filter(|v| area == Area::Global || v.source.area == area)
            .map(|v| {
                let pos = node_pos.get(v.name.as_str());
                if area != Area::Global {
                    let slot = pos.and_then(|n| n.overlay).unwrap_or(v.decl_order);
                    return (2u8, slot, v.name.as_str());
                }
                // The seed fires in the layer whose doc carries the TYPED
                // declaration; the node position within that layer's doc.
                let typed_primary = v.source.module.as_str() == "(primary)";
                if typed_primary {
                    let slot = pos.and_then(|n| n.primary).unwrap_or(v.decl_order);
                    (1u8, slot, v.name.as_str())
                } else {
                    let slot = pos.and_then(|n| n.modules).unwrap_or(v.decl_order);
                    (0u8, slot, v.name.as_str())
                }
            })
            .collect();
        vtype_seeds.sort();

        let plugin_data = plugin_data_of(export);

        let mut state = Inherit {
            defs,
            global_vtypes: &global_vtypes,
            plugin_data: &plugin_data,
            plugin_index: plugin_data
                .iter()
                .enumerate()
                .map(|(i, (t, _))| (t.as_str(), i))
                .collect(),
            inherited: Vec::new(),
            inherited_index: HashMap::new(),
            processed: Vec::new(),
            findings: Vec::new(),
        };
        for (_, _, seed) in &vtype_seeds {
            state.inherit(seed);
        }
        let type_names: Vec<&str> = plugin_data.iter().map(|(t, _)| t.as_str()).collect();
        for name in type_names {
            state.inherit(name);
        }
        // classDefinitions->getClasses() is the RUNTIME definition during
        // compile — it returns []; no third seed group.

        files.push((filename, render_triple(&plugin_data, &state)));
        findings.extend(state.findings);
    }

    // Custom-registered areas (postcode-nl's postcode_eu, …). They come AFTER
    // graphql in getAllScopes, so — like every scope past frontend — each reads
    // from an EMPTY base and contributes only its own overlay. Seeds are the
    // overlay's virtual types at slot 2 (the overlay export holds nothing else,
    // so there is no source.area filter to apply).
    for code in crate::areaconfig::custom_area_codes(magento) {
        let export = magento.di_export_custom_area_overlay(&code);
        let scopes: Vec<&str> = vec!["primary", "global", code.as_str()];
        let mut sorted = scopes.clone();
        sorted.sort_unstable();
        let filename = format!("{}|plugin-list.php", sorted.join("|"));

        let node_pos: HashMap<&str, &magequery_core::TypeNodePosition> =
            export.node_positions.iter().map(|n| (n.name.as_str(), n)).collect();
        let mut vtype_seeds: Vec<(u8, u32, &str)> = export
            .virtual_types
            .iter()
            .map(|v| {
                let slot =
                    node_pos.get(v.name.as_str()).and_then(|n| n.overlay).unwrap_or(v.decl_order);
                (2u8, slot, v.name.as_str())
            })
            .collect();
        vtype_seeds.sort();

        let plugin_data = plugin_data_of(&export);
        let mut state = Inherit {
            defs,
            global_vtypes: &global_vtypes,
            plugin_data: &plugin_data,
            plugin_index: plugin_data
                .iter()
                .enumerate()
                .map(|(i, (t, _))| (t.as_str(), i))
                .collect(),
            inherited: Vec::new(),
            inherited_index: HashMap::new(),
            processed: Vec::new(),
            findings: Vec::new(),
        };
        for (_, _, seed) in &vtype_seeds {
            state.inherit(seed);
        }
        let type_names: Vec<&str> = plugin_data.iter().map(|(t, _)| t.as_str()).collect();
        for name in type_names {
            state.inherit(name);
        }
        files.push((filename, render_triple(&plugin_data, &state)));
        findings.extend(state.findings);
    }

    GeneratedPluginLists { files, findings }
}

/// The GLOBAL-scope listener chains, flattened for the fused-interceptor
/// renderer. `nodes` maps `{type}_{method}_{prev}` → (before names, around name,
/// after names) — the same `_processed` structure the plugin-list file encodes.
/// `instances` maps a type to its enabled plugins (name → resolved instance
/// FQCN, no leading backslash), in sort order.
pub struct GlobalChains {
    pub nodes: HashMap<String, (Vec<String>, Option<String>, Vec<String>)>,
    pub instances: HashMap<String, Vec<(String, String)>>,
}

/// Run the global-scope inherit/process pass (mirroring `generate`'s Global
/// iteration) and expose the resolved per-method chains for the fused renderer.
pub fn global_plugin_chains(magento: &Magento, defs: &Definitions) -> GlobalChains {
    let global_export = magento.di_export(Area::Global);
    let global_vtypes: HashMap<String, String> = global_export
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str().to_owned(), v.base.as_str().to_owned()))
        .collect();
    let node_pos: HashMap<&str, &magequery_core::TypeNodePosition> =
        global_export.node_positions.iter().map(|n| (n.name.as_str(), n)).collect();
    let mut vtype_seeds: Vec<(u8, u32, &str)> = global_export
        .virtual_types
        .iter()
        .map(|v| {
            let pos = node_pos.get(v.name.as_str());
            if v.source.module.as_str() == "(primary)" {
                (1u8, pos.and_then(|n| n.primary).unwrap_or(v.decl_order), v.name.as_str())
            } else {
                (0u8, pos.and_then(|n| n.modules).unwrap_or(v.decl_order), v.name.as_str())
            }
        })
        .collect();
    vtype_seeds.sort();

    let plugin_data = plugin_data_of(&global_export);
    let mut state = Inherit {
        defs,
        global_vtypes: &global_vtypes,
        plugin_data: &plugin_data,
        plugin_index: plugin_data.iter().enumerate().map(|(i, (t, _))| (t.as_str(), i)).collect(),
        inherited: Vec::new(),
        inherited_index: HashMap::new(),
        processed: Vec::new(),
        findings: Vec::new(),
    };
    for (_, _, seed) in &vtype_seeds {
        state.inherit(seed);
    }
    let type_names: Vec<String> = plugin_data.iter().map(|(t, _)| t.clone()).collect();
    for name in &type_names {
        state.inherit(name);
    }

    let mut nodes = HashMap::new();
    for (key, listeners) in &state.processed {
        let mut before = Vec::new();
        let mut around = None;
        let mut after = Vec::new();
        for (l, v) in listeners {
            match (*l, v) {
                (LISTENER_BEFORE, ProcessedValue::List(names)) => before = names.clone(),
                (LISTENER_AROUND, ProcessedValue::Around(name)) => around = Some(name.clone()),
                (LISTENER_AFTER, ProcessedValue::List(names)) => after = names.clone(),
                _ => {}
            }
        }
        nodes.insert(key.clone(), (before, around, after));
    }
    let mut instances = HashMap::new();
    for (t, v) in &state.inherited {
        if let Some(plugins) = v {
            let list: Vec<(String, String)> = plugins
                .iter()
                .filter(|(_, e)| e.disabled != Some(true))
                .filter_map(|(n, e)| {
                    e.instance
                        .as_ref()
                        .map(|i| (n.clone(), chase(&global_vtypes, i.trim_start_matches('\\'))))
                })
                .collect();
            instances.insert(t.clone(), list);
        }
    }
    GlobalChains { nodes, instances }
}

struct Inherit<'a> {
    defs: &'a Definitions,
    global_vtypes: &'a HashMap<String, String>,
    plugin_data: &'a PluginData,
    plugin_index: HashMap<&'a str, usize>,
    /// type -> None (no plugins) | sorted plugin map — insertion ordered.
    inherited: Vec<(String, Option<Vec<(String, Entry)>>)>,
    inherited_index: HashMap<String, usize>,
    /// `{type}_{method}_{prev}` -> (listener -> value), insertion ordered.
    processed: Vec<(String, Vec<(i64, ProcessedValue)>)>,
    findings: Vec<String>,
}

enum ProcessedValue {
    Around(String),
    List(Vec<String>),
}

impl Inherit<'_> {
    fn inherit(&mut self, type_name: &str) -> Option<Vec<(String, Entry)>> {
        let key = type_name.trim_start_matches('\\').to_owned();
        if let Some(&i) = self.inherited_index.get(&key) {
            // CACHE HIT. Magento's `inheritPlugins` returns `$inherited[$type]`
            // verbatim here — the STORED list, which KEEPS disabled entries.
            // Only the FIRST-computation return path (below) strips disabled.
            // So a class whose ancestor was already computed inherits that
            // ancestor's disabled plugins into its own config; their target
            // methods still generate (passthrough) interceptor wrappers. This
            // is order-dependent, faithful to Magento: whether a subclass gets
            // an ancestor's disabled plugin turns on which was processed first
            // (seed order below is sorted to make it deterministic).
            return self.inherited[i].1.clone();
        }
        // Cycle guard (Magento would recurse forever; none exist).
        self.inherited_index.insert(key.clone(), usize::MAX);

        let real = chase(self.global_vtypes, &key);
        let mut plugins: Vec<(String, Entry)> = Vec::new();
        if real != key {
            if let Some(inherited) = self.inherit(&real) {
                plugins = inherited;
            }
        } else if let Some(record) = self.defs.get(&key) {
            if record.meta.kind == magecommand_php::ClassKind::Class {
                for relation in self.defs.relations_of(&key) {
                    if let Some(relation_plugins) = self.inherit(&relation) {
                        replace_recursive_plugins(&mut plugins, relation_plugins);
                    }
                }
            }
        } else if let Some(relations) = crate::interception::internal_relations(&key) {
            for relation in relations {
                if let Some(relation_plugins) = self.inherit(relation) {
                    replace_recursive_plugins(&mut plugins, relation_plugins);
                }
            }
        }
        if let Some(&i) = self.plugin_index.get(key.as_str()) {
            replace_recursive_plugins(&mut plugins, self.plugin_data[i].1.clone());
        }

        let value = if plugins.is_empty() {
            None
        } else {
            // filterPlugins: entries without an instance are dropped.
            plugins.retain(|(_, e)| e.instance.is_some());
            // Stable sort by sortOrder.
            plugins.sort_by_key(|(_, e)| e.sort_order);
            self.process(&key, &plugins);
            Some(plugins)
        };
        let index = self.inherited.len();
        // Store the FULL list (disabled kept) — this is `_inherited`, what the
        // plugin-list file renders and what the interception SET reads. But on
        // THIS first-computation path RETURN the enabled-only list: Magento's
        // `inheritPlugins` returns `$plugins` after unsetting disabled entries.
        // So a child computed DURING its parent's first computation misses the
        // parent's disabled plugins; a child computed after the parent already
        // cached gets them (the cache-hit path above). Order decides.
        let ret = enabled_only(value.as_deref());
        self.inherited.push((key.clone(), value));
        self.inherited_index.insert(key, index);
        ret
    }

    fn process(&mut self, type_name: &str, plugins: &[(String, Entry)]) {
        let mut last_per_method: HashMap<String, String> = HashMap::new();
        for (plugin_name, entry) in plugins {
            if entry.disabled == Some(true) {
                continue;
            }
            let Some(instance) = &entry.instance else { continue };
            let plugin_type = chase(self.global_vtypes, instance.trim_start_matches('\\'));
            if self.defs.get(&plugin_type).is_none() {
                self.findings
                    .push(format!("plugin class {plugin_type} not in the class universe"));
                continue;
            }
            // getMethodList: bits OR'd per TARGET method (map insertion at
            // the first prefix-method seen), then per target the checks run
            // in fixed AROUND, BEFORE, AFTER order.
            let mut method_bits: Vec<(String, i64)> = Vec::new();
            for method in self.defs.public_methods(&plugin_type) {
                let prefix = &method[..method.len().min(5)];
                let (listener, cut) = match prefix {
                    "befor" => (LISTENER_BEFORE, 6),
                    "aroun" => (LISTENER_AROUND, 6),
                    "after" => (LISTENER_AFTER, 5),
                    _ => continue,
                };
                if method.len() < cut {
                    continue;
                }
                let target = lcfirst(&method[cut..]);
                match method_bits.iter_mut().find(|(t, _)| *t == target) {
                    Some((_, bits)) => *bits |= listener,
                    None => method_bits.push((target, listener)),
                }
            }
            for (target, bits) in method_bits {
                let current = last_per_method
                    .get(&target)
                    .cloned()
                    .unwrap_or_else(|| "__self".to_owned());
                let key = format!("{type_name}_{target}_{current}");
                let slot = match self.processed.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v,
                    None => {
                        self.processed.push((key.clone(), Vec::new()));
                        &mut self.processed.last_mut().unwrap().1
                    }
                };
                if bits & LISTENER_AROUND != 0 {
                    match slot.iter_mut().find(|(l, _)| *l == LISTENER_AROUND) {
                        Some((_, v)) => *v = ProcessedValue::Around(plugin_name.clone()),
                        None => slot.push((
                            LISTENER_AROUND,
                            ProcessedValue::Around(plugin_name.clone()),
                        )),
                    }
                    last_per_method.insert(target.clone(), plugin_name.clone());
                }
                for l in [LISTENER_BEFORE, LISTENER_AFTER] {
                    if bits & l == 0 {
                        continue;
                    }
                    match slot.iter_mut().find(|(sl, _)| *sl == l) {
                        Some((_, ProcessedValue::List(list))) => list.push(plugin_name.clone()),
                        Some((_, v)) => *v = ProcessedValue::List(vec![plugin_name.clone()]),
                        None => slot.push((l, ProcessedValue::List(vec![plugin_name.clone()]))),
                    }
                }
            }
        }
    }
}

/// The child-inheritance view of a stored plugin config: disabled entries
/// removed (Magento's `inheritPlugins` returns `$plugins` after unsetting
/// them). An empty result is `None`, matching PHP's falsy `if ($plugins)`
/// check that skips the parent merge.
fn enabled_only(stored: Option<&[(String, Entry)]>) -> Option<Vec<(String, Entry)>> {
    let stored = stored?;
    let filtered: Vec<(String, Entry)> = stored
        .iter()
        .filter(|(_, e)| e.disabled != Some(true))
        .cloned()
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn chase(map: &HashMap<String, String>, name: &str) -> String {
    let mut current = name;
    let mut seen = HashSet::new();
    while let Some(next) = map.get(current) {
        if !seen.insert(next.as_str()) {
            break;
        }
        current = next;
    }
    current.to_owned()
}

fn lcfirst(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// array_replace_recursive over plugin maps: same name -> field-level merge
/// keeping the base entry's field order; new names append.
fn replace_recursive_plugins(base: &mut Vec<(String, Entry)>, over: Vec<(String, Entry)>) {
    for (name, entry) in over {
        match base.iter_mut().find(|(n, _)| *n == name) {
            Some((_, existing)) => {
                existing.sort_order = entry.sort_order;
                if entry.disabled.is_some() {
                    if existing.disabled.is_none() {
                        // A newly-appearing key appends AFTER existing ones.
                        existing.disabled_before_instance = existing.instance.is_none();
                    }
                    existing.disabled = entry.disabled;
                }
                if entry.instance.is_some() {
                    if existing.instance.is_none() && existing.disabled.is_some() {
                        existing.disabled_before_instance = true;
                    }
                    existing.instance = entry.instance;
                }
            }
            None => base.push((name, entry)),
        }
    }
}

fn render_triple(plugin_data: &PluginData, state: &Inherit) -> String {
    let data_value = PhpValue::Array(
        plugin_data
            .iter()
            .map(|(t, plugins)| {
                (
                    PhpKey::str(t.clone()),
                    PhpValue::Array(
                        plugins
                            .iter()
                            .map(|(n, e)| (PhpKey::str(n.clone()), e.render()))
                            .collect(),
                    ),
                )
            })
            .collect(),
    );
    let inherited_value = PhpValue::Array(
        state
            .inherited
            .iter()
            .map(|(t, v)| {
                let value = match v {
                    None => PhpValue::Null,
                    Some(plugins) => PhpValue::Array(
                        plugins
                            .iter()
                            .map(|(n, e)| (PhpKey::str(n.clone()), e.render_trimmed()))
                            .collect(),
                    ),
                };
                (PhpKey::str(t.clone()), value)
            })
            .collect(),
    );
    let processed_value = PhpValue::Array(
        state
            .processed
            .iter()
            .map(|(k, listeners)| {
                (
                    PhpKey::str(k.clone()),
                    PhpValue::Array(
                        listeners
                            .iter()
                            .map(|(l, v)| {
                                let value = match v {
                                    ProcessedValue::Around(name) => PhpValue::str(name.clone()),
                                    ProcessedValue::List(names) => PhpValue::Array(
                                        names
                                            .iter()
                                            .enumerate()
                                            .map(|(i, n)| {
                                                (PhpKey::Int(i as i64), PhpValue::str(n.clone()))
                                            })
                                            .collect(),
                                    ),
                                };
                                (PhpKey::Int(*l), value)
                            })
                            .collect(),
                    ),
                )
            })
            .collect(),
    );
    crate::phpexport::to_php_file(&PhpValue::Array(vec![
        (PhpKey::Int(0), data_value),
        (PhpKey::Int(1), inherited_value),
        (PhpKey::Int(2), processed_value),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definitions::ClassRecord;
    use std::path::PathBuf;

    fn record(src: &str) -> (String, ClassRecord) {
        let meta = magecommand_php::parse_file(src.as_bytes())
            .declarations
            .into_iter()
            .next()
            .expect("one declaration");
        (meta.fqcn.clone(), ClassRecord { meta, file: PathBuf::new() })
    }

    fn defs(records: impl IntoIterator<Item = (String, ClassRecord)>) -> Definitions {
        Definitions::from_records(records)
    }

    fn entry(instance: &str, disabled: bool) -> Entry {
        Entry {
            sort_order: 0,
            disabled: disabled.then_some(true),
            instance: Some(instance.to_owned()),
            disabled_before_instance: true,
        }
    }

    fn state<'a>(
        defs: &'a Definitions,
        vtypes: &'a HashMap<String, String>,
        plugin_data: &'a PluginData,
    ) -> Inherit<'a> {
        Inherit {
            defs,
            global_vtypes: vtypes,
            plugin_data,
            plugin_index: plugin_data
                .iter()
                .enumerate()
                .map(|(i, (t, _))| (t.as_str(), i))
                .collect(),
            inherited: Vec::new(),
            inherited_index: HashMap::new(),
            processed: Vec::new(),
            findings: Vec::new(),
        }
    }

    fn derived_plugin_names<'a>(s: &'a Inherit) -> Vec<&'a str> {
        s.inherited
            .iter()
            .find(|(k, _)| k.as_str() == "Derived")
            .and_then(|(_, v)| v.as_ref())
            .expect("Derived has a config")
            .iter()
            .map(|(n, _)| n.as_str())
            .collect()
    }

    /// The order-dependent disabled-plugin inheritance quirk, ported faithfully
    /// (validated byte-exact on the oracle synthetic Acme_PluginDisable): Base
    /// carries a DISABLED plugin, Derived a subclass with an enabled one.
    /// Magento's `inheritPlugins` returns the with-disabled `$inherited[$type]`
    /// on a CACHE HIT but the enabled-only `$plugins` on first computation. So
    /// when Base is computed FIRST, Derived cache-hits Base and inherits its
    /// disabled plugin (whose target method still gets a passthrough
    /// interceptor). Computed the other way round, Derived misses it.
    #[test]
    fn subclass_inherits_ancestors_disabled_plugin_only_when_ancestor_computed_first() {
        let defs = defs([
            record("<?php class Base { public function gamma() {} public function delta() {} }"),
            record("<?php class Derived extends Base {}"),
        ]);
        let vtypes = HashMap::new();
        let plugin_data: PluginData = vec![
            ("Base".to_owned(), vec![("base_gamma".to_owned(), entry("BasePlugin", true))]),
            ("Derived".to_owned(), vec![("derived_delta".to_owned(), entry("DerivedPlugin", false))]),
        ];

        // Ancestor FIRST: Derived cache-hits Base -> inherits the disabled plugin.
        let mut s = state(&defs, &vtypes, &plugin_data);
        s.inherit("Base");
        s.inherit("Derived");
        let names = derived_plugin_names(&s);
        assert!(names.contains(&"base_gamma"), "cache hit inherits disabled plugin: {names:?}");
        assert!(names.contains(&"derived_delta"));

        // Descendant FIRST: Derived's own recursion first-computes Base, whose
        // return strips disabled -> Derived does NOT inherit it.
        let mut s = state(&defs, &vtypes, &plugin_data);
        s.inherit("Derived");
        s.inherit("Base");
        let names = derived_plugin_names(&s);
        assert!(!names.contains(&"base_gamma"), "first-computation strips disabled: {names:?}");
        assert!(names.contains(&"derived_delta"));
    }
}
