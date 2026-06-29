//! The merged DI configuration, per area.
//!
//! Magento builds an area's config by merging every module's `etc/di.xml` (in load order)
//! to form the global config, then overlaying every module's `etc/<area>/di.xml` on top.
//! We mirror that: build `global` once, then `global.clone()` overlaid per area. Files are
//! read+parsed in parallel; the sequential merge runs in load order so last-wins is
//! deterministic.

use std::collections::HashMap;
use std::path::PathBuf;

use rayon::prelude::*;

use crate::error::Diagnostic;
use crate::ids::{Area, ClassName, ModuleName};
use crate::model::Module;
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
}

struct Parsed {
    load_order: usize,
    area: Area,
    module: ModuleName,
    path: PathBuf,
    file: Result<parse::DiFile, String>,
}

pub(crate) fn build(modules: &[Module], diags: &mut Vec<Diagnostic>) -> DiIndex {
    // Enumerate di.xml files: global `etc/di.xml` plus `etc/<area>/di.xml` per module.
    let mut jobs: Vec<Job> = Vec::new();
    for m in modules {
        let global = m.path.join("etc/di.xml");
        if global.is_file() {
            jobs.push(Job {
                load_order: m.load_order,
                area: Area::Global,
                module: m.name.clone(),
                path: global,
            });
        }
        for area in REAL_AREAS {
            let p = m.path.join("etc").join(area.dir().unwrap()).join("di.xml");
            if p.is_file() {
                jobs.push(Job { load_order: m.load_order, area, module: m.name.clone(), path: p });
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
    use crate::model::{ArgItem, ArgValue};
    match raw {
        parse::RawArg::Object(c) => ArgValue::Object(c.clone()),
        parse::RawArg::Scalar { xsi_type, text } => {
            ArgValue::Scalar { xsi_type: xsi_type.clone(), text: text.clone() }
        }
        parse::RawArg::Null => ArgValue::Null,
        parse::RawArg::Array(items) => ArgValue::Array(
            items
                .iter()
                .map(|(key, value, line)| ArgItem {
                    key: key.clone(),
                    value: to_arg_value(value, p),
                    source: Source {
                        module: p.module.clone(),
                        file: p.path.clone(),
                        line: *line,
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
    for (target, arg_name, raw, line) in &file.arguments {
        let value = to_arg_value(raw, p);
        let by_name = cfg.type_args.entry(target.clone()).or_default();
        match by_name.get_mut(arg_name) {
            // Array arguments merge item-by-item across modules; others replace.
            Some(existing) => {
                existing.value = existing.value.merged_with(&value);
                existing.source = src(*line);
            }
            None => {
                by_name.insert(arg_name.clone(), LocatedArg { value, source: src(*line) });
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
