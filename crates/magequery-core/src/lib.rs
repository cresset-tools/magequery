//! magequery-core: static analysis of a Magento 2 codebase.
//!
//! The library never prints, never exits, and never reads ambient state — it takes a
//! filesystem root, builds an index, and returns owned, structured data. The `magequery`
//! CLI is a thin renderer on top.
//!
//! ```no_run
//! use magequery_core::{Magento, ids::{ClassName, Area}};
//!
//! let mage = Magento::open("/path/to/magento")?;
//! for d in mage.diagnostics() {
//!     eprintln!("warning: {}", d.message);
//! }
//! let res = mage.resolve(&ClassName::new("Magento\\Quote\\Api\\CartManagementInterface"), Area::Global)?;
//! println!("instantiates {}", res.concrete);
//! # Ok::<(), magequery_core::Error>(())
//! ```

pub mod error;
pub mod ids;
pub mod model;
pub mod source;

mod breadth;
mod composer;
mod di;
mod index;
mod parse;
mod php;
mod resolver;

pub use error::{Diagnostic, Error, Result, Severity};
pub use ids::{Area, ClassName, ConfigPath, EventName, ModuleName};
pub use model::{
    ArgValue, Argument, ByArea, ChainPluginRef, ChainStep, CronJob, InterceptKind, MethodChain,
    Module, ModuleCheck, Observer, Preference, PreferenceStep, Plugin, PluginMethod, Resolution,
    Route, UnregisteredModule, WebapiRoute,
};
pub use source::Source;

use std::path::Path;
use std::sync::OnceLock;

/// An opened Magento installation. Holds the parsed module index and merged per-area DI
/// config; every query method is a projection over it. Cheap to query, built once on
/// [`open`](Magento::open).
pub struct Magento {
    // Filled in by the indexer (steps 1–3). Kept private so the internal representation
    // can evolve without breaking the public, struct-returning API.
    index: index::Index,
    // Breadth indexes, built lazily on first use so they don't slow the core commands.
    events: OnceLock<breadth::EventIndex>,
    cron: OnceLock<breadth::CronIndex>,
    routes: OnceLock<breadth::RouteIndex>,
    webapi: OnceLock<breadth::WebapiIndex>,
}

impl Magento {
    /// Build the index for the installation rooted at `root`.
    ///
    /// Fails only on problems that prevent any meaningful answer (no Magento root,
    /// unreadable `config.php`). Per-module problems are recorded as [`Diagnostic`]s and
    /// retrieved via [`diagnostics`](Magento::diagnostics) — a single broken file does not
    /// fail the build.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let index = index::Index::build(root.as_ref())?;
        Ok(Self {
            index,
            events: OnceLock::new(),
            cron: OnceLock::new(),
            routes: OnceLock::new(),
            webapi: OnceLock::new(),
        })
    }

    /// Non-fatal problems found while building the index, in discovery order.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.index.diagnostics
    }

    /// All modules, in `config.php` load order.
    pub fn modules(&self) -> &[Module] {
        &self.index.modules
    }

    /// Consistency between the modules on disk and those listed in `config.php`. A
    /// non-clean result usually means `bin/magento setup:upgrade` was not run.
    pub fn module_check(&self) -> &ModuleCheck {
        &self.index.check
    }

    /// The concrete type Magento instantiates for `class` in `area`, with the full
    /// preference chain. If no preference applies, the class is its own concrete type
    /// (empty chain) — matching Magento, which instantiates the requested class directly.
    pub fn preference(&self, class: &ClassName, area: Area) -> Result<Preference> {
        let cfg = self.index.di.config(area);
        let mut current = class.clone();
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Follow `for -> type` redirects to a fixpoint, guarding against cycles.
        while seen.insert(current.clone()) {
            match cfg.preferences.get(&current) {
                Some(located) => {
                    let to = located.value.clone();
                    chain.push(PreferenceStep {
                        from: current.clone(),
                        to: to.clone(),
                        source: located.source.clone(),
                    });
                    if to == current {
                        break;
                    }
                    current = to;
                }
                None => break,
            }
        }

        // With no preference, the class is its own concrete type — but only if it actually
        // exists. Otherwise the user gave a name that resolves to nothing; say so.
        if chain.is_empty() && !self.class_known(class, area) {
            return Err(Error::ClassNotFound(class.clone()));
        }

        Ok(Preference { requested: class.clone(), concrete: current, chain, area })
    }

    /// Whether `class` is something we can see: a real source file (PSR-4), a virtualType,
    /// or a type referenced by DI config. Used to distinguish a real concrete class from a
    /// name that resolves to nothing. Checks the area's config plus the global base.
    fn class_known(&self, class: &ClassName, area: Area) -> bool {
        if self.index.resolver.exists(class) {
            return true;
        }
        let referenced = |cfg: &di::AreaConfig| {
            cfg.virtual_types.contains_key(class) || cfg.plugins.contains_key(class)
        };
        referenced(self.index.di.config(area)) || referenced(self.index.di.config(Area::Global))
    }

    /// Plugins that fire on `class` in `area`, in execution order. The preference is
    /// resolved first, then plugins are collected from the concrete type **and every
    /// ancestor/interface** — so plugins declared on an interface or parent are included
    /// (each tagged with `declared_on`). Disabled plugins are included but flagged.
    ///
    /// A plugin *name* is unique across the resolved type's hierarchy: if the same name is
    /// declared on both the concrete class and an ancestor, the nearest one wins (Magento
    /// merges by name). Order is Magento's: ascending `sort_order`, ties broken by
    /// declaration order (module load order, then position in file) — not by name.
    pub fn plugins(&self, class: &ClassName, area: Area) -> Result<Vec<Plugin>> {
        let concrete = self.preference(class, area)?.concrete;
        let targets = self.plugin_targets(&concrete);
        let mut collected = self.collect_plugins(area, &targets);
        // Execution order: sort_order, then declaration order (load order, then line).
        collected.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(collected.into_iter().map(|(_, p)| p).collect())
    }

    /// Plugins across **all areas**, merged into one set: each plugin appears once, tagged
    /// (via its `source.area`) with where it's declared — `base` (global) or a specific
    /// area. Base plugins win a name clash over an area override. Ordered like `plugins()`.
    /// Targets are taken from the global concrete (preference rarely differs per area).
    pub fn plugins_all_areas(&self, class: &ClassName) -> Result<Vec<Plugin>> {
        let concrete = self.preference(class, Area::Global)?.concrete;
        let targets = self.plugin_targets(&concrete);

        use std::collections::{BTreeSet, HashMap};
        let mut best: HashMap<String, ((i32, (u8, u32, u32)), Plugin)> = HashMap::new();
        // Every area a given plugin name is declared in (across the merge).
        let mut areas_of: HashMap<String, BTreeSet<Area>> = HashMap::new();
        for area in Area::ALL {
            for (key, plugin) in self.collect_plugins(area, &targets) {
                areas_of.entry(plugin.name.clone()).or_default().insert(plugin.source.area);
                match best.get(&plugin.name) {
                    // Keep the lowest area_rank (base, rank 0) over an area overlay (rank 1).
                    Some((bk, _)) if bk.1 .0 <= key.1 .0 => {}
                    _ => {
                        best.insert(plugin.name.clone(), (key, plugin));
                    }
                }
            }
        }
        let mut v: Vec<_> = best.into_values().collect();
        for (_, p) in &mut v {
            p.areas = areas_of[&p.name].iter().copied().collect();
        }
        v.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(v.into_iter().map(|(_, p)| p).collect())
    }

    /// The concrete type plus its ancestors/interfaces — the set of types whose plugins
    /// apply to the concrete.
    fn plugin_targets(&self, concrete: &ClassName) -> Vec<ClassName> {
        let mut targets = vec![concrete.clone()];
        targets.extend(self.index.resolver.ancestors(concrete));
        targets
    }

    /// Collect plugins for `targets` in one area, keyed sort-order + declaration order.
    /// Dedups by name nearest-target-first (concrete wins over an ancestor).
    fn collect_plugins(
        &self,
        area: Area,
        targets: &[ClassName],
    ) -> Vec<((i32, (u8, u32, u32)), Plugin)> {
        let cfg = self.index.di.config(area);
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for target in targets {
            let Some(by_name) = cfg.plugins.get(target) else { continue };
            for (name, lp) in by_name {
                if !seen.insert(name.clone()) {
                    continue;
                }
                let Some(plugin_class) = lp.class.clone() else { continue };
                let methods = self.index.resolver.plugin_methods(&plugin_class);
                out.push((
                    (lp.sort_order, lp.order_key),
                    Plugin {
                        name: name.clone(),
                        class: plugin_class,
                        sort_order: lp.sort_order,
                        methods,
                        declared_on: target.clone(),
                        disabled: lp.disabled,
                        areas: vec![lp.source.area],
                        source: lp.source.clone(),
                    },
                ));
            }
        }
        out
    }

    /// The interceptor execution chain (the "onion") for each method intercepted on `class`
    /// in `area`: before plugins (ascending `sort_order`), around plugins nested
    /// (ascending = outer), the target method, then around unwinding and after plugins
    /// (descending). Disabled plugins are excluded (they don't run). `only` restricts to a
    /// single method name.
    ///
    /// Note: this is the standard onion. Magento's exact segmentation when `around` plugins
    /// interleave with the before/after of *other* plugins across sort orders is simplified
    /// here (all befores, then all arounds, etc.) — accurate for the common case.
    pub fn plugin_chains(
        &self,
        class: &ClassName,
        area: Area,
        only: Option<&str>,
    ) -> Result<Vec<MethodChain>> {
        Ok(chains_from(&self.plugins(class, area)?, only))
    }

    /// Like [`plugin_chains`](Magento::plugin_chains) but over the merged all-areas plugin
    /// set (see [`plugins_all_areas`](Magento::plugins_all_areas)) — one onion per method
    /// showing every plugin that can intercept it across areas, each tagged by its area.
    pub fn plugin_chains_all_areas(
        &self,
        class: &ClassName,
        only: Option<&str>,
    ) -> Result<Vec<MethodChain>> {
        Ok(chains_from(&self.plugins_all_areas(class)?, only))
    }

    /// Observers bound to `event` in `area`.
    pub fn observers(&self, event: &EventName, area: Area) -> Vec<Observer> {
        self.events_index().observers(event, area)
    }

    /// All events in `area` with their observer counts.
    pub fn events(&self, area: Area) -> Vec<(EventName, usize)> {
        self.events_index().events(area)
    }

    /// Cron jobs, optionally restricted to one group.
    pub fn cron_jobs(&self, group: Option<&str>) -> Vec<CronJob> {
        self.cron.get_or_init(|| breadth::CronIndex::build(&self.index.modules)).jobs(group)
    }

    /// Frontend/adminhtml routes (frontName → modules) in `area`.
    pub fn routes(&self, area: Area) -> Vec<Route> {
        self.routes.get_or_init(|| breadth::RouteIndex::build(&self.index.modules)).routes(area)
    }

    /// REST endpoints from `webapi.xml`, optionally filtered by a URL substring.
    pub fn webapi(&self, url_filter: Option<&str>) -> Vec<WebapiRoute> {
        self.webapi.get_or_init(|| breadth::WebapiIndex::build(&self.index.modules)).routes(url_filter)
    }

    fn events_index(&self) -> &breadth::EventIndex {
        self.events.get_or_init(|| breadth::EventIndex::build(&self.index.modules))
    }

    /// The flagship: full DI resolution of `class` in a single `area` — the concrete type,
    /// preference chain, virtual-type indirection, merged constructor arguments, and the
    /// plugin chain, with provenance throughout.
    pub fn resolve(&self, class: &ClassName, area: Area) -> Result<Resolution> {
        let pref = self.preference(class, area)?;
        let concrete = pref.concrete.clone();
        let cfg = self.index.di.config(area);

        // If the concrete is a virtual type, follow the type= chain to the real class.
        let instantiates = {
            let mut cur = concrete.clone();
            let mut seen = std::collections::HashSet::new();
            let mut is_virtual = false;
            while let Some(vt) = cfg.virtual_types.get(&cur) {
                if !seen.insert(cur.clone()) {
                    break;
                }
                is_virtual = true;
                cur = vt.value.clone();
            }
            is_virtual.then_some(cur)
        };

        let arguments = self.resolve_arguments(&concrete, area);
        let plugins = self.plugins(class, area)?;

        // Ancestors/interfaces that actually contributed plugins or arguments.
        let contributing_ancestors = self
            .index
            .resolver
            .ancestors(&concrete)
            .into_iter()
            .filter(|a| cfg.plugins.contains_key(a) || cfg.type_args.contains_key(a))
            .collect();

        Ok(Resolution {
            requested: class.clone(),
            area,
            concrete,
            preference_chain: pref.chain,
            instantiates,
            plugins,
            arguments,
            contributing_ancestors,
        })
    }

    /// Merged constructor arguments for `name` in `area`: virtual-type base args overlaid by
    /// the virtual type's own; for a real type, parent-type args (along the PHP ancestor
    /// chain) overlaid by the type's own. Per-argument last-wins; sorted by name.
    fn resolve_arguments(&self, name: &ClassName, area: Area) -> Vec<Argument> {
        let map = self.args_of(name, area, &mut std::collections::HashSet::new());
        let mut v: Vec<Argument> = map
            .into_iter()
            .map(|(name, (value, source))| Argument { name, value, source })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn args_of(
        &self,
        name: &ClassName,
        area: Area,
        seen: &mut std::collections::HashSet<ClassName>,
    ) -> std::collections::HashMap<String, (ArgValue, Source)> {
        let mut merged = std::collections::HashMap::new();
        if !seen.insert(name.clone()) {
            return merged; // cycle guard
        }
        let cfg = self.index.di.config(area);

        if let Some(vt) = cfg.virtual_types.get(name) {
            // Virtual type: inherit the base type's args, then overlay our own.
            merged = self.args_of(&vt.value, area, seen);
        } else {
            // Real type: merge parent-type args (distant ancestor first), self overrides.
            let mut chain = self.index.resolver.ancestors(name);
            chain.reverse();
            for ancestor in &chain {
                merge_args_into(&mut merged, cfg.type_args.get(ancestor));
            }
        }

        merge_args_into(&mut merged, cfg.type_args.get(name));
        merged
    }

    /// [`resolve`](Magento::resolve) across every [`Area`]. The caller renders it directly
    /// (`--all-areas`) or via [`ByArea::deltas`] (the default collapsed-diff view).
    pub fn resolve_all(&self, class: &ClassName) -> Result<ByArea<Resolution>> {
        let _ = class;
        todo!()
    }
}

/// Build the execution chain (onion) per intercepted method from a run-ordered plugin list:
/// before (ascending) → around (nested ascending) → target → around (unwind) → after
/// (descending). `plugins` must already be in canonical run order; we only filter, so the
/// order is preserved. Disabled plugins are excluded.
/// Deep-merge one type's declared arguments into the accumulator (array args merge
/// item-by-item; scalars/objects replace).
fn merge_args_into(
    merged: &mut std::collections::HashMap<String, (ArgValue, Source)>,
    args: Option<&std::collections::HashMap<String, di::LocatedArg>>,
) {
    let Some(args) = args else { return };
    for (k, la) in args {
        let value = match merged.get(k) {
            Some((existing, _)) => existing.merged_with(&la.value),
            None => la.value.clone(),
        };
        merged.insert(k.clone(), (value, la.source.clone()));
    }
}

fn chains_from(plugins: &[Plugin], only: Option<&str>) -> Vec<MethodChain> {
    use std::collections::BTreeMap;

    let mut by_method: BTreeMap<String, Vec<(InterceptKind, ChainPluginRef)>> = BTreeMap::new();
    for p in plugins {
        if p.disabled {
            continue;
        }
        for m in &p.methods {
            if only.is_some_and(|f| f != m.target) {
                continue;
            }
            let r = ChainPluginRef {
                name: p.name.clone(),
                class: p.class.clone(),
                plugin_method: m.plugin_method.clone(),
                sort_order: p.sort_order,
                declared_on: p.declared_on.clone(),
                source: p.source.clone(),
                areas: p.areas.clone(),
            };
            by_method.entry(m.target.clone()).or_default().push((m.kind, r));
        }
    }

    let mut chains = Vec::new();
    for (method, parts) in by_method {
        let pick = |kind: InterceptKind| -> Vec<ChainPluginRef> {
            parts.iter().filter(|(k, _)| *k == kind).map(|(_, r)| r.clone()).collect()
        };
        let befores = pick(InterceptKind::Before);
        let arounds = pick(InterceptKind::Around);
        let afters = pick(InterceptKind::After);

        let mut steps = Vec::new();
        for r in &befores {
            steps.push(ChainStep::Before(r.clone()));
        }
        for r in &arounds {
            steps.push(ChainStep::AroundEnter(r.clone()));
        }
        steps.push(ChainStep::Target);
        for r in arounds.iter().rev() {
            steps.push(ChainStep::AroundExit(r.clone()));
        }
        for r in afters.iter().rev() {
            steps.push(ChainStep::After(r.clone()));
        }
        chains.push(MethodChain { method, steps });
    }
    chains
}
