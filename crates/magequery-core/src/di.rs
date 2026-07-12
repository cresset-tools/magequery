//! The merged DI configuration, per area.
//!
//! Magento builds an area's config by merging every module's `etc/di.xml` (in load order)
//! to form the global config, then overlaying every module's `etc/<area>/di.xml` on top.
//! We mirror that: build `global` once, then `global.clone()` overlaid per area. Files are
//! read+parsed in parallel; the sequential merge runs in load order so last-wins is
//! deterministic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::error::Diagnostic;
use crate::ids::{Area, ClassName, ModuleName};
use crate::model::{
    DiExport, Module, PluginDecl, PreferenceDecl, TypeArgDecl, TypeSharedDecl, VirtualTypeDecl,
};
use crate::parse;
use crate::source::Source;

/// A value plus where it was declared.
#[derive(Clone)]
pub(crate) struct Located {
    pub value: ClassName,
    pub source: Source,
}

#[derive(Clone)]
pub(crate) struct LocatedPlugin {
    pub class: Option<ClassName>,
    pub sort_order: i32,
    pub disabled: bool,
    pub source: Source,
    /// Declaration order, for breaking `sort_order` ties the way Magento does: by where the
    /// plugin was first declared — `(area_rank, module load_order, line)`. Global (rank 0)
    /// before area overlay (rank 1); set on first insert, preserved across attribute merges.
    pub order_key: (u8, u32, u32),
}

#[derive(Clone)]
pub(crate) struct LocatedArg {
    pub value: crate::model::ArgValue,
    pub source: Source,
    /// Config layer (0 = primary app/etc, 1 = module global, 2 = area
    /// overlay). Within one layer array arguments merge item-by-item; across
    /// layers Magento's Config::extend replaces same-named args wholesale.
    pub layer: u8,
}

/// A boolean plus where it was declared.
#[derive(Clone)]
pub(crate) struct LocatedBool {
    pub value: bool,
    pub source: Source,
}

/// Fully merged DI config for one area.
#[derive(Clone, Default)]
pub(crate) struct AreaConfig {
    pub preferences: HashMap<ClassName, Located>,
    /// target type -> (plugin name -> plugin)
    pub plugins: HashMap<ClassName, HashMap<String, LocatedPlugin>>,
    pub virtual_types: HashMap<ClassName, Located>,
    /// type/virtualType name -> (argument name -> value). Per-argument last-wins.
    pub type_args: HashMap<ClassName, HashMap<String, LocatedArg>>,
    /// Explicit `shared=` declarations, last-wins (absent = Magento's default: shared).
    pub shared: HashMap<ClassName, LocatedBool>,
}

impl AreaConfig {
    /// Export the merged config wholesale as sorted, owned declarations.
    pub(crate) fn export(&self, area: Area) -> DiExport {
        let mut preferences: Vec<PreferenceDecl> = self
            .preferences
            .iter()
            .map(|(for_type, located)| PreferenceDecl {
                for_type: for_type.clone(),
                prefer: located.value.clone(),
                source: located.source.clone(),
            })
            .collect();
        preferences.sort_by(|a, b| a.for_type.cmp(&b.for_type));

        let mut virtual_types: Vec<VirtualTypeDecl> = self
            .virtual_types
            .iter()
            .map(|(name, located)| VirtualTypeDecl {
                name: name.clone(),
                base: located.value.clone(),
                source: located.source.clone(),
            })
            .collect();
        virtual_types.sort_by(|a, b| a.name.cmp(&b.name));

        // Per target: Magento's execution order — sort_order ascending, ties
        // by declaration order (the stored order_key).
        let mut plugins: Vec<(i32, (u8, u32, u32), PluginDecl)> = Vec::new();
        for (target, by_name) in &self.plugins {
            for (name, plugin) in by_name {
                plugins.push((
                    plugin.sort_order,
                    plugin.order_key,
                    PluginDecl {
                        target: target.clone(),
                        name: name.clone(),
                        class: plugin.class.clone(),
                        sort_order: plugin.sort_order,
                        disabled: plugin.disabled,
                        source: plugin.source.clone(),
                    },
                ));
            }
        }
        plugins.sort_by(|a, b| (&a.2.target, a.0, a.1).cmp(&(&b.2.target, b.0, b.1)));
        let plugins = plugins.into_iter().map(|(_, _, decl)| decl).collect();

        let mut arguments: Vec<TypeArgDecl> = Vec::new();
        for (type_name, by_arg) in &self.type_args {
            for (arg, located) in by_arg {
                arguments.push(TypeArgDecl {
                    type_name: type_name.clone(),
                    arg: arg.clone(),
                    value: located.value.clone(),
                    source: located.source.clone(),
                });
            }
        }
        arguments.sort_by(|a, b| (&a.type_name, &a.arg).cmp(&(&b.type_name, &b.arg)));

        let mut shared: Vec<TypeSharedDecl> = self
            .shared
            .iter()
            .map(|(type_name, located)| TypeSharedDecl {
                type_name: type_name.clone(),
                shared: located.value,
                source: located.source.clone(),
            })
            .collect();
        shared.sort_by(|a, b| a.type_name.cmp(&b.type_name));

        DiExport {
            area,
            preferences,
            virtual_types,
            plugins,
            arguments,
            shared,
        }
    }
}

pub(crate) struct DiIndex {
    global: AreaConfig,
    areas: HashMap<Area, AreaConfig>,
}

impl DiIndex {
    /// The merged config for `area` (the global base for [`Area::Global`]).
    pub fn config(&self, area: Area) -> &AreaConfig {
        match area {
            Area::Global => &self.global,
            other => self.areas.get(&other).unwrap_or(&self.global),
        }
    }
}

/// The non-global areas that overlay the global config, in canonical order.
const REAL_AREAS: [Area; 6] = [
    Area::Frontend,
    Area::Adminhtml,
    Area::Crontab,
    Area::WebapiRest,
    Area::WebapiSoap,
    Area::Graphql,
];

struct Job {
    load_order: usize,
    area: Area,
    module: ModuleName,
    path: PathBuf,
    layer: u8,
}

struct Parsed {
    load_order: usize,
    area: Area,
    module: ModuleName,
    path: PathBuf,
    layer: u8,
    file: Result<parse::DiFile, String>,
}

pub(crate) fn build(root: &Path, modules: &[Module], diags: &mut Vec<Diagnostic>) -> DiIndex {
    // Enumerate di.xml files: Magento's "primary" config (where the framework-level
    // preferences live, e.g. CommandListInterface → CommandList) merged first, then each
    // module's global `etc/di.xml` plus `etc/<area>/di.xml`. Module load orders are
    // shifted by 1 so every primary file sorts before every module (they all share load
    // order 0; the sort is stable, so their glob order is preserved).
    let mut jobs: Vec<Job> = Vec::new();
    for path in primary_di_files(root) {
        jobs.push(Job {
            load_order: 0,
            area: Area::Global,
            module: ModuleName::new("(primary)"),
            path,
            layer: 0,
        });
    }
    for m in modules {
        // Magento only loads enabled modules' configuration.
        if !m.enabled {
            continue;
        }
        let global = m.path.join("etc/di.xml");
        if global.is_file() {
            jobs.push(Job {
                load_order: m.load_order + 1,
                area: Area::Global,
                module: m.name.clone(),
                path: global,
                layer: 1,
            });
        }
        for area in REAL_AREAS {
            let p = m.path.join("etc").join(area.dir().unwrap()).join("di.xml");
            if p.is_file() {
                jobs.push(Job { load_order: m.load_order + 1, area, module: m.name.clone(), path: p, layer: 2 });
            }
        }
    }

    // Read + parse all files in parallel (the heavy part: ~900 files of real XML).
    let parsed: Vec<Parsed> = jobs
        .par_iter()
        .map(|j| {
            let file = std::fs::read_to_string(&j.path)
                .map_err(|e| format!("reading {}: {e}", j.path.display()))
                .and_then(|text| parse::di_xml(&text));
            Parsed {
                load_order: j.load_order,
                area: j.area,
                module: j.module.clone(),
                path: j.path.clone(),
                layer: j.layer,
                file,
            }
        })
        .collect();

    // Surface parse failures once (non-fatal — the rest still merges).
    for p in &parsed {
        if let Err(e) = &p.file {
            diags.push(Diagnostic::warning(format!("parsing {}: {e}", p.path.display()), None));
        }
    }

    let global = merge_area(&parsed, Area::Global, AreaConfig::default());
    let mut areas = HashMap::new();
    for area in REAL_AREAS {
        areas.insert(area, merge_area(&parsed, area, global.clone()));
    }

    DiIndex { global, areas }
}

/// The primary DI config files, exactly as Magento's bootstrap resolves them
/// (`App\Arguments\FileResolver\Primary`): the glob `{*di.xml,*/*di.xml}` under `app/etc` —
/// any file *ending in* `di.xml` directly in `app/etc/` plus one directory level below —
/// in glob order (top-level matches sorted, then subdirectory matches sorted), so e.g. a
/// project's `app/etc/zz_di.xml` merges after (and can override) `app/etc/di.xml`.
fn primary_di_files(root: &Path) -> Vec<PathBuf> {
    let etc = root.join("app/etc");
    let entries = |dir: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> =
            std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        v.sort();
        v
    };
    let is_di_file = |p: &PathBuf| {
        p.is_file()
            && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("di.xml"))
    };

    let top = entries(&etc);
    let mut files: Vec<PathBuf> = top.iter().filter(|p| is_di_file(p)).cloned().collect();
    for dir in top.iter().filter(|p| p.is_dir()) {
        files.extend(entries(dir).into_iter().filter(|p| is_di_file(p)));
    }
    files
}

/// Merge every parsed file for `area` into `base`, in module load order.
fn merge_area(parsed: &[Parsed], area: Area, mut base: AreaConfig) -> AreaConfig {
    let mut order: Vec<&Parsed> =
        parsed.iter().filter(|p| p.area == area && p.file.is_ok()).collect();
    order.sort_by_key(|p| p.load_order);
    for p in order {
        merge_file(&mut base, p);
    }
    base
}

/// Convert a parse-level `RawArg` into a `model::ArgValue`, attaching a `Source` to every
/// array item from the file being merged (`p`).
fn to_arg_value(raw: &parse::RawArg, p: &Parsed) -> crate::model::ArgValue {
    use crate::model::{ArgItem, ArgValue, ObjectRef};
    match raw {
        parse::RawArg::Object { class, shared, sort_order } => ArgValue::Object(ObjectRef {
            class: class.clone(),
            shared: *shared,
            sort_order: *sort_order,
        }),
        parse::RawArg::Scalar { xsi_type, text } => {
            ArgValue::Scalar { xsi_type: xsi_type.clone(), text: text.clone() }
        }
        parse::RawArg::Null => ArgValue::Null,
        parse::RawArg::Array(items) => ArgValue::Array(
            items
                .iter()
                .map(|item| ArgItem {
                    key: item.key.clone(),
                    value: to_arg_value(&item.value, p),
                    sort_order: item.sort_order,
                    source: Source {
                        module: p.module.clone(),
                        file: p.path.clone(),
                        line: item.line,
                        area: p.area,
                    },
                })
                .collect(),
        ),
    }
}

fn merge_file(cfg: &mut AreaConfig, p: &Parsed) {
    let file = match &p.file {
        Ok(f) => f,
        Err(_) => return,
    };
    let src = |line: u32| Source {
        module: p.module.clone(),
        file: p.path.clone(),
        line,
        area: p.area,
    };

    for (for_, type_, line) in &file.preferences {
        cfg.preferences
            .insert(for_.clone(), Located { value: type_.clone(), source: src(*line) });
    }
    for (name, type_, line) in &file.virtual_types {
        cfg.virtual_types
            .insert(name.clone(), Located { value: type_.clone(), source: src(*line) });
    }
    for (name, shared, line) in &file.shared {
        cfg.shared
            .insert(name.clone(), LocatedBool { value: *shared, source: src(*line) });
    }
    for (target, arg_name, raw, line) in &file.arguments {
        let value = to_arg_value(raw, p);
        let by_name = cfg.type_args.entry(target.clone()).or_default();
        match by_name.get_mut(arg_name) {
            // Within one config layer, array arguments merge item-by-item
            // (the layer's files are one XML DOM-merge scope). ACROSS layers
            // (primary -> module global -> area overlay) Magento's
            // Config::extend replaces a same-named argument WHOLESALE — the
            // oracle's OperationPool proves it ('default' from app/etc/di.xml
            // vanishes when a module re-declares 'operations').
            Some(existing) if existing.layer == p.layer => {
                existing.value = existing.value.merged_with(&value);
                existing.source = src(*line);
            }
            Some(existing) => {
                *existing = LocatedArg { value, source: src(*line), layer: p.layer };
            }
            None => {
                by_name.insert(
                    arg_name.clone(),
                    LocatedArg { value, source: src(*line), layer: p.layer },
                );
            }
        }
    }
    for (target, rp) in &file.plugins {
        let by_name = cfg.plugins.entry(target.clone()).or_default();
        match by_name.get_mut(&rp.name) {
            // Attribute-level merge: only override fields the new declaration specifies.
            Some(existing) => {
                if let Some(c) = &rp.class {
                    existing.class = Some(c.clone());
                }
                if let Some(s) = rp.sort_order {
                    existing.sort_order = s;
                }
                if let Some(d) = rp.disabled {
                    existing.disabled = d;
                }
                existing.source = src(rp.line);
            }
            None => {
                let area_rank = if p.area == Area::Global { 0 } else { 1 };
                by_name.insert(
                    rp.name.clone(),
                    LocatedPlugin {
                        class: rp.class.clone(),
                        sort_order: rp.sort_order.unwrap_or(0),
                        disabled: rp.disabled.unwrap_or(false),
                        source: src(rp.line),
                        order_key: (area_rank, p.load_order as u32, rp.line),
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;

    fn src(line: u32) -> Source {
        Source {
            module: ModuleName::new("Acme_Test"),
            file: PathBuf::from("app/code/Acme/Test/etc/di.xml"),
            line,
            area: Area::Global,
        }
    }

    fn located(class: &str, line: u32) -> Located {
        Located {
            value: ClassName::new(class),
            source: src(line),
        }
    }

    #[test]
    fn export_is_sorted_and_complete() {
        let mut config = AreaConfig::default();
        config
            .preferences
            .insert(ClassName::new("Z\\Iface"), located("Z\\Impl", 3));
        config
            .preferences
            .insert(ClassName::new("A\\Iface"), located("A\\Impl", 7));
        config
            .virtual_types
            .insert(ClassName::new("myVirtual"), located("Real\\Base", 9));

        // Three plugins on one target: sort_order wins, declaration order
        // (order_key) breaks the tie — NOT alphabetical by name.
        let mut by_name = HashMap::new();
        by_name.insert(
            "zz_first_declared".to_owned(),
            LocatedPlugin {
                class: Some(ClassName::new("P\\One")),
                sort_order: 10,
                disabled: false,
                source: src(1),
                order_key: (0, 1, 1),
            },
        );
        by_name.insert(
            "aa_later_declared".to_owned(),
            LocatedPlugin {
                class: Some(ClassName::new("P\\Two")),
                sort_order: 10,
                disabled: false,
                source: src(2),
                order_key: (0, 2, 2),
            },
        );
        by_name.insert(
            "runs_first".to_owned(),
            LocatedPlugin {
                class: Some(ClassName::new("P\\Zero")),
                sort_order: 0,
                disabled: true,
                source: src(5),
                order_key: (0, 3, 5),
            },
        );
        config.plugins.insert(ClassName::new("T\\Target"), by_name);

        let export = config.export(Area::Global);

        let prefs: Vec<&str> = export
            .preferences
            .iter()
            .map(|p| p.for_type.as_str())
            .collect();
        assert_eq!(prefs, ["A\\Iface", "Z\\Iface"]);
        assert_eq!(export.preferences[0].prefer, ClassName::new("A\\Impl"));

        assert_eq!(export.virtual_types.len(), 1);
        assert_eq!(export.virtual_types[0].base, ClassName::new("Real\\Base"));

        let plugin_order: Vec<&str> = export.plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            plugin_order,
            ["runs_first", "zz_first_declared", "aa_later_declared"]
        );
        assert!(export.plugins[0].disabled);
        assert_eq!(export.plugins[1].class, Some(ClassName::new("P\\One")));
    }
}
