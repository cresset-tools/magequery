//! Dependency-injection and wiring [`Magento`] queries.

use super::super::*;

impl Magento {
    /// The concrete type Magento instantiates for `class` in `area`, with the full
    /// preference chain. If no preference applies, the class is its own concrete type
    /// (empty chain) — matching Magento, which instantiates the requested class directly.
    pub fn preference(&self, class: &ClassName, area: Area) -> Result<Preference> {
        let cfg = self.di_index().config(area);
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
        let referenced = |cfg: &engine::di::AreaConfig| {
            cfg.virtual_types.contains_key(class) || cfg.plugins.contains_key(class)
        };
        referenced(self.di_index().config(area)) || referenced(self.di_index().config(Area::Global))
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
        let targets = self.plugin_lookup_chain(&concrete);
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
        let targets = self.plugin_lookup_chain(&concrete);

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
    fn plugin_lookup_chain(&self, concrete: &ClassName) -> Vec<ClassName> {
        let mut targets = vec![concrete.clone()];
        targets.extend(self.index.resolver.ancestors(concrete));
        targets
    }

    /// Every `<plugin>` declaration whose *class* is `class` — the reverse of
    /// [`plugins`](Magento::plugins): given a plugin class, the types it intercepts.
    /// Merged across all areas, deduped by (type, plugin name), sorted. `declared_on` is
    /// the type as written in di.xml (often an interface/parent); resolve it with
    /// [`preference`](Magento::preference) for the concrete class that actually runs.
    pub fn plugin_targets(&self, class: &ClassName) -> Vec<PluginTarget> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for area in Area::ALL {
            let cfg = self.di_index().config(area);
            for (target, plugins) in &cfg.plugins {
                for (name, plugin) in plugins {
                    if plugin.class.as_ref() != Some(class) {
                        continue;
                    }
                    if seen.insert((target.clone(), name.clone())) {
                        out.push(PluginTarget {
                            declared_on: target.clone(),
                            plugin_name: name.clone(),
                            disabled: plugin.disabled,
                            source: plugin.source.clone(),
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            (a.declared_on.as_str(), &a.plugin_name).cmp(&(b.declared_on.as_str(), &b.plugin_name))
        });
        out
    }

    /// The full ancestor set of a class (parents + interfaces, transitively,
    /// nearest-first) — the hierarchy plugin resolution walks, exposed for frontends
    /// that need it themselves (the LSP's jump from a plugin method to the intercepted
    /// implementation searches it for the method's defining file).
    pub fn ancestors(&self, class: &ClassName) -> Vec<ClassName> {
        self.index.resolver.ancestors(class)
    }

    /// Every class name derivable from the composer autoload maps and the app/code
    /// convention — the completion candidate set, sorted and deduped. A parallel
    /// directory walk (~100ms on a real install), so long-lived frontends cache the
    /// result outside the handle: it only changes when PHP files are created or
    /// deleted, never on content edits or handle rebuilds.
    pub fn class_names(&self) -> Vec<ClassName> {
        self.index.resolver.class_names()
    }

    /// Every virtual type name declared in any area's merged DI config — completion
    /// candidates alongside [`class_names`](Magento::class_names) (virtual types are
    /// valid wherever di.xml expects a type).
    pub fn virtual_type_names(&self) -> Vec<ClassName> {
        let mut names = std::collections::BTreeSet::new();
        for area in Area::ALL {
            for name in self.di_index().config(area).virtual_types.keys() {
                names.insert(name.clone());
            }
        }
        names.into_iter().collect()
    }

    /// Collect plugins for `targets` in one area, keyed sort-order + declaration order.
    /// Dedups by name nearest-target-first (concrete wins over an ancestor).
    fn collect_plugins(
        &self,
        area: Area,
        targets: &[ClassName],
    ) -> Vec<((i32, (u8, u32, u32)), Plugin)> {
        let cfg = self.di_index().config(area);
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

    /// Reverse DI — everything the merged di.xml config wires *to* `class` (which may
    /// itself be a virtual type): the types whose preference resolves to it, the virtual
    /// types built on it, and every constructor argument (incl. nested array items) that
    /// injects it — as the class itself, its generated `\Proxy`, or its name as a string
    /// (factory/pool style). di.xml facts only: plain constructor type-hints resolved by
    /// autowiring have no di.xml declaration and aren't listed.
    ///
    /// With `area: None`, scans the global config plus each area's **own** declarations
    /// (facts inherited from global aren't repeated per area) — a merged all-areas view;
    /// each hit's `source.area` says where it was declared. With `Some(area)`, scans that
    /// area's fully merged config.
    pub fn uses(&self, class: &ClassName, area: Option<Area>) -> Result<Uses> {
        let mut uses = Uses {
            class: class.clone(),
            preferred_for: Vec::new(),
            virtual_types: Vec::new(),
            injections: Vec::new(),
        };
        match area {
            Some(a) => self.scan_uses(a, class, None, &mut uses),
            None => {
                self.scan_uses(Area::Global, class, None, &mut uses);
                for &a in Area::ALL.iter().filter(|&&a| a != Area::Global) {
                    self.scan_uses(a, class, Some(a), &mut uses);
                }
            }
        }
        uses.preferred_for.sort_by(|a, b| a.name.cmp(&b.name));
        uses.virtual_types.sort_by(|a, b| a.name.cmp(&b.name));
        uses.injections.sort_by(|a, b| {
            a.consumer
                .cmp(&b.consumer)
                .then_with(|| a.argument.cmp(&b.argument))
                .then_with(|| a.item_path.cmp(&b.item_path))
        });

        // No references at all: fine for a real class ("unused"), an error for a typo.
        if uses.preferred_for.is_empty()
            && uses.virtual_types.is_empty()
            && uses.injections.is_empty()
            && !self.class_known(class, area.unwrap_or(Area::Global))
        {
            return Err(Error::ClassNotFound(class.clone()));
        }
        Ok(uses)
    }

    /// Scan one area's merged config for references to `class`. `declared_in` restricts
    /// hits to declarations made in that area's own files (used by the merged view to
    /// avoid repeating global-inherited facts per area).
    fn scan_uses(&self, area: Area, class: &ClassName, declared_in: Option<Area>, out: &mut Uses) {
        let cfg = self.di_index().config(area);
        let keep = |s: &Source| declared_in.is_none_or(|a| s.area == a);
        let proxy = ClassName::new(format!("{}\\Proxy", class.as_str()));

        for (for_, located) in &cfg.preferences {
            if located.value == *class && keep(&located.source) {
                out.preferred_for.push(UseRef { name: for_.clone(), source: located.source.clone() });
            }
        }
        for (name, vt) in &cfg.virtual_types {
            if vt.value == *class && keep(&vt.source) {
                out.virtual_types.push(UseRef { name: name.clone(), source: vt.source.clone() });
            }
        }
        for (consumer, args) in &cfg.type_args {
            let consumer_is_virtual = cfg.virtual_types.contains_key(consumer);
            for (arg_name, la) in args {
                scan_arg_for_class(
                    &la.value,
                    &la.source,
                    &mut Vec::new(),
                    &UseScan { class, proxy: &proxy, consumer, consumer_is_virtual, argument: arg_name, keep: &keep },
                    &mut out.injections,
                );
            }
        }
    }

    /// Console commands modules register on `CommandListInterface`'s `commands` array
    /// argument in di.xml — what `bin/magento` picks up. Each command's actual CLI name and
    /// description are extracted from its class (never executed). Optionally filtered by a
    /// case-insensitive substring of the name, class, or di.xml item key; sorted by command
    /// name (unknown names last, by class).
    pub fn console_commands(&self, filter: Option<&str>) -> Vec<ConsoleCommand> {
        let iface = ClassName::new("Magento\\Framework\\Console\\CommandListInterface");
        // The preference (app/etc/di.xml) points at the concrete CommandList; `args_of`
        // then merges arguments declared on the concrete AND — via the ancestor walk — on
        // the interface, because modules register on either.
        let concrete = match self.preference(&iface, Area::Global) {
            Ok(p) => p.concrete,
            Err(_) => iface,
        };
        let args = self.args_of(&concrete, Area::Global, &mut std::collections::HashSet::new());
        let Some((ArgValue::Array(items), _)) = args.get("commands") else {
            return Vec::new();
        };

        let needle = filter.map(str::to_lowercase);
        let mut out: Vec<ConsoleCommand> = items
            .iter()
            .filter_map(|item| {
                let ArgValue::Object(class) = &item.value else { return None };
                let (name, description) = self.index.resolver.command_info(class);
                let cmd = ConsoleCommand {
                    name,
                    description,
                    item_key: item.key.clone(),
                    class: class.clone(),
                    source: item.source.clone(),
                };
                match &needle {
                    Some(n)
                        if !cmd.name.as_deref().unwrap_or("").to_lowercase().contains(n)
                            && !cmd.class.as_str().to_lowercase().contains(n)
                            && !cmd.item_key.to_lowercase().contains(n) =>
                    {
                        None
                    }
                    _ => Some(cmd),
                }
            })
            .collect();
        out.sort_by(|a, b| match (&a.name, &b.name) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.class.cmp(&b.class),
        });
        out
    }

    /// The flagship: full DI resolution of `class` in a single `area` — the concrete type,
    /// preference chain, virtual-type indirection, merged constructor arguments, and the
    /// plugin chain, with provenance throughout.
    pub fn resolve(&self, class: &ClassName, area: Area) -> Result<Resolution> {
        let pref = self.preference(class, area)?;
        let concrete = pref.concrete.clone();
        let cfg = self.di_index().config(area);

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

    pub(crate) fn args_of(
        &self,
        name: &ClassName,
        area: Area,
        seen: &mut std::collections::HashSet<ClassName>,
    ) -> std::collections::HashMap<String, (ArgValue, Source)> {
        let mut merged = std::collections::HashMap::new();
        if !seen.insert(name.clone()) {
            return merged; // cycle guard
        }
        let cfg = self.di_index().config(area);

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

fn scan_arg_for_class(
    value: &ArgValue,
    source: &Source,
    path: &mut Vec<String>,
    scan: &UseScan<'_>,
    out: &mut Vec<model::InjectionSite>,
) {
    let mut hit = |declared: ClassName, as_string: bool| {
        if (scan.keep)(source) {
            out.push(model::InjectionSite {
                consumer: scan.consumer.clone(),
                consumer_is_virtual: scan.consumer_is_virtual,
                argument: scan.argument.to_string(),
                item_path: path.clone(),
                declared,
                as_string,
                source: source.clone(),
            });
        }
    };
    match value {
        ArgValue::Object(c) => {
            if c == scan.class || c == scan.proxy {
                hit(c.clone(), false);
            }
        }
        ArgValue::Scalar { xsi_type, text } => {
            if xsi_type == "string" && text.trim().trim_start_matches('\\') == scan.class.as_str() {
                hit(scan.class.clone(), true);
            }
        }
        ArgValue::Array(items) => {
            for item in items {
                path.push(item.key.clone());
                scan_arg_for_class(&item.value, &item.source, path, scan, out);
                path.pop();
            }
        }
        ArgValue::Null => {}
    }
}

/// Deep-merge one type's declared arguments into the accumulator (array args merge
/// item-by-item; scalars/objects replace).
fn merge_args_into(
    merged: &mut std::collections::HashMap<String, (ArgValue, Source)>,
    args: Option<&std::collections::HashMap<String, engine::di::LocatedArg>>,
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
